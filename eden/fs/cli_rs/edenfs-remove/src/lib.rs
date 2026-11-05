/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
#[cfg(unix)]
use std::fs::Permissions;
use std::future::Future;
use std::io::ErrorKind;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use edenfs_core::checkout::CheckoutConfig;
use edenfs_core::checkout::EdenFsCheckout;
use edenfs_core::checkout::find_checkout;
use edenfs_core::checkout::get_mounts;
use edenfs_core::fsutil;
use edenfs_core::fsutil::forcefully_remove_dir_all;
use edenfs_core::instance::EdenFsInstance;
use edenfs_core::redirect::Redirection;
use edenfs_core::redirect::RedirectionBackingCleanupAction;
use edenfs_core::redirect::RedirectionBackingCleanupPlanner;
#[cfg(target_os = "linux")]
use edenfs_core::redirect::finish_backing_cleanup_unmount;
use edenfs_core::redirect::get_effective_redirections;
#[cfg(target_os = "linux")]
use edenfs_core::redirect::redirection_mount_status;
use edenfs_utils::is_active_eden_mount;
#[cfg(windows)]
use edenfs_utils::path_from_bytes;
use fail::fail_point;
use tracing::debug;
use tracing::warn;

const DEFAULT_AUXILIARY_PROCESS_TIMEOUT: Duration = Duration::from_secs(60);

#[cfg(target_os = "macos")]
const BACKING_CLEANUP_RECOVERY: &str = "APFS volumes left behind require manual cleanup.";
#[cfg(not(target_os = "macos"))]
const BACKING_CLEANUP_RECOVERY: &str =
    "Run `eden du --clean-orphaned` to reclaim leftover backing storage.";

/// Whether checkout removal should preserve or delete managed redirection backing targets.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BackingCleanupPolicy {
    /// Leave all redirection backing targets intact.
    Preserve,
    /// Delete every managed redirection backing target.
    DeleteManaged,
}

/// Options controlling removal of one registered EdenFS checkout.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RemoveCheckoutOptions {
    /// Leave the checkout mount-point directory in place after unregistering it.
    pub preserve_mount_point: bool,
    /// Disable forced EdenFS unmounting.
    pub no_force: bool,
    /// Maximum wait for cleanup planning and redirection unmounts.
    pub auxiliary_process_timeout: Duration,
    /// Policy for redirection backing targets.
    pub backing_cleanup: BackingCleanupPolicy,
}

impl Default for RemoveCheckoutOptions {
    fn default() -> Self {
        Self {
            preserve_mount_point: false,
            no_force: false,
            auxiliary_process_timeout: DEFAULT_AUXILIARY_PROCESS_TIMEOUT,
            backing_cleanup: BackingCleanupPolicy::Preserve,
        }
    }
}

/// Return `configured`, else `EDENFS_AUX_PROCESSES_TIMEOUT_SECS`, else the default timeout.
pub fn auxiliary_process_timeout(configured: Option<Duration>) -> Duration {
    configured
        .or_else(|| {
            std::env::var("EDENFS_AUX_PROCESSES_TIMEOUT_SECS")
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .map(Duration::from_secs)
        })
        .unwrap_or(DEFAULT_AUXILIARY_PROCESS_TIMEOUT)
}

/// A non-fatal condition encountered while removing a checkout.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RemoveCheckoutWarning {
    /// Redirection cleanup failed before checkout removal continued.
    RedirectionUnmountFailed { path: PathBuf, error: String },
    /// Redirection cleanup exceeded the configured timeout before checkout removal continued.
    RedirectionUnmountTimedOut { path: PathBuf, timeout: Duration },
    /// Managed backing cleanup was skipped because the checkout's client config is missing.
    BackingCleanupSkipped { path: PathBuf },
    /// Managed backing cleanup skipped repository-configured redirections because the checkout
    /// is not mounted.
    RepositoryRedirectionCleanupSkipped { path: PathBuf },
    /// Some managed backing could not be planned or deleted before checkout removal continued.
    BackingCleanupFailed { path: PathBuf, error: String },
}

/// Progress and non-fatal warnings reported while removing a checkout.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RemoveCheckoutEvent {
    /// Redirections are about to be unmounted.
    UnmountingRedirections,
    /// A backing target is about to be deleted or emptied.
    DeletingBackingStorage { path: PathBuf },
    /// The checkout is about to be unmounted.
    UnmountingCheckout,
    /// The checkout's EdenFS client state is about to be deleted.
    DeletingCheckoutState,
    /// Removal continues after this warning.
    Warning(RemoveCheckoutWarning),
}

impl fmt::Display for RemoveCheckoutWarning {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RedirectionUnmountFailed { path, error } => write!(
                formatter,
                "Error unmounting redirections for {}: {error}",
                path.display()
            ),
            Self::RedirectionUnmountTimedOut { path, timeout } => write!(
                formatter,
                "Unmounting redirections for {} timed out after {} seconds. Continuing with unmount...",
                path.display(),
                timeout.as_secs_f64()
            ),
            Self::BackingCleanupSkipped { path } => write!(
                formatter,
                "Skipping redirection backing cleanup for {} because its checkout config is missing. {BACKING_CLEANUP_RECOVERY}",
                path.display()
            ),
            Self::RepositoryRedirectionCleanupSkipped { path } => write!(
                formatter,
                "Cannot inspect repository-configured redirections while {} is unmounted. Their backing storage may remain. {BACKING_CLEANUP_RECOVERY}",
                path.display()
            ),
            Self::BackingCleanupFailed { path, error } => write!(
                formatter,
                "Redirection backing cleanup did not finish for {}: {error}. Continuing with removal. {BACKING_CLEANUP_RECOVERY}",
                path.display()
            ),
        }
    }
}

/// Remove one registered EdenFS checkout.
pub async fn remove_checkout(
    instance: &EdenFsInstance,
    path: &Path,
    options: RemoveCheckoutOptions,
) -> Result<()> {
    remove_checkout_with_warning_handler(instance, path, options, |warning| warn!("{warning}"))
        .await
}

/// Remove one registered EdenFS checkout and report non-fatal warnings to the caller.
pub async fn remove_checkout_with_warning_handler(
    instance: &EdenFsInstance,
    path: &Path,
    options: RemoveCheckoutOptions,
    mut warning_handler: impl FnMut(RemoveCheckoutWarning),
) -> Result<()> {
    remove_checkout_with_event_handler(instance, path, options, |event| {
        if let RemoveCheckoutEvent::Warning(warning) = event {
            warning_handler(warning);
        }
    })
    .await
}

