/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::BTreeSet;
use std::collections::HashSet;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use cas_client::CasClient;
use cas_client::build_mononoke_cas_client;
use clap::ArgGroup;
use clap::Args;
use context::CoreContext;
use filestore::FetchKey;
use futures::stream;
use futures::stream::StreamExt;
use futures::stream::TryStreamExt;
use itertools::Itertools;
use metaconfig_types::RepoConfigRef;
use mononoke_app::MononokeApp;
use mononoke_app::args::RepoArgs;
use mononoke_types::BlobstoreKey;
use mononoke_types::ContentId;
use mononoke_types::MononokeDigest;
use mononoke_types::typed_hash::RedactionKeyListId;
use repo_blobstore::RepoBlobstoreRef;
use repo_identity::RepoIdentityRef;

use super::Repo;
use super::create_key_list::redaction_config_key_lists;

const FETCH_CONCURRENCY: usize = 100;
const CAS_BATCH_SIZE: usize = 500;
const CAS_CONCURRENCY: usize = 4;
const VERIFY_ATTEMPTS: u32 = 5;
const VERIFY_BACKOFF: Duration = Duration::from_secs(2);

#[derive(Args)]
#[clap(group(ArgGroup::new("key-lists").args(&["key_list_ids", "all_enforced"]).required(true)))]
pub struct RedactionDeleteFromCasArgs {
    #[clap(flatten)]
    repo_args: RepoArgs,

    /// Delete the content of every enforced key list in the redaction config.
    #[clap(long)]
    all_enforced: bool,

    /// Report which redacted files are present in CAS without deleting them.
    #[clap(long)]
    dry_run: bool,

    /// Verbose logging of the CAS client.
    #[clap(long)]
    verbose: bool,

    /// Key lists whose content to delete from CAS. Each must be enforced in
    /// the redaction config.
    #[clap(value_name = "KEY LIST ID")]
    key_list_ids: Vec<RedactionKeyListId>,
}

pub async fn delete_from_cas(
    ctx: &CoreContext,
    app: &MononokeApp,
    args: RedactionDeleteFromCasArgs,
) -> Result<()> {
    let repo: Repo = app
        .open_repo(&args.repo_args)
        .await
        .context("Failed to open repo")?;
    let repo_name = repo.repo_identity().name();
    let cas_config = repo
        .repo_config()
        .mononoke_cas_sync_config
        .as_ref()
        .ok_or_else(|| anyhow!("Missing mononoke_cas_sync_config config for repo: {repo_name}"))?;

    let key_list_ids = select_key_lists(app, args.key_list_ids, args.all_enforced)?;
    let digests = redacted_digests(ctx, app, &repo, &key_list_ids).await?;
    println!(
        "Found {} redacted files in {} key lists",
        digests.len(),
        key_list_ids.len()
    );

    let use_cases = BTreeSet::from([
        cas_config.use_case_public.as_str(),
        cas_config.use_case_draft.as_str(),
    ]);
    for use_case in use_cases {
        let client =
            build_mononoke_cas_client(ctx.fb, ctx.clone(), repo_name, args.verbose, use_case)?;
        delete_digests(&client, use_case, &digests, args.dry_run)
            .await
            .with_context(|| {
                format!("Failed to delete redacted files from CAS use case {use_case}")
            })?;
    }
    Ok(())
}

/// Content uploaded to CAS before redaction is enforced can be uploaded again
/// after we delete it, so only enforced key lists are accepted.
fn select_key_lists(
    app: &MononokeApp,
    requested: Vec<RedactionKeyListId>,
    all_enforced: bool,
) -> Result<Vec<RedactionKeyListId>> {
    let enforcement = redaction_config_key_lists(app)?;
    if all_enforced {
        let enforced = enforcement
            .into_iter()
            .filter_map(|(id, enforced)| enforced.then_some(id))
            .collect::<Vec<_>>();
        if enforced.is_empty() {
            bail!("The redaction config contains no enforced key lists");
        }
        return Ok(enforced);
    }

    let not_enforced = requested
        .iter()
        .filter(|id| enforcement.get(id) != Some(&true))
        .join(", ");
    if !not_enforced.is_empty() {
        bail!(
            "Refusing to delete from CAS: key lists must be enforced in the redaction config first, but these are not: {not_enforced}"
        );
    }
    Ok(requested.into_iter().unique().collect())
}

