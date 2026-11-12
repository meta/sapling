/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::cmp::Ordering;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fmt;
use std::fmt::Display;
use std::io;
use std::io::Write;
use std::iter::Once;
use std::iter::once;
use std::os::unix::ffi::OsStrExt;
use std::slice::Iter;
use std::str::FromStr;

use anyhow::Context as _;
use anyhow::Error;
use anyhow::Result;
use anyhow::bail;
use ascii::AsciiString;
use itertools::Itertools;
use quickcheck::Arbitrary;
use quickcheck::Gen;
use quickcheck_arbitrary_derive::Arbitrary;
use regex::Regex;
use serde_derive::Deserialize;
use serde_derive::Serialize;

use self::mpath_element::MPathElement;
use crate::ThriftConvert;
use crate::bonsai_changeset::BonsaiChangeset;
use crate::errors::MononokeTypeError;
use crate::hash::Blake2;
use crate::hash::Context;
use crate::thrift;

pub mod mpath_element;

/// A path or filename within Mononoke, with information about whether
/// it's the root of the repo, a directory or a file.
#[derive(Arbitrary, Clone, Debug, PartialEq, Eq, Hash)]
#[derive(Serialize, Deserialize)]
#[derive(bincode::Encode, bincode::Decode)]
pub enum RepoPath {
    // It is now *completely OK* to create a RepoPath directly. All MPaths are valid once
    // constructed.
    RootPath,
    DirectoryPath(NonRootMPath),
    FilePath(NonRootMPath),
}

impl RepoPath {
    #[inline]
    pub fn root() -> Self {
        RepoPath::RootPath
    }

    pub fn dir<P>(path: P) -> Result<Self>
    where
        P: TryInto<NonRootMPath>,
        Error: From<P::Error>,
    {
        let path = path.try_into()?;
        Ok(RepoPath::DirectoryPath(path))
    }

    pub fn file<P>(path: P) -> Result<Self>
    where
        P: TryInto<NonRootMPath>,
        Error: From<P::Error>,
    {
        let path = path.try_into()?;
        Ok(RepoPath::FilePath(path))
    }

    /// Whether this path represents the root.
    #[inline]
    pub fn is_root(&self) -> bool {
        match *self {
            RepoPath::RootPath => true,
            _ => false,
        }
    }

    /// Whether this path represents a directory that isn't the root.
    #[inline]
    pub fn is_dir(&self) -> bool {
        match *self {
            RepoPath::DirectoryPath(_) => true,
            _ => false,
        }
    }

    /// Whether this patch represents a tree (root or other directory).
    #[inline]
    pub fn is_tree(&self) -> bool {
        match *self {
            RepoPath::RootPath => true,
            RepoPath::DirectoryPath(_) => true,
            _ => false,
        }
    }

    /// Whether this path represents a file.
    #[inline]
    pub fn is_file(&self) -> bool {
        match *self {
            RepoPath::FilePath(_) => true,
            _ => false,
        }
    }

    #[allow(clippy::len_without_is_empty)]
    /// Get the length of this repo path in bytes. `RepoPath::Root` has length 0.
    pub fn len(&self) -> usize {
        match *self {
            RepoPath::RootPath => 0,
            RepoPath::DirectoryPath(ref path) => path.len(),
            RepoPath::FilePath(ref path) => path.len(),
        }
    }

    pub fn mpath(&self) -> Option<&NonRootMPath> {
        match *self {
            RepoPath::RootPath => None,
            RepoPath::DirectoryPath(ref path) => Some(path),
            RepoPath::FilePath(ref path) => Some(path),
        }
    }

    pub fn into_mpath(self) -> Option<NonRootMPath> {
        match self {
            RepoPath::RootPath => None,
            RepoPath::DirectoryPath(path) => Some(path),
            RepoPath::FilePath(path) => Some(path),
        }
    }

    pub fn from_thrift(path: thrift::path::RepoPath) -> Result<Self> {
        let path = match path {
            thrift::path::RepoPath::RootPath(_) => Self::root(),
            thrift::path::RepoPath::DirectoryPath(path) => {
                Self::dir(NonRootMPath::from_thrift(path)?)?
            }
            thrift::path::RepoPath::FilePath(path) => Self::file(NonRootMPath::from_thrift(path)?)?,
            thrift::path::RepoPath::UnknownField(unknown) => {
                bail!("Unknown field encountered when parsing thrift::path::RepoPath: {unknown}",)
            }
        };
        Ok(path)
    }

    pub fn into_thrift(self) -> thrift::path::RepoPath {
        match self {
            // dummy false here is required because thrift doesn't support mixing enums with and
            // without payload
            RepoPath::RootPath => thrift::path::RepoPath::RootPath(false),
            RepoPath::DirectoryPath(path) => {
                thrift::path::RepoPath::DirectoryPath(NonRootMPath::into_thrift(path))
            }
            RepoPath::FilePath(path) => {
                thrift::path::RepoPath::FilePath(NonRootMPath::into_thrift(path))
            }
        }
    }
}

impl Display for RepoPath {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            RepoPath::RootPath => write!(f, "(root path)"),
            RepoPath::DirectoryPath(ref path) => write!(f, "directory '{path}'"),
            RepoPath::FilePath(ref path) => write!(f, "file '{path}'"),
        }
    }
}

/// This trait impl allows passing in a &RepoPath where `Into<RepoPath>` is requested.
impl<'a> From<&'a RepoPath> for RepoPath {
    fn from(path: &'a RepoPath) -> RepoPath {
        path.clone()
    }
}

impl From<MPathElement> for NonRootMPath {
    fn from(element: MPathElement) -> Self {
        NonRootMPath(MPath {
            elements: vec![element],
        })
    }
}

/// A path or filename within Mononoke (typically within manifests or changegroups).
/// The path can be root path (i.e. just /) or non-root (i.e. /home/here).
///
/// This is called `MPath` so that it can be differentiated from `std::path::Path`.
#[derive(Clone, Ord, PartialOrd, Eq, PartialEq, Hash)]
#[derive(Serialize, Deserialize)]
#[derive(bincode::Encode, bincode::Decode)]
pub struct MPath {
    elements: Vec<MPathElement>,
}

impl MPath {
    pub const ROOT: MPath = MPath {
        elements: Vec::new(),
    };

    /// Constructs a new MPath instance from path bytes subject to correctness checks
    pub fn new<P: AsRef<[u8]>>(p: P) -> Result<Self> {
        let p = p.as_ref();
        let elements: Vec<_> = p
            .split(|c| *c == b'/')
            .filter(|e| !e.is_empty())
            .map(MPathElement::new_from_slice)
            .collect::<Result<_, _>>()?;
        Ok(Self { elements })
    }

    /// Returns whether the path is root
    pub fn is_root(&self) -> bool {
        self.elements.is_empty()
    }

    pub fn join<'a, Elements: IntoIterator<Item = &'a MPathElement>>(
        &self,
        another: Elements,
    ) -> Self {
        let mut newelements = self.elements.clone();
        newelements.extend(
            another
                .into_iter()
                .filter(|elem| !elem.0.is_empty())
                .cloned(),
        );
        Self {
            elements: newelements,
        }
    }

    /// Converts MPath into NonRootMath by adding one element to it
    pub fn join_into_non_root_mpath(&self, another: &MPathElement) -> NonRootMPath {
        NonRootMPath(self.join(another))
    }

    pub fn join_element(&self, element: Option<&MPathElement>) -> Self {
        match element {
            Some(element) => self.join(element),
            None => self.clone(),
        }
    }

    /// The final component of this path, if its exists
    pub fn basename(&self) -> Option<&MPathElement> {
        self.elements.last()
    }

    /// The number of components in this path.
    pub fn num_components(&self) -> usize {
        self.elements.len()
    }

    /// The number of leading components that are common.
    pub fn common_components<'a, E: IntoIterator<Item = &'a MPathElement>>(
        &self,
        other: E,
    ) -> usize {
        self.elements
            .iter()
            .zip(other)
            .take_while(|&(e1, e2)| e1 == e2)
            .count()
    }

    /// Whether this path is related to the given path. A path P1 is related to path P2 iff
    /// P1 is prefix of P2 or vice versa.
    pub fn is_related_to(&self, other: &MPath) -> bool {
        self.is_prefix_of(other.as_ref()) || other.is_prefix_of(self.as_ref())
    }

    /// Whether this path is a path prefix of the given path.
    /// `foo` is a prefix of `foo/bar`, but not of `foo1`.
    #[inline]
    pub fn is_prefix_of<'a, E: IntoIterator<Item = &'a MPathElement>>(&self, other: E) -> bool {
        let common_components = self.common_components(other);
        let total_components = self.num_components();
        // If all the components of this path are present in the other path, then this path is
        // considered as a prefix of other path.
        common_components == total_components
    }

