/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Push diversion to the Multi-Repo Land Service.
//!
//! When a push targets a repo whose name contains the configured marker
//! (`rl_land_service_repo_prefix` in CommonConfig, a substring match),
//! branch creates and moves land through one synchronous
//! `submit_manifest_land` call; other refs (deletes, tags, non-branch
//! refs) take the normal git server path. Per-branch enablement is owned
//! entirely by the server's `scm/mononoke:sslv2_enabled` gate.
//!
//! `#[cfg(fbcode_build)]`: the Thrift client is fbcode-only.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use bookmarks::BookmarkKey;
use bookmarks_movement::BookmarkKindRestrictions;
use bytes::Bytes;
use context::CoreContext;
use multi_repo_land_if::MemberRefUpdate;
use multi_repo_land_if::RepoBookmarkModification;
use multi_repo_land_if::RepoBookmarkModificationMove;
use multi_repo_land_if::RepoBookmarkModificationSpec;
use multi_repo_land_if::SubmitManifestLandParams;
use multi_repo_land_if_clients::errors::AsManifestBranchNotEnabled;
use multi_repo_land_if_clients::make_MultiRepoLandService;
use permission_checker::AclProvider;
use permission_checker::MononokeIdentitySetExt;
use repo_authorization::AuthorizationContext;
use repo_authorization::RepoWriteOperation;
use repo_identity::RepoIdentityRef;
use source_control::CommitId;
use source_control::RepoSpecifier;
use thrift_client::MononokeThriftClient;
use tracing::error;
use tracing::info;

use crate::Repo;
use crate::command::RefUpdate;
use crate::model::RepositoryRequestContext;
use crate::service::GitMappingsStore;
use crate::service::GitObjectStore;

/// The SMC tier name for the Multi-Repo Land Service.
const MRL_TIER: &str = "mononoke-multi-repo-land-service";

/// Explicit per-attempt budget: the land's whole regenerate-and-retry loop
/// runs inside this one RPC.
const MRL_THRIFT_PROCESSING_TIMEOUT: Duration = Duration::from_secs(1800);

/// Total client-side wall clock across SR retries. Equal to the processing
/// budget on purpose: the tier retries only connection and overload
/// failures, which fail in milliseconds, so a retry still gets the whole
/// budget.
const MRL_THRIFT_OVERALL_TIMEOUT: Duration = Duration::from_secs(1800);

/// Check whether this push should be diverted.
///
/// Repos whose name *contains* the value configured in
/// `rl_land_service_repo_prefix` (CommonConfig) are diverted. The marker is
/// topology, not rollout state: which BRANCHES actually land through
/// Multi-Repo Land is decided by the server's `scm/mononoke:sslv2_enabled`
/// gate, and a fully-gated push falls back to the normal path. The
/// `_repo_prefix` name is historical; both `aosp/foo` and
/// `oculus/aosp/vendor/oculus` match a configured `aosp/`.
pub fn should_divert_to_rl_land_service(
    request_context: &RepositoryRequestContext,
) -> anyhow::Result<bool> {
    let marker = match &request_context
        .repo_configs
        .common
        .rl_land_service_repo_prefix
    {
        Some(p) if !p.is_empty() => p.as_str(),
        _ => return Ok(false),
    };
    let repo_name = request_context.repo.repo_identity().name();
    Ok(repo_name.contains(marker))
}

fn branch_name(ref_update: &RefUpdate) -> String {
    ref_update
        .ref_name
        .strip_prefix("refs/heads/")
        .unwrap_or(ref_update.ref_name.as_str())
        .to_string()
}

/// Longest matching marker wins; a repo appearing as a value IS a manifest
/// repo and routes to itself.
fn manifest_repo_for(repos: &BTreeMap<String, String>, repo_name: &str) -> Option<String> {
    if repos.values().any(|manifest| manifest == repo_name) {
        return Some(repo_name.to_string());
    }
    repos
        .iter()
        .filter(|(marker, _)| repo_name.contains(marker.as_str()))
        .max_by_key(|(marker, _)| marker.len())
        .map(|(_, manifest)| manifest.clone())
}

