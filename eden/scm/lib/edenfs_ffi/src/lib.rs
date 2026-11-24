/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::LazyLock;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use anyhow::anyhow;
use configmodel::config::ConfigExt;
use cxx::UniquePtr;
use cxxerror::Result;
use filters::filter::FilterGenerator;
use metrics::Counter;
use parking_lot::Mutex;
use pathmatcher::AlwaysMatcher;
use pathmatcher::DirectoryMatch;
use repo::repo::Repo;
use sparse::Root;
use types::RepoPath;
use workingcopy::sparse::fetch_sparse_profile_content;

use crate::ffi::MatcherPromise;
use crate::ffi::set_matcher_promise_error;
use crate::ffi::set_matcher_promise_result;

struct CachedObjects {
    expiration: Option<Instant>,
    repo: Repo,
    filter_gen: FilterGenerator,
}

static OBJECT_CACHE: LazyLock<Mutex<HashMap<PathBuf, CachedObjects>>> = LazyLock::new(|| {
    let map = Mutex::new(HashMap::new());

    // Start the cleanup thread that checks for evictions every ~15 seconds
    thread::spawn(cleanup_expired_objects);

    map
});

static LOOKUPS: Counter = Counter::new_counter("edenffi.ffs.lookups");
static LOOKUP_FAILURES: Counter = Counter::new_counter("edenffi.ffs.lookup_failures");
static INVALID_REPO: Counter = Counter::new_counter("edenffi.ffs.invalid_repo");
static OBJECT_CACHE_MISSES: Counter = Counter::new_counter("edenffi.ffs.object_cache_misses");
static OBJECT_CACHE_HITS: Counter = Counter::new_counter("edenffi.ffs.object_cache_hits");
static OBJECT_CACHE_CLEANUPS: Counter = Counter::new_counter("edenffi.ffs.object_cache_cleanups");

const DEFAULT_CACHE_EXPIRY_DURATION: Duration = Duration::from_mins(5); // 5 minutes

// Background thread to clean up expired objects
fn cleanup_expired_objects() {
    loop {
        // Check for expired cache entries every 15 seconds
        thread::sleep(Duration::from_secs(15));

        let mut object_map = OBJECT_CACHE.lock();
        let now = Instant::now();
        let initial_count = object_map.len();

        // Only keep objects that aren't expired
        object_map
            .retain(|_path, cached_object| cached_object.expiration.is_none_or(|exp| now < exp));

        let final_count = object_map.len();
        drop(object_map);

        let cleaned_count = initial_count.saturating_sub(final_count);
        if cleaned_count > 0 {
            tracing::info!(
                "Cleaned up {} expired object entries from cache",
                cleaned_count
            );
            OBJECT_CACHE_CLEANUPS.add(cleaned_count);
        }
    }
}

fn get_or_create_cached_objects(
    abs_repo_path: &PathBuf,
    object_map: &mut HashMap<PathBuf, CachedObjects>,
) -> anyhow::Result<()> {
    if object_map.contains_key(abs_repo_path) {
        OBJECT_CACHE_HITS.increment();
        return Ok(());
    }

    OBJECT_CACHE_MISSES.increment();

    let repo = Repo::load(abs_repo_path, &[])
        .with_context(|| anyhow!("failed to load Repo object for {}", abs_repo_path.display()))?;

    let config = repo.config();
    // NOTE: This is technically wrong. The repository at abs_repo_path *is* the shared repo,
    // so we're passing in the same path for both arguments. This is only okay since the FFI
    // code never tries to read/write the .hg/sparse file directly.
    //
    // We *cannot* use the checkout's mount path to load the repo, since that leads to
    // deadlocks. Sapling acquires the lock for checkout, then the FFI layer tries to acquire
    // the lock for filter evaluation, and a deadlock occurs. Using the shared repo for filter
    // evaluation prevents this deadlock from occurring.
    let filter_gen = FilterGenerator::from_dot_dirs(
        repo.shared_dot_hg_path(),
        repo.shared_dot_hg_path(),
        config,
    )?;

    let ttl = repo
        .config()
        .must_get("edenfs", "ffs-repo-cache-ttl")
        .unwrap_or(DEFAULT_CACHE_EXPIRY_DURATION);
    let expiration = if ttl == Duration::ZERO {
        None
    } else {
        Some(Instant::now() + ttl)
    };

    object_map.insert(
        abs_repo_path.clone(),
        CachedObjects {
            expiration,
            repo,
            filter_gen,
        },
    );

    Ok(())
}

