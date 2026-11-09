/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::LazyLock;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use configmodel::Config;
use configmodel::ConfigExt;
use context::CoreContext;
#[cfg(feature = "eden")]
use edenfs_client::EdenFsClient;
use identity::Identity;
use journal::Journal;
use manifest::FileType;
use manifest::FsNodeMetadata;
use manifest::Manifest;
use manifest_tree::TreeManifest;
use manifest_tree::TreeResolver;
use parking_lot::Mutex;
use pathmatcher::DynMatcher;
use pathmatcher::GitignoreMatcher;
use pathmatcher::IntersectMatcher;
use pathmatcher::Matcher;
use pathmatcher::NegateMatcher;
use pathmatcher::UnionMatcher;
use regex::Regex;
use repolock::LockedPath;
use repolock::RepoLocker;
use repostate::MergeState;
use status::FileStatus;
use status::Status;
use status::StatusBuilder;
use storemodel::FileStore;
use submodule::Submodule;
use submodule::parse_gitmodules;
use tracing::debug;
use treestate::filestate::StateFlags;
use treestate::treestate::TreeState;
use types::HgId;
use types::RepoPath;
use types::RepoPathBuf;
use types::hgid::NULL_ID;
use util::file::atomic_write;
use util::file::read_to_string_if_exists;
use util::file::unlink_if_exists;
use vfs::VFS;

use crate::client::WorkingCopyClient;
use crate::errors;
use crate::filesystem::DotGitFileSystem;
#[cfg(feature = "eden")]
use crate::filesystem::EdenFileSystem;
use crate::filesystem::FileSystem;
use crate::filesystem::FileSystemType;
use crate::filesystem::GrepoFileSystem;
use crate::filesystem::PendingChange;
use crate::filesystem::PhysicalFileSystem;
use crate::filesystem::WatchmanFileSystem;
use crate::filesystem::grepo::grepo_manifest_path;
use crate::filesystem::grepo::parse_grepo_manifest;
use crate::manifest::apply_status;
use crate::status::compute_status;
use crate::util::added_files;
use crate::util::fast_path_wdir_parents_with_config;
use crate::util::walk_treestate;
use crate::watchman_client::DeferredWatchmanClient;

#[cfg(not(feature = "eden"))]
pub struct EdenFsClient {}

#[cfg(not(feature = "eden"))]
impl EdenFsClient {
    pub fn from_wdir(_wdir_root: &Path) -> anyhow::Result<Self> {
        panic!("cannot use EdenFS in a non-EdenFS build");
    }
}

type ArcFileStore = Arc<dyn FileStore>;
type ArcTreeResolver = Arc<dyn TreeResolver>;
type BoxFileSystem = Box<dyn FileSystem + Send>;

pub struct WorkingCopy {
    vfs: VFS,
    config: Arc<dyn Config>,
    ident: Identity,
    treestate: Arc<Mutex<TreeState>>,
    tree_resolver: ArcTreeResolver,
    filestore: ArcFileStore,
    pub(crate) filesystem: Mutex<BoxFileSystem>,
    pub ignore_matcher: Arc<GitignoreMatcher>,
    pub(crate) locker: Arc<RepoLocker>,
    pub(crate) dot_hg_path: PathBuf,
    pub journal: Journal,
    watchman_client: Arc<DeferredWatchmanClient>,
    notify_parents_change_func: Option<Box<dyn Fn(&[HgId]) -> Result<()> + Send + Sync>>,
    support_submodules: bool,
    pub file_system_type: FileSystemType,
}

const ACTIVE_BOOKMARK_FILE: &str = "bookmarks.current";

