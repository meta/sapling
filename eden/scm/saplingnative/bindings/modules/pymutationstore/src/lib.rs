/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#![allow(non_camel_case_types)]

use std::cell::RefCell;
use std::io::Cursor;

use ::mutationstore::DagFlags;
use ::mutationstore::MutationStore;
use ::mutationstore::Repair;
use anyhow::Error;
use async_runtime::block_on;
use byteorder::ReadBytesExt;
use byteorder::WriteBytesExt;
use cpython::*;
use cpython_ext::PyNone;
use cpython_ext::PyPath;
use cpython_ext::ResultPyErrExt;
use cpython_ext::convert::Serde;
use pydag::Names;
use pydag::dagalgo::dagalgo;
use thiserror::Error;
use types::mutation::MutationEntry;
use types::node::Node;
use vlqencoding::VLQDecode;
use vlqencoding::VLQEncode;

/// Supported format of bundle version.
/// Format 1 is:
///  * Single byte version: 0x01
///  * VLQ-encoded count of entries: ``count``
///  * A sequence of ``count`` entries encoded using ``MutationEntry::serialize``
const BUNDLE_FORMAT_VERSION: u8 = 1u8;

pub fn init_module(py: Python, package: &str) -> PyResult<PyModule> {
    let name = [package, "mutationstore"].join(".");
    let m = PyModule::new(py, &name)?;
    m.add_class::<mutationentry>(py)?;
    m.add_class::<mutationstore>(py)?;
    m.add(
        py,
        "bundle",
        py_fn!(py, bundle(entries: Vec<mutationentry>)),
    )?;
    m.add(py, "unbundle", py_fn!(py, unbundle(data: PyBytes)))?;
    Ok(m)
}

fn bundle(py: Python, entries: Vec<mutationentry>) -> PyResult<PyBytes> {
    // Pre-allocate capacity for all the entries, plus one for the header and extra breathing room.
    let mut buf = Vec::with_capacity((entries.len() + 1) * types::mutation::DEFAULT_ENTRY_SIZE);
    buf.write_u8(BUNDLE_FORMAT_VERSION).map_pyerr(py)?;
    buf.write_vlq(entries.len()).map_pyerr(py)?;
    for entry in entries {
        let entry = entry.entry(py);
        entry.serialize(&mut buf).map_pyerr(py)?;
    }
    Ok(PyBytes::new(py, &buf))
}

fn unbundle(py: Python, data: PyBytes) -> PyResult<Vec<mutationentry>> {
    let mut cursor = Cursor::new(data.data(py));
    let version = cursor.read_u8().map_pyerr(py)?;
    if version != BUNDLE_FORMAT_VERSION {
        return Err(PyErr::new::<exc::IOError, _>(
            py,
            format!("Unsupported mutation format: {version}"),
        ));
    }
    let count = cursor.read_vlq().map_pyerr(py)?;
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        let entry = MutationEntry::deserialize(&mut cursor).map_pyerr(py)?;
        entries.push(mutationentry::create_instance(py, entry)?);
    }
    Ok(entries)
}

#[derive(Error, Debug)]
enum InvalidNode {
    #[error("Invalid successor node: {0}")]
    Successor(Error),
    #[error("Invalid predecessor node: {0}")]
    Predecessor(Error),
    #[error("Invalid split node: {0}")]
    Split(Error),
}

