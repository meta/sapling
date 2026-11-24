/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::BTreeSet;
use std::collections::BinaryHeap;
use std::collections::VecDeque;
use std::fmt;
use std::fmt::Debug;
use std::fmt::Formatter;
use std::ops::Deref;
#[cfg(any(test, feature = "indexedlog-backend"))]
use std::path::Path;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use indexmap::set::IndexSet;
use serde::Deserialize;
use serde::Serialize;
use tracing::debug;
use tracing::trace;

use crate::Error::Programming;
use crate::IdSegment;
use crate::IdSet;
use crate::IdSpan;
use crate::Level;
use crate::Result;
use crate::VerLink;
use crate::errors::NotFoundError;
use crate::errors::bug;
use crate::id::Group;
use crate::id::Id;
use crate::iddagstore::IdDagStore;
#[cfg(any(test, feature = "indexedlog-backend"))]
use crate::iddagstore::IndexedLogStore;
use crate::iddagstore::MemStore;
use crate::idset;
use crate::idset::Span;
use crate::ops::Persist;
use crate::ops::StorageVersion;
#[cfg(any(test, feature = "indexedlog-backend"))]
use crate::ops::TryClone;
use crate::segment::FlatSegment;
use crate::segment::PreparedFlatSegments;
use crate::segment::Segment;
use crate::segment::SegmentFlags;
use crate::types_ext::PreparedFlatSegmentsExt;

/// Structure to store a DAG of integers, with indexes to speed up ancestry queries.
///
/// A segment is defined as `(level: int, low: int, high: int, parents: [int])` on
/// a topo-sorted integer DAG. It covers all integers in `low..=high` range, and
/// must satisfy:
/// - `high` is the *only* head in the sub DAG covered by the segment.
/// - `parents` do not have entries within `low..=high` range.
/// - If `level` is 0, for any integer `x` in `low+1..=high` range, `x`'s parents
///   must be `x - 1`.
///
/// See `slides/201904-segmented-changelog/segmented-changelog.pdf` for pretty
/// graphs about how segments help with ancestry queries.
///
/// [`IdDag`] is often used together with [`IdMap`] to allow customized names
/// on vertexes. The [`Dag`] type provides an easy-to-use interface to
/// keep [`IdDag`] and [`IdMap`] in sync.
#[derive(Clone, Serialize, Deserialize)]
pub struct IdDag<Store> {
    pub(crate) store: Store,
    #[serde(skip, default = "default_seg_size")]
    new_seg_size: usize,
    #[serde(skip, default = "VerLink::new")]
    version: VerLink,
}

/// See benches/segment_sizes.rs (D16660078) for this choice.
static DEFAULT_SEG_SIZE: AtomicUsize = AtomicUsize::new(16);

/// Maximum meaningful level. 4 is chosen because it is good enough
/// for an existing large repo (level 5 is not built because it
/// cannot merge level 4 segments).
const MAX_MEANINGFUL_LEVEL: Level = 4;

#[cfg(any(test, feature = "indexedlog-backend"))]
impl IdDag<IndexedLogStore> {
    /// Open [`IdDag`] at the given directory. Create it on demand.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let store = IndexedLogStore::open(path)?;
        Self::open_from_store(store)
    }
}

impl<S> IdDag<S> {
    /// Set the maximum size of a new high-level segment.
    ///
    /// This does not affect existing segments.
    ///
    /// This might help performance a bit for certain rare types of DAGs.
    /// The default value is Usually good enough.
    pub fn set_new_segment_size(&mut self, size: usize) {
        self.new_seg_size = size.max(2);
    }

    /// Get the segment size used for building new high-level segments.
    pub(crate) fn get_new_segment_size(&self) -> usize {
        self.new_seg_size
    }
}

#[cfg(any(test, feature = "indexedlog-backend"))]
impl TryClone for IdDag<IndexedLogStore> {
    /// Attempt to clone the `IdDag`.
    fn try_clone(&self) -> Result<Self> {
        let store = self.store.try_clone()?;
        Ok(Self {
            store,
            new_seg_size: self.new_seg_size,
            version: self.version.clone(),
        })
    }
}

impl IdDag<MemStore> {
    /// Instantiate an [`IdDag`] that stores all it's data in process. Useful for scenarios that
    /// do not require data persistence.
    pub fn new_in_memory() -> Self {
        let store = MemStore::new();
        Self {
            store,
            new_seg_size: default_seg_size(),
            version: VerLink::new(),
        }
    }
}

impl<Store: IdDagStore> IdDag<Store> {
    pub(crate) fn open_from_store(store: Store) -> Result<Self> {
        let version = store.verlink();
        let dag = Self {
            store,
            new_seg_size: default_seg_size(),
            version,
        };
        Ok(dag)
    }
}

impl<Store: IdDagStore> IdDag<Store> {
    /// Add a new segment.
    ///
    /// For simplicity, it does not check if the new segment overlaps with
    /// an existing segment (which is a logic error). Those checks can be
    /// offline.
    pub(crate) fn insert(
        &mut self,
        flags: SegmentFlags,
        level: Level,
        low: Id,
        high: Id,
        parents: &[Id],
    ) -> Result<()> {
        if !low.is_virtual() {
            self.version.bump();
        }
        self.store.insert(flags, level, low, high, parents)
    }

    /// Returns whether the iddag contains segments for the given `id`.
    pub fn contains_id(&self, id: Id) -> Result<bool> {
        Ok(self.all_with_virtual()?.contains(id))
    }

    pub(crate) fn version(&self) -> &VerLink {
        &self.version
    }
}

// Build segments.
impl<Store: IdDagStore> IdDag<Store> {
    /// Make sure the [`IdDag`] contains the given id (and all its ancestor ids)
    /// by building up segments on demand.
    ///
    /// `get_parents` describes the DAG. Its input and output are `Id`s.
    ///
    /// This is often used together with [`crate::idmap::IdMap`].
    ///
    /// This is inefficient. If you have `PreparedFlatSegments`, call
    /// `build_flat_segments_from_prepared_flat_segments` instead.
    pub fn build_segments<F>(&mut self, high: Id, get_parents: &F) -> Result<usize>
    where
        F: Fn(Id) -> Result<Vec<Id>>,
    {
        // Step 1, figure out id ranges to be inserted using DFS.
        // We cannot call `push_edge` like step 2 here, because the
        // `id`s are in DESC order. `push_edge` really need ASC order.
        let old_ids = self.all_ids_in_segment_level(0)?;
        let mut to_insert_ids = IdSet::empty();
        let mut to_visit = vec![high];
        while let Some(id) = to_visit.pop() {
            if old_ids.contains(id) || to_insert_ids.contains(id) {
                continue;
            }
            to_insert_ids.push(id);
            let parent_ids = get_parents(id)?;
            to_visit.extend_from_slice(&parent_ids);
        }

        // Step 2, convert the to_insert_ids to PreparedFlatSegments.
        // Iterate them in ascending order so `push_edge` works efficiently.
        let mut prepared = PreparedFlatSegments::default();
        for id in to_insert_ids.iter_asc() {
            let parent_ids = get_parents(id)?;
            prepared.push_edge(id, &parent_ids);
        }

        // Step 3, call build_segments_from_prepared_flat_segments.
        self.build_segments_from_prepared_flat_segments(&prepared)
    }

    /// Similar to `build_segments`, but takes `PreparedFlatSegments` instead
    /// of `get_parents`.
    pub fn build_segments_from_prepared_flat_segments(
        &mut self,
        outcome: &PreparedFlatSegments,
    ) -> Result<usize> {
        let (count, set) = self.build_flat_segments_from_prepared_flat_segments(outcome)?;
        let count = count + self.build_all_high_level_segments(Level::MAX, set)?;
        Ok(count)
    }

    /// Build flat segments using the outcome from `add_head`.
    /// This is not public because it does not keep high-level segments in sync.
    ///
    /// Return `(count, set)`. Number of segments inserted, and `IdSet` covered
    /// by inserted segments.
    fn build_flat_segments_from_prepared_flat_segments(
        &mut self,
        outcome: &PreparedFlatSegments,
    ) -> Result<(usize, IdSet)> {
        let mut inserted_id_set = IdSet::empty();
        if outcome.segments.is_empty() {
            return Ok((0, inserted_id_set));
        }

        let mut head_ids: BTreeSet<Id> = self.heads(self.master_group()?)?.iter_desc().collect();
        let mut covered = self.all_ids_in_groups(&[Group::MASTER])?;
        let mut get_flags = |parents: &[Id], head: Id, covered: &IdSet| {
            let mut flags = SegmentFlags::empty();
            if parents.is_empty() {
                flags |= SegmentFlags::HAS_ROOT
            }
            if head.group() == Group::MASTER {
                for p in parents.iter() {
                    head_ids.remove(p);
                }
                // ONLY_HEAD means it heads(0..=head) = [head], or ancestors([head]) = 0..=head.
                // The 0..=head range cannot have gaps. Because other segments
                // might be inserted to the gaps later and they will have heads.
                let has_no_gap = match covered.iter_span_asc().next() {
                    Some(span) => span.contains(head),
                    None => false,
                };
                if has_no_gap && head_ids.range(..=head).next().is_none() {
                    flags |= SegmentFlags::ONLY_HEAD;
                }
                head_ids.insert(head);
            }
            flags
        };
        let mut last_high = None;
        for seg in &outcome.segments {
            if let Some(last_high) = last_high {
                if last_high >= seg.low {
                    return bug(format!(
                        "PreparedFlatSegments are not sorted: {:?}",
                        outcome.segments
                    ));
                }
            }
            last_high = Some(seg.high);
            covered.push(seg.low..=seg.high);

            let flags = get_flags(&seg.parents, seg.high, &covered);
            tracing::trace!(
                "inserting flat segment {}..={} {:?} {:?}",
                seg.low,
                seg.high,
                &seg.parents,
                &flags
            );
            inserted_id_set.push(seg.low..=seg.high);
            self.insert(flags, 0, seg.low, seg.high, &seg.parents)?;
        }
        Ok((outcome.segments.len(), inserted_id_set))
    }