/// The CAS digests of the content in the given key lists, sorted.
async fn redacted_digests(
    ctx: &CoreContext,
    app: &MononokeApp,
    repo: &Repo,
    key_list_ids: &[RedactionKeyListId],
) -> Result<Vec<MononokeDigest>> {
    let redaction_blobstore = app.redaction_config_blobstore().await?;
    let content_ids = stream::iter(key_list_ids)
        .map(|&key_list_id| {
            let redaction_blobstore = &redaction_blobstore;
            async move {
                let key_list = redaction::fetch_key_list(ctx, redaction_blobstore, key_list_id)
                    .await
                    .with_context(|| format!("Failed to fetch key list {key_list_id}"))?;
                key_list
                    .keys
                    .iter()
                    .map(|key| {
                        ContentId::parse_blobstore_key(key).with_context(|| {
                            format!("Key list {key_list_id} contains a key that is not file content: {key}")
                        })
                    })
                    .collect::<Result<Vec<_>>>()
            }
        })
        .buffer_unordered(FETCH_CONCURRENCY)
        .try_fold(BTreeSet::new(), |mut content_ids, key_list_content_ids| async move {
            content_ids.extend(key_list_content_ids);
            Ok(content_ids)
        })
        .await?;

    // Metadata blobs are never redacted, so the digest is available even
    // though the content is not.
    let digests = stream::iter(content_ids)
        .map(|content_id| async move {
            let metadata = filestore::get_metadata(
                repo.repo_blobstore(),
                ctx,
                &FetchKey::Canonical(content_id),
            )
            .await?
            .ok_or_else(|| anyhow!("Missing metadata for redacted content {content_id}"))?;
            Ok::<_, anyhow::Error>(MononokeDigest(metadata.seeded_blake3, metadata.total_size))
        })
        .buffer_unordered(FETCH_CONCURRENCY)
        .try_collect::<BTreeSet<_>>()
        .await?;
    Ok(digests.into_iter().collect())
}

async fn delete_digests(
    client: &impl CasClient,
    use_case: &str,
    digests: &[MononokeDigest],
    dry_run: bool,
) -> Result<()> {
    let present = present_digests(client, digests).await?;
    println!(
        "{use_case}: {} of {} redacted files are present in CAS",
        present.len(),
        digests.len()
    );
    for digest in &present {
        println!("  {digest}");
    }
    if dry_run || present.is_empty() {
        return Ok(());
    }

    delete_present_digests(client, &present).await?;
    println!(
        "{use_case}: deleted {} redacted files and verified they are absent",
        present.len()
    );
    Ok(())
}

/// The CAS delete call reports no per-digest status, so anything still
/// present afterwards is an error.
pub(crate) async fn delete_present_digests(
    client: &impl CasClient,
    present: &[MononokeDigest],
) -> Result<()> {
    stream::iter(present.chunks(CAS_BATCH_SIZE).map(Ok))
        .try_for_each_concurrent(CAS_CONCURRENCY, |chunk| client.delete_blobs(chunk))
        .await?;

    let still_present = still_present_after_delete(client, present).await?;
    if !still_present.is_empty() {
        bail!(
            "{} blobs are still present in CAS after deleting them: {}",
            still_present.len(),
            still_present.iter().join(", ")
        );
    }
    Ok(())
}

/// A lookup issued right after the delete can still report a deleted digest as
/// present, so only digests that stay present across several lookups count.
async fn still_present_after_delete(
    client: &impl CasClient,
    deleted: &[MononokeDigest],
) -> Result<Vec<MononokeDigest>> {
    let mut still_present = present_digests(client, deleted).await?;
    for attempt in 1..VERIFY_ATTEMPTS {
        if still_present.is_empty() {
            break;
        }
        tokio::time::sleep(VERIFY_BACKOFF * attempt).await;
        still_present = present_digests(client, &still_present).await?;
    }
    Ok(still_present)
}

pub(crate) async fn present_digests(
    client: &impl CasClient,
    digests: &[MononokeDigest],
) -> Result<Vec<MononokeDigest>> {
    let missing = stream::iter(digests.chunks(CAS_BATCH_SIZE))
        .map(|chunk| client.missing_digests(chunk))
        .buffer_unordered(CAS_CONCURRENCY)
        .try_fold(HashSet::new(), |mut missing, chunk_missing| async move {
            missing.extend(chunk_missing);
            Ok(missing)
        })
        .await?;
    Ok(digests
        .iter()
        .filter(|digest| !missing.contains(digest))
        .copied()
        .collect())
}