fn mrl_manifest_repo(request_context: &RepositoryRequestContext) -> anyhow::Result<String> {
    let repo_name = request_context.repo.repo_identity().name();
    manifest_repo_for(
        &request_context
            .repo_configs
            .common
            .multi_repo_land_manifest_repos,
        repo_name,
    )
    .ok_or_else(|| {
        anyhow::anyhow!("no multi_repo_land_manifest_repos entry matches repo '{repo_name}'")
    })
}

/// Whether an emergency push was requested and authorized.
pub enum EmergencyPushStatus {
    /// The x-git-emergency-push pushvar was not set.
    NotRequested,
    /// The pushvar was set and the caller is authorized.
    Authorized,
}

/// The Hipster ACL that controls emergency push access.
const EMERGENCY_PUSH_ACL: &str = "scm_emergency_git_push";

/// `EmergencyPushStatus::NotRequested` if the pushvar is unset,
/// `Authorized` if the caller is in the emergency push ACL, error otherwise.
///
/// The `acl_provider` must be extracted from `State` by the caller before
/// entering a `Send` future, because `State` is not `Sync`.
pub async fn check_emergency_push(
    acl_provider: &Arc<dyn AclProvider>,
    request_context: &RepositoryRequestContext,
) -> anyhow::Result<EmergencyPushStatus> {
    if !request_context.pushvars.emergency_push() {
        return Ok(EmergencyPushStatus::NotRequested);
    }

    let identities = request_context.ctx.metadata().identities();
    let checker = acl_provider
        .group(EMERGENCY_PUSH_ACL)
        .await
        .with_context(|| format!("Failed to load ACL '{EMERGENCY_PUSH_ACL}'"))?;

    if checker.is_member(identities).await {
        info!(
            "Emergency push authorized for repo {} by {}",
            request_context.repo.repo_identity().name(),
            identities.to_string(),
        );
        let mut scuba = request_context.ctx.scuba().clone();
        scuba.add("log_tag", "mrl_emergency_push_authorized");
        scuba.add("repo", request_context.repo.repo_identity().name());
        scuba.add("identities", identities.to_string());
        scuba.unsampled();
        scuba.log();
        Ok(EmergencyPushStatus::Authorized)
    } else {
        anyhow::bail!(
            "Emergency push rejected: identities [{}] are not authorized. \
             Request membership in the '{}' ACL to use emergency push.",
            identities.to_string(),
            EMERGENCY_PUSH_ACL,
        )
    }
}

/// Knob: when off, a denied diverted ref is only logged ("would fail") and
/// still lands; when on, it is rejected before the submit. Off is the
/// measurement stage of the rollout, not a long-term state.
const ENFORCE_DIVERTED_PUSH_ACL: &str = "scm/mononoke:git_server_enforce_diverted_push_acl";