/// Remove one registered EdenFS checkout and report progress and warnings to the caller.
pub async fn remove_checkout_with_event_handler(
    instance: &EdenFsInstance,
    path: &Path,
    options: RemoveCheckoutOptions,
    mut event_handler: impl FnMut(RemoveCheckoutEvent),
) -> Result<()> {
    let path = canonicalize_checkout_path(path).await?;
    let client_name = registered_client(instance, &path)?
        .ok_or_else(|| anyhow!("{} is not a registered EdenFS checkout", path.display()))?;
    let checkout_is_mounted = is_mounted_checkout(instance, &path).await;

    let client_config = instance.config_directory(&client_name).join("config.toml");
    let backing_cleanup = match options.backing_cleanup {
        BackingCleanupPolicy::DeleteManaged => match tokio::fs::try_exists(&client_config).await {
            Ok(true) => BackingCleanupPolicy::DeleteManaged,
            // Client state is deleted after backing cleanup, so a retry of an
            // interrupted removal has no redirections left to plan from.
            Ok(false) => {
                event_handler(RemoveCheckoutEvent::Warning(
                    RemoveCheckoutWarning::BackingCleanupSkipped { path: path.clone() },
                ));
                BackingCleanupPolicy::Preserve
            }
            Err(error) => {
                event_handler(RemoveCheckoutEvent::Warning(
                    RemoveCheckoutWarning::BackingCleanupFailed {
                        path: path.clone(),
                        error: format!("failed to check {}: {error}", client_config.display()),
                    },
                ));
                BackingCleanupPolicy::Preserve
            }
        },
        BackingCleanupPolicy::Preserve => BackingCleanupPolicy::Preserve,
    };

    let redirection_cleanup = redirection_cleanup_policy(checkout_is_mounted, backing_cleanup);
    if redirection_cleanup.is_some()
        && let Err(error) = clear_redirection_targets(instance, &client_name).await
    {
        event_handler(RemoveCheckoutEvent::Warning(
            RemoveCheckoutWarning::RedirectionUnmountFailed {
                path: path.clone(),
                error: format!("{error:#}"),
            },
        ));
    }
    match redirection_cleanup {
        Some(BackingCleanupPolicy::DeleteManaged) => {
            // `.eden-redirections` is checkout content, so only client config redirections
            // can be planned while the checkout is unmounted.
            if !checkout_is_mounted {
                event_handler(RemoveCheckoutEvent::Warning(
                    RemoveCheckoutWarning::RepositoryRedirectionCleanupSkipped {
                        path: path.clone(),
                    },
                ));
            }
            clean_up_managed_backing(
                instance,
                &path,
                checkout_is_mounted,
                options.auxiliary_process_timeout,
                &mut event_handler,
            )
            .await;
        }
        Some(BackingCleanupPolicy::Preserve) => {
            event_handler(RemoveCheckoutEvent::UnmountingRedirections);
            unmount_redirections_with_timeout(
                &path,
                options.auxiliary_process_timeout,
                unmount_redirections(instance, &path),
                &mut |warning| event_handler(RemoveCheckoutEvent::Warning(warning)),
            )
            .await;
        }
        None => {}
    }

    if checkout_is_mounted {
        remove_active_checkout(instance, &path, &client_name, options, &mut event_handler).await
    } else {
        remove_inactive_checkout(instance, &path, &client_name, options, &mut event_handler).await
    }
}

fn redirection_cleanup_policy(
    checkout_is_mounted: bool,
    backing_cleanup: BackingCleanupPolicy,
) -> Option<BackingCleanupPolicy> {
    // A redirection mount can outlive the checkout root, so managed cleanup
    // always tries to unmount redirections before deleting their backing.
    match (checkout_is_mounted, backing_cleanup) {
        (_, BackingCleanupPolicy::DeleteManaged) => Some(BackingCleanupPolicy::DeleteManaged),
        (true, BackingCleanupPolicy::Preserve) => Some(BackingCleanupPolicy::Preserve),
        (false, BackingCleanupPolicy::Preserve) => None,
    }
}

/// Whether `path` is the root of a mounted checkout.
#[cfg(not(windows))]
async fn is_mounted_checkout(_instance: &EdenFsInstance, path: &Path) -> bool {
    is_active_eden_mount(path)
}

/// Whether `path` is the root of a mounted checkout. On Windows, `.eden\config` stays in
/// the folder after the checkout is unmounted, so the daemon is asked as well. If it can't
/// answer, the metadata decides, as it does on the other platforms.
#[cfg(windows)]
async fn is_mounted_checkout(instance: &EdenFsInstance, path: &Path) -> bool {
    if !is_active_eden_mount(path) {
        return false;
    }
    let path_key = dunce::simplified(path);
    match instance
        .daemon()
        .list_mounts(Some(Duration::from_secs(3)))
        .await
    {
        Ok(mounts) => mounts.iter().any(|mount| {
            path_from_bytes(&mount.mountPoint)
                .is_ok_and(|mount_path| dunce::simplified(&mount_path) == path_key)
        }),
        Err(error) => {
            debug!(path = %path.display(), %error, "Could not list mounts, assuming the checkout is mounted");
            true
        }
    }
}

/// Canonicalize `path`. If its folder is gone, canonicalize its closest existing ancestor
/// instead, so that a registered checkout whose folder was deleted can still be found and
/// removed.
pub async fn canonicalize_checkout_path(path: &Path) -> Result<PathBuf> {
    let error = match tokio::fs::canonicalize(path).await {
        Ok(path) => return Ok(path),
        Err(error) => error,
    };
    if error.kind() == ErrorKind::NotFound
        && let Ok(absolute) = std::path::absolute(path)
    {
        for ancestor in absolute.ancestors().skip(1) {
            match tokio::fs::canonicalize(ancestor).await {
                Ok(canonical) => {
                    let missing = absolute
                        .strip_prefix(ancestor)
                        .expect("should be a prefix, since it is an ancestor");
                    return Ok(canonical.join(missing));
                }
                Err(ancestor_error) if ancestor_error.kind() == ErrorKind::NotFound => {}
                Err(_) => break,
            }
        }
    }
    Err(error).with_context(|| format!("failed to canonicalize checkout path {}", path.display()))
}

/// Return the client registered for `path` in the EdenFS directory map.
fn registered_client(instance: &EdenFsInstance, path: &Path) -> Result<Option<String>> {
    let path_key = dunce::simplified(path);
    Ok(instance
        .get_configured_mounts_map()?
        .into_iter()
        .find(|(registered_path, _)| dunce::simplified(registered_path) == path_key)
        .map(|(_, client_name)| client_name))
}

struct RedirectionCleanup {
    redirection: Redirection,
    actions: Vec<RedirectionBackingCleanupAction>,
}

struct ManagedBackingCleanupPlan {
    checkout: EdenFsCheckout,
    redirections: Vec<RedirectionCleanup>,
    /// Redirections whose backing could not be planned. They are still unmounted.
    failures: Vec<String>,
}

