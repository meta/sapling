/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Ok;
use anyhow::Result;
use anyhow::bail;
use arc_swap::ArcSwap;
use futures::future::BoxFuture;
use futures::stream::AbortHandle;
use parking_lot::Mutex;

mod slot;

pub use crate::slot::RepoSlot;

/// The future is owned rather than borrowing the loader: the load runs on a
/// task that outlives the call that started it.
pub trait RepoLoader<R>: Send + Sync {
    fn load(&self, repo_name: String) -> BoxFuture<'static, Result<R>>;
}

/// Set of repos currently associated with an instance of Mononoke
/// service or command. This type doesn't derive clone and thus
/// sharing of MononokeRepo should occur under Arc / Rc clones.
pub struct MononokeRepos<R> {
    name_to_repo_map: ArcSwap<HashMap<String, Arc<RepoSlot<R>>>>,
    id_to_name_map: ArcSwap<HashMap<i32, String>>,
    update_lock: Arc<Mutex<()>>, // Dedicated lock for guarding update operations.
    stats_handles: ArcSwap<HashMap<String, AbortHandle>>,
    /// Absent for a collection whose repos are all built elsewhere and handed
    /// in through `add` and friends.
    loader: Option<Arc<dyn RepoLoader<R>>>,
}

impl<R> MononokeRepos<R> {
    /// Creates a new instance of MononokeRepos that starts out
    /// with zero repos.
    pub fn new() -> Self {
        Self {
            name_to_repo_map: ArcSwap::from_pointee(HashMap::new()),
            id_to_name_map: ArcSwap::from_pointee(HashMap::new()),
            update_lock: Arc::new(Mutex::new(())),
            stats_handles: ArcSwap::from_pointee(HashMap::new()),
            loader: None,
        }
    }

    /// Set once, at construction, so a collection never exists in a state
    /// where it is meant to build but cannot.
    pub fn new_lazy(loader: Arc<dyn RepoLoader<R>>) -> Self {
        Self {
            loader: Some(loader),
            ..Self::new()
        }
    }

    /// Whether this collection can build a placeholder on request, i.e. was
    /// constructed with a loader.
    pub fn loads_on_demand(&self) -> bool {
        self.loader.is_some()
    }

    /// Get the repo corresponding to the repo-name if the repo
    /// has been loaded for the service/command, else return None.
    pub fn get_by_name(&self, repo_name: &str) -> Option<Arc<R>> {
        self.name_to_repo_map.load().get(repo_name)?.loaded()
    }

    /// Get the repo corresponding to the repo-id if the repo
    /// has been loaded for the service/command, else return None.
    pub fn get_by_id(&self, repo_id: i32) -> Option<Arc<R>> {
        self.id_to_name_map
            .load()
            .get(&repo_id)
            .and_then(|repo_name| self.name_to_repo_map.load().get(repo_name)?.loaded())
    }

    /// Returns an iterator over the set of repos currently loaded
    /// for the service/command.
    pub fn iter(&self) -> impl Iterator<Item = Arc<R>> + use<R> {
        let result: Vec<_> = self
            .name_to_repo_map
            .load()
            .values()
            .filter_map(|repo_slot| repo_slot.loaded())
            .collect();
        result.into_iter()
    }

    /// Returns an iterator over the set of repo-names corresponding
    /// to the repos currently loaded for the service / command.
    pub fn iter_names(&self) -> impl Iterator<Item = String> + use<R> {
        let result: Vec<_> = self
            .id_to_name_map
            .load()
            .values()
            .map(|name| name.to_string())
            .collect();
        result.into_iter()
    }

    /// Names of the repos that are **built**, unlike [`Self::iter_names`],
    /// which also reports repos that are merely assigned. Rebuild decisions
    /// need this one: an unbuilt repo has no resolved config to compare
    /// against, so counting it as loaded makes it look permanently drifted.
    pub fn iter_loaded_names(&self) -> impl Iterator<Item = String> + use<R> {
        let result: Vec<_> = self
            .name_to_repo_map
            .load()
            .iter()
            .filter(|(_, repo_slot)| repo_slot.loaded().is_some())
            .map(|(name, _)| name.to_string())
            .collect();
        result.into_iter()
    }