/// Authorize each divertable ref as the pusher, exactly as the local push
/// path does inside `bookmarks_movement`: the bookmark's kind must be
/// permitted, the pusher must have write access to the repo for that kind of
/// operation, and the bookmark must not be restricted to other users.
///
/// Multi-Repo Land authorizes a `submit_manifest_land` against its Thrift
/// peer, which is this server's service identity; the pusher's own identities
/// never reach it. Without this check a read-only user can move any diverted
/// branch.
///
/// Returns the refs that may be diverted and, for each denied ref, the error
/// to report for it. Only listing failures are errors.
async fn authorize_diverted_refs(
    request_context: &RepositoryRequestContext,
    divertable: Vec<RefUpdate>,
    atomic: bool,
) -> anyhow::Result<(Vec<RefUpdate>, Vec<(RefUpdate, anyhow::Error)>)> {
    let ctx = &request_context.ctx;
    let repo = request_context.repo.as_ref();
    let repo_name = repo.repo_identity().name();
    let enforce = justknobs::eval(ENFORCE_DIVERTED_PUSH_ACL, None, Some(repo_name));
    let authz = AuthorizationContext::new_non_draft(ctx);

    let mut permitted = Vec::with_capacity(divertable.len());
    let mut denied: Vec<(RefUpdate, anyhow::Error)> = Vec::new();
    for ref_update in divertable {
        match authorize_one_diverted_ref(ctx, &authz, repo, &ref_update).await {
            Ok(()) => permitted.push(ref_update),
            Err(err) => {
                let mut scuba = ctx.scuba().clone();
                scuba.add(
                    "log_tag",
                    if enforce {
                        "mrl_diverted_push_acl_denied"
                    } else {
                        "mrl_diverted_push_acl_would_deny"
                    },
                );
                scuba.add("repo", repo_name);
                scuba.add("bookmark", ref_update.ref_name.as_str());
                scuba.add("identities", ctx.metadata().identities().to_string());
                scuba.add("error", format!("{err:#}"));
                scuba.unsampled();
                scuba.log();
                if enforce {
                    denied.push((ref_update, err));
                } else {
                    permitted.push(ref_update);
                }
            }
        }
    }

    // `git push --atomic`: one refused ref refuses them all, as the local
    // path does.
    if atomic && !denied.is_empty() {
        let reason = format!("{:#}", denied[0].1);
        denied.extend(permitted.drain(..).map(|r| {
            let err = anyhow::anyhow!("atomic push refused: {reason}");
            (r, err)
        }));
    }
    Ok((permitted, denied))
}

/// The checks `UpdateBookmarkOp`/`CreateBookmarkOp` run before a local
/// bookmark move, so a diverted push cannot do what a local one could not.
/// Creates and moves are told apart by the ref's `from`.
///
/// Publishing only: Multi-Repo Land lands every diverted branch as a
/// publishing bookmark, so a name in the repo's scratch namespace must not
/// pass on the (weaker) draft check and come out public.
///
/// A ref name that is not a bookmark is a denial of that ref, not a push
/// failure: the local path answers the same ref with a per-ref error.
async fn authorize_one_diverted_ref(
    ctx: &CoreContext,
    authz: &AuthorizationContext,
    repo: &Repo,
    ref_update: &RefUpdate,
) -> anyhow::Result<()> {
    let bookmark = BookmarkKey::new(
        ref_update
            .ref_name
            .strip_prefix("refs/")
            .unwrap_or(ref_update.ref_name.as_str()),
    )?;
    let kind = BookmarkKindRestrictions::OnlyPublishing.check_kind(repo, &bookmark)?;
    let op = if ref_update.from.is_null() {
        RepoWriteOperation::CreateBookmark(kind)
    } else {
        RepoWriteOperation::UpdateBookmark(kind)
    };
    authz.require_repo_write(ctx, repo, op).await?;
    authz.require_bookmark_modify(ctx, repo, &bookmark).await?;
    Ok(())
}

/// Result of diverting a push.
pub struct DiversionResult {
    /// Results for ref updates that were processed by the land service.
    pub diverted: Vec<(RefUpdate, anyhow::Result<()>)>,
    /// Ref updates that were not diverted (deletes, tags, non-branch refs)
    /// and should be processed by the normal git server path.
    pub remaining: Vec<RefUpdate>,
}

/// Branch creates and moves divert; everything else stays local.
fn is_divertable_ref(ref_update: &RefUpdate) -> bool {
    if ref_update.is_content() {
        return false;
    }
    if !ref_update.ref_name.starts_with("refs/heads/") {
        return false;
    }
    !ref_update.to.is_null()
}

