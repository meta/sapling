/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use anyhow::bail;
use clidispatch::TermLogger;
use cmdutil::ConfigExt;
use cmdutil::Result;
use edenfs_core::daemon::SocketDaemon;
use edenfs_core::instance::EdenFsInstance;
use edenfs_core::utils::get_config_dir;
use edenfs_core::utils::get_etc_eden_dir;
use edenfs_core::utils::get_home_dir;
use edenfs_remove::BackingCleanupPolicy;
use edenfs_remove::LocalCheckoutState;
use edenfs_remove::RemoveCheckoutEvent;
use edenfs_remove::RemoveCheckoutOptions;
use repo::repo::Repo;

/// Remove one EdenFS checkout. Returns `true` after removing its EdenFS state, even
/// if its folder was already gone, or `false` if the path is missing and unregistered.
pub(super) fn run_direct_eden_remove(
    logger: &TermLogger,
    repo: &Repo,
    path: &Path,
) -> Result<bool> {
    // `edenfs.basepath` lets tests point at a non-default EdenFS deployment;
    // the subprocess path honors it the same way (see `build_eden_command_type`).
    let base_path = repo.config().get_opt::<PathBuf>("edenfs", "basepath")?;
    let (config_dir, etc_eden_dir, home_dir) = match base_path {
        Some(base_path) => (
            base_path.join("eden"),
            base_path.join("etc_eden"),
            Some(base_path.join("home")),
        ),
        None => (
            eden_config_dir(repo, path)?,
            get_etc_eden_dir(&None),
            get_home_dir(&None),
        ),
    };
    let daemon = Arc::new(SocketDaemon::new(config_dir.join("socket")));
    let instance = EdenFsInstance::with_daemon(config_dir, etc_eden_dir, home_dir, daemon);
    let options = RemoveCheckoutOptions {
        auxiliary_process_timeout: edenfs_remove::auxiliary_process_timeout(None),
        backing_cleanup: BackingCleanupPolicy::DeleteManaged,
        ..Default::default()
    };
    let state = async_runtime::block_on(edenfs_remove::classify_checkout(&instance, path))
        .with_context(|| format!("failed to inspect EdenFS checkout {}", path.display()))?;

    match state {
        LocalCheckoutState::Active | LocalCheckoutState::Inactive => async_runtime::block_on(
            edenfs_remove::remove_checkout_with_event_handler(&instance, path, options, |event| {
                match event {
                    RemoveCheckoutEvent::UnmountingRedirections => {
                        logger.info("Unmounting redirections...");
                    }
                    RemoveCheckoutEvent::DeletingBackingStorage { path } => {
                        logger.info(format!(
                            "Deleting redirection backing storage: {}...",
                            path.display()
                        ));
                    }
                    RemoveCheckoutEvent::UnmountingCheckout => {
                        logger.info("Unmounting checkout...");
                    }
                    RemoveCheckoutEvent::DeletingCheckoutState => {
                        logger.info("Deleting EdenFS checkout state...");
                    }
                    RemoveCheckoutEvent::Warning(warning) => logger.warn(warning.to_string()),
                }
                logger.flush();
            }),
        )
        .with_context(|| format!("direct EdenFS removal failed for {}", path.display()))?,
        LocalCheckoutState::Unknown if !path.exists() => return Ok(false),
        LocalCheckoutState::Unknown => bail!(
            "refusing to delete {}: it is not a registered EdenFS checkout",
            path.display()
        ),
    }
    Ok(true)
}

// Checkout-local metadata names the client directory on every platform, whereas
// `get_config_dir` only knows the Unix `.eden/socket` symlink and otherwise falls
// back to the current directory or the default location. A checkout without
// metadata (unmounted on Unix, or with its folder gone) uses the metadata of the
// repo's own checkout, which the same EdenFS serves.
fn eden_config_dir(repo: &Repo, path: &Path) -> Result<PathBuf> {
    let config_dir = [path, repo.path()].into_iter().find_map(|checkout| {
        let client_dir = edenfs_client::get_client_dir(checkout).ok()?;
        Some(client_dir.parent()?.parent()?.to_path_buf())
    });
    match config_dir {
        Some(config_dir) => Ok(config_dir),
        None => Ok(get_config_dir(&None, &Some(path.to_path_buf()))?),
    }
}
