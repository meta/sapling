/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Worktree registry: data model and persistence for worktree groups.
//!
//! This crate manages the JSON registry that tracks which EdenFS-backed
//! working copies belong together in a worktree group. It is intentionally
//! kept separate from the `cmdworktree` command implementation so that
//! other crates (e.g., `clone`, Python bindings for smartlog) can access
//! worktree group information without pulling in command-layer dependencies.

mod backend;
mod disk;

use std::cell::Cell;
use std::collections::BTreeMap;
use std::fmt::Write as _;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt as _;
#[cfg(target_os = "macos")]
use std::os::unix::fs::MetadataExt as _;
#[cfg(windows)]
use std::os::windows::ffi::OsStrExt as _;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use anyhow::Context;
use anyhow::Result;
use blake2::Blake2s256;
use blake2::Digest;
use configmodel::Config;
use fs_err as fs;
use repolock::LockError;
use repolock::LockHandle;
use serde::Deserialize;
use serde::Serialize;
use util::lock::PathLock;

pub use crate::backend::ListedWorktree;
pub use crate::backend::Worktrees;
pub use crate::disk::check_disk_space;

// --- Data Model ---

#[derive(Serialize, Deserialize)]
pub struct Registry {
    pub version: u32,
    pub groups: BTreeMap<String, Group>,
}

#[derive(Serialize, Deserialize)]
pub struct Group {
    pub main: PathBuf,
    pub worktrees: BTreeMap<PathBuf, WorktreeEntry>,
}

#[derive(Serialize, Deserialize)]
pub struct WorktreeEntry {
    pub added: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Worktree registry information for a single checkout.
///
/// Note: `Group::new` inserts the main checkout into `worktrees`, so both counts
/// include the main checkout, not just linked worktrees.
#[derive(Debug, Eq, PartialEq)]
pub struct WorktreeInfo {
    /// Whether this checkout is a linked worktree (i.e. not the group's main checkout).
    pub is_linked: bool,
    /// Total number of checkouts in this checkout's group (main + linked).
    pub worktree_count_group: usize,
    /// Total number of checkouts across all groups in the registry (main + linked).
    pub worktree_count: usize,
}

/// In-flight `worktree add` slot reservations, keyed by a unique per-add
/// reservation id (see [`new_reservation_id`]).
///
/// Persisted in a **separate** file (`worktree-reservations.json`), *not* inside
/// `worktrees.json`. This separation is deliberate and load-bearing:
///
/// * An older `sl` binary that predates reservations rewrites `worktrees.json`
///   whenever it prunes on `list`/`remove`. If reservations lived there it would
///   silently drop them, deleting a concurrent add's slot and re-opening the
///   over-limit race. Old binaries never open `worktree-reservations.json`, so
///   they cannot clobber it.
/// * `worktrees.json` therefore keeps its existing (version 1) shape, so no
///   on-disk migration or version bump is needed.
///
/// A reservation is recorded (under the registry lock) *before* the destination
/// checkout is cloned, counts against `worktree.max-count`, and is removed once
/// the add finishes (or fails). Keying by a unique id (rather than by
/// destination) ensures each add only ever releases its own reservation: two
/// adds targeting the *same* destination get distinct ids, so one aborting
/// cannot free the other's slot.
#[derive(Serialize, Deserialize, Default)]
pub struct Reservations {
    #[serde(default)]
    pub reservations: BTreeMap<String, Reservation>,
}

/// A single reserved slot held by an in-progress `worktree add`.
#[derive(Serialize, Deserialize)]
pub struct Reservation {
    /// Canonical main path of the group whose limit this slot counts against.
    pub group_main: PathBuf,
    /// Destination checkout path (for observability/debugging).
    pub dest: PathBuf,
    /// RFC 3339 timestamp of when the slot was reserved. Used to expire stale
    /// reservations orphaned by a crashed `worktree add` so they cannot consume
    /// a slot forever (see [`Reservations::prune_stale`]).
    pub added: String,
}

/// Default time a `worktree add` slot reservation stays valid, in seconds (1
/// hour). Overridable via the `worktree.reservation-ttl` config. A reservation
/// is created just before cloning the destination and removed once the add
/// finishes (or fails); anything older than this was orphaned by a crash and is
/// pruned so it cannot consume a slot forever. The default is sized generously
/// so a slow clone or snapshot restore is never mistaken for a crash.
pub const DEFAULT_RESERVATION_TTL_SECONDS: i64 = 60 * 60;

/// Per-process counter making reservation ids unique even if two are created
/// within the same nanosecond in one process.
static RESERVATION_SEQ: AtomicU64 = AtomicU64::new(0);

/// Generate a globally-unique id for a `worktree add` slot reservation.
///
/// Combines the process id, a high-resolution timestamp, and a per-process
/// counter. Concurrent adds run in separate processes (distinct pids), and a
/// crashed add whose pid is later reused differs in the timestamp, so two live
/// reservations never collide on the same id. Keying reservations by this id
/// (instead of by destination) means each add releases only its own slot.
pub fn new_reservation_id() -> String {
    let seq = RESERVATION_SEQ.fetch_add(1, Ordering::Relaxed);
    let now = chrono::Utc::now();
    let nanos = now
        .timestamp_nanos_opt()
        .unwrap_or_else(|| now.timestamp_micros());
    format!("{}-{}-{}", std::process::id(), nanos, seq)
}

impl Registry {
    pub fn new() -> Self {
        Self {
            version: 1,
            groups: BTreeMap::new(),
        }
    }

    pub fn find_group_for_path(&self, path: &Path) -> Option<String> {
        self.groups
            .iter()
            .find(|(_, group)| group.worktrees.contains_key(path))
            .map(|(id, _)| id.clone())
    }
}

impl Group {
    pub fn new(main_path: PathBuf) -> Self {
        let mut worktrees = BTreeMap::new();
        worktrees.insert(
            main_path.clone(),
            WorktreeEntry {
                added: chrono::Utc::now().to_rfc3339(),
                label: None,
            },
        );
        Self {
            main: main_path,
            worktrees,
        }
    }

    /// Number of linked worktrees in the group, excluding the main worktree.
    pub fn linked_worktree_count(&self) -> usize {
        self.worktrees.keys().filter(|p| *p != &self.main).count()
    }
}

impl Reservations {
    /// Drop reservations older than `ttl_seconds`. A reservation only lives for
    /// the duration of a single `worktree add`; anything older was almost
    /// certainly orphaned by a crashed add and must not keep consuming a slot.
    pub fn prune_stale(&mut self, ttl_seconds: i64) {
        let cutoff = chrono::Utc::now() - chrono::Duration::seconds(ttl_seconds);
        self.reservations.retain(|_, reservation| {
            match chrono::DateTime::parse_from_rfc3339(&reservation.added) {
                Ok(added) => added.with_timezone(&chrono::Utc) > cutoff,
                // Keep entries whose timestamp we cannot parse: dropping one
                // would re-open the race the reservation exists to prevent. In
                // practice this never happens since we always write RFC 3339.
                Err(_) => true,
            }
        });
    }