#[tracing::instrument(level = "debug", skip_all, fields(path = %path.display()))]
async fn plan_managed_backing_cleanup(
    instance: &EdenFsInstance,
    path: &Path,
    timeout: Duration,
) -> Result<ManagedBackingCleanupPlan> {
    let instance = instance.clone();
    let path = path.to_path_buf();
    let path_for_error = path.clone();
    run_blocking_operation_with_timeout("eden-backing-cleanup-plan", timeout, move || {
        let checkout = find_checkout(&instance, &path)
            .with_context(|| format!("failed to find checkout for {}", path.display()))?;
        let mut redirections = Vec::new();
        let mut failures = Vec::new();
        let mut planner = RedirectionBackingCleanupPlanner::new(&path);
        for (repo_path, redirection) in get_effective_redirections(&instance, &checkout)
            .with_context(|| format!("failed to get redirections for {}", path.display()))?
        {
            // Plan each redirection on its own so one that fails validation doesn't
            // keep the others' backing.
            let label = repo_path.display().to_string();
            let planned = BTreeMap::from([(repo_path, redirection)]);
            let actions = planner.plan(&planned).unwrap_or_else(|error| {
                failures.push(format!("{label}: {error:#}"));
                Vec::new()
            });
            let redirection = planned
                .into_values()
                .next()
                .expect("the single planned redirection should still be present");
            redirections.push(RedirectionCleanup {
                redirection,
                actions,
            });
        }
        Ok(ManagedBackingCleanupPlan {
            checkout,
            redirections,
            failures,
        })
    })
    .await
    .with_context(|| {
        format!(
            "failed to plan managed redirection backing cleanup for {}",
            path_for_error.display()
        )
    })
}

async fn run_blocking_operation_with_timeout<T, Operation>(
    thread_name: &'static str,
    timeout: Duration,
    operation: Operation,
) -> Result<T>
where
    T: Send + 'static,
    Operation: FnOnce() -> Result<T> + Send + 'static,
{
    // A timed-out filesystem call can stay blocked; Tokio runtime shutdown must not wait for it.
    let (result_sender, result_receiver) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name(thread_name.to_owned())
        .spawn(move || {
            let _ = result_sender.send(operation());
        })
        .with_context(|| format!("failed to start {thread_name} task"))?;

    match tokio::time::timeout(timeout, result_receiver).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err(anyhow!("{thread_name} task failed")),
        Err(_) => Err(anyhow!(
            "{thread_name} task timed out after {} seconds; it may continue until this process exits",
            timeout.as_secs_f64()
        )),
    }
}

fn get_test_delay() -> Option<Duration> {
    std::env::var("TEST_ONLY_AUX_PROCESSES_STOP_DELAY_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_secs)
}

/// EdenFS on Windows refuses to delete the symlink of a redirection listed in the checkout's
/// redirection targets, so forget them before unmounting redirections, as
/// `edenfsctl redirect unmount` does.
async fn clear_redirection_targets(instance: &EdenFsInstance, client_name: &str) -> Result<()> {
    let config_dir = instance.config_directory(client_name);
    if !tokio::fs::try_exists(config_dir.join("config.toml")).await? {
        return Ok(());
    }
    let mut checkout_config = CheckoutConfig::parse_config(&config_dir).with_context(|| {
        format!(
            "failed to parse checkout config in {}",
            config_dir.display()
        )
    })?;
    checkout_config
        .remove_redirection_targets(&config_dir)
        .context("failed to remove redirection targets from the checkout config")?;
    Ok(())
}

async fn unmount_redirections(instance: &EdenFsInstance, path: &Path) -> Result<()> {
    if let Some(delay) = get_test_delay() {
        debug!("Injecting test delay of {} seconds", delay.as_secs());
        tokio::time::sleep(delay).await;
    }

    let checkout = find_checkout(instance, path)
        .with_context(|| format!("failed to find checkout for {}", path.display()))?;
    let redirections = get_effective_redirections(instance, &checkout)
        .with_context(|| format!("failed to get redirections for {}", path.display()))?;

    for redirection in redirections.values() {
        redirection
            .remove_existing(instance, &checkout, false, false, "fixup")
            .await
            .with_context(|| {
                format!(
                    "failed to unmount redirection {}",
                    redirection.repo_path().display()
                )
            })?;
    }
    Ok(())
}

#[tracing::instrument(level = "debug", skip_all, fields(path = %path.display()))]
async fn unmount_redirections_with_timeout<UnmountFuture, WarningHandler>(
    path: &Path,
    timeout: Duration,
    unmount: UnmountFuture,
    warning_handler: &mut WarningHandler,
) where
    UnmountFuture: Future<Output = Result<()>>,
    WarningHandler: FnMut(RemoveCheckoutWarning),
{
    let start = Instant::now();
    match tokio::time::timeout(timeout, unmount).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => warning_handler(RemoveCheckoutWarning::RedirectionUnmountFailed {
            path: path.to_path_buf(),
            error: format!("{error:#}"),
        }),
        Err(_) => warning_handler(RemoveCheckoutWarning::RedirectionUnmountTimedOut {
            path: path.to_path_buf(),
            timeout,
        }),
    }
    debug!(path = %path.display(), elapsed = ?start.elapsed(), "Finished unmounting redirections");
}

async fn remove_active_checkout(
    instance: &EdenFsInstance,
    path: &Path,
    client_name: &str,
    options: RemoveCheckoutOptions,
    event_handler: &mut impl FnMut(RemoveCheckoutEvent),
) -> Result<()> {
    event_handler(RemoveCheckoutEvent::UnmountingCheckout);
    let start = Instant::now();
    edenfs_core::unmount::unmount_for_removal(instance, path, options.no_force)
        .await
        .with_context(|| format!("failed to unmount mount point at {}", path.display()))?;
    debug!(path = %path.display(), elapsed = ?start.elapsed(), "Unmounted checkout");
    remove_inactive_checkout(instance, path, client_name, options, event_handler).await
}

/// How much of a redirection's backing can be deleted after trying to unmount it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BackingDeletionScope {
    /// The redirection is unmounted, so its backing targets can be deleted.
    Target,
    /// The redirection may still be mounted, so its backing targets are kept but emptied.
    Contents,
}

