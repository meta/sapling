/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

mod repo;
mod session;

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::env::args_os;
use std::io;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use anyhow::Error;
use blobstore::Loadable;
use bonsai_tag_mapping::BonsaiTagMappingRef;
use bookmarks::BookmarkCategory;
use bookmarks::BookmarkKind;
use bookmarks::BookmarkPagination;
use bookmarks::BookmarkPrefix;
use bookmarks::BookmarksRef;
use clap::Parser;
use clap::Subcommand;
use clientinfo::ClientEntryPoint;
use clientinfo::ClientInfo;
use cloned::cloned;
use context::CoreContext;
use context::SessionContainer;
use fbinit::FacebookInit;
use futures::TryStreamExt;
use futures::future;
use futures::stream;
use futures_retry::retry;
use git_symbolic_refs::GitSymbolicRefsRef;
use gix_hash::ObjectId;
use import_tools::BackfillDerivation;
use import_tools::BookmarkOperation;
use import_tools::GitImportLfs;
use import_tools::GitRef;
use import_tools::GitRepoReader;
use import_tools::GitUploader;
use import_tools::GitimportPreferences;
use import_tools::GitimportTarget;
use import_tools::LfsServerUrlFormat;
use import_tools::ReuploadCommits;
use import_tools::bookmark::BookmarkOperationErrorReporting;
use import_tools::bookmark::set_bookmarks;
use import_tools::create_changeset_for_annotated_tag;
use import_tools::git_reader::GitReader;
use import_tools::import_tree_as_single_bonsai_changeset;
use import_tools::is_internal_only_ref;
use import_tools::set_bookmark;
use import_tools::upload_git_object;
use import_tools::upload_git_tag;
use import_tools::upload_git_tree_recursively;
use linked_hash_map::LinkedHashMap;
use mercurial_derivation::DeriveHgChangeset;
use mercurial_derivation::get_manifest_from_bonsai;
use metaconfig_types::RepoConfigRef;
use mononoke_api::BookmarkFreshness;
use mononoke_api::BookmarkKey;
use mononoke_api::RepoContext;
use mononoke_api::repo::RepoContextBuilder;
use mononoke_api::repo::git::TagMappingWrite;
use mononoke_app::MononokeApp;
use mononoke_app::MononokeAppBuilder;
use mononoke_app::MononokeReposManager;
use mononoke_app::MononokeService;
use mononoke_app::args::RepoArgs;
use mononoke_app::args::TLSArgs;
use mononoke_app::monitoring::AliveService;
use mononoke_app::monitoring::MonitoringAppExtension;
use mononoke_types::ChangesetId;
use mononoke_types::DerivableType;
use mononoke_types::hash::GitSha1;
#[allow(unused_imports)]
use permission_checker::MononokeIdentity;
use repo_authorization::AuthorizationContext;
use repo_blobstore::RepoBlobstoreArc;
use repo_derived_data::RepoDerivedDataRef;
use repo_identity::RepoIdentityRef;
use restricted_paths::RestrictedPathsArc;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::warn;

use crate::repo::Repo;
use crate::session::SessionFailure;
use crate::session::SessionResultExt;

pub const HEAD_SYMREF: &str = "HEAD";
// Retry policy for the one-shot bulk bookmark listing.
const BOOKMARK_LIST_RETRY_DELAY: Duration = Duration::from_secs(1);
const BOOKMARK_LIST_RETRY_ATTEMPTS: usize = 4;
// Cleanup deletions per bookmark transaction. One transaction per bookmark
// makes a large backlog take hours (a full commit round-trip each); one
// transaction for everything makes a single stale CAS fail the whole batch
// and unboundedly widens the transaction.
const CLEANUP_DELETE_BATCH_SIZE: usize = 100;

// Refactor this a bit. Use a thread pool for git operations. Pass that wherever we use store repo.
// Transform the walk into a stream of commit + file changes.

async fn derive_hg(
    ctx: &CoreContext,
    repo: &(
         impl RepoBlobstoreArc + RepoDerivedDataRef + RepoIdentityRef + RestrictedPathsArc + Send + Sync
     ),
    import_map: impl Iterator<Item = (&gix_hash::ObjectId, &ChangesetId)>,
) -> Result<(), Error> {
    let mut hg_manifests = HashMap::new();

    for (id, bcs_id) in import_map {
        let bcs = bcs_id.load(ctx, repo.repo_blobstore()).await?;
        let parent_manifests = future::try_join_all(bcs.parents().map({
            let hg_manifests = &hg_manifests;
            move |p| async move {
                let manifest = if let Some(manifest) = hg_manifests.get(&p) {
                    *manifest
                } else {
                    repo.derive_hg_changeset(ctx, p)
                        .await?
                        .load(ctx, repo.repo_blobstore())
                        .await?
                        .manifestid()
                };
                Result::<_, Error>::Ok(manifest)
            }
        }))
        .await?;

        let manifest = get_manifest_from_bonsai(
            ctx.clone(),
            repo.repo_blobstore_arc(),
            repo.restricted_paths_arc().config_based().clone(),
            bcs.clone(),
            parent_manifests,
        )
        .await?;

        hg_manifests.insert(*bcs_id, manifest);

        info!("Hg: {:?}: {:?}", id, manifest);
    }

    Ok(())
}

/// URL pattern used by the LFS server to serve a single object by SHA256.
#[derive(Clone, Copy, Debug, Default, clap::ValueEnum)]
enum LfsServerUrlFormatArg {
    /// Dewey-style: `GET {server}/{sha256}`
    #[default]
    Dewey,
    /// Mononoke git-LFS: `GET {server}/{repo}/download_sha256/{sha256}`
    MononokeGitLfs,
}

