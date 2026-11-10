/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The mutationstore is a store for recording records of commit mutations for
//! commits that are not in the local repository.
//!
//! It uses an indexedlog to store the data.  Each mutation entry corresponds to
//! the information of the mutation that led to the creation of a particular commit,
//! which is recorded as the successor in the entry.
//!
//! Entries can come from three possible places:
//!
//! * Commit metadata for a commit not available locally
//!
//! * Obsmarkers for repos that have been migrated from evolution tracking
//!
//! * Synthetic for entries created synthetically, e.g. by a pullcreatemarkers
//!   implementation.
//!
//! The other commits referred to in an entry must predate the successor commit.
//! For entries that originated from commits, this is ensured, as the successor
//! commit hash includes the other commit hashes.  For other entry types, it is
//! an error to refer to later commits, and any entry that causes a cycle will
//! be ignored.

#![allow(clippy::redundant_closure)]

use std::collections::HashMap;
use std::collections::HashSet;
use std::io::Cursor;
use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use bitflags::bitflags;
use dag::DagAlgorithm;
use dag::Set;
use dag::Vertex;
use dag::dag::MemDag;
use dag::ops::DagAddHeads;
use dag::ops::IdConvert;
use dag::set::hints::Flags;
use futures::stream::StreamExt;
use futures::stream::TryStreamExt;
use indexedlog::DefaultOpenOptions;
use indexedlog::OpenWithRepair;
pub use indexedlog::Repair;
use indexedlog::log::IndexDef;
use indexedlog::log::IndexOutput;
use indexedlog::log::Log;
use indexedlog::log::{self as ilog};
pub use types::mutation::MutationEntry;
use types::node::Node;
use vlqencoding::VLQDecodeAt;

pub struct MutationStore {
    log: Log,
    pending: Vec<MutationEntry>,
}

bitflags! {
    #[derive(PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Clone, Copy)]
    pub struct DagFlags: u8 {
        /// Include successors.
        const SUCCESSORS = 0b1;

        /// Include predecessors.
        const PREDECESSORS = 0b10;

        /// Link every part of a split to the split commit, not only the
        /// last part, whose entry records the split.
        const SPLITS = 0b100;
    }
}

const INDEX_PRED: usize = 0;
const INDEX_SUCC: usize = 1;
const INDEX_SPLIT: usize = 2;

impl DefaultOpenOptions<ilog::OpenOptions> for MutationStore {
    fn default_open_options() -> ilog::OpenOptions {
        const NODE_LEN: usize = Node::len();
        const SUCC_START: usize = 1usize;
        const PRED_COUNT_START: usize = SUCC_START + NODE_LEN;
        let succ_index = |_data: &[u8]| {
            vec![IndexOutput::Reference(
                SUCC_START as u64..PRED_COUNT_START as u64,
            )]
        };
        let pred_index = |data: &[u8]| {
            let (pred_count, pred_start) = data
                .read_vlq_at(PRED_COUNT_START)
                .map(|(pred_count, vlq_size)| (pred_count, PRED_COUNT_START + vlq_size))
                .unwrap_or((0, 0));
            (0..pred_count)
                .map(|i| pred_start + NODE_LEN * i)
                .map(|i: usize| IndexOutput::Reference(i as u64..i as u64 + NODE_LEN as u64))
                .collect()
        };
        let split_index = |data: &[u8]| {
            let (split_count, split_start) = data
                .read_vlq_at(PRED_COUNT_START)
                .and_then(|(pred_count, vlq1_size): (usize, usize)| {
                    data.read_vlq_at(PRED_COUNT_START + vlq1_size + NODE_LEN * pred_count)
                        .map(|(split_count, vlq2_size)| {
                            (
                                split_count,
                                PRED_COUNT_START + vlq1_size + NODE_LEN * pred_count + vlq2_size,
                            )
                        })
                })
                .unwrap_or((0, 0));
            (0..split_count)
                .map(|i| split_start + NODE_LEN * i)
                .map(|i: usize| IndexOutput::Reference(i as u64..i as u64 + NODE_LEN as u64))
                .collect()
        };

        // Allow some lag to make the indexing more efficient.  Set to 10KB, which is roughly
        // 100 records.
        let lag_threshold = 10000;
        ilog::OpenOptions::new().create(true).index_defs(vec![
            IndexDef::new("pred", pred_index).lag_threshold(lag_threshold),
            IndexDef::new("succ", succ_index).lag_threshold(lag_threshold),
            IndexDef::new("split", split_index).lag_threshold(lag_threshold),
        ])
    }
}

