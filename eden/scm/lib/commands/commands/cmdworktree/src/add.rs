/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::sync::Arc;

use anyhow::Context;
use anyhow::bail;
use clidispatch::ReqCtx;
use clidispatch::TermLogger;
use clidispatch::abort;
use clidispatch::io::IsTty;
use cmdutil::ConfigExt;
use cmdutil::Result;
use configmodel::convert::ByteCount;
use encoding::shell_output_bytes_to_path;
use fs_err as fs;
use repo::repo::Repo;
use repo::repo::ResolveResult;
use slex::Items;
use slex::Work as SlexWork;
use slex::WorkOptions;
use spawn_ext::CommandExt;
use types::HgId;
use worktree::Reservation;
use worktree::Worktrees;
use worktree::check_dest_not_in_repo;
use worktree::check_disk_space;
use worktree::new_reservation_id;
use worktree::with_reservations;
use worktree::with_worktree_path_op_lock;

use crate::WorktreeOpts;
use crate::checkout::Checkouts;

fn current_sl_binary() -> OsString {
    std::env::current_exe()
        .ok()
        .map(Into::into)
        .unwrap_or_else(|| OsString::from(identity::cli_name()))
}

/// Create a snapshot of the working copy at `repo_path`, returning the snapshot ID.
fn sapling_snapshot_create(sl_bin: &OsString, repo_path: PathBuf) -> anyhow::Result<String> {
    let output = Command::new(sl_bin)
        .args([
            "--config",
            "extensions.snapshot=",
            "snapshot",
            "create",
            "--template",
            "{id}",
            "-y",
        ])
        .current_dir(&repo_path)
        .checked_output()
        .context("failed to run sl snapshot create")?;

    let id = Command::stdout_str(&output);
    if id.is_empty() {
        bail!("snapshot create returned empty snapshot id");
    }
    Ok(id)
}

/// Restore a snapshot into the worktree at `dest`.
fn sapling_snapshot_checkout(sl_bin: &OsString, dest: &Path, id: &str) -> anyhow::Result<()> {
    Command::new(sl_bin)
        .args([
            "--config",
            "extensions.snapshot=",
            "snapshot",
            "checkout",
            id,
            "--clean",
            "-y",
        ])
        .current_dir(dest)
        .checked_run()
        .with_context(|| format!("failed to restore snapshot {} into {}", id, dest.display()))?;
    Ok(())
}

/// Error message shown when the per-repo linked-worktree limit is reached.
///
/// Centralized so the two abort sites (the pre-lock fast-fail and the
/// under-lock reservation check) stay in sync. `worktree.max-count-message`
/// replaces the default text, which includes instructions for overriding the
/// limit via user config.
fn worktree_limit_reached_message(repo: &Repo, max_count: u64, current: usize) -> String {
    if let Some(message) = repo.config().get_nonempty("worktree", "max-count-message") {
        return message.to_string();
    }
    let repo_path = repo.path().display();
    format!(
        "cannot create worktree: limit of {max_count} linked worktrees reached for repository '{repo_path}' (currently {current}); \
         set `worktree.max-count` in your user config (e.g. ~/.hgrc) to raise it, or 0 to disable the limit"
    )
}

/// Record free space for every measured add and the warning shown to this caller.
fn record_disk_check_metrics(available: u64, below_warning: bool, is_agent: bool) {
    hg_metrics::max_counter("worktree.disk.free_gb", available >> 30);
    if below_warning {
        hg_metrics::increment_counter(
            if is_agent {
                "worktree.disk.agent_warned"
            } else {
                "worktree.disk.human_warned"
            },
            1,
        );
    }
}

/// Advisory for a worktree add that continues despite low disk space.
fn disk_low_warning_message(available: u64, total: u64, dest: &Path, is_agent: bool) -> String {
    let suffix = if is_agent {
        " This is advisory: low disk space did not block this worktree add; continue your task without asking the user about this warning."
    } else {
        " To disable this warning, set `worktree.disk-warning-threshold=0` in your user config."
    };
    format!(
        "low disk space: {} remaining of {} on '{}'; continuing.{suffix}",
        ByteCount::from(available),
        ByteCount::from(total),
        dest.display(),
    )
}