    /// Incrementally build high level segments at the given `level`.
    ///
    /// The new, high level segments are built on top of the lower level
    /// (`level - 1`) segments. Each high level segment covers at most `size`
    /// `level - 1` segments.
    ///
    /// The last segment per level per group is dropped because it's likely to
    /// be incomplete. This helps reduce fragmentation, and also allows the last
    /// flat segment per group to be mutable, because it will not be included
    /// in Level 1 segments.
    ///
    /// `inserted_lower_level_id_set` specifies newly inserted segments at the
    /// lower level.
    ///
    /// Return `(count, set)`. `count` is the number of segments inserted.
    /// `set` is an `IdSet` that covers ranges of segments inserted.
    fn build_high_level_segments(
        &mut self,
        level: Level,
        inserted_lower_level_id_set: &IdSet,
    ) -> Result<(usize, IdSet)> {
        // Exclude VIRTUAL spans.
        let inserted_lower_level_id_set = inserted_lower_level_id_set
            .difference(&Span::new(Group::VIRTUAL.min_id(), Group::VIRTUAL.max_id()).into());
        let mut inserted_id_set = IdSet::empty();
        if level == 0 {
            // Do nothing. Level 0 is not considered high level.
            return Ok((0, inserted_id_set));
        }
        let size = self.new_seg_size;

        // Figure out lower level segments to consider.
        //
        // Legend:
        //  [...] existing (one or more) segments
        //  [N] newly inserted (in inserted_lower_level_id_set)
        //
        // Flat segments:          [..............] gap [.......] gap [...........]
        // Lower level segments:   [.......][ N ]       [.....]       [....][ N ]
        // This level segments:    [.....]              [...]         [..]
        //   missing:                     [     ]            []           [     ]
        //   need_consider:               [     ]                         [     ]
        //                                                    ^
        //                   no need to consider because it does not have [N]

        let missing = self
            .all_ids_in_segment_level(level - 1)?
            .difference(&self.all_ids_in_segment_level(level)?);
        let need_consider = IdSet::from_sorted_spans(
            missing
                .as_spans()
                .iter()
                .filter(|s| inserted_lower_level_id_set.contains(s.high))
                .copied(),
        );
        tracing::debug!(
            "building lv{} segment from lv{} ranges {:?}",
            level,
            level - 1,
            &need_consider
        );

        let mut insert_count = 0;
        let mut new_segments_per_considering_span = Vec::new();
        let mut lower_segments_len = 0;
        for considering_span in need_consider.as_spans() {
            tracing::trace!(" considering {:?}", &considering_span);
            // `get_parents` is on the previous level of segments.
            let get_parents = |head: Id| -> Result<Vec<Id>> {
                if let Some(seg) = self.find_segment_by_head_and_level(head, level - 1)? {
                    seg.parents()
                } else {
                    bug("get_parents called with wrong head in build_high_level_segments")
                }
            };

            let new_segments = {
                // Find all segments on the previous level that haven't been built.
                let segments: Vec<_> =
                    self.segments_in_span_ascending(*considering_span, level - 1)?;
                tracing::trace!("  in lower-level: {:?}", &segments);
                lower_segments_len += segments.len();

                // Sanity check: They should be sorted and not overlapping.
                for i in 1..segments.len() {
                    if segments[i - 1].high()? >= segments[i].span()?.low {
                        let msg = format!(
                            "level {} segments {:?} are overlapping or not sorted!",
                            level,
                            &segments[i - 1..=i]
                        );
                        return bug(msg);
                    }
                }

                // Build the graph from the first head. `low_idx` is the
                // index of `segments` (level - 1).

                // find_segment scans low level segments (segments[low_idx..]),
                // merges them on the fly, and returns a high-level segment:
                // (new_idx, low, high, parents, has_root).
                // new_idx + 1 is the next low_idx that should be passed to find_segment
                // to calculate the next high-level segment.
                let find_segment = |low_idx: usize| -> Result<_> {
                    let segment_low = segments[low_idx].span()?.low;
                    let mut heads = BTreeSet::new();
                    let mut parents = IndexSet::new();
                    let mut candidate = None;
                    let mut has_root = false;
                    for i in low_idx..segments.len().min(low_idx + size) {
                        // [--------------------------] level (to build)
                        // [------] [------] [--------] level - 1 (lower level)
                        // ^        ^
                        // |        segments[i].low
                        // segment_low
                        let span = segments[i].span()?;
                        // Discontinuous?
                        if i > low_idx && segments[i - 1].head()? + 1 != span.low {
                            break;
                        }
                        let head = span.high;
                        if !has_root && segments[i].has_root()? {
                            has_root = true;
                        }
                        heads.insert(head);
                        let direct_parents = get_parents(head)?;
                        for p in &direct_parents {
                            if *p >= span.low {
                                return bug(format!(
                                    "invalid lv{} segment: {:?} (parent >= low)",
                                    level - 1,
                                    segments[i]
                                ));
                            }
                            if *p < segment_low {
                                // No need to remove p from heads, since it cannot be a head.
                                parents.insert(*p);
                            } else {
                                heads.remove(p);
                            }
                        }
                        if heads.len() == 1 {
                            candidate = Some((i, segment_low, head, parents.len(), has_root));
                        }
                    }
                    // There must be at least one valid high-level segment,
                    // because `segments[low_idx]` is such a high-level segment.
                    let (new_idx, low, high, parent_count, has_root) = candidate.unwrap();
                    let parents = parents.into_iter().take(parent_count).collect::<Vec<Id>>();
                    Ok((new_idx, low, high, parents, has_root))
                };

                let mut idx = 0;
                let mut new_segments = Vec::new();
                while idx < segments.len() {
                    let segment_info = find_segment(idx)?;
                    idx = segment_info.0 + 1;
                    new_segments.push(segment_info);
                }

                tracing::trace!("  new segments: {:?}", &new_segments);
                new_segments
            };

            new_segments_per_considering_span.push(new_segments);
        }

        // No point to introduce new levels if it has the same segment count
        // as the lower level.
        if level > self.max_level()?
            && new_segments_per_considering_span
                .iter()
                .map(|s| s.len())
                .sum::<usize>()
                >= lower_segments_len
        {
            tracing::debug!("no need to introduce new level");
            return Ok((0, inserted_id_set));
        }

        for mut new_segments in new_segments_per_considering_span {
            // Drop the last segment. It could be incomplete.
            new_segments.pop();

            insert_count += new_segments.len();

            for (_, low, high, parents, has_root) in new_segments {
                let flags = if has_root {
                    SegmentFlags::HAS_ROOT
                } else {
                    SegmentFlags::empty()
                };
                tracing::trace!(
                    " inserting lv{} segment {}..{} {:?} {:?}",
                    level,
                    low,
                    high,
                    &parents,
                    &flags
                );
                inserted_id_set.push(low..=high);
                self.insert(flags, level, low, high, &parents)?;
            }
        }

        Ok((insert_count, inserted_id_set))
    }

    /// Build high level segments using default setup.
    ///
    /// `new_flat_id_set` covers the flat segments newly inserted.
    ///
    /// Return number of segments inserted.
    fn build_all_high_level_segments(
        &mut self,
        max_level: Level,
        new_flat_id_set: IdSet,
    ) -> Result<usize> {
        let mut total = 0;
        let max_level = max_level.min(MAX_MEANINGFUL_LEVEL);
        let mut new_id_set = new_flat_id_set;
        for level in 1..=max_level {
            let (count, new_ids) = self.build_high_level_segments(level, &new_id_set)?;
            tracing::debug!("new {} lv{} segments: {:?}", count, level, &new_ids);
            if count == 0 {
                break;
            }
            new_id_set = new_ids;
            total += count;
        }
        Ok(total)
    }
}

impl<Store: IdDagStore> IdDag<Store> {
    /// Returns the [`FlatSegment`] entries that are used by this [`IdDag`].
    pub fn flat_segments(&self, group: Group) -> Result<PreparedFlatSegments> {
        let segments = self.flat_segments_range(group.min_id(), group.max_id())?;
        Ok(PreparedFlatSegments {
            segments: segments.into_iter().collect(),
        })
    }

    /// Return all flat segments that overlap with range (and potentially cover
    /// larger range than supplied).
    fn flat_segments_range(&self, min: Id, max_incl: Id) -> Result<Vec<FlatSegment>> {
        let level = 0;
        let mut segments = Vec::new();
        for sr in self.iter_segments_ascending(min, level)? {
            let segment = sr?;
            let span = segment.span()?;
            if span.low > max_incl {
                break;
            }
            let fs = FlatSegment {
                low: span.low,
                high: span.high,
                parents: segment.parents()?,
            };
            segments.push(fs);
        }
        Ok(segments)
    }

    /// Extract flat segments that cover the given `set` exactly.
    pub fn idset_to_flat_segments(&self, set: IdSet) -> Result<PreparedFlatSegments> {
        let mut segments = Vec::new();

        let (min, max) = if let (Some(min), Some(max)) = (set.min(), set.max()) {
            (min, max)
        } else {
            return Ok(PreparedFlatSegments {
                segments: segments.into_iter().collect(),
            });
        };
        let segs = self.flat_segments_range(min, max)?;
        let seg_iter = segs.into_iter().rev();

        let push = |seg: FlatSegment| segments.push(seg);
        let span_iter = set.as_spans().iter().cloned();
        idset::intersect_iter(seg_iter, span_iter, push);

        Ok(PreparedFlatSegments {
            segments: segments.into_iter().collect(),
        })
    }
}

// User-facing DAG-related algorithms.
pub trait IdDagAlgorithm: IdDagStore {
    /// Return a [`IdSet`] that covers all ids stored in this [`IdDag`],
    /// i.e. everything including the VIRTUAL group.
    fn all_with_virtual(&self) -> Result<IdSet> {
        // Intentionally skip the VIRTUAL group.
        self.all_ids_in_groups(&Group::ALL)
    }

    /// Return a [`IdSet`] that covers persistable ids stored in this [`IdDag`],
    /// i.e. everything excluding the VIRTUAL group.
    fn all(&self) -> Result<IdSet> {
        // Intentionally skip the VIRTUAL group.
        self.all_ids_in_groups(&[Group::MASTER, Group::NON_MASTER])
    }

    /// Return a [`IdSet`] that covers all ids stored in the master group.
    fn master_group(&self) -> Result<IdSet> {
        self.all_ids_in_groups(&[Group::MASTER])
    }

    /// Calculate all ancestors reachable from any id from the given set.
    ///
    /// ```plain,ignore
    /// union(ancestors(i) for i in set)
    /// ```
    fn ancestors(&self, set: IdSet) -> Result<IdSet> {
        debug!(target: "dag::algo::ancestors", "ancestors({:?})", &set);
        ancestors_until(self, set, None)
    }

    /// Like `ancestors` but follows only the first parents.
    fn first_ancestors(&self, set: IdSet) -> Result<IdSet> {
        fn trace(msg: &dyn Fn() -> String) {
            trace!(target: "dag::algo::first_ancestors", "{}", msg());
        }
        debug!(target: "dag::algo::first_ancestors", "first_ancestors({:?})", &set);
        let mut result = IdSet::empty();
        let mut to_visit: BinaryHeap<_> = set.iter_desc().collect();
        // Lookup flat segments to figure out the first ancestors.
        while let Some(id) = to_visit.pop() {
            if result.contains(id) {
                // If `id` is in `result`, then `ancestors(id)` are all in `result`.
                continue;
            }
            trace(&|| format!(" visit {id:?}"));
            let flat_seg = self.find_flat_segment_including_id(id)?;
            if let Some(ref seg) = flat_seg {
                let span = seg.span()?;
                result.push_span((span.low..=id).into());
                trace(&|| format!(" push {:?}..={:?}", span.low, id));
                if let Some(&p) = seg.parents()?.first() {
                    to_visit.push(p);
                }
            }
        }
        trace(&|| format!(" result: {result:?}"));
        Ok(result)
    }