    /// Create a new path with the number of leading components specified.
    pub fn take_prefix_components(&self, components: usize) -> Result<Self> {
        match components {
            0 => Ok(Self::ROOT),
            x if x > self.num_components() => bail!(
                "taking {} components but path only has {}",
                components,
                self.num_components()
            ),
            _ => Ok(Self {
                elements: self.elements[..components].to_vec(),
            }),
        }
    }

    /// Create a new path, removing `prefix`. If the `prefix` is not a strict prefix,
    /// it returns an empty MPath
    pub fn remove_prefix_component<'a, E: IntoIterator<Item = &'a MPathElement>>(
        &self,
        prefix: E,
    ) -> Self {
        let mut self_iter = self.elements.iter();
        for elem in prefix {
            // The prefix is not a strict prefix, so return empty MPath
            if Some(elem) != self_iter.next() {
                return Self::ROOT;
            }
        }
        let elements: Vec<_> = self_iter.cloned().collect();
        Self { elements }
    }

    pub fn generate<W: Write>(&self, out: &mut W) -> io::Result<()> {
        out.write_all(&self.to_vec())
    }

    pub fn to_vec(&self) -> Vec<u8> {
        let ret: Vec<_> = self.elements.iter().map(|e| e.0.as_ref()).collect();
        ret.join(&b'/')
    }

    pub fn iter_bytes(&self) -> impl Iterator<Item = u8> + '_ {
        Itertools::intersperse(self.elements.iter().map(|e| e.as_ref()), b"/".as_ref())
            .flatten()
            .copied()
    }

    pub fn compare_bytes(&self, other: &MPath) -> std::cmp::Ordering {
        let self_bytes = self.iter_bytes();
        let other_bytes = other.iter_bytes();
        self_bytes.cmp(other_bytes)
    }

    #[allow(clippy::len_without_is_empty)]
    /// The length of this path, including any slashes in it.
    pub fn len(&self) -> usize {
        // n elements means n-1 slashes
        let slashes = self.elements.len().saturating_sub(1);
        let elem_len: usize = self.elements.iter().map(|elem| elem.len()).sum();
        slashes + elem_len
    }

    /// Create an MPath out of an iterator of elements. If the iterator is empty, we create en
    /// empty (root) MPath
    pub fn from_elements<'a, I>(elements: I) -> Self
    where
        I: Iterator<Item = &'a MPathElement>,
    {
        Self {
            elements: elements.cloned().collect(),
        }
    }

    /// Split a MPath into dirname and file name, if possible. If a split is not possible,
    /// None is returned
    pub fn split_dirname(&self) -> Option<(Self, &MPathElement)> {
        self.elements
            .split_last()
            .map(|(last, rest)| (Self::from_elements(rest.iter()), last))
    }

    /// Split a MPath into first path component and the rest, if possible. If a split is not possible,
    /// None is returned
    pub fn split_first(&self) -> Option<(&MPathElement, Self)> {
        self.elements
            .split_first()
            .map(|(first, rest)| (first, Self::from_elements(rest.iter())))
    }

    pub fn iter(&self) -> Iter<'_, MPathElement> {
        self.elements.iter()
    }

    pub fn get_path_hash(&self) -> MPathHash {
        let mut context = MPathHashContext::new();
        let num_el = self.elements.len();
        if num_el > 0 {
            if num_el > 1 {
                for e in &self.elements[..num_el - 1] {
                    context.update(e.as_ref());
                    context.update(*b"/")
                }
            }
            context.update(self.elements[num_el - 1].as_ref());
        } else {
            context.update([])
        }
        context.finish()
    }

    /// Returns an iterator of the path and each of its ancestor directories, up
    /// to and including the root.
    pub fn into_ancestors(self) -> MPathAncestors {
        MPathAncestors { next: Some(self) }
    }

    pub fn into_optional_non_root_path(self) -> Option<NonRootMPath> {
        if self.is_root() {
            None
        } else {
            Some(NonRootMPath(self))
        }
    }

    pub fn display_opt<'a>(path_opt: &'a Self) -> DisplayOpt<'a> {
        DisplayOpt(path_opt)
    }

    /// Convert this path to a null separated bytes sequence. If the path is non-empty, the null-byte
    /// is used as a separator to combine the elements of the path (i.e. a path with 3 elements will have 2 null bytes).
    /// However, if the path is empty (root) then simply a null-byte is returned. This helps in providing a storable representation
    /// of empty paths which is not offered by an empty Vec
    pub fn to_null_separated_bytes(&self) -> Vec<u8> {
        // The root path will be represented as a null byte
        if self.is_root() {
            vec![b'\0']
        } else {
            let ret: Vec<_> = self.elements.iter().map(|e| e.0.as_ref()).collect();
            ret.join(&b'\0')
        }
    }

    /// Create an MPath out of sequence of MPathElement bytes separated by the null byte. If the
    /// byte sequence just contains the null byte, then the empty (root) path is returned
    pub fn from_null_separated_bytes(path: Vec<u8>) -> Result<Self> {
        if path == vec![b'\0'] {
            return Ok(Self::ROOT);
        }
        let segments = path
            .split(|elem| elem == &b'\0')
            .map(MPathElement::new_from_slice)
            .collect::<Result<Vec<_>>>()
            .with_context(|| format!("Error in creating Vec<MPathElement> from {path:?}"))?;
        Ok(MPath::from_elements(segments.iter()))
    }

    /// Returns the depth of this path. The root has a depth 0, and any non-root path has depth
    /// equal to the number of elements in it.
    pub fn depth(&self) -> u64 {
        self.elements.len() as u64
    }

    pub fn reparent(&self, old_prefix: &MPath, new_prefix: &MPath) -> Result<Self> {
        if !old_prefix.is_prefix_of(self) {
            bail!("Cannot reparent path {self:?} with old prefix {old_prefix:?}");
        }
        let mut new_path = new_prefix.clone();
        new_path
            .elements
            .extend(self.elements[old_prefix.num_components()..].iter().cloned());
        Ok(new_path)
    }
}

impl From<Option<NonRootMPath>> for MPath {
    fn from(value: Option<NonRootMPath>) -> Self {
        match value {
            Some(v) => v.0,
            None => Self::ROOT,
        }
    }
}

impl From<MPath> for Option<NonRootMPath> {
    fn from(value: MPath) -> Self {
        if value.is_root() {
            None
        } else {
            Some(NonRootMPath(value))
        }
    }
}

impl<'a> From<Option<&'a NonRootMPath>> for &'a MPath {
    fn from(value: Option<&'a NonRootMPath>) -> Self {
        static EMPTY: MPath = MPath::ROOT;
        match value {
            Some(v) => &v.0,
            None => &EMPTY,
        }
    }
}

impl From<NonRootMPath> for MPath {
    fn from(value: NonRootMPath) -> Self {
        value.0
    }
}

impl<'a> From<&'a NonRootMPath> for &'a MPath {
    fn from(value: &'a NonRootMPath) -> Self {
        &value.0
    }
}

impl TryFrom<MPath> for NonRootMPath {
    type Error = anyhow::Error;
    fn try_from(path: MPath) -> Result<Self> {
        if path.is_root() {
            bail!(MononokeTypeError::InvalidPath(
                path.to_string(),
                "path cannot be empty".into()
            ));
        } else {
            Ok(NonRootMPath(path))
        }
    }
}

impl AsRef<[MPathElement]> for MPath {
    fn as_ref(&self) -> &[MPathElement] {
        &self.elements
    }
}

impl IntoIterator for MPath {
    type Item = MPathElement;
    type IntoIter = ::std::vec::IntoIter<Self::Item>;

    fn into_iter(self) -> Self::IntoIter {
        self.elements.into_iter()
    }
}

impl<'a> IntoIterator for &'a MPath {
    type Item = &'a MPathElement;
    type IntoIter = Iter<'a, MPathElement>;

    fn into_iter(self) -> Self::IntoIter {
        self.elements.iter()
    }
}

impl<'a> From<&'a MPath> for Vec<u8> {
    fn from(path: &MPath) -> Self {
        path.to_vec()
    }
}

impl<'a> TryFrom<&'a [u8]> for MPath {
    type Error = Error;

    fn try_from(value: &[u8]) -> Result<Self> {
        MPath::new(value)
    }
}

impl<'a> TryFrom<&'a str> for MPath {
    type Error = Error;

