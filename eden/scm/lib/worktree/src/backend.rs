/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::BTreeSet;
use std::io;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use configmodel::Config;
use configmodel::ConfigExt;
use fs_err as fs;
use gitcompat::RepoGit;
use identity::Identity;
use repo_minimal_info::RepoMinimalInfo;

use crate::Group;
use crate::Registry;
use crate::Reservations;
use crate::WORKTREE_NAME_FILE;
use crate::WorktreeEntry;
use crate::changed_registry_content;
use crate::dissolve_group;
use crate::dissolve_group_if_empty;
use crate::group_id_for_main_path;
use crate::load_registry;
use crate::read_registry;
use crate::with_registry_lock;
use crate::write_worktree_name_marker;

#[derive(Debug, Eq, PartialEq)]
/// A worktree presented through the backend-independent listing API.
pub struct ListedWorktree {
    /// Absolute path to the working tree.
    pub path: PathBuf,
    /// Whether this is the repository's main working tree.
    pub is_main: bool,
    /// Optional Sapling label associated with this working tree.
    pub label: Option<String>,
    /// Whether this is the working tree from which the command was run.
    pub current: bool,
}

enum WorktreeBackend {
    Eden(EdenBackend),
    Git(GitBackend),
}

struct EdenBackend {
    repo_path: PathBuf,
    shared_store_path: PathBuf,
    ident: Identity,
}

struct GitBackend {
    repo_path: PathBuf,
    /// `$GIT_COMMON_DIR/sl/store`, which holds the worktree operation locks.
    shared_store_path: PathBuf,
    git: RepoGit,
    ident: Identity,
}

impl EdenBackend {
    fn list(&self) -> Result<Vec<ListedWorktree>> {
        let current = util::path::strip_unc_prefix(
            util::path::canonicalize_best_effort(&self.repo_path)
                .with_context(|| format!("failed to canonicalize {}", self.repo_path.display()))?,
        );

        // `worktrees.json` is replaced atomically, so reading it doesn't need the
        // registry lock. Taking the lock only to prune means a listing neither
        // waits for other worktree commands nor holds them up while it checks
        // whether worktrees exist.
        if let Some(listed) = list_unlocked(&self.shared_store_path, &current)
            .context("failed to list EdenFS worktrees")?
        {
            return Ok(listed);
        }

        with_registry_lock(&self.shared_store_path, |registry| {
            list_and_prune_group(registry, &current)
        })
        .context("failed to list EdenFS worktrees")
    }

    fn set_label(&self, target: &Path, label: Option<&str>) -> Result<Option<anyhow::Error>> {
        let current = canonicalize_or_original(&self.repo_path);
        with_registry_lock(&self.shared_store_path, |registry| {
            let Some(group_id) = registry.find_group_for_path(&current) else {
                anyhow::bail!("this worktree is not part of a group");
            };
            let Some(entry) = registry
                .groups
                .get_mut(&group_id)
                .and_then(|group| group.worktrees.get_mut(target))
            else {
                anyhow::bail!("'{}' is not in this worktree group", target.display());
            };
            entry.label = label.map(str::to_owned);

            // The registry is the source of truth; the marker only lets external
            // tools such as `scm-prompt.sh` avoid reading it. Writing it under the
            // registry lock keeps the two in step.
            Ok(
                write_worktree_name_marker(target, &self.ident.resolve_full_dot_dir(target), label)
                    .err(),
            )
        })
    }

    fn check_add(&self, dest: &Path) -> Result<(PathBuf, usize)> {
        let registry = load_registry(&self.shared_store_path)?;
        check_unregistered(&registry, dest)?;
        let main =
            resolve_group_for_main_path(&registry, &canonicalize_or_original(&self.repo_path));
        let linked = count_linked_worktrees(&registry, &main);
        Ok((main, linked))
    }

