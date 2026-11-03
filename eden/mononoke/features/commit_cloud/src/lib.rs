/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#![feature(trait_alias)]
pub mod ctx;
pub mod references;
pub mod smartlog;
pub mod sql;
pub mod utils;

use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt::Display;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Context;
use anyhow::bail;
use anyhow::ensure;
use bonsai_git_mapping::BonsaiGitMapping;
use bonsai_hg_mapping::BonsaiHgMapping;
use commit_cloud_helpers::make_workspace_acl_name;
#[cfg(fbcode_build)]
use commit_cloud_intern_utils::acl_check::ACL_LINK;
#[cfg(fbcode_build)]
use commit_cloud_intern_utils::interngraph_publisher::publish_single_update;
#[cfg(fbcode_build)]
use commit_cloud_intern_utils::notification::NotificationData;
use commit_cloud_types::CommitCloudError;
use commit_cloud_types::CommitCloudInternalError;
use commit_cloud_types::CommitCloudUserError;
use commit_cloud_types::HistoricalVersion;
use commit_cloud_types::ReferencesData;
use commit_cloud_types::SmartlogFilter;
use commit_cloud_types::UpdateReferencesParams;
use commit_cloud_types::WorkspaceData;
use commit_cloud_types::WorkspaceSharingData;
use commit_cloud_types::changeset::CloudChangesetId;
use context::CoreContext;
use facet::facet;
#[cfg(fbcode_build)]
use futures_stats::futures03::TimedFutureExt;
use metaconfig_types::CommitCloudConfig;
#[cfg(fbcode_build)]
use mononoke_macros::mononoke;
use mononoke_types::DateTime;
use mononoke_types::Timestamp;
use permission_checker::AclProvider;
use permission_checker::BoxPermissionChecker;
use references::history::WorkspaceHistory;
use references::history::historical_versions_from_get_output;
use references::rename_all;
use repo_derived_data::ArcRepoDerivedData;
use sql::history_ops::GetOutput;
use sql::history_ops::GetType;
use sql::history_ops::HistoryDeleteArgs;
use sql::versions_ops::UpdateVersionArgs;

use crate::ctx::CommitCloudContext;
use crate::references::cast_references_data;
use crate::references::fetch_references;
use crate::references::resolve_write_head_author_dates;
use crate::references::update_references_data;
use crate::references::versions::WorkspaceVersion;
use crate::sql::ops::Delete;
use crate::sql::ops::GenericGet;
use crate::sql::ops::Get;
use crate::sql::ops::Insert;
use crate::sql::ops::SqlCommitCloud;
use crate::sql::ops::Update;

/// Checks that a request based on `base_version` refers to a version the
/// workspace has had, given that its latest version is `latest_version`.
fn check_base_version(
    cc_ctx: &CommitCloudContext,
    base_version: u64,
    latest_version: u64,
) -> Result<(), CommitCloudUserError> {
    if base_version <= latest_version {
        return Ok(());
    }
    if latest_version == 0 {
        return Err(CommitCloudUserError::WorkspaceWasRemoved(
            cc_ctx.workspace.clone(),
            cc_ctx.reponame.clone(),
        ));
    }
    Err(CommitCloudUserError::InvalidVersions(
        base_version,
        latest_version,
        cc_ctx.workspace.clone(),
        cc_ctx.reponame.clone(),
    ))
}

#[facet]
pub struct CommitCloud {
    pub storage: SqlCommitCloud,
    pub bonsai_hg_mapping: Arc<dyn BonsaiHgMapping>,
    pub bonsai_git_mapping: Arc<dyn BonsaiGitMapping>,
    pub repo_derived_data: ArcRepoDerivedData,
    pub ctx: CoreContext,
    pub acl_provider: Arc<dyn AclProvider>,
    pub config: Arc<CommitCloudConfig>,
}