// CXX only allows exposing structures that are defined in the bridge crate.
// Therefore, MercurialMatcher simply serves as a wrapper around the actual Matcher object that's
// passed to C++ and back to Rust
pub struct MercurialMatcher {
    matcher: Box<dyn pathmatcher::Matcher>,
}

impl MercurialMatcher {
    // Returns true if the given path and all of its children are unfiltered
    fn matches_directory(self: &MercurialMatcher, path: &str) -> Result<ffi::FilterDirectoryMatch> {
        let repo_path = RepoPath::from_str(path)?;
        // This is tricky -- a filter file defines which files should be *excluded* from the repo.
        // The filtered files are put in the [exclude] section of the file. So, if something is
        // recursively unfiltered, then it means that there are no exclude patterns that match it.
        let res = self.matcher.matches_directory(repo_path)?;
        // EdenFS asks about paths of unknown type (journal entries, tree
        // entries of either type). `DirectoryMatch::Nothing` only says
        // nothing *under* the path matches -- ex. rule `*.cpp` and path
        // `a.cpp` -- so a path that itself matches as a file must not be
        // reported as recursively filtered.
        if res == DirectoryMatch::Nothing && self.matcher.matches_file(repo_path)? {
            return Ok(ffi::FilterDirectoryMatch::Unfiltered);
        }
        Ok(res.into())
    }
}

// It's safe to move MatcherPromises between threads
unsafe impl Send for MatcherPromise {}

// NOTE: While MercurialPromises are safe to move between threads, they cannot be shared between threads.
// Ex: calling setPromise from multiple threads is undefined. Therefore we should avoid marking
// MercurialMatcher as Sync. More info here: https://doc.rust-lang.org/stable/std/marker/trait.Sync.html

#[cxx::bridge]
mod ffi {

    pub enum FilterDirectoryMatch {
        RecursivelyFiltered,
        RecursivelyUnfiltered,
        Unfiltered,
    }

    unsafe extern "C++" {
        include!("eden/scm/lib/edenfs_ffi/include/ffi.h");

        #[namespace = "facebook::eden"]
        type MatcherPromise;

        #[namespace = "facebook::eden"]
        fn set_matcher_promise_result(
            promise: UniquePtr<MatcherPromise>,
            value: Box<MercurialMatcher>,
        );

        #[namespace = "facebook::eden"]
        fn set_matcher_promise_error(promise: UniquePtr<MatcherPromise>, error: String);
    }

    #[namespace = "facebook::eden"]
    extern "Rust" {
        type MercurialMatcher;

        // Takes a filter_id that corresponds to a filter file that's checked
        // into the repo.
        //
        // Note: The corresponding call in C++ will throw if the Rust function
        // returns an error result.
        fn profile_from_filter_id(
            id: &[u8],
            checkout_path: &str,
            promise: UniquePtr<MatcherPromise>,
        ) -> Result<()>;

        // Returns true if the given path and all of its children are unfiltered.
        fn matches_directory(self: &MercurialMatcher, path: &str) -> Result<FilterDirectoryMatch>;

        // Checks if two FilterIds are identical by parsing and comparing their
        // underlying Filter objects. Returns true if both FilterIds parse to
        // the same commit_id and filter_paths
        fn are_filter_ids_identical(lhs: &[u8], rhs: &[u8], checkout_path: &str) -> Result<bool>;
    }
}

impl From<DirectoryMatch> for ffi::FilterDirectoryMatch {
    fn from(dm: DirectoryMatch) -> Self {
        match dm {
            DirectoryMatch::Everything => Self::RecursivelyUnfiltered,
            DirectoryMatch::Nothing => Self::RecursivelyFiltered,
            DirectoryMatch::ShouldTraverse => Self::Unfiltered,
        }
    }
}

