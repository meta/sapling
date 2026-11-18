/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Stage identities and payloads for pipeline derivation requests.

use anyhow::Result;
use anyhow::anyhow;
use anyhow::ensure;
use mononoke_types::MPath;
use mononoke_types::MPathElement;
use mononoke_types::MPathHash;
use mononoke_types::NonRootMPath;
use mononoke_types::ThriftConvert;
use serde::Deserialize;
use serde::Serialize;

/// Path-based identity of a pipeline stage.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum StageId {
    Manifest(MPath),
    Subentries(MPath, ChildNameRange),
    Finalize,
}

/// Hash-based identity of a pipeline stage.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StageKey {
    Manifest(MPathHash),
    Subentries(MPathHash, ChildNameRange),
    Finalize,
}

impl StageId {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Manifest(path) if path.is_root()) || matches!(self, Self::Finalize)
    }

    pub fn to_thrift(&self) -> derivation_queue_thrift::DerivationStageId {
        use derivation_queue_thrift::DerivationStageId;
        match self {
            Self::Manifest(path) => DerivationStageId::manifest(path.clone().into_thrift()),
            Self::Subentries(path, range) => {
                DerivationStageId::subentries(derivation_queue_thrift::SubentriesStagePayload {
                    path: path.clone().into_thrift(),
                    start: range.start.into(),
                    end: range.end.into(),
                })
            }
            Self::Finalize => {
                DerivationStageId::finalize(derivation_queue_thrift::FinalizeStagePayload {})
            }
        }
    }

    pub fn from_thrift(stage: derivation_queue_thrift::DerivationStageId) -> Result<Self> {
        use derivation_queue_thrift::DerivationStageId;
        match stage {
            DerivationStageId::manifest(path) => Ok(Self::Manifest(MPath::from_thrift(path)?)),
            DerivationStageId::subentries(payload) => Ok(Self::Subentries(
                MPath::from_thrift(payload.path)?,
                ChildNameRange::new(payload.start.try_into()?, payload.end.try_into()?)?,
            )),
            DerivationStageId::finalize(_) => Ok(Self::Finalize),
            DerivationStageId::UnknownField(x) => {
                Err(anyhow!("Unknown DerivationStageId variant: {x}"))
            }
        }
    }

    /// Whether this stage's parent output can resolve an entry at `path`.
    pub fn covers(&self, path: &NonRootMPath) -> bool {
        match self {
            Self::Manifest(prefix) => prefix.is_prefix_of(path),
            Self::Subentries(prefix, range) => range.contains_path(prefix, path),
            Self::Finalize => false,
        }
    }

    pub fn to_key(&self) -> StageKey {
        match self {
            StageId::Manifest(path) => StageKey::Manifest(path.get_path_hash()),
            StageId::Subentries(path, range) => StageKey::Subentries(path.get_path_hash(), *range),
            StageId::Finalize => StageKey::Finalize,
        }
    }
}

/// Stage payload sent by the pipeline tailer to its workers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DerivationStagePayload {
    Manifest(ManifestStagePayload),
    Subentries { path: MPath, range: ChildNameRange },
    Finalize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestStagePayload {
    pub path: MPath,
    pub deps: ManifestStageDependencies,
}

/// Inclusive first-byte bounds for immediate child names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChildNameRange {
    start: u8,
    end: u8,
}

impl ChildNameRange {
    pub fn new(start: u8, end: u8) -> Result<Self> {
        ensure!(start <= end, "invalid child-name range {start}..={end}");
        Ok(Self { start, end })
    }

    pub fn contains(&self, name: &MPathElement) -> bool {
        self.contains_byte(name.as_ref()[0])
    }

    pub fn contains_path(&self, prefix: &MPath, path: &NonRootMPath) -> bool {
        path.remove_prefix_component(prefix)
            .and_then(|path| path.iter().next().cloned())
            .is_some_and(|name| self.contains(&name))
    }

    pub fn contains_byte(&self, byte: u8) -> bool {
        (self.start..=self.end).contains(&byte)
    }

    pub fn suffix(&self) -> String {
        format!("{:02x}-{:02x}", self.start, self.end)
    }
}

/// Cut points are strictly increasing and exclude zero.
pub fn partition_ranges(cut_points: &[u8]) -> impl Iterator<Item = ChildNameRange> + '_ {
    std::iter::once(0)
        .chain(cut_points.iter().copied())
        .zip(
            cut_points
                .iter()
                .map(|cut| cut - 1)
                .chain(std::iter::once(u8::MAX)),
        )
        .map(|(start, end)| ChildNameRange { start, end })
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, derive_more::From)]
pub enum ManifestStageDependencies {
    Children(Vec<MPathElement>),
    #[from(ignore)]
    Partitions(Vec<u8>),
}

impl ManifestStageDependencies {
    pub fn children(&self) -> &[MPathElement] {
        match self {
            Self::Children(children) => children,
            Self::Partitions(_) => &[],
        }
    }
}