    fn try_from(value: &str) -> Result<Self> {
        MPath::new(value.as_bytes())
    }
}

impl<'a> TryFrom<&'a String> for MPath {
    type Error = Error;

    fn try_from(value: &String) -> Result<Self> {
        MPath::new(value.as_bytes())
    }
}

impl<'a> TryFrom<&'a OsStr> for MPath {
    type Error = Error;

    fn try_from(value: &OsStr) -> Result<Self> {
        MPath::new(value.as_bytes())
    }
}

impl From<Vec<MPathElement>> for MPath {
    fn from(elements: Vec<MPathElement>) -> Self {
        Self { elements }
    }
}

impl Arbitrary for MPath {
    #[inline]
    fn arbitrary(g: &mut Gen) -> Self {
        let size = g.size();
        let mut path = Vec::new();

        for i in 0..size {
            if i > 0 {
                path.push(b'/');
            }
            let element = MPathElement::arbitrary(g);
            path.extend(&element.0);
        }
        MPath::new(path).unwrap()
    }
}

/// A path or filename within Mononoke (typically within manifests or changegroups).
/// The path can ONLY be non-root path (i.e. just / is not allowed)
#[derive(Clone, Ord, PartialOrd, Eq, PartialEq, Hash)]
#[derive(Serialize, Deserialize)]
#[derive(bincode::Encode, bincode::Decode)]
pub struct NonRootMPath(MPath);

impl Extend<MPathElement> for MPath {
    fn extend<T: IntoIterator<Item = MPathElement>>(&mut self, iter: T) {
        self.elements.extend(iter);
    }
}

impl Extend<MPathElement> for Option<NonRootMPath> {
    fn extend<T: IntoIterator<Item = MPathElement>>(&mut self, iter: T) {
        match self {
            Some(path) => path.0.extend(iter),
            None => {
                let elements = Vec::from_iter(iter);
                if elements.is_empty() {
                    *self = None;
                } else {
                    *self = Some(NonRootMPath::from_elements(elements.iter()));
                }
            }
        }
    }
}

impl NonRootMPath {
    pub fn new<P: AsRef<[u8]>>(p: P) -> Result<Self> {
        let p = p.as_ref();
        let mpath = MPath::new(p)?;
        if mpath.is_root() {
            bail!(MononokeTypeError::InvalidPath(
                String::from_utf8_lossy(p).into_owned(),
                "path cannot be empty".into()
            ));
        }
        Ok(Self(mpath))
    }

    /// Same as `NonRootMPath::new`, except the input bytes may be empty.
    pub fn new_opt<P: AsRef<[u8]>>(p: P) -> Result<Option<NonRootMPath>> {
        let p = p.as_ref();
        if p.is_empty() {
            Ok(None)
        } else {
            Ok(Some(NonRootMPath::new(p)?))
        }
    }

    pub fn from_thrift(non_root_mpath: thrift::path::NonRootMPath) -> Result<NonRootMPath> {
        let mpath = MPath::from_thrift(non_root_mpath.0)?;
        if mpath.is_root() {
            bail!("Unexpected empty path in thrift::path::NonRootMPath")
        } else {
            Ok(NonRootMPath(mpath))
        }
    }

    pub fn join<'a, Elements: IntoIterator<Item = &'a MPathElement>>(
        &self,
        another: Elements,
    ) -> Self {
        NonRootMPath(self.0.join(another))
    }

    pub fn join_element(&self, element: Option<&MPathElement>) -> Self {
        match element {
            Some(element) => self.join(element),
            None => self.clone(),
        }
    }

    pub fn join_opt<'a, Elements: IntoIterator<Item = &'a MPathElement>>(
        path: Option<&Self>,
        another: Elements,
    ) -> Option<Self> {
        match path {
            Some(path) => Some(path.join(another)),
            None => {
                let elements: Vec<MPathElement> = another
                    .into_iter()
                    .filter(|elem| !elem.0.is_empty())
                    .cloned()
                    .collect();
                if elements.is_empty() {
                    None
                } else {
                    Some(NonRootMPath::from_elements(elements.iter()))
                }
            }
        }
    }

    pub fn is_prefix_of_opt<'a, E: IntoIterator<Item = &'a MPathElement>>(
        prefix: Option<&NonRootMPath>,
        other: E,
    ) -> bool {
        match prefix {
            Some(prefix) => prefix.is_prefix_of(other),
            None => true,
        }
    }

    pub fn join_opt_element(path: Option<&Self>, element: &MPathElement) -> Self {
        match path {
            Some(path) => path.join_element(Some(element)),
            None => NonRootMPath::from_elements(Some(element).into_iter()),
        }
    }

    pub fn join_element_opt(path: Option<&Self>, element: Option<&MPathElement>) -> Option<Self> {
        match element {
            Some(element) => Self::join_opt(path, element),
            None => path.cloned(),
        }
    }

    pub fn iter_opt(path: Option<&Self>) -> Iter<'_, MPathElement> {
        match path {
            Some(path) => path.into_iter(),
            None => [].iter(),
        }
    }

    pub fn iter(&self) -> Iter<'_, MPathElement> {
        self.0.iter()
    }

    pub fn into_iter_opt(path: Option<Self>) -> ::std::vec::IntoIter<MPathElement> {
        match path {
            Some(path) => path.into_iter(),
            None => (vec![]).into_iter(),
        }
    }

    /// The number of components in this path.
    pub fn num_components(&self) -> usize {
        self.0.num_components()
    }

    /// The number of leading components that are common.
    pub fn common_components<'a, E: IntoIterator<Item = &'a MPathElement>>(
        &self,
        other: E,
    ) -> usize {
        self.0.common_components(other)
    }

    /// Whether this path is related to the given path. A path P1 is related to path P2 iff
    /// P1 is prefix of P2 or vice versa.
    pub fn is_related_to(&self, other: &NonRootMPath) -> bool {
        self.0.is_related_to(&other.0)
    }

    /// Whether this path is a path prefix of the given path.
    /// `foo` is a prefix of `foo/bar`, but not of `foo1`.
    #[inline]
    pub fn is_prefix_of<'a, E: IntoIterator<Item = &'a MPathElement>>(&self, other: E) -> bool {
        self.0.is_prefix_of(other)
    }

    /// The final component of this path.
    pub fn basename(&self) -> &MPathElement {
        self.0
            .basename()
            .expect("MPaths have at least one component")
    }

    /// Create a new path with the number of leading components specified.
    pub fn take_prefix_components(&self, components: usize) -> Result<Option<NonRootMPath>> {
        self.0
            .take_prefix_components(components)
            .map(MPath::into_optional_non_root_path)
    }

    /// Create a new path, removing `prefix`. Returns `None` if `prefix` is not a strict
    /// prefix of this path - i.e. having removed `prefix`, there are no elements left.
    /// For the intended use case of stripping a directory prefix from a file path,
    /// this is the correct behaviour, since it should not be possible to have
    /// `self == prefix`.
    pub fn remove_prefix_component<'a, E: IntoIterator<Item = &'a MPathElement>>(
        &self,
        prefix: E,
    ) -> Option<NonRootMPath> {
        self.0
            .remove_prefix_component(prefix)
            .into_optional_non_root_path()
    }

    pub fn generate<W: Write>(&self, out: &mut W) -> io::Result<()> {
        out.write_all(&self.to_vec())
    }

    pub fn to_vec(&self) -> Vec<u8> {
        self.0.to_vec()
    }

    pub fn iter_bytes(&self) -> impl Iterator<Item = u8> + '_ {
        self.0.iter_bytes()
    }

    pub fn compare_bytes(&self, other: &NonRootMPath) -> std::cmp::Ordering {
        self.0.compare_bytes(&other.0)
    }

    /// The length of this path, including any slashes in it.
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    // Private because it does not validate elements - you must ensure that it's non-empty
    fn from_elements<'a, I>(elements: I) -> Self
    where
        I: Iterator<Item = &'a MPathElement>,
    {
        Self(MPath::from_elements(elements))
    }

    /// Split an NonRootMPath into dirname (if possible) and file name
    pub fn split_dirname(&self) -> (Option<NonRootMPath>, &MPathElement) {
        let (filename, dirname_elements) = self
            .0
            .elements
            .split_last()
            .expect("NonRootMPaths should never be empty");

        if dirname_elements.is_empty() {
            (None, filename)
        } else {
            (
                Some(NonRootMPath::from_elements(dirname_elements.iter())),
                filename,
            )
        }
    }

    /// Split an MPath into first path component and the rest
    pub fn split_first(&self) -> (&MPathElement, Option<Self>) {
        let (first, file_elements) = self
            .0
            .elements
            .split_first()
            .expect("NonRootMPaths should never be empty");

        if file_elements.is_empty() {
            (first, None)
        } else {
            (first, Some(Self::from_elements(file_elements.iter())))
        }
    }

    pub fn into_thrift(self) -> thrift::path::NonRootMPath {
        thrift::path::NonRootMPath(self.0.into_thrift())
    }

    pub fn display_opt<'a>(path_opt: Option<&'a NonRootMPath>) -> DisplayOpt<'a> {
        DisplayOpt(path_opt.into())
    }

    pub fn get_path_hash(&self) -> MPathHash {
        let mut context = MPathHashContext::new();
        let num_el = self.0.elements.len();
        if num_el > 0 {
            if num_el > 1 {
                for e in &self.0.elements[..num_el - 1] {
                    context.update(e.as_ref());
                    context.update(*b"/")
                }
            }
            context.update(self.0.elements[num_el - 1].as_ref());
        } else {
            context.update([])
        }
        context.finish()
    }

    /// Returns an iterator of the path and each of its ancestor directories, up
    /// to but not includling the root.
    pub fn into_non_root_ancestors(self) -> NonRootMPathAncestors {
        NonRootMPathAncestors { next: Some(self) }
    }

    pub fn matches_regex(&self, re: &Regex) -> bool {
        let s: String = format!("{self}");
        re.is_match(&s)
    }

    pub fn as_mpath(&self) -> &MPath {
        &self.0
    }
}