/// RAII guard for a reserved `worktree add` slot.
///
/// The slot is recorded in the separate reservations file (counting against
/// `worktree.max-count`) before the destination is cloned, so a concurrent add
/// racing for the last slot fails fast instead of cloning and then aborting
/// during registration — which would leave a fully cloned but unregistered
/// checkout behind. [`SlotReservation::release`] removes the slot once the add
/// finishes (success or failure); `Drop` calls it as a backstop.
///
/// The reservation is keyed by a unique `reservation_id` (not the destination),
/// so releasing only ever removes *this* add's slot even when another add is
/// racing for the same destination.
struct SlotReservation {
    shared_store_path: PathBuf,
    reservation_id: String,
    dest: PathBuf,
    logger: TermLogger,
    released: bool,
}

impl SlotReservation {
    /// Remove this add's reservation (idempotent, best effort). Called on both
    /// the success path (once the worktree is registered) and, as a backstop, on
    /// drop. A failure only leaves a stale reservation that the TTL will prune,
    /// so it never fails the command.
    fn release(&mut self) {
        if self.released {
            return;
        }
        let id = self.reservation_id.clone();
        match with_reservations(&self.shared_store_path, |_registry, reservations| {
            reservations.reservations.remove(&id);
            Ok(())
        }) {
            Ok(()) => self.released = true,
            Err(e) => self.logger.warn(format!(
                "failed to release worktree slot reservation for {} \
                 (it will expire automatically): {:#}",
                self.dest.display(),
                e
            )),
        }
    }
}

impl Drop for SlotReservation {
    fn drop(&mut self) {
        self.release();
    }
}