impl CommitCloud {
    pub fn new(
        storage: SqlCommitCloud,
        bonsai_hg_mapping: Arc<dyn BonsaiHgMapping>,
        bonsai_git_mapping: Arc<dyn BonsaiGitMapping>,
        repo_derived_data: ArcRepoDerivedData,
        ctx: CoreContext,
        acl_provider: Arc<dyn AclProvider>,
        config: Arc<CommitCloudConfig>,
    ) -> Self {
        CommitCloud {
            storage,
            bonsai_hg_mapping,
            bonsai_git_mapping,
            repo_derived_data,
            ctx,
            acl_provider,
            config,
        }
    }
}

#[derive(Clone)]
pub enum Phase {
    Public,
    Draft,
}

const HISTORY_KEEP_VERSIONS: u64 = 100;

impl Display for Phase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Phase::Public => write!(f, "public"),
            Phase::Draft => write!(f, "draft"),
        }
    }
}

impl CommitCloud {
    pub async fn get_workspace(
        &self,
        cc_ctx: &CommitCloudContext,
    ) -> Result<WorkspaceData, CommitCloudError> {
        let maybeworkspace = WorkspaceVersion::fetch_from_db(
            &self.ctx,
            &self.storage,
            &cc_ctx.workspace,
            &cc_ctx.reponame,
        )
        .await
        .map_err(CommitCloudInternalError::Error)?;
        if let Some(res) = maybeworkspace {
            return Ok(res.into_workspace_data());
        }
        Err(CommitCloudUserError::NonexistentWorkspace(
            cc_ctx.workspace.clone(),
            cc_ctx.reponame.clone(),
        )
        .into())
    }

    pub async fn get_workspaces(
        &self,
        prefix: &str,
        reponame: &str,
    ) -> anyhow::Result<Vec<WorkspaceData>> {
        ensure!(
            !reponame.is_empty() && !prefix.is_empty(),
            "'get workspaces' failed: empty reponame or prefix "
        );

        ensure!(
            prefix.len() >= 3,
            "'get workspaces' failed: prefix must be at least 3 characters "
        );

        let maybeworkspace =
            WorkspaceVersion::fetch_by_prefix(&self.ctx, &self.storage, prefix, reponame).await?;

        Ok(maybeworkspace
            .into_iter()
            .map(|wp| wp.into_workspace_data())
            .collect())
    }

    pub async fn get_references(
        &self,
        cc_ctx: &CommitCloudContext,
        version: u64,
    ) -> Result<ReferencesData, CommitCloudError> {
        let base_version = version;

        let mut latest_version: u64 = 0;
        let mut version_timestamp: i64 = 0;
        let maybeworkspace = WorkspaceVersion::fetch_from_db(
            &self.ctx,
            &self.storage,
            &cc_ctx.workspace,
            &cc_ctx.reponame,
        )
        .await
        .map_err(CommitCloudInternalError::Error)?;
        if let Some(workspace_version) = maybeworkspace {
            latest_version = workspace_version.version;
            version_timestamp = workspace_version.timestamp.timestamp_seconds();
        }

        check_base_version(cc_ctx, base_version, latest_version)?;

        if base_version == latest_version {
            return Ok(ReferencesData {
                version: latest_version,
                heads: None,
                bookmarks: None,
                heads_dates: None,
                remote_bookmarks: None,
                snapshots: None,
                timestamp: Some(version_timestamp),
            });
        }

        // Always-on, O(1) phase timing for the (non-no-op) read path. The early
        // `base_version == latest_version` return above emits nothing so that
        // fast path stays free. See references::log_get_references_timing.
        let total_start = Instant::now();

        let raw_references_data = fetch_references(&self.ctx, cc_ctx, &self.storage)
            .await
            .map_err(CommitCloudInternalError::Error)?;
        let fetch_ms = total_start.elapsed().as_millis() as i64;

        let cast_start = Instant::now();
        let references_data = cast_references_data(
            raw_references_data,
            latest_version,
            version_timestamp,
            self.bonsai_hg_mapping.clone(),
            self.bonsai_git_mapping.clone(),
            self.repo_derived_data.clone(),
            &self.ctx,
            cc_ctx,
        )
        .await
        .map_err(CommitCloudInternalError::Error)?;
        let cast_ms = cast_start.elapsed().as_millis() as i64;

        let total_ms = total_start.elapsed().as_millis() as i64;
        references::log_get_references_timing(fetch_ms, cast_ms, total_ms);

        Ok(references_data)
    }

