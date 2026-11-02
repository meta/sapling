/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use context::CoreContext;
use diff_service_client::DiffInput;
use diff_service_client::DiffServiceClient;
use diff_service_client::RepoDiffServiceClient;
use diff_service_if_clients::errors::CommitCompareError;
use diff_service_if_clients::errors::DiffHunksError;
use diff_service_if_clients::errors::DiffUnifiedError;
use diff_service_if_clients::errors::DiffUnifiedHeaderlessError;
use diff_service_if_clients::errors::DiffUnifiedHeaderlessUnaryError;
use diff_service_if_clients::errors::DiffUnifiedUnaryError;
use diff_service_if_clients::errors::MetadataDiffError;
use environment::RemoteDiffOptions;
use futures::StreamExt;
use metaconfig_types::RemoteDiffConfig;
use mononoke_api::ChangesetPathContentContext;
use mononoke_api::ChangesetPathDiffContext;
use mononoke_api::FileContext;
use mononoke_api::HeaderlessUnifiedDiff;
use mononoke_api::MononokeRepo;
use mononoke_api::Repo;
use mononoke_api::UnifiedDiff;
use mononoke_api::UnifiedDiffMode;
use mononoke_api::headerless_unified_diff;
use mononoke_types::NonRootMPath;
use scs_errors::ServiceError;
use source_control as thrift;

#[cfg(fbcode_build)]
mod facebook;
#[cfg(fbcode_build)]
use facebook as platform;

/// Trait for extracting declared errors from diff-service RPC failures.
trait DiffServiceError {
    fn request_error(&self) -> Option<&diff_service_if::RequestError>;
    fn overloaded_error(&self) -> Option<&diff_service_if::OverloadedError>;
    fn repo_not_loaded_error(&self) -> Option<&diff_service_if::RepoNotLoadedError>;
}

macro_rules! impl_diff_service_error {
    ($($error:ty),+ $(,)?) => {
        $(
            impl DiffServiceError for $error {
                fn request_error(&self) -> Option<&diff_service_if::RequestError> {
                    match self {
                        Self::ex(error) => Some(error),
                        _ => None,
                    }
                }

                fn overloaded_error(&self) -> Option<&diff_service_if::OverloadedError> {
                    match self {
                        Self::overloaded(error) => Some(error),
                        _ => None,
                    }
                }

                fn repo_not_loaded_error(&self) -> Option<&diff_service_if::RepoNotLoadedError> {
                    match self {
                        Self::repo_not_loaded(error) => Some(error),
                        _ => None,
                    }
                }
            }
        )+
    };
}

impl_diff_service_error!(
    DiffUnifiedHeaderlessError,
    DiffUnifiedError,
    DiffHunksError,
    MetadataDiffError,
    CommitCompareError,
    DiffUnifiedUnaryError,
    DiffUnifiedHeaderlessUnaryError,
);

fn convert_diff_service_error<E: DiffServiceError + std::fmt::Debug>(e: E) -> ServiceError {
    if let Some(overloaded) = e.overloaded_error() {
        return scs_errors::overloaded(format!("diff service overloaded: {}", overloaded.message))
            .into();
    }

    if let Some(repo_not_loaded) = e.repo_not_loaded_error() {
        return scs_errors::internal_error(format!(
            "diff service repo not loaded: {}",
            repo_not_loaded.message
        ))
        .into();
    }

    match e.request_error() {
        Some(req_err) => match &req_err.reason {
            diff_service_if::RequestErrorReason::diff_error(diff_err) => {
                scs_errors::invalid_request(format!("diff service: {}", diff_err.reason)).into()
            }
            diff_service_if::RequestErrorReason::transient_error(transient) => {
                match transient.error_type {
                    diff_service_if::TransientErrorType::OVERLOADED => scs_errors::overloaded(
                        format!("diff service overloaded: {}", transient.message),
                    )
                    .into(),
                    diff_service_if::TransientErrorType::REPO_NOT_FOUND => {
                        scs_errors::internal_error(format!(
                            "diff service repo not found: {}",
                            transient.message
                        ))
                        .into()
                    }
                    _ => scs_errors::internal_error(format!(
                        "diff service transient error: {}",
                        transient.message
                    ))
                    .into(),
                }
            }
            diff_service_if::RequestErrorReason::UnknownField(_) => {
                scs_errors::internal_error(format!("diff service error: {e:#?}")).into()
            }
        },
        None => scs_errors::internal_error(format!("diff service error: {e:#?}")).into(),
    }
}

fn remote_infra_error(reason: impl ToString) -> ServiceError {
    scs_errors::internal_error(reason).into()
}

/// Router for diff operations that can use either local mononoke_api
/// or remote diff_service based on command line args and JustKnobs configuration.
pub struct DiffRouter<'a> {
    pub(crate) fb: fbinit::FacebookInit,
    pub(crate) diff_options: &'a RemoteDiffOptions,
    pub(crate) remote_diff_config: Option<&'a RemoteDiffConfig>,
}

