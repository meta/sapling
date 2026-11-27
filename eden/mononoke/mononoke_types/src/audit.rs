/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Per-commit storage audit.
//!
//! A [`Walkable`] family enumerates the storage one commit added relative to
//! its parents. Each enumerated object is a [`Validatable`] Mononoke type,
//! wrapped in a [`StoredObject`] that carries its exact [`ObjectId`].

use std::any::Any;
use std::fmt;
use std::hash::Hash;
use std::hash::Hasher;
use std::num::NonZeroUsize;
use std::sync::Arc;

use anyhow::Error as AnyhowError;
use async_trait::async_trait;
use blobstore::LoadableError;
use futures::stream::BoxStream;
use thiserror::Error;

pub use self::load::LoadContext;
pub use self::load::LoadTag;
pub use self::load::LoadTags;
pub use self::load::TaggedContext;
use crate::ChangesetId;
use crate::DerivableType;

mod changeset;
mod deleted_manifest;
mod file_content;
mod load;
mod unode;

/// The commit being audited.
pub struct CommitInfo {
    pub changeset_id: ChangesetId,
    /// From the commit graph, so families can diff against the parents
    /// without loading the changeset.
    pub parents: Vec<ChangesetId>,
}

/// The audited commit's own mapping is absent.
///
/// Only `Mapping::load_own` may create one, so no other absent mapping can end
/// a family as not derived: any other absent mapping is
/// [`LoadError::Missing`].
#[derive(Debug)]
pub struct NotDerived(String);

impl NotDerived {
    /// For `Mapping::load_own` only; see [`NotDerived`].
    pub fn new(mapping: String) -> Self {
        Self(mapping)
    }
}

impl fmt::Display for NotDerived {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Debug, Error)]
pub enum LoadError {
    #[error("{0} is missing")]
    Missing(String),
    #[error("{0} is not derived")]
    NotDerived(NotDerived),
    #[error(transparent)]
    Unreadable(#[from] AnyhowError),
}

impl LoadError {
    pub fn class(&self) -> &'static str {
        match self {
            Self::Missing(_) => "missing",
            Self::NotDerived(_) => "not_derived",
            Self::Unreadable(_) => "unreadable",
        }
    }
}