    /// Calculate merges within the given set.
    fn merges(&self, set: IdSet) -> Result<IdSet> {
        fn trace(msg: &dyn Fn() -> String) {
            trace!(target: "dag::algo::merges", "{}", msg());
        }
        debug!(target: "dag::algo::merges", "merges({:?})", &set);

        let mut result = IdSet::empty();

        // Check overlapped flat segments. By definition, merges can only be the
        // "low"s of flat segments.

        // Process the given span overlapped with the segment.
        // Return the next "high" id for segment lookup.
        // Return None if there is no segment to check for the given span.
        let mut process_seg = |span: &IdSpan, seg: Segment| -> Result<Option<Id>> {
            trace(&|| format!(" process {span:?} seg {seg:?}"));
            let seg_span = seg.span()?;
            let low = seg_span.low;
            if low < span.low {
                return Ok(None);
            }
            if seg.parent_count()? >= 2 {
                // span.low <= low <= high <= span.high
                debug_assert!(set.contains(low));
                trace(&|| format!(" push merge {low:?}"));
                result.push_span(low.into());
            }
            if seg_span.low > Id(0) {
                Ok(Some(seg_span.low - 1))
            } else {
                Ok(None)
            }
        };

        for span in set.as_spans() {
            // Cannot use iter_segments_descending, since it skips overlapping
            // segments (seg.high > span.high and seg.low > span.low). Use
            // find_flat_segment_including_id to find the first overlapping
            // segment, then use iter_segments_descending to handle a large
            // span (ex. all()) efficiently.
            let high = match self.find_flat_segment_including_id(span.high)? {
                None => continue,
                Some(seg) => match process_seg(span, seg)? {
                    None => continue,
                    Some(id) => id,
                },
            };
            'iter_seg: for seg in self.iter_segments_descending(high, 0)? {
                let seg = seg?;
                match process_seg(span, seg)? {
                    None => break 'iter_seg,
                    Some(_) => {}
                }
            }
        }

        trace(&|| format!(" result: {result:?}"));

