/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! edenfsctl handles

use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;
use clap::Parser;
use edenfs_client::fsutil::find_resource_locks;
use edenfs_client::fsutil::get_process_tree;
use sysinfo::Pid;
use sysinfo::ProcessRefreshKind;
use sysinfo::RefreshKind;
use sysinfo::System;

use crate::ExitCode;

#[derive(Debug, Parser)]
#[clap(about = "List processes keeping a handle to a resource (such as an Eden mount)")]
pub struct HandlesCmd {
    #[clap(long, help = "The EdenFS mount point path.")]
    mount: PathBuf,
    #[clap(long, help = "Kill processes holding handles to the mount.")]
    kill: bool,
}

impl HandlesCmd {
    fn list(&self, mount: PathBuf, kill: bool) -> Result<ExitCode> {
        let Ok(entries) = find_resource_locks(&mount) else {
            return Ok(1);
        };
        if entries.is_empty() {
            println!(
                "No resource locks found. Typical reasons: 1) There's actually no processes holding a lock on this mount; 2) The processes are run by a different user. 3) handle.exe failed to return data, and retrying may help."
            );
            return Ok(0);
        }
        let my_ancestors = if kill {
            get_process_tree()
        } else {
            HashSet::new()
        };

        // We only want to initialize sys if we're going to kill processes, because getting the process tree is expensive.
        let sys: Option<System> = if kill {
            Some(System::new_with_specifics(
                RefreshKind::nothing().with_processes(ProcessRefreshKind::everything()),
            )?)
        } else {
            None
        };

        println!("Processes holding handles to the mount or its children:");
        let mut seen = HashSet::new();
        for entry in entries {
            if !seen.insert(entry.process_id.clone()) {
                continue;
            }
            println!("{} ({})", entry.process_name, entry.process_id);
            let Ok(pid) = entry.process_id.parse::<u32>() else {
                println!("  (Invalid PID string)");
                continue;
            };
            if kill {
                if my_ancestors.contains(&pid) {
                    println!("  (this process is an ancestor of this command, so I won't kill it)");
                    continue;
                }
                // TODO: We probably want to skip some processes by name (eden, watchman)?
                // Unwrap here OK since we know that for kill, we have Some
                if let Some(process) = sys.as_ref().unwrap().process(Pid::from_u32(pid)) {
                    let res = process.kill();
                    println!("  (kill signal sent, result = {res})");
                }
            }
        }
        Ok(0)
    }
}

#[async_trait]
impl crate::Subcommand for HandlesCmd {
    async fn run(&self) -> Result<ExitCode> {
        self.list(self.mount.to_owned(), self.kill)
    }
}