/// One submit covering the whole push: divertable refs either become the
/// request or are handed back for the normal path.
#[derive(Debug)]
struct MrlPlan {
    params: Option<SubmitManifestLandParams>,
    /// Refs the request covers.
    submitted: Vec<RefUpdate>,
    /// Divertable refs the contract cannot express (manifest-repo branch
    /// creates); processed by the normal path instead.
    unsupported: Vec<RefUpdate>,
}

/// A push to the manifest repo itself maps to `user_manifest_modification`
/// (move-only, one branch per land); anything else maps to member updates.
fn plan_mrl_submit(
    repo_name: &str,
    manifest_repo: &str,
    divertable: Vec<RefUpdate>,
    pushvars: &HashMap<String, Bytes>,
    allow_non_ffwd: bool,
    caller_request_id: String,
) -> anyhow::Result<MrlPlan> {
    let thrift_pushvars: Option<BTreeMap<String, Vec<u8>>> = (!pushvars.is_empty()).then(|| {
        pushvars
            .iter()
            .map(|(k, v)| (k.clone(), v.to_vec()))
            .collect()
    });
    // Raw 20-byte ids: CommitId.git is binary, not hex.
    let git_id = |oid: &gix_hash::ObjectId| CommitId::git(oid.as_slice().to_vec());

    let mut submitted = Vec::new();
    let mut unsupported = Vec::new();
    let mut member_updates = Vec::new();
    let mut user_manifest_modification = None;
    let mut manifest_branch = None;

    for ref_update in divertable {
        if repo_name == manifest_repo {
            if ref_update.from.is_null() {
                unsupported.push(ref_update);
                continue;
            }
            anyhow::ensure!(
                user_manifest_modification.is_none(),
                "multi-branch push to manifest repo '{repo_name}' is not supported; \
                 push one branch at a time"
            );
            let branch = branch_name(&ref_update);
            manifest_branch = Some(branch.clone());
            user_manifest_modification = Some(RepoBookmarkModification {
                repo: RepoSpecifier {
                    name: manifest_repo.to_string(),
                    ..Default::default()
                },
                bookmark_name: branch,
                modification: RepoBookmarkModificationSpec::move_bookmark(
                    RepoBookmarkModificationMove {
                        target: git_id(&ref_update.to),
                        old_target: Some(git_id(&ref_update.from)),
                        allow_non_fast_forward_move: allow_non_ffwd,
                    },
                ),
                pushvars: thrift_pushvars.clone(),
            });
            submitted.push(ref_update);
        } else {
            member_updates.push(MemberRefUpdate {
                repo_name: repo_name.to_string(),
                bookmark_name: branch_name(&ref_update),
                target: git_id(&ref_update.to),
                old_target: (!ref_update.from.is_null()).then(|| git_id(&ref_update.from)),
                allow_non_fast_forward_move: allow_non_ffwd,
                pushvars: thrift_pushvars.clone(),
                commit_message_overrides: None,
            });
            submitted.push(ref_update);
        }
    }

    let params = (!member_updates.is_empty() || user_manifest_modification.is_some()).then(|| {
        SubmitManifestLandParams {
            member_updates,
            manifest_branch,
            manifest_repo: RepoSpecifier {
                name: manifest_repo.to_string(),
                ..Default::default()
            },
            user_manifest_modification,
            // The git protocol cannot express a server-side member rebase.
            disable_rebase_on_cas_failure: true,
            caller_request_id: Some(caller_request_id),
            retry_budget_ms: None,
        }
    });
    Ok(MrlPlan {
        params,
        submitted,
        unsupported,
    })
}

fn mrl_client(
    request_context: &RepositoryRequestContext,
    service_address: Option<String>,
) -> anyhow::Result<MononokeThriftClient<make_MultiRepoLandService>> {
    let fb = request_context.ctx.fb;
    let client = if let Some(host_port) = service_address {
        MononokeThriftClient::from_host_port(fb, host_port, make_MultiRepoLandService)?
    } else {
        MononokeThriftClient::from_tier_name(fb, MRL_TIER.to_string(), make_MultiRepoLandService)?
    };
    Ok(client
        .with_processing_timeout(MRL_THRIFT_PROCESSING_TIMEOUT)
        .with_overall_timeout(MRL_THRIFT_OVERALL_TIMEOUT))
}