impl WorkingCopy {
    pub fn new(
        path: &Path,
        config: &Arc<dyn Config>,
        tree_resolver: ArcTreeResolver,
        filestore: ArcFileStore,
        locker: Arc<RepoLocker>,
        // For dirstate
        dot_dir: &Path,
        // For FilterGenerator
        shared_dot_dir: &Path,
        has_requirement: &dyn Fn(&str) -> bool,
    ) -> Result<Self> {
        tracing::trace!("initializing vfs at {path:?}");
        let vfs = VFS::new_destructive(path.to_path_buf())?;

        let is_eden = has_requirement("eden");

        // Symlink support is currently set by a requirement.
        if cfg!(windows) {
            let supports_symlink = std::env::var_os("SL_DEBUG_DISABLE_SYMLINKS").is_none()
                && has_requirement("windowssymlinks");
            vfs.set_supports_symlinks(supports_symlink)
        }

        let support_submodules =
            has_requirement("git") && config.get_or("git", "submodules", || true)?;

        // In case the "requires" file gets corrupted, check `.eden` directory
        // and prevent treating edenfs as non-edenfs.
        if !is_eden && path.join(".eden").is_dir() {
            anyhow::bail!(
                "Detected conflicting information about whether EdenFS is enabled.\n\
                 This might indicate repo metadata (ex. {}) corruption.\n\
                 To avoid further corruption, this is a fatal error.\n\
                 Contact the Source Control support team for investigation.",
                dot_dir.join("requires").display()
            );
        }

        let file_system_type = if is_eden {
            FileSystemType::Eden
        } else if has_requirement("dotgit") {
            FileSystemType::DotGit
        } else if has_requirement("grepo") {
            FileSystemType::Grepo
        } else {
            let fsmonitor_ext = config.get("extensions", "fsmonitor");
            let fsmonitor_mode = config.get_nonempty("fsmonitor", "mode");
            let is_watchman = if fsmonitor_ext.is_none() || fsmonitor_ext == Some("!".into()) {
                false
            } else {
                fsmonitor_mode.is_none() || fsmonitor_mode == Some("on".into())
            };
            if is_watchman {
                FileSystemType::Watchman
            } else {
                FileSystemType::Normal
            }
        };

        let ignore_matcher = Arc::new(GitignoreMatcher::new(
            vfs.root(),
            WorkingCopy::global_ignore_paths(vfs.root(), config)
                .iter()
                .map(|i| i.as_path())
                .collect(),
            vfs.case_sensitive(),
        ));

        let watchman_client = Arc::new(DeferredWatchmanClient::new(config.clone()));

        let filesystem = Self::construct_file_system(
            vfs.clone(),
            dot_dir,
            shared_dot_dir,
            config.clone(),
            file_system_type.clone(),
            tree_resolver.clone(),
            filestore.clone(),
            locker.clone(),
            watchman_client.clone(),
        )?;
        let treestate = filesystem.get_treestate()?;
        tracing::debug!(target: "dirstate_size", dirstate_size=treestate.lock().len());
        let filesystem = Mutex::new(filesystem);

        let root = vfs.root();
        let ident = match identity::sniff_dir(root)? {
            Some(ident) => ident,
            None => {
                return Err(errors::RepoNotFound(root.to_string_lossy().to_string()).into());
            }
        };
        let dot_hg_path = ident.resolve_full_dot_dir(vfs.root());
        let journal = Journal::open(dot_hg_path.clone())?;

        Ok(WorkingCopy {
            vfs,
            config: config.clone(),
            ident,
            treestate,
            tree_resolver,
            filestore,
            filesystem,
            ignore_matcher,
            locker,
            dot_hg_path,
            journal,
            watchman_client,
            notify_parents_change_func: None,
            support_submodules,
            file_system_type,
        })
    }

    /// Working copy root path, with `.hg`.
    pub fn dot_hg_path(&self) -> &Path {
        &self.dot_hg_path
    }