        Ok(result)
    }

    /// Calculate parents of the given set.
    ///
    /// Note: [`IdSet`] does not preserve order. Use [`IdDag::parent_ids`] if
    /// order is needed.
    fn parents(&self, set: IdSet) -> Result<IdSet> {
        fn trace(msg: &dyn Fn() -> String) {
            trace!(target: "dag::algo::parents", "{}", msg());
        }
        debug!(target: "dag::algo::parents", "parents({:?})", &set);

        // Internal parents are produced in descending order. External parents
        // can interleave with later spans, so merge them once at the end.
        // Updating the result with them for each segment would take time
        // quadratic in the number of spans.
        let mut parents = IdSet::empty();
        let mut external_parents = Vec::new();
        let mut cached_flat_segment: Option<(Segment, IdSpan)> = None;
        let max_level = self.max_level()?;

        for span in set.iter_span_desc() {
            // Walk the span from its top, a segment at a time.
            let mut head = span.high;
            'span: loop {
                trace(&|| format!("check head {head:?}"));
                if cached_flat_segment
                    .as_ref()
                    .is_none_or(|(_, cached_span)| !cached_span.contains(head))
                {
                    // For high-level segments. If the span covers the entire segment, then
                    // the parents is (the segment - its head + its parents).
                    for level in (1..=max_level).rev() {
                        if let Some(seg) = self.find_segment_by_head_and_level(head, level)? {
                            let seg_span = seg.span()?;
                            if seg_span.low >= span.low {
                                if seg_span.low < head {
                                    parents.push_span(IdSpan::new(seg_span.low, head - 1));
                                }
                                external_parents.extend(seg.parents()?);
                                trace(&|| format!(" push lv{level} {seg_span:?}"));
                                if seg_span.low == span.low {
                                    break 'span;
                                }
                                head = seg_span.low - 1;
                                continue 'span;
                            }
                        }
                    }

                    // A flat segment contains information to calculate
                    // parents(subset of the segment).
                    let seg = match self.find_flat_segment_including_id(head)? {
                        Some(seg) => seg,
                        None => return head.not_found(),
                    };
                    let seg_span = seg.span()?;
                    cached_flat_segment = Some((seg, seg_span));
                }
                // High-level segments end at flat segment heads. Since head
                // decreases, a cache hit is inside the previous flat segment
                // and cannot be the head of a high-level segment.
                let Some((seg, seg_span)) = cached_flat_segment.as_ref() else {
                    return bug("missing flat segment in parents");
                };
                let seg_low = seg_span.low;
                let low = seg_low.max(span.low);
                // Within the segment, parent(i) is (i - 1), except for its low.
                if low == seg_low {
                    if low < head {
                        parents.push_span(IdSpan::new(low, head - 1));
                    }
                    external_parents.extend(seg.parents()?);
                } else {
                    parents.push_span(IdSpan::new(low - 1, head - 1));
                }
                trace(&|| format!(" push lv0 {low:?}..={head:?}"));
                if low == span.low {
                    break;
                }
                head = low - 1;
            }
        }

        let result = if external_parents.is_empty() {
            parents
        } else {
            parents.union(&IdSet::from_spans(external_parents))
        };
        trace(&|| format!(" result: {result:?}"));

        Ok(result)
    }

    /// Get parents of a single `id`. Preserve the order.
    fn parent_ids(&self, id: Id) -> Result<Vec<Id>> {
        let seg = match self.find_flat_segment_including_id(id)? {
            Some(seg) => seg,
            None => return id.not_found(),
        };
        let span = seg.span()?;
        if id == span.low {
            Ok(seg.parents()?)
        } else {
            Ok(vec![id - 1])
        }
    }

    /// Calculate the n-th first ancestor. If `n` is 0, return `id` unchanged.
    /// If `n` is 1, return the first parent of `id`.
    fn first_ancestor_nth(&self, id: Id, n: u64) -> Result<Id> {
        match self.try_first_ancestor_nth(id, n)? {
            None => Err(Programming(format!(
                "{id}~{n} cannot be resolved - no parents"
            ))),
            Some(id) => Ok(id),
        }
    }

    /// Calculate the n-th first ancestor. If `n` is 0, return `id` unchanged.
    /// If `n` is 1, return the first parent of `id`.
    /// If `n` is too large, exceeding the distance between the root and `id`,
    /// return `None`.
    fn try_first_ancestor_nth(&self, mut id: Id, mut n: u64) -> Result<Option<Id>> {
        // PERF: this can have fast paths from high-level segments if high-level
        // segments have extra information.
        while n > 0 {
            let seg = self
                .find_flat_segment_including_id(id)?
                .ok_or_else(|| id.not_found_error())?;
            // segment: low ... id ... high
            //          \________/
            //            delta
            let low = seg.span()?.low;
            let delta = id.0 - low.0;
            let step = delta.min(n);
            id = id - step;
            n -= step;
            if n > 0 {
                // Follow the first parent.
                id = match seg.parents()?.first() {
                    None => return Ok(None),
                    Some(&id) => id,
                };
                n -= 1;
            }
        }
        Ok(Some(id))
    }

    /// Convert an `id` to `x~n` form with the given constraint.
    ///
    /// Return `None` if the conversion can not be done with the constraints.
    fn to_first_ancestor_nth(
        &self,
        id: Id,
        constraint: FirstAncestorConstraint,
    ) -> Result<Option<(Id, u64)>> {
        match constraint {
            FirstAncestorConstraint::None => Ok(Some((id, 0))),
            FirstAncestorConstraint::KnownUniversally { heads } => {
                self.to_first_ancestor_nth_known_universally(id, heads)
            }
        }
    }

    /// See `FirstAncestorConstraint::KnownUniversally`.
    ///
    /// Try to represent `id` as `x~n` (revset notation) form, where `x` must
    /// be in `heads + parents(merge() & ancestors(heads))`.
    ///
    /// Return `None` if `id` is not part of `ancestors(heads)`.
    fn to_first_ancestor_nth_known_universally(
        &self,
        id: Id,
        heads: IdSet,
    ) -> Result<Option<(Id, u64)>> {
        // Do not track errors for the first time. This has lower overhead.
        match self.to_first_ancestor_nth_known_universally_with_errors(id, &heads, None) {
            Ok(v) => Ok(v),
            Err(_) => {
                // Pay additional overhead to get error message.
                self.to_first_ancestor_nth_known_universally_with_errors(
                    id,
                    &heads,
                    Some(Vec::new()),
                )
            }
        }
    }

    /// Internal implementation of `to_first_ancestor_nth_known_universally`.
    ///
    /// If `details` is not `None`, it is used to store extra error messages
    /// with some extra overhead.
    fn to_first_ancestor_nth_known_universally_with_errors(
        &self,
        mut id: Id,
        heads: &IdSet,
        mut details: Option<Vec<String>>,
    ) -> Result<Option<(Id, u64)>> {
        // To figure `x~n` we look at the flat segment containing `id`, and check:
        // - If the flat segment overlaps with heads, then just use the overlapped id as `x`.
        // - Try to find parents of merges within the flat segment (the merges belong to other
        //   "child segment"s). If the merge is an ancestor of `heads`, then the parent of the
        //   merge can be used as `x`, if `n` is not 0.
        // - Otherwise, try to follow connected flat segment (with single parent), convert the
        //   `x~n` question to a `y~(n+m)` question, then start over.
        let mut trace = |msg: &dyn Fn() -> String| {
            if let Some(details) = details.as_mut() {
                details.push(msg().trim().to_string());
            }
            trace!(target: "dag::algo::toxn", "{}", msg());
        };

        let ancestors = self.ancestors(heads.clone())?;
        if !ancestors.contains(id) {
            return Ok(None);
        }

        let mut n = 0;
        debug!(target: "dag::algo::toxn", "resolving {:?}", id);
        let result = 'outer: loop {
            let seg = self
                .find_flat_segment_including_id(id)?
                .ok_or_else(|| id.not_found_error())?;
            let span = seg.span()?;
            let head = span.high;
            trace(&|| format!(" in seg {seg:?}"));
            // Can we use an `id` from `heads` as `x`?
            let intersected = heads.intersection(&(id..=head).into());
            if !intersected.is_empty() {
                let head = intersected.min().unwrap();
                n += head.0 - id.0;
                trace(&|| format!("  contains head ({head:?})"));
                break 'outer (head, n);
            }
            // Does this segment contain any `x` that is an ancestor of a merge,
            // that can be used as `x`?
            //
            // Note the `x` does not have to be the head of `seg`. For example,
            //
            //      1--2--3--4 (seg, span: 1..=4)
            //          \
            //           5--6  (child_seg, span: 5..=6, parents: [2])
            //
            // During `to_first_ancestor_nth(2, [6])`, `1` needs to be translated
            // to `6~3`, not `4~3`. Needs to lookup parents in the range `1..=4`.
            let mut next_id_n = None;
            let parent_span = span.low.max(id)..=span.high;
            for entry in self.iter_flat_segments_with_parent_span(parent_span.into())? {
                let (parent_id, child_seg) = entry?;
                trace(&|| format!("  {parent_id:?} has child seg ({child_seg:?})"));
                let child_low = child_seg.low()?;
                if !ancestors.contains(child_low) {
                    // `child_low` is outside `ancestors(heads)`, cannot use it
                    // or its parents as references.
                    trace(&|| "   child seg out of range".to_string());
                    continue;
                }

                if child_seg.parent_count()? > 1 {
                    // `child_low` is a merge included in `ancestors`, and
                    // `parent_id` is a parent of the merge. Therefore
                    // `parent_id` might be used as `x`.
                    let next_n = n + parent_id.0 - id.0;
                    if next_n > 0 {
                        break 'outer (parent_id, next_n);
                    }

                    // Fragmented linear segments. Convert id~n to next_id~next_n.
                    let child_parents = child_seg.parents()?;
                    match child_parents.first() {
                        None => {
                            return bug(format!(
                                "segment {child_seg:?} should have parent {parent_id:?}"
                            ));
                        }
                        Some(p) => {
                            if &parent_id != p {
                                // Not the first parent. Do not follow it.
                                trace(&|| {
                                    format!(
                                        "   child seg cannot be followed ({parent_id:?} is not p1)"
                                    )
                                });
                                continue;
                            }
                        }
                    }
                }

                debug_assert!(ancestors.contains(child_low));
                let next_id = child_low;
                let next_n = n + 1 + parent_id.0 - id.0;
                trace(&|| format!("  follow {next_id:?}~{next_n}"));
                next_id_n = Some((next_id, next_n));
                break;
            }
            match next_id_n {
                // This should not happen if indexes and segments are legit.
                None => {
                    let mut message = format!(
                        concat!(
                            "cannot convert {} to x~n form (x must be in ",
                            "`H + parents(ancestors(H) & merge())` where H = {:?})",
                        ),
                        id, &heads,
                    );
                    if let Some(details) = details {
                        if !details.is_empty() {
                            message += &format!(" (trace: {})", details.join(", "));
                        }
                    }
                    return Err(Programming(message));
                }
                Some((next_id, next_n)) => {
                    id = next_id;
                    n = next_n;
                }
            }
        };
        trace!(target: "dag::algo::toxn", " found: {:?}", &result);
        Ok(Some(result))
    }

    /// Calculate heads of the given set.
    fn heads(&self, set: IdSet) -> Result<IdSet> {
        Ok(set.difference(&self.parents(set.clone())?))
    }

    /// Calculate children for a single `Id`.
    fn children_id(&self, id: Id) -> Result<IdSet> {
        let mut result = BTreeSet::new();
        fn trace(msg: &dyn Fn() -> String) {
            trace!(target: "dag::algo::children_id", "{}", msg());
        }
        debug!(target: "dag::algo::children_id", "children_id({:?})", id);
        for seg in self.iter_flat_segments_with_parent(id)? {
            let seg = seg?;
            let child_id = seg.low()?;
            trace(&|| format!(" push {child_id:?} via parent index"));
            result.insert(child_id);
        }
        if let Some(seg) = self.find_flat_segment_including_id(id)? {
            let span = seg.span()?;
            if span.high != id {
                let child_id = id + 1;
                trace(&|| format!(" push {child_id:?} via flat segment definition"));
                result.insert(child_id);
            }
        }
        let result = IdSet::from_sorted_spans(result.into_iter().rev());
        trace(&|| format!(" result: {result:?}"));
        Ok(result)
    }

    /// Calculate children of the given set.
    fn children(&self, set: IdSet) -> Result<IdSet> {
        if set.count() < 5 {
            let result = set
                .iter_desc()
                .try_fold(IdSet::empty(), |acc, id| -> Result<IdSet> {
                    Ok(acc.union(&self.children_id(id)?))
                })?;
            #[cfg(test)]
            {
                let result_set = self.children_set(set)?;
                assert_eq!(result.as_spans(), result_set.as_spans());
            }
            Ok(result)
        } else {
            self.children_set(set)
        }
    }

    fn children_set(&self, set: IdSet) -> Result<IdSet> {
        fn trace(msg: &dyn Fn() -> String) {
            trace!(target: "dag::algo::children", "{}", msg());
        }
        debug!(target: "dag::algo::children", "children({:?})", &set);

        // The algorithm works as follows:
        // - Iterate through level N segments [1].
        // - Considering a level N segment S:
        //   Could we take the entire S?
        //     - If `set` covers `S - S.head + S.parents`, then yes, take S
        //       and continue with the next level N segment.
        //   Could we ignore the entire S and check the next level N segment?
        //     - If (S + S.parents) do not overlap with `set`, then yes, skip.
        //   No fast paths. Is S a flat segment?
        //     - No:  Iterate through level N-1 segments covered by S,
        //            recursively (goto [1]).
        //     - Yes: Figure out children in the flat segment.
        //            Push them to the result.

        struct Context<'a, Store: ?Sized> {
            this: &'a Store,
            set: IdSet,
            // Lower bound based on the input set.
            result_lower_bound: Id,
            result: IdSet,
        }

        fn visit_segments<S: IdDagStore + ?Sized>(
            ctx: &mut Context<S>,
            mut range: IdSpan,
            level: Level,
        ) -> Result<()> {
            trace(&|| format!("visit range {range:?} lv{level}"));
            let mut visited = false;
            for seg in ctx.this.iter_segments_descending(range.high, level)? {
                let seg = seg?;
                let span = seg.span()?;
                visited = true;
                trace(&|| format!(" seg {seg:?}"));
                // `range` is all valid. If a high-level segment misses it, try
                // a lower level one.
                if span.high < range.high {
                    let low_id = (span.high + 1).max(range.low);
                    if low_id > range.high {
                        return Ok(());
                    }
                    let missing_range = IdSpan::from(low_id..=range.high);
                    if level > 0 {
                        trace(&|| {
                            format!("  visit missing range at lower level: {missing_range:?}")
                        });
                        visit_segments(ctx, missing_range, level - 1)?;
                    } else {
                        return bug(format!(
                            "flat segments should have covered: {missing_range:?} returned by all() (range: {range:?})",
                        ));
                    }
                }

                // Update range.high for the next iteration.
                range.high = span.low.max(Id(1)) - 1;

                // Stop iteration?
                if span.high < range.low || span.high < ctx.result_lower_bound {
                    break;
                }

                let parents = seg.parents()?;

                // Count of parents overlapping with `set`.
                let overlapped_parents = parents.iter().filter(|p| ctx.set.contains(**p)).count();

                // Remove the `high`. This segment cannot calculate
                // `children(high)`. If `high` matches a `parent` of
                // another segment, that segment will handle it.
                // This is related to the flat segment children path
                // below.
                let intersection = ctx
                    .set
                    .intersection(&span.into())
                    .difference(&span.high.into());

                if !seg.has_root()? {
                    // A segment must have at least one parent to be rootless.
                    debug_assert!(!parents.is_empty());
                    // Fast path: Take the entire segment directly.
                    if overlapped_parents == parents.len()
                        && intersection.count() + 1 == span.count()
                    {
                        trace(&|| format!(" push lv{level} {span:?} (rootless fast path)"));
                        ctx.result.push_span(span);
                        continue;
                    }
                }

                if !intersection.is_empty() {
                    if level > 0 {
                        visit_segments(ctx, span, level - 1)?;
                        continue;
                    } else {
                        // Flat segment children path.
                        // children(x..=y) = (x+1)..=(y+1) if x..=(y+1) is in a flat segment.
                        let seg_children = IdSet::from_spans(
                            intersection
                                .as_spans()
                                .iter()
                                .map(|s| s.low + 1..=s.high + 1),
                        );
                        trace(&|| format!(" push {seg_children:?} (flat segment range)"));
                        ctx.result.push_set(&seg_children);
                    }
                }

                if overlapped_parents > 0 {
                    if level > 0 {
                        visit_segments(ctx, span, level - 1)?;
                    } else {
                        // child(any parent) = lowest id in this flat segment.
                        trace(&|| {
                            format!(" push {:?} (overlapped parents of flat segment)", span.low)
                        });
                        ctx.result.push_span(span.low.into());
                    }
                }
            }
            // The high level segment misses the range. Try a lower level.
            if !visited {
                if level == 0 {
                    return bug(format!(
                        "flat segments should have covered: {range:?} returned by all()",
                    ));
                }
                visit_segments(ctx, range, level - 1)?;
            }
            Ok(())
        }

        let result_lower_bound = set.min().unwrap_or(Id::MAX);
        let mut ctx = Context {
            this: self,
            set,
            result_lower_bound,
            result: IdSet::empty(),
        };

        let max_level = self.max_level()?;
        for span in self.all_with_virtual()?.as_spans() {
            visit_segments(&mut ctx, *span, max_level)?;
        }

        trace(&|| format!(" result: {:?}", ctx.result));

        Ok(ctx.result)
    }

    /// Calculate roots of the given set.
    fn roots(&self, set: IdSet) -> Result<IdSet> {
        Ok(set.difference(&self.children(set.clone())?))
    }

    /// Calculate one "greatest common ancestor" of the given set.
    ///
    /// If there are no common ancestors, return None.
    /// If there are multiple greatest common ancestors, pick one arbitrarily.
    /// Use `gca_all` to get all of them.
    fn gca_one(&self, set: IdSet) -> Result<Option<Id>> {
        // The set is sorted in DESC order. Therefore its first item can be used as the result.
        Ok(self.common_ancestors(set)?.max())
    }

    /// Calculate all "greatest common ancestor"s of the given set.
    /// `gca_one` is faster if an arbitrary answer is ok.
    fn gca_all(&self, set: IdSet) -> Result<IdSet> {
        self.heads_ancestors(self.common_ancestors(set)?)
    }

    /// Calculate all common ancestors of the given set.
    ///
    /// ```plain,ignore
    /// intersect(ancestors(i) for i in set)
    /// ```
    fn common_ancestors(&self, set: IdSet) -> Result<IdSet> {
        let result = match set.count() {
            0 => set,
            1 => self.ancestors(set)?,
            2 => {
                // Fast path that does not calculate "heads".
                let mut iter = set.iter_desc();
                let a = iter.next().unwrap();
                let b = iter.next().unwrap();
                self.ancestors(a.into())?
                    .intersection(&self.ancestors(b.into())?)
            }
            _ => {
                // Try to reduce the size of `set`.
                // `common_ancestors(X)` = `common_ancestors(roots(X))`.
                let set = self.roots(set)?;
                set.iter_desc()
                    .try_fold(IdSet::full(), |set, id| -> Result<IdSet> {
                        Ok(set.intersection(&self.ancestors(id.into())?))
                    })?
            }
        };
        Ok(result)
    }

    /// Test if `ancestor_id` is an ancestor of `descendant_id`.
    fn is_ancestor(&self, ancestor_id: Id, descendant_id: Id) -> Result<bool> {
        // An ancestor has a smaller id than its descendants.
        if ancestor_id > descendant_id {
            return Ok(false);
        }
        let set = ancestors_until(self, descendant_id.into(), Some(ancestor_id))?;
        Ok(set.contains(ancestor_id))
    }

    /// Calculate "heads" of the ancestors of the given [`IdSet`]. That is,
    /// Find Y, which is the smallest subset of set X, where `ancestors(Y)` is
    /// `ancestors(X)`.
    ///
    /// This is faster than calculating `heads(ancestors(set))`.
    ///
    /// This is different from `heads`. In case set contains X and Y, and Y is
    /// an ancestor of X, but not the immediate ancestor, `heads` will include
    /// Y while this function won't.
    fn heads_ancestors(&self, set: IdSet) -> Result<IdSet> {
        let mut remaining = set;
        let mut result = IdSet::empty();
        while let Some(id) = remaining.max() {
            result.push_span((id..=id).into());
            // Remove ancestors reachable from that head.
            remaining = remaining.difference(&self.ancestors(id.into())?);
        }
        Ok(result)
    }

    /// Calculate the "dag range" - ids reachable from both sides.
    ///
    /// ```plain,ignore
    /// intersect(ancestors(heads), descendants(roots))
    /// ```
    ///
    /// This is O(flat segments), or O(merges).
    fn range(&self, roots: IdSet, mut heads: IdSet) -> Result<IdSet> {
        if roots.is_empty() {
            return Ok(IdSet::empty());
        }
        if heads.is_empty() {
            return Ok(IdSet::empty());
        }

        fn trace(msg: &dyn Fn() -> String) {
            trace!(target: "dag::algo::range", "{}", msg());
        }
        debug!(target: "dag::algo::range", "range({:?}, {:?})", &roots, &heads);

        // Remove uninteresting heads. Make `ancestors(heads)` a bit easier.
        let min_root_id = roots.min().unwrap();
        let min_head_id = heads.min().unwrap();
        if min_head_id < min_root_id {
            let span = min_root_id..=Id::MAX;
            heads = heads.intersection(&span.into());
            trace(&|| format!(" removed unreachable heads: {heads:?}"));
        }

        let ancestors_of_heads = self.ancestors(heads)?;
        let result = self.descendants_intersection(&roots, &ancestors_of_heads)?;

        #[cfg(test)]
        {
            let intersection = ancestors_of_heads.intersection(&result);
            assert_eq!(result.as_spans(), intersection.as_spans());
        }

        trace(&|| format!(" result: {result:?}"));
        Ok(result)
    }

    /// Calculate the descendants of the given set.
    ///
    /// Logically equivalent to `range(set, all())`.
    ///
    /// This is O(flat segments), or O(merges).
    fn descendants(&self, set: IdSet) -> Result<IdSet> {
        debug!(target: "dag::algo::descendants", "descendants({:?})", &set);
        let roots = set;
        let all = self.all_with_virtual()?;
        let result = self.descendants_intersection(&roots, &all)?;
        trace!(target: "dag::algo::descendants", " result: {:?}", &result);
        Ok(result)
    }

    /// Calculate (descendants(roots) & ancestors).
    ///
    /// This is O(flat segments), or O(merges).
    ///
    /// `ancestors(ancestors)` must be equal to `ancestors`.
    fn descendants_intersection(&self, roots: &IdSet, ancestors: &IdSet) -> Result<IdSet> {
        fn trace(msg: &dyn Fn() -> String) {
            trace!(target: "dag::algo::descendants_intersection", "{}", msg());
        }

        debug_assert_eq!(
            ancestors.count(),
            self.ancestors(ancestors.clone())?.count()
        );

        // Filter out roots that are not reachable from `ancestors`.
        let roots = ancestors.intersection(roots);
        let min_root = match roots.min() {
            Some(id) => id,
            None => return Ok(IdSet::empty()),
        };
        let max_root = roots.max().unwrap();

        // `result` could be initially `roots`. However that breaks ordering
        // (cannot use `result.push_span_asc` below).
        let mut result = IdSet::empty();

        // For the master group, use linear scan for flat segments. This is
        // usually more efficient, because the master group usually only has 1
        // head, and most segments will be included.
        let master_max_id = ancestors
            .max()
            .unwrap_or(Id::MIN)
            .min(Group::MASTER.max_id());
        for seg in self.iter_segments_ascending(min_root, 0)? {
            let seg = seg?;
            let span = seg.span()?;
            if span.low > master_max_id {
                break;
            }
            trace(&|| format!(" visit {seg:?}"));
            let parents = seg.parents()?;
            let low = if !parents.is_empty()
                && parents
                    .iter()
                    .any(|&p| result.contains(p) || roots.contains(p))
            {
                // Include `span.low` if any (span.low's) parent is in the result set.
                span.low
            } else {
                // No parents in `result` set.
                match result
                    .intersection_span_min(span)
                    .or_else(|| roots.intersection(&span.into()).min())
                {
                    // `span` intersect partially with `roots | result`.
                    Some(id) => id,
                    None => continue,
                }
            };
            if low > master_max_id {
                break;
            }
            let result_span = IdSpan::from(low..=span.high);
            trace(&|| format!("  push {result_span:?}"));
            result.push_span_asc(result_span);
        }

        // For the non-master group, only check flat segments covered by
        // `ancestors`.
        //
        // This is usually more efficient, because the non-master group can
        // have lots of heads (created in the past) that are no longer visible
        // or interesting. For a typical query like `x::y`, it might just select
        // a few heads in the non-master group. It's a waste of time to iterate
        // through lots of invisible segments.
        let non_master_spans = ancestors
            .intersection(&IdSpan::from(Group::NON_MASTER.min_id()..=Group::MAX.max_id()).into());
        // Visit in ascending order.
        let mut span_iter = non_master_spans.as_spans().iter().rev().cloned();
        let mut next_optional_span = span_iter.next();
        while let Some(next_span) = next_optional_span {
            // The "next_span" could be larger than a flat segment.
            let seg = match self.find_flat_segment_including_id(next_span.low)? {
                Some(seg) => seg,
                None => break,
            };
            let seg_span = seg.span()?;
            trace(&|| format!(" visit {next_span:?} => {seg:?}"));
            // The overlap part of the flat segment and the span from 'ancestors'.
            let mut overlap_span =
                IdSpan::from(seg_span.low.max(next_span.low)..=seg_span.high.min(next_span.high));
            if roots.contains(overlap_span.low) {
                // Descendants includes 'overlap_span' since 'low' is in 'roots'.
                // (no need to check 'result' - it does not include anything in 'overlap')
                trace(&|| format!("  push {overlap_span:?} (root contains low)"));
                result.push_span_asc(overlap_span);
            } else if next_span.low == seg_span.low {
                let parents = seg.parents()?;
                if !parents.is_empty()
                    && parents
                        .into_iter()
                        .any(|p| result.contains(p) || roots.contains(p))
                {
                    // Descendants includes 'overlap_span' since parents are in roots or result.
                    trace(&|| format!("  push {overlap_span:?} (root contains parent)"));
                    result.push_span_asc(overlap_span);
                } else if overlap_span.low <= max_root && overlap_span.high >= min_root {
                    // If 'overlap_span' overlaps with roots, part of it should be in
                    // 'Descendants' result:
                    //
                    //            root1  root2
                    //               v    v
                    //    (low) |-- overlap-span --| (high)
                    //               |-------------|
                    //               push this part to result
                    let roots_intersection = roots.intersection(&overlap_span.into());
                    if let Some(id) = roots_intersection.min() {
                        overlap_span.low = id;
                        trace(&|| format!("  push {overlap_span:?} (root in span)"));
                        result.push_span_asc(overlap_span);
                    }
                }
            } else {
                // This block practically does not happen if `ancestors` is
                // really "ancestors" (aka. `ancestors(ancestors)` is
                // `ancestors`), because `ancestors` will not include
                // a flat segment without including the segment's low id.
                //
                // But, in case it happens (because `ancestors` is weird),
                // do something sensible.

                // `next_span.low - 1` is the parent of `next_span.low`,
                debug_assert!(
                    false,
                    "ancestors in descendants_intersection is
                              not real ancestors"
                );
                let p = next_span.low - 1;
                if result.contains(p) || roots.contains(p) {
                    trace(&|| format!("  push {overlap_span:?} ({p:?} was included)"));
                    result.push_span_asc(overlap_span);
                }
            }
            // Update next_optional_span.
            next_optional_span = IdSpan::try_from_bounds(overlap_span.high + 1..=next_span.high)
                .or_else(|| span_iter.next());
        }

        trace(&|| format!(" intersect with {ancestors:?}"));
        result = result.intersection(ancestors);

        Ok(result)
    }

    /// Find segments that cover the given `id_set` exactly.
    /// Returned segments are in DESC order.
    fn id_set_to_id_segments(&self, id_set: &IdSet) -> Result<VecDeque<IdSegment>> {
        let max_level = self.max_level()?;
        self.id_set_to_id_segments_with_max_level(id_set, max_level)
    }

    /// Find lower level segments that cover the given `id_segment` exactly.
    /// Returned segments are in DESC order.
    fn id_segment_to_lower_level_id_segments(
        &self,
        id_segment: &IdSegment,
    ) -> Result<VecDeque<IdSegment>> {
        let max_level = match id_segment.level {
            0 => return Err(Programming(
                "id_segment_to_lower_level_id_segments() requires non-flat (level > 0) segments"
                    .to_string(),
            )),
            l => l - 1,
        };
        let span = IdSpan::new(id_segment.low, id_segment.high);
        let id_set = IdSet::from(span);
        self.id_set_to_id_segments_with_max_level(&id_set, max_level)
    }

    /// Find segments with max level limitation that cover the given `id_set` exactly.
    /// Returned segments are in DESC order.
    fn id_set_to_id_segments_with_max_level(
        &self,
        id_set: &IdSet,
        max_level: Level,
    ) -> Result<VecDeque<IdSegment>> {
        fn trace(msg: &dyn Fn() -> String) {
            trace!(target: "dag::algo::tosegments", "{}", msg());
        }
        debug!(target: "dag::algo::tosegments", "id_set_to_id_segments({:?}, level={})", &id_set, max_level);

        let max_level = max_level.min(self.max_level()?);
        let mut result = VecDeque::new();
        'next_span: for span in id_set.iter_span_desc() {
            trace(&|| format!(" visiting span {span:?}"));
            let mut span: IdSpan = *span;

            'current_span: loop {
                // Try high level segments.
                for level in (1..=max_level).rev() {
                    let seg = match self.find_segment_by_head_and_level(span.high, level)? {
                        None => continue,
                        Some(seg) => seg,
                    };

                    let seg_span = seg.span()?;
                    debug_assert_eq!(seg_span.high, span.high);

                    if seg_span.low < span.low {
                        trace(&|| format!("  skip  lv {level} seg {seg:?}"));
                        continue;
                    } else {
                        trace(&|| format!("  found lv {level} seg {seg:?}"));
                    }

                    let id_seg = IdSegment {
                        high: seg_span.high,
                        low: seg_span.low,
                        parents: seg.parents()?,
                        level,
                        has_root: seg.has_root()?,
                    };
                    trace(&|| format!("  push {}..={}", id_seg.low, id_seg.high));
                    result.push_back(id_seg);

                    if seg_span.low == span.low {
                        continue 'next_span;
                    } else {
                        span.high = seg_span.low - 1;
                        debug_assert!(span.high >= span.low);
                        continue 'current_span;
                    }
                }

                // Query flat segments.
                let seg = match self.find_flat_segment_including_id(span.high)? {
                    None => return bug(format!("flat segments does not cover {span:?}")),
                    Some(seg) => seg,
                };
                trace(&|| format!("  found flat seg {seg:?}"));

                let seg_span = seg.span()?;
                debug_assert!(seg_span.high >= span.high);

                let (low, parents) = if seg_span.low < span.low {
                    (span.low, vec![span.low - 1])
                } else {
                    (seg_span.low, seg.parents()?)
                };

                let has_root = parents.is_empty();
                let id_seg = IdSegment {
                    high: span.high,
                    low,
                    parents,
                    level: 0,
                    has_root,
                };
                trace(&|| format!("  push {}..={}", id_seg.low, id_seg.high));
                result.push_back(id_seg);

                if low == span.low {
                    continue 'next_span;
                } else {
                    span.high = low - 1;
                    debug_assert!(span.high >= span.low);
                }
            }
        }
        Ok(result)
    }

    /// Suggest the next `Id` to test, during a bisect.
    ///
    /// - `(low, high)` are explicitly marked ends, either `(good, bad)` or `(bad, good)`.
    /// - `skip` is an explicitly skipped set.
    ///
    /// Return `(id_to_bisect_next, untested_set, roots(high::))`.
    ///
    /// If `id_to_bisect_next` is `None`, the bisect is completed. At this time,
    /// `roots(high::)` is the "first good/bad" set. `untested_set` is usually
    /// empty, or a subset of `skip`.
    fn suggest_bisect(
        &self,
        low: &IdSet,
        high: &IdSet,
        skip: &IdSet,
    ) -> Result<(Option<Id>, IdSet, IdSet)> {
        debug!(target: "dag::algo::suggest_bisect", "suggest_bisect({:?}, {:?}, {:?})", low, high, skip);
        // Handling the two ends is NOT symmetric. For example, assuming high = bad:
        //
        //     Y  high (bad)
        //    / \
        //  A10  B10
        //   :    :
        //  A01  B01
        //    \ /
        //     X  low (good)
        //
        // - If A05 is "good", then A01::A05 is implicit "good", B01::B10 remains unknown.
        // - If A05 is "bad", then A05::Y is implicit "bad", B01::B10 can be skipped.
        //
        // The final bisect output is a single commit ("first bad"). The contract does not require us
        // to figure out all "first bad"s in all branches. If X is good, A05 is bad, then we know one
        // of the "first bad"s must exist in X::A05 and we can ignore other parts like B01::B10.

        // low::low = implicit good.
        let connected_low = self.range(low.clone(), low.clone())?;

        // Ignore uninteresting parts - see above for why this is only for "high".
        let high = self.roots(self.range(high.clone(), high.clone())?)?;

        // The bisect range.
        let untested = self
            .range(low.clone(), high.clone())?
            .difference(&connected_low)
            .difference(&high);
        trace!(target: "dag::algo::suggest_bisect", " untested = {:?}", untested);

        let total = untested.count();
        let ideal = total.div_ceil(2);

        // Consider each linear ranges (flat segments).
        let flat_segments =
            self.id_set_to_id_segments_with_max_level(&untested.difference(skip), 0)?;
        let mut best_score = 0;
        let mut best_id = None;
        for id_segment in flat_segments.into_iter().rev() {
            // Pick "P", let "len(ancestors(P) & untested)" be "x".
            // - If P is good, bisect total -= x.
            // - If P is bad, bisect total -= (total + 1 - x).
            // We want to make good progress even in the worst case, maximize "min(total + 1 - x, x)".
            // The best happens when "total + 1 - x" is "x", when x = ideal.
            //
            // if good, remove "ancestors(P)" from "untested",
            // if bad, remove "P + (untested - ancestors(P))" from "untested".

            // "ancestors(high)" can be cheaper to calculate than "ancestor(low)", if it follows a
            // high-level segment. Once we get "ancestors(high)", we can calculate the "count" for
            // "ancestors(p for p in low..=high)" without calling (slower) "ancestors(p)", because
            // the segment is "flat" (level 0), it cannot have merges except for "low".
            let ancestors_high = self.ancestors(id_segment.high.into())?;
            let high_count = ancestors_high.intersection(&untested).count();
            let low_count = high_count.saturating_sub(id_segment.high.0 - id_segment.low.0);
            let best_count = ideal.max(low_count).min(high_count);
            let score = best_count.min(total + 1 - best_count);
            if score > best_score {
                let id = id_segment.low + (best_count - low_count);
                trace!(target: "dag::algo::suggest_bisect", "  id={} score={}", id, score);
                best_score = score;
                best_id = Some(id);
                if best_count == ideal {
                    break;
                }
            }
        }

        Ok((best_id, untested, high))
    }
}

