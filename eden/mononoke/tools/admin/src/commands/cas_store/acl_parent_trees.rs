/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::collections::HashSet;
use std::fs::TryLockError;
use std::io;
use std::iter;
use std::num::NonZeroU32;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use blobstore::Loadable;
use bookmarks::BookmarkKey;
use bookmarks::BookmarkUpdateLogId;
use bookmarks::BookmarkUpdateLogRef;
use bookmarks::Freshness;
use cas_client::CasClient;
use cas_client::build_mononoke_cas_client;
use changesets_uploader::CasChangesetsUploader;
use chrono::NaiveDateTime;
use clap::Args;
use commit_graph::CommitGraphRef;
use context::CoreContext;
use futures::StreamExt;
use futures::TryStreamExt;
use futures::stream;
use mercurial_derivation::RootHgAugmentedManifestId;
use mercurial_types::HgAugmentedManifestEntry;
use mercurial_types::HgAugmentedManifestId;
use mercurial_types::HgPreloadedAugmentedManifest;
use metaconfig_types::RepoConfigRef;
use mononoke_macros::mononoke;
use mononoke_types::ChangesetId;
use mononoke_types::DateTime;
use mononoke_types::MPath;
use mononoke_types::MononokeDigest;
use mononoke_types::NonRootMPath;
use mononoke_types::Timestamp;
use repo_blobstore::RepoBlobstoreRef;
use repo_derived_data::RepoDerivedDataRef;
use repo_identity::RepoIdentityRef;
use restricted_paths::ManifestType;
use restricted_paths::RestrictedPathsRef;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::time::timeout;
use tracing::info;
use tracing::warn;

use super::Repo;

const CONCURRENCY: usize = 100;
/// The CAS presence lookup has no timeout of its own.
const PRESENCE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// Slice files are named `digests-<start>-<end>.txt`, with both times in UTC.
const SLICE_TIME_FORMAT: &str = "%Y%m%dT%H%M%SZ";
/// A resumed run looks up its first commit this long before the end of the slices already
/// written, so that replica lag or host clock skew in the bookmark log can only make it scan some
/// commits again.
const RESUME_MARGIN_SECS: i64 = 10 * 60;

/// Print `<blake3>:<size> <present|missing|unchecked> <hgid> <path>` for every tree in the scanned
/// commits that has a restriction root as a direct child. The state is the tree's presence in CAS,
/// looked up only with `--check-presence`. The window is scanned in time slices, one after another,
/// and the trees found in a slice are printed, or written to the slice's file, when it is done.
/// Slices are not deduplicated against each other: a tree is listed by every slice with a commit
/// that has it, and the first slice of a run also lists the trees of its start commit.
#[derive(Args)]
pub struct CasStoreAclParentTreesArgs {
    /// Bookmark whose commits to scan. Defaults to the bookmark synced to CAS.
    #[clap(long, short = 'B')]
    bookmark: Option<String>,

    /// Scan from the commit the bookmark pointed to at this time
    /// (either absolute time, or e.g. "2 days ago").
    #[clap(long, short = 's')]
    start_time: DateTime,

    /// Scan up to the commit the bookmark pointed to at this time. Defaults to the bookmark's
    /// latest move.
    #[clap(long, short = 'e')]
    end_time: Option<DateTime>,

    /// Length of a time slice in hours.
    #[clap(long, default_value = "24")]
    slice_hours: NonZeroU32,

    /// Look up the trees in CAS. A lookup extends the TTL of a blob close to expiry.
    #[clap(long)]
    check_presence: bool,

    /// Write the trees of each slice to `DIR/digests-<start>-<end>.txt` instead of stdout, with
    /// the times in UTC. A tree can be listed in several files. A folder holds one scan:
    /// rerunning with the same absolute start time continues from where its files end.
    #[clap(long, value_name = "DIR")]
    output_dir: Option<PathBuf>,
}

/// A tree with a restriction root as a direct child.
#[derive(PartialEq, Eq, PartialOrd, Ord, Hash)]
struct ParentTree {
    path: MPath,
    tree_id: HgAugmentedManifestId,
    digest: MononokeDigest,
}