    /// Returns an iterator over the set of repo-ids corresponding
    /// to the repos currently loaded for the service / command.
    pub fn iter_ids(&self) -> impl Iterator<Item = i32> + use<R> {
        let result: Vec<_> = self.id_to_name_map.load().keys().copied().collect();
        result.into_iter()
    }

    /// Private method that performs the add/update operations without lock-related
    /// logic. The public accessors to this method ensure that the lock is
    /// acquired before this method is invoked.
    fn add_or_update_inner(&self, repo_name: &str, repo_id: i32, repo_slot: RepoSlot<R>) {
        // First, add the repo-id to repo-name mapping since the actual
        // repo addition should be the last step.
        let id_to_name_map = self.id_to_name_map.load();
        let mut new_id_to_name_map = HashMap::from_iter(
            id_to_name_map
                .iter()
                .map(|(id, name)| (*id, name.to_string())),
        );
        new_id_to_name_map.insert(repo_id, repo_name.to_string());
        self.id_to_name_map.store(Arc::new(new_id_to_name_map));

        // Add the repo-name to repo mapping.
        let name_to_repo_map = self.name_to_repo_map.load();
        let mut new_name_to_repo_map = HashMap::from_iter(
            name_to_repo_map
                .iter()
                .map(|(name, repo_slot)| (name.to_string(), Arc::clone(repo_slot))),
        );
        new_name_to_repo_map.insert(repo_name.to_string(), Arc::new(repo_slot));
        self.name_to_repo_map.store(Arc::new(new_name_to_repo_map));
    }

    /// Adds a new repo corresponding to the provided repo-name
    /// and repo-id. If a repo already exists for that combination,
    /// then it is replaced by the passed in new repo.
    /// NOTE: This is a mutex guarded operation that can induce wait times for
    /// the caller thread. If this isn't desired, use try_add instead.
    /// Before calling this method ensure that the caller is not holding additional
    /// locks. If the caller does hold additional locks, ensure that the locks are
    /// acquired in proper sequence to avoid deadlock or starvation.
    pub fn add(&self, repo_name: &str, repo_id: i32, repo: R) {
        // Acquire the lock to avoid race conditions during update.
        let lock = self.update_lock.lock();
        self.add_or_update_inner(
            repo_name,
            repo_id,
            RepoSlot::ready(repo_name.to_string(), Arc::new(repo)),
        );
        // Drop the lock to allow other threads to update the repos.
        drop(lock);
    }

    /// Registers a repo as assigned to this service without building it. The
    /// repo is reported by `iter_names` and `iter_ids`, but `get_by_name` and
    /// `get_by_id` return `None` for it until something builds it.
    ///
    /// No-op if the repo is already present: registering an assignment only
    /// ever adds one, and never un-builds a repo that is already loaded.
    pub fn add_placeholder(&self, repo_name: &str, repo_id: i32) {
        // Acquire the lock to avoid race conditions during update.
        let lock = self.update_lock.lock();
        // Presence check under update_lock is atomic vs add/remove/reload/populate.
        if self.name_to_repo_map.load().contains_key(repo_name) {
            drop(lock);
            return;
        }
        self.add_or_update_inner(repo_name, repo_id, RepoSlot::empty(repo_name.to_string()));
        // Drop the lock to allow other threads to update the repos.
        drop(lock);
    }

    pub fn add_stats_handle_for_repo(&self, repo_name: &str, handle: AbortHandle) {
        // Acquire the lock to avoid race conditions during update.
        let lock = self.update_lock.lock();
        let stats_handles = self.stats_handles.load();
        let mut new_stats_handles = HashMap::from_iter(
            stats_handles
                .iter()
                .map(|(name, stats_handle)| (name.to_owned(), stats_handle.clone())),
        );
        new_stats_handles.insert(repo_name.to_owned(), handle);
        self.stats_handles.store(Arc::new(new_stats_handles));
        // Drop the lock to allow other threads to update the repos.
        drop(lock);
    }