/// Mononoke Git Importer
#[derive(Parser)]
struct GitimportArgs {
    /// Accept bounded serial requests on stdin while retaining runtime and client caches.
    /// Only supported with the scoped incremental import mode.
    #[clap(long)]
    persistent: bool,
    /// Emit bounded phase names and elapsed milliseconds for import profiling.
    #[clap(long)]
    log_import_phases: bool,
    #[clap(long)]
    derive_hg: bool,
    /// This is used to suppress the printing of the potentially really long git Reference -> BonzaiID mapping.
    #[clap(long)]
    suppress_ref_mapping: bool,
    /// **Dangerous** Generate bookmarks for all git refs (tags and branches)
    /// Make sure not to use this on a mononoke repo in production or you will overwhelm any
    /// service doing backfilling on public changesets!
    /// Use at your own risk!
    #[clap(long)]
    generate_bookmarks: bool,
    /// If set, will skip recording the HEAD symref in Mononoke for the given repo
    #[clap(long)]
    skip_head_symref: bool,
    /// When set, the gitimport tool would bypass the read-only check while creating and moving bookmarks.
    #[clap(long)]
    bypass_readonly: bool,
    /// When set, the gitimport tool would bypass all hooks while creating and moving bookmarks.
    #[clap(long)]
    bypass_all_hooks: bool,
    /// When set, the gitimport tool would bypass the non-fast-forward check while moving bookmarks.
    /// This is useful for re-importing repos where bookmarks may already exist from a prior import.
    #[clap(long)]
    bypass_non_fast_forward: bool,
    /// The concurrency to be used while importing commits in Mononoke
    #[clap(long, default_value_t = 20)]
    concurrency: usize,
    /// Set the path to the git binary - preset to git.real
    #[clap(long)]
    git_command_path: Option<String>,
    /// Path to a git repository to import
    git_repository_path: String,
    /// Reupload git commits, even if they already exist in Mononoke
    #[clap(long)]
    reupload_commits: bool,
    #[clap(subcommand)]
    subcommand: GitimportSubcommand,
    #[clap(flatten)]
    repo_args: RepoArgs,
    /// Discard any git submodule during import.
    /// **WARNING**: This will make the repo import lossy: round trip between Mononoke and git won't be
    /// possible anymore.
    /// In particular, this is not suitable as a precursor step to setting up live sync with
    /// Mononoke.
    /// Only use if you are sure that's what you want.
    #[clap(long)]
    discard_submodules: bool,
    /// Allow the import of repos with refs pointing to blobs or trees
    #[clap(long)]
    allow_content_refs: bool,
    /// Don't backfill derived data during this import.
    /// This is dangerous if used in conjunction with generate_bookmarks as public
    /// commits which are not derived may create high load for the derived data service
    #[clap(long)]
    bypass_derived_data_backfilling: bool,
    /// The refs to exclude while importing the repo. Can be used to skip cross-synced refs to avoid
    /// race condition with live gitimport
    #[clap(long, use_value_delimiter = true, value_delimiter = ',')]
    exclude_refs: Vec<String>,
    /// The refs to be included while importing the repo. When provided, gitimport will only import the
    /// explicitly specified refs
    #[clap(long, use_value_delimiter = true, value_delimiter = ',')]
    include_refs: Vec<String>,
    /// LFS server URL to fetch LFS files from over HTTP. When unset, gitimport
    /// falls back to internal mode (resolves LFS pointers from the local
    /// Mononoke filestore by SHA256 alias). Mutually exclusive with
    /// `--internal-lfs` and `--github-lfs-url`.
    #[clap(long, conflicts_with_all = ["internal_lfs", "github_lfs_url"])]
    lfs_server: Option<String>,
    /// URL pattern that the LFS server uses to serve raw objects by SHA256.
    /// Defaults to the Dewey-style `GET {server}/{sha256}`. Use `mononoke-git-lfs` for
    /// the `GET {server}/{repo}/download_sha256/{sha256}` shape served by Mononoke LFS.
    #[clap(long, value_enum, default_value_t = LfsServerUrlFormatArg::Dewey)]
    lfs_server_url_format: LfsServerUrlFormatArg,
    /// Explicitly request internal mode (resolve LFS pointers from the local
    /// Mononoke filestore by SHA256 alias). Internal mode is also the default
    /// when `--lfs-server` is not set; this flag is mainly useful for
    /// documentation or to force a clap error if `--lfs-server` is also
    /// passed by mistake. Mutually exclusive with `--lfs-server` and
    /// `--github-lfs-url`.
    #[clap(long, default_value_t = false, conflicts_with = "github_lfs_url")]
    internal_lfs: bool,
    /// GitHub LFS Batch API endpoint, e.g.
    /// `https://github.com/par-msl/jarvis.git/info/lfs/objects/batch`. When
    /// set, gitimport speaks the LFS Batch protocol against this URL using a
    /// GitHub App installation token read from `--github-lfs-token-file`.
    /// Mutually exclusive with `--lfs-server` and `--internal-lfs`.
    #[clap(long, requires = "github_lfs_token_file")]
    github_lfs_url: Option<String>,
    /// Path to a file containing the GitHub App installation token used as
    /// `Authorization: Bearer <token>` against the GitHub LFS Batch endpoint.
    /// The file is re-read after a 401/403 response, so an out-of-process
    /// refresher can rotate the token while gitimport is running
    /// (installation tokens expire after ~1h; large imports take several
    /// hours). Required when `--github-lfs-url` is set.
    #[clap(long, requires = "github_lfs_url")]
    github_lfs_token_file: Option<PathBuf>,
    /// HTTP forward proxy URL used to tunnel GitHub LFS Batch API requests
    /// through via HTTP CONNECT. Defaults to Meta's prod forward proxy
    /// (`http://fwdproxy:8080`), which is required to reach github.com from
    /// any Meta host (Sandcastle workers, devservers, OD sandboxes). Pair
    /// with `--github-lfs-no-https-proxy` to disable proxying entirely
    /// (e.g. when running outside Meta in OSS environments where github.com
    /// is reachable directly). Only applies to the `--github-lfs-url` code
    /// path; the other LFS modes ignore it.
    #[clap(
        long,
        requires = "github_lfs_url",
        default_value = "http://fwdproxy:8080"
    )]
    github_lfs_https_proxy: String,
    /// Disable HTTPS proxying for GitHub LFS Batch requests, overriding the
    /// default `--github-lfs-https-proxy`. Use when running outside Meta
    /// (OSS) where github.com is reachable directly without a forward proxy.
    #[clap(long, requires = "github_lfs_url")]
    github_lfs_no_https_proxy: bool,
    /// TLS parameters for this service used for outbound LFS connections
    #[clap(flatten)]
    tls_args: Option<TLSArgs>,
    /// If LFS file can't be obtained from LFS server, don't fail the import
    /// but import the pointer as-is.
    #[clap(long)]
    allow_dangling_lfs_pointers: bool,
    /// How many times to retry fetching LFS files from the server
    /// before deciding that the file is missing.
    #[clap(long, default_value_t = 5)]
    lfs_import_max_attempts: u32,
    /// Maximum number of LFS objects downloaded from the LFS server at the
    /// same time. Lower it for repos with many multi-hundred-MB objects so
    /// each transfer finishes before the forward proxy drops it.
    #[clap(long, default_value_t = 20)]
    lfs_concurrency: usize,
    /// If any bookmarks were present in Mononoke but are not present in Git, delete them in
    /// Mononoke.
    /// This is necessary for a catch-up import situation if a Git branch was deleted between both
    /// imports.
    #[clap(long)]
    cleanup_mononoke_bookmarks: bool,
    /// On failure, persist `bonsai_git_mapping` rows for commits already
    /// fully processed so retries can resume incrementally. Also surfaces
    /// the real underlying error to Scuba instead of the SendError
    /// cascade. See `GitimportPreferences::persist_partial_mappings`.
    #[clap(long)]
    persist_partial_mappings: bool,
}

