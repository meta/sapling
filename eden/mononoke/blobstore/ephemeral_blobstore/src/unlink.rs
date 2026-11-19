/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::num::NonZeroUsize;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnlinkRetry {
    pub max_attempts: NonZeroUsize,
    pub initial_delay: Duration,
    pub max_delay: Duration,
}