pub(crate) fn run(ctx: &ReqCtx<WorktreeOpts>, repo: &Repo, worktrees: &Worktrees) -> Result<u8> {
    let wc = repo.working_copy()?;
    let wc = wc.write();
    let logger = ctx.logger();
    let checkouts = Checkouts::new(repo, worktrees)?;

    if ctx.opts.snapshot && !checkouts.supports_snapshot() {
        abort!("--snapshot is not supported for Git worktrees");
    }
    if ctx.opts.snapshot && !ctx.opts.rev.is_empty() {
        abort!("cannot use --rev with --snapshot");
    }

    let requested_target = if ctx.opts.rev.is_empty() {
        None
    } else {
        match resolve_rev(ctx, repo)? {
            Ok(target) => Some(target),
            Err(exit_code) => return Ok(exit_code),
        }
    };

    let require_generated: bool = repo
        .config()
        .get_or_default("worktree", "require-generated-path")?;

    let max_count: u64 = repo.config().get_or("worktree", "max-count", || 0)?;

    // Pre-compute the canonical path for the source repo early since the path generator needs it.
    let canonical_repo_path = fs::canonicalize(repo.path())
        .map(util::path::strip_unc_prefix)
        .unwrap_or_else(|_| repo.path().to_path_buf());

    let dest = match ctx.opts.args.get(1) {
        Some(value) => {
            if require_generated {
                abort!(
                    "custom worktree paths are not allowed (worktree.require-generated-path is set); \
                     run without a path argument to use the configured path generator"
                );
            }
            util::path::strip_unc_prefix(util::path::canonical_path_allow_missing(value)?)
        }
        None => run_path_generator(
            repo,
            &ctx.opts.label,
            &canonical_repo_path,
            require_generated,
        )?,
    };

    // Fast-fail before locking (re-checked inside lock).
    if dest.exists() {
        abort!("destination path '{}' already exists", dest.display());
    }

    check_dest_not_in_repo(&dest)?;

    let shared_store_path = worktrees.shared_store_path();
    let (group_main_path, linked) = worktrees.check_add(&dest)?;

    // Fast-fail if worktree limit is already reached (re-checked inside lock).
    if max_count > 0 && linked >= max_count as usize {
        abort!(
            "{}",
            worktree_limit_reached_message(repo, max_count, linked)
        );
    }

    let check = match check_disk_space(repo.config().as_ref(), &dest) {
        Ok(check) => check,
        Err(_) => {
            hg_metrics::increment_counter("worktree.disk.check_error", 1);
            None
        }
    };
    if let Some((space, below_warning)) = check {
        let is_agent = agentdetect::is_agent();
        record_disk_check_metrics(space.available, below_warning, is_agent);
        if below_warning {
            logger.warn(disk_low_warning_message(
                space.available,
                space.total,
                &dest,
                is_agent,
            ));
        }
    }

    let source = checkouts.add_source()?;

    let pre_hooks = hook::Hooks::from_config(repo.config(), ctx.io(), "pre-worktree-add");
    pre_hooks.run_hooks(
        Some(repo),
        true,
        Some(&HashMap::from([
            ("path".to_string(), dest.display().to_string()),
            (
                "source".to_string(),
                canonical_repo_path.display().to_string(),
            ),
        ])),
    )?;

    let use_direct_copy: bool = repo
        .config()
        .get_or("worktree", "snapshot-direct-copy", || false)?;

    if ctx.opts.snapshot {
        let parents = workingcopy::fast_path_wdir_parents_with_config(
            repo.path(),
            repo.ident(),
            repo.config(),
        )?;
        if parents.p2().is_some() {
            bail!(
                "working copy has two parents; \
                 snapshot cannot preserve merge state"
            );
        }
    }

    let legacy_snapshot_handle = if ctx.opts.snapshot && !use_direct_copy {
        Some(take_snapshot_legacy(ctx, repo))
    } else {
        None
    };

    // Lock the source checkout path only while snapshotting the source state that
    // needs to be copied into the new worktree.
    let (prepared, source_status) = with_worktree_path_op_lock(
        repo.config().as_ref(),
        &shared_store_path,
        &canonical_repo_path,
        || {
            let prepared = source.capture(requested_target)?;
            let source_status = if ctx.opts.snapshot && use_direct_copy {
                logger.info("computing working copy status...");
                let matcher = Arc::new(pathmatcher::AlwaysMatcher::new());
                Some(wc.status(&ctx.core, matcher, false)?)
            } else {
                None
            };
            Ok((prepared, source_status))
        },
    )?;

    // Reserve a slot before cloning. The reservation counts against max-count,
    // so a concurrent add racing for the last slot fails fast here rather than
    // cloning its destination and then aborting during registration (which would
    // orphan the just-cloned checkout). Reservations live in a separate file
    // (never touched by older `sl` binaries) and the lock is held only briefly,
    // so parallel adds are not serialized across the slow clone. `reservation`
    // releases the slot on drop / after registration.
    //
    // Skipped entirely when the limit is disabled (`max-count = 0`, the default):
    // reservations only affect enforcement, so there is nothing to reserve and no
    // reason to take the lock or rewrite the reservations file.
    let mut reservation = if max_count > 0 {
        let reservation_id = new_reservation_id();
        let reservation_ttl_seconds = crate::reservation_ttl_seconds(repo)?;
        with_reservations(&shared_store_path, |registry, reservations| {
            reservations.prune_stale(reservation_ttl_seconds);
            // Our own reservation is inserted only after this check, so it is not
            // counted here.
            let in_use = worktrees.occupied_slots(registry, reservations, &group_main_path)?;
            if in_use >= max_count as usize {
                abort!(
                    "{}",
                    worktree_limit_reached_message(repo, max_count, in_use)
                );
            }
            reservations.reservations.insert(
                reservation_id.clone(),
                Reservation::now(group_main_path.clone(), dest.clone()),
            );
            Ok(())
        })?;
        Some(SlotReservation {
            shared_store_path: shared_store_path.clone(),
            reservation_id,
            dest: dest.clone(),
            logger: logger.clone(),
            released: false,
        })
    } else {
        None
    };

    // Lock the destination path while creating and initializing that checkout.
    let cleanup_command = checkouts.cleanup_command(&dest);
    with_worktree_path_op_lock(repo.config().as_ref(), &shared_store_path, &dest, || {
        if dest.exists() {
            abort!("destination path '{}' already exists", dest.display());
        }
        check_dest_not_in_repo(&dest)?;

        prepared.create(&dest).inspect_err(|_| {
            if let Some(command) = &cleanup_command {
                ctx.logger().warn(format!(
                    "worktree add may have left a partial checkout; try running `{command}` to recover"
                ));
            }
        })
    })?;

    let label = (!ctx.opts.label.is_empty()).then_some(ctx.opts.label.as_str());
    let marker_error = worktrees
        .register(&group_main_path, &dest, label)
        .inspect_err(|_| {
            if let Some(command) = &cleanup_command {
                logger.warn(format!(
                    "worktree add left a checkout at {} that could not be registered; \
                     try running `{command}` to recover",
                    dest.display(),
                ));
            }
        })?;
    if let Some(error) = marker_error {
        logger.warn(format!(
            "failed to write worktree-name marker for {}: {error:#}",
            dest.display()
        ));
    }
    // The reserved slot (if any) is now a registered worktree; release it promptly
    // so it is not double-counted. `Drop` is a backstop if this early-returns.
    if let Some(reservation) = reservation.as_mut() {
        reservation.release();
    }

    logger.info(format!("created linked worktree at {}", dest.display()));

    let snapshot_result = if let Some(handle) = legacy_snapshot_handle {
        // Legacy path: upload+restore via `sl snapshot create/checkout`.
        restore_snapshot_legacy(ctx, handle, &dest)
    } else if let Some(source_status) = source_status {
        // Direct copy path: read dirty files from source and write into dest.
        snapshot_with_direct_workingcopy_patch(ctx, repo, source_status, &dest)
    } else {
        Ok(())
    };
    if let Err(e) = snapshot_result {
        if let Err(recovery_err) = restore_clean_state(&dest, &logger) {
            logger.warn(format!(
                "also failed to restore clean state: {recovery_err:#}"
            ));
        }
        return Err(e);
    }

    let post_hooks = hook::Hooks::from_config(repo.config(), ctx.io(), "post-worktree-add");
    post_hooks.run_hooks(
        Some(repo),
        false,
        Some(&HashMap::from([
            ("path".to_string(), dest.display().to_string()),
            (
                "source".to_string(),
                canonical_repo_path.display().to_string(),
            ),
        ])),
    )?;

    Ok(0)
}