py_class!(class mutationentry |py| {
    data entry: MutationEntry;

    def __new__(
        _cls,
        succ: &PyBytes,
        preds: Option<Vec<PyBytes>>,
        split: Option<Vec<PyBytes>>,
        op: &PyString,
        user: &PyString,
        time: i64,
        tz: i32,
        extra: Option<Vec<(PyBytes, PyBytes)>>
    ) -> PyResult<mutationentry> {
        let succ = Node::from_slice(succ.data(py))
            .map_err(Error::from)
            .map_err(InvalidNode::Successor)
            .map_pyerr(py)?;
        let preds = {
            let mut nodes = Vec::new();
            if let Some(preds) = preds {
                for p in preds {
                    nodes.push(Node::from_slice(p.data(py))
                        .map_err(Error::from)
                        .map_err(InvalidNode::Predecessor)
                        .map_pyerr(py)?);
                }
            }
            nodes
        };
        let split = {
            let mut nodes = Vec::new();
            if let Some(split) = split {
                for s in split {
                    nodes.push(Node::from_slice(s.data(py))
                        .map_err(Error::from)
                        .map_err(InvalidNode::Split)
                        .map_pyerr(py)?);
                }
            }
            nodes
        };
        let op = op.to_string(py)?.into();
        let user = user.to_string(py)?.into();
        let extra = {
            let mut items = Vec::new();
            if let Some(extra) = extra {
                for (k, v) in extra {
                    items.push((Box::from(k.data(py)), Box::from(v.data(py))));
                }
            }
            items
        };
        mutationentry::create_instance(py, MutationEntry {
            succ, preds, split, op, user, time, tz, extra
        })
    }

    def succ(&self) -> PyResult<PyBytes> {
        Ok(PyBytes::new(py, self.entry(py).succ.as_ref()))
    }

    def preds(&self) -> PyResult<Vec<PyBytes>> {
        Ok(self.entry(py).preds.iter().map(|p| PyBytes::new(py, p.as_ref())).collect())
    }

    def split(&self) -> PyResult<Vec<PyBytes>> {
        Ok(self.entry(py).split.iter().map(|s| PyBytes::new(py, s.as_ref())).collect())
    }

    def op(&self) -> PyResult<String> {
        Ok(self.entry(py).op.clone())
    }

    def user(&self) -> PyResult<String> {
        Ok(self.entry(py).user.clone())
    }

    def time(&self) -> PyResult<i64> {
        Ok(self.entry(py).time)
    }

    def tz(&self) -> PyResult<i32> {
        Ok(self.entry(py).tz)
    }

    def extra(&self) -> PyResult<Vec<(PyBytes, PyBytes)>> {
        Ok(self.entry(py).extra.iter().map(|(k, v)| {
            (PyBytes::new(py, k.as_ref()), PyBytes::new(py, v.as_ref()))
        }).collect())
    }
});