struct Scanner<C>
where
    C: CasClient,
{
    ctx: CoreContext,
    repo: Repo,
    uploader: CasChangesetsUploader<C>,
    towards_config_roots: HashSet<MPath>,
}

pub async fn acl_parent_trees(
    ctx: &CoreContext,
    repo: &Repo,
    args: CasStoreAclParentTreesArgs,
) -> Result<()> {
    let config = repo
        .repo_config()
        .mononoke_cas_sync_config
        .as_ref()
        .ok_or_else(|| {
            anyhow!(
                "Missing mononoke_cas_sync_config config for repo: {}",
                repo.repo_identity().name()
            )
        })?;
    let bookmark = BookmarkKey::new(
        args.bookmark
            .as_deref()
            .unwrap_or(&config.main_bookmark_to_sync),
    )?;

    let time = |secs| DateTime::from_timestamp(secs, 0);
    let requested_start = args.start_time.timestamp_secs();
    let (slice_files, start_secs) = match &args.output_dir {
        Some(dir) => {
            let (slice_files, start_secs) = SliceFiles::open(dir, requested_start).await?;
            (Some(slice_files), start_secs)
        }
        None => (None, requested_start),
    };
    // Times are cut to the second, so a slice file never claims more than its slice scanned, and
    // a resumed run starts at or before where the previous one stopped.
    let (latest, latest_time) = latest_move(ctx, repo, &bookmark).await?;
    let (end, end_secs) = match args.end_time {
        Some(end_time) if Timestamp::from(end_time) < latest_time => (
            bookmark_at(ctx, repo, &bookmark, end_time).await?,
            end_time.timestamp_secs(),
        ),
        _ => (latest, latest_time.timestamp_seconds()),
    };
    if slice_files.is_some() && end_secs <= start_secs {
        info!(
            "Nothing to scan from {} to {}",
            time(start_secs)?,
            time(end_secs)?
        );
        return Ok(());
    }
    let step = i64::from(args.slice_hours.get()) * 3600;
    let bounds: Vec<i64> = iter::once(start_secs)
        .chain(
            (1..)
                .map(|index| start_secs + index * step)
                .take_while(|secs| *secs < end_secs),
        )
        .chain(iter::once(end_secs.max(start_secs)))
        .collect();
    let slices = bounds.len() - 1;
    let start_lookup = (start_secs - RESUME_MARGIN_SECS).max(requested_start);
    let start = bookmark_at(ctx, repo, &bookmark, time(start_lookup)?).await?;

    let scanner = Arc::new(Scanner {
        ctx: ctx.clone(),
        repo: repo.clone(),
        uploader: CasChangesetsUploader::new(build_mononoke_cas_client(
            ctx.fb,
            ctx.clone(),
            repo.repo_identity().name(),
            false,
            &config.use_case_public,
        )?),
        towards_config_roots: towards_config_roots(ctx, repo).await?,
    });
    let mut commits = 0;
    let mut found = 0;
    let mut present = 0;
    let mut from = start;
    for (index, bound) in bounds.windows(2).enumerate() {
        let (slice_start, slice_end) = (bound[0], bound[1]);
        let started = Instant::now();
        let to = if index + 1 < slices {
            bookmark_at(ctx, repo, &bookmark, time(slice_end)?).await?
        } else {
            end
        };
        let (slice_commits, mut parents) = scanner.scan_slice(from, to, index == 0).await?;
        parents.sort();
        let missing = if args.check_presence {
            let digests: Vec<_> = parents.iter().map(|parent| parent.digest).collect();
            let missing = timeout(
                PRESENCE_TIMEOUT,
                scanner.uploader.client.missing_digests(&digests),
            )
            .await
            .map_err(|_| {
                anyhow!(
                    "Looking up {} trees in CAS took longer than {:?}",
                    digests.len(),
                    PRESENCE_TIMEOUT
                )
            })??;
            present += digests
                .iter()
                .filter(|digest| !missing.contains(digest))
                .count();
            Some(missing)
        } else {
            None
        };
        let lines: String = parents
            .iter()
            .map(|parent| {
                let state = match &missing {
                    None => "unchecked",
                    Some(missing) if missing.contains(&parent.digest) => "missing",
                    Some(_) => "present",
                };
                format!(
                    "{} {} {} /{}\n",
                    parent.digest, state, parent.tree_id, parent.path
                )
            })
            .collect();
        match &slice_files {
            Some(slice_files) => slice_files.write(slice_start, slice_end, &lines).await?,
            None => print!("{lines}"),
        }
        info!(
            "Slice {}/{} from {} to {}: {} commits, {} parent trees, scanned in {:.1?}",
            index + 1,
            slices,
            time(slice_start)?,
            time(slice_end)?,
            slice_commits,
            parents.len(),
            started.elapsed(),
        );
        commits += slice_commits;
        found += parents.len();
        from = to;
    }
    let presence = if args.check_presence {
        format!(", {present} of them in CAS")
    } else {
        String::new()
    };
    info!(
        "Scanned {} commits of {} from {} to {} in {} slices: {} parent trees of restriction roots{}, counted once per slice",
        commits, bookmark, start, end, slices, found, presence,
    );
    Ok(())
}

