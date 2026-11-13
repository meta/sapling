/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::collections::HashSet;
use std::fs::File;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use blobstore::Blobstore;
use blobstore::OverwriteStatus;
use blobstore::PutBehaviour;
use bookmarks::BookmarkKey;
use bookmarks::BookmarksRef;
use clap::ArgGroup;
use clap::Args;
use commit_id::parse_commit_id;
use content_manifest_derivation::RootContentManifestId;
use context::CoreContext;
use derivation_queue_thrift::DerivationPriority;
use futures::stream;
use futures::stream::StreamExt;
use futures::stream::TryStreamExt;
use manifest::Entry;
use manifest::ManifestOps;
use mononoke_app::MononokeApp;
use mononoke_app::args::RepoArgs;
use mononoke_app::args::RepoBlobstoreArgs;
use mononoke_types::BlobstoreKey;
use mononoke_types::BlobstoreValue;
use mononoke_types::ChangesetId;
use mononoke_types::NonRootMPath;
use mononoke_types::RedactionKeyList;
use mononoke_types::typed_hash::RedactionKeyListId;
use redaction_set::RedactionSets;
use repo_blobstore::RepoBlobstoreArc;
use repo_derived_data::RepoDerivedDataRef;
use tokio::io::AsyncReadExt;

use super::Repo;
use super::list::paths_for_content_keys;

const COMMIT_LOOKUP_CONCURRENCY: usize = 10;
const AWS_SYNC_CONCURRENCY: usize = 50;
const AWS_SYNC_MAX_PAYLOAD_BYTES: usize = 64 * 1024 * 1024;

#[derive(Args)]
#[clap(group(ArgGroup::new("files-input=file").args(&["files", "input_file"]).required(true)))]
pub struct RedactionCreateKeyListArgs {
    #[clap(flatten)]
    repo_args: RepoArgs,

    /// Commit containing all files to redact. If omitted, --input-file must
    /// contain one commit:path pair per line.
    #[clap(long, short = 'i')]
    commit_id: Option<String>,

    /// Fail if any of the content to be redacted is reachable from this main
    /// bookmark unless --force is set.
    #[clap(long, default_value = "master")]
    main_bookmark: BookmarkKey,

    /// Force content redaction even if content is reachable from the main
    /// bookmark.
    #[clap(long)]
    force: bool,

    /// Name of a file with a list of filenames to redact. Without --commit-id,
    /// each line must be a commit:path pair.
    #[clap(long)]
    input_file: Option<PathBuf>,

    /// Name of a file to write the new key to.
    #[clap(long)]
    output_file: Option<PathBuf>,

    /// Skip syncing the keylist to the AWS Mononoke instance.
    #[clap(long)]
    skip_aws_sync: bool,

    /// Files to redact
    #[clap(value_name = "FILE")]
    files: Vec<String>,
}

#[derive(Args)]
pub struct RedactionFetchKeyListArgs {
    #[clap(flatten)]
    repo_blobstore_args: RepoBlobstoreArgs,

    /// Redaction key list id, as obtained from `create-key-list` or `create-key-list-from-id`
    #[clap(value_name = "KEY LIST ID")]
    key_list_id: RedactionKeyListId,

    /// Name of a file to write the key list to.
    #[clap(long)]
    output_file: Option<PathBuf>,
}

#[derive(Args)]
pub struct RedactionSyncToAwsArgs {
    /// Upload this local monad binary into a temporary AWS Job. Intended for testing before deployment.
    #[clap(long)]
    bootstrap_monad: bool,
}

#[derive(Args)]
#[clap(group(
    ArgGroup::new("payload-source")
        .args(&["payload", "payload_stdin"])
        .required(true)
))]
pub struct RedactionSyncKeyListsFromJsonArgs {
    /// JSON-encoded payload supplied by the AWS sync orchestrator.
    #[clap(long)]
    payload: Option<String>,

    /// Read the JSON-encoded payload from stdin.
    #[clap(long)]
    payload_stdin: bool,
}