    pub fn lock(&self) -> Result<LockedWorkingCopy<'_>, repolock::LockError> {
        let locked_path = self.locker.lock_working_copy(self.dot_hg_path.clone())?;
        Ok(LockedWorkingCopy {
            dot_hg_path: locked_path,
            wc: self,
        })
    }

    pub fn is_locked(&self) -> bool {
        self.locker.working_copy_locked(&self.dot_hg_path)
    }

    pub fn treestate(&self) -> Arc<Mutex<TreeState>> {
        self.treestate.clone()
    }

    pub fn vfs(&self) -> &VFS {
        &self.vfs
    }

    pub fn parents(&self) -> Result<Vec<HgId>> {
        self.treestate.lock().parents().collect()
    }

    /// Return the first working copy parent, or the null commit if there are no parents.
    pub fn first_parent(&self) -> Result<HgId> {
        Ok(self.parents()?.into_iter().next().unwrap_or(NULL_ID))
    }

    pub fn filestore(&self) -> ArcFileStore {
        self.filestore.clone()
    }

    pub fn tree_resolver(&self) -> ArcTreeResolver {
        self.tree_resolver.clone()
    }

    pub(crate) fn current_manifests(
        treestate: &TreeState,
        tree_resolver: &ArcTreeResolver,
    ) -> Result<Vec<Arc<TreeManifest>>> {
        let mut parents = treestate.parents().peekable();
        if parents.peek_mut().is_some() {
            parents
                .map(|p| Ok(Arc::new(tree_resolver.get(&p?)?)))
                .collect()
        } else {
            let null_commit = HgId::null_id().clone();
            Ok(vec![Arc::new(
                tree_resolver
                    .get(&null_commit)
                    .context("resolving null commit tree")?,
            )])
        }
    }

    fn global_ignore_paths(root: &Path, config: &dyn Config) -> Vec<PathBuf> {
        config
            .keys_prefixed("ui", "ignore.")
            .iter()
            .chain(Some(&"ignore".into()))
            .filter_map(
                |name| match config.get_nonempty_opt::<PathBuf>("ui", name) {
                    Ok(Some(path)) => Some(root.join(path)),
                    _ => None,
                },
            )
            .collect()
    }

    fn construct_file_system(
        vfs: VFS,
        dot_dir: &Path,
        shared_dot_dir: &Path,
        config: Arc<dyn Config>,
        file_system_type: FileSystemType,
        tree_resolver: ArcTreeResolver,
        store: ArcFileStore,
        locker: Arc<RepoLocker>,
        watchman_client: Arc<DeferredWatchmanClient>,
    ) -> Result<BoxFileSystem> {
        // Read-only status crawls the filesystem without updating Watchman clocks
        // or treestate metadata. Classification still uses the shared status engine.
        if config.get_or_default::<bool>("workingcopy", "status-read-only")?
            && matches!(
                file_system_type,
                FileSystemType::Normal | FileSystemType::Watchman
            )
        {
            return Ok(Box::new(PhysicalFileSystem::new_with_read_only(
                vfs,
                dot_dir,
                tree_resolver,
                store,
                locker,
                true,
            )?));
        }
        Ok(match file_system_type {
            FileSystemType::Normal => Box::new(PhysicalFileSystem::new(
                vfs.clone(),
                dot_dir,
                tree_resolver,
                store.clone(),
                locker,
            )?),
            FileSystemType::Watchman => Box::new(WatchmanFileSystem::new(
                vfs.clone(),
                dot_dir,
                tree_resolver,
                store.clone(),
                locker,
                watchman_client,
            )?),
            FileSystemType::Eden => {
                #[cfg(not(feature = "eden"))]
                {
                    let _ = shared_dot_dir;
                    panic!("cannot use EdenFS in a non-EdenFS build");
                }
                #[cfg(feature = "eden")]
                {
                    let client = Arc::new(EdenFsClient::from_wdir(
                        vfs.root(),
                        shared_dot_dir,
                        config.clone(),
                    )?);
                    Box::new(EdenFileSystem::new(
                        config.clone(),
                        client,
                        vfs.clone(),
                        dot_dir,
                        store.clone(),
                    )?)
                }
            }
            FileSystemType::DotGit => {
                // This is the working-copy `.git` entry. For a linked worktree or submodule it can
                // be a gitfile; lib/gitcompat's RepoGit follows it to GIT_DIR and separately resolves
                // GIT_COMMON_DIR.
                let git_dir = vfs.root().join(".git");
                Box::new(DotGitFileSystem::new(
                    vfs.clone(),
                    dot_dir,
                    &git_dir,
                    store.clone(),
                    &config,
                )?)
            }
            FileSystemType::Grepo => {
                let git_dir = vfs.root().join(".repo/manifests/.git");
                let inner =
                    DotGitFileSystem::new(vfs.clone(), dot_dir, &git_dir, store.clone(), &config)?;
                Box::new(GrepoFileSystem::new(
                    inner,
                    vfs.clone(),
                    tree_resolver.clone(),
                    &config,
                )?)
            }
        })
    }

    pub fn status(
        &self,
        ctx: &CoreContext,
        matcher: DynMatcher,
        include_ignored: bool,
    ) -> Result<Status> {
        let result = self.status_internal(ctx, matcher.clone(), include_ignored);

        result.or_else(|e| {
            if self
                .config
                .get_or("experimental", "repair-eden-dirstate", || true)?
            {
                let errmsg = e.to_string();
                if errmsg.contains("EdenError: error computing status") {
                    match parse_edenfs_status_error(&errmsg) {
                        Some(parent) => {
                            self.treestate
                                .lock()
                                .set_parents(&mut std::iter::once(&parent))?;
                            tracing::warn!("repaired eden dirstate: set parent to {}", parent);
                        }

                        None => {
                            tracing::warn!(
                                "could not parse a parent from error message {}",
                                errmsg
                            );
                            return Err(e);
                        }
                    }

                    // retry
                    return self.status_internal(ctx, matcher, include_ignored);
                }
            }
            Err(e)
        })
    }

    pub fn sparse_matcher(&self) -> Result<Option<DynMatcher>> {
        let manifests =
            WorkingCopy::current_manifests(&self.treestate.lock(), &self.tree_resolver)?;
        let sparse_matcher = self
            .filesystem
            .lock()
            .sparse_matcher(&manifests, self.ident.dot_dir())?;
        Ok(sparse_matcher)
    }

    /// Returns a manifest representing the current working copy state.
    ///
    /// This combines the p1 manifest with uncommitted changes from status to produce
    /// a manifest that reflects the current working directory contents.
    ///
    /// # Arguments
    /// * `ctx` - The core context for the operation
    /// * `matcher` - Matcher for filtering which files to include in status
    /// * `include_unknown` - If true, untracked files are included in the manifest
    pub fn working_manifest(
        &self,
        ctx: &CoreContext,
        matcher: DynMatcher,
        include_unknown: bool,
    ) -> Result<TreeManifest> {
        // Get the current parent manifests.
        let manifests =
            WorkingCopy::current_manifests(&self.treestate.lock(), &self.tree_resolver)?;

        // Clone the p1 manifest as our base.
        let mut manifest = (*manifests[0]).clone();

        // Get the status of uncommitted changes.
        let status = self.status(ctx, matcher.clone(), false)?;

        // Get the copymap and convert to HashMap.
        let copymap: HashMap<RepoPathBuf, RepoPathBuf> =
            self.copymap(matcher)?.into_iter().collect();

        // Build the list of parent manifest references for apply_status.
        let parent_refs: Vec<&TreeManifest> = manifests.iter().map(|m| m.as_ref()).collect();

        // Apply the status changes to the manifest.
        apply_status(
            ctx,
            &mut manifest,
            &status,
            &self.vfs,
            &self.filestore,
            &parent_refs,
            copymap,
            include_unknown,
        )?;

        Ok(manifest)
    }

    pub fn status_internal(
        &self,
        ctx: &CoreContext,
        mut matcher: DynMatcher,
        include_ignored: bool,
    ) -> Result<Status> {
        let span = tracing::info_span!("status", status_len = tracing::field::Empty);
        let _enter = span.enter();

        let added_files = added_files(&mut self.treestate.lock())?;

        let manifests =
            WorkingCopy::current_manifests(&self.treestate.lock(), &self.tree_resolver)?;

        let sparse_matcher = self
            .filesystem
            .lock()
            .sparse_matcher(&manifests, self.ident.dot_dir())?;

        if let Some(sparse) = sparse_matcher.clone() {
            matcher = Arc::new(IntersectMatcher::new(vec![matcher, sparse]));
        }

        let mut ignore_matcher: DynMatcher = self.ignore_matcher.clone();

        // Treat files outside sparse profile as ignored.
        if let Some(sparse) = sparse_matcher.clone() {
            ignore_matcher = Arc::new(UnionMatcher::new(vec![
                // Check sparse matcher first. It is cheaper than ignore matcher.
                Arc::new(NegateMatcher::new(sparse)),
                ignore_matcher,
            ]));
        }

        let mut ignore_dirs = vec![PathBuf::from(self.ident.dot_dir())];
        // Ignore file within submodules. Python has some additional logic layered on
        // top to add submodule info into status results.
        let submodules = self.parse_submodule_config()?;
        if !submodules.is_empty() {
            ignore_dirs.extend(
                submodules
                    .iter()
                    .map(move |s| PathBuf::from(s.path.clone())),
            );
        }

        let pending_changes = self
            .filesystem
            .lock()
            .pending_changes(
                ctx,
                matcher.clone(),
                ignore_matcher,
                ignore_dirs,
                include_ignored,
            )?
            // fs.pending_changes() won't return ignored files, but we want added ignored files to
            // show up in the results, so let's inject them here.
            .chain(added_files.into_iter().filter_map(|path| {
                match self.ignore_matcher.matches_file(&path) {
                    Ok(result) if result => match self.vfs.metadata(&path) {
                        Ok(ref attr) if attr.is_dir() => None,
                        Ok(_) => Some(Ok(PendingChange::Changed(path))),
                        Err(err) => {
                            if let Some(io_err) = err.downcast_ref::<std::io::Error>() {
                                // If file is not on disk, report as deleted so it shows up as "!".
                                if io_err.kind() == std::io::ErrorKind::NotFound {
                                    return Some(Ok(PendingChange::Deleted(path)));
                                }
                            }

                            // Propagate error otherwise this added file might disappear from "status".
                            Some(Err(err))
                        }
                    },
                    Ok(_) => None,
                    Err(e) => Some(Err(e)),
                }
            }))
            .filter_map(|result| match result {
                Ok(change_type) => match matcher.matches_file(change_type.get_path()) {
                    Ok(true) => {
                        tracing::trace!(?change_type, "pending change");
                        Some(Ok(change_type))
                    }
                    Err(e) => Some(Err(e)),
                    _ => None,
                },
                Err(e) => Some(Err(e)),
            });

        let p1_manifest = manifests[0].as_ref();
        let mut status_builder = compute_status(
            p1_manifest,
            self.treestate.clone(),
            pending_changes,
            matcher.clone(),
        )?;

        if !self.vfs.supports_symlinks()
            && ctx
                .config
                .get_or_default("unsafe", "filtersuspectsymlink")
                .unwrap_or_default()
        {
            status_builder = self.filter_accidental_symlink_changes(status_builder, p1_manifest)?;
        }

        // Calculate submodule status.
        if !submodules.is_empty() {
            if let Some(tree) =
                Self::current_manifests(&self.treestate.lock(), &self.tree_resolver)?
                    .into_iter()
                    .next()
            {
                for subm in submodules.iter() {
                    let path = RepoPath::from_str(&subm.path)?;
                    if !subm.active {
                        status_builder.forget(path);
                        continue;
                    }
                    // The submodule path is treated as a file.
                    // See https://sapling-scm.com/docs/git/submodule.
                    if !matcher.matches_file(path)? {
                        continue;
                    }
                    let subm_path = self.vfs.root().join(&subm.path);
                    // cgit behavior: if a submodule is "active" (enabled in config), but not
                    // "initialized" (subm_path/.git is not a git directory), then it's considered
                    // as "not modified".
                    // https://github.com/git/git/blob/e813a0200a7121b97fec535f0d0b460b0a33356c/submodule.c#L1888
                    if self.ident.dot_dir().starts_with(".git") && !subm_path.join(".git").is_dir()
                    {
                        continue;
                    }
                    // PERF: This does not do batch fetching properly.
                    let tree_node = tree.get(path)?.and_then(|m| match m {
                        FsNodeMetadata::File(f) => match f.file_type {
                            FileType::GitSubmodule => Some(f.hgid),
                            _ => None,
                        },
                        FsNodeMetadata::Directory(_) => None,
                    });
                    // The submodule working copy should use the same dotdir.
                    let file_node = fast_path_wdir_parents_with_config(
                        &subm_path,
                        self.ident,
                        self.config.as_ref(),
                    )?
                    .p1()
                    .copied();
                    if file_node == tree_node {
                        status_builder.forget(path);
                        continue;
                    } else {
                        let paths = vec![path.to_owned()];
                        status_builder = match (tree_node, file_node) {
                            (None, Some(_)) => status_builder.added(paths),
                            (Some(_), None) => status_builder.removed(paths),
                            (None, None) => status_builder,
                            (Some(_), Some(_)) => status_builder.modified(paths),
                        };
                    }
                }
            }
        }

        let status = status_builder.build();

        span.record("status_len", status.len());

        Ok(status)
    }

    // Filter out modified symlinks where it appears the symlink has
    // been modified to no longer be a symlink. This happens often on
    // Windows because we don't materialize symlinks in the working
    // copy. The comment in Python's _filtersuspectsymlink suggests it
    // can also happen on network mounts.
    fn filter_accidental_symlink_changes(
        &self,
        mut status_builder: StatusBuilder,
        manifest: &impl Manifest,
    ) -> Result<StatusBuilder> {
        let mut override_clean = Vec::new();
        for (path, status) in status_builder.iter() {
            if status != FileStatus::Modified {
                continue;
            }

            let file_metadata = match manifest.get_file_if_visible(path)? {
                Some(md) => md,
                None => continue,
            };

            if file_metadata.file_type != FileType::Symlink {
                continue;
            }

            let data = self.vfs.read(path)?;
            if data.is_empty() || data.len() >= 1024 || data.iter().any(|b| *b == b'\n' || *b == 0)
            {
                override_clean.push(path.to_owned());
            }
        }

        if !override_clean.is_empty() {
            status_builder = status_builder.clean(override_clean)
        }

        Ok(status_builder)
    }

    /// Parse the `.gitmodules` config from the working copy.
    ///
    /// Respect submodule related configs. If git.submodules=false, or the repo
    /// does not use git format, return an empty list.
    pub fn parse_submodule_config(&self) -> Result<Vec<Submodule>> {
        if !self.support_submodules {
            tracing::debug!(target: "workingcopy::submodule", "submodules are disabled");
            return Ok(Vec::new());
        }

        let git_modules_path = self.vfs.join(".gitmodules".try_into()?);
        let parsed = if git_modules_path.exists() {
            let origin_url = self.config.get("paths", "default");
            let parsed = parse_gitmodules(
                &fs_err::read(&git_modules_path)?,
                origin_url.as_deref(),
                Some(&self.config),
            );
            tracing::debug!(target: "workingcopy::submodule", "parsed {} submodules", parsed.len());
            parsed
        } else {
            tracing::debug!(target: "workingcopy::submodule", ".gitmodules does not exist");
            Vec::new()
        };

        Ok(parsed)
    }

    /// Synthesize `Submodule` entries from the grepo projects in the workingcopy.
    pub fn parse_grepo_submodules(&self) -> Result<Vec<Submodule>> {
        let manifest_path = grepo_manifest_path(&self.vfs, &self.config)?;
        let manifest = parse_grepo_manifest(&manifest_path)?;

        manifest
            .projects
            .iter()
            .map(|(path, project)| {
                Ok(Submodule {
                    name: project.name.clone(),
                    url: "".to_string(), // Can probably be inferred but not useful for now
                    path: path.to_string_lossy().into_owned(),
                    r#ref: project
                        .upstream
                        .clone()
                        .or_else(|| project.revision.clone()),
                    active: true,
                })
            })
            .collect()
    }

    pub fn copymap(&self, matcher: DynMatcher) -> Result<Vec<(RepoPathBuf, RepoPathBuf)>> {
        let mut copied: Vec<(RepoPathBuf, RepoPathBuf)> = Vec::new();

        walk_treestate(
            &mut self.treestate.lock(),
            matcher,
            StateFlags::COPIED,
            StateFlags::empty(),
            StateFlags::empty(),
            |path, state| {
                let copied_path = state
                    .copied
                    .clone()
                    .ok_or_else(|| anyhow!("Invalid treestate entry for {path}: missing copied from path on file with COPIED flag"))
                    .map(|p| p.into_vec())
                    .and_then(|p| RepoPathBuf::from_utf8(p).map_err(|e| anyhow!(e)))?;

                copied.push((path, copied_path));

                Ok(())
            },
        )?;

        Ok(copied)
    }

    /// For supported working copies, get the "client" that talks to the external
    /// "working copy" program for low-level access.
    pub fn working_copy_client(&self) -> Result<Arc<dyn WorkingCopyClient>> {
        match self.filesystem.lock().get_client() {
            Some(v) => Ok(v),
            None => anyhow::bail!("bug: working_copy_client() called on wrong type"),
        }
    }

    pub fn read_merge_state(&self) -> Result<Option<MergeState>> {
        // Conceptually it seems like read_merge_state should be on LockedWorkingCopy.
        // In practice, light weight operations such as status+morestatus read the
        // merge state without a lock, so we can't require a lock. The merge
        // state is written atomically so we won't see an incomplete merge
        // state, but if we read other state files without locking then things
        // can be inconsistent.

        MergeState::read(&self.dot_hg_path().join("merge/state2"))
    }

    pub fn active_bookmark(&self) -> Result<Option<String>> {
        Ok(read_to_string_if_exists(
            self.dot_hg_path.join(ACTIVE_BOOKMARK_FILE),
        )?)
    }

    pub fn watchman_client(&self) -> Result<Arc<watchman_client::Client>> {
        self.watchman_client.get()
    }

    pub fn config(&self) -> &Arc<dyn Config> {
        &self.config
    }

    /// Update the "parent change" callback.
    /// It will be called immediately with the current parents.
    pub fn set_notify_parents_change_func(
        &mut self,
        func: impl Fn(&[HgId]) -> Result<()> + 'static + Send + Sync,
    ) -> Result<()> {
        let func = Box::new(func);
        let parents = self.parents()?;
        (func)(&parents)?;
        self.notify_parents_change_func = Some(func);
        Ok(())
    }

    fn notify_parents_change(&self, parents: &[HgId]) -> Result<()> {
        if let Some(func) = self.notify_parents_change_func.as_ref() {
            (func)(parents)?;
        }
        Ok(())
    }
}

