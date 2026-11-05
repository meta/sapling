/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#![cfg(test)]

use std::collections::BTreeMap;
use std::result::Result;

use fb303_core_clients::BaseService;
use fb303_core_clients::errors::*;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use mockall::mock;
use thrift_streaming_clients::StreamingEdenService;
use thrift_streaming_clients::errors::*;
use thrift_streaming_clients::types::*;
use thrift_thriftclients::EdenService;
use thrift_thriftclients::thrift::errors::*;
use thrift_types::edenfs::*;
use thrift_types::edenfs_config::EdenConfigData;
use thrift_types::fb303_core::fb303_status;

// The mock definitions will require updates whenever the EdenFS Thrift API
// changes. Not all changes will require an update, only those that either:
//   1. Add/Rename a new method to the interface
//   2. Change the parameters or return type of a method
//   3. Introduce a breaking Thrift API changes (e.g. rename a method)
//
// For more information on mocking external traits see:
//   https://docs.rs/mockall/latest/mockall/#external-traits
//
// For reference, EdenService is defined here:
//   https://www.internalfb.com/code/fbsource/fbcode/eden/fs/service/eden.thrift
//
// and StreamingEdenService is defined here:
//   https://www.internalfb.com/code/fbsource/fbcode/eden/fs/service/streamingeden.thrift
//
mock! {
    pub EdenFsService {}

    #[allow(non_snake_case)]
    impl BaseService for EdenFsService {
        fn getStatus(&self) -> BoxFuture<'static, Result<fb303_status, GetStatusError>>;

        fn getName(&self) -> BoxFuture<'static, Result<String, GetNameError>>;

        fn getVersion(&self) -> BoxFuture<'static, Result<String, GetVersionError>>;

        fn getStatusDetails(&self) -> BoxFuture<'static, Result<String, GetStatusDetailsError>>;

        fn getCounters(&self) -> BoxFuture<'static, Result<BTreeMap<String, i64>, GetCountersError>>;

        fn getRegexCounters(
            &self,
            regex: &str,
        ) -> BoxFuture<'static, Result<BTreeMap<String, i64>, GetRegexCountersError>>;

        fn getSelectedCounters(
            &self,
            keys: &[String],
        ) -> BoxFuture<'static, Result<BTreeMap<String, i64>, GetSelectedCountersError>>;

        fn getCounter(&self, key: &str) -> BoxFuture<'static, Result<i64, GetCounterError>>;

        fn getExportedValues(
            &self,
        ) -> BoxFuture<'static, Result<BTreeMap<String, String>, GetExportedValuesError>>;

        fn getSelectedExportedValues(
            &self,
            keys: &[String],
        ) -> BoxFuture<'static, Result<BTreeMap<String, String>, GetSelectedExportedValuesError>>;

        fn getRegexExportedValues(
            &self,
            regex: &str,
        ) -> BoxFuture<'static, Result<BTreeMap<String, String>, GetRegexExportedValuesError>>;

        fn getExportedValue(
            &self,
            key: &str,
        ) -> BoxFuture<'static, Result<String, GetExportedValueError>>;

        fn setOption(&self, key: &str, value: &str) -> BoxFuture<'static, Result<(), SetOptionError>>;

        fn getOption(&self, key: &str) -> BoxFuture<'static, Result<String, GetOptionError>>;

        fn getOptions(&self) -> BoxFuture<'static, Result<BTreeMap<String, String>, GetOptionsError>>;

        fn aliveSince(&self) -> BoxFuture<'static, Result<i64, AliveSinceError>>;
    }

    #[allow(non_snake_case)]
    impl EdenService for EdenFsService {
        fn listMounts(&self) -> BoxFuture<'static, Result<Vec<MountInfo>, ListMountsError>>;

        fn mount(&self, info: &MountArgument) -> BoxFuture<'static, Result<(), MountError>>;

        fn unmount(&self, mount_point: &PathString) -> BoxFuture<'static, Result<(), UnmountError>>;

        fn unmountV2(
            &self,
            unmount_argument: &UnmountArgument,
        ) -> BoxFuture<'static, Result<(), UnmountV2Error>>;

        fn checkOutRevision(
            &self,
            mount_point: &PathString,
            snapshot_hash: &ThriftRootId,
            checkout_mode: &CheckoutMode,
            params: &CheckOutRevisionParams,
        ) -> BoxFuture<'static, Result<Vec<CheckoutConflict>, CheckOutRevisionError>>;

        fn getCheckoutProgressInfo(
            &self,
            params: &CheckoutProgressInfoRequest,
        ) -> BoxFuture<'static, Result<CheckoutProgressInfoResponse, GetCheckoutProgressInfoError>>;

        fn resetParentCommits(
            &self,
            mount_point: &PathString,
            parents: &WorkingDirectoryParents,
            params: &ResetParentCommitsParams,
        ) -> BoxFuture<'static, Result<(), ResetParentCommitsError>>;

        fn getCurrentSnapshotInfo(
            &self,
            params: &GetCurrentSnapshotInfoRequest,
        ) -> BoxFuture<'static, Result<GetCurrentSnapshotInfoResponse, GetCurrentSnapshotInfoError>>;

        fn synchronizeWorkingCopy(
            &self,
            mount_point: &PathString,
            params: &SynchronizeWorkingCopyParams,
        ) -> BoxFuture<'static, Result<(), SynchronizeWorkingCopyError>>;

        fn getSHA1(
            &self,
            mount_point: &PathString,
            paths: &[PathString],
            sync: &SyncBehavior,
        ) -> BoxFuture<'static, Result<Vec<SHA1Result>, GetSHA1Error>>;

        fn getBlake3(
            &self,
            mount_point: &PathString,
            paths: &[PathString],
            sync: &SyncBehavior,
        ) -> BoxFuture<'static, Result<Vec<Blake3Result>, GetBlake3Error>>;

        fn getDigestHash(
            &self,
            mount_point: &PathString,
            paths: &[PathString],
            sync: &SyncBehavior,
        ) -> BoxFuture<'static, Result<Vec<DigestHashResult>, GetDigestHashError>>;

        fn addBindMount(
            &self,
            mount_point: &PathString,
            repo_path: &PathString,
            target_path: &PathString,
        ) -> BoxFuture<'static, Result<(), AddBindMountError>>;

        fn removeBindMount(
            &self,
            mount_point: &PathString,
            repo_path: &PathString,
        ) -> BoxFuture<'static, Result<(), RemoveBindMountError>>;

        fn getCurrentJournalPosition(
            &self,
            mount_point: &PathString,
        ) -> BoxFuture<'static, Result<JournalPosition, GetCurrentJournalPositionError>>;

        fn peekCurrentJournalPosition(
            &self,
            params: &PeekCurrentJournalPositionRequest,
        ) -> BoxFuture<'static, Result<PeekCurrentJournalPositionResponse, PeekCurrentJournalPositionError>>;

        fn getFilesChangedSince(
            &self,
            mount_point: &PathString,
            from_position: &JournalPosition,
        ) -> BoxFuture<'static, Result<FileDelta, GetFilesChangedSinceError>>;

        fn setJournalMemoryLimit(
            &self,
            mount_point: &PathString,
            limit: i64,
        ) -> BoxFuture<'static, Result<(), SetJournalMemoryLimitError>>;

        fn getJournalMemoryLimit(
            &self,
            mount_point: &PathString,
        ) -> BoxFuture<'static, Result<i64, GetJournalMemoryLimitError>>;

        fn flushJournal(
            &self,
            mount_point: &PathString,
        ) -> BoxFuture<'static, Result<(), FlushJournalError>>;

        fn debugGetRawJournal(
            &self,
            params: &DebugGetRawJournalParams,
        ) -> BoxFuture<'static, Result<DebugGetRawJournalResponse, DebugGetRawJournalError>>;

        fn getEntryInformation(
            &self,
            mount_point: &PathString,
            paths: &[PathString],
            sync: &SyncBehavior,
        ) -> BoxFuture<'static, Result<Vec<EntryInformationOrError>, GetEntryInformationError>>;

        fn getFileInformation(
            &self,
            mount_point: &PathString,
            paths: &[PathString],
            sync: &SyncBehavior,
        ) -> BoxFuture<'static, Result<Vec<FileInformationOrError>, GetFileInformationError>>;

        fn getAttributesFromFilesV2(
            &self,
            params: &GetAttributesFromFilesParams,
        ) -> BoxFuture<'static, Result<GetAttributesFromFilesResultV2, GetAttributesFromFilesV2Error>>;

        fn readdir(
            &self,
            params: &ReaddirParams,
        ) -> BoxFuture<'static, Result<ReaddirResult, ReaddirError>>;

        fn globFiles(&self, params: &GlobParams) -> BoxFuture<'static, Result<Glob, GlobFilesError>>;

        fn prefetchFilesV2(
            &self,
            params: &PrefetchParams,
        ) -> BoxFuture<'static, Result<PrefetchResult, PrefetchFilesV2Error>>;

        fn queryPreloadProgress(
            &self,
            request: &PreloadProgressRequest,
        ) -> BoxFuture<'static, Result<PreloadProgressResponse, QueryPreloadProgressError>>;

        fn changeOwnership(
            &self,
            request: &ChangeOwnershipRequest,
        ) -> BoxFuture<'static, Result<ChangeOwnershipResponse, ChangeOwnershipError>>;

        fn getScmStatusV2(
            &self,
            params: &GetScmStatusParams,
        ) -> BoxFuture<'static, Result<GetScmStatusResult, GetScmStatusV2Error>>;

        fn getScmStatusBetweenRevisions(
            &self,
            mount_point: &PathString,
            old_hash: &ThriftRootId,
            new_hash: &ThriftRootId,
        ) -> BoxFuture<'static, Result<ScmStatus, GetScmStatusBetweenRevisionsError>>;

        fn matchFilesystem(
            &self,
            params: &MatchFileSystemRequest,
        ) -> BoxFuture<'static, Result<MatchFileSystemResponse, MatchFilesystemError>>;

        fn getDaemonInfo(&self) -> BoxFuture<'static, Result<DaemonInfo, GetDaemonInfoError>>;

        fn checkPrivHelper(&self) -> BoxFuture<'static, Result<PrivHelperInfo, CheckPrivHelperError>>;

        fn getPid(&self) -> BoxFuture<'static, Result<i64, GetPidError>>;

        fn initiateShutdown(
            &self,
            reason: &str,
        ) -> BoxFuture<'static, Result<(), InitiateShutdownError>>;

        fn getConfig(
            &self,
            params: &GetConfigParams,
        ) -> BoxFuture<'static, Result<EdenConfigData, GetConfigError>>;

        fn reloadConfig(&self) -> BoxFuture<'static, Result<(), ReloadConfigError>>;

        fn debugGetTree(
            &self,
            request: &DebugGetScmTreeRequest,
        ) -> BoxFuture<'static, Result<DebugGetScmTreeResponse, DebugGetTreeError>>;

        fn debugGetBlob(
            &self,
            request: &DebugGetScmBlobRequest,
        ) -> BoxFuture<'static, Result<DebugGetScmBlobResponse, DebugGetBlobError>>;

        fn debugGetBlobMetadata(
            &self,
            request: &DebugGetBlobMetadataRequest,
        ) -> BoxFuture<'static, Result<DebugGetBlobMetadataResponse, DebugGetBlobMetadataError>>;

        fn debugInodeStatus(
            &self,
            mount_point: &PathString,
            path: &PathString,
            flags: i64,
            sync: &SyncBehavior,
        ) -> BoxFuture<'static, Result<Vec<TreeInodeDebugInfo>, DebugInodeStatusError>>;

        fn debugOutstandingFuseCalls(
            &self,
            mount_point: &PathString,
        ) -> BoxFuture<'static, Result<Vec<FuseCall>, DebugOutstandingFuseCallsError>>;

        fn debugOutstandingNfsCalls(
            &self,
            mount_point: &PathString,
        ) -> BoxFuture<'static, Result<Vec<NfsCall>, DebugOutstandingNfsCallsError>>;

        fn debugOutstandingPrjfsCalls(
            &self,
            mount_point: &PathString,
        ) -> BoxFuture<'static, Result<Vec<PrjfsCall>, DebugOutstandingPrjfsCallsError>>;

        fn debugOutstandingThriftRequests(
            &self,
        ) -> BoxFuture<'static, Result<Vec<ThriftRequestMetadata>, DebugOutstandingThriftRequestsError>>;

        fn debugOutstandingHgEvents(
            &self,
            mount_point: &PathString,
        ) -> BoxFuture<'static, Result<Vec<HgEvent>, DebugOutstandingHgEventsError>>;

        fn debugStartRecordingActivity(
            &self,
            mount_point: &PathString,
            output_dir: &PathString,
        ) -> BoxFuture<'static, Result<ActivityRecorderResult, DebugStartRecordingActivityError>>;

        fn debugStopRecordingActivity(
            &self,
            mount_point: &PathString,
            unique: i64,
        ) -> BoxFuture<'static, Result<ActivityRecorderResult, DebugStopRecordingActivityError>>;

        fn debugListActivityRecordings(
            &self,
            mount_point: &PathString,
        ) -> BoxFuture<'static, Result<ListActivityRecordingsResult, DebugListActivityRecordingsError>>;

        fn debugGetInodePath(
            &self,
            mount_point: &PathString,
            inode_number: i64,
        ) -> BoxFuture<'static, Result<InodePathDebugInfo, DebugGetInodePathError>>;

        fn clearFetchCounts(&self) -> BoxFuture<'static, Result<(), ClearFetchCountsError>>;

        fn clearFetchCountsByMount(
            &self,
            mount_path: &PathString,
        ) -> BoxFuture<'static, Result<(), ClearFetchCountsByMountError>>;

        fn getAccessCounts(
            &self,
            duration: i64,
        ) -> BoxFuture<'static, Result<GetAccessCountsResult, GetAccessCountsError>>;

        fn startRecordingBackingStoreFetch(
            &self,
        ) -> BoxFuture<'static, Result<(), StartRecordingBackingStoreFetchError>>;

        fn stopRecordingBackingStoreFetch(
            &self,
        ) -> BoxFuture<'static, Result<GetFetchedFilesResult, StopRecordingBackingStoreFetchError>>;

        fn debugDropAllPendingRequests(
            &self,
        ) -> BoxFuture<'static, Result<i64, DebugDropAllPendingRequestsError>>;

        fn unloadInodeForPath(
            &self,
            mount_point: &PathString,
            path: &PathString,
            age: &TimeSpec,
        ) -> BoxFuture<'static, Result<i64, UnloadInodeForPathError>>;

        fn debugInvalidateNonMaterialized(
            &self,
            params: &DebugInvalidateRequest,
        ) -> BoxFuture<'static, Result<DebugInvalidateResponse, DebugInvalidateNonMaterializedError>>;

        fn flushStatsNow(&self) -> BoxFuture<'static, Result<(), FlushStatsNowError>>;

        fn invalidateKernelInodeCache(
            &self,
            mount_point: &PathString,
            path: &PathString,
        ) -> BoxFuture<'static, Result<(), InvalidateKernelInodeCacheError>>;

        fn getStatInfo(
            &self,
            params: &GetStatInfoParams,
        ) -> BoxFuture<'static, Result<InternalStats, GetStatInfoError>>;

        fn enableTracing(&self) -> BoxFuture<'static, Result<(), EnableTracingError>>;

        fn disableTracing(&self) -> BoxFuture<'static, Result<(), DisableTracingError>>;

        fn getTracePoints(&self) -> BoxFuture<'static, Result<Vec<TracePoint>, GetTracePointsError>>;

        fn getRetroactiveThriftRequestEvents(
            &self,
        ) -> BoxFuture<
            'static,
            Result<GetRetroactiveThriftRequestEventsResult, GetRetroactiveThriftRequestEventsError>,
        >;

        fn getRetroactiveHgEvents(
            &self,
            params: &GetRetroactiveHgEventsParams,
        ) -> BoxFuture<'static, Result<GetRetroactiveHgEventsResult, GetRetroactiveHgEventsError>>;

        fn getRetroactiveInodeEvents(
            &self,
            params: &GetRetroactiveInodeEventsParams,
        ) -> BoxFuture<'static, Result<GetRetroactiveInodeEventsResult, GetRetroactiveInodeEventsError>>;

        fn injectFault(
            &self,
            fault: &FaultDefinition,
        ) -> BoxFuture<'static, Result<(), InjectFaultError>>;

        fn removeFault(
            &self,
            fault: &RemoveFaultArg,
        ) -> BoxFuture<'static, Result<bool, RemoveFaultError>>;

        fn unblockFault(
            &self,
            info: &UnblockFaultArg,
        ) -> BoxFuture<'static, Result<i64, UnblockFaultError>>;

        fn getBlockedFaults(
            &self,
            request: &GetBlockedFaultsRequest,
        ) -> BoxFuture<'static, Result<GetBlockedFaultsResponse, GetBlockedFaultsError>>;

        fn setPathObjectId(
            &self,
            params: &SetPathObjectIdParams,
        ) -> BoxFuture<'static, Result<SetPathObjectIdResult, SetPathObjectIdError>>;

        fn removeRecursively(
            &self,
            params: &RemoveRecursivelyParams,
        ) -> BoxFuture<'static, Result<(), RemoveRecursivelyError>>;

        fn ensureMaterialized(
            &self,
            params: &EnsureMaterializedParams,
        ) -> BoxFuture<'static, Result<(), EnsureMaterializedError>>;

        fn changesSinceV2(
            &self,
            params: &ChangesSinceV2Params,
        ) -> BoxFuture<'static, Result<ChangesSinceV2Result, ChangesSinceV2Error>>;

        fn startFileAccessMonitor(
            &self,
            params: &StartFileAccessMonitorParams,
        ) -> BoxFuture<'static, Result<StartFileAccessMonitorResult, StartFileAccessMonitorError>>;

        fn stopFileAccessMonitor(
            &self,
        ) -> BoxFuture<'static, Result<StopFileAccessMonitorResult, StopFileAccessMonitorError>>;

        fn sendNotification(
            &self,
            request: &SendNotificationRequest,
        ) -> BoxFuture<'static, Result<SendNotificationResponse, SendNotificationError>>;

        fn listRedirections(
            &self,
            request: &ListRedirectionsRequest,
        ) -> BoxFuture<'static, Result<ListRedirectionsResponse, ListRedirectionsError>>;

        fn getFileContent(
            &self,
            request: &GetFileContentRequest,
        ) -> BoxFuture<'static, Result<GetFileContentResponse, GetFileContentError>>;

        fn cancelRequests(
            &self,
            params: &CancelRequestsParams,
        ) -> BoxFuture<'static, Result<CancelRequestsResponse, CancelRequestsError>>;

        fn getActiveRequests(
            &self,
        ) -> BoxFuture<'static, Result<GetActiveRequestsResponse, GetActiveRequestsError>>;
    }

    #[allow(non_snake_case)]
    impl StreamingEdenService for EdenFsService {
        fn subscribeStreamTemporary(
            &self,
            mount_point: &PathString,
        ) -> BoxFuture<'static, Result<BoxStream<'static, Result<JournalPosition, SubscribeStreamTemporaryStreamError>>, SubscribeStreamTemporaryError>>;

        fn streamJournalChanged(
            &self,
            mount_point: &PathString,
        ) -> BoxFuture<'static, Result<BoxStream<'static, Result<JournalPosition, StreamJournalChangedStreamError>>, StreamJournalChangedError>>;

        fn traceFsEvents(
            &self,
            mount_point: &PathString,
            event_category_mask: i64,
        ) -> BoxFuture<'static, Result<BoxStream<'static, Result<FsEvent, TraceFsEventsStreamError>>, TraceFsEventsError>>;

        fn traceThriftRequestEvents(
            &self,
        ) -> BoxFuture<'static, Result<BoxStream<'static, Result<ThriftRequestEvent, TraceThriftRequestEventsStreamError>>, TraceThriftRequestEventsError>>;

        fn traceHgEvents(
            &self,
            mount_point: &PathString,
        ) -> BoxFuture<'static, Result<BoxStream<'static, Result<HgEvent, TraceHgEventsStreamError>>, TraceHgEventsError>>;

        fn traceInodeEvents(
            &self,
            mount_point: &PathString,
        ) -> BoxFuture<'static, Result<BoxStream<'static, Result<InodeEvent, TraceInodeEventsStreamError>>, TraceInodeEventsError>>;

        fn traceTaskEvents(
            &self,
            request: &TraceTaskEventsRequest,
        ) -> BoxFuture<'static, Result<BoxStream<'static, Result<TaskEvent, TraceTaskEventsStreamError>>, TraceTaskEventsError>>;

        fn streamChangesSince(
            &self,
            params: &StreamChangesSinceParams,
        ) -> BoxFuture<'static, Result<(ChangesSinceResult, BoxStream<'static, Result<ChangedFileResult, StreamChangesSinceStreamError>>), StreamChangesSinceError>>;

        fn streamSelectedChangesSince(
            &self,
            params: &StreamSelectedChangesSinceParams,
        ) -> BoxFuture<'static, Result<(ChangesSinceResult, BoxStream<'static, Result<ChangedFileResult, StreamSelectedChangesSinceStreamError>>), StreamSelectedChangesSinceError>>;

        fn streamStartStatus(
            &self,
        ) -> BoxFuture<'static, Result<(DaemonInfo, BoxStream<'static, Result<EdenStartStatusUpdate, StreamStartStatusStreamError>>), StreamStartStatusError>>;
    }
}