/// Unmount every redirection and delete its managed backing. This is best effort:
/// failures are reported as warnings so they never block checkout removal.
async fn clean_up_managed_backing<EventHandler>(
    instance: &EdenFsInstance,
    path: &Path,
    checkout_is_mounted: bool,
    timeout: Duration,
    event_handler: &mut EventHandler,
) where
    EventHandler: FnMut(RemoveCheckoutEvent),
{
    let start = Instant::now();
    let plan = plan_managed_backing_cleanup(instance, path, timeout).await;
    debug!(path = %path.display(), elapsed = ?start.elapsed(), "Finished planning redirection backing cleanup");
    let ManagedBackingCleanupPlan {
        checkout,
        redirections,
        failures,
    } = match plan {
        Ok(plan) => plan,
        Err(error) => {
            event_handler(RemoveCheckoutEvent::Warning(
                RemoveCheckoutWarning::BackingCleanupFailed {
                    path: path.to_path_buf(),
                    error: format!("{error:#}"),
                },
            ));
            return;
        }
    };
    if !failures.is_empty() {
        event_handler(RemoveCheckoutEvent::Warning(
            RemoveCheckoutWarning::BackingCleanupFailed {
                path: path.to_path_buf(),
                error: format!(
                    "failed to plan backing cleanup for {} redirection(s):\n{}",
                    failures.len(),
                    failures.join("\n")
                ),
            },
        ));
    }

    if !redirections.is_empty() {
        event_handler(RemoveCheckoutEvent::UnmountingRedirections);
    }
    let start = Instant::now();

    #[cfg(target_os = "linux")]
    let scopes = unmount_managed_redirections(
        instance,
        path,
        &checkout,
        &redirections,
        checkout_is_mounted,
        timeout,
        &mut |warning| event_handler(RemoveCheckoutEvent::Warning(warning)),
    )
    .await
    .unwrap_or_else(|error| {
        event_handler(RemoveCheckoutEvent::Warning(
            RemoveCheckoutWarning::RedirectionUnmountFailed {
                path: path.to_path_buf(),
                error: format!("{error:#}"),
            },
        ));
        vec![BackingDeletionScope::Contents; redirections.len()]
    });

    #[cfg(not(target_os = "linux"))]
    let checkout = &checkout;
    #[cfg(not(target_os = "linux"))]
    let scopes = unmount_each_redirection(
        path,
        timeout,
        Instant::now(),
        &redirections,
        |cleanup| async move {
            let redirection = &cleanup.redirection;
            let unmounted = redirection
                .unmount_for_backing_cleanup(instance, checkout)
                .await
                .with_context(|| {
                    format!(
                        "failed to unmount redirection {}",
                        redirection.repo_path().display()
                    )
                });
            // Bind unmounts go through the daemon, which only serves mounted checkouts.
            if checkout_is_mounted {
                unmounted
            } else {
                unmounted.with_context(|| {
                    format!(
                        "{} is not mounted, so EdenFS cannot unmount its redirections",
                        path.display()
                    )
                })
            }
        },
        &mut |warning| event_handler(RemoveCheckoutEvent::Warning(warning)),
    )
    .await;

    debug!(path = %path.display(), elapsed = ?start.elapsed(), "Finished unmounting redirections");
    let deletions = redirections
        .into_iter()
        .zip(scopes)
        .flat_map(|(cleanup, scope)| {
            cleanup
                .actions
                .into_iter()
                .map(move |action| (action, scope))
        })
        .collect();
    if let Err(error) = execute_backing_cleanup_actions(deletions, event_handler).await {
        event_handler(RemoveCheckoutEvent::Warning(
            RemoveCheckoutWarning::BackingCleanupFailed {
                path: path.to_path_buf(),
                error: format!("{error:#}"),
            },
        ));
    }
}

#[cfg(target_os = "linux")]
#[tracing::instrument(level = "debug", skip_all, fields(path = %path.display()))]
async fn unmount_managed_redirections(
    instance: &EdenFsInstance,
    path: &Path,
    checkout: &EdenFsCheckout,
    redirections: &[RedirectionCleanup],
    checkout_is_mounted: bool,
    timeout: Duration,
    warning_handler: &mut impl FnMut(RemoveCheckoutWarning),
) -> Result<Vec<BackingDeletionScope>> {
    if redirections.is_empty() {
        return Ok(Vec::new());
    }
    let start = Instant::now();
    let checkout_path = checkout.path();
    let paths: Vec<_> = redirections
        .iter()
        .map(|cleanup| checkout_path.join(cleanup.redirection.repo_path()))
        .collect();
    let before_paths = paths.clone();
    let mounted =
        run_blocking_operation_with_timeout("eden-redirection-mounts", timeout, move || {
            Ok(redirection_mount_status(&before_paths)?)
        })
        .await
        .context("failed to inspect redirection mounts")?;
    let candidates: Vec<_> = redirections.iter().zip(mounted).collect();
    let scopes = unmount_each_redirection(
        path,
        timeout,
        start,
        &candidates,
        |(cleanup, mounted)| async move {
            if *mounted {
                let unmounted = cleanup
                    .redirection
                    .detach_for_backing_cleanup(instance, checkout)
                    .await
                    .with_context(|| {
                        format!(
                            "failed to unmount redirection {}",
                            cleanup.redirection.repo_path().display()
                        )
                    });
                if checkout_is_mounted {
                    unmounted?;
                } else {
                    unmounted.with_context(|| {
                        format!(
                            "{} is not mounted, so EdenFS cannot unmount its redirections",
                            path.display()
                        )
                    })?;
                }
            }
            Ok(())
        },
        warning_handler,
    )
    .await;
    if !scopes.contains(&BackingDeletionScope::Target) {
        return Ok(scopes);
    }
    let remaining = timeout.saturating_sub(start.elapsed());
    if remaining.is_zero() {
        return Err(anyhow!("redirection unmount verification timed out"));
    }

    // All mount changes finish before this fresh observation. Path cleanup below
    // only unlinks symlinks or removes empty directories, so it cannot detach mounts.
    let verified =
        run_blocking_operation_with_timeout("eden-redirection-verify", remaining, move || {
            let mounted = redirection_mount_status(&paths)?;
            Ok(paths
                .iter()
                .zip(mounted)
                .zip(scopes)
                .map(|((repo_path, mounted), scope)| {
                    if scope == BackingDeletionScope::Contents {
                        return Ok(scope);
                    }
                    let unmounted = if mounted {
                        Err(anyhow!(
                            "redirection {} is still mounted after unmount",
                            repo_path.display()
                        ))
                    } else {
                        finish_backing_cleanup_unmount(repo_path).map_err(anyhow::Error::from)
                    };
                    unmounted.with_context(|| {
                        format!(
                            "failed to unmount redirection {}",
                            repo_path
                                .strip_prefix(&checkout_path)
                                .unwrap_or(repo_path)
                                .display()
                        )
                    })?;
                    Ok(BackingDeletionScope::Target)
                })
                .collect::<Vec<Result<_>>>())
        })
        .await
        .context("failed to verify redirection unmounts")?;
    Ok(verified
        .into_iter()
        .map(|result| match result {
            Ok(scope) => scope,
            Err(error) => {
                warning_handler(RemoveCheckoutWarning::RedirectionUnmountFailed {
                    path: path.to_path_buf(),
                    error: format!("{error:#}"),
                });
                BackingDeletionScope::Contents
            }
        })
        .collect())
}

