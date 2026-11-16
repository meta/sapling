/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use strum::IntoStaticStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, IntoStaticStr)]
#[strum(serialize_all = "kebab_case")]
#[repr(u32)]
pub enum EdenThriftMethod {
    GetAttributesFromFilesV2,
    GetFileContent,
    ChangesSinceV2,
    GetRegexCounters,
    GetSelectedCounters,
    GetCounters,
    GetCounter,
    PrefetchFilesV2,
    GlobFiles,
    ReadDir,
    GetAccessCounts,
    GetScmStatusV2,
    UnmountV2,
    Unmount,
    GetCurrentJournalPosition,
    StreamJournalChanged,
    GetConfig,
    StartFileAccessMonitor,
    StopFileAccessMonitor,
    StartRecordingBackingStoreFetch,
    StopRecordingBackingStoreFetch,
    FlushStatsNow,
    GetCurrentSnapshotInfo,
    AddBindMount,
    RemoveBindMount,
    ListMounts,
    StreamStartStatus,
    GetDaemonInfo,
    GetRegexExportedValues,
    Unknown,
}

impl EdenThriftMethod {
    pub fn name(&self) -> &'static str {
        self.into()
    }
}
