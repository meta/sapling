/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! edenfsctl debug stress
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use async_trait::async_trait;
use clap::Parser;
use edenfs_client::attributes::AttributesRequestScope;
use edenfs_client::attributes::GetAttributesV2Request;
use edenfs_client::attributes::SourceControlType;
use edenfs_client::attributes::SourceControlTypeOrError;
use edenfs_client::checkout::find_checkout;
use edenfs_client::request_factory::RequestFactory;
use edenfs_client::request_factory::send_requests;
use edenfs_client::types::FileAttributes;
use edenfs_client::utils::expand_path_or_cwd;

use crate::ExitCode;
use crate::get_edenfs_instance;

#[derive(Parser, Debug)]
pub struct CommonOptions {
    #[clap(long, value_parser = |s: &str| -> Result<PathBuf, anyhow::Error> { expand_path_or_cwd(s) }, default_value = "")]
    /// Path to the mount point
    mount_point: PathBuf,

    #[clap(short, long, default_value = "10")]
    /// Number of tasks to use for sending requests
    num_tasks: usize,
}

#[derive(Parser, Debug)]
#[clap(about = "Stress tests an EdenFS daemon by issuing a large number of thrift requests")]
pub enum StressCmd {
    #[clap(about = "Stress the getAttributesFromFilesV2 endpoint")]
    GetAttributesV2 {
        #[clap(flatten)]
        common: CommonOptions,

        #[clap(
            index = 1,
            required = true,
            help = "Glob pattern to enumerate the list of files for which we'll query attributes"
        )]
        glob_pattern: String,

        #[clap(
            long,
            default_value = "1000",
            help = "Number of requests to send to the Thrift server"
        )]
        num_requests: usize,

        #[clap(
            long,
            use_value_delimiter = true,
            help = "Attributes to query for each file"
        )]
        attributes: Option<Vec<FileAttributes>>,

        #[clap(
            long,
            help = "Indicates whether requests should include results for files, trees, or both."
        )]
        scope: Option<AttributesRequestScope>,
    },

    #[clap(about = "Stress the readdir endpoint by issuing a recursive readdir request")]
    RecursiveReaddir {
        #[clap(flatten)]
        common: CommonOptions,

        #[clap(
            index = 1,
            required = true,
            help = "directory to recursively call readdir on"
        )]
        root_dir: PathBuf,

        #[clap(
            long,
            use_value_delimiter = true,
            help = "Attributes to query with each readdir request"
        )]
        attributes: Option<Vec<FileAttributes>>,

        #[clap(long, help = "Print attributes for every file and directory")]
        print_all_attributes: bool,
    },
}

#[async_trait]
impl crate::Subcommand for StressCmd {
    async fn run(&self) -> Result<ExitCode> {
        let instance = get_edenfs_instance();
        let client = instance.get_client();

        match self {
            Self::GetAttributesV2 {
                common,
                glob_pattern,
                num_requests,
                attributes,
                scope,
            } => {
                // Resolve the glob that the user specified since getAttributesFromFilesV2 takes a
                // list of files, not a glob pattern.
                let checkout = find_checkout(instance, &common.mount_point).with_context(|| {
                    anyhow!(
                        "Failed to find checkout with path {}",
                        common.mount_point.display()
                    )
                })?;
                let glob_result = client
                    .glob_files_foreground(&checkout.path(), vec![glob_pattern.to_string()])
                    .await?;

                // Construct a RequestFactory so that we can issue the requests efficiently
                let paths: Vec<String> = glob_result
                    .matching_files
                    .iter()
                    .map(|p| String::from_utf8_lossy(p).into())
                    .collect();
                let attributes = attributes
                    .clone()
                    .unwrap_or(FileAttributes::all_attributes());
                let request_factory = Arc::new(GetAttributesV2Request::new(
                    checkout.path(),
                    &paths,
                    attributes.as_slice(),
                    scope.clone(),
                ));

                // Issue the requests and bail early if any of them fail
                let request_name = request_factory.request_name();
                let num_requests = *num_requests;
                let num_tasks = common.num_tasks;
                send_requests(client, request_factory, num_requests, num_tasks)
                    .await
                    .with_context(|| {
                        anyhow!(
                            "failed to complete {num_requests} {request_name} requests across {num_tasks} tasks"
                        )
                    })?;
                println!(
                    "Successfully issued {num_requests} {request_name} requests across {num_tasks} tasks"
                );
                Ok(0)
            }
            Self::RecursiveReaddir {
                common,
                root_dir,
                attributes,
                print_all_attributes,
            } => {
                let checkout = find_checkout(instance, &common.mount_point).with_context(|| {
                    anyhow!(
                        "Failed to find checkout with path {}",
                        common.mount_point.display()
                    )
                })?;
                let attributes = attributes
                    .clone()
                    .unwrap_or(FileAttributes::all_attributes());
                let readdir_results = client
                    .recursive_readdir(
                        &checkout.path(),
                        root_dir,
                        attributes.as_slice(),
                        common.num_tasks,
                    )
                    .await?;

                let num_results = readdir_results.len();
                let (mut num_tree_results, mut num_file_results) = (0, 0);
                for (path, readdir_result) in readdir_results {
                    if *print_all_attributes {
                        println!("Success - {}: {:?}", path.display(), readdir_result);
                    }
                    match readdir_result.scm_type {
                        Some(SourceControlTypeOrError::SourceControlType(
                            SourceControlType::Tree,
                        )) => num_tree_results += 1,
                        Some(SourceControlTypeOrError::SourceControlType(_)) => {
                            num_file_results += 1
                        }
                        _ => continue,
                    }
                }
                println!(
                    "Total results: {num_results} ({num_tree_results} directories, {num_file_results} files)."
                );
                Ok(0)
            }
        }
    }
}