/// The commit of the bookmark's latest move and the time of the move, from the primary database.
async fn latest_move(
    ctx: &CoreContext,
    repo: &Repo,
    bookmark: &BookmarkKey,
) -> Result<(ChangesetId, Timestamp)> {
    let (_, cs_id, _, timestamp) = repo
        .bookmark_update_log()
        .list_bookmark_log_entries(
            ctx.clone(),
            bookmark.clone(),
            1,
            None,
            Freshness::MostRecent,
        )
        .try_next()
        .await?
        .ok_or_else(|| anyhow!("Bookmark {bookmark} has no log entry"))?;
    let cs_id = cs_id.ok_or_else(|| anyhow!("Bookmark {bookmark} was deleted"))?;
    Ok((cs_id, timestamp))
}

/// The folder of a scan with `--output-dir`, with one file per finished slice. A slice is written
/// to a hidden file and renamed once complete, so the files that chain from the scan's start time,
/// each starting where the previous one ends, show how far the scan has got.
struct SliceFiles {
    dir: PathBuf,
    /// Held for the whole run, so that two runs cannot write to the same folder.
    _lock: std::fs::File,
}

impl SliceFiles {
    /// Lock `dir` and return where the scan that starts at `start` (in seconds) continues.
    async fn open(dir: &Path, start: i64) -> Result<(Self, i64)> {
        fs::create_dir_all(dir)
            .await
            .with_context(|| format!("Failed to create {}", dir.display()))?;
        let lock_path = dir.join(".lock");
        let lock = fs::File::create(&lock_path)
            .await
            .with_context(|| format!("Failed to create {}", lock_path.display()))?
            .into_std()
            .await;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => bail!("Another run is writing to {}", dir.display()),
            Err(TryLockError::Error(e)) => {
                return Err(e).with_context(|| format!("Failed to lock {}", lock_path.display()));
            }
        }

        let mut slices = HashMap::new();
        let mut entries = fs::read_dir(dir)
            .await
            .with_context(|| format!("Failed to list {}", dir.display()))?;
        while let Some(entry) = entries
            .next_entry()
            .await
            .with_context(|| format!("Failed to list {}", dir.display()))?
        {
            let Some((slice_start, slice_end)) =
                entry.file_name().to_str().and_then(parse_slice_name)
            else {
                continue;
            };
            if let Some(other_end) = slices.insert(slice_start, slice_end) {
                bail!(
                    "{} has two slices with the same start, {} and {}. Use a new folder for a new scan",
                    dir.display(),
                    slice_name(slice_start, other_end)?,
                    slice_name(slice_start, slice_end)?
                );
            }
        }
        let mut end = start;
        while let Some(next) = slices.remove(&end) {
            end = next;
        }
        if let Some((&slice_start, &slice_end)) = slices.iter().min() {
            bail!(
                "{} has slices that do not continue a scan from {}, the earliest being {}. Use a new folder for a new scan",
                dir.display(),
                DateTime::from_timestamp(start, 0)?,
                slice_name(slice_start, slice_end)?
            );
        }
        if end > start {
            info!(
                "Resuming at {}, where the slices in {} from {} end",
                DateTime::from_timestamp(end, 0)?,
                dir.display(),
                DateTime::from_timestamp(start, 0)?
            );
        }
        Ok((
            Self {
                dir: dir.to_owned(),
                _lock: lock,
            },
            end,
        ))
    }

    async fn write(&self, start: i64, end: i64, lines: &str) -> Result<()> {
        let name = slice_name(start, end)?;
        let hidden = self.dir.join(format!(".{name}"));
        let path = self.dir.join(name);
        let written: io::Result<()> = async {
            let mut file = fs::File::create(&hidden).await?;
            file.write_all(lines.as_bytes()).await?;
            // `flush` reports the errors of tokio's background writes, which `sync_all` does not.
            file.flush().await?;
            file.sync_all().await?;
            fs::rename(&hidden, &path).await
        }
        .await;
        written.with_context(|| format!("Failed to write {}", path.display()))
    }
}