    pub async fn update_references(
        &self,
        cc_ctx: &CommitCloudContext,
        params: &UpdateReferencesParams,
    ) -> Result<ReferencesData, CommitCloudError> {
        let mut latest_version: u64 = 0;
        let mut version_timestamp: i64 = 0;

        let maybeworkspace = WorkspaceVersion::fetch_from_db(
            &self.ctx,
            &self.storage,
            &cc_ctx.workspace,
            &cc_ctx.reponame,
        )
        .await
        .map_err(CommitCloudInternalError::Error)?;

        if let Some(workspace_version) = maybeworkspace {
            latest_version = workspace_version.version;
            version_timestamp = workspace_version.timestamp.timestamp_seconds();
        }

        check_base_version(cc_ctx, params.version, latest_version)?;

        let raw_references_data = fetch_references(&self.ctx, cc_ctx, &self.storage)
            .await
            .map_err(CommitCloudInternalError::Error)?;
        if params.version < latest_version {
            return cast_references_data(
                raw_references_data,
                latest_version,
                version_timestamp,
                self.bonsai_hg_mapping.clone(),
                self.bonsai_git_mapping.clone(),
                self.repo_derived_data.clone(),
                &self.ctx,
                cc_ctx,
            )
            .await
            .map_err(CommitCloudError::internal_error);
        }

        // Resolved before the transaction opens: deriving inside it would hold a
        // never-retried XDB transaction across N blobstore round trips.
        let head_author_dates = resolve_write_head_author_dates(
            &self.ctx,
            cc_ctx,
            self.bonsai_hg_mapping.clone(),
            self.bonsai_git_mapping.clone(),
            &self.repo_derived_data,
            &params.new_heads,
        )
        .await;

        let mut txn = self
            .storage
            .connections
            .write_connection
            .start_transaction(self.ctx.sql_query_telemetry())
            .await
            .map_err(CommitCloudInternalError::Error)?;

        let initiate_workspace = params.version == 0
            && params.new_heads.is_empty()
            && params.updated_bookmarks.is_empty()
            && params
                .updated_remote_bookmarks
                .clone()
                .is_none_or(|x| x.is_empty());

        if !initiate_workspace {
            txn = update_references_data(
                &self.storage,
                txn,
                &self.ctx,
                params.clone(),
                cc_ctx,
                &head_author_dates,
            )
            .await
            .context(format!(
                "Failed to update references for request {:?}",
                params.clone()
            ))
            .map_err(CommitCloudInternalError::Error)?;
        }

        let new_version_timestamp = Timestamp::now();
        let new_version = latest_version + 1;

        let args = WorkspaceVersion {
            workspace: cc_ctx.workspace.clone(),
            reponame: cc_ctx.reponame.clone(),
            version: new_version,
            timestamp: new_version_timestamp,
            archived: false,
        };

        txn = self
            .storage
            .insert(
                txn,
                cc_ctx.reponame.clone(),
                cc_ctx.workspace.clone(),
                args.clone(),
            )
            .await
            .context(format!(
                "Failed to update references for request {:?} for new version {:?}",
                params.clone(),
                args
            ))
            .map_err(CommitCloudInternalError::Error)?;

        let history_entry = WorkspaceHistory::from_references(
            raw_references_data,
            latest_version,
            version_timestamp,
        );

        txn = self
            .storage
            .insert(
                txn,
                cc_ctx.reponame.clone(),
                cc_ctx.workspace.clone(),
                history_entry,
            )
            .await
            .map_err(CommitCloudInternalError::Error)?;

        if latest_version >= HISTORY_KEEP_VERSIONS {
            let delete_limit =
                justknobs::get_as::<u64>("scm/mononoke:commitcloud_history_gc_chunk_size", None);

            // Delete old history entries
            txn = Delete::<WorkspaceHistory>::delete(
                &self.storage,
                txn,
                cc_ctx.reponame.clone(),
                cc_ctx.workspace.clone(),
                HistoryDeleteArgs {
                    latest_version,
                    keep_version: HISTORY_KEEP_VERSIONS,
                    delete_limit,
                },
            )
            .await
            .map_err(CommitCloudInternalError::Error)?;
        }

        txn.commit()
            .await
            .map_err(CommitCloudInternalError::Error)?;

        #[cfg(fbcode_build)]
        if !self.config.disable_interngraph_notification && !initiate_workspace {
            let mut scuba = self.ctx.scuba().clone();
            let fb = self.ctx.fb.clone();
            let cc_ctx = cc_ctx.clone();
            let params = params.clone();
            mononoke::spawn_task(async move {
                let notification =
                    NotificationData::from_update_references_params(params, new_version);

                let (stats, res) = publish_single_update(
                    notification,
                    &cc_ctx.workspace.clone(),
                    &cc_ctx.reponame.clone(),
                    fb,
                )
                .timed()
                .await;

                scuba.add_future_stats(&stats).log_with_msg(
                    "commit cloud: sent interngraph notification",
                    format!(
                        "For workspace {} in repo {} with response {:?}",
                        cc_ctx.workspace,
                        cc_ctx.reponame,
                        res.map_err(CommitCloudInternalError::Error)
                    ),
                );
            });
        }

        Ok(ReferencesData {
            version: new_version,
            heads: None,
            bookmarks: None,
            heads_dates: None,
            remote_bookmarks: None,
            snapshots: None,
            timestamp: Some(new_version_timestamp.timestamp_seconds()),
        })
    }