impl MutationStore {
    pub fn open(path: impl AsRef<Path>) -> Result<MutationStore> {
        let log = Self::default_open_options().open_with_repair(path.as_ref())?;
        let pending = Vec::new();
        Ok(MutationStore { log, pending })
    }

    /// Add an entry. Consider adding automatic entries based on this entry.
    /// See `flush` for automatic entries.
    pub fn add(&mut self, entry: &MutationEntry) -> Result<()> {
        self.add_raw(entry)?;
        self.pending.push(entry.clone());
        Ok(())
    }

    /// Add an entry. Do not consider adding automatic entries.
    pub fn add_raw(&mut self, entry: &MutationEntry) -> Result<()> {
        let mut buf = Vec::with_capacity(types::mutation::DEFAULT_ENTRY_SIZE);
        entry.serialize(&mut buf)?;
        self.log.append(buf.as_slice())?;
        Ok(())
    }

    pub async fn flush(&mut self) -> Result<()> {
        // If P -> Q, X -> Y are being added, and there is an existing chain P
        // -> ... -> X, add a Q -> Y marker automatically.
        // Note: P must not equal to X or Y.
        //
        // See also D7121487.

        // Prepare for calculation.
        let mut pred_map = HashMap::with_capacity(self.pending.len()); // node -> index
        let mut pred_nodes = Vec::with_capacity(self.pending.len());
        for (i, entry) in self.pending.iter().enumerate() {
            let pred = entry.preds[0];
            pred_map.insert(pred, i);
            pred_nodes.push(pred);
        }
        let pred_set =
            Set::from_static_names(pred_nodes.iter().map(|p| Vertex::copy_from(p.as_ref())));
        let dag = self
            .get_dag_advanced(pred_nodes, DagFlags::SUCCESSORS)
            .await?;
        let mut new_entries = Vec::new();

        // Scan through "X"s.
        for entry in &self.pending {
            let x = entry.preds[0];
            // Find all "P"s, as in P -> ... -> X, and X -> Y.
            let x_set = Vertex::copy_from(x.as_ref()).into();
            // "dag" is locally built and should be non-blocking.
            let x_ancestors = match dag.ancestors(x_set).await {
                Ok(set) => set,
                Err(_) => continue, // might have cycles
            };
            let ps = x_ancestors & pred_set.clone();
            let mut iter = ps.iter().await?;
            while let Some(p) = iter.next().await {
                let p = Node::from_slice(p?.as_ref())?;
                let y = entry.succ;
                if p == x || p == y {
                    continue;
                }
                let q = self.pending[pred_map[&p]].succ;
                if q == x || q == y || q == p {
                    continue;
                }
                // Copy P -> X to Q -> Y.
                let copy_entry = match self.get(x)? {
                    Some(entry) => entry,
                    _ => continue,
                };
                let op = if copy_entry.op.ends_with("-copy") {
                    copy_entry.op.clone()
                } else {
                    format!("{}-copy", copy_entry.op)
                };
                // The new entry will be the one returned by `get(y)`.
                // It overrides the "x -> y" entry.
                let new_entry = MutationEntry {
                    succ: y,
                    preds: vec![x, q],
                    op,
                    ..copy_entry
                };
                new_entries.push(new_entry);
            }
        }

        let mut buf = Vec::with_capacity(types::mutation::DEFAULT_ENTRY_SIZE);
        for entry in new_entries {
            buf.clear();
            entry.serialize(&mut buf)?;
            self.log.append(buf.as_slice())?;
        }

        self.log.flush()?;
        self.pending.clear();
        Ok(())
    }

    fn try_clone(&self) -> Result<Self> {
        Ok(Self {
            log: self.log.try_clone()?,
            pending: self.pending.clone(),
        })
    }

