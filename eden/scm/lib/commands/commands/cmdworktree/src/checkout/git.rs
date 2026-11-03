/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::path::Path;

use anyhow::Context;
use cmdutil::Result;
use gitcompat::RepoGit;
use repo::repo::Repo;
use types::HgId;

use super::resolve_target;

/// Creates and removes checkouts with `git worktree`.
#[derive(Clone, Copy)]
pub(crate) struct GitCheckouts<'a> {
    repo: &'a Repo,
    git: &'a RepoGit,
}

pub(crate) struct PreparedAdd<'a> {
    checkouts: GitCheckouts<'a>,
    target: HgId,
}

impl<'a> GitCheckouts<'a> {
    pub(super) fn new(repo: &'a Repo, git: &'a RepoGit) -> Self {
        Self { repo, git }
    }

    pub(super) fn capture(self, requested_target: Option<HgId>) -> Result<PreparedAdd<'a>> {
        let target = resolve_target(self.repo, requested_target)?
            .filter(|target| !target.is_null())
            .context("cannot create a Git worktree without a commit; commit first or pass --rev")?;
        Ok(PreparedAdd {
            checkouts: self,
            target,
        })
    }

    pub(super) fn remove(&self, path: &Path, force: bool) -> Result<()> {
        Ok(self.git.remove_worktree(path, force)?)
    }

    /// Uses `git status`, because Sapling may not have run in a checkout
    /// created with Git yet.
    pub(super) fn has_changes(&self, path: &Path) -> Result<bool> {
        Ok(self.git.worktree_has_changes(path)?)
    }

    /// Sapling keeps a Git checkout's shelves in `$GIT_DIR/sl`, which removal
    /// deletes with the checkout.
    pub(super) fn has_shelves(&self, path: &Path) -> Result<bool> {
        super::has_shelves(self.repo, path)
    }
}

impl PreparedAdd<'_> {
    pub(super) fn create(self, dest: &Path) -> Result<()> {
        let GitCheckouts { repo, git } = self.checkouts;
        git.add_worktree(dest, self.target).with_context(|| {
            format!(
                "a checkout may remain at {}; inspect it and use `git worktree remove` to recover",
                dest.display()
            )
        })?;
        gitcompat::init::maybe_init_inside_dotgit(dest, repo.ident())?;
        Ok(())
    }
}