pub async fn fetch_key_list(
    ctx: &CoreContext,
    app: &MononokeApp,
    args: RedactionFetchKeyListArgs,
) -> Result<()> {
    let redaction_blobstore = app.redaction_config_blobstore().await?;
    let key_list = redaction::fetch_key_list(ctx, &redaction_blobstore, args.key_list_id).await?;
    if let Some(output_file) = args.output_file.as_deref() {
        let mut output = File::create(output_file).with_context(|| {
            format!(
                "Failed to open output file '{}'",
                output_file.to_string_lossy()
            )
        })?;
        for key in key_list.keys {
            output
                .write(format!("{key}\n").as_bytes())
                .with_context(|| {
                    format!(
                        "Failed to write to output file '{}'",
                        output_file.to_string_lossy()
                    )
                })?;
        }
    } else {
        for key in key_list.keys {
            println!("{key}");
        }
    }
    Ok(())
}

/// Key lists in the prod redaction config, each with whether every config
/// entry for it is enforced.
pub(super) fn redaction_config_key_lists(
    app: &MononokeApp,
) -> Result<HashMap<RedactionKeyListId, bool>> {
    let repo_configs = app.repo_configs();
    let config_path = &repo_configs
        .common
        .require_redaction_config()?
        .redaction_sets_location;
    let config: Arc<RedactionSets> = app
        .environment()
        .config_store
        .get_config_handle(config_path.clone())
        .with_context(|| format!("Redaction sets not found at {config_path}"))?
        .get();

    config
        .all_redactions
        .iter()
        .try_fold(HashMap::new(), |mut key_lists, redaction| {
            let id = RedactionKeyListId::from_str(&redaction.id).with_context(|| {
                format!(
                    "Invalid key list id in prod redaction config: {}",
                    redaction.id
                )
            })?;
            *key_lists.entry(id).or_insert(true) &= redaction.enforce;
            Ok(key_lists)
        })
}

fn active_redaction_key_list_ids(app: &MononokeApp) -> Result<Vec<RedactionKeyListId>> {
    Ok(redaction_config_key_lists(app)?.into_keys().collect())
}

fn print_aws_sync_summary(report: &super::aws_sync::AwsSyncReport) {
    let already_present = report
        .items
        .iter()
        .filter(|item| {
            matches!(
                item,
                super::aws_sync::AwsSyncItemResult::AlreadyPresent { .. }
            )
        })
        .count();
    let inserted = report
        .items
        .iter()
        .filter_map(|item| match item {
            super::aws_sync::AwsSyncItemResult::Inserted { id } => Some(id),
            _ => None,
        })
        .collect::<Vec<_>>();
    let failed = report
        .items
        .iter()
        .filter_map(|item| match item {
            super::aws_sync::AwsSyncItemResult::Failed { id, error } => Some((id, error)),
            _ => None,
        })
        .collect::<Vec<_>>();

    eprintln!("AWS sync summary:");
    eprintln!("  Already present: {already_present}");
    eprintln!("  Inserted: {}", inserted.len());
    for id in inserted {
        eprintln!("    {id}");
    }
    eprintln!("  Failed: {}", failed.len());
    for (id, error) in failed {
        eprintln!("    {id}: {error}");
    }
}