    /// Calculate the "obsolete" set, a subset of `draft` with visible successors.
    /// A vertex that is in `public` or `draft` is considered visible.
    pub async fn calculate_obsolete(&self, public: Set, draft: Set) -> Result<Set> {
        let visible = public | draft.clone();
        let this = Arc::new(self.try_clone()?);
        let hints = draft.hints().clone();
        hints.update_flags_with(|f| f - Flags::ANCESTORS - Flags::FULL);

        // Vertex -> Node.
        let draft_nodes = draft
            .iter()
            .await?
            .filter_map(|v| async { v.ok().and_then(|v| Node::from_slice(v.as_ref()).ok()) })
            .collect::<Vec<Node>>()
            .await;

        // Obtain the obsolete graph about draft successors.
        let obsdag = this
            .get_dag_advanced(draft_nodes, DagFlags::SUCCESSORS)
            .await
            .map_err(|e| dag::errors::BackendError::Other(e))?;

        // Filter out invisible successors.
        let obsvisible = {
            let mut obsall = obsdag.all().await?;
            // In a non-lazy graph the following code is good enough to calculate
            // obsvisible: obsdag.ancestors(obsall & visible).await?
            //
            // However, in a lazy graph obsdag.all() might have too many names
            // outside the main graph and cause excessive server-side lookups.
            // So we manually ignore names not in the local graph to avoid the
            // slow path.
            if let Some(visible_id_convert) = visible.id_convert() {
                let obsnames: Vec<Vertex> = { obsall.iter().await?.try_collect().await? };
                let obsnames: Vec<Vertex> = {
                    let contains = visible_id_convert
                        .contains_vertex_name_locally(&obsnames)
                        .await?;
                    obsnames
                        .into_iter()
                        .zip(contains)
                        .filter_map(|(v, c)| if c { Some(v) } else { None })
                        .collect()
                };
                obsall = Set::from_static_names(obsnames);
            }
            obsdag.ancestors(obsall & visible).await?
        };

        // Heads of `obsvisible` are not obsoleted. Other part (parent) of
        // `obsvisible` are obsoleted.
        let obsoleted = obsdag.parents(obsvisible).await?;

        // Filter out unknown nodes.
        let obsoleted = draft & obsoleted;

        // Flatten the set for performance.
        Ok(obsoleted.flatten().await?)
    }

    /// Query successors by a predecessor.
    pub fn get_successors_sets(&self, node: Node) -> Result<Vec<Vec<Node>>> {
        let mut successors_sets = Vec::new();
        for entry in self.log.lookup(INDEX_PRED, node)? {
            let mutation_entry = MutationEntry::deserialize(&mut Cursor::new(entry?))?;
            let mut successors = Vec::new();
            successors.extend(&mutation_entry.split);
            successors.push(mutation_entry.succ);
            successors_sets.push(successors);
        }
        Ok(successors_sets)
    }

    /// Return whether a node has any recorded successors.
    pub fn has_successors(&self, node: Node) -> Result<bool> {
        Ok(self
            .log
            .lookup(INDEX_PRED, node)?
            .next()
            .transpose()?
            .is_some())
    }

    /// Query predecessors by a successor. Consider split.
    pub fn get_predecessors(&self, node: Node) -> Result<Vec<Node>> {
        let mut lookup = self
            .log
            .lookup(INDEX_SUCC, node)?
            .chain(self.log.lookup(INDEX_SPLIT, node)?);
        let predecessors = if let Some(entry) = lookup.next() {
            let mutation_entry = MutationEntry::deserialize(&mut Cursor::new(entry?))?;
            mutation_entry.preds
        } else {
            vec![]
        };
        Ok(predecessors)
    }

    pub fn get_split_head(&self, node: Node) -> Result<Option<MutationEntry>> {
        let mutation_entry = match self.log.lookup(INDEX_SPLIT, node)?.next() {
            Some(entry) => Some(MutationEntry::deserialize(&mut Cursor::new(entry?))?),
            None => None,
        };
        Ok(mutation_entry)
    }

    /// Query predecessor entry by a successor.
    pub fn get(&self, succ: Node) -> Result<Option<MutationEntry>> {
        let mutation_entry = match self.log.lookup(INDEX_SUCC, succ)?.next() {
            Some(entry) => Some(MutationEntry::deserialize(&mut Cursor::new(entry?))?),
            None => None,
        };
        Ok(mutation_entry)
    }