/// Divert branch creates/moves to the Multi-Repo Land Service: one
/// synchronous `submit_manifest_land` for the whole push, no polling.
pub async fn divert_to_rl_land_service(
    ref_updates: Vec<RefUpdate>,
    request_context: Arc<RepositoryRequestContext>,
    _git_bonsai_mapping_store: Arc<GitMappingsStore>,
    _object_store: Arc<GitObjectStore>,
    service_address: Option<String>,
    atomic: bool,
) -> anyhow::Result<DiversionResult> {
    let ctx = &request_context.ctx;
    let repo_name = request_context.repo.repo_identity().name().to_string();

    let (divertable, mut remaining): (Vec<_>, Vec<_>) =
        ref_updates.into_iter().partition(is_divertable_ref);
    if divertable.is_empty() {
        return Ok(DiversionResult {
            diverted: vec![],
            remaining,
        });
    }

    // Authorize as the pusher before anything leaves this server: the land
    // service sees only our service identity.
    let (divertable, denied) =
        authorize_diverted_refs(&request_context, divertable, atomic).await?;
    let mut diverted: Vec<(RefUpdate, anyhow::Result<()>)> =
        denied.into_iter().map(|(r, err)| (r, Err(err))).collect();
    if divertable.is_empty() {
        return Ok(DiversionResult {
            diverted,
            remaining,
        });
    }

    let caller_request_id = format!("git-push:{}", ctx.metadata().session_id());
    // Config or request-shape problems reject the divertable refs with the
    // reason; they must not 500 the whole push.
    let plan = match mrl_manifest_repo(&request_context).and_then(|manifest_repo| {
        plan_mrl_submit(
            &repo_name,
            &manifest_repo,
            divertable.clone(),
            request_context.pushvars.as_ref(),
            request_context.pushvars.allow_non_fast_forward(),
            caller_request_id.clone(),
        )
    }) {
        Ok(plan) => plan,
        Err(e) => {
            let err_str = format!("cannot land push for repo {repo_name}: {e:#}");
            error!("{err_str}");
            diverted.extend(
                divertable
                    .into_iter()
                    .map(|r| (r, Err(anyhow::anyhow!(err_str.clone())))),
            );
            return Ok(DiversionResult {
                diverted,
                remaining,
            });
        }
    };
    remaining.extend(plan.unsupported);
    let Some(params) = plan.params else {
        return Ok(DiversionResult {
            diverted,
            remaining,
        });
    };

    info!(
        "Diverting push for repo {} ({} branch ref updates) to Multi-Repo Land, {} refs handled locally",
        repo_name,
        plan.submitted.len(),
        remaining.len(),
    );

    let service = mrl_client(&request_context, service_address)?.get_service_client(Some(ctx))?;
    match service.submit_manifest_land(&params).await {
        Ok(response) => {
            let mut scuba = ctx.scuba().clone();
            scuba.add("log_tag", "mrl_direct_land");
            scuba.add("repo", repo_name.as_str());
            scuba.add("request_id", response.request_id.as_str());
            scuba.add("attempts", response.attempts);
            scuba.add("skipped_branches", response.skipped_branches.len());
            // The names exist only here: the server logs a count, and scribe
            // records changed branches, never skipped ones.
            scuba.add(
                "skipped_branch_names",
                response
                    .skipped_branches
                    .iter()
                    .take(100)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(","),
            );
            scuba.unsampled();
            scuba.log();
            diverted.extend(plan.submitted.into_iter().map(|r| (r, Ok(()))));
            Ok(DiversionResult {
                diverted,
                remaining,
            })
        }
        Err(e) => {
            if let Some(not_enabled) = e.as_manifest_branch_not_enabled() {
                info!(
                    "Multi-Repo Land returned branch-not-enabled for repo {}: {} — falling back to normal push path",
                    repo_name, not_enabled.message,
                );
                let mut scuba = ctx.scuba().clone();
                scuba.add("log_tag", "mrl_branch_not_enabled_fallback");
                scuba.add("repo", repo_name.as_str());
                scuba.add("ref_count", plan.submitted.len());
                scuba.add("mrl_error_message", not_enabled.message.as_str());
                scuba.unsampled();
                scuba.log();
                remaining.extend(plan.submitted);
                return Ok(DiversionResult {
                    diverted,
                    remaining,
                });
            }
            // Per-ref errors, not a whole-push failure: the remaining refs
            // still process normally.
            let err_str =
                format!("Multi-Repo Land submit_manifest_land failed for repo {repo_name}: {e:#}");
            error!("{err_str}");
            let mut scuba = ctx.scuba().clone();
            scuba.add("log_tag", "mrl_direct_land_failed");
            scuba.add("repo", repo_name.as_str());
            scuba.add("caller_request_id", caller_request_id.as_str());
            scuba.add("error", format!("{e:#}"));
            scuba.unsampled();
            scuba.log();
            diverted.extend(
                plan.submitted
                    .into_iter()
                    .map(|r| (r, Err(anyhow::anyhow!(err_str.clone())))),
            );
            Ok(DiversionResult {
                diverted,
                remaining,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use gix_hash::Kind;
    use gix_hash::ObjectId;
    use mononoke_macros::mononoke;

    use super::*;
    use crate::command::RefType;

    fn ref_update(ref_name: &str, from: ObjectId, to: ObjectId) -> RefUpdate {
        RefUpdate {
            ref_name: ref_name.to_string(),
            ref_type: RefType::Standard,
            from,
            to,
        }
    }

    fn oid(byte: u8) -> ObjectId {
        ObjectId::from_hex(format!("{byte:02x}").repeat(20).as_bytes()).unwrap()
    }

    /// Longest marker wins; manifest repos route to themselves; no match is None.
    #[mononoke::test]
    fn manifest_repo_routing() {
        let repos = BTreeMap::from([
            ("aosp/".to_string(), "aosp/manifest".to_string()),
            (
                "aosp/vendor/".to_string(),
                "aosp/vendor-manifest".to_string(),
            ),
        ]);
        assert_eq!(
            manifest_repo_for(&repos, "aosp/platform/build").as_deref(),
            Some("aosp/manifest")
        );
        assert_eq!(
            manifest_repo_for(&repos, "aosp/vendor/oculus").as_deref(),
            Some("aosp/vendor-manifest"),
            "the longer, more specific marker must win"
        );
        assert_eq!(
            manifest_repo_for(&repos, "aosp/manifest").as_deref(),
            Some("aosp/manifest"),
            "a manifest repo routes to itself"
        );
        assert_eq!(manifest_repo_for(&repos, "zephyr/kernel"), None);
    }

    /// Member move and create map to raw-byte git CommitIds with the CAS
    /// baseline present only for the move.
    #[mononoke::test]
    fn plan_maps_member_moves_and_creates() {
        let plan = plan_mrl_submit(
            "aosp/member",
            "aosp/manifest",
            vec![
                ref_update("refs/heads/main", oid(0x11), oid(0x22)),
                ref_update(
                    "refs/heads/new-branch",
                    ObjectId::null(Kind::Sha1),
                    oid(0x33),
                ),
            ],
            &HashMap::new(),
            false,
            "test".to_string(),
        )
        .unwrap();

        let params = plan.params.expect("member updates must produce a request");
        assert_eq!(params.member_updates.len(), 2);
        assert!(plan.unsupported.is_empty());
        assert_eq!(plan.submitted.len(), 2);
        let mv = &params.member_updates[0];
        assert_eq!(mv.bookmark_name, "main");
        assert_eq!(
            mv.target,
            CommitId::git(vec![0x22; 20]),
            "raw bytes, not hex"
        );
        assert_eq!(mv.old_target, Some(CommitId::git(vec![0x11; 20])));
        let create = &params.member_updates[1];
        assert_eq!(create.old_target, None, "create has no CAS baseline");
        assert!(params.user_manifest_modification.is_none());
        assert!(params.disable_rebase_on_cas_failure);
    }

    /// A manifest-repo self-push move becomes the user manifest edit plus
    /// the manifest_branch pin, with no member updates.
    #[mononoke::test]
    fn plan_maps_manifest_self_push_to_user_edit() {
        let plan = plan_mrl_submit(
            "aosp/manifest",
            "aosp/manifest",
            vec![ref_update("refs/heads/release", oid(0x44), oid(0x55))],
            &HashMap::new(),
            true,
            "test".to_string(),
        )
        .unwrap();

        let params = plan
            .params
            .expect("a self-push move must produce a request");
        assert!(params.member_updates.is_empty());
        assert_eq!(params.manifest_branch.as_deref(), Some("release"));
        let user_mod = params.user_manifest_modification.expect("user edit");
        assert_eq!(user_mod.bookmark_name, "release");
        match user_mod.modification {
            RepoBookmarkModificationSpec::move_bookmark(mv) => {
                assert_eq!(mv.target, CommitId::git(vec![0x55; 20]));
                assert!(mv.allow_non_fast_forward_move);
            }
            other => panic!("expected move_bookmark, got {other:?}"),
        }
    }

    /// Manifest-repo branch creates are unrepresentable as user edits and
    /// fall back to the normal path; alone, they produce no request.
    #[mononoke::test]
    fn plan_leaves_manifest_creates_to_the_normal_path() {
        let plan = plan_mrl_submit(
            "aosp/manifest",
            "aosp/manifest",
            vec![ref_update(
                "refs/heads/new",
                ObjectId::null(Kind::Sha1),
                oid(0x66),
            )],
            &HashMap::new(),
            false,
            "test".to_string(),
        )
        .unwrap();

        assert!(plan.params.is_none());
        assert_eq!(plan.unsupported.len(), 1);
        assert!(plan.submitted.is_empty());
    }

    /// One user edit per land: a multi-branch self-push is rejected.
    #[mononoke::test]
    fn plan_rejects_multi_branch_manifest_self_push() {
        let err = plan_mrl_submit(
            "aosp/manifest",
            "aosp/manifest",
            vec![
                ref_update("refs/heads/a", oid(0x11), oid(0x22)),
                ref_update("refs/heads/b", oid(0x33), oid(0x44)),
            ],
            &HashMap::new(),
            false,
            "test".to_string(),
        )
        .expect_err("two manifest-repo branch moves must be rejected");
        assert!(err.to_string().contains("one branch at a time"), "{err}");
    }

    /// Pushvars ride along on every member update so hooks see them.
    #[mononoke::test]
    fn plan_forwards_pushvars() {
        let pushvars =
            HashMap::from([("ALLOW_LARGE_FILES".to_string(), Bytes::from_static(b"true"))]);
        let plan = plan_mrl_submit(
            "aosp/member",
            "aosp/manifest",
            vec![ref_update("refs/heads/main", oid(0x11), oid(0x22))],
            &pushvars,
            false,
            "test".to_string(),
        )
        .unwrap();

        let params = plan.params.unwrap();
        let sent = params.member_updates[0]
            .pushvars
            .as_ref()
            .expect("pushvars");
        assert_eq!(sent.get("ALLOW_LARGE_FILES"), Some(&b"true".to_vec()));
    }
}
