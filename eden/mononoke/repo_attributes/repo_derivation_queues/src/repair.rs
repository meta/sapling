/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use mononoke_types::ChangesetId;
use mononoke_types::DerivableType;
use serde::Serialize;

use crate::DagItemId;

pub struct RepairOptions {
    pub derived_data_types: Vec<DerivableType>,
    /// Empty means discover isolated stranded items.
    pub roots: Vec<ChangesetId>,
    pub apply: bool,
    pub limit: usize,
}

#[derive(Debug, Serialize)]
pub struct RepairResult {
    pub id: DagItemId,
    pub head: ChangesetId,
    pub needs_derivation: bool,
}