    /// Number of active reservations counting against `group_main`'s limit.
    /// Call [`Reservations::prune_stale`] first to exclude orphaned slots.
    pub fn count_for_group(&self, group_main: &Path) -> usize {
        self.reservations
            .values()
            .filter(|reservation| reservation.group_main == group_main)
            .count()
    }
}

impl Reservation {
    /// Create a reservation for `group_main`/`dest` stamped with the current time.
    pub fn now(group_main: PathBuf, dest: PathBuf) -> Self {
        Self {
            group_main,
            dest,
            added: chrono::Utc::now().to_rfc3339(),
        }
    }
}

const GROUP_ID_NAMESPACE: &[u8] = b"group-id";
const WORKTREE_OP_LOCK_NAMESPACE: &[u8] = b"worktree-op-lock";

std::thread_local! {
    static REGISTRY_LOCK_DEPTH: Cell<usize> = const { Cell::new(0) };
}

struct RegistryLockScope;

impl RegistryLockScope {
    fn enter() -> Result<Self> {
        if REGISTRY_LOCK_DEPTH.with(|depth| depth.get() > 0) {
            anyhow::bail!("cannot acquire registry lock while already holding registry lock");
        }
        REGISTRY_LOCK_DEPTH.with(|depth| depth.set(depth.get() + 1));
        Ok(Self)
    }
}

impl Drop for RegistryLockScope {
    fn drop(&mut self) {
        REGISTRY_LOCK_DEPTH.with(|depth| depth.set(depth.get() - 1));
    }
}

// These derived names must be stable outside a single process: group IDs are
// written to the registry, and lock names are used for on-disk coordination.
// Callers are expected to pass the same canonical path spelling they use when
// reading or writing the registry; this helper only does lexical cleanup
// (`.`, `..`, duplicate separators, `\\?\` stripping), not symlink resolution.
// The ids intentionally model destination-path identity before the checkout
// exists, so they follow host path semantics rather than the checkout's future
// case-sensitive/case-insensitive mount setting.
//
// `OsStr::as_encoded_bytes()` is only documented for round-tripping within the
// same Rust version and target platform:
// https://doc.rust-lang.org/std/ffi/struct.OsStr.html#method.as_encoded_bytes
// Convert to a stable platform representation before hashing.
#[cfg(all(unix, not(target_os = "macos")))]
fn update_stable_path_bytes(hasher: &mut Blake2s256, path: &Path) {
    hasher.update(path.as_os_str().as_bytes());
}

#[cfg(windows)]
fn update_stable_path_bytes(hasher: &mut Blake2s256, path: &Path) {
    // EdenFS clone/config path handling on Windows follows the platform's
    // usual case-insensitive path identity, so case-only spelling differences
    // should coordinate on the same derived id.
    let normalized = path.to_string_lossy().to_lowercase();
    for unit in normalized.encode_utf16() {
        hasher.update(unit.to_le_bytes());
    }
}

#[cfg(target_os = "macos")]
fn update_case_folded_unix_path_bytes(hasher: &mut Blake2s256, path: &Path) {
    for chunk in path.as_os_str().as_bytes().utf8_chunks() {
        if !chunk.valid().is_empty() {
            let normalized = chunk.valid().to_lowercase();
            hasher.update(normalized.as_bytes());
        }
        if !chunk.invalid().is_empty() {
            hasher.update(chunk.invalid());
        }
    }
}

#[cfg(target_os = "macos")]
fn detect_case_sensitive_existing_ancestor(path: &Path) -> Result<bool> {
    let mut probe = util::path::absolute(path)?;
    while !probe.exists() {
        if !probe.pop() {
            return Ok(true);
        }
    }
    detect_case_sensitive(&probe)
}

#[cfg(target_os = "macos")]
fn detect_case_sensitive(path: &Path) -> Result<bool> {
    let original = path.symlink_metadata()?;
    let Some(path_str) = path.to_str() else {
        return Ok(true);
    };
    let lowercase = path_str.to_lowercase();
    let case_variant = if lowercase != path_str {
        lowercase
    } else {
        let uppercase = path_str.to_uppercase();
        if uppercase == path_str {
            return Ok(true);
        }
        uppercase
    };
    let variant = match Path::new(&case_variant).symlink_metadata() {
        Ok(metadata) => metadata,
        Err(_) => return Ok(true),
    };
    Ok(original.dev() != variant.dev() || original.ino() != variant.ino())
}

#[cfg(target_os = "macos")]
fn update_stable_path_bytes(hasher: &mut Blake2s256, path: &Path) {
    if detect_case_sensitive_existing_ancestor(path).unwrap_or(true) {
        hasher.update(path.as_os_str().as_bytes());
    } else {
        update_case_folded_unix_path_bytes(hasher, path);
    }
}

#[cfg(not(any(unix, windows)))]
fn update_stable_path_bytes(hasher: &mut Blake2s256, path: &Path) {
    let normalized = path.to_string_lossy();
    hasher.update(normalized.as_bytes());
}

// Build an opaque deterministic identifier from a path. The identifier is
// process-independent so multiple racing commands can derive the same group id
// or per-path lock file name before touching the registry.
fn stable_path_id(domain: &[u8], path: &Path) -> String {
    let normalized_path = util::path::strip_unc_prefix(util::path::normalize(path));
    let mut hasher = Blake2s256::new();
    hasher.update(domain);
    hasher.update([0]);
    // Hash a path representation we control rather than Rust's opaque
    // OsStr encoding, since these ids are persisted and used cross-process.
    update_stable_path_bytes(&mut hasher, &normalized_path);

    // Truncate to 128 bits to keep ids compact while preserving opaque, deterministic names.
    let digest = hasher.finalize();
    let mut id = String::with_capacity(32);
    for byte in &digest[..16] {
        write!(&mut id, "{byte:02x}").expect("writing to String cannot fail");
    }
    id
}

/// Derive the registry group id from the canonical main worktree path.
///
/// The first `worktree add` that creates a group and any concurrent racers must
/// independently pick the same id without first consulting shared state.
pub fn group_id_for_main_path(main_path: &Path) -> String {
    stable_path_id(GROUP_ID_NAMESPACE, main_path)
}

fn worktree_path_lockfile_name(worktree_path: &Path) -> String {
    // Keep the lock under the shared store so callers can coordinate on a
    // target path before the worktree itself exists on disk. `repolock` locks
    // `<name>.lock`, the file older `sl` binaries lock.
    format!(
        "worktree-op-{}",
        stable_path_id(WORKTREE_OP_LOCK_NAMESPACE, worktree_path)
    )
}

/// Take the per-worktree operation lock for `worktree_path`.
///
/// Holding it makes the caller the only worktree command that may create,
/// label, or remove the checkout at that path. Like the repository locks, a
/// contended lock shows its holder while waiting and gives up after
/// `ui.timeout`.
///
/// Intended composition:
/// 1. Take this lock around the long-running filesystem / EdenFS operation for
///    a specific worktree path.
/// 2. Enter `with_registry_lock()` only for the short read-modify-write of
///    `worktrees.json`.
pub fn lock_worktree_path_op(
    config: &dyn Config,
    shared_store_path: &Path,
    worktree_path: &Path,
) -> Result<LockHandle> {
    let registry_lock_held = REGISTRY_LOCK_DEPTH.with(|depth| depth.get() > 0);
    if registry_lock_held {
        anyhow::bail!("cannot acquire worktree path operation lock while holding registry lock");
    }
    let name = worktree_path_lockfile_name(worktree_path);
    repolock::lock_with_contents(config, shared_store_path, &name, "worktree").map_err(|err| {
        match err {
            LockError::Contended(err) => {
                let holder = if err.contents.is_empty() {
                    "another sl process".into()
                } else {
                    util::utf8::escape_non_utf8(&err.contents)
                };
                anyhow::anyhow!(
                    "timed out waiting for lock on '{}' held by {holder}",
                    worktree_path.display(),
                )
            }
            err => err.into(),
        }
    })
}

/// Hold the per-worktree operation lock for `worktree_path` while running `f`;
/// see [`lock_worktree_path_op`].
pub fn with_worktree_path_op_lock<T>(
    config: &dyn Config,
    shared_store_path: &Path,
    worktree_path: &Path,
    f: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let _lock = lock_worktree_path_op(config, shared_store_path, worktree_path)?;
    f()
}

// --- Worktree-name marker ---

/// Filename of the per-worktree name marker, written into the worktree's dot
/// directory (e.g., `.sl/worktreename`).
///
/// Read by external tools (notably `eden/scm/contrib/scm-prompt.sh`) to display
/// the worktree's name in the shell prompt without consulting the registry.
const WORKTREE_NAME_FILE: &str = "worktreename";

/// Compute what the worktree name marker should contain: the label if non-empty,
/// otherwise the basename of the worktree path.
fn worktree_name_marker_content(worktree_path: &Path, label: Option<&str>) -> String {
    label
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            worktree_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        })
}

