/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::sync::Arc;

use anyhow::Result;
use smallvec::SmallVec;
use sorted_vector_map::SortedVectorMap;

/// SortedVectorTrieMap is a wrapper around a SortedVectorMap with a smallvec key which can be used as a TrieMap wherever TrieMapOps are required.
/// This tracks the common prefix and subset of the map using pointers, so it is more efficient than converting the map to a trie map.
#[derive(Clone, Default)]
pub struct SortedVectorTrieMap<V> {
    /// The underlying entries of the full map.  These are shared between triemap instances for the same original map.
    pub entries: Arc<Vec<(SmallVec<[u8; 24]>, V)>>,
    /// The length of the common prefix of the values in the trie map.  All entries between start and end will be at least this length.
    pub prefix: usize,
    /// The start of the subrange in entries.
    pub start: usize,
    /// The end of the subrange in entries.  This is exclusive, so `entries[end]` is not included.
    pub end: usize,
}

impl<V> SortedVectorTrieMap<V> {
    pub fn new(entries: SortedVectorMap<SmallVec<[u8; 24]>, V>) -> Self {
        let entries = entries.into_inner();
        Self {
            prefix: 0,
            start: 0,
            end: entries.len(),
            entries: Arc::new(entries),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }
}

impl<V: Clone> SortedVectorTrieMap<V> {
    fn make_subtrie(&self, start: usize, end: usize) -> Self {
        let prefix = self.prefix + 1;
        Self {
            prefix,
            start,
            end,
            entries: self.entries.clone(),
        }
    }

    pub fn expand(self) -> Result<(Option<V>, Vec<(u8, Self)>)> {
        let mut value = None;
        let mut subtries = Vec::new();
        let mut range = &self.entries[self.start..self.end];
        let mut start = self.start;
        if let Some((first, rest)) = range.split_first() {
            if first.0.len() == self.prefix {
                // The first item has the same length as the prefix, so it is the value at this point.
                value = Some(first.1.clone());
                start += 1;
                range = rest;
            };
        }
        if range.is_empty() {
            // Nothing else to do
        } else if range.len() == 1
            || range.first().map(|(k, _)| k[self.prefix])
                == range.last().map(|(k, _)| k[self.prefix])
        {
            // Fast path: single range
            subtries.push((
                range
                    .first()
                    .map(|(k, _)| k[self.prefix])
                    .expect("should have an entry as range is not empty"),
                self.make_subtrie(start, self.end),
            ));
        } else {
            // Slow path: multiple ranges
            let mut cur_key = None;
            let mut cur_start = start;
            for (index, (name, _)) in range.iter().enumerate() {
                let key = name[self.prefix];
                if let Some(prev_key) = cur_key {
                    if key == prev_key {
                        continue;
                    }
                    subtries.push((prev_key, self.make_subtrie(cur_start, start + index)));
                }
                cur_key = Some(key);
                cur_start = start + index;
            }
            if let Some(prev_key) = cur_key {
                subtries.push((prev_key, self.make_subtrie(cur_start, self.end)));
            }
        }
        Ok((value, subtries))
    }
}

impl<V: PartialEq> PartialEq for SortedVectorTrieMap<V> {
    fn eq(&self, other: &Self) -> bool {
        let self_slice = &self.entries[self.start..self.end];
        let other_slice = &other.entries[other.start..other.end];
        if self_slice.len() != other_slice.len() {
            return false;
        }
        self_slice
            .iter()
            .zip(other_slice.iter())
            .all(|((key_a, value_a), (key_b, value_b))| {
                key_a[self.prefix..] == key_b[other.prefix..] && value_a == value_b
            })
    }
}

impl<V: Eq> Eq for SortedVectorTrieMap<V> {}

impl<V: std::fmt::Debug> std::fmt::Debug for SortedVectorTrieMap<V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SortedVectorTrieMap")
            .field("prefix", &self.prefix)
            .field("start", &self.start)
            .field("end", &self.end)
            .field("entries", &&self.entries[self.start..self.end])
            .finish()
    }
}

pub struct SortedVectorTrieMapIter<V>(SortedVectorTrieMap<V>);

impl<V: Clone> Iterator for SortedVectorTrieMapIter<V> {
    type Item = (SmallVec<[u8; 24]>, V);

    fn next(&mut self) -> Option<Self::Item> {
        if self.0.start < self.0.end {
            let (mut name, value) = self.0.entries[self.0.start].clone();
            name.drain(..self.0.prefix);
            self.0.start += 1;
            Some((name, value))
        } else {
            None
        }
    }
}

impl<V: Clone> IntoIterator for SortedVectorTrieMap<V> {
    type Item = (SmallVec<[u8; 24]>, V);
    type IntoIter = SortedVectorTrieMapIter<V>;

    fn into_iter(self) -> Self::IntoIter {
        SortedVectorTrieMapIter(self)
    }
}

#[cfg(test)]
mod tests {
    use quickcheck::quickcheck;

    use super::*;
    use crate::MPathElement;
    use crate::TrieMap;

    macro_rules! check_eq {
        ($a:expr, $b:expr) => {
            if ($a) != ($b) {
                return false;
            }
        };
    }

    quickcheck! {
        fn quickcheck_sorted_vector_trie_map(entries: SortedVectorMap<MPathElement, u8>) -> bool {
            let entries = entries
                .into_iter()
                .map(|(k, v)| (k.to_smallvec(), v))
                .collect::<SortedVectorMap<_, _>>();

            let trie_map = TrieMap::from_iter(entries.clone());
            let sv_trie_map = SortedVectorTrieMap::new(entries);

            check_eq!(trie_map.is_empty(), sv_trie_map.is_empty());

            let all_trie_map = trie_map.clone().into_iter().collect::<Vec<_>>();
            let all_sv_trie_map = sv_trie_map.clone().into_iter().collect::<Vec<_>>();
            check_eq!(all_trie_map, all_sv_trie_map);

            let mut expansions = vec![(trie_map, sv_trie_map)];

            while let Some((trie_map, sv_trie_map)) = expansions.pop() {
                let (sv_value, sv_subtries) = sv_trie_map.expand().unwrap();
                let (value, subtries) = trie_map.expand();

                check_eq!(value, sv_value);
                check_eq!(subtries.len(), sv_subtries.len());
                for (subtrie, sv_subtrie) in subtries.into_iter().zip(sv_subtries) {
                    check_eq!(subtrie.0, sv_subtrie.0);
                    expansions.push((subtrie.1, sv_subtrie.1));
                }
            }

            true
        }
    }
}
