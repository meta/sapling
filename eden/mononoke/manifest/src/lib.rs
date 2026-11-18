/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

pub use blobstore::StoreLoadable;

pub use crate::bonsai::BonsaiDiffFileChange;
pub use crate::bonsai::ManifestLeaf;
pub use crate::bonsai::bonsai_diff;
pub use crate::bonsai::bonsai_diff_sharded;
pub use crate::combined::Combined;
pub use crate::combined::CombinedId;
pub use crate::comparison::ManifestComparison;
pub use crate::comparison::Span;
pub use crate::comparison::compare_manifest;
pub use crate::deferred::Deferred;
pub use crate::derive::KnownEntry;
pub use crate::derive::LeafInfo;
pub use crate::derive::TreeInfo;
pub use crate::derive::TreeInfoSubentries;
pub use crate::derive::derive_manifest;
pub use crate::derive::derive_manifest_subentries;
pub use crate::derive::derive_manifest_with_known_entries;
pub use crate::derive::flatten_subentries;
pub use crate::derive_batch::ManifestChanges;
pub use crate::derive_batch::derive_manifests_for_simple_stack_of_commits;
pub use crate::derive_from_predecessor::FromPredecessorLeafInfo;
pub use crate::derive_from_predecessor::FromPredecessorTreeInfo;
pub use crate::derive_from_predecessor::derive_manifest_from_predecessor;
pub use crate::implicit_deletes::ImplicitDeletesAndExistingFiles;
pub use crate::implicit_deletes::get_implicit_deletes;
pub use crate::implicit_deletes::get_implicit_deletes_and_existing_files;
pub use crate::ops::Diff;
pub use crate::ops::ManifestOps;
pub use crate::ops::find_entries_from_subentries;
pub use crate::ops::find_intersection_of_diffs;
pub use crate::ops::find_intersection_of_diffs_and_parents;
pub use crate::ops::find_intersection_of_diffs_and_parents_pruned;
pub use crate::ops::find_intersection_of_diffs_pruned;
pub use crate::ops::find_intersection_of_subentry_diffs;
pub use crate::ordered_ops::After;
pub use crate::ordered_ops::ManifestOrderedOps;
pub use crate::path_tree::PathTree;
pub use crate::select::PathOrPrefix;
pub use crate::traced::Traced;
pub use crate::trie_map_ops::TrieMapOps;
pub use crate::types::Entry;
pub use crate::types::Manifest;
pub use crate::types::OrderedManifest;

mod bonsai;
mod combined;
mod comparison;
mod deferred;
mod derive;
mod derive_batch;
mod derive_from_predecessor;
mod implicit_deletes;
mod ops;
mod ordered_ops;
mod path_tree;
mod select;
mod traced;
mod trie_map_ops;
mod types;

#[cfg(test)]
mod tests;
