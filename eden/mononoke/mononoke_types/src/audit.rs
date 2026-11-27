/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Per-commit storage audit.
//!
//! Each audited object is a [`Validatable`] Mononoke type with an exact
//! [`ObjectId`].

use std::any::Any;
use std::fmt;
use std::hash::Hash;
use std::hash::Hasher;
use std::sync::Arc;

use anyhow::Error as AnyhowError;
use async_trait::async_trait;
use blobstore::LoadableError;
use thiserror::Error;

pub use self::load::LoadContext;
pub use self::load::LoadTag;
pub use self::load::LoadTags;
pub use self::load::TaggedContext;

mod load;

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
