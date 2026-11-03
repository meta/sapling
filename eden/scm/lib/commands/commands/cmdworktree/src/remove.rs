/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::BTreeSet;
use std::collections::HashMap;
use std::io::BufRead;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use clidispatch::ReqCtx;
use clidispatch::abort;
use cmdutil::ConfigExt;
use cmdutil::Result;
use repo::repo::Repo;
use worktree::Worktrees;
use worktree::lock_worktree_path_op;

use crate::WorktreeOpts;
use crate::checkout::Checkouts;

pub(crate) fn run(ctx: &ReqCtx<WorktreeOpts>, repo: &Repo, worktrees: &Worktrees) -> Result<u8> {
    let logger = ctx.logger();

    let (main, mut targets) = if ctx.opts.all {
        let Some(main) = worktrees.main_for_remove()? else {
            abort!("this worktree is not part of a group");
        };
        let targets = worktrees.linked_worktrees(&main)?;
        (main, targets)
    } else {
        let path_args = &ctx.opts.args[1..];
        if path_args.is_empty() {
            abort!("usage: sl worktree remove PATH [PATH...]");
        }
        let targets = path_args
            .iter()
            .map(|s| -> Result<PathBuf> {
                Ok(util::path::strip_unc_prefix(
                    util::path::canonical_path_allow_missing(s)?,
                ))
            })
            .collect::<Result<BTreeSet<_>>>()?;
        let Some(main) = worktrees.main_for_remove()? else {
            let names: Vec<String> = targets.iter().map(|p| p.display().to_string()).collect();
            abort!(
                "cannot remove {}: {} has no linked worktrees",
                names.join(", "),
                repo.path().display()
            );
        };
        for target in &targets {
            worktrees.validate_removal(&main, target)?;
        }
        (main, targets.into_iter().collect())
    };
    if targets.is_empty() {
        logger.info("no linked worktrees to remove");
        return Ok(0);
    }

    // Later removals still run from the invoking checkout, so remove it last.
    let current = util::path::strip_unc_prefix(util::path::canonicalize_best_effort(repo.path())?);
    targets.sort_by_key(|target| *target == current);
    let target_refs: Vec<&Path> = targets.iter().map(|p| p.as_path()).collect();
    confirm_remove(ctx, &target_refs)?;

    // Own every target before running hooks, so no other worktree command can
    // act on one until this removal is done with it. Lock in path order so
    // overlapping removals cannot deadlock.
    let shared_store_path = worktrees.shared_store_path();
    let mut lock_order = targets.clone();
    lock_order.sort();
    let mut path_locks = lock_order
        .into_iter()
        .map(|target| {
            let lock = lock_worktree_path_op(repo.config().as_ref(), &shared_store_path, &target)?;
            Ok((target, lock))
        })
        .collect::<Result<HashMap<_, _>>>()?;

    // Targets may have been removed while this command waited for their locks.
    // `--all` skips them silently; explicit targets fail validation below.
    // A target removed and re-added at the same path in that window is not
    // detected, since the registry tracks worktrees by path.
    if ctx.opts.all {
        let linked = worktrees.linked_worktrees(&main)?;
        targets.retain(|target| linked.contains(target));
        if targets.is_empty() {
            logger.info("no linked worktrees to remove");
            return Ok(0);
        }
    }

    let checkouts = Checkouts::new(repo, worktrees)?;
    let pre_hooks = hook::Hooks::from_config(repo.config(), ctx.io(), "pre-worktree-remove");
    let multi_target = ctx.opts.all || targets.len() > 1;
    let mut remove_error = None;
    let mut failed = 0;
    for target in &targets {
        let result = worktrees
            .validate_removal(&main, target)
            .and_then(|()| {
                if !ctx.opts.force {
                    ensure_clean(ctx, &checkouts, target)?;
                    ensure_no_shelves(repo, &checkouts, target)?;
                }
                pre_hooks.run_hooks(
                    Some(repo),
                    false,
                    Some(&HashMap::from([(
                        "path".to_string(),
                        target.display().to_string(),
                    )])),
                )
            })
            .and_then(|()| {
                if !checkouts.remove(&logger, target, ctx.opts.force)? {
                    logger.warn(format!(
                        "eden checkout {} not found on disk, continuing to remove from registry",
                        target.display()
                    ));
                }
                worktrees.forget_removed(&main, target)
            });
        path_locks.remove(target);
        match result {
            Ok(()) => logger.info(format!("removed {}", target.display())),
            Err(err) if multi_target => {
                logger.warn(format!("failed to remove {}: {:#}", target.display(), err));
                failed += 1;
                remove_error.get_or_insert(err);
            }
            Err(err) => return Err(err),
        }
    }
    if let Some(err) = remove_error {
        logger.warn(format!(
            "removed {} of {} worktrees; {} failed",
            targets.len() - failed,
            targets.len(),
            failed
        ));
        return Err(err);
    }

    // NOTE: Add post-worktree-remove hook if the need arises. Note that the
    // hook's cwd (repo.path()) may not exist if the user removed the worktree
    // they were standing in (see D98226466).

    Ok(0)
}

/// Refuse to remove a checkout with modified or untracked files.
fn ensure_clean(ctx: &ReqCtx<WorktreeOpts>, checkouts: &Checkouts, target: &Path) -> Result<()> {
    if !target.exists() {
        return Ok(());
    }
    let has_changes = checkouts.has_changes(ctx, target).with_context(|| {
        format!(
            "cannot check {} for modified or untracked files, use --force to remove it anyway",
            target.display()
        )
    })?;
    if has_changes {
        anyhow::bail!(
            "{} contains modified or untracked files, use --force to remove it",
            target.display()
        );
    }
    Ok(())
}

/// Refuse to remove a checkout with shelved changes, which do not count as
/// modified or untracked files.
fn ensure_no_shelves(repo: &Repo, checkouts: &Checkouts, target: &Path) -> Result<()> {
    if !target.exists()
        || !repo
            .config()
            .get_or("worktree", "check-shelves-on-remove", || true)?
    {
        return Ok(());
    }
    let has_shelves = checkouts.has_shelves(target).with_context(|| {
        format!(
            "cannot check {} for shelved changes, use --force to remove it anyway",
            target.display()
        )
    })?;
    if has_shelves {
        anyhow::bail!(
            "{} has shelved changes, use --force to remove it",
            target.display()
        );
    }
    Ok(())
}

fn confirm_remove(ctx: &ReqCtx<WorktreeOpts>, paths: &[&Path]) -> Result<()> {
    // Don't prompt without a terminal. The checks for modified or untracked files
    // and shelved changes still apply.
    if ctx.global_opts().noninteractive
        || ctx.opts.force
        || !ctx.io().with_input(|input| input.is_tty())
    {
        return Ok(());
    }

    if paths.len() == 1 {
        ctx.io()
            .write(format!("will remove {}\n", paths[0].display()))?;
    } else {
        ctx.io()
            .write(format!("will remove {} worktrees:\n", paths.len()))?;
        for p in paths {
            ctx.io().write(format!("  {}\n", p.display()))?;
        }
    }
    ctx.io().write("proceed? [y/N] ")?;
    ctx.io().flush()?;

    let mut input = ctx.io().input();
    let mut line = String::new();
    let mut reader = std::io::BufReader::new(&mut input);
    reader.read_line(&mut line)?;
    let answer = line.trim();
    if !(answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes")) {
        abort!("aborted by user");
    }
    Ok(())
}
