/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use clidispatch::ReqCtx;
use clidispatch::TermLogger;
use cmdutil::Result;
use configmodel::Text;
use repo::repo::Repo;
use types::HgId;

#[cfg(feature = "direct-removal")]
use super::direct::run_direct_eden_remove;
use super::resolve_target;
use crate::WorktreeOpts;

/// Creates checkouts with `eden clone` and removes them like `eden rm`.
pub(crate) struct EdenCheckouts<'a> {
    repo: &'a Repo,
}

pub(crate) struct AddSource<'a> {
    repo: &'a Repo,
    clone_filters: Option<HashSet<Text>>,
}

pub(crate) struct PreparedAdd<'a> {
    repo: &'a Repo,
    target: Option<HgId>,
    clone_filters: Option<HashSet<Text>>,
    sparse_config: Option<Vec<u8>>,
    user_config: Option<String>,
}

impl<'a> EdenCheckouts<'a> {
    pub(super) fn new(repo: &'a Repo) -> Self {
        Self { repo }
    }

    pub(super) fn add_source(&self) -> Result<AddSource<'a>> {
        let repo = self.repo;
        // Replicate the source repo's scm type and active filters.
        // When edensparse is in requirements, the backing store should be filteredhg
        // (even with no filter paths configured). Otherwise the backing store is hg.
        // Read active filter paths from the source repo's .sl/sparse file.
        let clone_filters = repo
            .requirements
            .contains("edensparse")
            .then(|| -> anyhow::Result<_> {
                let paths = filters::util::read_filter_config(repo.dot_hg_path())?
                    .map(|paths| paths.into_iter().map(|p| p.into_string().into()).collect())
                    .unwrap_or_default();
                Ok(paths)
            })
            .transpose()?;
        Ok(AddSource {
            repo,
            clone_filters,
        })
    }

    pub(super) fn remove(&self, logger: &TermLogger, path: &Path) -> Result<bool> {
        run_direct_eden_remove(logger, self.repo, path)
    }

    pub(super) fn has_shelves(&self, path: &Path) -> Result<bool> {
        super::has_shelves(self.repo, path)
    }

    pub(super) fn has_changes(&self, ctx: &ReqCtx<WorktreeOpts>, path: &Path) -> Result<bool> {
        let repo = Repo::load(path, &[])?;
        let wc = repo.working_copy()?;
        let matcher = Arc::new(pathmatcher::AlwaysMatcher::new());
        let status = wc.read().status(&ctx.core, matcher, false)?;
        Ok(status.dirty() || status.unknown().next().is_some())
    }
}

impl<'a> AddSource<'a> {
    pub(super) fn capture(self, requested_target: Option<HgId>) -> Result<PreparedAdd<'a>> {
        let repo = self.repo;
        let target = resolve_target(repo, requested_target)?;
        let source_client_dir = edenfs_client::get_client_dir(repo.path())?;
        Ok(PreparedAdd {
            repo,
            target,
            clone_filters: self.clone_filters,
            sparse_config: clone::snapshot_sparse_config(repo.dot_hg_path())?,
            user_config: clone::snapshot_eden_user_config(&source_client_dir)?,
        })
    }
}

impl PreparedAdd<'_> {
    pub(super) fn create(self, dest: &Path) -> Result<()> {
        let repo = self.repo;
        clone::eden_clone(repo, dest, self.target, self.clone_filters)?;
        if let Some(config) = self.sparse_config {
            clone::write_sparse_config(&config, &dest.join(repo.ident().dot_dir()))?;
        }
        if let Some(config) = self.user_config {
            clone::apply_eden_user_config_snapshot(repo.config().as_ref(), &config, dest)?;
        }
        Ok(())
    }
}

#[cfg(not(feature = "direct-removal"))]
fn run_direct_eden_remove(_logger: &TermLogger, _repo: &Repo, path: &Path) -> Result<bool> {
    clidispatch::abort!(
        "removing EdenFS worktrees is not supported in this build; run `eden rm {}` instead",
        path.display()
    );
}