impl<'a> DiffRouter<'a> {
    /// Create a DiffServiceClient based on the remote_diff_config.
    /// Falls back to Service Manager discovery if no config is provided.
    fn create_diff_service_client(
        &self,
        repo_name: &str,
    ) -> Result<DiffServiceClient, ServiceError> {
        platform::create_diff_service_client(self.fb, self.remote_diff_config, repo_name)
            .map_err(|e| scs_errors::internal_error(format!("{e:#}")).into())
    }

    /// Check if remote diff should be used for this repo
    fn should_use_remote_diff(&self, repo_name: &str) -> bool {
        // Gate 1: CLI flag must be enabled
        if !self.diff_options.diff_remotely {
            return false;
        }

        // Gate 2: Check JK - this is the kill switch in production
        justknobs::eval("scm/mononoke:remote_diff", None, Some(repo_name))
    }

    /// Generate headerless unified diff between two files.
    /// Routes to either local mononoke_api or remote diff_service based on command line args and JustKnobs.
    pub async fn headerless_unified_diff(
        &self,
        ctx: &CoreContext,
        repo_name: &str,
        other_file: &FileContext<Repo>,
        base_file: &FileContext<Repo>,
        context_lines: usize,
    ) -> Result<HeaderlessUnifiedDiff, ServiceError> {
        if self.should_use_remote_diff(repo_name) {
            return self
                .remote_headerless_diff(ctx, repo_name, other_file, base_file, context_lines)
                .await;
        }
        headerless_unified_diff(other_file, base_file, context_lines, false)
            .await
            .map_err(ServiceError::from)
    }

    pub async fn unified_diff(
        &self,
        ctx: &CoreContext,
        repo_name: &str,
        path_context: &ChangesetPathDiffContext<Repo>,
        mode: UnifiedDiffMode,
        context_lines: usize,
    ) -> Result<UnifiedDiff, ServiceError> {
        if self.should_use_remote_diff(repo_name) {
            return self
                .remote_unified_diff(ctx, repo_name, path_context, mode, context_lines)
                .await;
        }
        Ok(path_context
            .unified_diff(ctx, context_lines, mode, false)
            .await?)
    }

    pub async fn metadata_diff(
        &self,
        ctx: &CoreContext,
        repo_name: &str,
        path_context: &ChangesetPathDiffContext<Repo>,
    ) -> Result<mononoke_api::MetadataDiff, ServiceError> {
        if self.should_use_remote_diff(repo_name) {
            return self
                .remote_metadata_diff(ctx, repo_name, path_context)
                .await;
        }
        Ok(path_context.metadata_diff(ctx, false).await?)
    }

    /// Check if the unary transport for remote unified/headerless diffs should
    /// be used for this repo. Independent of `should_use_remote_diff`: that
    /// kill switch decides whether to call diff_service at all; this one
    /// decides whether to use the unary RPC vs. the streamed RPC.
    ///
    /// Uses per-correlator consistent hashing — the only Mononoke routing
    /// decision (vs. logging-only call sites) that does so. Aligns the JK
    /// evaluation at routing time with the JK evaluation at scuba-logging
    /// time so the `enabled_experiments_jk` Scuba column reliably reflects
    /// which transport was actually used.
    fn should_use_remote_diff_unary(&self, ctx: &CoreContext, repo_name: &str) -> bool {
        if !self.diff_options.diff_remotely {
            return false;
        }

        let correlator = ctx
            .metadata()
            .client_request_info()
            .map(|cri| cri.correlator.as_str());

        justknobs::eval(
            "scm/mononoke:remote_diff_unary",
            correlator,
            Some(repo_name),
        )
    }

    /// Check if remote commit_compare should be used for this repo.
    /// Uses a separate JK from file-level diffs for independent rollout.
    pub fn should_use_remote_commit_compare(&self, repo_name: &str) -> bool {
        // Gate 1: CLI flag must be enabled
        if !self.diff_options.diff_remotely {
            return false;
        }

        // Gate 2: Check JK - this is the kill switch in production
        justknobs::eval("scm/mononoke:remote_commit_compare", None, Some(repo_name))
    }

    /// Forward a commit_compare request to the remote diff_service.
    pub async fn remote_commit_compare(
        &self,
        ctx: &CoreContext,
        repo_name: &str,
        commit_id: thrift::CommitId,
        params: thrift::CommitCompareParams,
    ) -> Result<thrift::CommitCompareResponse, ServiceError> {
        let client = self
            .create_diff_service_client(repo_name)
            .map_err(|e| remote_infra_error(format!("{e:?}")))?;
        let repo_client = RepoDiffServiceClient::new(repo_name.to_string(), client);

        let response = repo_client
            .commit_compare(ctx, commit_id, params)
            .await
            .map_err(convert_diff_service_error)?;
        // Propagate diff_service's verdict so the remote route sets the
        // same flag (and `nocache`) as the local path.
        if response
            .partial_info
            .as_ref()
            .is_some_and(|info| info.partial)
        {
            ctx.set_partial_response();
        }

        Ok(response)
    }