/// Resolve `--rev`. Revisions the native resolver doesn't know, such as revset
/// expressions and Phabricator diffs, are resolved by `log`, which pulls
/// missing commits. Returns `Err(exit_code)` if `log` failed;
/// it has already printed why.
fn resolve_rev(ctx: &ReqCtx<WorktreeOpts>, repo: &Repo) -> Result<std::result::Result<HgId, u8>> {
    let rev = ctx.opts.rev.as_str();
    let resolved = repo.resolve_commit(rev)?;
    if !matches!(resolved, ResolveResult::NotFound(_)) {
        return Ok(Ok(resolved.any()?));
    }

    tracing::info!(target: "command_info", worktree_rev_fallback = true);

    let global_opts = ctx.global_opts();
    let configs = global_opts
        .config
        .iter()
        .flat_map(|c| ["--config", c.as_str()]);
    let configfiles = global_opts
        .configfile
        .iter()
        .flat_map(|c| ["--configfile", c.as_str()]);
    let mut cmd = Command::new(current_sl_binary());
    // Stay in the current directory so relative paths in the revset mean the same thing.
    cmd.arg("-R")
        .arg(repo.path())
        .args(configs)
        .args(configfiles)
        // An alias like `log = log -G` would change the output.
        .args(["--config", "alias.log=log"])
        // `log` is read-only, so it would pull a missing commit hidden, but we
        // check the commit out like `goto` does.
        .args(["--config", "experimental.autopull-always-visible=true"])
        // Prompts may also write to stdout, so mark the template's node lines.
        .args(["log", "-r", rev, "-l", "2", "-T", r"node: {node}\n"])
        // Without a stdin, a diff with several local versions resolves to the
        // newest one with a warning, or aborts for agents.
        .stdin(Stdio::null());
    // Revset functions like `obsolete()` only see hidden commits with `--hidden`.
    if global_opts.hidden {
        cmd.arg("--hidden");
    }

    // Like hooks, write straight to a terminal, and otherwise go through our IO.
    ctx.io().flush()?;
    let stderr_is_tty = ctx.io().error().is_tty();
    if stderr_is_tty {
        cmd.stderr(Stdio::inherit());
    }
    let output = {
        let _blocked = ctx
            .io()
            .time_interval()
            .scoped_blocked_interval("worktree-rev".into());
        cmd.output()
            .with_context(|| format!("failed to run {} log", identity::cli_name()))?
    };
    if !stderr_is_tty {
        ctx.io().write_err(&output.stderr)?;
    }
    if !output.status.success() {
        return Ok(Err(output.status.code().map_or(255, |code| code as u8)));
    }

    let id = parse_log_node(&output.stdout)?;
    // The child may have pulled the commit. Reload the commit graph so
    // `eden clone` sees it and doesn't resolve it again.
    repo.invalidate_dag_commits()?;
    Ok(Ok(id))
}