impl DerivationStagePayload {
    /// The absolute path prefix this stage covers, or `None` for the finalize
    /// stage which is not path-scoped.
    pub fn path(&self) -> Option<&MPath> {
        match self {
            DerivationStagePayload::Manifest(p) => Some(&p.path),
            DerivationStagePayload::Subentries { path, .. } => Some(path),
            DerivationStagePayload::Finalize => None,
        }
    }

    pub fn stage_id(&self) -> StageId {
        match self {
            Self::Manifest(payload) => StageId::Manifest(payload.path.clone()),
            Self::Subentries { path, range } => StageId::Subentries(path.clone(), *range),
            Self::Finalize => StageId::Finalize,
        }
    }

    /// Whether this stage derives the entry at this absolute path.
    pub fn owns(&self, path: &MPath) -> bool {
        match self {
            Self::Manifest(stage) => {
                stage.path.is_prefix_of(path)
                    && match &stage.deps {
                        ManifestStageDependencies::Children(children) => !children
                            .iter()
                            .any(|name| stage.path.join_element(Some(name)).is_prefix_of(path)),
                        ManifestStageDependencies::Partitions(_) => path == &stage.path,
                    }
            }
            Self::Subentries {
                path: prefix,
                range,
            } => path
                .clone()
                .into_optional_non_root_path()
                .is_some_and(|path| range.contains_path(prefix, &path)),
            Self::Finalize => false,
        }
    }

    pub fn dependencies(&self) -> Vec<StageId> {
        match self {
            Self::Manifest(payload) => match &payload.deps {
                ManifestStageDependencies::Children(children) => children
                    .iter()
                    .map(|name| StageId::Manifest(payload.path.join_element(Some(name))))
                    .collect(),
                ManifestStageDependencies::Partitions(cuts) => partition_ranges(cuts)
                    .map(|range| StageId::Subentries(payload.path.clone(), range))
                    .collect(),
            },
            Self::Subentries { .. } => Vec::new(),
            Self::Finalize => vec![StageId::Manifest(MPath::ROOT)],
        }
    }

    pub fn to_thrift(&self) -> derivation_queue_thrift::DerivationStagePayload {
        match self {
            DerivationStagePayload::Manifest(payload) => {
                derivation_queue_thrift::DerivationStagePayload::manifest(payload.to_thrift())
            }
            DerivationStagePayload::Subentries { path, range } => {
                derivation_queue_thrift::DerivationStagePayload::subentries(
                    derivation_queue_thrift::SubentriesStagePayload {
                        path: path.clone().into_thrift(),
                        start: range.start.into(),
                        end: range.end.into(),
                    },
                )
            }
            DerivationStagePayload::Finalize => {
                derivation_queue_thrift::DerivationStagePayload::finalize(
                    derivation_queue_thrift::FinalizeStagePayload {},
                )
            }
        }
    }

    pub fn from_thrift(payload: derivation_queue_thrift::DerivationStagePayload) -> Result<Self> {
        match payload {
            derivation_queue_thrift::DerivationStagePayload::manifest(payload) => Ok(
                DerivationStagePayload::Manifest(ManifestStagePayload::from_thrift(payload)?),
            ),
            derivation_queue_thrift::DerivationStagePayload::subentries(payload) => {
                Ok(Self::Subentries {
                    path: MPath::from_thrift(payload.path)?,
                    range: ChildNameRange::new(payload.start.try_into()?, payload.end.try_into()?)?,
                })
            }
            derivation_queue_thrift::DerivationStagePayload::finalize(_) => {
                Ok(DerivationStagePayload::Finalize)
            }
            derivation_queue_thrift::DerivationStagePayload::prepare(_) => {
                Err(anyhow!("preparation is not a derivation stage"))
            }
            derivation_queue_thrift::DerivationStagePayload::UnknownField(x) => {
                Err(anyhow!("Unknown DerivationStagePayload variant: {x}"))
            }
        }
    }
}

impl ManifestStagePayload {
    pub fn from_config(
        path: MPath,
        config: &metaconfig_types::DerivationPipelineStageConfig,
    ) -> Self {
        let deps = if !config.partition_cut_points.is_empty() {
            ManifestStageDependencies::Partitions(config.partition_cut_points.clone())
        } else {
            ManifestStageDependencies::Children(
                config
                    .dependencies
                    .iter()
                    .map(|path| {
                        path.iter()
                            .last()
                            .expect("validated child stage path")
                            .clone()
                    })
                    .collect(),
            )
        };
        Self { path, deps }
    }

    pub fn partitions(&self) -> Vec<ChildNameRange> {
        match &self.deps {
            ManifestStageDependencies::Partitions(cuts) => partition_ranges(cuts).collect(),
            ManifestStageDependencies::Children(_) => Vec::new(),
        }
    }