pub async fn sync_all_key_lists_to_aws(
    ctx: &CoreContext,
    app: &MononokeApp,
    args: RedactionSyncToAwsArgs,
) -> Result<()> {
    let key_list_ids = active_redaction_key_list_ids(app)?;
    if key_list_ids.is_empty() {
        eprintln!("Prod redaction config contains no key lists; nothing to sync");
        return Ok(());
    }
    eprintln!(
        "Found {} unique key list(s) in the active prod redaction config",
        key_list_ids.len()
    );

    let redaction_blobstore = app.redaction_config_blobstore().await?;
    let fetched_key_lists = stream::iter(key_list_ids)
        .map(|key_list_id| {
            let redaction_blobstore = &redaction_blobstore;
            async move {
                (
                    key_list_id,
                    redaction::fetch_key_list(ctx, redaction_blobstore, key_list_id).await,
                )
            }
        })
        .buffer_unordered(AWS_SYNC_CONCURRENCY)
        .collect::<Vec<_>>()
        .await;
    let mut key_lists = Vec::new();
    let mut report = super::aws_sync::AwsSyncReport::default();
    let mut aggregate_sync_error = None;
    for (key_list_id, result) in fetched_key_lists {
        match result {
            Ok(key_list) => {
                eprintln!(
                    "Read key list {key_list_id} ({} keys) from prod",
                    key_list.keys.len()
                );
                key_lists.push(super::aws_sync::AwsKeyList {
                    id: key_list_id.to_string(),
                    keys: key_list.keys,
                });
            }
            Err(error) => report
                .items
                .push(super::aws_sync::AwsSyncItemResult::failed(
                    key_list_id.to_string(),
                    format!("Failed to read key list from prod: {error:#}"),
                )),
        }
    }

    if !key_lists.is_empty() {
        let sync_result: Result<_> = async {
            let target = super::aws_sync::prepare_sync(args.bootstrap_monad).await?;
            super::aws_sync::sync_key_lists_to_aws(&target, &key_lists).await
        }
        .await;
        match sync_result {
            Ok((method, remote_report)) => {
                let method = match method {
                    super::aws_sync::AwsSyncMethod::RunningPod => "running pod",
                    super::aws_sync::AwsSyncMethod::TemporaryJob => "temporary job",
                };
                eprintln!("AWS sync completed via {method}");
                report.items.extend(remote_report.items);
            }
            Err(error) => {
                eprintln!(
                    "AWS sync outcome is unknown for {} key list(s):",
                    key_lists.len()
                );
                for key_list in &key_lists {
                    eprintln!("    {}", key_list.id);
                }
                aggregate_sync_error = Some(format!("AWS operation failed: {error:#}"));
            }
        }
    }

    print_aws_sync_summary(&report);
    if let Some(error) = aggregate_sync_error {
        bail!(error);
    }
    if report.has_failures() {
        bail!("Redaction key-list synchronization failed; see the per-list errors above");
    }
    Ok(())
}

fn write_key_list_id(key_list_id: RedactionKeyListId, output_file: Option<&Path>) -> Result<()> {
    if let Some(output_file) = output_file {
        let mut output = File::create(output_file).with_context(|| {
            format!(
                "Failed to open output file '{}'",
                output_file.to_string_lossy()
            )
        })?;
        output
            .write_all(key_list_id.to_string().as_bytes())
            .with_context(|| {
                format!(
                    "Failed to write to output file '{}'",
                    output_file.to_string_lossy()
                )
            })?;
    }
    Ok(())
}

async fn create_key_list(
    ctx: &CoreContext,
    app: &MononokeApp,
    keys: Vec<String>,
    output_file: Option<&Path>,
) -> Result<RedactionKeyListId> {
    let redaction_blobstore = app.redaction_config_blobstore().await?;
    let key_list_id = redaction::create_key_list(ctx, &redaction_blobstore, keys).await?;
    write_key_list_id(key_list_id, output_file)?;
    Ok(key_list_id)
}

/// Returns the content keys for the given paths.
async fn content_keys_for_paths(
    ctx: &CoreContext,
    repo: &Repo,
    cs_id: ChangesetId,
    commit_label: &str,
    paths: Vec<NonRootMPath>,
) -> Result<HashSet<String>> {
    let root_manifest_id = repo
        .repo_derived_data()
        .derive::<RootContentManifestId>(ctx, cs_id, DerivationPriority::LOW)
        .await?
        .into_content_manifest_id();

    let path_content_keys = root_manifest_id
        .find_entries(ctx.clone(), repo.repo_blobstore_arc(), paths.clone())
        .try_filter_map(|(path, entry)| async move {
            match (path.into_optional_non_root_path(), entry) {
                (Some(path), Entry::Leaf(file)) => {
                    Ok(Some((path, file.content_id.blobstore_key())))
                }
                _ => Ok(None),
            }
        })
        .try_collect::<HashMap<_, _>>()
        .await?;

    let mut missing_paths = 0;
    for path in paths.iter() {
        if !path_content_keys.contains_key(path) {
            eprintln!("Missing file in commit {commit_label}: {path}");
            missing_paths += 1;
        }
    }
    if missing_paths > 0 {
        bail!("Failed to find {missing_paths} files in commit {commit_label}");
    }

    Ok(path_content_keys.into_values().collect())
}