impl From<LoadableError> for LoadError {
    fn from(error: LoadableError) -> Self {
        match error {
            LoadableError::Missing(key) => Self::Missing(key),
            LoadableError::Error(error) => Self::Unreadable(error),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ValidationOutcome {
    Passed {
        check: &'static str,
    },
    Failed {
        check: &'static str,
        message: String,
    },
    NotApplicable {
        check: &'static str,
    },
}

/// A stored object that can be loaded, and optionally checked, on its own.
///
/// The implementing value is the object's identity: two values that compare
/// equal are the same stored object.
#[async_trait]
pub trait Validatable<R>:
    Clone + fmt::Debug + fmt::Display + Eq + Hash + Send + Sync + 'static
{
    /// Storage type name used in diagnostics.
    const NAME: &'static str;

    type Loaded: Send + 'static;

    /// Reads the object from `ctx.blobstore()`, or through `repo` for data
    /// the blobstore alone cannot provide.
    async fn load(&self, ctx: &TaggedContext, repo: &R) -> Result<Self::Loaded, LoadError>;

    /// Checks run only when the run validates, after a successful load.
    fn check(&self, _loaded: &Self::Loaded) -> Vec<ValidationOutcome> {
        Vec::new()
    }
}

pub type WalkStream<R> = BoxStream<'static, Result<StoredObject<R>, WalkError>>;

/// A family that enumerates the storage one commit added relative to every parent.
///
/// A family that compares the commit's data with its parents' does not emit
/// an object equal to a parent's object at the same position, and never
/// emits parent objects it read only to compute the difference. A family
/// that does not compare with its parents may emit objects a parent also
/// holds. An object may be emitted more than once, such as one content blob
/// for two files; the runner validates it each time. The stream ends after
/// its first error.
///
/// Missing derived data: load the commit's own mapping first, with
/// `Mapping::load_own`. It alone reports an absent mapping as not derived,
/// which ends the family without failing it. Any other mapping load reports
/// an absent mapping as `LoadError::Missing`: once the commit's own data is
/// derived, the data it was derived from, such as its parents' mappings, must
/// exist. Types implementing `DerivableUntopologically` can have that gap
/// legitimately while a sliced backfill is in progress; those false alarms
/// are accepted, since backfills are rare.
pub trait Walkable<R> {
    /// The `--family` value.
    const NAME: &'static str;

    /// The derived data type of the family's own mapping, or `None` for a
    /// family that reads no derived data. As in Walker, a repository runs the
    /// family only if its derived data config enables this type.
    const DERIVED_TYPE: Option<DerivableType>;

    /// Enumerates what `commit` added. Each kind of read the family makes
    /// loads at most `max_loads` objects at once, so a family that loads its
    /// own objects for the entries another walk yields can have up to twice
    /// `max_loads` reads in flight while it enumerates. Every load, including
    /// a parent's, runs under a context from `ctx.tag`.
    fn walk_new(
        ctx: LoadContext,
        repo: Arc<R>,
        commit: Arc<CommitInfo>,
        max_loads: NonZeroUsize,
    ) -> WalkStream<R>;
}

/// One enumerated object: its identity plus the behaviour to load and check it.
pub struct StoredObject<R> {
    id: ObjectId,
    validatable: Box<dyn ErasedValidatable<R>>,
}

impl<R: Send + Sync + 'static> StoredObject<R> {
    pub fn new<T: Validatable<R>>(key: T) -> Self {
        Self::build(key, None)
    }

    /// Wraps a key whose value was already loaded while enumerating it, so
    /// validation does not read it again.
    pub fn preloaded<T: Validatable<R>>(key: T, loaded: T::Loaded) -> Self {
        Self::build(key, Some(loaded))
    }

    fn build<T: Validatable<R>>(key: T, loaded: Option<T::Loaded>) -> Self {
        Self {
            id: ObjectId::of::<R, T>(&key),
            validatable: Box::new(Typed { key, loaded }),
        }
    }

    pub fn id(&self) -> &ObjectId {
        &self.id
    }

    /// Loads the object unless it was preloaded, then runs its checks if
    /// `run_checks`.
    pub async fn validate(
        self,
        ctx: &LoadContext,
        repo: &R,
        run_checks: bool,
    ) -> Result<Vec<ValidationOutcome>, LoadError> {
        self.validatable
            .validate(ctx, self.id, repo, run_checks)
            .await
    }
}

impl<R> fmt::Debug for StoredObject<R> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("StoredObject")
            .field(&self.id)
            .finish()
    }
}

/// The object-safe form of `Validatable`, so one stream can carry keys of
/// different types.
#[async_trait]
trait ErasedValidatable<R>: Send {
    async fn validate(
        self: Box<Self>,
        ctx: &LoadContext,
        id: ObjectId,
        repo: &R,
        run_checks: bool,
    ) -> Result<Vec<ValidationOutcome>, LoadError>;
}

struct Typed<T, Loaded> {
    key: T,
    loaded: Option<Loaded>,
}

#[async_trait]
impl<R, T> ErasedValidatable<R> for Typed<T, T::Loaded>
where
    R: Send + Sync + 'static,
    T: Validatable<R>,
{
    async fn validate(
        self: Box<Self>,
        ctx: &LoadContext,
        id: ObjectId,
        repo: &R,
        run_checks: bool,
    ) -> Result<Vec<ValidationOutcome>, LoadError> {
        let Typed { key, loaded } = *self;
        let loaded = match loaded {
            Some(loaded) => loaded,
            None => key.load(&ctx.tag(id), repo).await?,
        };
        Ok(if run_checks {
            key.check(&loaded)
        } else {
            Vec::new()
        })
    }
}

/// Exact, type-erased identity of a stored object.
///
/// Equality compares the concrete Rust type and the typed value, so objects of
/// different types are never equal even when their hashes share bytes.
#[derive(Clone)]
pub struct ObjectId {
    name: &'static str,
    key: Arc<dyn ErasedKey>,
}

impl ObjectId {
    pub fn of<R, T: Validatable<R>>(object: &T) -> Self {
        Self {
            name: T::NAME,
            key: Arc::new(object.clone()),
        }
    }

    pub fn name(&self) -> &'static str {
        self.name
    }
}

impl PartialEq for ObjectId {
    fn eq(&self, other: &Self) -> bool {
        self.key.eq_key(other.key.as_ref())
    }
}

impl Eq for ObjectId {}