fn slice_name(start: i64, end: i64) -> Result<String> {
    let utc = |secs| -> Result<String> {
        Ok(DateTime::from_timestamp(secs, 0)?
            .into_chrono()
            .format(SLICE_TIME_FORMAT)
            .to_string())
    };
    Ok(format!("digests-{}-{}.txt", utc(start)?, utc(end)?))
}

/// The start and end, in seconds, of the slice file `digests-<start>-<end>.txt`.
fn parse_slice_name(name: &str) -> Option<(i64, i64)> {
    let (start, end) = name
        .strip_prefix("digests-")?
        .strip_suffix(".txt")?
        .split_once('-')?;
    let secs = |time| {
        NaiveDateTime::parse_from_str(time, SLICE_TIME_FORMAT)
            .ok()
            .map(|time| time.and_utc().timestamp())
    };
    let (start, end) = (secs(start)?, secs(end)?);
    (start < end).then_some((start, end))
}

/// The commit `bookmark` pointed to at `time`. Log ids grow with time, so this bisects on them:
/// the timestamp-range query scans the log from the newest entry, and times out for times more
/// than a few weeks ago.
async fn bookmark_at(
    ctx: &CoreContext,
    repo: &Repo,
    bookmark: &BookmarkKey,
    time: DateTime,
) -> Result<ChangesetId> {
    let log = repo.bookmark_update_log();
    let timestamp = Timestamp::from(time);
    // Invariant: `found` is the last entry at or before `time` with an id up to `low`, and no
    // entry with an id above `high` is at or before `time`.
    let mut found = None;
    let mut low = 0;
    let mut high = log
        .get_largest_log_id(ctx.clone(), Freshness::MaybeStale)
        .await?
        .unwrap_or(0);
    while low < high {
        let mid = low + (high - low) / 2;
        let next = log
            .read_next_bookmark_log_entries_by_bookmark(
                ctx.clone(),
                bookmark.clone(),
                BookmarkUpdateLogId(mid),
                1,
                Freshness::MaybeStale,
            )
            .try_next()
            .await?;
        match next {
            Some(entry) if entry.id.0 <= high && entry.timestamp <= timestamp => {
                low = entry.id.0;
                found = Some(entry);
            }
            _ => high = mid,
        }
    }
    found
        .ok_or_else(|| anyhow!("Bookmark {bookmark} has no log entry before {time}"))?
        .to_changeset_id
        .ok_or_else(|| anyhow!("Bookmark {bookmark} was deleted at {time}"))
}

