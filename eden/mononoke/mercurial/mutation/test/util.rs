/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::cmp::Ordering;
use std::collections::HashMap;
use std::collections::HashSet;

use anyhow::Result;
use context::CoreContext;
use mercurial_mutation::HgMutationEntry;
use mercurial_mutation::HgMutationStore;
use mercurial_types::HgChangesetId;

fn compare_entries(a: &HgMutationEntry, b: &HgMutationEntry) -> Ordering {
    a.successor().cmp(b.successor())
}

pub(crate) fn get_entries(
    entries: &HashMap<usize, HgMutationEntry>,
    indexes: &[usize],
) -> Vec<HgMutationEntry> {
    let mut result = Vec::new();
    for index in indexes.iter() {
        if let Some(entry) = entries.get(index) {
            result.push(entry.clone());
        }
    }
    result
}

fn get_successors(entries: &[HgMutationEntry]) -> Vec<&HgChangesetId> {
    entries.iter().map(|entry| entry.successor()).collect()
}

pub(crate) async fn check_entries_following_splits(
    store: &dyn HgMutationStore,
    ctx: &CoreContext,
    changeset_ids: HashSet<HgChangesetId>,
    entries: &HashMap<usize, HgMutationEntry>,
    indexes: &[usize],
) -> Result<()> {
    let mut fetched = HashSet::new();
    for changeset_id in changeset_ids {
        let history = store
            .all_predecessors_following_splits(ctx, changeset_id)
            .await?;
        assert_eq!(history.len(), history.iter().collect::<HashSet<_>>().len());
        fetched.extend(history);
    }
    assert_eq!(fetched, get_entries(entries, indexes).into_iter().collect());
    Ok(())
}

pub(crate) async fn check_entries(
    store: &dyn HgMutationStore,
    ctx: &CoreContext,
    changeset_ids: HashSet<HgChangesetId>,
    entries: &HashMap<usize, HgMutationEntry>,
    indexes: &[usize],
) -> Result<()> {
    let mut fetched_entries = store.all_predecessors(ctx, changeset_ids).await?;
    let mut expected_entries = get_entries(entries, indexes);
    fetched_entries.sort_unstable_by(compare_entries);
    expected_entries.sort_unstable_by(compare_entries);
    assert_eq!(
        get_successors(&fetched_entries),
        get_successors(&expected_entries)
    );
    assert_eq!(fetched_entries, expected_entries);
    Ok(())
}
