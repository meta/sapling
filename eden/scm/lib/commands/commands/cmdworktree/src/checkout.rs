/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#[cfg(all(feature = "eden", feature = "fb"))]
mod direct;
#[cfg(feature = "eden")]
mod eden;
mod git;

use std::path::Path;

use clidispatch::ReqCtx;
use clidispatch::TermLogger;
use cmdutil::Result;
use repo::repo::Repo;
use types::HgId;
use worktree::Worktrees;

use crate::WorktreeOpts;

/// Creates and removes checkouts with EdenFS or Git, whichever backs the repo.
pub(crate) enum Checkouts<'a> {
    #[cfg(feature = "eden")]
    Eden(eden::EdenCheckouts<'a>),
    Git(git::GitCheckouts<'a>),
}

/// Clone options captured before the pre-add hooks run.
pub(crate) enum AddSource<'a> {
    #[cfg(feature = "eden")]
    Eden(eden::AddSource<'a>),
    Git(git::GitCheckouts<'a>),
}

/// Source state needed to materialize a new checkout.
pub(crate) enum PreparedAdd<'a> {
    #[cfg(feature = "eden")]
    Eden(eden::PreparedAdd<'a>),
    Git(git::PreparedAdd<'a>),
}

impl<'a> Checkouts<'a> {
    pub(crate) fn new(repo: &'a Repo, worktrees: &'a Worktrees) -> Result<Self> {
        if let Some(git) = worktrees.git() {
            return Ok(Self::Git(git::GitCheckouts::new(repo, git)));
        }
        #[cfg(feature = "eden")]
        {
            Ok(Self::Eden(eden::EdenCheckouts::new(repo)))
        }
        #[cfg(not(feature = "eden"))]
        {
            clidispatch::abort!("EdenFS worktrees are not supported in this build");
        }
    }

    /// Whether `add --snapshot` can carry working copy changes into a new checkout.
    pub(crate) fn supports_snapshot(&self) -> bool {
        match self {
            #[cfg(feature = "eden")]
            Self::Eden(_) => true,
            Self::Git(_) => false,
        }
    }

    /// The command that cleans up what a failed add left at `dest`, if the
    /// error doesn't already say.
    #[cfg_attr(not(feature = "eden"), allow(unused_variables))]
    pub(crate) fn cleanup_command(&self, dest: &Path) -> Option<String> {
        match self {
            #[cfg(feature = "eden")]
            Self::Eden(_) => Some(format!("eden rm {}", dest.display())),
            Self::Git(_) => None,
        }
    }

    /// Remove one checkout. The caller holds its path lock.
    ///
    /// Returns `true` when the backend performed removal, including EdenFS state
    /// cleanup for a missing folder. Returns `false` only when the backend skipped
    /// a missing EdenFS checkout.
    #[cfg_attr(not(feature = "eden"), allow(unused_variables))]
    pub(crate) fn remove(&self, logger: &TermLogger, path: &Path, force: bool) -> Result<bool> {
        match self {
            #[cfg(feature = "eden")]
            Self::Eden(eden) => eden.remove(logger, path),
            Self::Git(git) => {
                git.remove(path, force)?;
                Ok(true)
            }
        }
    }

    /// Whether the checkout at `path` has modified or untracked files.
    #[cfg_attr(not(feature = "eden"), allow(unused_variables))]
    pub(crate) fn has_changes(&self, ctx: &ReqCtx<WorktreeOpts>, path: &Path) -> Result<bool> {
        match self {
            #[cfg(feature = "eden")]
            Self::Eden(eden) => eden.has_changes(ctx, path),
            Self::Git(git) => git.has_changes(path),
        }
    }

    /// Whether the checkout at `path` has shelved changes.
    pub(crate) fn has_shelves(&self, path: &Path) -> Result<bool> {
        match self {
            #[cfg(feature = "eden")]
            Self::Eden(eden) => eden.has_shelves(path),
            Self::Git(git) => git.has_shelves(path),
        }
    }

    pub(crate) fn add_source(&self) -> Result<AddSource<'a>> {
        match self {
            #[cfg(feature = "eden")]
            Self::Eden(eden) => Ok(AddSource::Eden(eden.add_source()?)),
            Self::Git(git) => Ok(AddSource::Git(*git)),
        }
    }
}

impl<'a> AddSource<'a> {
    /// Capture the source checkout state. The caller holds the source path lock.
    pub(crate) fn capture(self, requested_target: Option<HgId>) -> Result<PreparedAdd<'a>> {
        match self {
            #[cfg(feature = "eden")]
            Self::Eden(source) => Ok(PreparedAdd::Eden(source.capture(requested_target)?)),
            Self::Git(git) => Ok(PreparedAdd::Git(git.capture(requested_target)?)),
        }
    }
}

impl PreparedAdd<'_> {
    /// Create and configure the checkout. The caller holds the destination path lock.
    pub(crate) fn create(self, dest: &Path) -> Result<()> {
        match self {
            #[cfg(feature = "eden")]
            Self::Eden(prepared) => prepared.create(dest),
            Self::Git(prepared) => prepared.create(dest),
        }
    }
}

/// Shelves are listed from `<name>.patch` files in the checkout's own dot dir,
/// so removing the checkout drops them from `shelve --list` and `unshelve`.
/// The dot dir is sniffed from the checkout because its flavour can differ from
/// the invoking repo's.
fn has_shelves(repo: &Repo, path: &Path) -> Result<bool> {
    let ident = identity::sniff_dir(path)?.unwrap_or_else(|| repo.ident());
    let shelved = ident.resolve_full_dot_dir(path).join("shelved");
    let entries = match fs_err::read_dir(shelved) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err.into()),
    };
    for entry in entries {
        if entry?.file_name().to_string_lossy().ends_with(".patch") {
            return Ok(true);
        }
    }
    Ok(false)
}

fn resolve_target(repo: &Repo, requested_target: Option<HgId>) -> Result<Option<HgId>> {
    let parents =
        workingcopy::fast_path_wdir_parents_with_config(repo.path(), repo.ident(), repo.config())?;
    Ok(requested_target.or_else(|| parents.p1().copied()))
}