/// Unmount each redirection within one shared deadline and return how much of each one's
/// backing can then be deleted. Failures are reported as warnings.
async fn unmount_each_redirection<'a, T, Unmount, UnmountFuture, WarningHandler>(
    path: &Path,
    timeout: Duration,
    start: Instant,
    redirections: &'a [T],
    unmount: Unmount,
    warning_handler: &mut WarningHandler,
) -> Vec<BackingDeletionScope>
where
    Unmount: Fn(&'a T) -> UnmountFuture,
    UnmountFuture: Future<Output = Result<()>>,
    WarningHandler: FnMut(RemoveCheckoutWarning),
{
    let mut timed_out = false;
    let mut scopes = Vec::with_capacity(redirections.len());
    for redirection in redirections {
        let remaining = timeout.saturating_sub(start.elapsed());
        let unmounted = if remaining.is_zero() {
            None
        } else {
            tokio::time::timeout(remaining, unmount(redirection))
                .await
                .ok()
        };
        scopes.push(match unmounted {
            Some(Ok(())) => BackingDeletionScope::Target,
            Some(Err(error)) => {
                warning_handler(RemoveCheckoutWarning::RedirectionUnmountFailed {
                    path: path.to_path_buf(),
                    error: format!("{error:#}"),
                });
                BackingDeletionScope::Contents
            }
            None => {
                timed_out = true;
                BackingDeletionScope::Contents
            }
        });
    }
    if timed_out {
        warning_handler(RemoveCheckoutWarning::RedirectionUnmountTimedOut {
            path: path.to_path_buf(),
            timeout,
        });
    }
    scopes
}

async fn execute_backing_cleanup_actions(
    deletions: Vec<(RedirectionBackingCleanupAction, BackingDeletionScope)>,
    event_handler: &mut impl FnMut(RemoveCheckoutEvent),
) -> Result<()> {
    execute_backing_cleanup_actions_with(
        deletions,
        |(action, _)| action.target().to_path_buf(),
        |(action, scope)| {
            match scope {
                BackingDeletionScope::Target => action.execute(),
                BackingDeletionScope::Contents => action.execute_contents(),
            }
            .map_err(Into::into)
        },
        event_handler,
    )
    .await
}

async fn execute_backing_cleanup_actions_with<Action, Target, Execute>(
    actions: Vec<Action>,
    target: Target,
    execute: Execute,
    event_handler: &mut impl FnMut(RemoveCheckoutEvent),
) -> Result<()>
where
    Action: Send + 'static,
    Target: Fn(&Action) -> PathBuf,
    Execute: Fn(Action) -> Result<()> + Clone + Send + Sync + 'static,
{
    let mut failures = Vec::new();
    for action in actions {
        let action_target = target(&action);
        event_handler(RemoveCheckoutEvent::DeletingBackingStorage {
            path: action_target.clone(),
        });
        let execute = execute.clone();
        let span =
            tracing::debug_span!("delete_redirection_backing", path = %action_target.display());
        let result = tokio::task::spawn_blocking(move || span.in_scope(|| execute(action)))
            .await
            .context("backing cleanup worker failed")
            .and_then(|result| result)
            .with_context(|| {
                format!(
                    "failed to delete redirection backing target {}",
                    action_target.display()
                )
            });
        if let Err(error) = result {
            failures.push(format!("{}: {error:#}", action_target.display()));
        }
    }

    if !failures.is_empty() {
        return Err(anyhow!(
            "failed to delete {} redirection backing target(s):\n{}",
            failures.len(),
            failures.join("\n")
        ));
    }

    Ok(())
}

async fn remove_inactive_checkout(
    instance: &EdenFsInstance,
    path: &Path,
    client_name: &str,
    options: RemoveCheckoutOptions,
    event_handler: &mut impl FnMut(RemoveCheckoutEvent),
) -> Result<()> {
    event_handler(RemoveCheckoutEvent::DeletingCheckoutState);
    remove_client_config_dir(instance, path, client_name)?;
    instance
        .remove_path_from_directory_map(path)
        .with_context(|| format!("failed to remove {} from config json file", path.display()))?;

    if !options.preserve_mount_point {
        clean_mount_point(path)?;
    }
    validate_removal_completion(instance, path, options.preserve_mount_point).await
}

#[tracing::instrument(level = "debug", skip_all, fields(path = %path.display()))]
fn remove_client_config_dir(
    instance: &EdenFsInstance,
    path: &Path,
    client_name: &str,
) -> Result<()> {
    let client_dir = instance.config_directory(client_name);
    match remove_client_dir(&client_dir) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(anyhow!(
            "failed to remove client config directory for {}: {error}",
            path.display()
        )),
    }
}

fn remove_client_dir(client_dir: &Path) -> std::io::Result<()> {
    // EdenFS does not mount a client directory that has no SNAPSHOT.
    // edenfsctl reads config.toml to remove a partly deleted checkout.
    fsutil::remove_file(&client_dir.join("SNAPSHOT"))?;
    for entry in fs::read_dir(client_dir)? {
        let entry = entry?;
        if entry.file_name() == "config.toml" {
            continue;
        }
        if entry.file_type()?.is_dir() {
            forcefully_remove_dir_all(&entry.path())?;
        } else {
            fs::remove_file(entry.path())?;
        }
    }
    fsutil::remove_file(&client_dir.join("config.toml"))?;
    fs::remove_dir(client_dir)
}

/// Remove a directory using the same permission and retry handling as checkout removal.
/// A directory that is already gone counts as removed.
#[tracing::instrument(level = "debug", skip_all, fields(path = %path.display()))]
pub fn clean_mount_point(path: &Path) -> Result<()> {
    #[cfg(unix)]
    match std::fs::set_permissions(path, Permissions::from_mode(0o755)) {
        Err(error) if error.kind() == ErrorKind::NotFound => {
            debug!(path = %path.display(), "Mount point is already gone");
            return Ok(());
        }
        result => result
            .with_context(|| format!("failed to set permission 755 for path {}", path.display()))?,
    }

    forcefully_remove_dir_all(path)
        .with_context(|| format!("failed to remove mount point {}", path.display()))
}

/// What a directory is to EdenFS, decided from checkout-local metadata and the
/// directory map without talking to the daemon.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocalCheckoutState {
    /// The directory carries live checkout metadata. On Windows that metadata outlives
    /// an unmount, so removal still has to find the checkout in the directory map.
    Active,
    /// Registered in the directory map but not mounted.
    Inactive,
    /// Unregistered and not recognizably a checkout.
    Unknown,
}