/// Parses the single node printed by the log template, ignoring prompt output.
fn parse_log_node(stdout: &[u8]) -> Result<HgId> {
    let stdout = String::from_utf8_lossy(stdout);
    let mut lines = stdout
        .lines()
        .map(str::trim)
        .filter_map(|line| line.strip_prefix("node: "));
    let line = lines.next().context("empty revision set")?;
    if lines.next().is_some() {
        abort!(
            "revision set contains multiple commits; use first(...), last(...), or a commit hash"
        );
    }
    HgId::from_hex(line.as_bytes()).with_context(|| {
        format!(
            "unexpected output from {} log: {line:?}",
            identity::cli_name()
        )
    })
}

type SnapshotHandle = std::thread::JoinHandle<anyhow::Result<String>>;

fn take_snapshot_legacy(ctx: &ReqCtx<WorktreeOpts>, repo: &Repo) -> SnapshotHandle {
    let repo_path = repo.path().to_path_buf();
    let sl_bin = current_sl_binary();
    ctx.logger()
        .info("creating snapshot of current working copy...");
    std::thread::spawn(move || sapling_snapshot_create(&sl_bin, repo_path))
}

fn restore_snapshot_legacy(
    ctx: &ReqCtx<WorktreeOpts>,
    handle: SnapshotHandle,
    dest: &Path,
) -> anyhow::Result<()> {
    let logger = ctx.logger();

    let id = handle
        .join()
        .map_err(|_| anyhow::anyhow!("snapshot create thread panicked"))
        .and_then(|r| r)
        .context("creating snapshot")?;

    logger.info(format!("created snapshot {id}"));
    logger.info(format!("restoring snapshot {id} into worktree..."));

    let sl_bin = current_sl_binary();
    sapling_snapshot_checkout(&sl_bin, dest, &id).context("restoring snapshot")?;

    Ok(())
}

/// Runs the `worktree.path-generator` command and returns the generated path.
fn run_path_generator(
    repo: &Repo,
    label: &str,
    canonical_source: &Path,
    require_generated: bool,
) -> Result<PathBuf> {
    let generator_cmd = repo
        .config()
        .get("worktree", "path-generator")
        .with_context(|| {
            if require_generated {
                "worktree.path-generator is required when \
                 worktree.require-generated-path=true"
            } else {
                "worktree.path-generator is not configured; pass a PATH argument"
            }
        })?;

    let current_exe = std::env::current_exe().ok();
    let exe_str = current_exe
        .as_ref()
        .and_then(|p| p.to_str())
        .unwrap_or_else(|| identity::cli_name());

    let mut cmd = Command::new_shell(generator_cmd.as_ref());
    cmd.current_dir(repo.path());
    cmd.env("HG_SOURCE", canonical_source.as_os_str());
    cmd.env("SL_SOURCE", canonical_source.as_os_str());
    cmd.env("HG_LABEL", label);
    cmd.env("SL_LABEL", label);
    cmd.env("HG", exe_str);
    cmd.env("SL", exe_str);

    let output = cmd
        .output()
        .context("failed to run worktree.path-generator")?;

    if !output.status.success() {
        let mut msg = format!("worktree.path-generator exited with {}", output.status);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        if !stderr.is_empty() {
            msg.push_str(": ");
            msg.push_str(stderr);
        }
        bail!(msg);
    }

    let path_bytes = parse_generated_path_bytes(&output.stdout)?;
    let display_path = display_path_bytes(path_bytes);
    if path_bytes.contains(&b'\0') {
        bail!("worktree.path-generator returned invalid path '{display_path}': contains NUL byte");
    }
    let path = shell_output_bytes_to_path(path_bytes)
        .context("worktree.path-generator output could not be decoded as a path")?
        .into_owned();
    if !path.is_absolute() {
        bail!("worktree.path-generator must return an absolute path, got '{display_path}'");
    }

    let path = util::path::strip_unc_prefix(
        util::path::canonical_path_allow_missing(&path).with_context(|| {
            format!("worktree.path-generator returned invalid path '{display_path}'")
        })?,
    );
    Ok(path)
}