    /// Get all mutation entries that have one predecessor match or successor match.
    /// Might return duplicated entries.
    pub fn get_entries(
        &self,
        predecessors: &[Node],
        successors: &[Node],
    ) -> Result<Vec<MutationEntry>> {
        let mut result = Vec::new();
        for node in predecessors {
            for entry in self.log.lookup(INDEX_PRED, node)? {
                let entry = MutationEntry::deserialize(&mut Cursor::new(entry?))?;
                result.push(entry);
            }
        }
        for node in successors {
            for entry in self.log.lookup(INDEX_SUCC, node)? {
                let entry = MutationEntry::deserialize(&mut Cursor::new(entry?))?;
                result.push(entry);
            }
        }
        Ok(result)
    }

    /// Return a connected component that includes `nodes` and represents
    /// commit replacement relations.  The returned graph supports graph
    /// operations like common ancestors, heads, roots, etc. Parents in the
    /// graph are predecessors.
    pub async fn get_dag(&self, nodes: Vec<Node>) -> Result<MemDag> {
        self.get_dag_advanced(nodes, DagFlags::SUCCESSORS | DagFlags::PREDECESSORS)
            .await
    }

    /// Advanced version of `get_dag`. Specify whether to include successors or
    /// predecessors explicitly.
    pub async fn get_dag_advanced(&self, nodes: Vec<Node>, flags: DagFlags) -> Result<MemDag> {
        // Raw parent map. Might contain cycles.
        let mut parent_map = HashMap::<Node, Vec<Node>>::new();
        let mut non_heads = HashSet::<Node>::new();
        let mut add_parent = |parent: &Node, child: &Node| {
            let parents = parent_map.entry(child.clone()).or_default();
            if !parents.contains(parent) {
                parents.push(parent.clone());
                non_heads.insert(parent.clone());
            }
        };

        // Visit nodes. Fill parent map.
        let requested = nodes.clone();
        let mut to_visit = nodes;
        let mut connected = HashSet::new();
        while let Some(node) = to_visit.pop() {
            if !connected.insert(node.clone()) {
                continue;
            }
            let splits = flags.contains(DagFlags::SPLITS);
            if flags.contains(DagFlags::SUCCESSORS) {
                for entry in self.log.lookup(INDEX_PRED, node)? {
                    let entry = MutationEntry::deserialize(&mut Cursor::new(entry?))?;
                    let parts: &[Node] = if splits { &entry.split } else { &[] };
                    for succ in parts.iter().chain(std::iter::once(&entry.succ)) {
                        add_parent(&node, succ);
                        to_visit.push(*succ);
                    }
                }
            }
            if flags.contains(DagFlags::PREDECESSORS) {
                let split_entries = if splits {
                    Some(self.log.lookup(INDEX_SPLIT, node)?)
                } else {
                    None
                };
                for entry in self
                    .log
                    .lookup(INDEX_SUCC, node)?
                    .chain(split_entries.into_iter().flatten())
                {
                    let entry = MutationEntry::deserialize(&mut Cursor::new(entry?))?;
                    for pred in entry.preds {
                        add_parent(&pred, &node);
                        to_visit.push(pred);
                    }
                }
            }
        }

        // Construct parent_func.
        let parent_func = move |node: Vertex| -> dag::Result<Vec<Vertex>> {
            match parent_map.get(&Node::from_slice(node.as_ref()).unwrap()) {
                None => Ok(Vec::new()),
                Some(parents) => Ok(parents
                    .iter()
                    .map(|n| Vertex::copy_from(n.as_ref()))
                    .collect()),
            }
        };
        let parent_func = dag::utils::break_parent_func_cycle(parent_func);

        // Calculate heads. This makes multiple things more efficient:
        // `add_heads`, `break_parent_func_cycle`, and the resulting graph.
        let mut heads: Vec<Node> = connected.difference(&non_heads).cloned().collect();
        heads.sort_unstable();
        let heads: Vec<Vertex> = heads
            .into_iter()
            .map(|n| Vertex::copy_from(n.as_ref()))
            .collect();

        // Construct the graph.
        let mut dag = MemDag::new();
        let parents: Box<dyn Fn(Vertex) -> dag::Result<Vec<Vertex>> + Send + Sync> =
            Box::new(parent_func);

        // Inserting to a memory DAG from a fully known parent function is non-blocking.
        dag.add_heads(&parents, &heads.into()).await?;

        // A loop that nothing leads out of has no head, so it is not an
        // ancestor of any head. Add the requested nodes in such loops, so the
        // graph always contains every requested node.
        let mut missing = Vec::new();
        for node in requested {
            let vertex = Vertex::copy_from(node.as_ref());
            if !dag.contains_vertex_name(&vertex).await? {
                missing.push(vertex);
            }
        }
        if !missing.is_empty() {
            dag.add_heads(&parents, &missing.into()).await?;
        }
        Ok(dag)
    }
}