impl ThriftConvert for MPath {
    const NAME: &'static str = "MPath";
    type Thrift = thrift::path::MPath;

    fn from_thrift(thrift: Self::Thrift) -> Result<Self> {
        let elements: Result<Vec<_>> = thrift
            .0
            .into_iter()
            .map(MPathElement::from_thrift)
            .collect();
        let elements = elements?;

        Ok(Self { elements })
    }

    fn into_thrift(self) -> Self::Thrift {
        thrift::path::MPath(
            self.elements
                .into_iter()
                .map(|elem| elem.into_thrift())
                .collect(),
        )
    }
}

pub fn path_bytes_from_mpath(path: &MPath) -> Vec<u8> {
    path.to_vec()
}

impl AsRef<[MPathElement]> for NonRootMPath {
    fn as_ref(&self) -> &[MPathElement] {
        self.0.as_ref()
    }
}

pub fn non_root_mpath_element_iter<'a>(
    mpath: &'a Option<NonRootMPath>,
) -> Box<dyn Iterator<Item = &'a MPathElement> + 'a> {
    match mpath {
        Some(path) => Box::new(path.into_iter()),
        None => Box::new(std::iter::empty()),
    }
}

pub fn mpath_element_iter<'a>(mpath: &'a MPath) -> Box<dyn Iterator<Item = &'a MPathElement> + 'a> {
    Box::new(mpath.into_iter())
}

impl IntoIterator for NonRootMPath {
    type Item = MPathElement;
    type IntoIter = ::std::vec::IntoIter<Self::Item>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<'a> IntoIterator for &'a NonRootMPath {
    type Item = &'a MPathElement;
    type IntoIter = Iter<'a, MPathElement>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.as_ref().iter()
    }
}

impl<'a> From<&'a NonRootMPath> for Vec<u8> {
    fn from(path: &NonRootMPath) -> Self {
        path.to_vec()
    }
}

impl<'a> TryFrom<&'a [u8]> for NonRootMPath {
    type Error = Error;

    fn try_from(value: &[u8]) -> Result<Self> {
        NonRootMPath::new(value)
    }
}

impl<'a> TryFrom<&'a str> for NonRootMPath {
    type Error = Error;

    fn try_from(value: &str) -> Result<Self> {
        NonRootMPath::new(value.as_bytes())
    }
}

impl<'a> TryFrom<&'a OsStr> for NonRootMPath {
    type Error = Error;

    fn try_from(value: &OsStr) -> Result<Self> {
        NonRootMPath::new(value.as_bytes())
    }
}

impl TryFrom<Vec<MPathElement>> for NonRootMPath {
    type Error = Error;

    fn try_from(elements: Vec<MPathElement>) -> Result<Self> {
        let mpath: MPath = elements.into();
        if mpath.is_root() {
            bail!("non-root mpath can not be empty");
        };
        Ok(NonRootMPath(mpath))
    }
}

impl Arbitrary for NonRootMPath {
    #[inline]
    fn arbitrary(g: &mut Gen) -> Self {
        NonRootMPath(MPath::arbitrary(g))
    }
}

/// Iterator over the a path's ancestors.
pub struct MPathAncestors {
    next: Option<MPath>,
}

impl Iterator for MPathAncestors {
    type Item = MPath;

    fn next(&mut self) -> Option<Self::Item> {
        let parent = self
            .next
            .as_ref()?
            .split_dirname()
            .map(|(parent, _)| parent);
        std::mem::replace(&mut self.next, parent)
    }
}

/// Iterator over the a path's non-root ancestors.
pub struct NonRootMPathAncestors {
    next: Option<NonRootMPath>,
}

impl Iterator for NonRootMPathAncestors {
    type Item = NonRootMPath;

    fn next(&mut self) -> Option<Self::Item> {
        let parent = self.next.as_ref()?.split_dirname().0;
        std::mem::replace(&mut self.next, parent)
    }
}

/// Hash of the file path (used in unode)
#[derive(Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Debug, Hash)]
pub struct MPathHash(Blake2);

impl MPathHash {
    pub fn from_thrift(thrift_path: thrift::id::MPathHash) -> Result<MPathHash> {
        match thrift_path.0 {
            thrift::id::Id::Blake2(blake2) => Ok(MPathHash(Blake2::from_thrift(blake2)?)),
            thrift::id::Id::UnknownField(x) => bail!(MononokeTypeError::InvalidThrift(
                "MPathHash".into(),
                format!("unknown id type field: {x}")
            )),
        }
    }

    pub fn into_thrift(self) -> thrift::id::MPathHash {
        thrift::id::MPathHash(thrift::id::Id::Blake2(self.0.into_thrift()))
    }

    pub fn to_hex(&self) -> AsciiString {
        self.0.to_hex()
    }

    pub fn sampling_fingerprint(&self) -> u64 {
        self.0.sampling_fingerprint()
    }
}

impl FromStr for MPathHash {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        Ok(MPathHash(Blake2::from_str(s)?))
    }
}

impl Display for MPathHash {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        write!(fmt, "{}", self.0.to_hex())
    }
}

/// Context for incrementally computing a hash.
#[derive(Clone)]
pub struct MPathHashContext(Context);

impl MPathHashContext {
    /// Construct a context.
    #[inline]
    pub fn new() -> Self {
        Self(Context::new("mpathhash".as_bytes()))
    }

    #[inline]
    pub fn update<T>(&mut self, data: T)
    where
        T: AsRef<[u8]>,
    {
        self.0.update(data)
    }

    #[inline]
    pub fn finish(self) -> MPathHash {
        MPathHash(self.0.finish())
    }
}

pub struct DisplayOpt<'a>(&'a MPath);

impl<'a> Display for DisplayOpt<'a> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if self.0.is_root() {
            write!(f, "(none)")
        } else {
            write!(f, "{}", self.0)
        }
    }
}