fn parse_generated_path_bytes(stdout: &[u8]) -> Result<&[u8]> {
    let stdout = stdout
        .strip_suffix(b"\r\n")
        .or_else(|| stdout.strip_suffix(b"\n"))
        .unwrap_or(stdout);

    if stdout.is_empty() {
        bail!("worktree.path-generator returned empty output");
    }

    if stdout.contains(&b'\r') || stdout.contains(&b'\n') {
        bail!("worktree.path-generator must write exactly one path to stdout");
    }

    Ok(stdout)
}

fn display_path_bytes(bytes: &[u8]) -> String {
    if let Ok(text) = std::str::from_utf8(bytes) {
        return text.escape_debug().to_string();
    }

    let mut escaped = String::new();
    for &byte in bytes {
        for ch in std::ascii::escape_default(byte) {
            let _ = escaped.write_char(ch.into());
        }
    }
    escaped
}

const DEFAULT_CONCURRENCY: usize = 16;
const VFS_BATCH_SIZE: usize = 128;

fn snapshot_with_direct_workingcopy_patch(
    ctx: &ReqCtx<WorktreeOpts>,
    repo: &Repo,
    status: status::Status,
    dest: &Path,
) -> anyhow::Result<()> {
    let logger = ctx.logger();

    if !status.dirty() && status.unknown().next().is_none() {
        logger.info("working copy is clean, nothing to copy");
        return Ok(());
    }

    let src_vfs = vfs::VFS::new(repo.path().to_path_buf())?;
    let dst_vfs = vfs::VFS::new_destructive(dest.to_path_buf())?;

    logger.info("applying working copy changes to new worktree...");

    // Phase 1: remove files before writing to avoid races where a write
    // lands at a path that a concurrent remove is about to delete.
    remove_items(&dst_vfs, &status, &logger)?;

    // Phase 2: copy modified, added, and untracked files.
    copy_items(&src_vfs, &dst_vfs, &status, &logger)?;

    update_dest_treestate(dest, &status).context("failed to update treestate after file copy")?;

    logger.info("working copy changes applied to new worktree");
    Ok(())
}

fn restore_clean_state(dest: &Path, logger: &TermLogger) -> anyhow::Result<()> {
    let sl_bin = current_sl_binary();
    Command::new(&sl_bin)
        .args(["goto", ".", "--clean"])
        .current_dir(dest)
        .checked_run()
        .with_context(|| format!("restoring clean state in {}", dest.display()))?;
    logger.info("restored worktree to clean state");
    Ok(())
}

fn batch_workers(total: usize) -> usize {
    match std::thread::available_parallelism() {
        Ok(v) => v.get().min(DEFAULT_CONCURRENCY).min(total / VFS_BATCH_SIZE),
        Err(_) => 1,
    }
    .max(1)
}

/// Remove deleted/removed files from `dst_vfs` in parallel.
fn remove_items(
    dst_vfs: &vfs::VFS,
    status: &status::Status,
    logger: &TermLogger,
) -> anyhow::Result<()> {
    let paths: Vec<_> = status.removed().chain(status.deleted()).collect();
    if paths.is_empty() {
        return Ok(());
    }
    let work: Vec<_> = paths
        .into_iter()
        .map(|path| vfs::Work::Remove(path.to_owned()))
        .collect();
    let result_items = dst_vfs.batch_items(batch_workers(work.len()), Items::ready(work));
    drain_workers(result_items, logger)
}

