/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Intermediate stage outputs travel with requests; canonical mappings remain durable.

use std::any::Any;
use std::any::TypeId;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::fmt::Debug;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::Result;
use anyhow::ensure;
use bytes::Bytes;
use context::CoreContext;
use futures::StreamExt;
use futures::TryStreamExt;
use futures::stream;
use mononoke_types::BlobstoreBytes;
use mononoke_types::ChangesetId;
use mononoke_types::DerivableType;
use mononoke_types::MPath;
use mononoke_types::hash::Blake2;
use tokio::sync::OnceCell;

use crate::DerivationContext;
use crate::PipelineDerivable;
use crate::StageId;

/// Compare stage contents, resolving deferred inputs only for verification.
#[async_trait::async_trait]
pub trait StageOutput: Send + Sync + Clone + Debug + 'static {
    async fn equivalent(&self, other: &Self) -> Result<bool>;
}

#[async_trait::async_trait]
impl<T: Send + Sync + Clone + Debug + PartialEq + 'static> StageOutput for T {
    async fn equivalent(&self, other: &Self) -> Result<bool> {
        Ok(self == other)
    }
}

/// Encoded outputs for one derived type and stage, carried by the worker RPC.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StageOutputs {
    pub derived_data_type: DerivableType,
    pub stage: StageId,
    pub outputs: Arc<BTreeMap<ChangesetId, Bytes>>,
}

impl StageOutputs {
    pub fn from_thrift(outputs: derivation_queue_thrift::StageOutputs) -> Result<Self> {
        Ok(Self {
            derived_data_type: DerivableType::from_thrift(outputs.derived_data_type)?,
            stage: StageId::from_thrift(outputs.stage)?,
            outputs: Arc::new(
                outputs
                    .outputs
                    .into_iter()
                    .map(|(csid, bytes)| Ok((ChangesetId::from_thrift(csid)?, bytes)))
                    .collect::<Result<_>>()?,
            ),
        })
    }

    pub fn to_thrift(&self) -> derivation_queue_thrift::StageOutputs {
        derivation_queue_thrift::StageOutputs {
            derived_data_type: self.derived_data_type.into_thrift(),
            stage: self.stage.to_thrift(),
            outputs: self
                .outputs
                .iter()
                .map(|(csid, bytes)| (csid.into_thrift(), bytes.clone()))
                .collect(),
        }
    }
}

/// Encoded edits for one stage, produced before that stage derives its batch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedStageInputs {
    pub stage: StageId,
    pub changes: BTreeMap<ChangesetId, Bytes>,
}

impl PreparedStageInputs {
    pub fn from_thrift(inputs: derivation_queue_thrift::PreparedStageInputs) -> Result<Self> {
        Ok(Self {
            stage: StageId::from_thrift(inputs.stage)?,
            changes: inputs
                .changes
                .into_iter()
                .map(|(csid, bytes)| Ok((ChangesetId::from_thrift(csid)?, bytes)))
                .collect::<Result<_>>()?,
        })
    }

    pub fn to_thrift(&self) -> derivation_queue_thrift::PreparedStageInputs {
        derivation_queue_thrift::PreparedStageInputs {
            stage: self.stage.to_thrift(),
            changes: self
                .changes
                .iter()
                .map(|(csid, bytes)| (csid.into_thrift(), bytes.clone()))
                .collect(),
        }
    }
}

#[derive(Debug)]
pub struct StageBatchResult {
    pub duration: Duration,
    pub outputs: Option<StageOutputs>,
}

/// Immutable outputs supplied by a request's parent and dependency batches.
#[derive(Default)]
pub struct StageInputs {
    outputs: HashMap<(DerivableType, StageId), BTreeMap<ChangesetId, Bytes>>,
    snapshots: Mutex<HashMap<(TypeId, Blake2), Arc<OnceCell<Arc<dyn Any + Send + Sync>>>>>,
}

impl StageInputs {
    pub fn new(batches: &[StageOutputs]) -> Result<Arc<Self>> {
        let mut inputs = Self::default();
        for batch in batches {
            let outputs = inputs
                .outputs
                .entry((batch.derived_data_type, batch.stage.clone()))
                .or_default();
            for (csid, value) in batch.outputs.iter() {
                if let Some(previous) = outputs.insert(*csid, value.clone()) {
                    ensure!(
                        previous == *value,
                        "conflicting {:?} {:?} outputs for {csid}",
                        batch.derived_data_type,
                        batch.stage
                    );
                }
            }
        }
        Ok(Arc::new(inputs))
    }

    pub(crate) async fn load_snapshot<M: Clone + Send + Sync + 'static>(
        &self,
        id: Blake2,
        load: impl AsyncFnOnce() -> Result<M>,
    ) -> Result<M> {
        let cell = self
            .snapshots
            .lock()
            .unwrap()
            .entry((TypeId::of::<M>(), id))
            .or_default()
            .clone();
        let map = cell
            .get_or_try_init(async || {
                Ok::<_, anyhow::Error>(Arc::new(load().await?) as Arc<dyn Any + Send + Sync>)
            })
            .await?;
        Ok(map
            .downcast_ref::<M>()
            .expect("snapshot cache is keyed by map type")
            .clone())
    }

    fn get(&self, ty: DerivableType, stage: &StageId, csid: ChangesetId) -> Option<BlobstoreBytes> {
        self.outputs
            .get(&(ty, stage.clone()))?
            .get(&csid)
            .cloned()
            .map(BlobstoreBytes::from_bytes)
    }
}