    pub fn to_thrift(&self) -> derivation_queue_thrift::ManifestStagePayload {
        derivation_queue_thrift::ManifestStagePayload {
            path: self.path.clone().into_thrift(),
            deps: self
                .deps
                .children()
                .iter()
                .map(|e| e.clone().into_thrift())
                .collect(),
            partition_cut_points: match &self.deps {
                ManifestStageDependencies::Children(_) => None,
                ManifestStageDependencies::Partitions(cuts) => {
                    Some(cuts.iter().map(|cut| i16::from(*cut)).collect())
                }
            },
        }
    }

    pub fn from_thrift(payload: derivation_queue_thrift::ManifestStagePayload) -> Result<Self> {
        let deps = match payload.partition_cut_points {
            Some(cuts) => {
                ensure!(
                    payload.deps.is_empty(),
                    "partitioned stages cannot have child stages"
                );
                let cuts = cuts
                    .into_iter()
                    .map(u8::try_from)
                    .collect::<Result<Vec<_>, _>>()?;
                ensure!(
                    !cuts.is_empty()
                        && cuts[0] != 0
                        && cuts.windows(2).all(|pair| pair[0] < pair[1]),
                    "invalid partition cut points"
                );
                ManifestStageDependencies::Partitions(cuts)
            }
            None => ManifestStageDependencies::Children(
                payload
                    .deps
                    .into_iter()
                    .map(MPathElement::from_thrift)
                    .collect::<Result<_>>()?,
            ),
        };
        Ok(Self {
            path: MPath::from_thrift(payload.path)?,
            deps,
        })
    }
}

#[cfg(test)]
mod tests {
    use mononoke_macros::mononoke;

    use super::*;

    #[mononoke::test]
    fn partition_ranges_cover_every_byte_once() -> Result<()> {
        let ranges = partition_ranges(b"dgjnqtw").collect::<Vec<_>>();
        assert_eq!(ranges.len(), 8);
        for byte in 1..=u8::MAX {
            if byte == b'/' {
                continue;
            }
            let name = MPathElement::new(vec![byte, b'x'])?;
            assert_eq!(
                ranges.iter().filter(|range| range.contains(&name)).count(),
                1
            );
        }
        assert!(!ranges[0].contains(&MPathElement::new(b"d".to_vec())?));
        assert!(ranges[1].contains(&MPathElement::new(b"d".to_vec())?));
        Ok(())
    }

    #[mononoke::test]
    fn coverage_and_ownership_have_distinct_boundaries() -> Result<()> {
        let path = MPath::new("stage")?;
        let partition = DerivationStagePayload::Subentries {
            path: path.clone(),
            range: ChildNameRange::new(b'a', b'm')?,
        };
        let assembly = DerivationStagePayload::Manifest(ManifestStagePayload {
            path: path.clone(),
            deps: ManifestStageDependencies::Partitions(vec![b'n']),
        });
        let parent = DerivationStagePayload::Manifest(ManifestStagePayload {
            path: MPath::ROOT,
            deps: vec![MPathElement::new_from_slice(b"stage")?].into(),
        });
        for (name, partition_owns, assembly_owns, parent_owns) in [
            ("stage", false, true, false),
            ("stage/a", true, false, false),
            ("stage/m/deep", true, false, false),
            ("stage/n", false, false, false),
            ("outside/a", false, false, true),
        ] {
            let name = NonRootMPath::new(name)?;
            let absolute = MPath::from(name.clone());
            assert_eq!(partition.owns(&absolute), partition_owns);
            assert_eq!(partition.stage_id().covers(&name), partition_owns);
            assert_eq!(assembly.owns(&absolute), assembly_owns);
            assert_eq!(parent.owns(&absolute), parent_owns);
            assert!(parent.stage_id().covers(&name));
            assert_eq!(assembly.stage_id().covers(&name), path.is_prefix_of(&name));
        }
        assert!(!partition.owns(&MPath::ROOT));
        assert!(parent.owns(&MPath::ROOT));
        Ok(())
    }

    #[mononoke::test]
    fn stage_payloads_round_trip() -> Result<()> {
        for payload in [
            DerivationStagePayload::Manifest(ManifestStagePayload {
                path: MPath::ROOT,
                deps: vec![MPathElement::new(b"dir".to_vec())?].into(),
            }),
            DerivationStagePayload::Manifest(ManifestStagePayload {
                path: MPath::ROOT,
                deps: ManifestStageDependencies::Partitions(b"dgjnqtw".to_vec()),
            }),
            DerivationStagePayload::Subentries {
                path: MPath::ROOT,
                range: ChildNameRange::new(b'w', 255)?,
            },
            DerivationStagePayload::Finalize,
        ] {
            assert_eq!(
                DerivationStagePayload::from_thrift(payload.to_thrift())?,
                payload
            );
        }
        Ok(())
    }
}