#[cfg(test)]
mod tests {
    use dag::DagAlgorithm;
    use dag::nonblocking::non_blocking_result;
    use dag::nonblocking::non_blocking_result as r;
    use rand::SeedableRng;
    use rand_chacha::ChaChaRng;
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn test_basic_store() {
        let mut rng = ChaChaRng::from_seed([0u8; 32]);
        let dir = TempDir::with_prefix("mutationstore.").unwrap();
        let nodes = Node::random_distinct(&mut rng, 20);

        {
            let mut ms = MutationStore::open(dir.path()).expect("can open the store");
            ms.add(&MutationEntry {
                succ: nodes[1],
                preds: vec![nodes[0], nodes[2], nodes[3]],
                split: vec![],
                op: "fold".into(),
                user: "test".into(),
                time: 123456789,
                tz: -7200,
                extra: vec![(
                    Box::from(&b"note"[..]),
                    Box::from(&b"note about folding"[..]),
                )],
            })
            .expect("can add to the store");
            ms.add(&MutationEntry {
                succ: nodes[4],
                preds: vec![nodes[0]],
                split: vec![nodes[5], nodes[6]],
                op: "split".into(),
                user: "test".into(),
                time: 123456789,
                tz: -7200,
                extra: vec![],
            })
            .expect("can add to the store");

            r(ms.flush()).expect("can flush the store");
        }
        {
            let ms = MutationStore::open(dir.path()).expect("can re-open the store");
            let mut expected_successors_sets =
                vec![vec![nodes[1]], vec![nodes[5], nodes[6], nodes[4]]];
            expected_successors_sets.sort_unstable();
            let mut successors_sets = ms
                .get_successors_sets(nodes[0])
                .expect("can get successors sets");
            successors_sets.sort_unstable();
            assert_eq!(successors_sets, expected_successors_sets);
            assert_eq!(
                ms.get_successors_sets(nodes[3])
                    .expect("can get successors sets"),
                vec![vec![nodes[1]]]
            );
            assert_eq!(
                ms.get_successors_sets(nodes[1])
                    .expect("can get successors sets"),
                Vec::<Vec<Node>>::new()
            );
            assert!(ms.has_successors(nodes[0]).expect("can check successors"));
            assert!(!ms.has_successors(nodes[1]).expect("can check successors"));
            assert_eq!(
                ms.get_predecessors(nodes[1]).expect("can get predecessors"),
                vec![nodes[0], nodes[2], nodes[3]]
            );
            assert_eq!(
                ms.get_predecessors(nodes[4]).expect("can get predecessors"),
                vec![nodes[0]]
            );
            assert_eq!(
                ms.get_predecessors(nodes[5]).expect("can get predecessors"),
                vec![nodes[0]]
            );
            assert_eq!(
                ms.get_predecessors(nodes[3]).expect("can get predecessors"),
                vec![]
            );
            assert_eq!(
                ms.get_split_head(nodes[5])
                    .expect("can get split head")
                    .unwrap()
                    .succ,
                nodes[4],
            );
            assert_eq!(
                &ms.get(nodes[4])
                    .expect("can get mutation entry")
                    .unwrap()
                    .user,
                "test",
            );
            assert_eq!(
                &ms.get(nodes[1])
                    .expect("can get mutation entry")
                    .unwrap()
                    .extra[0]
                    .1,
                &Box::from(&b"note about folding"[..])
            );
        }
    }