/// Classify `path` before removing it. Fails if `path` lies inside a mounted checkout
/// rather than being its root.
pub async fn classify_checkout(
    instance: &EdenFsInstance,
    path: &Path,
) -> Result<LocalCheckoutState> {
    let path = canonicalize_checkout_path(path).await?;
    // A nested checkout's root is itself an active mount, so test the path before its ancestors.
    if is_active_eden_mount(&path) {
        return Ok(LocalCheckoutState::Active);
    }
    if let Some(root) = path
        .ancestors()
        .skip(1)
        .find(|ancestor| is_active_eden_mount(ancestor))
    {
        return Err(anyhow!(
            "{} is not the root of checkout {}, not removing",
            path.display(),
            root.display()
        ));
    }
    if registered_client(instance, &path)?.is_some() {
        return Ok(LocalCheckoutState::Inactive);
    }
    Ok(LocalCheckoutState::Unknown)
}

async fn validate_removal_completion(
    instance: &EdenFsInstance,
    path: &Path,
    preserve_mount_point: bool,
) -> Result<()> {
    if registered_client(instance, path)?.is_some() {
        return Err(anyhow!("repo {} is still registered", path.display()));
    }

    if preserve_mount_point {
        return Ok(());
    }

    fail_point!("remove:validate", |_| {
        Err(anyhow!("failpoint: expected failure"))
    });

    match path.try_exists() {
        Ok(false) => Ok(()),
        Ok(true) => Err(anyhow!(
            "directory left by repo {} is not removed",
            path.display()
        )),
        Err(error) => Err(anyhow!(
            "failed to check the status of path {}: {error}",
            path.display()
        )),
    }
}