impl<C> Scanner<C>
where
    C: CasClient + 'static,
{
    /// The number of commits reachable from `to` but not from `from`, and the parent trees in
    /// them. The first slice also scans `from`: its trees were uploaded before the window and stay
    /// current in the later commits that do not change them.
    async fn scan_slice(
        self: &Arc<Self>,
        from: ChangesetId,
        to: ChangesetId,
        first: bool,
    ) -> Result<(usize, Vec<ParentTree>)> {
        let (ctx, repo) = (&self.ctx, &self.repo);
        let mut commits = repo
            .commit_graph()
            .ancestors_difference(ctx, vec![to], vec![from])
            .await?;
        if first {
            commits.push(from);
        }

        let roots: Vec<Option<RootHgAugmentedManifestId>> = stream::iter(commits.iter().copied())
            .map(|cs_id| async move {
                repo.repo_derived_data()
                    .fetch_derived::<RootHgAugmentedManifestId>(ctx, cs_id)
                    .await
            })
            .buffer_unordered(CONCURRENCY)
            .try_collect()
            .await?;
        let underived = roots.iter().filter(|root| root.is_none()).count();
        if underived > 0 {
            warn!(
                "Skipped {} commits without an augmented manifest; none of their trees were uploaded",
                underived
            );
        }
        let roots: HashSet<HgAugmentedManifestId> = roots
            .into_iter()
            .flatten()
            .map(|root| root.hg_augmented_manifest_id())
            .collect();

        let parents = self.find_parent_trees(roots).await?;
        Ok((commits.len(), parents))
    }

    /// The trees under `roots` that have a restriction root as a direct child. The walk only
    /// enters directories on the way to a restriction root, and visits each tree once per path, as
    /// commits share most trees.
    async fn find_parent_trees(
        self: &Arc<Self>,
        roots: HashSet<HgAugmentedManifestId>,
    ) -> Result<Vec<ParentTree>> {
        let mut parents = Vec::new();
        let mut visited = HashSet::new();
        let mut frontier: Vec<_> = roots.into_iter().map(|id| (MPath::ROOT, id)).collect();
        while !frontier.is_empty() {
            frontier.retain(|tree| visited.insert(tree.clone()));
            // Each visit is a task, as parsing the trees takes more than one core.
            let visits: Vec<_> = stream::iter(frontier)
                .map(|(path, tree_id)| {
                    let visit = mononoke::spawn_task(self.clone().visit(path, tree_id));
                    async move { visit.await? }
                })
                .buffer_unordered(CONCURRENCY)
                .try_collect()
                .await?;
            frontier = Vec::new();
            for (parent, children) in visits {
                parents.extend(parent);
                frontier.extend(children);
            }
        }
        Ok(parents)
    }

    /// The tree as a parent tree if it has a restriction root as a direct child, and its
    /// directories on the way to a restriction root.
    async fn visit(
        self: Arc<Self>,
        path: MPath,
        tree_id: HgAugmentedManifestId,
    ) -> Result<(Option<ParentTree>, Vec<(MPath, HgAugmentedManifestId)>)> {
        let (ctx, repo) = (&self.ctx, &self.repo);
        let envelope = tree_id.load(ctx, repo.repo_blobstore()).await?;
        let manifest =
            HgPreloadedAugmentedManifest::load_from_sharded(envelope, ctx, repo.repo_blobstore())
                .await?;
        let is_parent = self
            .uploader
            .has_restricted_child(ctx, repo, &manifest)
            .await?;
        let children = manifest
            .children_augmented_metadata
            .iter()
            .filter_map(|(name, entry)| match entry {
                HgAugmentedManifestEntry::DirectoryNode(dir) => {
                    let child_path = path.join_element(Some(name));
                    let towards_root = manifest.child_restriction_map.contains_key(name)
                        || self.towards_config_roots.contains(&child_path);
                    towards_root.then(|| (child_path, HgAugmentedManifestId::new(dir.treenode)))
                }
                HgAugmentedManifestEntry::FileNode(_) => None,
            })
            .collect();
        let parent = is_parent.then(|| ParentTree {
            path,
            tree_id,
            digest: MononokeDigest(
                manifest.augmented_manifest_id,
                manifest.augmented_manifest_size,
            ),
        });
        Ok((parent, children))
    }
}

/// Every directory above a config-backed restriction root, past or present. Such a root is
/// restricted by its tree id, which the manifest-id store records with its path. A recorded tree
/// id that reappears at another path is not found.
async fn towards_config_roots(ctx: &CoreContext, repo: &Repo) -> Result<HashSet<MPath>> {
    let entries = repo
        .restricted_paths()
        .config_based()
        .manifest_id_store()
        .get_all_entries(ctx)
        .await?;
    let mut dirs = HashSet::new();
    for entry in entries {
        if !matches!(entry.manifest_type, ManifestType::HgAugmented) {
            continue;
        }
        let mut dir = NonRootMPath::new(&entry.path.0)?.split_dirname().0;
        while let Some(path) = dir {
            dir = path.split_dirname().0;
            dirs.insert(MPath::from(path));
        }
    }
    Ok(dirs)
}