    #[test]
    fn test_dag() -> Result<()> {
        let dir = TempDir::with_prefix("mutationstore.")?;
        let mut ms = MutationStore::open(dir.path())?;
        let parents = drawdag::parse(
            r#"
             D E Z
             |\| |
             B C Y
             |/  |
             A   X
             "#,
        );
        // str (length 1) -> Node
        let n = |s: &str| -> Node { Node::from_slice(s.repeat(Node::len()).as_bytes()).unwrap() };
        let mut iter = parents.iter().collect::<Vec<_>>();
        iter.sort();
        for (name, parents) in iter {
            let node = n(name);
            for parent in parents {
                let parent = n(parent);
                ms.add(&MutationEntry {
                    succ: node,
                    preds: vec![parent],
                    split: vec![],
                    op: "rewrite".into(),
                    user: "test".into(),
                    time: 123456789,
                    tz: -7200,
                    extra: vec![],
                })?;
            }
        }

        let dag = r(ms.get_dag(vec![n("B")]))?;
        assert_eq!(r(non_blocking_result(dag.all())?.count())?, 5); // A B C D E
        assert_eq!(
            dag::render::render_dag(&dag, |v| Some(format!("({})", v.as_ref()[0] as char)))?,
            r#"
            o  4545454545454545454545454545454545454545 (E)
            │
            │ o  4444444444444444444444444444444444444444 (D)
            ╭─┤
            o │  4343434343434343434343434343434343434343 (C)
            │ │
            │ o  4242424242424242424242424242424242424242 (B)
            ├─╯
            o  4141414141414141414141414141414141414141 (A)"#
        );
        Ok(())
    }

    #[test]
    fn test_dag_cycle() -> Result<()> {
        let dir = TempDir::with_prefix("mutationstore.")?;
        let mut ms = MutationStore::open(dir.path())?;

        for (pred, succ) in [("A", "B"), ("B", "C"), ("C", "A")] {
            add(&mut ms, pred, succ)?;
        }
        r(ms.flush())?;

        // A cycle without a head is still rendered from the requested node,
        // with the cycle broken below it.
        assert_eq!(
            render(&ms, "A")?,
            r#"
            o  4141414141414141414141414141414141414141 (A)
            │
            o  4343434343434343434343434343434343434343 (C)
            │
            o  4242424242424242424242424242424242424242 (B)"#
        );
        assert_eq!(
            render(&ms, "B")?,
            r#"
            o  4242424242424242424242424242424242424242 (B)
            │
            o  4141414141414141414141414141414141414141 (A)
            │
            o  4343434343434343434343434343434343434343 (C)"#
        );
        assert_eq!(
            render(&ms, "C")?,
            r#"
            o  4343434343434343434343434343434343434343 (C)
            │
            o  4242424242424242424242424242424242424242 (B)
            │
            o  4141414141414141414141414141414141414141 (A)"#
        );

        // Add a "head" to "revive" the graph.
        add(&mut ms, "C", "D")?;
        r(ms.flush())?;
        assert_eq!(
            render(&ms, "D")?,
            r#"
            o  4444444444444444444444444444444444444444 (D)
            │
            o  4343434343434343434343434343434343434343 (C)
            │
            o  4242424242424242424242424242424242424242 (B)
            │
            o  4141414141414141414141414141414141414141 (A)"#
        );

        Ok(())
    }

    #[test]
    fn test_copy_entries() -> Result<()> {
        let dir = TempDir::with_prefix("mutationstore.")?;
        let mut ms = MutationStore::open(dir.path())?;

        for (pred, succ) in [("P", "E"), ("E", "X")] {
            add(&mut ms, pred, succ)?;
        }
        r(ms.flush())?;

        for (pred, succ) in [("P", "Q"), ("X", "Y")] {
            add(&mut ms, pred, succ)?;
        }

        // Before flush, Q -> Y is not connected.
        assert_eq!(
            render(&ms, "P")?,
            r#"
            o  5959595959595959595959595959595959595959 (Y)
            │
            o  5858585858585858585858585858585858585858 (X)
            │
            o  4545454545454545454545454545454545454545 (E)
            │
            │ o  5151515151515151515151515151515151515151 (Q)
            ├─╯
            o  5050505050505050505050505050505050505050 (P)"#
        );

        // After flush, Q -> Y is auto created.
        r(ms.flush())?;
        assert_eq!(
            render(&ms, "P")?,
            r#"
            o    5959595959595959595959595959595959595959 (Y)
            ├─╮
            │ o  5151515151515151515151515151515151515151 (Q)
            │ │
            o │  5858585858585858585858585858585858585858 (X)
            │ │
            o │  4545454545454545454545454545454545454545 (E)
            ├─╯
            o  5050505050505050505050505050505050505050 (P)"#
        );

        Ok(())
    }