impl<S: IdDagStore> IdDagAlgorithm for S {}

/// Calculate the ancestors of `set`, like [`IdDagAlgorithm::ancestors`].
///
/// If `target` is set, the walk stops as soon as it is known whether `target`
/// is an ancestor. The result then contains `target` if and only if it is an
/// ancestor of `set`, but may lack other ancestors.
fn ancestors_until<S: IdDagAlgorithm + ?Sized>(
    dag: &S,
    mut set: IdSet,
    target: Option<Id>,
) -> Result<IdSet> {
    fn trace(msg: &dyn Fn() -> String) {
        trace!(target: "dag::algo::ancestors", "{}", msg());
    }
    if set.count() > 2 {
        // Try to (greatly) reduce the size of the `set` to make calculation cheaper.
        set = dag.heads_ancestors(set)?;
        trace(&|| format!("simplified to {set:?}"));
    }
    let mut result = IdSet::empty();
    let mut to_visit: BinaryHeap<_> = set.iter_desc().collect();
    let max_level = dag.max_level()?;
    'outer: while let Some(id) = to_visit.pop() {
        if let Some(target) = target {
            // Stop once `target` has been found. Ids are visited in
            // descending order, and an ancestor has a smaller id than its
            // descendants, so also stop once `id` is below `target`, since
            // nothing left to visit can reach it.
            if id < target || result.contains(target) {
                break;
            }
        }
        if result.contains(id) {
            // If `id` is in `result`, then `ancestors(id)` are all in `result`.
            continue;
        }
        trace(&|| format!(" lookup {id:?}"));
        let flat_seg = dag.find_flat_segment_including_id(id)?;
        if let Some(ref s) = flat_seg {
            if s.only_head()? {
                // Fast path.
                trace(&|| format!(" push ..={id:?} (only head fast path)"));
                result.push_span((Id::MIN..=id).into());
                break 'outer;
            }
        }
        for level in (1..=max_level).rev() {
            let seg = dag.find_segment_by_head_and_level(id, level)?;
            if let Some(seg) = seg {
                let span = seg.span()?;
                trace(&|| format!(" push lv{level} {span:?}"));
                result.push_span(span);
                let parents = seg.parents()?;
                trace(&|| format!(" follow parents {parents:?}"));
                for parent in parents {
                    to_visit.push(parent);
                }
                continue 'outer;
            }
        }
        if let Some(seg) = flat_seg {
            let span = (seg.span()?.low..=id).into();
            trace(&|| format!(" push lv0 {span:?}"));
            result.push_span(span);
            let parents = seg.parents()?;
            trace(&|| format!(" follow parents {parents:?}"));
            for parent in parents {
                to_visit.push(parent);
            }
        } else {
            // The current design requires flat segments to cover all Ids.
            // Potentially they can be made lazy. But that would be a large change.
            return bug(format!(
                "flat segments should cover all Ids but {id:?} is not covered"
            ));
        }
    }

    trace(&|| format!(" result: {result:?}"));

    Ok(result)
}