// Example:
// error.EdenError: error computing status: requested parent commit is out-of-date: requested 71060cd2999820e7c1e8cb85a48ef045b1ae79b4, but current parent commit is 01f208e3ffbfa4c32985e9247f26567bf2ec4683. Try running `eden doctor` to remediate
static EDENFS_STATUS_ERROR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"current parent commit is ([^.]*)\.").unwrap());

fn parse_edenfs_status_error(errmsg: &str) -> Option<HgId> {
    let caps = EDENFS_STATUS_ERROR_RE.captures(errmsg)?;
    let hash = caps.get(1)?;
    HgId::from_str(hash.as_str()).ok()
}

pub struct LockedWorkingCopy<'a> {
    dot_hg_path: LockedPath,
    wc: &'a WorkingCopy,
}

impl<'a> std::ops::Deref for LockedWorkingCopy<'a> {
    type Target = WorkingCopy;

    fn deref(&self) -> &Self::Target {
        self.wc
    }
}

impl<'a> LockedWorkingCopy<'a> {
    pub fn locked_dot_hg_path(&self) -> &LockedPath {
        &self.dot_hg_path
    }

    pub fn write_merge_state(&self, ms: &MergeState) -> Result<()> {
        let dir = self.dot_hg_path.join("merge");
        fs_err::create_dir_all(&dir)?;
        let mut f = util::file::atomic_open(&dir.join("state2"))?;
        ms.serialize(f.as_file())?;
        f.save()?;
        Ok(())
    }

    pub fn set_parents(&self, parents: Vec<HgId>, parent_tree_hash: Option<HgId>) -> Result<()> {
        debug!(?parents);

        let p1 = parents
            .first()
            .context("At least one parent is required for setting parents")?
            .clone();
        let p2 = parents.get(1).copied();
        self.treestate.lock().set_parents(&mut parents.iter())?;
        self.filesystem
            .lock()
            .set_parents(p1, p2, parent_tree_hash)?;
        self.wc.notify_parents_change(&parents)?;
        Ok(())
    }

    pub fn clear_merge_state(&self) -> Result<()> {
        let merge_state_dir = self.dot_hg_path().join("merge");
        if util::file::exists(&merge_state_dir)
            .context("clearing merge state")?
            .is_some()
        {
            fs_err::remove_dir_all(&merge_state_dir)?;
        }
        Ok(())
    }

    pub fn set_active_bookmark(&self, bm: Option<String>) -> Result<()> {
        let active_path = self.dot_hg_path.join(ACTIVE_BOOKMARK_FILE);
        match bm {
            Some(bm) => Ok(atomic_write(&active_path, |f| write!(f, "{bm}")).map(|_f| ())?),
            None => Ok(unlink_if_exists(&active_path)?),
        }
    }
}