#[derive(Subcommand)]
enum GitimportSubcommand {
    /// Import all of the commits in this repo
    FullRepo,
    /// Import missing history of the exact --include-refs selection, then reconcile
    /// those refs. Does not update HEAD or support bookmark cleanup/reupload.
    Incremental,
    /// Import all commits between <GIT_FROM> and <GIT_TO>
    GitRange {
        git_from: String,
        git_to: String,
    },
    /// Import <GIT_COMMIT> and all its history that's not yet been imported.
    /// Makes a pass over the repo on construction to find missing history
    MissingForCommit {
        git_commit: String,
    },
    ImportTreeAsSingleBonsaiChangeset {
        git_commit: String,
    },
    /// Import a single tag object into Mononoke data store
    UploadTags {
        tag_sha1s: Vec<String>,
    },
}

#[fbinit::main]
fn main(fb: FacebookInit) -> Result<(), Error> {
    let started = Instant::now();
    // Inspect only this opt-in flag before app setup; clap still parses and
    // validates the complete command line below.
    let log_import_phases = args_os()
        .skip(1)
        .take_while(|arg| arg != "--")
        .any(|arg| arg == "--log-import-phases");
    if log_import_phases {
        write_import_phase("main_entered", 0);
    }
    let lifecycle_timing = Arc::new(ImportLifecycleTiming::default());
    let app = MononokeAppBuilder::new(fb)
        .with_default_service(MononokeService::GITIMPORT)
        .with_app_extension(MonitoringAppExtension {})
        .build::<GitimportArgs>()?;

    let result = app.run_with_monitoring_and_logging(
        {
            let lifecycle_timing = lifecycle_timing.clone();
            move |app| {
                let lifecycle_timing = lifecycle_timing.clone();
                async move {
                    let result = async_main(app, started, &lifecycle_timing).await;
                    lifecycle_timing.log_async_cleanup(log_import_phases);
                    result
                }
            }
        },
        "gitimport",
        AliveService,
    );
    lifecycle_timing.log_runtime_shutdown(log_import_phases);

    if result.is_ok() {
        // Skip C++ singleton teardown which hangs for ~5 minutes due to
        // circular shared_ptr references in folly/scribe/manifold singletons.
        // All import work is complete at this point.
        //
        // Must use libc::_exit instead of std::process::exit because the latter
        // calls C exit() which still runs atexit handlers — and folly registers
        // singleton teardown via atexit, which is the exact code path that hangs.
        unsafe { libc::_exit(0) };
    }

    result
}

#[derive(Default)]
struct ImportLifecycleTiming {
    async_cleanup_started: OnceLock<Instant>,
    runtime_shutdown_started: OnceLock<Instant>,
}

impl ImportLifecycleTiming {
    fn log_async_cleanup(&self, enabled: bool) {
        if let Some(mut started) = self.async_cleanup_started.get().copied() {
            log_import_phase(enabled, "async_cleanup", &mut started);
            let _ = self.runtime_shutdown_started.set(started);
        }
    }