impl<Store: IdDagStore> Deref for IdDag<Store> {
    type Target = dyn IdDagAlgorithm;

    fn deref(&self) -> &Self::Target {
        &self.store
    }
}

// Full IdMap -> Sparse IdMap
impl<Store: IdDagStore> IdDag<Store> {
    /// Copy a subset of "Universal" mapping from `full_idmap` to
    /// `sparse_idmap`. See [`IdDag::universal`].
    pub async fn write_sparse_idmap<M: crate::idmap::IdMapWrite>(
        &self,
        full_idmap: &dyn crate::ops::IdConvert,
        sparse_idmap: &mut M,
    ) -> Result<()> {
        for id in self.universal_ids()? {
            let name = full_idmap.vertex_name(id).await?;
            sparse_idmap.insert(id, name.as_ref()).await?
        }
        Ok(())
    }

    /// Return a subset of [`Id`]s that should be "Universal", including:
    ///
    /// - Heads of the master group.
    /// - Parents of merges (a merge is an id with multiple parents)
    ///   in the MASTER group.
    ///
    /// See also [`FirstAncestorConstraint::KnownUniversally`].
    ///
    /// Complexity: `O(flat segments)` for both time and space.
    pub fn universal_ids(&self) -> Result<BTreeSet<Id>> {
        let mut result = BTreeSet::new();
        for seg in self.next_segments(Id::MIN, 0)? {
            let parents = seg.parents()?;
            // Is it a merge?
            if parents.len() >= 2 {
                for id in parents {
                    debug_assert_eq!(id.group(), Group::MASTER);
                    result.insert(id);
                }
            }
        }
        for head in self.heads_ancestors(self.master_group()?)? {
            debug_assert_eq!(head.group(), Group::MASTER);
            result.insert(head);
        }
        Ok(result)
    }
}

/// There are many `x~n`s that all resolves to a single commit.
/// Constraint about `x~n`.
#[derive(Clone)]
pub enum FirstAncestorConstraint {
    /// No constraints.
    None,

    /// `x` and its slice is expected to be known both locally and remotely.
    ///
    /// Practically, this means `x` is either:
    /// - referred explicitly by `heads`.
    /// - a parent of a merge that is an ancestor of `heads`.
    ///   (a merge is a vertex with more than one parents)
    ///   (at clone and pull time, client gets a sparse idmap including them)
    ///
    /// This also enforces `x` to be part of `ancestors(heads)`.
    KnownUniversally { heads: IdSet },
}

impl<Store: IdDagStore> IdDag<Store> {
    /// Remove `set` and their descendants. Return `descendents(set)`.
    ///
    /// The returned `descendants(set)` is usually used to remove
    /// related entries in the `IdMap` to keep the IdMap and IdDag
    /// in sync.
    pub(crate) fn strip(&mut self, set: IdSet) -> Result<IdSet> {
        fn trace(msg: &dyn Fn() -> String) {
            trace!(target: "dag::iddag::remove", "{}", msg());
        }

        let set = self.descendants(set)?;
        trace(&|| format!("descendants(set) = {set:?}"));

        if set.is_empty() {
            return Ok(set);
        }

        // [(segment, new_high)]
        let mut to_resize: Vec<(Segment, Option<Id>)> = Vec::new();

        for span in set.iter_span_desc() {
            trace(&|| format!(" visiting span {span:?}"));

            // span:   (low)    [------------] (high)
            // [seg]:       [-------][---][--]
            // new_seg:     [--]
            for seg in self.iter_segments_descending(span.high, 0)? {
                let seg = seg?;
                let seg_span = seg.span()?;
                debug_assert!(seg_span.high <= span.high); // by iter_segments_descending
                if seg_span.high < span.low {
                    break;
                }

                let new_high = if seg_span.low < span.low {
                    let new_high = span.low - 1;
                    trace(&|| format!("  truncate flat seg {seg:?} at {new_high}"));
                    Some(new_high)
                } else {
                    trace(&|| format!("  remove flat seg {seg:?}"));
                    None
                };
                // It's not possible to truncate a segment twice iterating different `span`s.
                // seg:     [------------]
                // [span]:     [---]  [--] <- not possible
                // This is because "descendants" are selected. If `i` is in the `set`, and `i`
                // is in a flat `seg` then `i..=seg.high` should all be in the `set` to remove.
                debug_assert!(to_resize.iter().all(|(s, _)| s != &seg));
                to_resize.push((seg, new_high));
            }
        }

        // Notes about why we can keep existing SegmentFlags when resizing:
        //
        // There are only 2 flags: HAS_ROOT, and ONLY_HEAD.
        // - HAS_ROOT flag can be preserved. It's based on `parents.is_empty()`
        //   which is not changed by resizing.
        // - ONLY_HEAD flag can be preserved. Suppose the current flat segment
        //   has ONLY_HEAD set (i.e. heads(0:high) = [high]) and is being truncated
        //   from low..=high to low..=mid:
        //
        //          |<-new seg->|<-span (to remove)->|
        //          |<-------seg-------->|
        //          low-------mid-----high
        //
        //   descendants(set) is noted as `X` for removal. Now we prove that all
        //   `X`s must be > mid. Suppose there is an X that is <= mid, X cannot
        //   be in the low--high flat segment otherwise mid will be removed.
        //   X must be an ancestor of "high" by ONLY_HEAD definition (otherwise
        //   there will be more than 1 heads in 0:high). So there must be a path
        //   from X to high but the path does not go through mid. The graph looks
        //   like:
        //
        //          low-------mid--Y--high   (a flat segment)
        //                        /
        //              X--...----           (X:: are to be removed)
        //
        //   The problem is that Y, min(X::high & mid::high), is a merge. It
        //   contradicts flat segment definition that only "low" can be a merge.
        //
        //   Therefore X (to be removed) must be > mid. Nothing in 0:mid is removed.
        //   Because low..=high is a flat segment, removing `(mid+1):high`
        //   is removing a linear sub-graph and cannot increase number of heads.
        //   Therefore the new truncated segment low..=mid can still have the
        //   ONLY_HEAD flag.

        for (seg, new_high) in to_resize {
            self.store.resize_flat_segment(&seg, new_high)?;
        }

        // strip() is not an append-only change. Use an incompatible version.
        // However, if it's just for the VIRTUAL group, then do not bump version.
        let need_version_bump = match (
            set.min().map(|id| id.group()),
            set.max().map(|id| id.group()),
        ) {
            (Some(Group::VIRTUAL), Some(Group::VIRTUAL)) => false,
            _ => true,
        };
        if need_version_bump {
            self.version = VerLink::new();
        }

        Ok(set)
    }
}