    #[test]
    fn test_calculate_obsolete() -> Result<()> {
        let dir = TempDir::with_prefix("mutationstore.")?;
        let mut ms = MutationStore::open(dir.path())?;

        // C   F  # C -> F
        // |   |
        // B D E  # B -> D -> E
        //  \|/   # X -> Y
        // X Y Z  # Y, Z are public; X, B, E, C are draft

        for (pred, succ) in [("B", "D"), ("D", "E"), ("C", "F"), ("X", "Y")] {
            add(&mut ms, pred, succ)?;
        }
        let public = Set::from_static_names(vec![v("Y"), v("Z")]);
        let draft = Set::from_static_names(vec![v("B"), v("E"), v("C"), v("X")]);
        let obsolete = non_blocking_result(ms.calculate_obsolete(public, draft))?;

        // B is obsoleted. It has a visible successor E (draft).
        assert!(r(obsolete.contains(&v("B")))?);

        // X is obsoleted. It has a visible successor Y (public).
        assert!(r(obsolete.contains(&v("X")))?);

        // C does not have a visible successor.
        assert!(!r(obsolete.contains(&v("C")))?);

        // D is not visible.
        assert!(!r(obsolete.contains(&v("D")))?);

        // A is not a draft.
        assert!(!r(obsolete.contains(&v("A")))?);

        // The set evaluated.
        // (0x42 == b'B', 0x58 == b'X')
        assert_eq!(format!("{obsolete:.2?}"), "<static [42, 58]>");

        // E does not have a successor.
        assert!(!r(obsolete.contains(&v("E")))?);

        Ok(())
    }

    /// Create a node from a single-char string.
    fn n(s: impl ToString) -> Node {
        Node::from_slice(s.to_string().repeat(Node::len()).as_bytes()).unwrap()
    }

    /// Create a vertex from a single-char string.
    fn v(s: impl ToString) -> Vertex {
        n(s).as_ref().to_vec().into()
    }

    /// Add (test) edges to the mutation store.
    fn add(ms: &mut MutationStore, pred: &str, succ: &str) -> Result<()> {
        ms.add(&MutationEntry {
            preds: vec![n(pred)],
            succ: n(succ),
            split: vec![],
            op: "rewrite".into(),
            user: "test".into(),
            time: 1,
            tz: -7200,
            extra: vec![],
        })
    }

    #[test]
    fn test_dag_splits() -> Result<()> {
        let dir = TempDir::with_prefix("mutationstore.")?;
        let mut ms = MutationStore::open(dir.path())?;
        // A is split into B and C; C is amended into D.
        ms.add(&MutationEntry {
            succ: n("C"),
            preds: vec![n("A")],
            split: vec![n("B")],
            op: "split".into(),
            user: "test".into(),
            time: 123456789,
            tz: -7200,
            extra: vec![],
        })?;
        add(&mut ms, "C", "D")?;
        r(ms.flush())?;

        let render_flags = |s: &str, flags: DagFlags| -> Result<String> {
            let dag = r(ms.get_dag_advanced(vec![n(s)], flags))?;
            dag::render::render_dag(&dag, |v| Some(format!("({})", v.as_ref()[0] as char)))
        };
        let both = DagFlags::SUCCESSORS | DagFlags::PREDECESSORS;

        // Without SPLITS, B is not connected.
        assert_eq!(
            render_flags("A", both)?,
            r#"
            o  4444444444444444444444444444444444444444 (D)
            │
            o  4343434343434343434343434343434343434343 (C)
            │
            o  4141414141414141414141414141414141414141 (A)"#
        );
        assert_eq!(
            render_flags("B", both)?,
            r#"
            o  4242424242424242424242424242424242424242 (B)"#
        );

        // With SPLITS, both parts are successors of A, from either side.
        let expected = r#"
            o  4444444444444444444444444444444444444444 (D)
            │
            o  4343434343434343434343434343434343434343 (C)
            │
            │ o  4242424242424242424242424242424242424242 (B)
            ├─╯
            o  4141414141414141414141414141414141414141 (A)"#;
        assert_eq!(render_flags("A", both | DagFlags::SPLITS)?, expected);
        assert_eq!(render_flags("B", both | DagFlags::SPLITS)?, expected);
        Ok(())
    }

    /// Render the mutation store for the given nodes.
    fn render(ms: &MutationStore, s: &str) -> Result<String> {
        let dag = r(ms.get_dag(s.chars().map(n).collect::<Vec<Node>>()))?;
        dag::render::render_dag(&dag, |v| Some(format!("({})", v.as_ref()[0] as char)))
    }
}
