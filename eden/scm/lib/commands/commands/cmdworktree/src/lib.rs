/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

mod add;
mod checkout;
mod label;
mod list;
mod remove;

use std::time::Duration;

use clidispatch::ReqCtx;
use clidispatch::abort;
use cmdutil::ConfigExt;
use cmdutil::FormatterOpts;
use cmdutil::Result;
use cmdutil::define_flags;
use repo::repo::Repo;
use worktree::Worktrees;

define_flags! {
    pub struct WorktreeOpts {
        /// a short label for the worktree (for 'add' and 'label')
        #[argtype("TEXT")]
        label: String,

        /// create a snapshot of the current working copy, then restore it in the new worktree (for 'add')
        snapshot: bool,

        /// revision to check out, must resolve to one commit (for 'add')
        #[short('r')]
        #[argtype("REV")]
        rev: String,

        /// remove all linked worktrees (for 'remove')
        all: bool,

        /// remove worktrees even if they have modified or untracked files or shelved changes, without prompting (for 'remove')
        force: bool,

        /// remove the label instead of setting it (for 'label')
        remove: bool,

        formatter_opts: FormatterOpts,

        #[args]
        args: Vec<String>,
    }
}

pub fn run(ctx: ReqCtx<WorktreeOpts>, repo: &Repo) -> Result<u8> {
    let subcmd = ctx.opts.args.first().map(|s| s.as_str()).unwrap_or("");
    if !ctx.opts.rev.is_empty() && subcmd != "add" {
        abort!("--rev can only be used with 'worktree add'");
    }
    let worktrees = || Worktrees::open(repo, repo.config().as_ref());
    match subcmd {
        "list" | "ls" => list::run(&ctx, repo, &worktrees()?),
        "label" => label::run(&ctx, repo, &worktrees()?),
        "add" => add::run(&ctx, repo, &worktrees()?),
        "remove" | "rm" => remove::run(&ctx, repo, &worktrees()?),
        "" => abort!("you need to specify a subcommand (run with --help to see a list)"),
        other => abort!("unknown worktree subcommand '{}'", other),
    }
}

/// Read the configured slot-reservation TTL in seconds. Reservations older than
/// this are treated as orphaned by a crashed `worktree add` and pruned. Defaults
/// to [`worktree::DEFAULT_RESERVATION_TTL_SECONDS`] (1 hour); override with the
/// `worktree.reservation-ttl` config (accepts durations like `30m` or `2h`).
pub(crate) fn reservation_ttl_seconds(repo: &Repo) -> Result<i64> {
    let ttl: Duration = repo.config().get_or("worktree", "reservation-ttl", || {
        Duration::from_secs(worktree::DEFAULT_RESERVATION_TTL_SECONDS as u64)
    })?;
    Ok(ttl.as_secs() as i64)
}

pub fn aliases() -> &'static str {
    "worktree|wt"
}

pub fn doc() -> &'static str {
    r#"manage multiple linked worktrees sharing the same repository

    worktree groups allow multiple EdenFS-backed or native Git working copies
    to share the same repository. One worktree is designated as the main worktree,
    and additional linked worktrees can be created, listed, labeled, and
    removed.

    Subcommands::

      list [-Tjson]                                     List all worktrees in the group
      add [PATH] [-r REV] [--label TEXT] [--snapshot]   Create a new linked worktree
      remove PATH [PATH...] [--all] [--force] [-y]      Remove linked worktree(s)
      label [PATH] TEXT [--remove]                      Set or remove a worktree label

    If PATH is omitted from `add`, `worktree.path-generator` is used to
    choose the destination path.

    Config options::

      worktree.max-count=N                     Hard limit on linked worktrees per repo group (0 = unlimited, the default)
      worktree.max-count-message=TEXT          Message shown instead of the default when max-count is reached
      worktree.reservation-ttl=DURATION        How long an in-flight add holds a slot before it is treated as stale (default 1h)
      worktree.require-generated-path=true     Require path generator, disallow manual PATH
      worktree.path-generator=CMD              Shell command to generate PATH when omitted
      worktree.snapshot-direct-copy=true       Use direct copy for --snapshot
      worktree.git-enabled=true                Enable worktree commands in native Git repositories
      worktree.check-shelves-on-remove=false   Let remove delete a checkout with shelved changes without --force (default: true)

    In native Git repositories, `add` creates a detached checkout and `remove`
    refuses locked worktrees, as Git does, even with `--force`. Worktrees
    created directly with Git are also supported. The `--snapshot` option
    requires EdenFS."#
}

pub fn synopsis() -> Option<&'static str> {
    Some("SUBCOMMAND [OPTIONS] [ARGS]")
}