    /// Attempts to add a new repo corresponding to the provided repo-name
    /// and repo-id. If a repo already exists for that combination, then
    /// it is replaced by the passed in new repo.
    /// NOTE: Repo changes are guarded by a mutex. This method attempts
    /// to acquire the lock if it is available, without getting blocked
    /// on the lock.
    pub fn try_add(&self, repo_name: &str, repo_id: i32, repo: R) -> Result<()> {
        // Attempt to acquire the lock before add, to avoid race condition.
        match self.update_lock.try_lock() {
            // Lock acquired, add repo.
            Some(lock) => {
                self.add_or_update_inner(
                    repo_name,
                    repo_id,
                    RepoSlot::ready(repo_name.to_string(), Arc::new(repo)),
                );
                drop(lock);
                Ok(())
            }
            // Someone else has the lock, bail.
            None => bail!("Lock could not be acquired for repo {repo_name}"),
        }
    }

    /// Private method that performs the remove operations without lock-related
    /// logic. The public accessors to this method ensure that the lock is
    /// acquired before this method is invoked.
    fn remove_inner(&self, repo_name: &str) {
        // First, remove the repo-id to repo-name mapping that exists
        // for this repo-name.
        let id_to_name_map = self.id_to_name_map.load();
        let new_id_to_name_map =
            HashMap::from_iter(id_to_name_map.iter().filter_map(|(id, name)| {
                if name != repo_name {
                    Some((*id, name.to_string()))
                } else {
                    None
                }
            }));
        self.id_to_name_map.store(Arc::new(new_id_to_name_map));
        // Remove the repo-name to repo mapping.
        let name_to_repo_map = self.name_to_repo_map.load();
        let new_name_to_repo_map =
            HashMap::from_iter(name_to_repo_map.iter().filter_map(|(name, repo_slot)| {
                if name != repo_name {
                    Some((name.to_string(), Arc::clone(repo_slot)))
                } else {
                    None
                }
            }));
        self.name_to_repo_map.store(Arc::new(new_name_to_repo_map));
    }

    pub fn remove_stats_handle_for_repo(&self, repo_name: &str) {
        // Acquire the lock to avoid race conditions during update.
        let lock = self.update_lock.lock();
        let stats_handles = self.stats_handles.load();
        let new_stats_handles =
            HashMap::from_iter(stats_handles.iter().filter_map(|(name, handle)| {
                if name != repo_name {
                    Some((name.to_string(), handle.clone()))
                } else {
                    handle.abort();
                    None
                }
            }));
        self.stats_handles.store(Arc::new(new_stats_handles));
        // Drop the lock to allow other threads to update the repos.
        drop(lock);
    }

    /// Removes an existing repo if that repo exists. If it doesn't
    /// then this method is essentially a no-op.
    /// NOTE: This is a mutex guarded operation that can induce wait
    /// times for the caller thread. If this isn't desired, use
    /// try_remove instead.
    pub fn remove(&self, repo_name: &str) {
        // Acquire the lock to avoid race conditions during update.
        let lock = self.update_lock.lock();
        self.remove_inner(repo_name);
        // Drop the lock to allow other threads to update the repos.
        drop(lock);
    }

    /// Attempts to remove an existing repo if that repo exists. If it
    /// doesn't then this method is essentially a no-op.
    /// NOTE: Repo changes are guarded by a mutex. This method attempts
    /// to acquire the lock if it is available, without getting blocked
    /// on the lock.
    pub fn try_remove(&self, repo_name: &str) -> Result<()> {
        // Attempt to acquire the lock before remove, to avoid race condition.
        match self.update_lock.try_lock() {
            // Lock acquired, remove repo.
            Some(lock) => {
                self.remove_inner(repo_name);
                drop(lock);
                Ok(())
            }
            // Someone else has the lock, bail.
            None => bail!("Lock could not be acquired for repo {repo_name}"),
        }
    }

    /// Method responsible for bulk populating MononokeRepos from an
    /// input iterator of Repos. This method completely discards any previous
    /// repos that were part of MononokeRepos and uses the input to generate
    /// a new collection. Do not use for partial updates.
    /// NOTE: This is a mutex guarded operation that can induce wait times for
    /// the caller thread.
    /// Before calling this method ensure that the caller is not holding additional
    /// locks. If the caller does hold additional locks, ensure that the locks are
    /// acquired in proper sequence to avoid deadlock or starvation.
    pub fn populate<I>(&self, repos: I)
    where
        I: IntoIterator<Item = (i32, String, R)>,
    {
        // Acquire the lock to avoid race conditions during update.
        let lock = self.update_lock.lock();
        let mut id_to_name_map: HashMap<i32, String> = HashMap::new();
        let mut name_to_repo_map: HashMap<String, Arc<RepoSlot<R>>> = HashMap::new();
        for (id, name, repo) in repos.into_iter() {
            id_to_name_map.insert(id, name.to_string());
            let repo_slot = RepoSlot::ready(name.clone(), Arc::new(repo));
            name_to_repo_map.insert(name, Arc::new(repo_slot));
        }
        self.id_to_name_map.store(Arc::new(id_to_name_map));
        self.name_to_repo_map.store(Arc::new(name_to_repo_map));
        // Drop the lock to allow other threads to update the repos.
        drop(lock);
    }

