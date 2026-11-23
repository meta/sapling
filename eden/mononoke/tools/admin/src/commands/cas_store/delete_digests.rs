/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::BTreeSet;
use std::collections::HashSet;
use std::iter;
use std::num::NonZeroU32;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Error;
use anyhow::Result;
use anyhow::anyhow;
use async_trait::async_trait;
use bytes::Bytes;
use cas_client::CasClient;
use cas_client::build_mononoke_cas_client;
use clap::Args;
use context::CoreContext;
use futures::Stream;
use governor::DefaultDirectRateLimiter;
use governor::Quota;
use governor::RateLimiter;
use metaconfig_types::RepoConfigRef;
use mononoke_types::MononokeDigest;
use repo_identity::RepoIdentityRef;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tracing::info;

use super::Repo;
use crate::commands::redaction::delete_from_cas::delete_present_digests;
use crate::commands::redaction::delete_from_cas::present_digests;

/// Digests looked up, deleted and verified together, between progress reports.
const CHUNK_SIZE: usize = 100_000;

#[derive(Args)]
pub struct CasStoreDeleteDigestsArgs {
    /// Report which of the digests are present in CAS without deleting them.
    #[clap(long)]
    dry_run: bool,

    /// Write `<digest> <deleted|missing> <use case>` for each digest and use case to this new
    /// file, with `present` instead of `deleted` for a dry run. The digests are written once their
    /// batch is verified, so the file lists the digests a failed run got through.
    #[clap(long, value_name = "FILE")]
    output_file: PathBuf,

    /// The most digests to send to CAS per second, counting the lookups, the deletes and the
    /// lookups that verify the deletes.
    #[clap(long)]
    digests_per_second: NonZeroU32,

    /// Files with one digest `<blake3>:<size>` per line.
    #[clap(value_name = "FILE", required = true)]
    files: Vec<PathBuf>,
}

pub async fn delete_digests(
    ctx: &CoreContext,
    repo: &Repo,
    args: CasStoreDeleteDigestsArgs,
) -> Result<()> {
    let repo_name = repo.repo_identity().name();
    let config = repo
        .repo_config()
        .mononoke_cas_sync_config
        .as_ref()
        .ok_or_else(|| anyhow!("Missing mononoke_cas_sync_config config for repo: {repo_name}"))?;

    let mut digests = BTreeSet::new();
    for file in &args.files {
        let contents = fs::read_to_string(file)
            .await
            .with_context(|| format!("Failed to read {}", file.display()))?;
        for line in contents.lines() {
            let digest = parse_digest(line).with_context(|| {
                format!(
                    "{} has a line that is not <blake3>:<size>: {line:?}",
                    file.display()
                )
            })?;
            digests.insert(digest);
        }
    }
    let digests: Vec<_> = digests.into_iter().collect();
    info!(
        "Read {} digests from {} files",
        digests.len(),
        args.files.len()
    );

    let mut output = fs::File::create_new(&args.output_file)
        .await
        .with_context(|| format!("Failed to create {}", args.output_file.display()))?;
    let (public, draft) = (&config.use_case_public, &config.use_case_draft);
    let use_cases = iter::once(public).chain((draft != public).then_some(draft));
    for use_case in use_cases {
        let client = ThrottledCasClient {
            client: build_mononoke_cas_client(ctx.fb, ctx.clone(), repo_name, false, use_case)?,
            limiter: RateLimiter::direct(Quota::per_second(args.digests_per_second)),
        };
        let mut done = 0;
        let mut present = 0;
        for chunk in digests.chunks(CHUNK_SIZE) {
            let chunk_present = present_digests(&client, chunk).await?;
            if !args.dry_run {
                delete_present_digests(&client, &chunk_present)
                    .await
                    .with_context(|| {
                        format!("Failed to delete digests from CAS use case {use_case}")
                    })?;
            }
            let chunk_present: HashSet<_> = chunk_present.into_iter().collect();
            let lines: String = chunk
                .iter()
                .map(|digest| {
                    let state = match (chunk_present.contains(digest), args.dry_run) {
                        (false, _) => "missing",
                        (true, true) => "present",
                        (true, false) => "deleted",
                    };
                    format!("{digest} {state} {use_case}\n")
                })
                .collect();
            output.write_all(lines.as_bytes()).await?;
            output.flush().await?;
            done += chunk.len();
            present += chunk_present.len();
            info!(
                "{use_case}: looked up {done} of {} digests, {present} present{}",
                digests.len(),
                if args.dry_run { "" } else { " and deleted" }
            );
        }
    }
    Ok(())
}

fn parse_digest(line: &str) -> Option<MononokeDigest> {
    let (hash, size) = line.split_once(':')?;
    Some(MononokeDigest(hash.parse().ok()?, size.parse().ok()?))
}

/// A CAS client that waits on `limiter` for each digest it sends to CAS.
struct ThrottledCasClient<C> {
    client: C,
    limiter: DefaultDirectRateLimiter,
}

impl<C> ThrottledCasClient<C> {
    async fn throttle(&self, digests: usize) {
        for _ in 0..digests {
            self.limiter.until_ready().await;
        }
    }
}

#[async_trait]
impl<C: CasClient> CasClient for ThrottledCasClient<C> {
    async fn streaming_upload_blob(
        &self,
        digest: &MononokeDigest,
        bytes_stream: impl Stream<Item = Result<Bytes, Error>> + Send,
    ) -> Result<(), Error> {
        self.throttle(1).await;
        self.client
            .streaming_upload_blob(digest, bytes_stream)
            .await
    }

    async fn upload_blob(&self, digest: &MononokeDigest, bytes: Bytes) -> Result<(), Error> {
        self.throttle(1).await;
        self.client.upload_blob(digest, bytes).await
    }

    async fn upload_blobs(&self, blobs: Vec<(MononokeDigest, Bytes)>) -> Result<(), Error> {
        self.throttle(blobs.len()).await;
        self.client.upload_blobs(blobs).await
    }

    async fn lookup_blob(&self, digest: &MononokeDigest) -> Result<bool, Error> {
        self.throttle(1).await;
        self.client.lookup_blob(digest).await
    }

    async fn missing_digests<'a>(
        &self,
        digests: &'a [MononokeDigest],
    ) -> Result<Vec<MononokeDigest>, Error> {
        self.throttle(digests.len()).await;
        self.client.missing_digests(digests).await
    }

    async fn delete_blobs(&self, digests: &[MononokeDigest]) -> Result<(), Error> {
        self.throttle(digests.len()).await;
        self.client.delete_blobs(digests).await
    }

    fn repo_name(&self) -> &str {
        self.client.repo_name()
    }
}