impl Hash for ObjectId {
    fn hash<H: Hasher>(&self, state: &mut H) {
        Any::type_id(self.key.as_any()).hash(state);
        self.key.hash_key(state);
    }
}

impl fmt::Display for ObjectId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}:{}", self.name, self.key)
    }
}

impl fmt::Debug for ObjectId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}

trait ErasedKey: fmt::Display + Send + Sync {
    fn as_any(&self) -> &dyn Any;
    fn eq_key(&self, other: &dyn ErasedKey) -> bool;
    fn hash_key(&self, state: &mut dyn Hasher);
}

impl<T: Eq + Hash + fmt::Display + Send + Sync + 'static> ErasedKey for T {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn eq_key(&self, other: &dyn ErasedKey) -> bool {
        other
            .as_any()
            .downcast_ref::<T>()
            .is_some_and(|other| self == other)
    }

    fn hash_key(&self, mut state: &mut dyn Hasher) {
        self.hash(&mut state);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WalkStage {
    /// Reading data needed to decide which objects the commit added.
    Enumerate,
    /// Loading an object the commit added.
    Load,
}

impl WalkStage {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Enumerate => "enumerate",
            Self::Load => "load",
        }
    }
}

/// Why one family's walk of one commit ended early. Its message names the
/// class, stage and object; the load error is its source.
#[derive(Debug, Error)]
pub enum WalkError {
    /// An object the commit added could not be loaded.
    #[error("{} at load for {object}", .source.class())]
    Load {
        object: ObjectId,
        #[source]
        source: LoadError,
    },
    /// Data needed to decide which objects the commit added could not be
    /// read, for `object` if the failure is one object's.
    #[error("{} at enumerate{}", .source.class(), for_object(.object.as_ref()))]
    Enumerate {
        object: Option<ObjectId>,
        #[source]
        source: LoadError,
    },
}

fn for_object(object: Option<&ObjectId>) -> String {
    object.map_or_else(String::new, |object| format!(" for {object}"))
}

impl WalkError {
    pub fn load(object: ObjectId, source: LoadError) -> Self {
        Self::Load { object, source }
    }

    pub fn enumerate(object: Option<ObjectId>, source: LoadError) -> Self {
        Self::Enumerate { object, source }
    }

    pub fn stage(&self) -> WalkStage {
        match self {
            Self::Load { .. } => WalkStage::Load,
            Self::Enumerate { .. } => WalkStage::Enumerate,
        }
    }

    pub fn object(&self) -> Option<&ObjectId> {
        match self {
            Self::Load { object, .. } => Some(object),
            Self::Enumerate { object, .. } => object.as_ref(),
        }
    }

    pub fn load_error(&self) -> &LoadError {
        match self {
            Self::Load { source, .. } | Self::Enumerate { source, .. } => source,
        }
    }

    pub fn into_parts(self) -> (WalkStage, Option<ObjectId>, LoadError) {
        match self {
            Self::Load { object, source } => (WalkStage::Load, Some(object), source),
            Self::Enumerate { object, source } => (WalkStage::Enumerate, object, source),
        }
    }

    pub fn class(&self) -> &'static str {
        self.load_error().class()
    }
}

#[cfg(test)]
mod tests {
    use mononoke_macros::mononoke;

    use super::*;

    #[derive(Clone, Debug, Eq, Hash, PartialEq)]
    struct Blob(&'static str);

    impl fmt::Display for Blob {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(self.0)
        }
    }

    #[async_trait]
    impl Validatable<()> for Blob {
        const NAME: &'static str = "Blob";

        type Loaded = ();

        async fn load(&self, _ctx: &TaggedContext, _repo: &()) -> Result<(), LoadError> {
            Ok(())
        }
    }

    #[mononoke::test]
    fn a_walk_error_names_its_cause_once() {
        let load = WalkError::load(
            ObjectId::of::<(), _>(&Blob("b")),
            LoadError::Missing("key".to_string()),
        );
        let enumerate = WalkError::enumerate(None, LoadError::Missing("key".to_string()));

        assert_eq!(
            format!("{:#}", AnyhowError::from(load)),
            "missing at load for Blob:b: key is missing"
        );
        assert_eq!(
            format!("{:#}", AnyhowError::from(enumerate)),
            "missing at enumerate: key is missing"
        );
    }
}
