/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

static LOAD_PARENT_TREE_ENTRY: AtomicBool = AtomicBool::new(true);

/// Set whether persisting a tree loads the p1 tree entry when it isn't cached, so that the ACL
/// children can be migrated. If the load fails, persisting fails.
pub fn set_load_parent_tree_entry(value: bool) {
    LOAD_PARENT_TREE_ENTRY.store(value, Ordering::Release);
}

pub(crate) fn load_parent_tree_entry() -> bool {
    LOAD_PARENT_TREE_ENTRY.load(Ordering::Acquire)
}
