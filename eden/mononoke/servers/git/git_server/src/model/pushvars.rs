/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;

use bytes::Bytes;
use gotham_derive::StateData;

const WAIT_FOR_WBC_UPDATE: &str = "x-git-read-after-write-consistency";
const METAGIT_BYPASS_ALL_HOOKS: &str = "x-metagit-bypass-hooks";
const USE_ONLY_OFFSET_DELTA: &str = "x-git-only-offset-delta";
const PUSH_CONCURRENCY: &str = "x-git-push-concurrency";
const BYPASS_BOOKMARK_CACHE: &str = "x-git-bypass-bookmark-cache";
const UNSAMPLED_PERF_LOGGING: &str = "x-git-unsampled-perf-logging";
#[cfg_attr(
    not(fbcode_build),
    expect(
        dead_code,
        reason = "Only RL Land Service diversion reads these, in internal builds"
    )
)]
const EMERGENCY_PUSH: &str = "x-git-emergency-push";
#[cfg_attr(
    not(fbcode_build),
    expect(
        dead_code,
        reason = "Only RL Land Service diversion reads these, in internal builds"
    )
)]
const ALLOW_NON_FAST_FORWARD: &str = "x-git-allow-non-ffwd-push";
const ALLOW_DANGLING_LFS_POINTERS: &str = "x-git-allow-dangling-lfs-pointers";

#[derive(Clone, StateData)]
pub struct Pushvars(HashMap<String, Bytes>);

impl Pushvars {
    pub fn new(pushvars: HashMap<String, Bytes>) -> Self {
        let pushvars = pushvars
            .into_iter()
            .map(|(name, value)| {
                if name.as_str() == METAGIT_BYPASS_ALL_HOOKS {
                    // Mononoke doesn't understand Metagit bypass pushvar, so update it accordingly
                    ("BYPASS_ALL_HOOKS".to_string(), value)
                } else {
                    (name, value)
                }
            })
            .collect();
        Self(pushvars)
    }

    pub fn wait_for_wbc_update(&self) -> bool {
        self.0
            .get(WAIT_FOR_WBC_UPDATE)
            .is_some_and(|v| **v == *b"1")
    }

    pub fn use_only_offset_delta(&self) -> bool {
        self.0
            .get(USE_ONLY_OFFSET_DELTA)
            .is_some_and(|v| **v == *b"1")
    }

    pub fn bypass_bookmark_cache(&self) -> bool {
        self.0
            .get(BYPASS_BOOKMARK_CACHE)
            .is_some_and(|v| **v == *b"1")
    }

    pub fn concurrency(&self) -> usize {
        self.0
            .get(PUSH_CONCURRENCY)
            .and_then(|v| String::from_utf8_lossy(v).parse().ok())
            .unwrap_or(100)
            .clamp(10, 500)
    }

    pub fn unsampled_perf_logging(&self) -> bool {
        self.0
            .get(UNSAMPLED_PERF_LOGGING)
            .is_some_and(|v| **v == *b"1")
    }

    #[cfg_attr(
        not(fbcode_build),
        expect(
            dead_code,
            reason = "Only RL Land Service diversion reads this, in internal builds"
        )
    )]
    pub fn emergency_push(&self) -> bool {
        self.0.get(EMERGENCY_PUSH).is_some_and(|v| **v == *b"1")
    }

    /// Mirrors `bookmarks_movement::ALLOW_NON_FFWD_PUSHVAR`: presence of the
    /// header is sufficient (value is not inspected), so the diverted MRL
    /// path behaves identically to the local push path.
    #[cfg_attr(
        not(fbcode_build),
        expect(
            dead_code,
            reason = "Only RL Land Service diversion reads this, in internal builds"
        )
    )]
    pub fn allow_non_fast_forward(&self) -> bool {
        self.0.contains_key(ALLOW_NON_FAST_FORWARD)
    }

    /// When set, the git server should accept LFS pushes whose pointer
    /// content is not (yet) present in the Mononoke filestore (internal mode)
    /// or in the upstream LFS server. The pointer text itself becomes the
    /// file content in the bonsai and the file is marked as
    /// `GitLfs::FullContent`. Presence of the header is sufficient — the
    /// value is not inspected. When the content *is* available, this flag
    /// changes nothing: the pointer is still resolved and interpreted as LFS.
    pub fn allow_dangling_lfs_pointers(&self) -> bool {
        self.0.contains_key(ALLOW_DANGLING_LFS_POINTERS)
    }
}

impl AsRef<HashMap<String, Bytes>> for Pushvars {
    fn as_ref(&self) -> &HashMap<String, Bytes> {
        &self.0
    }
}