    async fn remote_headerless_diff(
        &self,
        ctx: &CoreContext,
        repo_name: &str,
        other_file: &FileContext<Repo>,
        base_file: &FileContext<Repo>,
        context_lines: usize,
    ) -> Result<HeaderlessUnifiedDiff, ServiceError> {
        let other_content_id = other_file
            .id()
            .await
            .map_err(|e| remote_infra_error(format!("{e:?}")))?;
        let base_content_id = base_file
            .id()
            .await
            .map_err(|e| remote_infra_error(format!("{e:?}")))?;

        let base_input = Some(DiffInput::content(base_content_id));
        let other_input = Some(DiffInput::content(other_content_id));

        let options = Some(diff_service_if::DiffUnifiedHeaderlessOptions {
            context_lines: context_lines as i32,
            inspect_binary_data: false,
            ..Default::default()
        });

        let client = self
            .create_diff_service_client(repo_name)
            .map_err(|e| remote_infra_error(format!("{e:?}")))?;
        let repo_client = RepoDiffServiceClient::new(repo_name.to_string(), client);

        if self.should_use_remote_diff_unary(ctx, repo_name) {
            let response = repo_client
                .diff_unified_headerless_unary(ctx, base_input, other_input, options)
                .await
                .map_err(convert_diff_service_error)?;
            return Ok(HeaderlessUnifiedDiff {
                raw_diff: response.content,
                is_binary: response.is_binary,
            });
        }

        let (response, mut stream) = repo_client
            .diff_unified_headerless(ctx, base_input, other_input, options)
            .await
            .map_err(convert_diff_service_error)?;

        let mut raw_diff = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| remote_infra_error(format!("stream error: {e:?}")))?;
            raw_diff.extend_from_slice(&chunk.content);
        }

        Ok(HeaderlessUnifiedDiff {
            raw_diff,
            is_binary: response.is_binary,
        })
    }

    fn input_from_changeset<R: MononokeRepo>(
        content: &ChangesetPathContentContext<R>,
        replacement_path: Option<String>,
    ) -> Result<DiffInput, ServiceError> {
        let path = NonRootMPath::try_from(content.path().clone())
            .map_err(|e| remote_infra_error(format!("{e:?}")))?
            .to_string();
        let changeset_id = content.changeset().id();
        Ok(diff_service_client::DiffInput::CommitPath {
            commit_id: diff_service_if::CommitId::bonsai(changeset_id.as_ref().to_vec()),
            path,
            replacement_path,
        })
    }

    /// Builds a unified-diff input that names the file by content ID, so the
    /// diff service does not have to load the changeset to find it.
    ///
    /// Keeps `CommitPath` where the diff service needs the changeset: bubble
    /// commits, whose content it can only read after opening the bubble from
    /// the changeset, and repos that interpret Git LFS pointers, whose pointer
    /// it reads from the changeset's file change.
    async fn unified_diff_input(
        content: &ChangesetPathContentContext<Repo>,
        replacement_path: Option<String>,
    ) -> Result<DiffInput, ServiceError> {
        let repo_ctx = content.repo_ctx();
        if repo_ctx.in_bubble() || repo_ctx.config().git_configs.git_lfs_interpret_pointers {
            return Self::input_from_changeset(content, replacement_path);
        }

        let Some(file) = content
            .file()
            .await
            .map_err(|e| remote_infra_error(format!("{e:?}")))?
        else {
            return Self::input_from_changeset(content, replacement_path);
        };
        let content_id = file
            .id()
            .await
            .map_err(|e| remote_infra_error(format!("{e:?}")))?;
        let path = match replacement_path {
            Some(replacement_path) => replacement_path,
            None => NonRootMPath::try_from(content.path().clone())
                .map_err(|e| remote_infra_error(format!("{e:?}")))?
                .to_string(),
        };
        Ok(DiffInput::content_with_path(content_id, path))
    }

    async fn remote_unified_diff(
        &self,
        ctx: &CoreContext,
        repo_name: &str,
        path_context: &ChangesetPathDiffContext<Repo>,
        mode: UnifiedDiffMode,
        context_lines: usize,
    ) -> Result<UnifiedDiff, ServiceError> {
        let replacement_path = path_context.subtree_copy_dest_path().map(|p| p.to_string());

        // The Base file is the "old" file, with Other is the "new" one
        // the replacement path goes in the "old" file, so that it can show
        // the new path after a move.
        let base_input = match path_context.get_old_content() {
            Some(c) => Some(Self::unified_diff_input(c, replacement_path).await?),
            None => None,
        };

        let other_input = match path_context.get_new_content() {
            Some(c) => Some(Self::unified_diff_input(c, None).await?),
            None => None,
        };

        let copy_info = path_context.copy_info();

        let file_type = match path_context
            .get_old_content()
            .or(path_context.get_new_content())
        {
            Some(content) => content
                .file_type()
                .await
                .map_err(|e| remote_infra_error(format!("{e:?}")))?,
            None => None,
        };

        let options = diff_service_if::DiffUnifiedOptions {
            context_lines: context_lines as i32,
            diff_mode: match mode {
                UnifiedDiffMode::Inline => diff_service_if::DiffMode::INLINE,
                UnifiedDiffMode::OmitContent => diff_service_if::DiffMode::OMIT_CONTENT,
            },
            file_type: file_type.map_or(diff_service_if::DiffFileType::REGULAR, |file_type| {
                match file_type {
                    mononoke_api::FileType::Regular => diff_service_if::DiffFileType::REGULAR,
                    mononoke_api::FileType::Executable => diff_service_if::DiffFileType::EXECUTABLE,
                    mononoke_api::FileType::Symlink => diff_service_if::DiffFileType::SYMLINK,
                    mononoke_api::FileType::GitSubmodule => {
                        diff_service_if::DiffFileType::GIT_SUBMODULE
                    }
                }
            }),
            copy_info: match copy_info {
                mononoke_api::CopyInfo::None => diff_service_if::DiffCopyInfo::NONE,
                mononoke_api::CopyInfo::Move => diff_service_if::DiffCopyInfo::MOVE,
                mononoke_api::CopyInfo::Copy => diff_service_if::DiffCopyInfo::COPY,
            },
            inspect_lfs_pointers: false,
            ..Default::default()
        };

        let client = self
            .create_diff_service_client(repo_name)
            .map_err(|e| remote_infra_error(format!("{e:?}")))?;
        let repo_client = RepoDiffServiceClient::new(repo_name.to_string(), client);

        if self.should_use_remote_diff_unary(ctx, repo_name) {
            let response = repo_client
                .diff_unified_unary(ctx, base_input, other_input, options)
                .await
                .map_err(convert_diff_service_error)?;
            return Ok(UnifiedDiff {
                raw_diff: response.content,
                is_binary: response.is_binary,
            });
        }

        let (response, mut stream) = repo_client
            .diff_unified(ctx, base_input, other_input, options)
            .await
            .map_err(convert_diff_service_error)?;
        let mut raw_diff = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| remote_infra_error(format!("stream error: {e:?}")))?;
            raw_diff.extend_from_slice(&chunk.content);
        }

        Ok(UnifiedDiff {
            raw_diff,
            is_binary: response.is_binary,
        })
    }

    async fn remote_metadata_diff(
        &self,
        ctx: &CoreContext,
        repo_name: &str,
        path_context: &ChangesetPathDiffContext<Repo>,
    ) -> Result<mononoke_api::MetadataDiff, ServiceError> {
        let replacement_path = path_context.subtree_copy_dest_path().map(|p| p.to_string());

        // The Base file is the "old" file, with Other is the "new" one
        // the replacement path goes in the "old" file, so that it can show
        // the new path after a move.
        let base_input = path_context
            .get_old_content()
            .map(|c| Self::input_from_changeset(c, replacement_path))
            .transpose()?;

        let other_input = path_context
            .get_new_content()
            .map(|c| Self::input_from_changeset(c, None))
            .transpose()?;

        let client = self
            .create_diff_service_client(repo_name)
            .map_err(|e| remote_infra_error(format!("{e:?}")))?;
        let repo_client = RepoDiffServiceClient::new(repo_name.to_string(), client);

        let response = repo_client
            .metadata_diff(ctx, base_input, other_input, false)
            .await
            .map_err(convert_diff_service_error)?;

        // Convert the diff_service_if enums to mononoke_api enums
        let convert_file_type = |ft: Option<diff_service_if::DiffFileType>| -> Result<
            Option<mononoke_api::FileType>,
            ServiceError,
        > {
            ft.map(
                |file_type| -> Result<mononoke_api::FileType, ServiceError> {
                    match file_type {
                        diff_service_if::DiffFileType::REGULAR => {
                            Ok(mononoke_api::FileType::Regular)
                        }
                        diff_service_if::DiffFileType::EXECUTABLE => {
                            Ok(mononoke_api::FileType::Executable)
                        }
                        diff_service_if::DiffFileType::SYMLINK => {
                            Ok(mononoke_api::FileType::Symlink)
                        }
                        diff_service_if::DiffFileType::GIT_SUBMODULE => {
                            Ok(mononoke_api::FileType::GitSubmodule)
                        }
                        unknown => Err(scs_errors::internal_error(format!(
                            "Unknown file type from diff service: {unknown:?}"
                        ))
                        .into()),
                    }
                },
            )
            .transpose()
        };

        let convert_content_type = |ct: Option<diff_service_if::DiffContentType>| -> Result<
            Option<mononoke_api::FileContentType>,
            ServiceError,
        > {
            ct.map(
                |content_type| -> Result<mononoke_api::FileContentType, ServiceError> {
                    match content_type {
                        diff_service_if::DiffContentType::TEXT => {
                            Ok(mononoke_api::FileContentType::Text)
                        }
                        diff_service_if::DiffContentType::NON_UTF8 => {
                            Ok(mononoke_api::FileContentType::NonUtf8)
                        }
                        diff_service_if::DiffContentType::BINARY => {
                            Ok(mononoke_api::FileContentType::Binary)
                        }
                        diff_service_if::DiffContentType::LFS_POINTER => {
                            Ok(mononoke_api::FileContentType::LfsPointer)
                        }
                        unknown => Err(scs_errors::internal_error(format!(
                            "Unknown content type from diff service: {unknown:?}"
                        ))
                        .into()),
                    }
                },
            )
            .transpose()
        };

        let convert_generated_status = |gs: Option<diff_service_if::DiffGeneratedStatus>| -> Result<
            Option<mononoke_api::FileGeneratedStatus>,
            ServiceError,
        > {
            gs.map(
                |generated_status| -> Result<mononoke_api::FileGeneratedStatus, ServiceError> {
                    match generated_status {
                        diff_service_if::DiffGeneratedStatus::FULLY => {
                            Ok(mononoke_api::FileGeneratedStatus::FullyGenerated)
                        }
                        diff_service_if::DiffGeneratedStatus::PARTIALLY => {
                            Ok(mononoke_api::FileGeneratedStatus::PartiallyGenerated)
                        }
                        diff_service_if::DiffGeneratedStatus::NON_GENERATED => {
                            Ok(mononoke_api::FileGeneratedStatus::NotGenerated)
                        }
                        unknown => Err(scs_errors::internal_error(format!(
                            "Unknown generated status from diff service: {unknown:?}"
                        ))
                        .into()),
                    }
                },
            )
            .transpose()
        };

        // Convert the response back to mononoke_api::MetadataDiff
        Ok(mononoke_api::MetadataDiff {
            old_file_info: mononoke_api::MetadataDiffFileInfo {
                file_type: convert_file_type(response.base_file_info.file_type)?,
                file_content_type: convert_content_type(response.base_file_info.content_type)?,
                file_generated_status: convert_generated_status(
                    response.base_file_info.generated_status,
                )?,
            },
            new_file_info: mononoke_api::MetadataDiffFileInfo {
                file_type: convert_file_type(response.other_file_info.file_type)?,
                file_content_type: convert_content_type(response.other_file_info.content_type)?,
                file_generated_status: convert_generated_status(
                    response.other_file_info.generated_status,
                )?,
            },
            lines_count: response
                .lines_count
                .map(|lc| mononoke_api::MetadataDiffLinesCount {
                    added_lines_count: lc.added_lines as usize,
                    deleted_lines_count: lc.deleted_lines as usize,
                    significant_added_lines_count: lc.significant_added_lines as usize,
                    significant_deleted_lines_count: lc.significant_deleted_lines as usize,
                    first_added_line_number: lc.first_added_line_number.map(|n| n as usize),
                }),
        })
    }
}