fn parse_commit_path_pair(line: &str, line_number: usize) -> Result<(String, NonRootMPath)> {
    let (commit_id, path) = line
        .split_once(':')
        .ok_or_else(|| anyhow!("Invalid input line {line_number}: expected a commit:path pair"))?;
    if commit_id.is_empty() {
        bail!("Invalid input line {line_number}: commit is empty");
    }
    if path.is_empty() {
        bail!("Invalid input line {line_number}: path is empty");
    }
    let path = NonRootMPath::new(path)
        .with_context(|| format!("Invalid path on input line {line_number}"))?;
    Ok((commit_id.to_string(), path))
}

async fn content_keys_for_commit_paths(
    ctx: &CoreContext,
    repo: &Repo,
    paths_by_commit: HashMap<String, Vec<NonRootMPath>>,
) -> Result<HashSet<String>> {
    stream::iter(paths_by_commit)
        .map(|(commit_id, paths)| async move {
            let cs_id = parse_commit_id(ctx, repo, &commit_id)
                .await
                .with_context(|| format!("Failed to parse commit id '{commit_id}'"))?;
            content_keys_for_paths(ctx, repo, cs_id, &commit_id, paths)
                .await
                .with_context(|| format!("Failed to find content keys in commit '{commit_id}'"))
        })
        .buffer_unordered(COMMIT_LOOKUP_CONCURRENCY)
        .try_fold(HashSet::new(), |mut keys, commit_keys| async move {
            keys.extend(commit_keys);
            Ok(keys)
        })
        .await
}

pub async fn create_key_list_from_commit_files(
    ctx: &CoreContext,
    app: &MononokeApp,
    create_args: RedactionCreateKeyListArgs,
) -> Result<()> {
    let RedactionCreateKeyListArgs {
        repo_args,
        commit_id,
        main_bookmark,
        force,
        input_file,
        output_file,
        skip_aws_sync,
        files,
    } = create_args;

    let mut paths_by_commit: HashMap<String, Vec<NonRootMPath>> = HashMap::new();
    if let Some(commit_id) = commit_id {
        let mut paths = files
            .into_iter()
            .map(NonRootMPath::new)
            .collect::<Result<Vec<_>>>()?;
        if let Some(input_file) = input_file {
            let input_file =
                BufReader::new(File::open(input_file).context("Failed to open input file")?);
            for line in input_file.lines() {
                paths.push(NonRootMPath::new(line?)?);
            }
        }
        paths_by_commit.insert(commit_id, paths);
    } else {
        if !files.is_empty() {
            bail!("--commit-id is required when passing FILE arguments");
        }
        let input_file = input_file
            .ok_or_else(|| anyhow!("--input-file is required when --commit-id is omitted"))?;
        let input_file =
            BufReader::new(File::open(input_file).context("Failed to open input file")?);
        for (line_number, line) in input_file.lines().enumerate() {
            let (commit_id, path) = parse_commit_path_pair(&line?, line_number + 1)?;
            paths_by_commit.entry(commit_id).or_default().push(path);
        }
    }
    if paths_by_commit.values().all(Vec::is_empty) {
        bail!("No files to redact");
    }
    let repo: Repo = app
        .open_repo(&repo_args)
        .await
        .context("Failed to open repo")?;

    let keys = content_keys_for_commit_paths(ctx, &repo, paths_by_commit).await?;

    println!("Checking redacted content doesn't exist in '{main_bookmark}' bookmark");
    let main_cs_id = repo
        .bookmarks()
        .get(
            ctx.clone(),
            &main_bookmark,
            bookmarks::Freshness::MostRecent,
        )
        .await?
        .ok_or_else(|| anyhow!("Main bookmark '{main_bookmark}' does not exist"))?;
    let main_redacted = paths_for_content_keys(ctx, &repo, main_cs_id, &keys).await?;

    if main_redacted.is_empty() {
        println!("No files would be redacted in the main bookmark ({main_bookmark})");
    } else {
        for (path, content_id) in main_redacted.iter() {
            println!(
                "Redacted content in main bookmark: {} {}",
                path,
                content_id.blobstore_key(),
            );
        }
        if force {
            println!(
                "Creating key list despite {} files being redacted in the main bookmark ({}) (--force)",
                main_redacted.len(),
                main_bookmark
            );
        } else {
            bail!(
                "Refusing to create key list because {} files would be redacted in the main bookmark ({})",
                main_redacted.len(),
                main_bookmark
            );
        }
    }

    let keys_vec: Vec<String> = keys.into_iter().collect();
    let keys_for_sync = keys_vec.clone();

    let key_list_id = create_key_list(ctx, app, keys_vec, output_file.as_deref()).await?;

    if !skip_aws_sync {
        super::aws_sync::sync_to_aws(&keys_for_sync, key_list_id).await;
    }

    Ok(())
}