py_class!(class mutationstore |py| {
    data mut_store: RefCell<MutationStore>;

    def __new__(_cls, path: &PyPath) -> PyResult<mutationstore> {
        let ms = MutationStore::open(path).map_pyerr(py)?;
        mutationstore::create_instance(py, RefCell::new(ms))
    }

    def add(&self, entry: &mutationentry) -> PyResult<PyNone> {
        let mut ms = self.mut_store(py).borrow_mut();
        ms.add(entry.entry(py)).map_pyerr(py)?;
        Ok(PyNone)
    }

    def addraw(&self, entry: &mutationentry) -> PyResult<PyNone> {
        let mut ms = self.mut_store(py).borrow_mut();
        ms.add_raw(entry.entry(py)).map_pyerr(py)?;
        Ok(PyNone)
    }

    def flush(&self) -> PyResult<PyNone> {
        let mut ms = self.mut_store(py).borrow_mut();
        block_on(ms.flush()).map_pyerr(py)?;
        Ok(PyNone)
    }

    def has(&self, succ: &PyBytes) -> PyResult<bool> {
        let succ = Node::from_slice(succ.data(py)).map_pyerr(py)?;
        let ms = self.mut_store(py).borrow();
        let entry = ms.get(succ).map_pyerr(py)?;
        Ok(entry.is_some())
    }

    /// get(succ) -> Optional[mutationentry].
    /// Query predecessor entry by a successor.
    def get(&self, succ: &PyBytes) -> PyResult<Option<mutationentry>> {
        let succ = Node::from_slice(succ.data(py)).map_pyerr(py)?;
        let ms = self.mut_store(py).borrow();
        let entry = ms.get(succ).map_pyerr(py)?;
        let entry = match entry {
            Some(entry) => Some(mutationentry::create_instance(py, entry)?),
            None => None,
        };
        Ok(entry)
    }

    def getsplithead(&self, node: &PyBytes) -> PyResult<Option<PyBytes>> {
        let node = Node::from_slice(node.data(py)).map_pyerr(py)?;
        let ms = self.mut_store(py).borrow();
        let entry = ms.get_split_head(node).map_pyerr(py)?;
        let succ = match entry {
            Some(entry) => Some(PyBytes::new(py, entry.succ.as_ref())),
            None => None,
        };
        Ok(succ)
    }

    /// Query successors by a predecessor.
    def getsuccessorssets(&self, node: &PyBytes) -> PyResult<Vec<Vec<PyBytes>>> {
        let node = Node::from_slice(node.data(py)).map_pyerr(py)?;
        let ms = self.mut_store(py).borrow();
        let ssets = ms.get_successors_sets(node).map_pyerr(py)?;
        let mut pyssets = Vec::with_capacity(ssets.len());
        for sset in ssets.into_iter() {
            let mut pysset = Vec::with_capacity(sset.len());
            for succ in sset.into_iter() {
                pysset.push(PyBytes::new(py, succ.as_ref()));
            }
            pyssets.push(pysset);
        }
        Ok(pyssets)
    }

    /// get_entries(predecessors, successors) -> List[mutationentry].
    ///
    /// Get all mutation entries that have one predecessor match or successor match.
    /// Might return duplicated entries.
    def get_entries(&self, predecessors: Serde<Vec<Node>>, successors: Serde<Vec<Node>>) -> PyResult<Vec<mutationentry>> {
        let ms = self.mut_store(py).borrow();
        let entries = ms.get_entries(&predecessors.0, &successors.0).map_pyerr(py)?;
        let entries = entries.into_iter().map(|e| mutationentry::create_instance(py, e)).collect::<PyResult<Vec<mutationentry>>>()?;
        Ok(entries)
    }

    /// get_dag(nodes, successors=True, predecessors=True, splits=False)
    ///
    /// Figure out connected components related to specified nodes.
    /// The returned graph supports DAG related calculations like
    /// ancestors, heads, roots, etc.
    ///
    /// If successors is True, follow successors.
    /// If predecessors is True, follow predecessors.
    /// Folds are included. If splits is True, every part of a split is a
    /// successor of the split commit; otherwise only the last part is.
    def getdag(&self, nodes: Vec<PyBytes>, successors: bool = true, predecessors: bool = true, splits: bool = false) -> PyResult<dagalgo> {
        let nodes = nodes
            .into_iter()
            .map(|n| Node::from_slice(n.data(py))).collect::<Result<Vec<_>, _>>().map_pyerr(py)?;
        let mut flags = DagFlags::empty();
        if successors {
            flags |= DagFlags::SUCCESSORS;
        }
        if predecessors {
            flags |= DagFlags::PREDECESSORS;
        }
        if splits {
            flags |= DagFlags::SPLITS;
        }
        let dag = block_on(self.mut_store(py).borrow().get_dag_advanced(nodes, flags)).map_pyerr(py)?;
        dagalgo::from_dag(py, dag)
    }

    /// Calculate the `obsolete` set from `public` and `draft` sets.
    def calculateobsolete(&self, public: Names, draft: Names) -> PyResult<Names> {
        let store = self.mut_store(py).borrow();
        Ok(Names(block_on(store.calculate_obsolete(public.0, draft.0)).map_pyerr(py)?))
    }

    @staticmethod
    def repair(path: &str) -> PyResult<String> {
        py.allow_threads(|| MutationStore::repair(path)).map_pyerr(py)
    }
});