/// Check that a sorted list of (NonRootMPath, is_changed) pairs is path-conflict-free. This means that
/// no changed path in the list (is_changed is true) is a directory of another path.
pub fn check_pcf<'a, I>(sorted_paths: I) -> Result<()>
where
    I: IntoIterator<Item = (&'a NonRootMPath, bool)>,
{
    let mut last_changed_path: Option<&NonRootMPath> = None;
    // The key observation to make here is that in a sorted list, "foo" will always appear before
    // "foo/bar", which in turn will always appear before "foo1".
    // The loop invariant is that last_changed_path at any point has no prefixes in the list.
    for (path, is_changed) in sorted_paths {
        if let Some(last_changed_path) = last_changed_path {
            if last_changed_path.is_prefix_of(path) {
                bail!(MononokeTypeError::NotPathConflictFree(
                    last_changed_path.clone(),
                    path.clone(),
                ));
            }
        }
        if is_changed {
            last_changed_path = Some(path);
        }
    }

    Ok(())
}

/// Find conflicting paths between two sets. Two paths conflict if they are
/// equal or one is a prefix of the other (e.g. `a/b` and `a/b/c`).
///
/// Returns a list of `(left, right)` pairs where each pair identifies a
/// conflict. Both inputs are sorted internally; duplicates are preserved.
///
/// This is a generic version of the algorithm used in pushrebase's
/// `intersect_changed_files`.
pub fn find_path_conflicts(left: Vec<MPath>, right: Vec<MPath>) -> Vec<(MPath, MPath)> {
    let mut left = {
        let mut left = left;
        left.sort_unstable();
        left.into_iter()
    };
    let mut right = {
        let mut right = right;
        right.sort_unstable();
        right.into_iter()
    };

    let mut conflicts = Vec::new();
    let mut state = (left.next(), right.next());
    loop {
        state = match state {
            (Some(l), Some(r)) => match l.cmp(&r) {
                Ordering::Equal => {
                    conflicts.push((l.clone(), r.clone()));
                    (left.next(), right.next())
                }
                Ordering::Less => {
                    if l.is_prefix_of(&r) {
                        conflicts.push((l.clone(), r.clone()));
                    }
                    (left.next(), Some(r))
                }
                Ordering::Greater => {
                    if r.is_prefix_of(&l) {
                        conflicts.push((l.clone(), r.clone()));
                    }
                    (Some(l), right.next())
                }
            },
            _ => break,
        };
    }
    conflicts
}

impl<'a> IntoIterator for &'a MPathElement {
    type Item = &'a MPathElement;
    type IntoIter = Once<&'a MPathElement>;

    fn into_iter(self) -> Self::IntoIter {
        once(self)
    }
}

impl Display for NonRootMPath {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        write!(fmt, "{}", String::from_utf8_lossy(&self.to_vec()))
    }
}

impl Display for MPath {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        write!(fmt, "{}", String::from_utf8_lossy(&self.to_vec()))
    }
}

// Implement our own Debug so that strings are displayed properly
impl fmt::Debug for NonRootMPath {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        write!(fmt, "NonRootMPath(\"{self}\")")
    }
}

impl fmt::Debug for MPath {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        write!(fmt, "MPath(\"{self}\")")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrefixTrie {
    Included,
    Children(HashMap<MPathElement, PrefixTrie>),
}

impl PrefixTrie {
    /// Create a new, empty, prefix trie.
    pub fn new() -> PrefixTrie {
        PrefixTrie::Children(HashMap::new())
    }

    /// Add a path prefix to the prefix trie.  Returns true if the prefix
    /// wasn't already present.
    pub fn add<'p, P: IntoIterator<Item = &'p MPathElement>>(&mut self, path: P) -> bool {
        match self {
            PrefixTrie::Included => false,
            PrefixTrie::Children(children) => {
                let mut iter = path.into_iter();
                match iter.next() {
                    None => {
                        *self = PrefixTrie::Included;
                        true
                    }
                    Some(element) => {
                        if let Some(child) = children.get_mut(element) {
                            return child.add(iter);
                        }
                        children
                            .entry(element.clone())
                            .or_insert_with(PrefixTrie::new)
                            .add(iter)
                    }
                }
            }
        }
    }

    /// Returns true if any path prefix of the given path has previously been
    /// added to the prefix trie.
    pub fn contains_prefix<'p, P: IntoIterator<Item = &'p MPathElement>>(&self, path: P) -> bool {
        match self {
            PrefixTrie::Included => true,
            PrefixTrie::Children(children) => {
                let mut iter = path.into_iter();
                match iter.next() {
                    None => false,
                    Some(element) => {
                        if let Some(child) = children.get(element) {
                            return child.contains_prefix(iter);
                        }
                        false
                    }
                }
            }
        }
    }

    /// Returns true if the given path is equal to, an ancestor of, or a
    /// descendant of a path in the trie.
    pub fn has_path_conflict<'p, P: IntoIterator<Item = &'p MPathElement>>(&self, path: P) -> bool {
        match self {
            PrefixTrie::Included => true,
            PrefixTrie::Children(children) => {
                let mut iter = path.into_iter();
                match iter.next() {
                    None => !children.is_empty(),
                    Some(element) => children
                        .get(element)
                        .is_some_and(|child| child.has_path_conflict(iter)),
                }
            }
        }
    }

    /// Returns true if this trie contains all paths.
    pub fn contains_everything(&self) -> bool {
        self == &PrefixTrie::Included
    }
}

impl Default for PrefixTrie {
    fn default() -> PrefixTrie {
        PrefixTrie::Children(HashMap::new())
    }
}

impl Extend<MPath> for PrefixTrie {
    fn extend<T: IntoIterator<Item = MPath>>(&mut self, iter: T) {
        for path in iter {
            if !path.is_root() {
                self.add(&path);
            } else {
                // The empty path (root) means all paths are included.
                *self = PrefixTrie::Included;
            }
        }
    }
}

impl FromIterator<MPath> for PrefixTrie {
    fn from_iter<I: IntoIterator<Item = MPath>>(iter: I) -> Self {
        let mut trie = PrefixTrie::new();
        trie.extend(iter);
        trie
    }
}

impl Extend<Option<NonRootMPath>> for PrefixTrie {
    fn extend<T: IntoIterator<Item = Option<NonRootMPath>>>(&mut self, iter: T) {
        for path in iter {
            if let Some(path) = path {
                self.add(&path);
            } else {
                // The empty path means all paths are included.
                *self = PrefixTrie::Included;
            }
        }
    }
}

impl FromIterator<Option<NonRootMPath>> for PrefixTrie {
    fn from_iter<I: IntoIterator<Item = Option<NonRootMPath>>>(iter: I) -> Self {
        let mut trie = PrefixTrie::new();
        trie.extend(iter);
        trie
    }
}

pub struct CaseConflictTrie<'a> {
    children: HashMap<MPathElement, CaseConflictTrie<'a>>,
    lowercase_to_original: HashMap<String, MPathElement>,
    /// The tree this trie stands for has entries under this node that are
    /// not in `children`, so removing every child does not empty it.
    has_unlisted_children: bool,
    exclusions: &'a PrefixTrie,
}

impl<'a> CaseConflictTrie<'a> {
    pub fn new(exclusions: &'a PrefixTrie) -> Self {
        Self {
            children: Default::default(),
            lowercase_to_original: Default::default(),
            has_unlisted_children: false,
            exclusions,
        }
    }

    /// Records a path that exists in the tree before any update is applied,
    /// so that later additions are checked against it and later removals
    /// can take it away. `has_unlisted_children` says the path is a
    /// directory with entries this trie is not told about: it then stays
    /// after every listed child is removed. A path that conflicts with an
    /// earlier existing entry is left out: the conflict predates the
    /// updates.
    pub fn add_existing(&mut self, path: &NonRootMPath, has_unlisted_children: bool) {
        if self.exclusions.contains_prefix(path) {
            return;
        }
        if self.add_internal(path).is_ok()
            && let Some(node) = self.node_mut(path)
        {
            node.has_unlisted_children |= has_unlisted_children;
        }
    }

    /// Records a changeset that is already part of the tree the trie stands
    /// for: what it removes is taken away, what it adds or changes becomes
    /// an existing entry. Unlike applying it as an update, nothing it adds
    /// is a conflict.
    pub fn apply_existing(&mut self, bcs: &BonsaiChangeset) {
        for (path, change) in bcs.file_changes() {
            if change.is_removed() {
                self.remove(path);
            }
        }
        for (path, change) in bcs.file_changes() {
            if change.is_changed() {
                self.add_existing(path, false);
            }
        }
    }

    fn node_mut<'p, P>(&mut self, path: P) -> Option<&mut Self>
    where
        P: IntoIterator<Item = &'p MPathElement>,
    {
        path.into_iter()
            .try_fold(self, |node, element| node.children.get_mut(element))
    }

    pub fn check_conflicts<P, I>(&mut self, iter: I) -> Option<(NonRootMPath, NonRootMPath)>
    where
        P: CaseConflictTrieUpdate,
        I: IntoIterator<Item = P>,
    {
        for update in iter {
            let conflict = update.apply(self);
            if conflict.is_some() {
                return conflict;
            }
        }
        None
    }

    fn is_empty(&self) -> bool {
        self.children.is_empty() && !self.has_unlisted_children
    }

    /// Returns Ok(()) if element was added successfully, or Err
    /// if trie already contains case conflicting entry.
    fn add<'p, P>(&mut self, path: P) -> Result<(), ReverseMPath>
    where
        P: IntoIterator<Item = &'p MPathElement> + Clone,
    {
        if self.exclusions.contains_prefix(path.clone()) {
            return Ok(());
        }

        self.add_internal(path)
    }