// Checks if two filterIds are identical by parsing and comparing their underlying Filter objects.
// This handles the case where a Legacy filterId might be semantically equivalent to a V1 filterId.
pub fn are_filter_ids_identical(lhs: &[u8], rhs: &[u8], checkout_path: &str) -> Result<bool> {
    // Quick check: if the bytes are identical, the filters are identical
    if lhs == rhs {
        return Ok(true);
    }

    let abs_repo_path = PathBuf::from(checkout_path);
    if identity::sniff_dir(&abs_repo_path).is_err() {
        return Err(anyhow!("{} is not a valid hg repo", abs_repo_path.display()).into());
    }

    let mut object_map = OBJECT_CACHE.lock();
    get_or_create_cached_objects(&abs_repo_path, &mut object_map)?;

    let cached_objects = object_map.get_mut(&abs_repo_path).context("loading repo")?;
    let filter_gen = &cached_objects.filter_gen;

    Ok(filter_gen.are_filter_ids_identical(lhs, rhs)?)
}

// As mentioned below, we return the MercurialMatcher via a promise to circumvent some async
// limitations in CXX. This function wraps the bulk of the Sparse logic and provides a single
// place for returning result/error info via the MatcherPromise.
fn profile_contents_from_repo(
    id: &[u8],
    abs_repo_path: PathBuf,
    promise: UniquePtr<MatcherPromise>,
) {
    // Try once, and if it fails, evict the (potentially stale) cache entry
    // and retry.
    let result = _profile_contents_from_repo(id, abs_repo_path.clone()).or_else(|first_err| {
        tracing::warn!(
            "Filter lookup failed, evicting cache and retrying: {:?}",
            first_err
        );
        invalidate_object_cache_for_repo(&abs_repo_path);
        _profile_contents_from_repo(id, abs_repo_path)
    });

    match result {
        Ok(res) => set_matcher_promise_result(promise, res),
        Err(e) => set_matcher_promise_error(promise, format!("Failed to get filter: {e:?}")),
    }
}

// Fetches the content of a filter file and turns it into a MercurialMatcher
fn _profile_contents_from_repo(
    id: &[u8],
    abs_repo_path: PathBuf,
) -> anyhow::Result<Box<MercurialMatcher>> {
    let mut object_map = OBJECT_CACHE.lock();
    get_or_create_cached_objects(&abs_repo_path, &mut object_map)?;

    let cached_objects = object_map.get_mut(&abs_repo_path).context("loading repo")?;
    let repo = &mut cached_objects.repo;
    let filter_gen = &mut cached_objects.filter_gen;

    let filter = filter_gen
        .get_filter_from_bytes(id)
        .with_context(|| anyhow!("failed to interpret bytes as valid filter: {id:?}"))?;

    // Create the tree manifest for the root tree of the repo
    let tree_manifest = match repo.tree_resolver()?.get(&filter.commit_id) {
        Ok(manifest_id) => manifest_id,
        Err(e) => {
            // It's possible that the commit exists but was only recently
            // created. Invalidate the in-memory commit graph and force a read
            // from disk. Note: This can be slow, so only do it on error.
            repo.invalidate_all()?;
            repo.tree_resolver()?
                .get(&filter.commit_id)
                .with_context(|| {
                    anyhow!(
                        "Failed to get root tree id for commit {:?}: {:?}",
                        filter.commit_id,
                        e
                    )
                })?
        }
    };

    let repo_store = repo
        .file_store()
        .context("failed to get FileStore from Repo object")?;

    // Get the metadata of the filter file and verify it's a valid file.
    let paths = filter.filter_paths.clone();

    let matcher = async_runtime::block_in_place(|| -> anyhow::Result<_> {
        let mut filters = Vec::with_capacity(paths.len());
        for path in paths {
            if let Some(entry_bytes) = fetch_sparse_profile_content(
                path.to_string(),
                &tree_manifest,
                repo_store.clone(),
                &HashMap::new(),
            )
            .with_context(|| format!("fetching content for filter: {path}"))?
            {
                filters.push(entry_bytes)
            } else {
                return Err(anyhow!("{path:?} is not a valid filter file"));
            }
        }

        // We no longer need to hold the lock on the object_map
        drop(object_map);

        let mut root = Root::from_profiles(filters, "edensparse".to_string())?;
        root.set_version_override(Some("2".to_owned()));
        let matcher = root.matcher(|_| Ok(Some(vec![])))?;
        Ok(matcher)
    });

    // If the result is an error, then the filter file doesn't exist or is
    // invalid. Return an always matcher instead of erroring out.
    let matcher = match matcher {
        Ok(m) => Box::new(m) as Box<dyn pathmatcher::Matcher>,
        Err(e) => {
            tracing::warn!("Failed to get sparse matcher for active filter: {:?}", e);
            LOOKUP_FAILURES.increment();
            Box::new(AlwaysMatcher::new()) as Box<dyn pathmatcher::Matcher>
        }
    };

    Ok(Box::new(MercurialMatcher { matcher }))
}