    pub async fn commit_cloud_acl(
        &self,
        name: &str,
    ) -> anyhow::Result<Option<BoxPermissionChecker>> {
        self.acl_provider
            .commitcloud_workspace_acl(name, &None)
            .await
    }

    pub async fn share_workspace(
        &self,
        ctx: &CommitCloudContext,
    ) -> anyhow::Result<WorkspaceSharingData> {
        let maybeworkspace = WorkspaceVersion::fetch_from_db(
            &self.ctx,
            &self.storage,
            &ctx.workspace,
            &ctx.reponame,
        )
        .await?;
        if maybeworkspace.is_none() {
            bail!(
                "'share_workspace' failed: workspace {} does not exist in repo {}",
                ctx.workspace,
                ctx.reponame
            )
        }

        if maybeworkspace.unwrap().archived {
            bail!(
                "'share_workspace' failed: workspace {} has been archived",
                ctx.workspace
            );
        }
        let acl_name = make_workspace_acl_name(&ctx.workspace, &ctx.reponame);

        #[cfg(fbcode_build)]
        let link = format!("[{ACL_LINK}{acl_name}]");
        #[cfg(not(fbcode_build))]
        let link = String::new();

        let maybe_acl = self.commit_cloud_acl(&acl_name).await?;
        if maybe_acl.is_some() {
            return Ok(WorkspaceSharingData {
                acl_name: acl_name.clone(),
                sharing_message: format!(
                    "'share_workspace' succeeded: workspace {} has been already shared under the acl {} {}",
                    ctx.workspace, acl_name, link
                ),
            });
        }

        if ctx.owner.is_none() {
            bail!(
                "'share_workspace' failed: no owner inferred for workspace {} ",
                ctx.workspace
            );
        }

        // Stamp both the workspace owner and the sharer (e.g. a manager
        // recovering a departed report's workspace) onto the new ACL so the
        // sharer also becomes a maintainer and retains access afterwards.
        let acl_identities = ctx
            .owner
            .iter()
            .flatten()
            .chain(self.ctx.metadata().identities().iter())
            .cloned()
            .collect();

        match self
            .acl_provider
            .commitcloud_workspace_acl(&acl_name, &Some(acl_identities))
            .await
        {
            Err(e) => bail!(
                "'share_workspace' failed: unable to create acl {} for workspace {}: {} ",
                acl_name,
                ctx.workspace,
                e
            ),
            Ok(_) => Ok(WorkspaceSharingData {
                acl_name: acl_name.clone(),
                sharing_message: format!(
                    "'share_workspace' succeeded: workspace {} is now marked for sharing through the ACL {} [{}]",
                    ctx.workspace, acl_name, link
                ),
            }),
        }
    }