    fn add_internal<'p, P>(&mut self, path: P) -> Result<(), ReverseMPath>
    where
        P: IntoIterator<Item = &'p MPathElement>,
    {
        let mut iter = path.into_iter();
        match iter.next() {
            None => Ok(()),
            Some(element) => {
                if let Some(child) = self.children.get_mut(element) {
                    return child.add_internal(iter).map_err(|mut e| {
                        e.elements.push(element.clone());
                        e
                    });
                }

                if let Some(lower) = element.to_lowercase_utf8() {
                    if let Some(conflict) = self.lowercase_to_original.get(&lower) {
                        return Err(ReverseMPath {
                            elements: vec![conflict.clone()],
                        });
                    } else {
                        self.lowercase_to_original.insert(lower, element.clone());
                    }
                }

                self.children
                    .entry(element.clone())
                    .or_insert_with(|| CaseConflictTrie::new(self.exclusions))
                    .add_internal(iter)
            }
        }
    }

    /// Remove path from a trie
    ///
    /// Returns `true` if path was removed, otherwise `false`.
    fn remove<'p, P>(&mut self, path: P) -> bool
    where
        P: IntoIterator<Item = &'p MPathElement> + Clone,
    {
        if self.exclusions.contains_prefix(path.clone()) {
            return true;
        }

        self.remove_internal(path)
    }

    fn remove_internal<'p, P>(&mut self, path: P) -> bool
    where
        P: IntoIterator<Item = &'p MPathElement>,
    {
        let mut iter = path.into_iter();
        match iter.next() {
            None => true,
            Some(element) => {
                let (found, remove) = match self.children.get_mut(element) {
                    None => return false,
                    Some(child) => (child.remove_internal(iter), child.is_empty()),
                };
                if remove {
                    self.children.remove(element);

                    if let Some(lower) = element.to_lowercase_utf8() {
                        self.lowercase_to_original.remove(&lower);
                    }
                }
                found
            }
        }
    }
}

struct ReverseMPath {
    /// Elements that are found to conflict. This is in reverse order.
    elements: Vec<MPathElement>,
}

impl ReverseMPath {
    pub fn into_mpath(self) -> NonRootMPath {
        let Self { mut elements } = self;
        elements.reverse();
        NonRootMPath(elements.into())
    }
}

pub trait CaseConflictTrieUpdate {
    /// Add this to the CaseConflictTrie. If this results in a case conflict, report the two paths
    /// that conflicted, in the order in which they were added to the CaseConflictTrie.
    fn apply(self, trie: &mut CaseConflictTrie) -> Option<(NonRootMPath, NonRootMPath)>;
}

impl<'a> CaseConflictTrieUpdate for &'a NonRootMPath {
    fn apply(self, trie: &mut CaseConflictTrie) -> Option<(NonRootMPath, NonRootMPath)> {
        match trie.add(self) {
            Ok(()) => None,
            Err(conflict) => Some((conflict.into_mpath(), self.clone())),
        }
    }
}

impl CaseConflictTrieUpdate for NonRootMPath {
    fn apply(self, trie: &mut CaseConflictTrie) -> Option<(NonRootMPath, NonRootMPath)> {
        match trie.add(&self) {
            Ok(()) => None,
            Err(conflict) => Some((conflict.into_mpath(), self)),
        }
    }
}

impl<'a> CaseConflictTrieUpdate for &'a BonsaiChangeset {
    fn apply(self, trie: &mut CaseConflictTrie) -> Option<(NonRootMPath, NonRootMPath)> {
        // we need apply deletion first
        for (path, change) in self.file_changes() {
            if change.is_removed() {
                trie.remove(path);
            }
        }
        for (path, change) in self.file_changes() {
            if change.is_changed() {
                if let Some(conflict) = path.apply(trie) {
                    return Some(conflict);
                }
            }
        }
        None
    }
}

/// Returns first path pair that would introduce a case-conflict, if any. The first element is the
/// first one that was added into the Trie, and the second is the last.
pub fn check_case_conflicts<P, I>(
    iter: I,
    exclusions: &PrefixTrie,
) -> Option<(NonRootMPath, NonRootMPath)>
where
    P: CaseConflictTrieUpdate,
    I: IntoIterator<Item = P>,
{
    CaseConflictTrie::new(exclusions).check_conflicts(iter)
}

#[cfg(test)]
mod test {
    use std::mem::size_of;

    use mononoke_macros::mononoke;
    use quickcheck::TestResult;
    use quickcheck::quickcheck;

    use super::*;

    #[mononoke::test]
    fn test_mpath_element_size() {
        // MPathElement size is important as we have a lot of them.
        // Test so we are aware of any change.
        assert_eq!(32, size_of::<MPathElement>());
    }

    #[mononoke::test]
    fn get_path_hash_multiple_elem() {
        let path = MPath::new("foo/bar/baz").unwrap();
        assert_eq!(
            format!("{}", path.get_path_hash().to_hex()).as_str(),
            "4b2cfeded9f9499ffecfed9cea1a36eab97511b241a74c2c84ab8cff45932d1e"
        );
    }

    #[mononoke::test]
    fn get_path_hash_single_elem() {
        let path = MPath::new("foo").unwrap();
        assert_eq!(
            format!("{}", path.get_path_hash().to_hex()).as_str(),
            "108cf7fc2bbc482daeab0ad8a9af2703cf041ba22ae728df26e1a33d51a3efb0"
        );
    }

    quickcheck! {
        /// Verify that instances generated by quickcheck are valid.
        fn path_gen(p: MPath) -> bool {
            p.elements
                .iter()
                .map(|elem| MPathElement::verify(elem.as_ref()))
                .all(|res| res.is_ok())
        }

        /// Verify that MPathElement instances generated by quickcheck are valid.
        fn pathelement_gen(p: MPathElement) -> bool {
            MPathElement::verify(p.as_ref()).is_ok()
        }

        fn elements_to_path(elements: Vec<MPathElement>) -> TestResult {
            let joined = elements.iter().map(|elem| elem.0.clone())
                .collect::<Vec<_>>()
                .join(&b'/');
            let expected_len = joined.len();
            let path = MPath::new(joined).unwrap();
            TestResult::from_bool(elements == path.elements && path.to_vec().len() == expected_len)
        }

        fn path_len(p: MPath) -> bool {
            p.len() == p.to_vec().len()
        }

        fn repo_path_thrift_roundtrip(p: RepoPath) -> bool {
            let thrift_path = p.clone().into_thrift();
            let p2 = RepoPath::from_thrift(thrift_path)
                .expect("converting a valid Thrift structure should always work");
            p == p2
        }

        fn path_thrift_roundtrip(p: MPath) -> bool {
            let thrift_path = p.clone().into_thrift();
            let p2 = MPath::from_thrift(thrift_path)
                .expect("converting a valid Thrift structure should always work");
            p == p2
        }

        fn non_root_path_thrift_roundtrip(p: NonRootMPath) -> bool {
            let thrift_path = p.clone().into_thrift();
            let p2 = NonRootMPath::from_thrift(thrift_path)
                .expect("converting a valid Thrift structure should always work");
            p == p2
        }

        fn pathelement_thrift_roundtrip(p: MPathElement) -> bool {
            let thrift_pathelement = p.clone().into_thrift();
            let p2 = MPathElement::from_thrift(thrift_pathelement)
                .expect("converting a valid Thrift structure should always works");
            p == p2
        }

        fn compare_bytes(p1: MPath, p2: MPath) -> bool {
            p1.to_vec().cmp(&p2.to_vec()) == p1.compare_bytes(&p2)
        }
    }

    #[mononoke::test]
    fn path_make() {
        let path = MPath::new(b"1234abc");
        assert!(MPath::new(b"1234abc").is_ok());
        assert_eq!(path.unwrap().to_vec().len(), 7);
    }

    #[mononoke::test]
    fn repo_path_make() {
        let path = NonRootMPath::new(b"abc").unwrap();
        assert_eq!(
            RepoPath::dir(path.clone()).unwrap(),
            RepoPath::dir("abc").unwrap()
        );
        assert_ne!(RepoPath::dir(path).unwrap(), RepoPath::file("abc").unwrap());
    }

    #[mononoke::test]
    fn empty_non_root_paths() {
        fn assert_empty(path: &str) {
            NonRootMPath::new(path)
                .expect_err(&format!("unexpected OK - path '{path}' is logically empty",));
        }
        assert_empty("");
        assert_empty("/");
        assert_empty("//");
        assert_empty("///");
        assert_empty("////");
    }

    #[mononoke::test]
    fn empty_paths() {
        fn assert_empty(path: &str) {
            MPath::new(path).unwrap_or_else(|_| panic!("unexpected err - path '{path}' is logically empty which should be allowed for MPath"));
        }
        assert_empty("");
        assert_empty("/");
        assert_empty("//");
        assert_empty("///");
        assert_empty("////");
    }