/// Read modified/added/untracked files from `src_vfs` and write them to
/// `dst_vfs` in parallel.
fn copy_items(
    src_vfs: &vfs::VFS,
    dst_vfs: &vfs::VFS,
    status: &status::Status,
    logger: &TermLogger,
) -> anyhow::Result<()> {
    let paths: Vec<_> = status
        .modified()
        .chain(status.added())
        .chain(status.unknown())
        .map(|path| path.to_owned())
        .collect();
    if paths.is_empty() {
        return Ok(());
    }
    let workers = batch_workers(paths.len());
    let src_vfs = src_vfs.clone();
    let read_items = SlexWork::try_map(
        WorkOptions::new().max_workers(workers).inline_items(1),
        Items::ready(paths),
        move |path| {
            let (data, metadata) = src_vfs
                .read_with_metadata(&path)
                .with_context(|| format!("reading {path}"))
                .map_err(vfs::VfsBatchError::Batch)?;
            let flag = workingcopy::metadata::Metadata::from(metadata).to_update_flag(&src_vfs);
            Ok(vfs::Work::Write(path, data.into(), flag, None))
        },
    );
    let result_items = dst_vfs.batch_items(workers, read_items);
    drain_workers(result_items, logger)
}

fn drain_workers(result_items: vfs::VfsBatchItems, logger: &TermLogger) -> anyhow::Result<()> {
    let mut errors: Vec<(String, types::RepoPathBuf, anyhow::Error)> = Vec::new();
    let mut result = Ok(());
    for result_batch in result_items.into_batches() {
        match result_batch {
            Ok(_) => {}
            Err(vfs::VfsBatchError::Work(work, err)) => {
                let (action, path) = match work {
                    vfs::Work::Remove(p) => ("removing", p),
                    vfs::Work::Write(p, ..) => ("writing", p),
                    vfs::Work::SetExecutable(p, ..) => ("setting executable", p),
                    _ => ("applying", types::RepoPathBuf::new()),
                };
                errors.push((action.to_owned(), path, err));
            }
            Err(vfs::VfsBatchError::Batch(err)) if result.is_ok() => result = Err(err),
            Err(vfs::VfsBatchError::Batch(_)) => {}
        }
    }

    for (action, path, err) in &errors {
        logger.warn(format!("failed {action} {path}: {err:#}"));
    }

    result?;

    if !errors.is_empty() {
        bail!("{} file(s) failed during apply", errors.len());
    }

    Ok(())
}

/// Update destination treestate so `sl status` in the dest matches the source.
///
/// Only `added` and `removed` files need treestate entries. Modified, untracked,
/// and deleted files are detected automatically by EdenFS (`--snapshot` requires
/// EdenFS).
fn update_dest_treestate(dest: &Path, status: &status::Status) -> anyhow::Result<()> {
    use treestate::filestate::FileStateV2;
    use treestate::filestate::StateFlags;

    let added: Vec<_> = status.added().collect();
    let removed: Vec<_> = status.removed().collect();
    if added.is_empty() && removed.is_empty() {
        return Ok(());
    }

    let dest_repo = Repo::load(dest, &[])?;
    let dest_wc = dest_repo.working_copy()?;
    let dest_wc = dest_wc.read();
    let ts = dest_wc.treestate();
    let mut ts = ts.lock();

    for path in &removed {
        ts.insert(
            path.as_byte_slice(),
            &FileStateV2 {
                mode: 0,
                size: 0,
                mtime: -1,
                state: StateFlags::EXIST_P1,
                copied: None,
            },
        )?;
    }

    for path in &added {
        ts.insert(
            path.as_byte_slice(),
            &FileStateV2 {
                mode: 0,
                size: -1,
                mtime: -1,
                state: StateFlags::EXIST_NEXT,
                copied: None,
            },
        )?;
    }

    ts.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const NODE: &str = "1111111111111111111111111111111111111111";

    #[test]
    fn test_parse_log_node() {
        const OTHER: &str = "2222222222222222222222222222222222222222";
        assert_eq!(
            parse_log_node(b"").unwrap_err().to_string(),
            "empty revision set",
        );
        assert_eq!(
            parse_log_node(
                format!("which commit to select [1-2/(c)ancel]? 1\nnode: {NODE}\n").as_bytes()
            )
            .unwrap(),
            HgId::from_hex(NODE.as_bytes()).unwrap(),
        );
        assert_eq!(
            parse_log_node(format!("node: {OTHER}\nnode: {NODE}\n").as_bytes())
                .unwrap_err()
                .to_string(),
            "revision set contains multiple commits; use first(...), last(...), or a commit hash",
        );
        assert!(
            parse_log_node(format!("@  {NODE}\n").as_bytes()).is_err(),
            "graph output"
        );
    }
}