    fn log_runtime_shutdown(&self, enabled: bool) {
        if let Some(mut started) = self.runtime_shutdown_started.get().copied() {
            log_import_phase(enabled, "runtime_shutdown", &mut started);
        }
    }
}

fn write_import_phase(phase: &str, duration_ms: u128) {
    let mut output = io::stdout().lock();
    let _ = writeln!(
        output,
        "gitimport_phase phase={phase} duration_ms={duration_ms}"
    );
    let _ = output.flush();
}

fn log_import_phase(enabled: bool, phase: &str, started: &mut Instant) {
    if enabled {
        write_import_phase(phase, started.elapsed().as_millis());
    }
    *started = Instant::now();
}

async fn selected_incremental_refs(
    path: &Path,
    prefs: &GitimportPreferences,
    include_refs: &[String],
) -> Result<BTreeMap<GitRef, ObjectId>, Error> {
    let refs = import_tools::read_git_refs(path, prefs).await?;
    let selected = refs
        .into_iter()
        .filter(|(git_ref, _)| {
            include_refs
                .iter()
                .any(|name| name.as_bytes() == git_ref.name)
        })
        .collect::<BTreeMap<_, _>>();
    for name in include_refs {
        let git_ref = selected
            .keys()
            .find(|git_ref| git_ref.name == name.as_bytes())
            .with_context(|| format!("Selected ref does not exist: {name}"))?;
        anyhow::ensure!(
            !git_ref.metadata.target.is_content(),
            "Incremental import does not support content ref {name}"
        );
        anyhow::ensure!(
            !is_internal_only_ref(&git_ref.name) && name != "refs/heads/HEAD",
            "Incremental import does not support internal ref {name}"
        );
    }
    Ok(selected)
}

async fn async_main(
    app: MononokeApp,
    mut phase_started: Instant,
    lifecycle_timing: &ImportLifecycleTiming,
) -> Result<(), Error> {
    let args: GitimportArgs = app.args()?;
    log_import_phase(args.log_import_phases, "startup", &mut phase_started);
    if args.persistent {
        return session::run(&app, &args, phase_started, lifecycle_timing)
            .await
            .map_err(|error| {
                error!("Persistent import session stopped: {error:?}");
                session::sanitize_error(error)
            });
    }
    run_import(
        &app,
        &args,
        phase_started,
        ImportExecution {
            expected_refs: None,
            lifecycle_timing,
            session_manager: None,
        },
    )
    .await
}

struct ImportExecution<'a> {
    expected_refs: Option<&'a BTreeMap<String, String>>,
    lifecycle_timing: &'a ImportLifecycleTiming,
    session_manager: Option<&'a mut Option<MononokeReposManager<mononoke_api::Repo>>>,
}