/// Persist canonical mappings and return all other outputs to the caller.
pub async fn store_stage_outputs<T: PipelineDerivable, Output, F>(
    ctx: &CoreContext,
    derivation: &DerivationContext,
    stage: &StageId,
    outputs: HashMap<ChangesetId, Output>,
    mapping_key: impl Fn(ChangesetId) -> Option<String>,
    encode: impl Fn(Output) -> F,
) -> Result<Option<StageOutputs>>
where
    F: std::future::Future<Output = Result<BlobstoreBytes>>,
{
    let outputs = stream::iter(outputs)
        .map(|(csid, output)| {
            let key = mapping_key(csid);
            let encoded = encode(output);
            async move {
                let bytes = encoded.await?;
                if let Some(key) = key {
                    derivation.blobstore().put(ctx, key, bytes).await?;
                    Ok::<_, anyhow::Error>(None)
                } else {
                    Ok(Some((csid, bytes.into_bytes())))
                }
            }
        })
        .buffered(100)
        .try_filter_map(|entry| async move { Ok(entry) })
        .try_collect::<BTreeMap<_, _>>()
        .await?;
    Ok((!outputs.is_empty()).then(|| StageOutputs {
        derived_data_type: T::VARIANT,
        stage: stage.clone(),
        outputs: Arc::new(outputs),
    }))
}

/// Missing outputs are omitted; encoded empty outputs remain present in the map.
pub async fn fetch_stage_outputs<T: PipelineDerivable, Output, F>(
    ctx: &CoreContext,
    derivation: &DerivationContext,
    stage: &StageId,
    csids: Vec<ChangesetId>,
    mapping_key: impl Fn(ChangesetId) -> Option<String>,
    decode: impl Fn(ChangesetId, BlobstoreBytes) -> F,
) -> Result<HashMap<ChangesetId, Output>>
where
    F: std::future::Future<Output = Result<Output>>,
{
    stream::iter(csids)
        .map(|csid| {
            let key = mapping_key(csid);
            let decode = &decode;
            async move {
                let bytes = match key {
                    Some(key) => derivation.blobstore().get(ctx, &key).await?.map(Into::into),
                    None => derivation.stage_inputs.get(T::VARIANT, stage, csid),
                };
                match bytes {
                    Some(bytes) => Ok::<_, anyhow::Error>(Some((csid, decode(csid, bytes).await?))),
                    None => Ok(None),
                }
            }
        })
        .buffered(100)
        .try_filter_map(|entry| async move { Ok(entry) })
        .try_collect()
        .await
}

/// Resolve supplied stage outputs, falling back to completed roots or canonical data.
pub async fn fetch_stage_outputs_with_fallback<T: PipelineDerivable>(
    ctx: &CoreContext,
    derivation: &DerivationContext,
    stage: &StageId,
    csids: Vec<ChangesetId>,
) -> Result<HashMap<ChangesetId, T::StageOutput>> {
    let outputs = T::fetch_stage_outputs(ctx, derivation, stage, csids.clone()).await?;
    let missing: Vec<_> = csids
        .into_iter()
        .filter(|csid| !outputs.contains_key(csid))
        .collect();
    let root_stage = if T::HAS_FINALIZE {
        StageId::Finalize
    } else {
        StageId::Manifest(MPath::ROOT)
    };
    let roots = if &root_stage == stage {
        HashMap::new()
    } else {
        T::fetch_stage_outputs(ctx, derivation, &root_stage, missing.clone()).await?
    };
    stream::iter(missing)
        .map(async |csid| {
            let output = match roots.get(&csid) {
                Some(root) => {
                    T::extract_stage_output_from_root(ctx, derivation, root.clone(), stage).await?
                }
                None => {
                    let derived = derivation.fetch_dependency::<T>(ctx, csid).await?;
                    T::extract_stage_output_from_derived(ctx, derivation, &derived, stage).await?
                }
            };
            Ok::<_, anyhow::Error>((csid, output))
        })
        .buffered(100)
        .try_fold(outputs, |mut outputs, (csid, output)| {
            outputs.insert(csid, output);
            futures::future::ready(Ok(outputs))
        })
        .await
}

#[cfg(test)]
mod tests {
    use mononoke_macros::mononoke;

    use super::*;

    #[mononoke::test]
    fn rpc_outputs_preserve_identity_and_empty_values() -> Result<()> {
        let csid = ChangesetId::new(Blake2::from_byte_array([1; 32]));
        let batch = StageOutputs {
            derived_data_type: DerivableType::Fsnodes,
            stage: StageId::Manifest(MPath::new("dir")?),
            outputs: Arc::new(BTreeMap::from([(csid, Bytes::new())])),
        };
        assert_eq!(StageOutputs::from_thrift(batch.to_thrift())?, batch);
        let inputs = StageInputs::new(&[batch.clone(), batch.clone()])?;
        assert_eq!(
            inputs.get(batch.derived_data_type, &batch.stage, csid),
            Some(BlobstoreBytes::empty())
        );
        assert_eq!(inputs.get(DerivableType::Unodes, &batch.stage, csid), None);
        assert_eq!(
            inputs.get(
                batch.derived_data_type,
                &StageId::Manifest(MPath::ROOT),
                csid
            ),
            None
        );
        let conflict = StageOutputs {
            outputs: Arc::new(BTreeMap::from([(csid, Bytes::from_static(b"different"))])),
            ..batch.clone()
        };
        assert!(StageInputs::new(&[batch, conflict]).is_err());
        Ok(())
    }
}