pub async fn sync_key_lists_from_json(
    ctx: &CoreContext,
    app: &MononokeApp,
    args: RedactionSyncKeyListsFromJsonArgs,
) -> Result<()> {
    let payload = match (args.payload, args.payload_stdin) {
        (Some(payload), false) => payload,
        (None, true) => {
            let mut payload = String::new();
            tokio::io::stdin()
                .take((AWS_SYNC_MAX_PAYLOAD_BYTES + 1) as u64)
                .read_to_string(&mut payload)
                .await
                .context("Failed to read AWS key-list sync payload from stdin")?;
            payload
        }
        _ => unreachable!("clap requires exactly one payload source"),
    };
    if payload.len() > AWS_SYNC_MAX_PAYLOAD_BYTES {
        bail!("AWS key-list sync payload exceeds {AWS_SYNC_MAX_PAYLOAD_BYTES} bytes");
    }
    let key_lists: Vec<super::aws_sync::AwsKeyList> =
        serde_json::from_str(&payload).context("Invalid AWS key-list sync payload")?;
    let mut ids = HashSet::with_capacity(key_lists.len());
    for key_list in &key_lists {
        if !ids.insert(key_list.id.as_str()) {
            bail!(
                "AWS key-list sync payload contains duplicate id {}",
                key_list.id
            );
        }
    }
    let redaction_blobstore = app.redaction_config_blobstore().await?;
    let items = stream::iter(key_lists)
        .map(|requested| {
            let redaction_blobstore = &redaction_blobstore;
            async move {
                let id = requested.id.clone();
                let result: Result<super::aws_sync::AwsSyncItemResult> = async {
                    let expected = RedactionKeyList {
                        keys: requested.keys,
                    };
                    let blob = expected.into_blob();
                    let actual_id = *blob.id();
                    if actual_id.to_string() != id {
                        bail!("Content hashes to {actual_id}, not the requested id {id}");
                    }

                    let overwrite_status = redaction_blobstore
                        .put_explicit(
                            ctx,
                            actual_id.blobstore_key(),
                            blob.into(),
                            PutBehaviour::IfAbsent,
                        )
                        .await
                        .context("Failed to write the redaction key list")?;
                    match overwrite_status {
                        OverwriteStatus::New => {
                            Ok(super::aws_sync::AwsSyncItemResult::inserted(id.clone()))
                        }
                        OverwriteStatus::Prevented => Ok(
                            super::aws_sync::AwsSyncItemResult::already_present(id.clone()),
                        ),
                        OverwriteStatus::NotChecked | OverwriteStatus::Overwrote => {
                            bail!("IfAbsent write returned unexpected status {overwrite_status:?}")
                        }
                    }
                }
                .await;

                result.unwrap_or_else(|error| {
                    super::aws_sync::AwsSyncItemResult::failed(id, format!("{error:#}"))
                })
            }
        })
        .buffer_unordered(AWS_SYNC_CONCURRENCY)
        .collect::<Vec<_>>()
        .await;

    super::aws_sync::print_sync_report(&super::aws_sync::AwsSyncReport { items })
}