    #[mononoke::test]
    fn ancestors() {
        fn path(p: &str) -> MPath {
            MPath::new(p).unwrap()
        }

        fn collect_ancestors(p: &str) -> Vec<MPath> {
            path(p).into_ancestors().collect()
        }

        assert_eq!(collect_ancestors("a"), vec![path("a"), MPath::ROOT]);
        assert_eq!(
            collect_ancestors("a/b"),
            vec![path("a/b"), path("a"), MPath::ROOT]
        );
        assert_eq!(
            collect_ancestors("a/b/c"),
            vec![path("a/b/c"), path("a/b"), path("a"), MPath::ROOT]
        );
    }

    #[mononoke::test]
    fn non_root_ancestors() {
        fn path(p: &str) -> NonRootMPath {
            NonRootMPath::new(p).unwrap()
        }

        fn collect_non_root_ancestors(p: &str) -> Vec<NonRootMPath> {
            path(p).into_non_root_ancestors().collect()
        }

        assert_eq!(collect_non_root_ancestors("a"), vec![path("a")]);
        assert_eq!(
            collect_non_root_ancestors("a/b"),
            vec![path("a/b"), path("a")]
        );
        assert_eq!(
            collect_non_root_ancestors("a/b/c"),
            vec![path("a/b/c"), path("a/b"), path("a")]
        );
    }

    #[mononoke::test]
    fn components() {
        let foo = MPath::new("foo").unwrap();
        let foo_bar1 = MPath::new("foo/bar1").unwrap();
        let foo_bar12 = MPath::new("foo/bar12").unwrap();
        let baz = MPath::new("baz").unwrap();

        assert_eq!(foo.common_components(&foo), 1);
        assert_eq!(foo.common_components(&foo_bar1), 1);
        assert_eq!(foo.common_components(&foo_bar12), 1);
        assert_eq!(foo_bar1.common_components(&foo_bar1), 2);
        assert_eq!(foo.common_components(&baz), 0);
        assert_eq!(foo.common_components(None), 0);

        let root_foo = MPath::new("").unwrap();
        assert_eq!(root_foo.common_components(&MPath::ROOT), 0);

        assert_eq!(foo_bar1.take_prefix_components(0).unwrap(), MPath::ROOT);
        assert_eq!(foo_bar1.take_prefix_components(1).unwrap(), foo);
        assert_eq!(foo_bar1.take_prefix_components(2).unwrap(), foo_bar1);
        foo_bar1
            .take_prefix_components(3)
            .expect_err("unexpected OK - too many components");
    }

    #[mononoke::test]
    fn remove_prefix_component() {
        let foo = MPath::new("foo").unwrap();
        let foo_bar1 = MPath::new("foo/bar1").unwrap();
        let foo_bar12 = MPath::new("foo/bar1/2").unwrap();
        let baz = MPath::new("baz").unwrap();
        let bar1 = MPath::new("bar1").unwrap();
        let bar12 = MPath::new("bar1/2").unwrap();
        let two = MPath::new("2").unwrap();

        assert_eq!(baz.remove_prefix_component(&foo), MPath::ROOT);
        assert_eq!(foo_bar1.remove_prefix_component(&foo), bar1);
        assert_eq!(foo_bar12.remove_prefix_component(&foo), bar12);
        assert_eq!(foo_bar12.remove_prefix_component(&foo_bar1), two);
    }

    #[mononoke::test]
    fn bad_path() {
        assert!(NonRootMPath::new(b"\0").is_err());
    }
    #[mononoke::test]
    fn bad_path2() {
        assert!(NonRootMPath::new(b"abc\0").is_err());
    }
    #[mononoke::test]
    fn bad_path3() {
        assert!(NonRootMPath::new(b"ab\0cde").is_err());
    }
    #[mononoke::test]
    fn bad_path4() {
        let p = vec![97; 255];
        assert!(NonRootMPath::new(p).is_ok());

        let p = vec![97; 256];
        assert!(NonRootMPath::new(p).is_err());
    }

    #[mononoke::test]
    fn bad_path_element() {
        let p = vec![97; 255];
        assert!(MPathElement::new(p).is_ok());

        let p = vec![97; 256];
        assert!(MPathElement::new(p).is_err());
    }

    #[mononoke::test]
    fn bad_path_thrift() {
        let bad_thrift =
            thrift::path::MPath(vec![thrift::path::MPathElement(b"abc\0".to_vec().into())]);
        MPath::from_thrift(bad_thrift).expect_err("unexpected OK - embedded null");

        let bad_thrift =
            thrift::path::MPath(vec![thrift::path::MPathElement(b"def/ghi".to_vec().into())]);
        MPath::from_thrift(bad_thrift).expect_err("unexpected OK - embedded slash");
    }

    #[mononoke::test]
    fn path_cmp() {
        let a = NonRootMPath::new(b"a").unwrap();
        let b = NonRootMPath::new(b"b").unwrap();

        assert!(a < b);
        assert!(a == a);
        assert!(b == b);
        assert!(a <= a);
        assert!(a <= b);
    }

    #[mononoke::test]
    fn pcf() {
        check_pcf_paths(vec![("foo", true), ("bar", true)])
            .expect("unexpected Err - no directories");
        check_pcf_paths(vec![("foo", true), ("foo/bar", true)])
            .expect_err("unexpected OK - foo is a prefix of foo/bar");
        check_pcf_paths(vec![("foo", false), ("foo/bar", true)])
            .expect("unexpected Err - foo is a prefix of foo/bar but is_changed is false");
        check_pcf_paths(vec![("foo", true), ("foo/bar", false)])
            .expect_err("unexpected OK - foo/bar's is_changed state does not matter");
        check_pcf_paths(vec![("foo", true), ("foo1", true)])
            .expect("unexpected Err - foo is not a path prefix of foo1");
        check_pcf_paths::<_, &str>(vec![])
            .expect("unexpected Err - empty path list has no prefixes");
        // '/' is ASCII 0x2f
        check_pcf_paths(vec![
            ("foo/bar", true),
            ("foo/bar\x2e", true),
            ("foo/bar/baz", true),
            ("foo/bar\x30", true),
        ])
        .expect_err("unexpected OK - other paths and prefixes");
    }

    #[mononoke::test]
    fn case_conflicts() {
        fn m(mpath: &str) -> NonRootMPath {
            NonRootMPath::new(mpath).unwrap()
        }

        let exclusions = Default::default();
        let mut trie = CaseConflictTrie::new(&exclusions);
        assert!(trie.add(&m("a/b/c")).is_ok());
        assert!(trie.add(&m("a/d")).is_ok());
        assert!(trie.add(&m("c/d/a")).is_ok());
        assert!(trie.add(&m("a/b/c")).is_ok());
        assert!(trie.add(&m("a/B/d")).is_err());
        assert!(trie.add(&m("a/b/C")).is_err());
        assert!(trie.remove(&m("a/b/c")));
        assert!(trie.add(&m("a/B/c")).is_ok());

        let paths = vec![
            m("a/b/c"),
            m("a/b/c"), // not a case conflict
            m("a/d"),
            m("a/B/d"),
            m("a/c"),
        ];
        assert_eq!(
            check_case_conflicts(paths.iter(), &Default::default()), // works from &NonRootMPath
            Some((m("a/b"), m("a/B/d"))),
        );
        assert_eq!(
            check_case_conflicts(paths.into_iter(), &Default::default()), // works from NonRootMPath
            Some((m("a/b"), m("a/B/d"))),
        );
    }

    #[mononoke::test]
    fn case_conflicts_with_existing_entries() {
        fn m(mpath: &str) -> NonRootMPath {
            NonRootMPath::new(mpath).unwrap()
        }

        let exclusions = Default::default();
        let mut trie = CaseConflictTrie::new(&exclusions);
        // `Dir/a` is listed; `Dir` has other entries the trie is not told about.
        trie.add_existing(&m("Dir"), true);
        trie.add_existing(&m("Dir/a"), false);
        trie.add_existing(&m("File"), false);

        // Existing names are checked against.
        assert!(trie.add(&m("dir/x")).is_err());
        assert!(trie.add(&m("file")).is_err());
        // Removing the only listed child does not empty a directory with
        // unlisted children.
        assert!(trie.remove(&m("Dir/a")));
        assert!(trie.add(&m("dir/x")).is_err());
        // A removed existing file is gone.
        assert!(trie.remove(&m("File")));
        assert!(trie.add(&m("file")).is_ok());

        // A directory whose children are all listed empties when they go.
        let mut trie = CaseConflictTrie::new(&exclusions);
        trie.add_existing(&m("Dir"), false);
        trie.add_existing(&m("Dir/a"), false);
        trie.add_existing(&m("Dir/b"), false);
        assert!(trie.remove(&m("Dir/a")));
        assert!(trie.add(&m("dir/x")).is_err());
        assert!(trie.remove(&m("Dir/b")));
        assert!(trie.add(&m("dir/x")).is_ok());

        // An existing entry that conflicts with an earlier one is left out.
        let mut trie = CaseConflictTrie::new(&exclusions);
        trie.add_existing(&m("Foo"), false);
        trie.add_existing(&m("foo"), false);
        assert!(trie.add(&m("Foo")).is_ok());
        assert!(trie.add(&m("FOO")).is_err());
    }