/// Write the worktree-name marker file at `<worktree_dot_dir>/worktreename`.
///
/// `worktree_path` is the canonical path of the worktree's working copy root
/// (used for the basename fallback when `label` is `None` or empty).
pub fn write_worktree_name_marker(
    worktree_path: &Path,
    worktree_dot_dir: &Path,
    label: Option<&str>,
) -> Result<()> {
    let content = worktree_name_marker_content(worktree_path, label);
    let path = worktree_dot_dir.join(WORKTREE_NAME_FILE);
    fs::write(&path, &content)
        .with_context(|| format!("failed to write worktree-name marker at {}", path.display()))?;
    Ok(())
}

// --- Validation ---

/// Verify that `dest` is not inside an existing source control checkout.
///
/// Walks from `dest` up to the filesystem root, checking each ancestor for
/// SCM marker directories (`.hg`, `.sl`, `.git`, `.svn`). Non-existent
/// intermediates are skipped — only the marker check matters, since
/// `Path::join().exists()` already returns false when the parent doesn't exist.
pub fn check_dest_not_in_repo(dest: &Path) -> Result<()> {
    const SCM_MARKERS: &[&str] = &[".hg", ".sl", ".git", ".svn"];
    for parent in dest.ancestors().skip(1) {
        for marker in SCM_MARKERS {
            if parent.join(marker).exists() {
                anyhow::bail!(
                    "destination '{}' is inside an existing checkout at {}",
                    dest.display(),
                    parent.display()
                );
            }
        }
    }
    Ok(())
}

// --- Registry Persistence ---

pub fn load_registry(shared_store_path: &Path) -> Result<Registry> {
    Ok(read_registry(shared_store_path)?.0)
}