    fn register(
        &self,
        main: &Path,
        dest: &Path,
        label: Option<&str>,
    ) -> Result<Option<anyhow::Error>> {
        with_registry_lock(&self.shared_store_path, |registry| {
            check_unregistered(registry, dest)?;
            let group_id = resolve_group_id_for_main_path(registry, main);
            // The limit was enforced at reservation time; deliberately no re-check
            // here, since re-checking after the clone is exactly what could strand
            // this checkout on disk unregistered.
            registry
                .groups
                .entry(group_id)
                .or_insert_with(|| Group::new(main.to_owned()))
                .worktrees
                .insert(
                    dest.to_owned(),
                    WorktreeEntry {
                        added: chrono::Utc::now().to_rfc3339(),
                        label: label.map(str::to_owned),
                    },
                );
            // Written under the registry lock, as in `set_label`.
            Ok(
                write_worktree_name_marker(dest, &self.ident.resolve_full_dot_dir(dest), label)
                    .err(),
            )
        })
    }

    fn main_for_remove(&self) -> Result<Option<PathBuf>> {
        let registry = load_registry(&self.shared_store_path)?;
        Ok(
            find_group(&registry, &canonicalize_or_original(&self.repo_path))
                .map(|group| group.main.clone()),
        )
    }

    fn linked_worktrees(&self, main: &Path) -> Result<Vec<PathBuf>> {
        let registry = load_registry(&self.shared_store_path)?;
        Ok(find_group(&registry, main)
            .map(|group| {
                group
                    .worktrees
                    .keys()
                    .filter(|path| *path != main)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default())
    }

    fn validate_removal(&self, main: &Path, target: &Path) -> Result<()> {
        let registry = load_registry(&self.shared_store_path)?;
        let members = find_group(&registry, main).map(|group| &group.worktrees);
        if members.is_some_and(|members| members.contains_key(target)) {
            if target == main {
                anyhow::bail!("cannot remove a main worktree with linked worktrees");
            }
            return Ok(());
        }
        if let Some(parent) = members
            .into_iter()
            .flat_map(|members| members.keys())
            .find(|path| target.starts_with(path))
        {
            anyhow::bail!(
                "{} is not the root of checkout {}, not removing",
                target.display(),
                parent.display()
            );
        }
        if !target.exists() {
            anyhow::bail!("{} does not exist", target.display());
        }
        anyhow::bail!(
            "{} is not in this worktree group, use `eden rm` instead",
            target.display()
        )
    }

    fn forget_removed(&self, main: &Path, target: &Path) -> Result<()> {
        with_registry_lock(&self.shared_store_path, |registry| {
            if let Some(group_id) = registry.find_group_for_path(main)
                && let Some(group) = registry.groups.get_mut(&group_id)
            {
                group.worktrees.remove(target);
                dissolve_group_if_empty(registry, &group_id);
            }
            Ok(())
        })
    }
}

impl GitBackend {
    fn list(&self) -> Result<Vec<ListedWorktree>> {
        let current = canonicalize_or_original(&self.repo_path);

        self.git
            .list_worktrees()
            .context("failed to list Git worktrees")?
            .into_iter()
            .map(|worktree| {
                let canonical_path = canonicalize_or_original(&worktree.path);
                Ok(ListedWorktree {
                    label: read_git_label(&self.ident.resolve_full_dot_dir(&worktree.path))?,
                    path: worktree.path,
                    is_main: worktree.is_main,
                    current: canonical_path == current,
                })
            })
            .collect::<Result<Vec<_>>>()
    }

    fn set_label(&self, target: &Path, label: Option<&str>) -> Result<()> {
        let is_member = self
            .git
            .list_worktrees()?
            .iter()
            .any(|worktree| canonicalize_or_original(&worktree.path) == target);
        if !is_member {
            anyhow::bail!("'{}' is not in this worktree group", target.display());
        }
        write_git_label(&self.ident.resolve_full_dot_dir(target), label)
    }

    fn check_add(&self, _dest: &Path) -> Result<(PathBuf, usize)> {
        let worktrees = self.git.list_worktrees()?;
        let main = worktrees
            .iter()
            .find(|worktree| worktree.is_main)
            .context("Git did not report a main worktree")?;
        let main = canonicalize_or_original(&main.path);
        let linked = worktrees
            .iter()
            .filter(|worktree| !worktree.is_main)
            .count();
        Ok((main, linked))
    }

    fn main(&self) -> Result<PathBuf> {
        let main = self
            .git
            .list_worktrees()?
            .into_iter()
            .find(|worktree| worktree.is_main)
            .context("Git did not report a main worktree")?;
        Ok(canonicalize_or_original(&main.path))
    }

    fn linked_worktrees(&self) -> Result<Vec<PathBuf>> {
        Ok(self
            .git
            .list_worktrees()?
            .into_iter()
            .filter(|worktree| !worktree.is_main)
            .map(|worktree| canonicalize_or_original(&worktree.path))
            .collect())
    }

    fn occupied_slots(&self, reservations: &Reservations, main: &Path) -> Result<usize> {
        // Git lists a new checkout before its reservation is released; count it once.
        let reserved = reservations
            .reservations
            .values()
            .filter(|reservation| reservation.group_main == main)
            .map(|reservation| reservation.dest.clone());
        Ok(self
            .linked_worktrees()?
            .into_iter()
            .chain(reserved)
            .collect::<BTreeSet<_>>()
            .len())
    }

    fn validate_removal(&self, target: &Path) -> Result<()> {
        let listed: Vec<_> = self
            .git
            .list_worktrees()?
            .into_iter()
            .map(|worktree| (canonicalize_or_original(&worktree.path), worktree.is_main))
            .collect();
        if let Some((_, is_main)) = listed.iter().find(|(path, _)| path == target) {
            if *is_main {
                anyhow::bail!("cannot remove the main worktree");
            }
            return Ok(());
        }
        // Linked checkouts may be nested inside the main one.
        if let Some((parent, _)) = listed
            .iter()
            .filter(|(path, _)| target.starts_with(path))
            .max_by_key(|(path, _)| path.components().count())
        {
            anyhow::bail!(
                "{} is not the root of checkout {}, not removing",
                target.display(),
                parent.display()
            );
        }
        anyhow::bail!("{} is not in this worktree group", target.display())
    }
}

/// Access to the worktrees associated with one repository.
pub struct Worktrees {
    backend: WorktreeBackend,
}

impl Worktrees {
    /// Open the worktree implementation appropriate for `repo`.
    pub fn open(repo: &RepoMinimalInfo, config: &dyn Config) -> Result<Self> {
        let is_git =
            repo.requirements.contains("dotgit") && repo.store_requirements.contains("git-store");
        let backend = if is_git {
            if !config
                .get_or("worktree", "git-enabled", || true)
                .context("invalid worktree.git-enabled configuration")?
            {
                anyhow::bail!("worktree commands for git are disabled by worktree.git-enabled");
            }
            WorktreeBackend::Git(GitBackend {
                repo_path: repo.path.clone(),
                shared_store_path: repo.store_path.clone(),
                git: RepoGit::from_root_and_config(repo.path.clone(), config),
                ident: repo.ident,
            })
        } else if repo.requirements.contains("eden") {
            WorktreeBackend::Eden(EdenBackend {
                repo_path: repo.path.clone(),
                shared_store_path: repo.store_path.clone(),
                ident: repo.ident,
            })
        } else {
            anyhow::bail!(
                "worktree commands require an EdenFS-backed repository or a native Git repository"
            );
        };

        Ok(Self { backend })
    }

    /// List the repository's worktrees using the backend's source of truth.
    pub fn list(&self) -> Result<Vec<ListedWorktree>> {
        match &self.backend {
            WorktreeBackend::Eden(backend) => backend.list(),
            WorktreeBackend::Git(backend) => backend.list(),
        }
    }

    /// Set or remove the label of `target`, a canonical member path.
    /// `Ok(Some(error))` means the label was saved but the EdenFS marker file read
    /// by `scm-prompt.sh` could not be written; callers should warn and continue.
    pub fn set_label(&self, target: &Path, label: Option<&str>) -> Result<Option<anyhow::Error>> {
        match &self.backend {
            WorktreeBackend::Eden(backend) => backend.set_label(target, label),
            WorktreeBackend::Git(backend) => backend.set_label(target, label).map(|()| None),
        }
    }

    /// Directory holding the registry, reservations, and operation locks.
    pub fn shared_store_path(&self) -> PathBuf {
        match &self.backend {
            WorktreeBackend::Eden(backend) => backend.shared_store_path.clone(),
            WorktreeBackend::Git(backend) => backend.shared_store_path.clone(),
        }
    }

    /// Reject registered destinations and return the group's main checkout and
    /// linked-worktree count from one snapshot. The count excludes reservations;
    /// enforce limits with `occupied_slots` under the reservation lock.
    pub fn check_add(&self, dest: &Path) -> Result<(PathBuf, usize)> {
        match &self.backend {
            WorktreeBackend::Eden(backend) => backend.check_add(dest),
            WorktreeBackend::Git(backend) => backend.check_add(dest),
        }
    }

    /// Count linked checkouts of `main` plus `reservations`. To enforce a limit,
    /// call this inside [`crate::with_reservations`] after pruning expired
    /// reservations.
    pub fn occupied_slots(
        &self,
        registry: &Registry,
        reservations: &Reservations,
        main: &Path,
    ) -> Result<usize> {
        match &self.backend {
            WorktreeBackend::Eden(_) => {
                Ok(count_linked_worktrees(registry, main) + reservations.count_for_group(main))
            }
            WorktreeBackend::Git(backend) => backend.occupied_slots(reservations, main),
        }
    }

    /// Record a newly created checkout. `Ok(Some(error))` means the checkout is
    /// usable, but its `worktreename` file could not be written.
    pub fn register(
        &self,
        main: &Path,
        dest: &Path,
        label: Option<&str>,
    ) -> Result<Option<anyhow::Error>> {
        match &self.backend {
            WorktreeBackend::Eden(backend) => backend.register(main, dest, label),
            WorktreeBackend::Git(backend) => {
                Ok(write_git_label(&backend.ident.resolve_full_dot_dir(dest), label).err())
            }
        }
    }

    /// Main checkout of the invoking checkout's group, or `None` if it has no
    /// linked worktrees.
    pub fn main_for_remove(&self) -> Result<Option<PathBuf>> {
        match &self.backend {
            WorktreeBackend::Eden(backend) => backend.main_for_remove(),
            WorktreeBackend::Git(backend) => backend.main().map(Some),
        }
    }

    /// Linked checkouts in `main`'s group.
    pub fn linked_worktrees(&self, main: &Path) -> Result<Vec<PathBuf>> {
        match &self.backend {
            WorktreeBackend::Eden(backend) => backend.linked_worktrees(main),
            WorktreeBackend::Git(backend) => backend.linked_worktrees(),
        }
    }

    /// Check that `target` is the root of a linked checkout in `main`'s group.
    pub fn validate_removal(&self, main: &Path, target: &Path) -> Result<()> {
        match &self.backend {
            WorktreeBackend::Eden(backend) => backend.validate_removal(main, target),
            WorktreeBackend::Git(backend) => backend.validate_removal(target),
        }
    }

    /// Forget `target` after its checkout has been removed. The caller holds its
    /// path lock.
    pub fn forget_removed(&self, main: &Path, target: &Path) -> Result<()> {
        match &self.backend {
            WorktreeBackend::Eden(backend) => backend.forget_removed(main, target),
            // `git worktree remove` already deleted the admin directory holding the label.
            WorktreeBackend::Git(_) => Ok(()),
        }
    }

    /// The Git repository, if this is a native Git checkout.
    pub fn git(&self) -> Option<&RepoGit> {
        match &self.backend {
            WorktreeBackend::Eden(_) => None,
            WorktreeBackend::Git(backend) => Some(&backend.git),
        }
    }
}

/// List `current`'s group from the registry read without the lock, or return
/// `None` if listing would change the registry.
fn list_unlocked(shared_store_path: &Path, current: &Path) -> Result<Option<Vec<ListedWorktree>>> {
    let (mut registry, content) = read_registry(shared_store_path)?;
    let listed = list_and_prune_group(&mut registry, current)?;
    Ok(changed_registry_content(content.as_deref(), &registry)?
        .is_none()
        .then_some(listed))
}

/// List the worktrees in `current`'s group, first removing worktrees that no
/// longer exist from `registry`, and the group itself if its main worktree no
/// longer exists or no linked worktrees remain.
fn list_and_prune_group(registry: &mut Registry, current: &Path) -> Result<Vec<ListedWorktree>> {
    let Some(group_id) = registry.find_group_for_path(current) else {
        return Ok(Vec::new());
    };

    let Some(group) = registry.groups.get(&group_id) else {
        anyhow::bail!("worktree group '{group_id}' disappeared from the registry");
    };

    if !group.main.exists() {
        dissolve_group(registry, &group_id);
        return Ok(Vec::new());
    }

    if group.worktrees.keys().any(|path| !path.exists()) {
        let Some(group) = registry.groups.get_mut(&group_id) else {
            anyhow::bail!("worktree group '{group_id}' disappeared from the registry");
        };
        group.worktrees.retain(|path, _| path.exists());
        dissolve_group_if_empty(registry, &group_id);
    }

    Ok(registry
        .groups
        .get(&group_id)
        .map(|group| {
            group
                .worktrees
                .iter()
                .map(|(path, entry)| ListedWorktree {
                    path: path.clone(),
                    is_main: *path == group.main,
                    label: entry.label.clone(),
                    current: *path == current,
                })
                .collect()
        })
        .unwrap_or_default())
}

/// Git labels live in each checkout's admin directory, which Git keeps attached
/// to the checkout across moves and deletes with it, so a label cannot outlive
/// or detach from its checkout.
fn read_git_label(dot_dir: &Path) -> Result<Option<String>> {
    match fs::read_to_string(dot_dir.join(WORKTREE_NAME_FILE)) {
        Ok(label) => Ok(Some(label).filter(|label| !label.is_empty())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Unlike the EdenFS marker, a missing file means no label; the prompt then
/// falls back to Git's worktree name.
fn write_git_label(dot_dir: &Path, label: Option<&str>) -> Result<()> {
    let path = dot_dir.join(WORKTREE_NAME_FILE);
    match label {
        Some(label) => {
            // Not `create_dir_all`: a concurrent `git worktree remove` deletes the
            // admin directory, and recreating it would leave junk at the removed path.
            match fs::create_dir(dot_dir) {
                Err(error) if error.kind() != io::ErrorKind::AlreadyExists => {
                    return Err(error.into());
                }
                _ => {}
            }
            // Written atomically so `list` never reads a partial marker.
            util::file::atomic_write(&path, |file| file.write_all(label.as_bytes()))?;
        }
        None => match fs::remove_file(&path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error.into()),
            _ => {}
        },
    }
    Ok(())
}

fn canonicalize_or_original(path: &Path) -> PathBuf {
    util::path::canonicalize_best_effort(path)
        .map(util::path::strip_unc_prefix)
        .unwrap_or_else(|_| path.to_owned())
}

fn check_unregistered(registry: &Registry, dest: &Path) -> Result<()> {
    if registry.find_group_for_path(dest).is_some() {
        anyhow::bail!(
            "destination path '{}' is already registered as a worktree",
            dest.display()
        );
    }
    Ok(())
}

fn find_group<'a>(registry: &'a Registry, member: &Path) -> Option<&'a Group> {
    registry
        .find_group_for_path(member)
        .and_then(|group_id| registry.groups.get(&group_id))
}

fn resolve_group_for_main_path(registry: &Registry, canonical_repo_path: &Path) -> PathBuf {
    registry
        .find_group_for_path(canonical_repo_path)
        .and_then(|group_id| {
            registry
                .groups
                .get(&group_id)
                .map(|group| group.main.clone())
        })
        .unwrap_or_else(|| canonical_repo_path.to_path_buf())
}

fn resolve_group_id_for_main_path(registry: &Registry, group_main_path: &Path) -> String {
    registry
        .find_group_for_path(group_main_path)
        .unwrap_or_else(|| group_id_for_main_path(group_main_path))
}

fn count_linked_worktrees(registry: &Registry, group_main_path: &Path) -> usize {
    registry
        .find_group_for_path(group_main_path)
        .and_then(|group_id| registry.groups.get(&group_id))
        .map(Group::linked_worktree_count)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Duration;

    use util::lock::PathLock;

    use super::*;
    use crate::save_registry;

    #[test]
    fn test_resolve_group_id_for_main_path_reuses_legacy_group_id() {
        let mut registry = Registry::new();
        let main = PathBuf::from("/tmp/main");
        let linked = PathBuf::from("/tmp/linked");
        let legacy_group_id = "legacy-random-group-id".to_string();

        let mut group = Group::new(main.clone());
        group.worktrees.insert(
            linked.clone(),
            WorktreeEntry {
                added: "2025-01-01T00:00:00Z".to_string(),
                label: None,
            },
        );
        registry.groups.insert(legacy_group_id.clone(), group);

        let group_main = resolve_group_for_main_path(&registry, &linked);

        assert_eq!(group_main, main);
        assert_eq!(
            resolve_group_id_for_main_path(&registry, &group_main),
            legacy_group_id
        );
    }

    #[test]
    fn test_list_while_registry_locked() {
        let store_dir = tempfile::tempdir().unwrap();
        let checkouts_dir = tempfile::tempdir().unwrap();
        let main_path = checkouts_dir.path().join("main");
        std::fs::create_dir(&main_path).unwrap();
        let main_path =
            util::path::strip_unc_prefix(util::path::canonicalize_best_effort(&main_path).unwrap());

        let mut registry = Registry::new();
        registry
            .groups
            .insert("test-group-id".to_string(), Group::new(main_path.clone()));
        save_registry(store_dir.path(), &registry).unwrap();

        let backend = EdenBackend {
            repo_path: main_path,
            shared_store_path: store_dir.path().to_path_buf(),
            ident: identity::default(),
        };

        // Another worktree command holds the registry lock.
        let lock = PathLock::exclusive(store_dir.path().join("worktrees.lock")).unwrap();

        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(backend.list().map(|listed| listed.len()));
        });

        // Listing doesn't wait for the registry lock.
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(60)).unwrap().unwrap(),
            1
        );
        drop(lock);
    }

    #[test]
    fn test_list_prunes_missing_worktrees() {
        let store_dir = tempfile::tempdir().unwrap();
        let checkouts_dir = tempfile::tempdir().unwrap();
        let main_path = checkouts_dir.path().join("main");
        let linked_path = checkouts_dir.path().join("linked");
        std::fs::create_dir(&main_path).unwrap();
        std::fs::create_dir(&linked_path).unwrap();
        let main_path =
            util::path::strip_unc_prefix(util::path::canonicalize_best_effort(&main_path).unwrap());
        let linked_path = util::path::strip_unc_prefix(
            util::path::canonicalize_best_effort(&linked_path).unwrap(),
        );
        let missing_path = checkouts_dir.path().join("missing");

        let mut group = Group::new(main_path.clone());
        for path in [&linked_path, &missing_path] {
            group.worktrees.insert(
                path.clone(),
                WorktreeEntry {
                    added: "2025-01-01T00:00:00Z".to_string(),
                    label: None,
                },
            );
        }
        let mut registry = Registry::new();
        registry.groups.insert("test-group-id".to_string(), group);
        save_registry(store_dir.path(), &registry).unwrap();

        let backend = EdenBackend {
            repo_path: main_path.clone(),
            shared_store_path: store_dir.path().to_path_buf(),
            ident: identity::default(),
        };
        let listed: Vec<PathBuf> = backend
            .list()
            .unwrap()
            .into_iter()
            .map(|listed| listed.path)
            .collect();
        assert_eq!(listed, vec![linked_path.clone(), main_path.clone()]);

        let registry = load_registry(store_dir.path()).unwrap();
        let stored: Vec<&PathBuf> = registry.groups["test-group-id"].worktrees.keys().collect();
        assert_eq!(stored, vec![&linked_path, &main_path]);
    }
}