fn invalidate_object_cache_for_repo(checkout_path: &Path) {
    let mut object_map = OBJECT_CACHE.lock();
    if object_map.remove(checkout_path).is_some() {
        tracing::info!(
            "Invalidated OBJECT_CACHE entry for {}",
            checkout_path.display()
        );
    }
}

// CXX doesn't allow async functions to be exposed to C++. This function wraps the bulk of the
// Sparse Profile creation logic.
pub fn profile_from_filter_id(
    id: &[u8],
    checkout_path: &str,
    promise: UniquePtr<MatcherPromise>,
) -> Result<()> {
    LOOKUPS.increment();

    // We need to verify the checkout exists. The passed in checkout_path
    // should correspond to a valid hg/sl repo that Mercurial is aware of.
    let abs_repo_path = PathBuf::from(checkout_path);
    if let Err(e) = identity::sniff_dir(&abs_repo_path) {
        INVALID_REPO.increment();
        return Err(anyhow!(
            "{} is not a valid hg repo: {:#}",
            abs_repo_path.display(),
            e
        )
        .into());
    }

    // If we've already loaded a filter from this repo before, we can skip Repo
    // object creation. Otherwise, we need to pay the 1 time cost of creating
    // the Repo object.
    profile_contents_from_repo(id, abs_repo_path, promise);

    Ok(())
}

#[cfg(test)]
mod tests {
    use pathmatcher::TreeMatcher;

    use super::*;

    fn glob_matcher(globs: &[&str]) -> MercurialMatcher {
        MercurialMatcher {
            matcher: Box::new(TreeMatcher::from_rules(globs.iter(), true).unwrap()),
        }
    }

    #[test]
    fn fixed_depth_globs_keep_matching_file_paths() {
        let matcher = glob_matcher(&["*.cpp"]);
        // A file path matched by a fixed-depth rule is not recursively
        // filtered even though nothing under `a.cpp/` could match.
        assert!(
            matcher.matches_directory("a.cpp").ok().unwrap()
                == ffi::FilterDirectoryMatch::Unfiltered
        );
        // Unmatched paths with no matching subpaths stay filtered.
        assert!(
            matcher.matches_directory("a.txt").ok().unwrap()
                == ffi::FilterDirectoryMatch::RecursivelyFiltered
        );
        assert!(
            matcher.matches_directory("dir").ok().unwrap()
                == ffi::FilterDirectoryMatch::RecursivelyFiltered
        );
    }

    #[test]
    fn recursive_globs_report_directory_coverage() {
        let matcher = glob_matcher(&["foo/**"]);
        assert!(
            matcher.matches_directory("foo").ok().unwrap()
                == ffi::FilterDirectoryMatch::RecursivelyUnfiltered
        );
        assert!(
            matcher.matches_directory("bar").ok().unwrap()
                == ffi::FilterDirectoryMatch::RecursivelyFiltered
        );
    }
}