    fn check_pcf_paths<I, T>(paths: I) -> Result<()>
    where
        I: IntoIterator<Item = (T, bool)>,
        NonRootMPath: TryFrom<T, Error = Error>,
    {
        let res: Result<Vec<_>> = paths
            .into_iter()
            .map(|(path, is_changed)| Ok((path.try_into()?, is_changed)))
            .collect();
        let mut paths = res.expect("invalid input path");
        // The input calls for a *sorted* list -- this is important.
        paths.sort_unstable();
        check_pcf(paths.iter().map(|(path, is_changed)| (path, *is_changed)))
    }

    #[mononoke::test]
    fn prefix_trie() {
        let mut prefixes = PrefixTrie::new();

        let path = |path| NonRootMPath::new(path).unwrap();

        // Add some paths
        assert!(prefixes.add(&path("a/b/c")));
        assert!(prefixes.add(&path("a/b/d")));
        assert!(prefixes.add(&path("e")));

        // These paths are already covered by existing prefixes
        assert!(!prefixes.add(&path("a/b/c")));
        assert!(!prefixes.add(&path("e/f")));

        // Expanding a prefix with a more general one is okay
        assert!(prefixes.add(&path("g/h/i")));
        assert!(prefixes.add(&path("g/h")));

        // These paths should match
        assert!(prefixes.contains_prefix(&path("a/b/c/d")));
        assert!(prefixes.contains_prefix(&path("a/b/d/e/f/g")));
        assert!(prefixes.contains_prefix(&path("a/b/d")));
        assert!(prefixes.contains_prefix(&path("e/a")));
        assert!(prefixes.contains_prefix(&path("e/f/g")));
        assert!(prefixes.contains_prefix(&path("e")));
        assert!(prefixes.contains_prefix(&path("g/h")));
        assert!(prefixes.contains_prefix(&path("g/h/i/j")));
        assert!(prefixes.contains_prefix(&path("g/h/k")));

        // These paths should not match
        assert!(!prefixes.contains_prefix(&path("a/b")));
        assert!(!prefixes.contains_prefix(&path("a/b/cc")));
        assert!(!prefixes.contains_prefix(&path("a/b/e")));
        assert!(!prefixes.contains_prefix(&path("a/c")));
        assert!(!prefixes.contains_prefix(&path("a")));
        assert!(!prefixes.contains_prefix(&path("b")));
        assert!(!prefixes.contains_prefix(&path("d")));
        assert!(!prefixes.contains_prefix(&path("abc")));
        assert!(!prefixes.contains_prefix(&path("g")));
        assert!(!prefixes.contains_prefix(&path("g/i")));
        assert!(!prefixes.contains_prefix(&path("g/i/h")));
        assert!(!prefixes.contains_everything());

        assert!(prefixes.has_path_conflict(&path("a/b/c")));
        assert!(prefixes.has_path_conflict(&path("a/b/c/d")));
        assert!(prefixes.has_path_conflict(&path("a/b")));
        assert!(!prefixes.has_path_conflict(&path("a/c")));

        // Adding the empty path makes the trie contain everything
        assert!(prefixes.add(&None));
        assert!(prefixes.contains_prefix(&path("a/b/c")));
        assert!(prefixes.contains_prefix(&path("x/y/z")));
        assert!(prefixes.contains_everything());
    }

    #[mononoke::test]
    fn has_suffix_suffix() {
        let path = |path| NonRootMPath::new(path).unwrap();

        // Assert that when the suffix equals the basename the result is
        // correct.
        assert!(&path("a/b/foo.bar").basename().has_suffix(b"foo.bar"));

        // Assert that when the suffix contains is not the basename, the result
        // is correct.
        assert!(!&path("a/b/c").basename().has_suffix(b"b"));

        // Assert when the potential suffix is a suffix the result is correct.
        assert!(&path("a/b/c.bar").basename().has_suffix(b"r"));
        assert!(&path("a/b/c.bar").basename().has_suffix(b"bar"));
        assert!(&path("a/b/c.bar").basename().has_suffix(b".bar"));
        assert!(&path("a/b/c.bar").basename().has_suffix(b"c.bar"));

        // Assert when the potential suffix is not a suffix the result is
        // correct.
        assert!(!&path("a/b/file.bar").basename().has_suffix(b".baz"));
        assert!(!&path("a/b/file.bar").basename().has_suffix(b"baz"));
        assert!(!&path("a/b/c.bar").basename().has_suffix(b"c.baz"));

        // Test case when potential suffix is longer than entire path.
        assert!(
            !&path("a/b/foo.bar")
                .basename()
                .has_suffix(b"file.very_very_very_long_extension")
        );
    }

    fn mpath(s: &str) -> MPath {
        MPath::new(s).unwrap()
    }

    fn mpaths(paths: &[&str]) -> Vec<MPath> {
        paths.iter().map(|s| mpath(s)).collect()
    }

    #[mononoke::test]
    fn test_find_path_conflicts_disjoint() {
        let left = mpaths(&["a/b", "c/d"]);
        let right = mpaths(&["e/f", "g/h"]);
        assert!(find_path_conflicts(left, right).is_empty());
    }

    #[mononoke::test]
    fn test_find_path_conflicts_exact_match() {
        let left = mpaths(&["a/b", "x/y"]);
        let right = mpaths(&["a/b", "z/w"]);
        let result = find_path_conflicts(left, right);
        assert_eq!(result, vec![(mpath("a/b"), mpath("a/b"))]);
    }

    #[mononoke::test]
    fn test_find_path_conflicts_left_is_prefix() {
        let left = mpaths(&["a/b"]);
        let right = mpaths(&["a/b/c"]);
        let result = find_path_conflicts(left, right);
        assert_eq!(result, vec![(mpath("a/b"), mpath("a/b/c"))]);
    }

    #[mononoke::test]
    fn test_find_path_conflicts_right_is_prefix() {
        let left = mpaths(&["a/b/c/d"]);
        let right = mpaths(&["a/b/c"]);
        let result = find_path_conflicts(left, right);
        assert_eq!(result, vec![(mpath("a/b/c/d"), mpath("a/b/c"))]);
    }

    #[mononoke::test]
    fn test_find_path_conflicts_multiple() {
        let left = mpaths(&["a/b", "c/d", "e/f"]);
        let right = mpaths(&["a/b", "c/d/e", "g/h"]);
        let result = find_path_conflicts(left, right);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0], (mpath("a/b"), mpath("a/b")));
        assert_eq!(result[1], (mpath("c/d"), mpath("c/d/e")));
    }

    #[mononoke::test]
    fn test_find_path_conflicts_empty_sets() {
        assert!(find_path_conflicts(vec![], mpaths(&["a/b"])).is_empty());
        assert!(find_path_conflicts(mpaths(&["a/b"]), vec![]).is_empty());
        assert!(find_path_conflicts(vec![], vec![]).is_empty());
    }

    /// Regression test: when a prefix on one side is less than a path on the
    /// other side, the algorithm correctly detects the conflict without
    /// advancing both pointers.
    #[mononoke::test]
    fn test_find_path_conflicts_one_vs_many() {
        // "a/b" is a prefix of "a/b/c", so this should detect a conflict
        // even though "a/b" < "a/b/c" lexicographically.
        let left = mpaths(&["a/b"]);
        let right = mpaths(&["a/b/c"]);
        let result = find_path_conflicts(left, right);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], (mpath("a/b"), mpath("a/b/c")));

        // When paths match exactly, only the exact match is reported;
        // further prefix relationships are not enumerated since
        // detecting any conflict is sufficient.
        let left = mpaths(&["a/b", "a/b/c"]);
        let right = mpaths(&["a/b"]);
        let result = find_path_conflicts(left, right);
        assert!(!result.is_empty());
        assert_eq!(result[0], (mpath("a/b"), mpath("a/b")));
    }
}
