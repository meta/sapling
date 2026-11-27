/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::fmt;
use std::hash::Hash;
use std::hash::Hasher;
use std::marker::PhantomData;

use anyhow::anyhow;
use async_trait::async_trait;
use blobstore::LoadableError;
use derived_data_manager::BonsaiDerivable;
use derived_data_manager::DerivationContext;
use derived_data_manager::DerivationError;
use mononoke_types::ChangesetId;
use mononoke_types::audit::CommitInfo;
use mononoke_types::audit::LoadContext;
use mononoke_types::audit::LoadError;
use mononoke_types::audit::NotDerived;
use mononoke_types::audit::ObjectId;
use mononoke_types::audit::TaggedContext;
use mononoke_types::audit::Validatable;
use mononoke_types::audit::WalkError;

use crate::RepoDerivedDataRef;

/// Where a derived data type keeps its mapping records, so that a mapping can
/// name the key it is stored under. Each type implements it with its own key
/// format.
pub trait MappingKey: BonsaiDerivable {
    /// The key the mapping for `changeset_id` is stored under in the
    /// blobstore, without the repository's prefix, or `None` for a type whose
    /// mapping is an SQL row.
    fn mapping_key(derivation_ctx: &DerivationContext, changeset_id: ChangesetId)
    -> Option<String>;
}

/// The derived-data mapping record for `D` at one changeset.
pub struct Mapping<D> {
    changeset_id: ChangesetId,
    derivable: PhantomData<fn() -> D>,
}

impl<D> Mapping<D> {
    pub fn new(changeset_id: ChangesetId) -> Self {
        Self {
            changeset_id,
            derivable: PhantomData,
        }
    }
}

impl<D: MappingKey> Mapping<D> {
    /// Loads a family's own mapping for the audited commit, as
    /// [`Validatable::load`] does, except that an absent mapping is not
    /// derived rather than missing. Only this reports an absent mapping as not
    /// derived; derivation is asynchronous, and on demand for drafts.
    pub async fn load_own<R>(
        ctx: &LoadContext,
        repo: &R,
        commit: &CommitInfo,
    ) -> Result<D, WalkError>
    where
        R: RepoDerivedDataRef + Send + Sync + 'static,
    {
        let mapping = Self::new(commit.changeset_id);
        let object = ObjectId::of::<R, _>(&mapping);
        match mapping.fetch(&ctx.tag(object.clone()), repo).await {
            Ok(Some(loaded)) => Ok(loaded),
            Ok(None) => Err(WalkError::load(
                object,
                LoadError::NotDerived(NotDerived::new(mapping.to_string())),
            )),
            Err(error) => Err(WalkError::load(object, error)),
        }
    }

    /// Loads the mapping of each of the audited commit's parents, in the
    /// commit's parent order, one at a time. The commit's own data was
    /// derived from them, so an absent one is missing. Failures are at the
    /// enumerate stage.
    pub async fn load_parents<R>(
        ctx: &LoadContext,
        repo: &R,
        commit: &CommitInfo,
    ) -> Result<Vec<D>, WalkError>
    where
        R: RepoDerivedDataRef + Send + Sync + 'static,
    {
        let mut parents = Vec::with_capacity(commit.parents.len());
        for &parent in &commit.parents {
            let mapping = Self::new(parent);
            let object = ObjectId::of::<R, _>(&mapping);
            let loaded = mapping
                .load(&ctx.tag(object.clone()), repo)
                .await
                .map_err(|error| WalkError::enumerate(Some(object), error))?;
            parents.push(loaded);
        }
        Ok(parents)
    }

    /// Fetches the mapping, or `None` if it is absent.
    async fn fetch<R>(&self, ctx: &TaggedContext, repo: &R) -> Result<Option<D>, LoadError>
    where
        R: RepoDerivedDataRef + Send + Sync + 'static,
    {
        repo.repo_derived_data()
            .fetch_derived::<D>(ctx, self.changeset_id)
            .await
            .map_err(fetch_error)
    }
}

impl<D> Clone for Mapping<D> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<D> Copy for Mapping<D> {}

impl<D> PartialEq for Mapping<D> {
    fn eq(&self, other: &Self) -> bool {
        self.changeset_id == other.changeset_id
    }
}

impl<D> Eq for Mapping<D> {}

impl<D> Hash for Mapping<D> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.changeset_id.hash(state);
    }
}

impl<D: BonsaiDerivable> fmt::Display for Mapping<D> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}:{}", D::NAME, self.changeset_id)
    }
}

impl<D: BonsaiDerivable> fmt::Debug for Mapping<D> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

#[async_trait]
impl<R, D> Validatable<R> for Mapping<D>
where
    R: RepoDerivedDataRef + Send + Sync + 'static,
    D: MappingKey,
{
    const NAME: &'static str = "Mapping";
    const SUBTYPE: Option<&'static str> = Some(D::NAME);

    type Loaded = D;

    // Once the audited commit's own mapping is present, every mapping its
    // derivation read must exist too; see `load_own` for the own mapping.
    async fn load(&self, ctx: &TaggedContext, repo: &R) -> Result<D, LoadError> {
        self.fetch(ctx, repo)
            .await?
            .ok_or_else(|| LoadError::Missing(self.to_string()))
    }

    // The key `fetch_derived` reads, from the configuration of the
    // repository's derived data manager.
    fn storage_key(&self, repo: &R) -> Option<String> {
        let derivation_ctx = repo.repo_derived_data().manager().derivation_context(None);
        D::mapping_key(&derivation_ctx, self.changeset_id)
    }
}

/// A failed fetch. Some fetches read blobs other than the mapping, such as
/// the filenodes mapping, which reads the commit's Hg changeset: one of those
/// being absent is missing, as when a family loads it itself.
fn fetch_error(error: DerivationError) -> LoadError {
    let absent = match &error {
        DerivationError::Error(error) => {
            error
                .chain()
                .find_map(|cause| match cause.downcast_ref::<LoadableError>() {
                    Some(LoadableError::Missing(key)) => Some(key.clone()),
                    _ => None,
                })
        }
        _ => None,
    };
    match absent {
        Some(key) => LoadError::Missing(key),
        None => LoadError::Unreadable(anyhow!(error)),
    }
}

#[cfg(test)]
mod tests {
    use anyhow::anyhow;
    use blobstore::LoadableError;
    use derived_data_manager::DerivationError;
    use mononoke_macros::mononoke;
    use mononoke_types::audit::LoadError;

    use super::fetch_error;

    fn fetch_failed(error: anyhow::Error) -> LoadError {
        fetch_error(DerivationError::from(error.context("While fetching")))
    }

    #[mononoke::test]
    fn a_blob_absent_inside_the_fetch_is_missing() {
        let error = fetch_failed(LoadableError::Missing("hgchangeset.sha1.abc".to_string()).into());

        assert!(
            matches!(&error, LoadError::Missing(key) if key == "hgchangeset.sha1.abc"),
            "{error:?}"
        );
    }

    #[mononoke::test]
    fn any_other_failure_inside_the_fetch_is_unreadable() {
        for error in [
            anyhow!("bytes that do not decode"),
            LoadableError::Error(anyhow!("a failed read")).into(),
        ] {
            let error = fetch_failed(error);

            assert!(matches!(error, LoadError::Unreadable(_)), "{error:?}");
        }
    }
}