/// Return whether a path is still registered in the EdenFS directory map.
pub async fn path_in_eden_config(instance: &EdenFsInstance, path: &Path) -> Result<bool> {
    let mut mounts = get_mounts(instance)
        .await
        .context("failed to call eden list")?;
    let entry_key = dunce::simplified(path);
    mounts.retain(|mount_path, _| dunce::simplified(mount_path) == entry_key);
    Ok(!mounts.is_empty())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::sync::Arc;

    use edenfs_core::daemon::DisconnectedDaemon;
    use tempfile::tempdir;

    use super::*;

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn batch_unmount_cleans_paths_without_following_symlinks() {
        use edenfs_core::redirect::RedirectionState;
        use edenfs_core::redirect::RedirectionType;

        let temp = tempdir().unwrap();
        let config_dir = temp.path().join("eden");
        let client_dir = config_dir.join("clients/test");
        let path = temp.path().join("checkout");
        fs::create_dir_all(&client_dir).unwrap();
        fs::create_dir(&path).unwrap();
        fs::write(
            config_dir.join("config.json"),
            serde_json::to_vec(&BTreeMap::from([(&path, "test")])).unwrap(),
        )
        .unwrap();
        fs::write(
            client_dir.join("config.toml"),
            "[repository]\npath = '/tmp'\ntype = 'hg'\n[redirections]\n",
        )
        .unwrap();
        let instance = EdenFsInstance::with_daemon(
            config_dir.clone(),
            config_dir,
            None,
            Arc::new(DisconnectedDaemon),
        );
        let checkout = find_checkout(&instance, &path).unwrap();
        let target = temp.path().join("backing");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("data"), "keep").unwrap();
        let redirections: Vec<_> = (0..64)
            .map(|index| {
                let repo_path = PathBuf::from(format!("redir-{index}"));
                let full_path = path.join(&repo_path);
                match index % 4 {
                    0 => fs::create_dir(&full_path).unwrap(),
                    1 => std::os::unix::fs::symlink(&target, &full_path).unwrap(),
                    2 => {
                        fs::create_dir(&full_path).unwrap();
                        fs::write(full_path.join("local"), "keep").unwrap();
                    }
                    _ => fs::write(&full_path, "keep").unwrap(),
                }
                RedirectionCleanup {
                    redirection: Redirection {
                        repo_path,
                        redir_type: RedirectionType::Bind,
                        source: String::new(),
                        state: RedirectionState::NotMounted,
                        target: None,
                    },
                    actions: Vec::new(),
                }
            })
            .collect();
        let mut warnings = Vec::new();
        let scopes = unmount_managed_redirections(
            &instance,
            &path,
            &checkout,
            &redirections,
            false,
            Duration::from_secs(10),
            &mut |warning| warnings.push(warning),
        )
        .await
        .unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(scopes, vec![BackingDeletionScope::Target; 64]);
        for index in 0..64 {
            let full_path = path.join(format!("redir-{index}"));
            match index % 4 {
                0 | 1 => assert!(!fs::symlink_metadata(full_path).is_ok()),
                2 => assert_eq!(fs::read_to_string(full_path.join("local")).unwrap(), "keep"),
                _ => assert_eq!(fs::read_to_string(full_path).unwrap(), "keep"),
            }
        }
        assert_eq!(fs::read_to_string(target.join("data")).unwrap(), "keep");
    }

    #[tokio::test]
    async fn unmount_batch_keeps_successes_before_shared_deadline() {
        let mut warnings = Vec::new();
        let attempts = std::cell::Cell::new(0);
        let scopes = unmount_each_redirection(
            Path::new("/checkout"),
            Duration::from_millis(20),
            Instant::now(),
            &[0, 1, 2],
            |index| {
                attempts.set(attempts.get() + 1);
                async move {
                    if *index == 1 {
                        std::future::pending::<()>().await;
                    }
                    Ok(())
                }
            },
            &mut |warning| warnings.push(warning),
        )
        .await;
        assert_eq!(attempts.get(), 2);
        assert_eq!(
            scopes,
            [
                BackingDeletionScope::Target,
                BackingDeletionScope::Contents,
                BackingDeletionScope::Contents
            ]
        );
        assert!(matches!(
            warnings.as_slice(),
            [RemoveCheckoutWarning::RedirectionUnmountTimedOut { .. }]
        ));
    }

    #[tokio::test]
    async fn unmount_failure_limits_backing_deletion_to_contents() {
        let mut warnings = Vec::new();

        let scopes = unmount_each_redirection(
            Path::new("/checkout"),
            Duration::from_secs(10),
            Instant::now(),
            &["buck-out", "stuck", "other"],
            |repo_path| async move {
                if *repo_path == "stuck" {
                    Err(anyhow!("device busy"))
                } else {
                    Ok(())
                }
            },
            &mut |warning| warnings.push(warning),
        )
        .await;

        assert_eq!(
            scopes,
            [
                BackingDeletionScope::Target,
                BackingDeletionScope::Contents,
                BackingDeletionScope::Target
            ],
            "only the redirection that stayed mounted keeps its backing target"
        );
        assert_eq!(
            warnings,
            vec![RemoveCheckoutWarning::RedirectionUnmountFailed {
                path: PathBuf::from("/checkout"),
                error: "device busy".to_owned(),
            }]
        );
    }

    #[tokio::test]
    async fn classify_checkout_tells_registered_and_unknown_directories_apart() {
        let temp_dir = tempdir().expect("temp directory should be created");
        let config_dir = temp_dir.path().join("eden");
        let inactive = temp_dir.path().join("inactive");
        let unknown = temp_dir.path().join("unknown");
        for dir in [&config_dir, &inactive, &unknown] {
            fs::create_dir_all(dir).expect("directory should be created");
        }
        fs::write(unknown.join("notes.txt"), "keep").expect("notes should be written");
        fs::write(
            config_dir.join("config.json"),
            serde_json::to_vec(&BTreeMap::from([(
                inactive.canonicalize().expect("inactive path"),
                "inactive-client",
            )]))
            .expect("directory map should serialize"),
        )
        .expect("directory map should be written");
        let instance = EdenFsInstance::with_daemon(
            config_dir.clone(),
            config_dir.join("etc"),
            None,
            Arc::new(DisconnectedDaemon),
        );

        assert_eq!(
            classify_checkout(&instance, &inactive)
                .await
                .expect("inactive lookup"),
            LocalCheckoutState::Inactive
        );
        assert_eq!(
            classify_checkout(&instance, &unknown)
                .await
                .expect("unknown lookup"),
            LocalCheckoutState::Unknown
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn classify_checkout_rejects_a_directory_inside_a_mounted_checkout() {
        let temp_dir = tempdir().expect("temp directory should be created");
        let root = temp_dir.path().canonicalize().expect("temp directory path");
        let config_dir = root.join("eden");
        let checkout = root.join("checkout");
        let inside = checkout.join("inside");
        let metadata = checkout.join(".eden");
        let nested = checkout.join("nested");
        let nested_metadata = nested.join(".eden");
        for dir in [&config_dir, &inside, &metadata, &nested_metadata] {
            fs::create_dir_all(dir).expect("directory should be created");
        }
        std::os::unix::fs::symlink(&checkout, metadata.join("root")).expect("root pointer");
        std::os::unix::fs::symlink(&nested, nested_metadata.join("root"))
            .expect("nested root pointer");
        fs::write(config_dir.join("config.json"), b"{}").expect("directory map should be written");
        let instance = EdenFsInstance::with_daemon(
            config_dir.clone(),
            config_dir.join("etc"),
            None,
            Arc::new(DisconnectedDaemon),
        );

        assert_eq!(
            classify_checkout(&instance, &checkout)
                .await
                .expect("checkout lookup"),
            LocalCheckoutState::Active
        );
        assert_eq!(
            classify_checkout(&instance, &nested)
                .await
                .expect("nested checkout lookup"),
            LocalCheckoutState::Active,
            "a nested checkout root is a mount, not a directory inside its parent"
        );
        let error = classify_checkout(&instance, &inside)
            .await
            .expect_err("a directory inside a mounted checkout must be rejected");
        assert!(
            error.to_string().contains("is not the root of checkout"),
            "{error:#}"
        );
    }

    #[tokio::test]
    async fn rejects_descendant_of_registered_checkout() {
        let temp_dir = tempdir().expect("temp directory should be created");
        let config_dir = temp_dir.path().join("eden");
        let checkout = temp_dir.path().join("checkout");
        let descendant = checkout.join("subdir");
        let client_dir = config_dir.join("clients").join("test-client");
        fs::create_dir_all(&descendant).expect("checkout contents should be created");
        fs::create_dir_all(&client_dir).expect("client directory should be created");

        let configured_mounts = BTreeMap::from([(
            checkout.to_string_lossy().into_owned(),
            "test-client".to_owned(),
        )]);
        fs::write(
            config_dir.join("config.json"),
            serde_json::to_vec(&configured_mounts).expect("directory map should serialize"),
        )
        .expect("directory map should be written");

        let instance = EdenFsInstance::with_daemon(
            config_dir.clone(),
            config_dir.join("etc"),
            None,
            Arc::new(DisconnectedDaemon),
        );
        let error = remove_checkout(&instance, &descendant, RemoveCheckoutOptions::default())
            .await
            .expect_err("a checkout descendant must not be removed");

        assert!(
            error
                .to_string()
                .contains("not a registered EdenFS checkout"),
            "unexpected error: {error:#}"
        );
        assert!(descendant.exists(), "checkout contents must remain intact");
        assert!(client_dir.exists(), "client state must remain intact");
        assert_eq!(
            instance
                .get_configured_mounts_map()
                .expect("directory map should remain readable"),
            configured_mounts
                .into_iter()
                .map(|(path, client)| (path.into(), client))
                .collect()
        );
    }

    #[tokio::test]
    async fn reports_redirection_unmount_failure() {
        let mut warnings = Vec::new();

        unmount_redirections_with_timeout(
            Path::new("/checkout"),
            Duration::from_secs(10),
            async { Err(anyhow!("device busy").context("failed to unmount buck-out")) },
            &mut |warning| warnings.push(warning),
        )
        .await;

        assert_eq!(warnings.len(), 1, "the caller must receive the failure");
        assert!(
            warnings[0]
                .to_string()
                .contains("failed to unmount buck-out: device busy"),
            "the warning must retain the underlying cause"
        );
    }

    #[tokio::test]
    async fn removal_ignores_other_checkout_configs() {
        let temp_dir = tempdir().expect("temporary directory");
        let root = temp_dir
            .path()
            .canonicalize()
            .expect("temporary directory path");
        let config_dir = root.join("eden");
        let checkout = root.join("checkout");
        let deleted_checkout = root.join("deleted-checkout");
        let other_checkout = root.join("other-checkout");
        let client_dir = config_dir.join("clients/test-client");
        let deleted_client_dir = config_dir.join("clients/deleted-client");
        let other_client_dir = config_dir.join("clients/other-client");
        for dir in [
            &checkout,
            &deleted_checkout,
            &other_checkout,
            &client_dir,
            &deleted_client_dir,
            &other_client_dir,
        ] {
            fs::create_dir_all(dir).expect("directory should be created");
        }
        fs::write(checkout.join("keep"), "checkout contents").expect("checkout contents");
        fs::write(deleted_checkout.join("gone"), "deleted contents").expect("deleted contents");
        let other_config = other_client_dir.join("config.toml");
        fs::write(&other_config, "[repository").expect("malformed checkout config");
        let mounts = BTreeMap::from([
            (checkout.clone(), "test-client".to_owned()),
            (deleted_checkout.clone(), "deleted-client".to_owned()),
            (other_checkout.clone(), "other-client".to_owned()),
        ]);
        fs::write(
            config_dir.join("config.json"),
            serde_json::to_vec(&mounts).expect("directory map"),
        )
        .expect("write directory map");
        let instance = EdenFsInstance::with_daemon(
            config_dir.clone(),
            config_dir,
            None,
            Arc::new(DisconnectedDaemon),
        );

        let error = validate_removal_completion(&instance, &checkout, true)
            .await
            .expect_err("preserving the mount point must still check registration");
        assert!(error.to_string().contains("still registered"), "{error:#}");

        remove_checkout(
            &instance,
            &checkout,
            RemoveCheckoutOptions {
                preserve_mount_point: true,
                ..RemoveCheckoutOptions::default()
            },
        )
        .await
        .expect("another checkout's config must not fail preserved removal");
        remove_checkout(
            &instance,
            &deleted_checkout,
            RemoveCheckoutOptions::default(),
        )
        .await
        .expect("another checkout's config must not fail ordinary removal");

        assert_eq!(
            fs::read_to_string(checkout.join("keep")).unwrap(),
            "checkout contents"
        );
        assert!(
            !client_dir.exists(),
            "removed checkout's client state must be gone"
        );
        assert!(
            !deleted_checkout.exists(),
            "ordinary removal must delete the mount point"
        );
        assert!(
            !deleted_client_dir.exists(),
            "ordinary removal must delete the client state"
        );
        assert_eq!(fs::read_to_string(other_config).unwrap(), "[repository");
        assert_eq!(
            instance.get_configured_mounts_map().expect("directory map"),
            BTreeMap::from([(other_checkout, "other-client".to_owned())])
        );
    }

    #[cfg(unix)]
    #[test]
    fn retry_removes_client_dir_after_failed_removal() {
        let temp_dir = tempdir().expect("temporary directory");
        let config_dir = temp_dir.path().join("eden");
        let client_dir = config_dir.join("clients/test");
        let locked_dir = client_dir.join("locked");
        fs::create_dir_all(&locked_dir).expect("locked directory");
        fs::write(client_dir.join("config.toml"), b"").expect("client config");
        fs::write(client_dir.join("SNAPSHOT"), b"").expect("snapshot");
        fs::write(locked_dir.join("file"), b"").expect("locked file");
        fs::set_permissions(&locked_dir, Permissions::from_mode(0o500)).expect("lock directory");
        let instance = EdenFsInstance::with_daemon(
            config_dir.clone(),
            config_dir,
            None,
            Arc::new(DisconnectedDaemon),
        );

        let result = remove_client_config_dir(&instance, temp_dir.path(), "test");

        fs::set_permissions(&locked_dir, Permissions::from_mode(0o700)).expect("unlock directory");
        assert!(result.is_err());
        assert!(
            !client_dir.join("SNAPSHOT").exists(),
            "SNAPSHOT is deleted first"
        );
        assert!(
            client_dir.join("config.toml").exists(),
            "config.toml is deleted last"
        );

        remove_client_config_dir(&instance, temp_dir.path(), "test")
            .expect("a retry removes the rest of the client directory");
        assert!(!client_dir.exists());
    }

    #[test]
    fn client_dir_without_client_config_is_removed() {
        let temp_dir = tempdir().expect("temporary directory");
        let config_dir = temp_dir.path().join("eden");
        let client_dir = config_dir.join("clients/test");
        fs::create_dir_all(client_dir.join("local")).expect("client directory");
        let instance = EdenFsInstance::with_daemon(
            config_dir.clone(),
            config_dir,
            None,
            Arc::new(DisconnectedDaemon),
        );

        remove_client_config_dir(&instance, temp_dir.path(), "test")
            .expect("a missing config.toml must not stop the removal");

        assert!(!client_dir.exists());
    }

    #[tokio::test]
    async fn delete_managed_unregisters_checkout_with_missing_client_config() {
        let temp_dir = tempdir().expect("temporary directory");
        let config_dir = temp_dir.path().join("eden");
        let checkout = temp_dir.path().join("checkout");
        fs::create_dir_all(&config_dir).expect("config directory");
        fs::create_dir_all(&checkout).expect("checkout directory");
        let checkout = checkout.canonicalize().expect("checkout path");
        fs::write(
            config_dir.join("config.json"),
            serde_json::to_vec(&BTreeMap::from([(&checkout, "missing-client")]))
                .expect("directory map"),
        )
        .expect("write directory map");
        let instance = EdenFsInstance::with_daemon(
            config_dir.clone(),
            config_dir,
            None,
            Arc::new(DisconnectedDaemon),
        );
        let mut warnings = Vec::new();

        remove_checkout_with_warning_handler(
            &instance,
            &checkout,
            RemoveCheckoutOptions {
                backing_cleanup: BackingCleanupPolicy::DeleteManaged,
                ..RemoveCheckoutOptions::default()
            },
            |warning| warnings.push(warning),
        )
        .await
        .expect("missing client state must not block unregistering the checkout");

        assert_eq!(
            warnings,
            vec![RemoveCheckoutWarning::BackingCleanupSkipped {
                path: checkout.clone()
            }]
        );
        assert!(!checkout.exists());
        assert!(
            instance
                .get_configured_mounts_map()
                .expect("directory map")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn removes_registered_checkout_whose_folder_is_gone() {
        let temp_dir = tempdir().expect("temporary directory");
        let config_dir = temp_dir.path().join("eden");
        let client_dir = config_dir.join("clients/test");
        fs::create_dir_all(&client_dir).expect("client directory");
        fs::write(client_dir.join("config.toml"), b"").expect("client config");
        // The folder's parent is gone too.
        let checkout = temp_dir
            .path()
            .canonicalize()
            .expect("temporary directory path")
            .join("worktrees")
            .join("checkout");
        fs::write(
            config_dir.join("config.json"),
            serde_json::to_vec(&BTreeMap::from([(&checkout, "test")])).expect("directory map"),
        )
        .expect("write directory map");
        let instance = EdenFsInstance::with_daemon(
            config_dir.clone(),
            config_dir,
            None,
            Arc::new(DisconnectedDaemon),
        );

        assert_eq!(
            classify_checkout(&instance, &checkout)
                .await
                .expect("classify checkout"),
            LocalCheckoutState::Inactive
        );
        assert_eq!(
            classify_checkout(&instance, &checkout.with_file_name("unregistered"))
                .await
                .expect("classify missing folder"),
            LocalCheckoutState::Unknown,
            "a missing folder that isn't registered is not a checkout"
        );
        remove_checkout(&instance, &checkout, RemoveCheckoutOptions::default())
            .await
            .expect("a registered checkout whose folder is gone must be removable");

        assert!(!client_dir.exists());
        assert!(
            instance
                .get_configured_mounts_map()
                .expect("directory map")
                .is_empty()
        );
    }
}