async fn run_import(
    app: &MononokeApp,
    args: &GitimportArgs,
    mut phase_started: Instant,
    execution: ImportExecution<'_>,
) -> Result<(), Error> {
    let incremental = matches!(&args.subcommand, GitimportSubcommand::Incremental);
    if incremental {
        anyhow::ensure!(
            !args.include_refs.is_empty(),
            "Incremental import requires --include-refs"
        );
        anyhow::ensure!(
            !args.cleanup_mononoke_bookmarks && !args.reupload_commits,
            "Incremental import does not support bookmark cleanup or reupload"
        );
        anyhow::ensure!(
            args.include_refs
                .iter()
                .all(|name| !args.exclude_refs.contains(name)),
            "Incremental import cannot include and exclude the same ref"
        );
    }

    let ctx = {
        let mut metadata = metadata::Metadata::default();
        metadata.add_client_info(ClientInfo::default_with_entry_point(
            ClientEntryPoint::GitImport,
        ));

        // When bypassing hooks, we need real identities from the local machine's
        // certificate so that the hook bypass authorization check can validate
        // that this caller is permitted to bypass hooks.
        #[cfg(fbcode_build)]
        if args.bypass_all_hooks {
            let local_idents = identity_ext::x509::get_locally_available_identities()
                .context(
                    "Failed to read local certificate identities. \
                     --bypass-all-hooks requires valid identities from the machine's \
                     x509 certificate (THRIFT_TLS_CL_CERT_PATH)",
                )
                .session_context(args.persistent, SessionFailure::Identity)?;
            let identities = local_idents
                .iter()
                .map(MononokeIdentity::from_identity)
                .collect();
            metadata = metadata.set_identities(identities);
        }

        let session = SessionContainer::builder(app.fb)
            .metadata(Arc::new(metadata))
            .build();
        session.new_context(scuba_ext::MononokeScubaSampleBuilder::with_discard())
    };

    let path = Path::new(&args.git_repository_path);

    let reupload = if args.reupload_commits {
        ReuploadCommits::Always
    } else {
        ReuploadCommits::Never
    };

    let repo: Repo = app
        .open_repo(&args.repo_args)
        .await
        .session_context(args.persistent, SessionFailure::OpenRepo)?;
    log_import_phase(args.log_import_phases, "open_repo", &mut phase_started);
    info!(
        "using repo \"{}\" repoid {:?}",
        repo.repo_identity().name(),
        repo.repo_identity().id(),
    );

    let backfill_derivation = if args.bypass_derived_data_backfilling {
        if args.generate_bookmarks {
            warn!(
                "Warning: gitimport was called bypassing derived data backfilling while generating bookmarks.\nIt is your responsibility to ensure that all derived data types are backfilled before this repository is expose to prod to avoid the risk of overloading the derived data service."
            );
        }
        BackfillDerivation::No
    } else if args.discard_submodules {
        let configured_types = &repo.repo_derived_data().active_config().types;
        BackfillDerivation::OnlySpecificTypes(
            configured_types
                .iter()
                .filter(|ty| match ty {
                    // If we discard submodules, we can't derive the git data types since they are inconsistent
                    DerivableType::GitCommits
                    | DerivableType::GitDeltaManifestsV2
                    | DerivableType::GitDeltaManifestsV3 => false,
                    _ => true,
                })
                .cloned()
                .collect(),
        )
    } else {
        BackfillDerivation::AllConfiguredTypes
    };
    // LFS resolution mode (only relevant when the repo config has LFS pointer
    // interpretation enabled). Internal mode is the default — pass
    // `--lfs-server URL` to opt into upstream HTTP fetches. The `--internal-lfs`
    // flag is documentary only; without it, the absence of `--lfs-server`
    // already selects internal mode.
    let lfs = if repo.repo_config().git_configs.git_lfs_interpret_pointers {
        match (&args.lfs_server, &args.github_lfs_url) {
            (Some(lfs_server), None) => {
                let url_format = match args.lfs_server_url_format {
                    LfsServerUrlFormatArg::Dewey => LfsServerUrlFormat::LegacyDewey,
                    LfsServerUrlFormatArg::MononokeGitLfs => LfsServerUrlFormat::MononokeGitLfs {
                        repo_name: repo.repo_identity().name().to_string(),
                    },
                };
                GitImportLfs::new(
                    lfs_server.clone(),
                    url_format,
                    args.allow_dangling_lfs_pointers,
                    args.lfs_import_max_attempts,
                    Some(args.lfs_concurrency),
                    args.tls_args.clone(),
                )
                .session_context(args.persistent, SessionFailure::LfsSetup)?
            }
            (None, Some(github_lfs_url)) => {
                let token_file = args
                    .github_lfs_token_file
                    .clone()
                    .ok_or_else(|| {
                        anyhow::format_err!(
                            "--github-lfs-token-file is required when --github-lfs-url is set",
                        )
                    })
                    .session_context(args.persistent, SessionFailure::LfsSetup)?;
                let https_proxy = if args.github_lfs_no_https_proxy {
                    None
                } else {
                    Some(args.github_lfs_https_proxy.clone())
                };
                GitImportLfs::new_github(
                    github_lfs_url.clone(),
                    token_file,
                    https_proxy,
                    args.allow_dangling_lfs_pointers,
                    args.lfs_import_max_attempts,
                    Some(args.lfs_concurrency),
                )
                .session_context(args.persistent, SessionFailure::LfsSetup)?
            }
            (None, None) => GitImportLfs::new_internal(
                repo.repo_blobstore_arc().boxed(),
                args.allow_dangling_lfs_pointers,
            ),
            (Some(_), Some(_)) => unreachable!("clap conflicts_with rejects this combination"),
        }
    } else {
        GitImportLfs::new_disabled()
    };

    let mut prefs = GitimportPreferences {
        concurrency: args.concurrency,
        submodules: !args.discard_submodules,
        allow_content_refs: args.allow_content_refs,
        backfill_derivation,
        lfs,
        persist_partial_mappings: args.persist_partial_mappings,
        ..Default::default()
    };

    if let Some(path) = &args.git_command_path {
        prefs.git_command_path = PathBuf::from(path);
    }

    let uploader = Arc::new(import_direct::DirectUploader::new(repo.clone(), reupload));
    let reader = Arc::new(
        GitRepoReader::new(&prefs.git_command_path, path)
            .await
            .session_context(args.persistent, SessionFailure::ReadGit)?,
    );
    let selected_refs = match execution.expected_refs {
        Some(refs) => refs.keys().cloned().collect::<Vec<_>>(),
        None => args.include_refs.clone(),
    };
    let incremental_refs = if incremental {
        let refs = selected_incremental_refs(path, &prefs, &selected_refs)
            .await
            .session_context(args.persistent, SessionFailure::ReadGit)?;
        if let Some(expected) = execution.expected_refs {
            session::check_ref_snapshot(&refs, expected)?;
        }
        Some(refs)
    } else {
        None
    };
    let target = match &args.subcommand {
        GitimportSubcommand::FullRepo => GitimportTarget::full(),
        GitimportSubcommand::Incremental => {
            let commits = incremental_refs
                .as_ref()
                .context("Incremental refs must be resolved before importing")?
                .values()
                .copied()
                .collect::<Vec<_>>();
            import_direct::missing_for_commits(&commits, &ctx, &repo, &prefs.git_command_path, path)
                .await
                .session_context(args.persistent, SessionFailure::DiscoverCommits)?
        }
        GitimportSubcommand::GitRange { git_from, git_to } => {
            let from = git_from.parse()?;
            let to = git_to.parse()?;
            import_direct::range(from, to, &ctx, &repo).await?
        }
        GitimportSubcommand::MissingForCommit { git_commit } => {
            let commit = git_commit.parse()?;
            import_direct::missing_for_commit(commit, &ctx, &repo, &prefs.git_command_path, path)
                .await?
        }
        GitimportSubcommand::ImportTreeAsSingleBonsaiChangeset { git_commit } => {
            let commit = git_commit.parse()?;
            let bcs_id = import_tree_as_single_bonsai_changeset(
                &ctx,
                path,
                uploader.clone(),
                commit,
                &prefs,
            )
            .await?;
            info!("imported as {}", bcs_id);
            if args.derive_hg {
                derive_hg(&ctx, &repo, [(&commit, &bcs_id)].into_iter()).await?;
            }
            return Ok(());
        }
        GitimportSubcommand::UploadTags { tag_sha1s } => {
            stream::iter(tag_sha1s.iter().map(Ok))
                .map_ok(|tag_sha1| {
                    cloned!(ctx, uploader, reader);
                    async move {
                        let tag_sha1 = ObjectId::from_str(tag_sha1).with_context(|| {
                            format!("Invalid SHA1 hash provided for Git Tag {tag_sha1}")
                        })?;
                        upload_git_tag(&ctx, uploader.clone(), reader.clone(), &tag_sha1)
                            .await
                            .with_context(|| {
                                format!("Error in uploading tag with ID {tag_sha1}")
                            })?;
                        info!("Uploaded tag with ID {}", tag_sha1);
                        anyhow::Ok(())
                    }
                })
                .try_buffer_unordered(100)
                .try_collect::<Vec<_>>()
                .await?;

            return Ok(());
        }
    };
    log_import_phase(
        args.log_import_phases,
        "discover_commits",
        &mut phase_started,
    );
    let mut gitimport_result: LinkedHashMap<_, _> =
        import_tools::gitimport(&ctx, path, uploader.clone(), &target, &prefs)
            .await
            .session_context(args.persistent, SessionFailure::ImportContents)
            .context("gitimport failed")?;
    log_import_phase(
        args.log_import_phases,
        "import_contents",
        &mut phase_started,
    );
    if let Some(expected) = execution.expected_refs {
        let current_refs = selected_incremental_refs(path, &prefs, &selected_refs)
            .await
            .session_context(args.persistent, SessionFailure::ReadGit)?;
        session::check_ref_snapshot(&current_refs, expected)?;
    }
    if let Some(refs) = &incremental_refs {
        let commits = refs.values().copied().collect::<Vec<_>>();
        gitimport_result.extend(
            uploader
                .preload_uploaded_commits(&ctx, &commits)
                .await
                .session_context(args.persistent, SessionFailure::ResolveRefs)?,
        );
        for commit in commits {
            if !gitimport_result.contains_key(&commit) {
                return Err(anyhow::anyhow!(
                    "Selected ref target was not imported: {commit}"
                ))
                .session_context(args.persistent, SessionFailure::ResolveRefs);
            }
        }
    }
    if args.derive_hg {
        derive_hg(&ctx, &repo, gitimport_result.iter())
            .await
            .context("derive_hg failed")?;
    }
    if !args.skip_head_symref && !incremental {
        let symref_entry = import_tools::read_symref(HEAD_SYMREF, path, &prefs)
            .await
            .context("read_symrefs failed")?;
        repo.git_symbolic_refs()
            .add_or_update_entries(&ctx, vec![symref_entry])
            .await
            .context("failed to add symbolic ref entries")?;
    }
    if !args.suppress_ref_mapping || args.generate_bookmarks {
        let refs = match incremental_refs {
            Some(refs) => refs,
            None => import_tools::read_git_refs(path, &prefs)
                .await
                .context("read_git_refs failed")?,
        };
        let git_ref_mapping = refs
            .into_iter()
            // Filtered here rather than via --exclude-refs because
            // --cleanup-mononoke-bookmarks builds its keep-set from the
            // unfiltered mapping: excluding later would strand a bookmark an
            // earlier import created. Same shape as the heads/HEAD skip below.
            .filter(|(git_ref, _)| !is_internal_only_ref(&git_ref.name))
            .map(|(git_ref, commit)| {
                Ok((
                    git_ref.metadata,
                    String::from_utf8(git_ref.name).map_err(|err| {
                        anyhow::anyhow!(
                            "Failed to parse git ref name {:?} due to invalid UTF-8 encoding, Cause: {}",
                            err.as_bytes(),
                            err.utf8_error()
                        )
                    })?,
                    gitimport_result.get(&commit),
                ))
            })
            .collect::<anyhow::Result<Vec<_>>>()
            .session_context(args.persistent, SessionFailure::ResolveRefs)?;
        if !args.suppress_ref_mapping {
            for (_, name, changeset) in &git_ref_mapping {
                info!("Ref: {:?}: {:?}", name, changeset);
            }
        }
        if args.generate_bookmarks {
            log_import_phase(args.log_import_phases, "resolve_refs", &mut phase_started);
            let authz = AuthorizationContext::new_bypass_access_control();
            let mut one_shot_manager = None;
            let manager_slot = execution.session_manager.unwrap_or(&mut one_shot_manager);
            if let Some(manager) = manager_slot.as_ref() {
                manager
                    .add_repo(repo.repo_identity().name())
                    .await
                    .session_context(args.persistent, SessionFailure::OpenManagedRepo)?;
            } else {
                *manager_slot = Some(
                    app.open_managed_repo_arg::<mononoke_api::Repo>(&args.repo_args)
                        .await
                        .context("failed to create mononoke app")
                        .session_context(args.persistent, SessionFailure::OpenManagedRepo)?,
                );
            }
            let repos_manager = manager_slot
                .as_ref()
                .context("selected repository manager was not initialized")
                .session_context(args.persistent, SessionFailure::OpenManagedRepo)?;
            let repo_context_builder = if incremental {
                let bookmark_repo = repos_manager
                    .repos()
                    .get_by_id(repo.repo_identity().id().id())
                    .context("selected repository was not initialized")
                    .session_context(args.persistent, SessionFailure::OpenManagedRepo)?;
                RepoContextBuilder::new(ctx.clone(), bookmark_repo, repos_manager.repos().clone())
                    .await
                    .session_context(args.persistent, SessionFailure::OpenManagedRepo)?
            } else {
                repos_manager
                    .make_mononoke_api()?
                    .repo_by_id(ctx.clone(), repo.repo_identity().id())
                    .await
                    .with_context(|| {
                        format!("failed to access repo: {}", repo.repo_identity().id())
                    })?
                    .expect("repo exists")
            };
            let repo_context: RepoContext<mononoke_api::Repo> = repo_context_builder
                .with_authorization_context(authz)
                .build()
                .await
                .context("failed to build RepoContext")
                .session_context(args.persistent, SessionFailure::OpenManagedRepo)?;
            log_import_phase(
                args.log_import_phases,
                "open_managed_repo",
                &mut phase_started,
            );
            let existing_tags = repo
                .bonsai_tag_mapping()
                .get_all_entries(&ctx)
                .await
                .context("Failed to fetch bonsai tag mapping")
                .session_context(args.persistent, SessionFailure::Publication)?
                .into_iter()
                .map(|entry| (entry.tag_name, entry.tag_hash))
                .collect::<HashMap<_, _>>();
            let pushvars = {
                let mut pvs = HashMap::new();
                if args.bypass_readonly {
                    pvs.insert("BYPASS_READONLY".to_string(), bytes::Bytes::from("true"));
                }
                if args.bypass_all_hooks {
                    pvs.insert("BYPASS_ALL_HOOKS".to_string(), bytes::Bytes::from("true"));
                }
                if args.bypass_non_fast_forward {
                    pvs.insert(
                        "x-git-allow-non-ffwd-push".to_string(),
                        bytes::Bytes::from("true"),
                    );
                }
                if pvs.is_empty() { None } else { Some(pvs) }
            };
            // We can make the below loop concurrent but since refs pointing to content is an anomaly,
            // we will only optimize its upload if we see a need.
            for (ref_metadata, content_ref_name, _) in git_ref_mapping
                .iter()
                .filter(|(ref_metadata, _, _)| ref_metadata.target.is_content())
            {
                let ref_name = content_ref_name
                    .strip_prefix("refs/")
                    .context("Ref does not start with refs/")?
                    .to_string();
                if let Some(tag_id) = ref_metadata.maybe_tag_id {
                    // If the ref pointing to tree or blob is a tag, then we need to upload the tag before
                    // storing the git ref content mapping.
                    process_tags(
                        &ctx,
                        &ref_name,
                        tag_id,
                        &existing_tags,
                        None, // No changeset for content refs
                        reupload,
                        uploader.clone(),
                        reader.clone(),
                    )
                    .await?;
                }
                let is_tree = ref_metadata.target.is_tree();
                let git_hash = ref_metadata.target.into_inner_hash()?;
                // Before adding the git ref content mapping, ensure that the object pointed to by
                // the content ref is already present in Mononoke
                if is_tree {
                    // The object pointed at is a tree. Ensure that all the members of the tree are uploaded
                    // recursively
                    upload_git_tree_recursively(&ctx, uploader.clone(), reader.clone(), &git_hash)
                        .await?;
                } else {
                    upload_git_object(&ctx, uploader.clone(), reader.clone(), &git_hash).await?;
                }
                uploader
                    .generate_ref_content_mapping(&ctx, ref_name.to_string(), git_hash, is_tree)
                    .await?;
            }

            // One bulk snapshot instead of a resolve_bookmark per ref, reused by
            // cleanup. u64::MAX matches the old cleanup listing; BTreeMap = stable order.
            let (existing_bookmarks_list, _) = retry(
                |_| {
                    repo.bookmarks()
                        .list(
                            ctx.clone(),
                            BookmarkFreshness::MostRecent,
                            &BookmarkPrefix::empty(),
                            BookmarkCategory::ALL,
                            BookmarkKind::ALL,
                            &BookmarkPagination::FromStart,
                            u64::MAX,
                        )
                        .try_collect::<Vec<_>>()
                },
                BOOKMARK_LIST_RETRY_DELAY,
            )
            .binary_exponential_backoff()
            .max_attempts(BOOKMARK_LIST_RETRY_ATTEMPTS)
            .inspect_err(|attempt, _err| {
                info!(
                    "attempt {attempt} of {BOOKMARK_LIST_RETRY_ATTEMPTS} to list bookmarks failed"
                )
            })
            .await
            .context("failed to list existing bookmarks")
            .session_context(args.persistent, SessionFailure::Publication)?;
            let existing_bookmarks: BTreeMap<BookmarkKey, ChangesetId> = existing_bookmarks_list
                .into_iter()
                .map(|(bookmark, cs_id)| (bookmark.key().clone(), cs_id))
                .collect();
            debug!(
                "Fetched {} existing bookmarks for reconciliation",
                existing_bookmarks.len()
            );

            for (maybe_tag_id, name, changeset) in
                git_ref_mapping
                    .iter()
                    .filter_map(|(ref_metadata, name, changeset)| {
                        // Exclude the ref if its specified in the exclude-list OR if its not explicitly specified in the include-list (if exists)
                        let exclude_ref = args.exclude_refs.contains(name)
                            || !(args.include_refs.is_empty() || args.include_refs.contains(name));
                        if exclude_ref {
                            None
                        } else {
                            changeset.map(|cs| (ref_metadata.maybe_tag_id, name, cs))
                        }
                    })
            {
                let final_changeset = changeset.clone();
                let name = name
                    .strip_prefix("refs/")
                    .context("Ref does not start with refs/")?
                    .to_string();
                if name.as_str() == "heads/HEAD" {
                    // Skip the HEAD revision: it shouldn't be imported as a bookmark in mononoke
                    continue;
                }
                match maybe_tag_id {
                    // Keyed on the tag object, not the bookmark target: always run.
                    Some(tag_id) => {
                        process_tags(
                            &ctx,
                            &name,
                            tag_id,
                            &existing_tags,
                            Some(*changeset),
                            reupload,
                            uploader.clone(),
                            reader.clone(),
                        )
                        .await
                        .session_context(args.persistent, SessionFailure::Publication)?;
                    }
                    // A lightweight tag that replaced an annotated one. The git
                    // server advertises the mapping's tag object over the
                    // bookmark, so the stale row must go even when the bookmark
                    // itself is already correct.
                    None if existing_tags.contains_key(&name) => {
                        repo.bonsai_tag_mapping()
                            .delete_mappings_by_name(&ctx, vec![name.clone()])
                            .await
                            .with_context(|| {
                                format!("failed to delete stale tag mapping for {name}")
                            })
                            .session_context(args.persistent, SessionFailure::Publication)?;
                    }
                    None => {}
                }
                let bookmark_key = BookmarkKey::new(&name)
                    .session_context(args.persistent, SessionFailure::Publication)?;

                // Unchanged per snapshot: skip both read and write.
                if existing_bookmarks.get(&bookmark_key) == Some(&final_changeset) {
                    continue;
                }
                // Re-read fresh so the CAS old-target is current under concurrent writers.
                let old_changeset = repo_context
                    .resolve_bookmark(&bookmark_key, BookmarkFreshness::MostRecent)
                    .await
                    .with_context(|| format!("failed to resolve bookmark {name}"))
                    .session_context(args.persistent, SessionFailure::Publication)?
                    .map(|context| context.id());
                if old_changeset.as_ref() == Some(&final_changeset) {
                    continue;
                }
                let allow_non_fast_forward = true;
                let operation =
                    BookmarkOperation::new(bookmark_key, old_changeset, Some(final_changeset))
                        .session_context(args.persistent, SessionFailure::Publication)?;
                set_bookmark(
                    &ctx,
                    &repo_context,
                    &operation,
                    pushvars.as_ref(),
                    allow_non_fast_forward,
                    BookmarkOperationErrorReporting::WithContext,
                    None,
                )
                .await
                .session_context(args.persistent, SessionFailure::Publication)?;
            }
            if args.cleanup_mononoke_bookmarks {
                // From the full ref mapping, not the filtered generate set, else
                // excluded/content refs still in git would be deleted.
                let git_ref_names: HashSet<String> = git_ref_mapping
                    .iter()
                    .filter_map(|(_, git_ref, _)| git_ref.strip_prefix("refs/"))
                    .filter(|ref_name| *ref_name != "heads/HEAD")
                    .map(str::to_owned)
                    .collect();
                // Each operation is paired with its log line up front; the line
                // is emitted only after its batch's transaction commits, so a
                // logged deletion is always a durable one.
                let delete_operations = existing_bookmarks
                    .iter()
                    .filter(|(bookmark_key, _)| {
                        !git_ref_names.contains(bookmark_key.name().as_str())
                    })
                    .map(|(bookmark_key, old_changeset)| {
                        anyhow::Ok((
                            BookmarkOperation::new(
                                bookmark_key.clone(),
                                Some(*old_changeset),
                                None,
                            )?,
                            format!(
                                "Bookmark: \"{}\": {:?} (deleted)",
                                bookmark_key.name(),
                                old_changeset
                            ),
                        ))
                    })
                    .collect::<Result<Vec<_>, _>>()?;

                let allow_non_fast_forward = true;
                let mut remaining = delete_operations.into_iter().peekable();
                while remaining.peek().is_some() {
                    let (batch, batch_log_lines): (Vec<_>, Vec<_>) =
                        remaining.by_ref().take(CLEANUP_DELETE_BATCH_SIZE).unzip();
                    set_bookmarks(
                        &ctx,
                        &repo_context,
                        batch,
                        pushvars.as_ref(),
                        allow_non_fast_forward,
                        BookmarkOperationErrorReporting::WithContext,
                        None,
                        vec![],
                    )
                    .await?;
                    for line in batch_log_lines {
                        info!("{line}");
                    }
                }
            }
        };
    }
    log_import_phase(args.log_import_phases, "publication", &mut phase_started);
    let _ = execution
        .lifecycle_timing
        .async_cleanup_started
        .set(phase_started);
    Ok(())
}

async fn process_tags<Uploader: GitUploader, Reader: GitReader>(
    ctx: &CoreContext,
    name: &String,
    tag_id: ObjectId,
    existing_tags: &HashMap<String, GitSha1>,
    changeset: Option<ChangesetId>,
    reupload: ReuploadCommits,
    uploader: Arc<Uploader>,
    reader: Arc<Reader>,
) -> anyhow::Result<()> {
    let new_or_updated_tag = existing_tags.get(name).is_none_or(|tag_hash| {
        if let Ok(new_hash) = GitSha1::from_object_id(&tag_id) {
            *tag_hash != new_hash
        } else {
            false
        }
    });
    // Only upload the tag if it's new or has changed.
    if new_or_updated_tag || reupload.reupload_commit() {
        // The ref getting imported is a tag, so store the raw git Tag object.
        upload_git_tag(ctx, uploader.clone(), reader.clone(), &tag_id).await?;
        // Create the changeset corresponding to the commit pointed to by the tag.
        create_changeset_for_annotated_tag(
            ctx,
            uploader.clone(),
            reader.clone(),
            &tag_id,
            Some(name.clone()),
            changeset,
            TagMappingWrite::Inline,
        )
        .await?;
    }
    Ok(())
}