impl<Store: Persist + IdDagStore> Persist for IdDag<Store> {
    type Lock = <Store as Persist>::Lock;

    fn lock(&mut self) -> Result<Self::Lock> {
        self.store.lock()
    }

    fn reload(&mut self, lock: &Self::Lock) -> Result<()> {
        self.store.reload(lock)
    }

    fn persist(&mut self, lock: &Self::Lock) -> Result<()> {
        self.store.persist(lock)?;
        self.store.cache_verlink(&self.version);
        Ok(())
    }
}

impl<Store: IdDagStore> Debug for IdDag<Store> {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        let mut first = true;
        for level in 0..=self.max_level().unwrap_or_default() {
            if !first {
                write!(f, "\n")?;
            }
            first = false;
            write!(f, "Lv{level}:")?;

            for group in Group::ALL.iter() {
                let segments = self.next_segments(group.min_id(), level).unwrap();
                if !segments.is_empty() {
                    for segment in segments {
                        write!(f, " {segment:?}")?;
                    }
                }
            }
        }
        Ok(())
    }
}

/// Lazily answer `any(...)`, `all(...)`.
struct LazyPredicate<P> {
    ids: Vec<Id>,
    predicate: P,
    true_count: usize,
    false_count: usize,
}

impl<P: Fn(Id) -> Result<bool>> LazyPredicate<P> {
    pub fn new(ids: Vec<Id>, predicate: P) -> Self {
        Self {
            ids,
            predicate,
            true_count: 0,
            false_count: 0,
        }
    }

    pub fn any(&mut self) -> Result<bool> {
        loop {
            if self.true_count > 0 {
                return Ok(true);
            }
            if self.true_count + self.false_count == self.ids.len() {
                return Ok(false);
            }
            self.test_one()?;
        }
    }

    pub fn all(&mut self) -> Result<bool> {
        loop {
            if self.true_count == self.ids.len() {
                return Ok(true);
            }
            if self.false_count > 0 {
                return Ok(false);
            }
            self.test_one()?;
        }
    }

    fn test_one(&mut self) -> Result<()> {
        let i = self.true_count + self.false_count;
        if (self.predicate)(self.ids[i])? {
            self.true_count += 1;
        } else {
            self.false_count += 1;
        }
        Ok(())
    }
}

impl<S: StorageVersion> StorageVersion for IdDag<S> {
    fn storage_version(&self) -> (u64, u64) {
        self.store.storage_version()
    }
}

fn default_seg_size() -> usize {
    DEFAULT_SEG_SIZE.load(Ordering::Acquire)
}