    /// Method responsible for bulk populating MononokeRepos from an input iterator of Repos.
    /// This method only REPLACES existing repos OR adds new repos with the set of repos
    /// provided as input maintaining a 1-to-1 mapping. In other words, this method will
    /// NEVER remove an existing repo.
    /// NOTE: This is a mutex guarded operation that can induce wait times for
    /// the caller thread.
    /// Before calling this method ensure that the caller is not holding additional
    /// locks. If the caller does hold additional locks, ensure that the locks are
    /// acquired in proper sequence to avoid deadlock or starvation.
    pub fn reload<I>(&self, repos: I)
    where
        I: IntoIterator<Item = (i32, String, R)>,
    {
        // Acquire the lock to avoid race conditions during update.
        let lock = self.update_lock.lock();
        let mut id_to_name_map: HashMap<i32, String> =
            Arc::<_>::unwrap_or_clone(self.id_to_name_map.load().clone());
        let mut name_to_repo_map: HashMap<String, Arc<RepoSlot<R>>> =
            Arc::<_>::unwrap_or_clone(self.name_to_repo_map.load().clone());
        for (id, name, repo) in repos.into_iter() {
            id_to_name_map.insert(id, name.to_string());
            let repo_slot = RepoSlot::ready(name.clone(), Arc::new(repo));
            name_to_repo_map.insert(name, Arc::new(repo_slot));
        }
        self.id_to_name_map.store(Arc::new(id_to_name_map));
        self.name_to_repo_map.store(Arc::new(name_to_repo_map));
        // Drop the lock to allow other threads to update the repos.
        drop(lock);
    }

    /// Replace a repo only if it is currently present (by name); no-op if absent.
    /// Prevents a rebuild from resurrecting a repo a concurrent `remove` dropped.
    /// Returns whether the repo was present (and thus replaced).
    pub fn reload_if_present(&self, id: i32, name: String, repo: R) -> bool {
        let lock = self.update_lock.lock();
        // Presence check under update_lock is atomic vs remove/reload/populate.
        if !self.name_to_repo_map.load().contains_key(&name) {
            drop(lock);
            return false;
        }
        let mut id_to_name_map: HashMap<i32, String> =
            Arc::<_>::unwrap_or_clone(self.id_to_name_map.load().clone());
        let mut name_to_repo_map: HashMap<String, Arc<RepoSlot<R>>> =
            Arc::<_>::unwrap_or_clone(self.name_to_repo_map.load().clone());
        id_to_name_map.insert(id, name.clone());
        let repo_slot = RepoSlot::ready(name.clone(), Arc::new(repo));
        name_to_repo_map.insert(name, Arc::new(repo_slot));
        self.id_to_name_map.store(Arc::new(id_to_name_map));
        self.name_to_repo_map.store(Arc::new(name_to_repo_map));
        drop(lock);
        true
    }

    /// Concurrent callers for the same repo share a single build, and a caller
    /// that goes away neither cancels nor restarts it.
    ///
    /// `None` covers both "not assigned to this service" and "assigned, but
    /// this collection cannot load anything", deliberately: splitting them
    /// would send a reader looking at shard assignment over a wiring mistake.
    pub async fn get(&self, repo_name: &str) -> Result<Option<Arc<R>>>
    where
        R: Send + Sync + 'static,
    {
        let Some(repo_slot) = self.name_to_repo_map.load().get(repo_name).cloned() else {
            return Ok(None);
        };

        if let Some(repo) = repo_slot.loaded() {
            return Ok(Some(repo));
        }

        let Some(loader) = self.loader.clone() else {
            return Ok(None);
        };

        repo_slot.get_or_build(loader).await.map(Some)
    }
}

#[cfg(test)]
mod tests;