    pub async fn rename_workspace(
        &self,
        cc_ctx: &CommitCloudContext,
        new_workspace: &str,
    ) -> anyhow::Result<String> {
        ensure!(
            !new_workspace.is_empty(),
            "'rename_workspace' failed: new workspace name cannot be empty"
        );

        ensure!(
            WorkspaceVersion::fetch_from_db(
                &self.ctx,
                &self.storage,
                new_workspace,
                &cc_ctx.reponame
            )
            .await?
            .is_none(),
            format!("'rename_workspace' failed: workspace {new_workspace} already exists"),
        );

        let (txn, affected_rows) =
            rename_all(&self.storage, &self.ctx, cc_ctx, new_workspace).await?;

        ensure!(
            affected_rows > 0,
            format!(
                "'rename_workspace' failed: workspace {} does not exist",
                cc_ctx.workspace
            )
        );
        txn.commit().await?;

        Ok("'rename_workspace' succeeded".to_string())
    }

    pub async fn update_workspace_archive(
        &self,
        cc_ctx: &CommitCloudContext,
        archived: bool,
    ) -> anyhow::Result<String> {
        // Check if workspace exists
        let _ = self.get_workspace(cc_ctx).await?;

        let txn = self
            .storage
            .connections
            .write_connection
            .start_transaction(self.ctx.sql_query_telemetry())
            .await?;

        let (txn, affected_rows) = Update::<WorkspaceVersion>::update(
            &self.storage,
            txn,
            &self.ctx,
            cc_ctx.clone(),
            UpdateVersionArgs::Archive(archived),
        )
        .await?;
        txn.commit().await?;

        ensure!(
            affected_rows > 0,
            "'update_workspace_archive' failed: failed on updating the workspace {} from DB",
            cc_ctx.workspace.clone()
        );

        Ok(String::from("'update_workspace_archive' succeeded"))
    }

    pub async fn get_history_by(
        &self,
        cc_ctx: &CommitCloudContext,
        filter: &SmartlogFilter,
    ) -> anyhow::Result<WorkspaceHistory> {
        let args = match filter {
            SmartlogFilter::Version(version) => {
                ensure!(
                    *version >= 0,
                    "'get_smartlog_by_version' failed: version must be greater than 0"
                );
                GetType::GetHistoryVersion {
                    version: *version as u64,
                }
            }
            SmartlogFilter::Timestamp(timestamp) => {
                let timestamp = Timestamp::from_timestamp_secs(*timestamp);
                GetType::GetHistoryDate {
                    timestamp,
                    limit: 1,
                }
            }
        };

        let history = GenericGet::<WorkspaceHistory>::get(
            &self.storage,
            &self.ctx,
            cc_ctx.reponame.clone(),
            cc_ctx.workspace.clone(),
            args.clone(),
        )
        .await?;

        ensure!(
            !history.is_empty(),
            "'get_smartlog_by_version' failed: no smartlog found for {}",
            match args {
                GetType::GetHistoryVersion { version } => format!("version {version}"),
                GetType::GetHistoryDate { timestamp, .. } =>
                    format!("timestamp {}", DateTime::from(timestamp)),
                _ => unreachable!(),
            }
        );
        match history.first() {
            Some(GetOutput::WorkspaceHistory(history)) => Ok(history.clone()),
            _ => Err(anyhow::anyhow!("unexpected history type")),
        }
    }