#[cfg(all(fbcode_build, test))]
mod tests {
    use environment::RemoteDiffOptions;
    use justknobs::test_helpers::JustKnobsInMemory;
    use justknobs::test_helpers::KnobVal;
    use justknobs::test_helpers::with_just_knobs;
    use maplit::hashmap;
    use metaconfig_types::RemoteDiffConfig;
    use mononoke_macros::mononoke;

    use super::*;

    fn create_diff_router<'a>(
        fb: fbinit::FacebookInit,
        diff_options: &'a RemoteDiffOptions,
        remote_diff_config: Option<&'a RemoteDiffConfig>,
    ) -> DiffRouter<'a> {
        DiffRouter {
            fb,
            diff_options,
            remote_diff_config,
        }
    }

    #[mononoke::fbinit_test]
    fn test_should_use_remote_diff_cli_flag_disabled(fb: fbinit::FacebookInit) {
        let diff_options = RemoteDiffOptions {
            diff_remotely: false,
        };
        let router = create_diff_router(fb, &diff_options, None);

        // Even with JK enabled, should return false when CLI flag is off
        let result = with_just_knobs(
            JustKnobsInMemory::new(hashmap![
                "scm/mononoke:remote_diff".to_string() => KnobVal::Bool(true)
            ]),
            || router.should_use_remote_diff("test_repo"),
        );
        assert!(!result, "Should return false when CLI flag is disabled");
    }

    #[mononoke::fbinit_test]
    fn test_should_use_remote_diff_jk_disabled(fb: fbinit::FacebookInit) {
        // When JK is disabled, should always return false even with CLI flag enabled
        let diff_options = RemoteDiffOptions {
            diff_remotely: true,
        };
        let router = create_diff_router(fb, &diff_options, None);

        let result = with_just_knobs(
            JustKnobsInMemory::new(hashmap![
                "scm/mononoke:remote_diff".to_string() => KnobVal::Bool(false)
            ]),
            || router.should_use_remote_diff("test_repo"),
        );
        assert!(!result, "Should return false when JK is disabled");
    }

    #[mononoke::fbinit_test]
    fn test_should_use_remote_diff_jk_is_kill_switch_with_config(fb: fbinit::FacebookInit) {
        // Even with remote_diff_config present, JK should still act as kill switch
        // This was the bug - config presence used to bypass JK check
        let diff_options = RemoteDiffOptions {
            diff_remotely: true,
        };
        let config = RemoteDiffConfig::HostPort("localhost:8080".to_string());
        let router = create_diff_router(fb, &diff_options, Some(&config));

        // With JK disabled, should return false even though config is present
        let result = with_just_knobs(
            JustKnobsInMemory::new(hashmap![
                "scm/mononoke:remote_diff".to_string() => KnobVal::Bool(false)
            ]),
            || router.should_use_remote_diff("test_repo"),
        );
        assert!(
            !result,
            "JK should act as kill switch even when remote_diff_config is present"
        );
    }

    #[mononoke::fbinit_test]
    fn test_should_use_remote_diff_enabled(fb: fbinit::FacebookInit) {
        // When both CLI flag and JK are enabled, should return true
        let diff_options = RemoteDiffOptions {
            diff_remotely: true,
        };
        let router = create_diff_router(fb, &diff_options, None);

        let result = with_just_knobs(
            JustKnobsInMemory::new(hashmap![
                "scm/mononoke:remote_diff".to_string() => KnobVal::Bool(true)
            ]),
            || router.should_use_remote_diff("test_repo"),
        );
        assert!(
            result,
            "Should return true when both CLI flag and JK are enabled"
        );
    }

    #[mononoke::fbinit_test]
    fn test_should_use_remote_diff_enabled_with_config(fb: fbinit::FacebookInit) {
        // When both CLI flag and JK are enabled with config, should return true
        let diff_options = RemoteDiffOptions {
            diff_remotely: true,
        };
        let config = RemoteDiffConfig::SmcTier("diff_service.smc".to_string());
        let router = create_diff_router(fb, &diff_options, Some(&config));

        let result = with_just_knobs(
            JustKnobsInMemory::new(hashmap![
                "scm/mononoke:remote_diff".to_string() => KnobVal::Bool(true)
            ]),
            || router.should_use_remote_diff("test_repo"),
        );
        assert!(
            result,
            "Should return true when CLI flag and JK are enabled with config"
        );
    }

    #[mononoke::fbinit_test]
    fn test_should_use_remote_commit_compare_cli_flag_disabled(fb: fbinit::FacebookInit) {
        let diff_options = RemoteDiffOptions {
            diff_remotely: false,
        };
        let router = create_diff_router(fb, &diff_options, None);

        let result = with_just_knobs(
            JustKnobsInMemory::new(hashmap![
                "scm/mononoke:remote_commit_compare".to_string() => KnobVal::Bool(true)
            ]),
            || router.should_use_remote_commit_compare("test_repo"),
        );
        assert!(!result, "Should return false when CLI flag is disabled");
    }

    #[mononoke::fbinit_test]
    fn test_should_use_remote_commit_compare_jk_disabled(fb: fbinit::FacebookInit) {
        let diff_options = RemoteDiffOptions {
            diff_remotely: true,
        };
        let router = create_diff_router(fb, &diff_options, None);

        let result = with_just_knobs(
            JustKnobsInMemory::new(hashmap![
                "scm/mononoke:remote_commit_compare".to_string() => KnobVal::Bool(false)
            ]),
            || router.should_use_remote_commit_compare("test_repo"),
        );
        assert!(!result, "Should return false when JK is disabled");
    }

    #[mononoke::fbinit_test]
    fn test_should_use_remote_commit_compare_jk_is_kill_switch_with_config(
        fb: fbinit::FacebookInit,
    ) {
        let diff_options = RemoteDiffOptions {
            diff_remotely: true,
        };
        let config = RemoteDiffConfig::HostPort("localhost:8080".to_string());
        let router = create_diff_router(fb, &diff_options, Some(&config));

        let result = with_just_knobs(
            JustKnobsInMemory::new(hashmap![
                "scm/mononoke:remote_commit_compare".to_string() => KnobVal::Bool(false)
            ]),
            || router.should_use_remote_commit_compare("test_repo"),
        );
        assert!(
            !result,
            "JK should act as kill switch even when remote_diff_config is present"
        );
    }

    #[mononoke::fbinit_test]
    fn test_should_use_remote_commit_compare_enabled(fb: fbinit::FacebookInit) {
        let diff_options = RemoteDiffOptions {
            diff_remotely: true,
        };
        let router = create_diff_router(fb, &diff_options, None);

        let result = with_just_knobs(
            JustKnobsInMemory::new(hashmap![
                "scm/mononoke:remote_commit_compare".to_string() => KnobVal::Bool(true)
            ]),
            || router.should_use_remote_commit_compare("test_repo"),
        );
        assert!(
            result,
            "Should return true when both CLI flag and JK are enabled"
        );
    }

    #[mononoke::fbinit_test]
    fn test_should_use_remote_commit_compare_enabled_with_config(fb: fbinit::FacebookInit) {
        let diff_options = RemoteDiffOptions {
            diff_remotely: true,
        };
        let config = RemoteDiffConfig::SmcTier("diff_service.smc".to_string());
        let router = create_diff_router(fb, &diff_options, Some(&config));

        let result = with_just_knobs(
            JustKnobsInMemory::new(hashmap![
                "scm/mononoke:remote_commit_compare".to_string() => KnobVal::Bool(true)
            ]),
            || router.should_use_remote_commit_compare("test_repo"),
        );
        assert!(
            result,
            "Should return true when CLI flag and JK are enabled with config"
        );
    }

    #[mononoke::fbinit_test]
    fn test_should_use_remote_diff_unary_cli_flag_disabled(fb: fbinit::FacebookInit) {
        let diff_options = RemoteDiffOptions {
            diff_remotely: false,
        };
        let router = create_diff_router(fb, &diff_options, None);
        let ctx = CoreContext::test_mock(fb);

        let result = with_just_knobs(
            JustKnobsInMemory::new(hashmap![
                "scm/mononoke:remote_diff_unary".to_string() => KnobVal::Bool(true)
            ]),
            || router.should_use_remote_diff_unary(&ctx, "test_repo"),
        );
        assert!(!result, "Should return false when CLI flag is disabled");
    }

    #[mononoke::fbinit_test]
    fn test_should_use_remote_diff_unary_jk_disabled(fb: fbinit::FacebookInit) {
        let diff_options = RemoteDiffOptions {
            diff_remotely: true,
        };
        let router = create_diff_router(fb, &diff_options, None);
        let ctx = CoreContext::test_mock(fb);

        let result = with_just_knobs(
            JustKnobsInMemory::new(hashmap![
                "scm/mononoke:remote_diff_unary".to_string() => KnobVal::Bool(false)
            ]),
            || router.should_use_remote_diff_unary(&ctx, "test_repo"),
        );
        assert!(!result, "Should return false when JK is disabled");
    }

    #[mononoke::fbinit_test]
    fn test_should_use_remote_diff_unary_jk_is_kill_switch_with_config(fb: fbinit::FacebookInit) {
        let diff_options = RemoteDiffOptions {
            diff_remotely: true,
        };
        let config = RemoteDiffConfig::HostPort("localhost:8080".to_string());
        let router = create_diff_router(fb, &diff_options, Some(&config));
        let ctx = CoreContext::test_mock(fb);

        let result = with_just_knobs(
            JustKnobsInMemory::new(hashmap![
                "scm/mononoke:remote_diff_unary".to_string() => KnobVal::Bool(false)
            ]),
            || router.should_use_remote_diff_unary(&ctx, "test_repo"),
        );
        assert!(
            !result,
            "JK should act as kill switch even when remote_diff_config is present"
        );
    }

    #[mononoke::fbinit_test]
    fn test_should_use_remote_diff_unary_enabled(fb: fbinit::FacebookInit) {
        let diff_options = RemoteDiffOptions {
            diff_remotely: true,
        };
        let router = create_diff_router(fb, &diff_options, None);
        let ctx = CoreContext::test_mock(fb);

        let result = with_just_knobs(
            JustKnobsInMemory::new(hashmap![
                "scm/mononoke:remote_diff_unary".to_string() => KnobVal::Bool(true)
            ]),
            || router.should_use_remote_diff_unary(&ctx, "test_repo"),
        );
        assert!(
            result,
            "Should return true when both CLI flag and JK are enabled"
        );
    }

    #[mononoke::fbinit_test]
    fn test_should_use_remote_diff_unary_enabled_with_config(fb: fbinit::FacebookInit) {
        let diff_options = RemoteDiffOptions {
            diff_remotely: true,
        };
        let config = RemoteDiffConfig::SmcTier("diff_service.smc".to_string());
        let router = create_diff_router(fb, &diff_options, Some(&config));
        let ctx = CoreContext::test_mock(fb);

        let result = with_just_knobs(
            JustKnobsInMemory::new(hashmap![
                "scm/mononoke:remote_diff_unary".to_string() => KnobVal::Bool(true)
            ]),
            || router.should_use_remote_diff_unary(&ctx, "test_repo"),
        );
        assert!(
            result,
            "Should return true when CLI flag and JK are enabled with config"
        );
    }

    #[mononoke::test]
    fn test_diff_error_maps_to_invalid_request() {
        // Given: diff_service rejects the request as invalid input.
        let err = DiffUnifiedHeaderlessError::ex(diff_service_if::RequestError {
            reason: diff_service_if::RequestErrorReason::diff_error(diff_service_if::DiffError {
                reason: "bad input".into(),
                ..Default::default()
            }),
            ..Default::default()
        });

        // When: SCS converts the diff_service error for its clients.
        let service_error = convert_diff_service_error(err);

        // Then: clients observe an invalid request with the diff_service context.
        match service_error {
            scs_errors::ServiceError::Request(request) => {
                assert_eq!(request.kind, thrift::RequestErrorKind::INVALID_REQUEST);
                assert_eq!(request.reason, "diff service: bad input");
            }
            other => panic!("expected SCS RequestError, got {other:?}"),
        }
    }

    #[mononoke::test]
    fn test_dedicated_overload_maps_to_scs_overload() {
        let err = DiffUnifiedHeaderlessError::overloaded(diff_service_if::OverloadedError {
            message: "diff service is overloaded".into(),
            ..Default::default()
        });

        let service_error = convert_diff_service_error(err);

        match service_error {
            scs_errors::ServiceError::Overload(overload) => {
                assert_eq!(
                    overload.reason,
                    "diff service overloaded: diff service is overloaded"
                );
            }
            other => panic!("expected SCS OverloadError, got {other:?}"),
        }
    }

    #[mononoke::test]
    fn test_legacy_overloaded_diff_service_error_maps_to_scs_overload() {
        // Given: an older diff_service rejects the request due to overload.
        let err = DiffUnifiedHeaderlessError::ex(diff_service_if::RequestError {
            reason: diff_service_if::RequestErrorReason::transient_error(
                diff_service_if::TransientError {
                    error_type: diff_service_if::TransientErrorType::OVERLOADED,
                    message: "diff service is overloaded".into(),
                    ..Default::default()
                },
            ),
            ..Default::default()
        });

        // When: SCS converts the diff_service error for its clients.
        let service_error = convert_diff_service_error(err);

        // Then: clients observe SCS overload rather than an internal error.
        match service_error {
            scs_errors::ServiceError::Overload(overload) => {
                assert!(
                    overload.reason.contains("diff service is overloaded"),
                    "overload reason should preserve diff_service context, got: {}",
                    overload.reason
                );
            }
            other => panic!("expected SCS OverloadError, got {other:?}"),
        }
    }

    #[mononoke::test]
    fn test_repo_not_loaded_diff_service_error_maps_to_scs_internal_error() {
        // Given: diff_service reports a shard-routing miss with its dedicated error.
        let err =
            DiffUnifiedHeaderlessError::repo_not_loaded(diff_service_if::RepoNotLoadedError {
                message: "repo not loaded on this server: test_repo".into(),
                ..Default::default()
            });

        // When: SCS converts the diff_service error for its clients.
        let service_error = convert_diff_service_error(err);

        // Then: clients observe the final failure as an internal error.
        match service_error {
            scs_errors::ServiceError::Internal(internal) => {
                assert_eq!(
                    internal.reason,
                    "diff service repo not loaded: repo not loaded on this server: test_repo"
                );
            }
            other => panic!("expected SCS InternalError, got {other:?}"),
        }
    }

    #[mononoke::test]
    fn test_legacy_repo_not_found_diff_service_error_maps_to_scs_internal_error() {
        // Given: an older diff_service reports a shard-routing miss as a nested error.
        let err = DiffUnifiedHeaderlessError::ex(diff_service_if::RequestError {
            reason: diff_service_if::RequestErrorReason::transient_error(
                diff_service_if::TransientError {
                    error_type: diff_service_if::TransientErrorType::REPO_NOT_FOUND,
                    message: "repo not loaded on this server: test_repo".into(),
                    ..Default::default()
                },
            ),
            ..Default::default()
        });

        // When: SCS converts the diff_service error for its clients.
        let service_error = convert_diff_service_error(err);

        // Then: clients observe the final failure as an internal error.
        match service_error {
            scs_errors::ServiceError::Internal(internal) => {
                assert!(
                    internal
                        .reason
                        .contains("repo not loaded on this server: test_repo"),
                    "internal error reason should preserve diff_service context, got: {}",
                    internal.reason
                );
            }
            other => panic!("expected SCS InternalError, got {other:?}"),
        }
    }
}