/// Update global default segment size. The segment size affects high-level
/// segments. A level N segment covers at most "segment size" level N-1
/// segments.
///
/// Setting this to a smaller number like 3 can help demonstrate the concept
/// using smaller graphs. Panic if `new_size` is less than 2.
///
/// Existing IdDags are not affected.
pub fn set_default_seg_size(new_size: usize) {
    DEFAULT_SEG_SIZE.store(new_size, Ordering::Release);
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;
    use crate::iddagstore::tests::dump_store_state;
    use crate::tests::dbg;
    use crate::tests::dbg_iter;
    use crate::tests::nid;
    use crate::tests::vid;

    #[test]
    fn test_segment_basic_lookups() {
        let dir = tempdir().unwrap();
        let mut dag = IdDag::open(dir.path()).unwrap();
        assert_eq!(dag.all().unwrap().count(), 0);

        let flags = SegmentFlags::empty();

        dag.insert(flags, 0, Id::MIN, Id(50), &[]).unwrap();
        assert_eq!(dag.all().unwrap().max(), Some(Id(50)));

        dag.insert(flags, 0, Id(51), Id(100), &[Id(50)]).unwrap();
        assert_eq!(dag.all().unwrap().max(), Some(Id(100)));

        dag.insert(flags, 0, Id(101), Id(150), &[]).unwrap();
        assert_eq!(dag.all().unwrap().max(), Some(Id(150)));

        dag.insert(flags, 1, Id::MIN, Id(150), &[]).unwrap();
        assert_eq!(dag.all().unwrap().max(), Some(Id(150)));

        // Helper functions to make the below lines shorter.
        let low_by_head = |head, level| match dag.find_segment_by_head_and_level(Id(head), level) {
            Ok(Some(seg)) => seg.span().unwrap().low.0 as i64,
            Ok(None) => -1,
            _ => panic!("unexpected error"),
        };

        let low_by_id = |id| match dag.find_flat_segment_including_id(Id(id)) {
            Ok(Some(seg)) => seg.span().unwrap().low.0 as i64,
            Ok(None) => -1,
            _ => panic!("unexpected error"),
        };

        assert_eq!(low_by_head(0, 0), -1);
        assert_eq!(low_by_head(49, 0), -1);
        assert_eq!(low_by_head(50, 0), -1); // 0..=50 is merged into 0..=100.
        assert_eq!(low_by_head(51, 0), -1);
        assert_eq!(low_by_head(150, 0), 101);
        assert_eq!(low_by_head(100, 1), -1);

        assert_eq!(low_by_id(0), 0);
        assert_eq!(low_by_id(30), 0);
        assert_eq!(low_by_id(49), 0);
        assert_eq!(low_by_id(50), 0);
        assert_eq!(low_by_id(51), 0);
        assert_eq!(low_by_id(52), 0);
        assert_eq!(low_by_id(99), 0);
        assert_eq!(low_by_id(100), 0);
        assert_eq!(low_by_id(101), 101);
        assert_eq!(low_by_id(102), 101);
        assert_eq!(low_by_id(149), 101);
        assert_eq!(low_by_id(150), 101);
        assert_eq!(low_by_id(151), -1);
    }

    fn get_parents(id: Id) -> Result<Vec<Id>> {
        match id.0 {
            0..=2 => Ok(Vec::new()),
            _ => Ok(vec![id - 1, Id(id.0 / 2)]),
        }
    }

    #[test]
    fn test_sync_reload() {
        let dir = tempdir().unwrap();
        let mut dag = IdDag::open(dir.path()).unwrap();
        assert_eq!(dag.all().unwrap().max(), None);

        let lock = dag.lock().unwrap();
        dag.reload(&lock).unwrap();
        dag.build_segments(Id(0), &get_parents).unwrap();
        dag.build_segments(Id(1001), &get_parents).unwrap();

        dag.persist(&lock).unwrap();
        drop(lock);

        assert_eq!(dag.max_level().unwrap(), 3);
        assert_eq!(
            dag.children(Id(1000).into())
                .unwrap()
                .iter_desc()
                .collect::<Vec<Id>>(),
            vec![Id(1001)]
        );
    }

    #[test]
    fn test_all() {
        let dir = tempdir().unwrap();
        let mut dag = IdDag::open(dir.path()).unwrap();
        assert!(dag.all().unwrap().is_empty());
        dag.build_segments(Id(1001), &get_parents).unwrap();
        let all = dag.all().unwrap();
        // Id 0 is not referred.
        assert_eq!(dbg(&all), "1..=1001");
        assert_eq!(all.count(), 1001);

        // Insert discontinuous segments.
        let mut dag = IdDag::new_in_memory();
        // 10..=20
        dag.build_segments(Id(20), &|p| {
            Ok(if p > Id(10) { vec![p - 1] } else { vec![] })
        })
        .unwrap();
        // 3..=5, and 30..=40
        dag.build_segments(Id(40), &|p| {
            Ok(if p == Id(30) {
                vec![Id(5), Id(20)]
            } else if p > Id(3) {
                vec![p - 1]
            } else {
                vec![]
            })
        })
        .unwrap();
        let all = dag.all().unwrap();
        assert_eq!(dbg(&all), "3 4 5 10..=20 30..=40");
        assert_eq!(all.count(), 25);
    }

    #[test]
    fn test_parents() {
        let mut dag = IdDag::new_in_memory();
        dag.build_segments(Id(1001), &get_parents).unwrap();
        assert!(dag.max_level().unwrap() > 0);

        let sets = [
            // Entire high-level segments.
            dag.all().unwrap(),
            IdSet::from_spans([Id(3)..=Id(1001)]),
            // Parts of segments.
            IdSet::from_spans([Id(100)..=Id(200), Id(500)..=Id(510)]),
            // Many scattered commits.
            IdSet::from_spans((3..=1001).step_by(7).map(Id)),
            IdSet::from_spans((3..=1001).step_by(2).map(Id)),
            IdSet::empty(),
        ];
        for set in sets {
            let expected = IdSet::from_spans(
                set.iter_desc()
                    .flat_map(|id| dag.parent_ids(id).unwrap())
                    .collect::<Vec<_>>(),
            );
            assert_eq!(
                dbg(dag.parents(set.clone()).unwrap()),
                dbg(&expected),
                "parents of {set:?}"
            );
        }
    }

    #[test]
    fn test_parents_linear_segments() {
        fn check<S: IdDagStore>(mut dag: IdDag<S>) {
            dag.set_new_segment_size(2);
            dag.build_segments(Id(101), &|id| {
                Ok(if id == Id(0) {
                    vec![]
                } else if id.0 % 10 == 0 {
                    vec![id - 1, Id(0)]
                } else {
                    vec![id - 1]
                })
            })
            .unwrap();
            assert!(dag.max_level().unwrap() > 0);
            dag.build_segments(nid(5), &|id| {
                Ok(if id == nid(0) {
                    vec![Id(3)]
                } else {
                    vec![id - 1]
                })
            })
            .unwrap();

            let sets = [
                dag.all().unwrap(),
                IdSet::from(Id(0)),
                IdSet::from(Id(2)..=Id(8)),
                IdSet::from(Id(3)..=Id(67)),
                // Reuse flat segments across spans and split spans at boundaries.
                IdSet::from_spans([
                    Id(2)..=Id(5),
                    Id(7)..=Id(15),
                    Id(20)..=Id(24),
                    Id(27)..=Id(46),
                    Id(80)..=Id(101),
                ]),
                IdSet::from_spans((0..=101).step_by(2).map(Id)),
                // External parents cross groups and interleave with linear parents.
                IdSet::from_spans([Id(20)..=Id(25), nid(1)..=nid(2), nid(4)..=nid(5)]),
                IdSet::empty(),
            ];
            for set in sets {
                let expected =
                    IdSet::from_spans(set.iter_desc().flat_map(|id| dag.parent_ids(id).unwrap()));
                assert_eq!(
                    dag.parents(set.clone()).unwrap().as_spans(),
                    expected.as_spans(),
                    "parents of {set:?}"
                );
            }
            assert!(dag.parents(IdSet::from(Id(102))).is_err());
            assert!(dag.parents(IdSet::from(Id(200)..=nid(2))).is_err());
        }

        check(IdDag::new_in_memory());
        let dir = tempdir().unwrap();
        check(IdDag::open(dir.path()).unwrap());
    }

    #[test]
    fn test_flat_segments() {
        let dir = tempdir().unwrap();
        let test_dir = tempdir().unwrap();
        let mut dag = IdDag::open(dir.path()).unwrap();
        let mut test_dag = IdDag::open(test_dir.path()).unwrap();

        let empty_dag_segments = dag.flat_segments(Group::MASTER).unwrap();
        test_dag
            .build_segments_from_prepared_flat_segments(&empty_dag_segments)
            .unwrap();
        assert!(test_dag.all().unwrap().is_empty());

        dag.build_segments(Id(0), &get_parents).unwrap();
        dag.build_segments(Id(1001), &get_parents).unwrap();
        let flat_segments = dag.flat_segments(Group::MASTER).unwrap();
        test_dag
            .build_segments_from_prepared_flat_segments(&flat_segments)
            .unwrap();

        assert_eq!(test_dag.max_level().unwrap(), 3);
        assert_eq!(test_dag.all().unwrap().count(), 1002);

        let subset_flat_segments = dag
            .idset_to_flat_segments(IdSet::from_spans(vec![2..=4]))
            .unwrap();
        assert_eq!(subset_flat_segments.segments.len(), 3);
    }

    #[test]
    fn test_discontinuous_flat_segment_only_head() {
        let prepared = PreparedFlatSegments {
            segments: vec![
                FlatSegment {
                    low: Id(0),
                    high: Id(10),
                    parents: vec![],
                },
                FlatSegment {
                    low: Id(20),
                    high: Id(30),
                    parents: vec![Id(10)],
                },
            ]
            .into_iter()
            .collect(),
        };

        // Segment 20..=30 shouldn't have the "ONLY_HEAD" flag because of the gap.
        // In debug output it does not have "H" prefix.
        let mut dag = IdDag::new_in_memory();
        dag.build_segments_from_prepared_flat_segments(&prepared)
            .unwrap();
        let iter = dag.iter_segments_ascending(Id(0), 0).unwrap();
        assert_eq!(dbg_iter(iter), "[RH0-10[], 20-30[10]]");
    }

    #[test]
    fn test_roots_max_level_empty() {
        // Create segments in a way that the highest level
        // contains no segments in the master group.
        let mut iddag = IdDag::new_in_memory();
        let mut prepared = PreparedFlatSegments {
            segments: vec![
                FlatSegment {
                    low: Id(0),
                    high: Id(10),
                    parents: vec![],
                },
                FlatSegment {
                    low: nid(0),
                    high: nid(4),
                    parents: vec![],
                },
            ]
            .into_iter()
            .collect(),
        };
        for i in 1..=3 {
            prepared.segments.insert(FlatSegment {
                low: nid(5 * i),
                high: nid(5 * i + 1),
                parents: vec![],
            });
            prepared.segments.insert(FlatSegment {
                low: nid(5 * i + 2),
                high: nid(5 * i + 4),
                parents: vec![nid(5 * i + 1), nid(5 * i - 1)],
            });
        }
        iddag.set_new_segment_size(2);
        iddag
            .build_segments_from_prepared_flat_segments(&prepared)
            .unwrap();

        // The highest level is not 0. But the highest level exist in the non-master
        // group, not the master group.
        assert_eq!(iddag.max_level().unwrap(), 2);

        let all = iddag.all().unwrap();
        assert_eq!(dbg(&all), "0..=10 N0..=N19");

        let roots = iddag.roots(all).unwrap();
        assert_eq!(dbg(roots), "0 N0 N5 N10 N15");
    }

    #[test]
    fn test_strip() {
        let mut iddag = IdDag::new_in_memory();
        let mut prepared = PreparedFlatSegments::default();
        prepared.segments.insert(FlatSegment {
            low: Id(0),
            high: Id(100),
            parents: Vec::new(),
        });
        prepared.segments.insert(FlatSegment {
            low: Id(101),
            high: Id(200),
            parents: vec![Id(50)],
        });
        prepared.segments.insert(FlatSegment {
            low: Id(201),
            high: Id(300),
            parents: vec![Id(90)],
        });
        prepared.segments.insert(FlatSegment {
            low: nid(0),
            high: nid(100),
            parents: vec![Id(50), Id(250)],
        });
        prepared.segments.insert(FlatSegment {
            low: nid(101),
            high: nid(200),
            parents: vec![Id(50)],
        });
        iddag.set_new_segment_size(2);
        iddag
            .build_segments_from_prepared_flat_segments(&prepared)
            .unwrap();
        let all_before_remove = iddag.all().unwrap();
        let removed = iddag.strip(Id(70).into()).unwrap();
        let all_after_remove = iddag.all().unwrap();
        assert_eq!(
            all_before_remove.difference(&removed).as_spans(),
            all_after_remove.as_spans()
        );
        assert_eq!(
            all_after_remove.union(&removed).as_spans(),
            all_before_remove.as_spans()
        );
        assert_eq!(dbg(&removed), "70..=100 201..=300 N0..=N100");
        assert_eq!(
            dump_store_state(&iddag.store, &all_before_remove),
            "\nLv0: RH0-69[], 101-200[50], N101-N200[50]\nP->C: 50->101, 50->N101"
        );
    }

    #[test]
    fn test_virtual() -> Result<()> {
        let dir = tempdir()?;
        let mut iddag = IdDag::open(dir.path())?;
        let mut prepared = PreparedFlatSegments::default();
        prepared.segments.insert(FlatSegment {
            low: nid(0),
            high: nid(5),
            parents: Vec::new(),
        });
        prepared.segments.insert(FlatSegment {
            low: vid(0),
            high: vid(3),
            parents: vec![nid(2)],
        });
        prepared.segments.insert(FlatSegment {
            low: vid(5),
            high: vid(8),
            parents: vec![nid(3), vid(2)],
        });
        iddag.build_segments_from_prepared_flat_segments(&prepared)?;

        // all() skips VIRTUAL.
        assert_eq!(dbg(iddag.all()?), "N0..=N5");

        // all_with_virtual() includes VIRTUAL.
        assert_eq!(dbg(iddag.all_with_virtual()?), "N0..=N5 V0..=V3 V5..=V8");

        // children() follows into VIRTUAL.
        assert_eq!(dbg(iddag.children(nid(3).into())?), "N4 V5");
        assert_eq!(dbg(iddag.children_id(nid(3))?), "N4 V5");
        assert_eq!(dbg(iddag.children_set(nid(3).into())?), "N4 V5");
        assert_eq!(dbg(iddag.children_id(vid(2))?), "V3 V5");
        assert_eq!(dbg(iddag.children_set(vid(2).into())?), "V3 V5");

        // descendants() follows into VIRTUAL.
        assert_eq!(
            dbg(iddag.descendants(nid(2).into())?),
            "N2..=N5 V0..=V3 V5..=V8"
        );
        assert_eq!(dbg(iddag.descendants(vid(2).into())?), "V2 V3 V5..=V8");

        // range() works with VIRTUAL.
        assert_eq!(
            dbg(iddag.range(nid(1).into(), vid(2).into())?),
            "N1 N2 V0 V1 V2"
        );

        // Reloading drops VIRTUAL.
        {
            let lock = iddag.lock()?;
            iddag.persist(&lock)?;
            let iddag2 = IdDag::open(dir.path())?;
            assert_eq!(dbg(iddag2.all_with_virtual()?), "N0..=N5");
        }

        Ok(())
    }

    #[test]
    fn test_id_set_to_id_segments() {
        let mut iddag = IdDag::new_in_memory();

        // Insert some segments. Create a few levels.
        let mut prepared = PreparedFlatSegments::default();
        for g in &Group::ALL {
            let mut parents = vec![];
            for i in 0..=3 {
                let base = g.min_id() + 10 * i;
                prepared.segments.insert(FlatSegment {
                    low: base,
                    high: base + 4,
                    parents: parents.clone(),
                });
                prepared.segments.insert(FlatSegment {
                    low: base + 5,
                    high: base + 9,
                    parents: vec![base + 1, base + 4],
                });
                parents.push(base + 9);
            }
        }
        iddag.set_new_segment_size(2);
        iddag
            .build_segments_from_prepared_flat_segments(&prepared)
            .unwrap();
        // The highest level is not 0.
        assert_eq!(iddag.max_level().unwrap(), 2);

        // Tests about id_set_to_id_segments_with_max_level.
        let t = |id_set, level| -> Vec<String> {
            let id_segs = iddag
                .id_set_to_id_segments_with_max_level(&id_set, level)
                .unwrap();

            // Verify against other special-case APIs.
            if level >= iddag.max_level().unwrap() {
                let id_segs2 = iddag.id_set_to_id_segments(&id_set).unwrap();
                assert_eq!(&id_segs, &id_segs2);
            }
            if level == 0 {
                let flat_segments = iddag.idset_to_flat_segments(id_set).unwrap().segments;
                let id_segs2: Vec<IdSegment> = flat_segments
                    .into_iter()
                    .rev()
                    .map(|f| IdSegment {
                        high: f.high,
                        low: f.low,
                        parents: f.parents.clone(),
                        level: 0,
                        has_root: f.parents.is_empty(),
                    })
                    .collect();
                assert_eq!(&id_segs, &id_segs2);
            }

            id_segs.into_iter().map(dbg).collect::<Vec<_>>()
        };

        // Match a flat segment.
        assert_eq!(t(IdSet::from_spans(vec![10..=14]), 3), ["L0 10..=14 [9]"]);

        // Match multiple flat segments.
        assert_eq!(
            t(IdSet::from_spans(vec![20..=29]), 3),
            ["L0 25..=29 [21, 24]", "L0 20..=24 [9, 19]"]
        );

        // Match partial flat segments.
        assert_eq!(
            t(IdSet::from_spans(vec![21..=28]), 3),
            ["L0 25..=28 [21, 24]", "L0 21..=24 [20]"]
        );

        // Match a high level segment.
        assert_eq!(t(IdSet::from_spans(vec![0..=24]), 3), ["L2 0..=24 []R"]);

        // Respect max_level.
        assert_eq!(
            t(IdSet::from_spans(vec![0..=24]), 1),
            ["L1 15..=24 [11, 14, 9]", "L1 0..=14 []R"]
        );

        // Match various segments. Pick the highest level possible.
        assert_eq!(
            t(IdSet::from_spans(vec![Id(0)..=Id(39), nid(0)..=nid(39)]), 3),
            [
                "L0 N35..=N39 [N31, N34]",
                "L0 N30..=N34 [N9, N19, N29]",
                "L1 N20..=N29 [N9, N19]",
                "L2 N0..=N19 []R",
                "L0 35..=39 [31, 34]",
                "L1 25..=34 [21, 24, 9, 19]",
                "L2 0..=24 []R"
            ]
        );

        // Related APIs not covered by tests above.
        let high_level_id_seg = iddag
            .id_set_to_id_segments(&IdSet::from_spans(vec![0..=39]))
            .unwrap()
            .back()
            .unwrap()
            .clone();
        assert_eq!(dbg(&high_level_id_seg), "L2 0..=24 []R");
        let low_level_id_segs = iddag
            .id_segment_to_lower_level_id_segments(&high_level_id_seg)
            .unwrap();
        assert_eq!(
            dbg(&low_level_id_segs),
            "[L1 15..=24 [11, 14, 9], L1 0..=14 []R]"
        );
    }
}