    pub async fn get_historical_versions(
        &self,
        cc_ctx: &CommitCloudContext,
    ) -> anyhow::Result<Vec<HistoricalVersion>> {
        ensure!(
            WorkspaceVersion::fetch_from_db(
                &self.ctx,
                &self.storage,
                &cc_ctx.workspace,
                &cc_ctx.reponame
            )
            .await?
            .is_some(),
            format!(
                "'get_historical_versions' failed: workspace {} does not exist",
                cc_ctx.workspace
            ),
        );

        let args = GetType::GetHistoryVersionTimestamp;
        let results = GenericGet::<WorkspaceHistory>::get(
            &self.storage,
            &self.ctx,
            cc_ctx.reponame.clone(),
            cc_ctx.workspace.clone(),
            args.clone(),
        )
        .await?;

        historical_versions_from_get_output(results)
    }

    pub async fn rollback_workspace(
        &self,
        cc_ctx: &CommitCloudContext,
        version: u64,
    ) -> anyhow::Result<String> {
        let maybeworkspace = WorkspaceVersion::fetch_from_db(
            &self.ctx,
            &self.storage,
            &cc_ctx.workspace,
            &cc_ctx.reponame,
        )
        .await?;
        ensure!(
            maybeworkspace.is_some(),
            format!(
                "'rollback_workspace' failed: workspace {} does not exist in repo {}",
                cc_ctx.workspace, cc_ctx.reponame
            )
        );
        let workspace_version = maybeworkspace.unwrap();

        let args = GetType::GetHistoryVersion { version };
        let result = GenericGet::<WorkspaceHistory>::get(
            &self.storage,
            &self.ctx,
            cc_ctx.reponame.clone(),
            cc_ctx.workspace.clone(),
            args.clone(),
        )
        .await?;

        ensure!(
            !result.is_empty(),
            format!("'rollback_workspace' failed: no record found for version {version}")
        );

        let destination_workspace = match result.first().unwrap() {
            GetOutput::WorkspaceHistory(workspace_history) => workspace_history.clone(),
            _ => bail!("'rollback_workspace' failed: expected output from get_version"),
        };

        let dst_heads: HashSet<CloudChangesetId> = destination_workspace
            .heads
            .iter()
            .map(|h| h.commit.clone())
            .collect();

        let current_workspace = fetch_references(&self.ctx, cc_ctx, &self.storage).await?;
        let current_heads: HashSet<CloudChangesetId> = current_workspace
            .heads
            .iter()
            .map(|h| h.commit.clone())
            .collect();

        let new_heads: Vec<CloudChangesetId> =
            dst_heads.difference(&current_heads).cloned().collect();
        let old_heads: Vec<CloudChangesetId> =
            current_heads.difference(&dst_heads).cloned().collect();

        let old_bookmarks: Vec<String> = current_workspace
            .local_bookmarks
            .iter()
            .map(|x| x.name().clone())
            .collect();

        let new_bookmarks: HashMap<String, CloudChangesetId> = destination_workspace
            .local_bookmarks
            .iter()
            .map(|x| (x.name().clone(), x.commit().clone()))
            .collect();

        let params = UpdateReferencesParams {
            workspace: cc_ctx.workspace.clone(),
            reponame: cc_ctx.reponame.clone(),
            version: workspace_version.version,
            removed_heads: old_heads,
            new_heads,
            updated_bookmarks: new_bookmarks,
            removed_bookmarks: old_bookmarks,
            updated_remote_bookmarks: Some(destination_workspace.remote_bookmarks.clone()),
            removed_remote_bookmarks: Some(current_workspace.remote_bookmarks.clone()),
            new_snapshots: vec![],
            removed_snapshots: vec![],
            client_info: None,
        };

        self.update_references(cc_ctx, &params)
            .await
            .map(|_| "rollback succeeded".to_string())
            .map_err(|e| anyhow::anyhow!(e.to_string()))
    }

    pub async fn get_other_repo_workspaces(
        &self,
        workspace: &str,
    ) -> anyhow::Result<Vec<WorkspaceData>> {
        ensure!(
            !workspace.is_empty(),
            "'get other repo workspaces' failed: empty workspace name"
        );

        let maybeworkspace =
            WorkspaceVersion::fetch_by_name(&self.ctx, &self.storage, workspace).await?;

        Ok(maybeworkspace
            .into_iter()
            .map(|wp| wp.into_workspace_data())
            .collect())
    }
}