/// Load the registry along with the file content it was parsed from, or `None`
/// if the file does not exist.
fn read_registry(shared_store_path: &Path) -> Result<(Registry, Option<String>)> {
    let path = shared_store_path.join("worktrees.json");
    match fs::read_to_string(&path) {
        // Before registry writes were fsynced, a host crash could leave the
        // renamed file empty. It holds no groups, and `with_registry_lock`
        // rewrites it because its content differs from the serialized registry.
        Ok(content) if content.is_empty() => {
            tracing::warn!("worktree registry {} is empty", path.display());
            Ok((Registry::new(), Some(content)))
        }
        Ok(content) => {
            let registry: Registry = serde_json::from_str(&content)
                .with_context(|| format!("failed to parse registry at {}", path.display()))?;
            Ok((registry, Some(content)))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((Registry::new(), None)),
        Err(e) => Err(e.into()),
    }
}

/// Return worktree group information for `repo_path`, if it is registered.
pub fn worktree_info(shared_store_path: &Path, repo_path: &Path) -> Result<Option<WorktreeInfo>> {
    let current = util::path::strip_unc_prefix(fs::canonicalize(repo_path).with_context(|| {
        format!(
            "failed to canonicalize repository path {}",
            repo_path.display()
        )
    })?);
    let registry = load_registry(shared_store_path)?;
    let Some(group_id) = registry.find_group_for_path(&current) else {
        return Ok(None);
    };
    let worktree_count = registry
        .groups
        .values()
        .map(|group| group.worktrees.len())
        .sum();

    Ok(registry.groups.get(&group_id).map(|group| WorktreeInfo {
        is_linked: current != group.main,
        worktree_count_group: group.worktrees.len(),
        worktree_count,
    }))
}

pub fn save_registry(shared_store_path: &Path, registry: &Registry) -> Result<()> {
    write_registry(shared_store_path, &serialize_registry(registry)?)
}

fn serialize_registry(registry: &Registry) -> Result<String> {
    serde_json::to_string_pretty(registry).context("failed to serialize registry")
}

fn write_registry(shared_store_path: &Path, content: &str) -> Result<()> {
    let path = shared_store_path.join("worktrees.json");
    // fsync so a host crash can't leave a renamed but unflushed, empty
    // registry, which every later worktree command would fail to parse.
    atomicfile::atomic_write(&path, 0o644, true, |f| {
        use std::io::Write;
        f.write_all(content.as_bytes())
    })
    .with_context(|| format!("failed to write registry at {}", path.display()))?;
    Ok(())
}

pub fn dissolve_group(registry: &mut Registry, group_id: &str) {
    registry.groups.remove(group_id);
}

/// Remove `group_id` from the registry if it has no linked worktrees.
///
/// In-flight reservations live in a separate file and are not consulted here: if
/// an add is mid-flight when its group is dissolved, its later registration
/// simply recreates the group. Reservations count by group main path, so they
/// remain enforced across the dissolve.
pub fn dissolve_group_if_empty(registry: &mut Registry, group_id: &str) {
    let should_dissolve = registry
        .groups
        .get(group_id)
        .is_some_and(|group| group.linked_worktree_count() == 0);
    if should_dissolve {
        dissolve_group(registry, group_id);
    }
}

/// Lock the registry file, load it, run `f`, and write back the result if `f`
/// changed it.
///
/// This lock is intentionally coarse and should stay scoped to the
/// `worktrees.json` read-modify-write sequence. Callers that need to serialize
/// longer operations for a specific worktree path should do that with
/// `with_worktree_path_op_lock()` and then use this helper only for the final
/// registry update.
pub fn with_registry_lock<T>(
    shared_store_path: &Path,
    f: impl FnOnce(&mut Registry) -> Result<T>,
) -> Result<T> {
    let _scope = RegistryLockScope::enter()?;
    let lock_path = shared_store_path.join("worktrees.lock");
    let _lock = PathLock::exclusive(&lock_path)?;
    let (mut registry, old_content) = read_registry(shared_store_path)?;
    let result = f(&mut registry)?;
    if let Some(new_content) = changed_registry_content(old_content.as_deref(), &registry)? {
        write_registry(shared_store_path, &new_content)?;
    }
    Ok(result)
}

/// Serialize `registry` if that changes the registry file, whose content is
/// `old_content` (`None` if there is no file).
fn changed_registry_content(
    old_content: Option<&str>,
    registry: &Registry,
) -> Result<Option<String>> {
    let new_content = serialize_registry(registry)?;
    let unchanged = match old_content {
        Some(old_content) => old_content == new_content,
        None => registry.groups.is_empty(),
    };
    Ok((!unchanged).then_some(new_content))
}

// --- Reservation Persistence ---
//
// Reservations are stored in their own file, separate from `worktrees.json`, so
// that an older `sl` binary rewriting the registry cannot drop them (see the
// `Reservations` doc comment).

const RESERVATIONS_FILE: &str = "worktree-reservations.json";

pub fn load_reservations(shared_store_path: &Path) -> Result<Reservations> {
    let path = shared_store_path.join(RESERVATIONS_FILE);
    match fs::read_to_string(&path) {
        Ok(content) => {
            let reservations: Reservations = serde_json::from_str(&content)
                .with_context(|| format!("failed to parse reservations at {}", path.display()))?;
            Ok(reservations)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Reservations::default()),
        Err(e) => Err(e.into()),
    }
}

pub fn save_reservations(shared_store_path: &Path, reservations: &Reservations) -> Result<()> {
    let path = shared_store_path.join(RESERVATIONS_FILE);
    let content =
        serde_json::to_string_pretty(reservations).context("failed to serialize reservations")?;
    util::file::atomic_write(&path, |f| {
        use std::io::Write;
        f.write_all(content.as_bytes())
    })?;
    Ok(())
}

/// Run `f` holding the registry lock, giving it read-only access to the registry
/// (for the linked-worktree count) and mutable access to the reservations. Only
/// the reservations file is written back — `worktrees.json` is left untouched,
/// so this never races an older `sl` binary that rewrites the registry.
///
/// The same lock file as [`with_registry_lock`] is used, so reservation and
/// registry updates serialize against each other and the linked-worktree count
/// observed here is consistent with concurrent registrations.
pub fn with_reservations<T>(
    shared_store_path: &Path,
    f: impl FnOnce(&Registry, &mut Reservations) -> Result<T>,
) -> Result<T> {
    let _scope = RegistryLockScope::enter()?;
    let lock_path = shared_store_path.join("worktrees.lock");
    let _lock = PathLock::exclusive(&lock_path)?;
    let registry = load_registry(shared_store_path)?;
    let mut reservations = load_reservations(shared_store_path)?;
    let result = f(&registry, &mut reservations)?;
    save_reservations(shared_store_path, &reservations)?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::ffi::OsStr;

    use super::*;

    #[test]
    fn test_registry_new() {
        let reg = Registry::new();
        assert_eq!(reg.version, 1);
        assert!(reg.groups.is_empty());
    }

    #[test]
    fn test_group_new() {
        let main_path = PathBuf::from("/tmp/test_repo");
        let group = Group::new(main_path.clone());
        assert_eq!(group.main, main_path);
        assert_eq!(group.worktrees.len(), 1);
        let entry = group.worktrees.get(&main_path).unwrap();
        assert!(entry.label.is_none());
        assert!(!entry.added.is_empty());
    }

    // --- check_dest_not_in_repo tests ---

    #[test]
    fn test_check_dest_not_in_repo_clean() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("my_worktree");
        assert!(check_dest_not_in_repo(&dest).is_ok());
    }

    #[test]
    fn test_check_dest_not_in_repo_hg() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".hg")).unwrap();
        let dest = dir.path().join("sub").join("worktree");
        let err = check_dest_not_in_repo(&dest).unwrap_err();
        assert!(format!("{err}").contains("inside an existing checkout"));
    }

    #[test]
    fn test_check_dest_not_in_repo_sl() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".sl")).unwrap();
        let dest = dir.path().join("worktree");
        let err = check_dest_not_in_repo(&dest).unwrap_err();
        assert!(format!("{err}").contains("inside an existing checkout"));
    }

    #[test]
    fn test_check_dest_not_in_repo_git() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        let dest = dir.path().join("worktree");
        let err = check_dest_not_in_repo(&dest).unwrap_err();
        assert!(format!("{err}").contains("inside an existing checkout"));
    }

    // --- Registry tests ---

    #[test]
    fn test_load_registry_missing() {
        let dir = tempfile::tempdir().unwrap();
        let reg = load_registry(dir.path()).unwrap();
        assert_eq!(reg.version, 1);
        assert!(reg.groups.is_empty());
    }

    #[test]
    fn test_worktree_info_roles() {
        let store_dir = tempfile::tempdir().unwrap();
        let checkouts_dir = tempfile::tempdir().unwrap();
        let main_path = checkouts_dir.path().join("main");
        let linked_path = checkouts_dir.path().join("linked");
        let unregistered_path = checkouts_dir.path().join("legacy-share");
        let other_main_path = checkouts_dir.path().join("other-main");
        let other_linked_path = checkouts_dir.path().join("other-linked");
        for path in [
            &main_path,
            &linked_path,
            &unregistered_path,
            &other_main_path,
            &other_linked_path,
        ] {
            std::fs::create_dir(path).unwrap();
        }

        assert_eq!(worktree_info(store_dir.path(), &main_path).unwrap(), None);

        let main_path = util::path::strip_unc_prefix(fs::canonicalize(main_path).unwrap());
        let linked_path = util::path::strip_unc_prefix(fs::canonicalize(linked_path).unwrap());
        let other_main_path = fs::canonicalize(other_main_path).unwrap();
        let other_linked_path = fs::canonicalize(other_linked_path).unwrap();
        let mut group = Group::new(main_path.clone());
        group.worktrees.insert(
            linked_path.clone(),
            WorktreeEntry {
                added: "2025-01-01T00:00:00Z".to_string(),
                label: None,
            },
        );
        let mut other_group = Group::new(other_main_path);
        other_group.worktrees.insert(
            other_linked_path,
            WorktreeEntry {
                added: "2025-01-01T00:00:00Z".to_string(),
                label: None,
            },
        );
        let mut registry = Registry::new();
        registry.groups.insert("test-group-id".to_string(), group);
        registry
            .groups
            .insert("other-group-id".to_string(), other_group);
        save_registry(store_dir.path(), &registry).unwrap();

        assert_eq!(
            worktree_info(store_dir.path(), &main_path).unwrap(),
            Some(WorktreeInfo {
                is_linked: false,
                worktree_count_group: 2,
                worktree_count: 4,
            })
        );
        assert_eq!(
            worktree_info(store_dir.path(), &linked_path).unwrap(),
            Some(WorktreeInfo {
                is_linked: true,
                worktree_count_group: 2,
                worktree_count: 4,
            })
        );
        assert_eq!(
            worktree_info(store_dir.path(), &unregistered_path).unwrap(),
            None
        );
    }

    #[test]
    fn test_save_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let mut reg = Registry::new();
        let main_path = PathBuf::from("/tmp/main_repo");
        let mut group = Group::new(main_path.clone());
        let linked_path = PathBuf::from("/tmp/linked_wt");
        group.worktrees.insert(
            linked_path.clone(),
            WorktreeEntry {
                added: "2025-01-01T00:00:00Z".to_string(),
                label: Some("feature-x".to_string()),
            },
        );
        reg.groups.insert("test-group-id".to_string(), group);

        save_registry(dir.path(), &reg).unwrap();
        let loaded = load_registry(dir.path()).unwrap();

        assert_eq!(loaded.version, 1);
        assert_eq!(loaded.groups.len(), 1);
        let grp = loaded.groups.get("test-group-id").unwrap();
        assert_eq!(grp.main, main_path);
        assert_eq!(grp.worktrees.len(), 2);
        let linked_entry = grp.worktrees.get(&linked_path).unwrap();
        assert_eq!(linked_entry.label.as_deref(), Some("feature-x"));
        assert_eq!(linked_entry.added, "2025-01-01T00:00:00Z");
    }

    #[test]
    fn test_check_dest_not_in_repo_svn() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".svn")).unwrap();
        let dest = dir.path().join("worktree");
        let err = check_dest_not_in_repo(&dest).unwrap_err();
        assert!(format!("{err}").contains("inside an existing checkout"));
    }

    #[test]
    fn test_check_dest_not_in_repo_deeply_nested() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        let dest = dir.path().join("a").join("b").join("c").join("d");
        let err = check_dest_not_in_repo(&dest).unwrap_err();
        assert!(format!("{err}").contains("inside an existing checkout"));
        assert!(format!("{err}").contains(&dir.path().display().to_string()));
    }

    #[test]
    fn test_check_dest_not_in_repo_root_level() {
        // Destination at filesystem root should succeed (no SCM markers above)
        let dest = PathBuf::from("/tmp/some_unique_worktree_test_path");
        assert!(check_dest_not_in_repo(&dest).is_ok());
    }

    // --- find_group_for_path tests ---

    #[test]
    fn test_find_group_for_path_found() {
        let mut reg = Registry::new();
        let main_path = PathBuf::from("/tmp/main");
        let linked_path = PathBuf::from("/tmp/linked");
        let mut group = Group::new(main_path.clone());
        group.worktrees.insert(
            linked_path.clone(),
            WorktreeEntry {
                added: "2025-01-01T00:00:00Z".to_string(),
                label: None,
            },
        );
        reg.groups.insert("group-1".to_string(), group);

        assert_eq!(
            reg.find_group_for_path(&main_path),
            Some("group-1".to_string())
        );
        assert_eq!(
            reg.find_group_for_path(&linked_path),
            Some("group-1".to_string())
        );
    }

    #[test]
    fn test_find_group_for_path_not_found() {
        let mut reg = Registry::new();
        let main_path = PathBuf::from("/tmp/main");
        reg.groups
            .insert("group-1".to_string(), Group::new(main_path));

        let unknown = PathBuf::from("/tmp/unknown");
        assert!(reg.find_group_for_path(&unknown).is_none());
    }

    #[test]
    fn test_find_group_for_path_multiple_groups() {
        let mut reg = Registry::new();
        let main_a = PathBuf::from("/tmp/repo_a");
        let main_b = PathBuf::from("/tmp/repo_b");
        reg.groups
            .insert("group-a".to_string(), Group::new(main_a.clone()));
        reg.groups
            .insert("group-b".to_string(), Group::new(main_b.clone()));

        assert_eq!(
            reg.find_group_for_path(&main_a),
            Some("group-a".to_string())
        );
        assert_eq!(
            reg.find_group_for_path(&main_b),
            Some("group-b".to_string())
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn test_group_id_for_main_path_is_stable() {
        let path = Path::new("/tmp/main");
        let id1 = group_id_for_main_path(path);
        let id2 = group_id_for_main_path(path);
        let other = group_id_for_main_path(Path::new("/tmp/other"));

        assert_eq!(id1, "a1931b83ce3c37d2c69e776d3436433f");
        assert_eq!(id1, id2);
        assert_ne!(id1, other);
    }

    #[test]
    fn test_group_id_for_main_path_normalizes_equivalent_spellings() {
        assert_eq!(
            group_id_for_main_path(Path::new("/tmp/repo/./main")),
            group_id_for_main_path(Path::new("/tmp/repo/main"))
        );
        assert_eq!(
            group_id_for_main_path(Path::new(r"\\?\C:\src\repo\main")),
            group_id_for_main_path(Path::new(r"C:\src\repo\main"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_group_id_for_main_path_preserves_non_utf8_bytes() {
        let path1 = Path::new(OsStr::from_bytes(b"/tmp/nonutf8-\x80"));
        let path2 = Path::new(OsStr::from_bytes(b"/tmp/nonutf8-\x81"));

        assert_ne!(group_id_for_main_path(path1), group_id_for_main_path(path2));
    }

    #[cfg(windows)]
    #[test]
    fn test_group_id_for_main_path_windows_drive_path_is_stable() {
        let path = Path::new(r"C:\src\repo\main");
        let id1 = group_id_for_main_path(path);
        let id2 = group_id_for_main_path(path);

        assert_eq!(id1, "0c365461676d023bda366cb242059c49");
        assert_eq!(id1, id2);
    }

    #[cfg(windows)]
    #[test]
    fn test_group_id_for_main_path_windows_paths_are_distinct() {
        let drive_path = Path::new(r"C:\src\repo\main");
        let other_drive_path = Path::new(r"D:\src\repo\main");
        let unc_path = Path::new(r"\\server\share\repo\main");

        assert_eq!(
            group_id_for_main_path(unc_path),
            "d6e031f169842e78da167212078603de"
        );
        assert_ne!(
            group_id_for_main_path(drive_path),
            group_id_for_main_path(other_drive_path)
        );
        assert_ne!(
            group_id_for_main_path(drive_path),
            group_id_for_main_path(unc_path)
        );
    }

    #[cfg(windows)]
    #[test]
    fn test_group_id_for_main_path_windows_case_is_folded() {
        assert_eq!(
            group_id_for_main_path(Path::new(r"C:\Src\Repo\Main")),
            group_id_for_main_path(Path::new(r"c:\src\repo\main"))
        );
    }

    #[test]
    fn test_lock_worktree_path_op_creates_lock_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = PathBuf::from("/tmp/some_worktree");
        let _lock =
            lock_worktree_path_op(&BTreeMap::<String, String>::new(), dir.path(), &path).unwrap();

        assert!(
            dir.path()
                .join(format!("{}.lock", worktree_path_lockfile_name(&path)))
                .exists()
        );
    }

    #[test]
    fn test_lock_worktree_path_op_different_paths_concurrent() {
        let dir = tempfile::tempdir().unwrap();
        let config = BTreeMap::<String, String>::new();
        let path1 = PathBuf::from("/tmp/worktree1");
        let path2 = PathBuf::from("/tmp/worktree2");

        let _lock1 = lock_worktree_path_op(&config, dir.path(), &path1).unwrap();
        let _lock2 = lock_worktree_path_op(&config, dir.path(), &path2).unwrap();
    }

    #[test]
    fn test_worktree_path_op_lock_timeout_names_holder() {
        let dir = tempfile::tempdir().unwrap();
        // `ui.timeout` is unset, so a contended lock gives up right away.
        let config = BTreeMap::<String, String>::new();
        let path = dir.path().join("worktree");
        let lock = lock_worktree_path_op(&config, dir.path(), &path).unwrap();
        let err = with_worktree_path_op_lock(&config, dir.path(), &path, || -> Result<()> {
            panic!("operation must not run while the path is locked")
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains("timed out waiting for lock on"), "{err}");
        assert!(err.contains(&std::process::id().to_string()), "{err}");
        drop(lock);

        let _lock = lock_worktree_path_op(&config, dir.path(), &path).unwrap();
    }

    #[test]
    fn test_worktree_path_op_lock_contends_with_older_binary_holder() {
        let dir = tempfile::tempdir().unwrap();
        let config = BTreeMap::<String, String>::new();
        let path = dir.path().join("worktree");
        // Older binaries lock the `.lock` file directly and record no holder.
        let old = PathLock::exclusive(
            dir.path()
                .join(format!("{}.lock", worktree_path_lockfile_name(&path))),
        )
        .unwrap();
        let err = lock_worktree_path_op(&config, dir.path(), &path)
            .map(|_lock| ())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("timed out waiting for lock on") && err.contains("another sl process"),
            "{err}"
        );
        drop(old);

        let _lock = lock_worktree_path_op(&config, dir.path(), &path).unwrap();
    }

    #[test]
    fn test_worktree_path_op_lock_waits_for_release() {
        let dir = tempfile::tempdir().unwrap();
        let config = BTreeMap::from([("ui.timeout", "60"), ("devel.lock_backoff", "0.001")]);
        let path = dir.path().join("worktree");
        let held = lock_worktree_path_op(&config, dir.path(), &path).unwrap();
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            drop(held);
        });

        let _lock = lock_worktree_path_op(&config, dir.path(), &path).unwrap();
        releaser.join().unwrap();
    }

    #[test]
    fn test_worktree_lock_then_registry_lock_is_allowed() {
        let dir = tempfile::tempdir().unwrap();
        let path = PathBuf::from("/tmp/worktree-lock-then-registry-lock");

        with_worktree_path_op_lock(
            &BTreeMap::<String, String>::new(),
            dir.path(),
            &path,
            || with_registry_lock(dir.path(), |_registry| Ok(())),
        )
        .unwrap();
    }

    #[test]
    fn test_registry_lock_then_worktree_lock_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = PathBuf::from("/tmp/registry-lock-then-worktree-lock");

        let err = with_registry_lock(dir.path(), |_registry| {
            lock_worktree_path_op(&BTreeMap::<String, String>::new(), dir.path(), &path)
                .map(|_lock| ())
        })
        .unwrap_err();

        assert!(
            err.to_string().contains(
                "cannot acquire worktree path operation lock while holding registry lock"
            )
        );
    }

    #[test]
    fn test_reservations_lock_then_worktree_lock_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = PathBuf::from("/tmp/reservations-lock-then-worktree-lock");

        let err = with_reservations(dir.path(), |_registry, _reservations| {
            lock_worktree_path_op(&BTreeMap::<String, String>::new(), dir.path(), &path)
                .map(|_lock| ())
        })
        .unwrap_err();

        assert!(
            err.to_string().contains(
                "cannot acquire worktree path operation lock while holding registry lock"
            )
        );
    }

    #[test]
    fn test_registry_lock_then_registry_lock_is_rejected() {
        let dir = tempfile::tempdir().unwrap();

        let err = with_registry_lock(dir.path(), |_registry| {
            with_registry_lock(dir.path(), |_registry| Ok(()))
        })
        .unwrap_err();

        assert!(
            err.to_string()
                .contains("cannot acquire registry lock while already holding registry lock")
        );
    }

    #[test]
    fn test_registry_lock_then_reservations_lock_is_rejected() {
        let dir = tempfile::tempdir().unwrap();

        let err = with_registry_lock(dir.path(), |_registry| {
            with_reservations(dir.path(), |_registry, _reservations| Ok(()))
        })
        .unwrap_err();

        assert!(
            err.to_string()
                .contains("cannot acquire registry lock while already holding registry lock")
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn test_worktree_path_lockfile_name_is_stable() {
        let path = Path::new("/tmp/consistent_path");
        let name1 = worktree_path_lockfile_name(path);
        let name2 = worktree_path_lockfile_name(path);
        let other = worktree_path_lockfile_name(Path::new("/tmp/other_path"));

        assert_eq!(name1, "worktree-op-418ede8060a3d4aa0723dc1ea9046340");
        assert_eq!(name1, name2);
        assert_ne!(name1, other);
    }

    #[test]
    fn test_worktree_path_lockfile_name_normalizes_equivalent_spellings() {
        assert_eq!(
            worktree_path_lockfile_name(Path::new("/tmp/repo/linked/../linked")),
            worktree_path_lockfile_name(Path::new("/tmp/repo/linked"))
        );
        assert_eq!(
            worktree_path_lockfile_name(Path::new(r"\\?\C:\src\repo\linked")),
            worktree_path_lockfile_name(Path::new(r"C:\src\repo\linked"))
        );
    }

    #[cfg(windows)]
    #[test]
    fn test_worktree_path_lockfile_name_windows_path_is_stable() {
        let path = Path::new(r"C:\src\repo\linked");
        let name1 = worktree_path_lockfile_name(path);
        let name2 = worktree_path_lockfile_name(path);

        assert_eq!(name1, "worktree-op-7cfc9b010bb34f6dfe81f449cbe366e8");
        assert_eq!(name1, name2);
        assert!(name1.starts_with("worktree-op-"));
    }

    #[cfg(windows)]
    #[test]
    fn test_worktree_path_lockfile_name_windows_paths_are_distinct() {
        let drive_path = Path::new(r"C:\src\repo\linked");
        let unc_path = Path::new(r"\\server\share\repo\linked");

        assert_eq!(
            worktree_path_lockfile_name(unc_path),
            "worktree-op-e35b48634bbcac02385a23320201f492"
        );
        assert_ne!(
            worktree_path_lockfile_name(drive_path),
            worktree_path_lockfile_name(unc_path)
        );
    }

    #[cfg(windows)]
    #[test]
    fn test_worktree_path_lockfile_name_windows_case_is_folded() {
        assert_eq!(
            worktree_path_lockfile_name(Path::new(r"C:\Src\Repo\Linked")),
            worktree_path_lockfile_name(Path::new(r"c:\src\repo\linked"))
        );
    }

    // --- with_registry_lock error propagation ---

    #[test]
    fn test_with_registry_lock_error_does_not_persist() {
        let dir = tempfile::tempdir().unwrap();
        let result: anyhow::Result<()> = with_registry_lock(dir.path(), |registry| {
            registry.groups.insert(
                "should-not-persist".to_string(),
                Group::new(PathBuf::from("/tmp/x")),
            );
            anyhow::bail!("simulated failure");
        });
        assert!(result.is_err());

        // Registry should still be empty since the closure failed.
        let loaded = load_registry(dir.path()).unwrap();
        assert!(loaded.groups.is_empty());
    }

    #[test]
    fn test_with_registry_lock_skips_unchanged_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("worktrees.json");

        // Without groups there is nothing to write.
        with_registry_lock(dir.path(), |_registry| Ok(())).unwrap();
        assert!(!path.exists());

        let mut registry = Registry::new();
        registry
            .groups
            .insert("grp".to_string(), Group::new(PathBuf::from("/tmp/main")));
        save_registry(dir.path(), &registry).unwrap();
        #[cfg(unix)]
        let inode = std::os::unix::fs::MetadataExt::ino(&std::fs::metadata(&path).unwrap());

        // An unchanged registry keeps the same file.
        with_registry_lock(dir.path(), |_registry| Ok(())).unwrap();
        #[cfg(unix)]
        assert_eq!(
            std::os::unix::fs::MetadataExt::ino(&std::fs::metadata(&path).unwrap()),
            inode
        );
        assert_eq!(load_registry(dir.path()).unwrap().groups.len(), 1);
    }

    // --- Other tests ---

    #[test]
    fn test_load_registry_malformed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("worktrees.json"), "not valid json!!!").unwrap();
        let result = load_registry(dir.path());
        assert!(result.is_err());
    }

    #[test]
    fn test_empty_registry_is_read_as_empty_and_rewritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("worktrees.json");
        std::fs::write(&path, "").unwrap();
        assert!(load_registry(dir.path()).unwrap().groups.is_empty());

        with_registry_lock(dir.path(), |_registry| Ok(())).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(serde_json::from_str::<Registry>(&content).is_ok());
    }

    #[test]
    fn test_with_registry_lock() {
        let dir = tempfile::tempdir().unwrap();
        with_registry_lock(dir.path(), |registry| {
            let main_path = PathBuf::from("/tmp/lock_test");
            registry
                .groups
                .insert("lock-group".to_string(), Group::new(main_path));
            Ok(())
        })
        .unwrap();

        // Verify changes were persisted.
        let loaded = load_registry(dir.path()).unwrap();
        assert_eq!(loaded.groups.len(), 1);
        assert!(loaded.groups.contains_key("lock-group"));
    }

    #[test]
    fn test_dissolve_group() {
        let mut registry = Registry::new();
        let main_path = PathBuf::from("/tmp/main_repo");
        let mut group = Group::new(main_path.clone());
        let linked_path = PathBuf::from("/tmp/linked_wt");
        group.worktrees.insert(
            linked_path,
            WorktreeEntry {
                added: "2025-01-01T00:00:00Z".to_string(),
                label: None,
            },
        );
        registry.groups.insert("grp1".to_string(), group);

        dissolve_group(&mut registry, "grp1");

        assert!(!registry.groups.contains_key("grp1"));
    }

    #[test]
    fn test_dissolve_group_nonexistent() {
        // Dissolving a group that doesn't exist should not panic.
        let mut registry = Registry::new();
        let main_path = PathBuf::from("/nonexistent/main");
        let group = Group::new(main_path);
        registry.groups.insert("grp2".to_string(), group);

        dissolve_group(&mut registry, "grp2");
        assert!(!registry.groups.contains_key("grp2"));
    }

    // --- Reservation tests ---

    fn stale_reservation(group_main: &str, dest: &str) -> Reservation {
        let stale =
            chrono::Utc::now() - chrono::Duration::seconds(DEFAULT_RESERVATION_TTL_SECONDS + 60);
        Reservation {
            group_main: PathBuf::from(group_main),
            dest: PathBuf::from(dest),
            added: stale.to_rfc3339(),
        }
    }

    #[test]
    fn test_load_registry_v1_has_no_reservations_field() {
        // A version 1 registry (the only on-disk shape) loads fine; reservations
        // are never stored in worktrees.json.
        let dir = tempfile::tempdir().unwrap();
        let legacy = r#"{
            "version": 1,
            "groups": {
                "grp1": {
                    "main": "/tmp/main",
                    "worktrees": {
                        "/tmp/main": {"added": "2025-01-01T00:00:00Z"}
                    }
                }
            }
        }"#;
        std::fs::write(dir.path().join("worktrees.json"), legacy).unwrap();

        let reg = load_registry(dir.path()).unwrap();
        assert_eq!(reg.version, 1);
        assert!(reg.groups.contains_key("grp1"));
    }

    #[test]
    fn test_registry_json_never_contains_reservations() {
        // Reservations live in their own file, so worktrees.json must never
        // gain a `reservations` key that an old binary would drop.
        let dir = tempfile::tempdir().unwrap();
        let mut reg = Registry::new();
        reg.groups
            .insert("grp1".to_string(), Group::new(PathBuf::from("/tmp/main")));
        save_registry(dir.path(), &reg).unwrap();

        let content = std::fs::read_to_string(dir.path().join("worktrees.json")).unwrap();
        assert!(!content.contains("reservation"));
    }

    #[test]
    fn test_reservations_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let mut reservations = Reservations::default();
        reservations.reservations.insert(
            "res-1".to_string(),
            Reservation::now(PathBuf::from("/tmp/main"), PathBuf::from("/tmp/pending")),
        );

        save_reservations(dir.path(), &reservations).unwrap();
        let loaded = load_reservations(dir.path()).unwrap();
        let res = loaded.reservations.get("res-1").unwrap();
        assert_eq!(res.group_main, PathBuf::from("/tmp/main"));
        assert_eq!(res.dest, PathBuf::from("/tmp/pending"));
    }

    #[test]
    fn test_load_reservations_missing_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            load_reservations(dir.path())
                .unwrap()
                .reservations
                .is_empty()
        );
    }

    #[test]
    fn test_new_reservation_id_is_unique() {
        assert_ne!(new_reservation_id(), new_reservation_id());
    }

    #[test]
    fn test_prune_stale() {
        let mut reservations = Reservations::default();
        // Fresh reservation is kept.
        reservations.reservations.insert(
            "res-fresh".to_string(),
            Reservation::now(PathBuf::from("/tmp/main"), PathBuf::from("/tmp/fresh")),
        );
        // A reservation stamped well beyond the TTL is dropped.
        reservations.reservations.insert(
            "res-stale".to_string(),
            stale_reservation("/tmp/main", "/tmp/stale"),
        );
        // An unparsable timestamp is conservatively kept.
        reservations.reservations.insert(
            "res-garbage".to_string(),
            Reservation {
                group_main: PathBuf::from("/tmp/main"),
                dest: PathBuf::from("/tmp/garbage"),
                added: "not-a-timestamp".to_string(),
            },
        );

        reservations.prune_stale(DEFAULT_RESERVATION_TTL_SECONDS);

        assert!(reservations.reservations.contains_key("res-fresh"));
        assert!(!reservations.reservations.contains_key("res-stale"));
        assert!(reservations.reservations.contains_key("res-garbage"));
    }

    #[test]
    fn test_count_for_group() {
        let mut reservations = Reservations::default();
        reservations.reservations.insert(
            "a".to_string(),
            Reservation::now(PathBuf::from("/tmp/main"), PathBuf::from("/tmp/a")),
        );
        reservations.reservations.insert(
            "b".to_string(),
            Reservation::now(PathBuf::from("/tmp/main"), PathBuf::from("/tmp/b")),
        );
        reservations.reservations.insert(
            "c".to_string(),
            Reservation::now(PathBuf::from("/tmp/other"), PathBuf::from("/tmp/c")),
        );

        assert_eq!(reservations.count_for_group(&PathBuf::from("/tmp/main")), 2);
        assert_eq!(
            reservations.count_for_group(&PathBuf::from("/tmp/other")),
            1
        );
        assert_eq!(reservations.count_for_group(&PathBuf::from("/tmp/none")), 0);
    }

    #[test]
    fn test_with_reservations_persists_only_reservations() {
        let dir = tempfile::tempdir().unwrap();
        // Seed a registry so with_reservations can read it; it must not rewrite it.
        with_registry_lock(dir.path(), |registry| {
            registry
                .groups
                .insert("grp1".to_string(), Group::new(PathBuf::from("/tmp/main")));
            Ok(())
        })
        .unwrap();

        with_reservations(dir.path(), |registry, reservations| {
            assert!(registry.groups.contains_key("grp1"));
            reservations.reservations.insert(
                "res-1".to_string(),
                Reservation::now(PathBuf::from("/tmp/main"), PathBuf::from("/tmp/pending")),
            );
            Ok(())
        })
        .unwrap();

        assert!(
            load_reservations(dir.path())
                .unwrap()
                .reservations
                .contains_key("res-1")
        );
    }

    #[test]
    fn test_linked_worktree_count_excludes_main() {
        let mut group = Group::new(PathBuf::from("/tmp/main"));
        assert_eq!(group.linked_worktree_count(), 0);
        group.worktrees.insert(
            PathBuf::from("/tmp/linked"),
            WorktreeEntry {
                added: "2025-01-01T00:00:00Z".to_string(),
                label: None,
            },
        );
        assert_eq!(group.linked_worktree_count(), 1);
    }

    #[test]
    fn test_dissolve_group_if_empty_removes_main_only_group() {
        let mut registry = Registry::new();
        registry
            .groups
            .insert("grp1".to_string(), Group::new(PathBuf::from("/tmp/main")));

        dissolve_group_if_empty(&mut registry, "grp1");
        assert!(!registry.groups.contains_key("grp1"));
    }

    #[test]
    fn test_dissolve_group_if_empty_keeps_group_with_linked_worktree() {
        let mut registry = Registry::new();
        let mut group = Group::new(PathBuf::from("/tmp/main"));
        group.worktrees.insert(
            PathBuf::from("/tmp/linked"),
            WorktreeEntry {
                added: "2025-01-01T00:00:00Z".to_string(),
                label: None,
            },
        );
        registry.groups.insert("grp1".to_string(), group);

        dissolve_group_if_empty(&mut registry, "grp1");
        assert!(registry.groups.contains_key("grp1"));
    }
}
