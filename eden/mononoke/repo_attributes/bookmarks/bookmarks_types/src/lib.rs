/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashSet;
use std::fmt;
use std::ops::Bound;
use std::ops::Range;
use std::ops::RangeBounds;
use std::ops::RangeFrom;
use std::ops::RangeFull;
use std::str::FromStr;

use anyhow::Error;
use anyhow::format_err;
use ascii::AsciiChar;
use ascii::AsciiString;
use quickcheck::Arbitrary;
use quickcheck::Gen;
use quickcheck_arbitrary_derive::Arbitrary;
use sql::mysql;
use sql::mysql_async::FromValueError;
use sql::mysql_async::Value;
use sql::mysql_async::prelude::FromValue;

/// This enum represents how fresh you want results to be. MostRecent will go to the master, so you
/// normally don't want to issue queries using MostRecent unless you have a very good reason.
/// MaybeStale will go to a replica, which might lag behind the master (there is no SLA on
/// replication lag). MaybeStale reads might also be served from a local cache.

#[derive(Arbitrary, Debug, Eq, PartialEq, Clone, Copy)]
pub enum Freshness {
    MostRecent,
    MaybeStale,
}

impl From<edenapi_types::bookmark::Freshness> for Freshness {
    fn from(f: edenapi_types::bookmark::Freshness) -> Self {
        match f {
            edenapi_types::bookmark::Freshness::MostRecent => Freshness::MostRecent,
            edenapi_types::bookmark::Freshness::MaybeStale => Freshness::MaybeStale,
        }
    }
}

#[derive(Arbitrary, Clone, Debug, Eq, Hash, PartialEq)]
pub struct Bookmark {
    pub key: BookmarkKey,
    pub kind: BookmarkKind,
}

impl Bookmark {
    pub fn new(key: BookmarkKey, kind: BookmarkKind) -> Self {
        Bookmark { key, kind }
    }

    pub fn into_key(self) -> BookmarkKey {
        self.key
    }

    pub fn key(&self) -> &BookmarkKey {
        &self.key
    }

    pub fn name(&self) -> &BookmarkName {
        &self.key.name
    }

    pub fn kind(&self) -> &BookmarkKind {
        &self.kind
    }

    pub fn publishing(&self) -> bool {
        use BookmarkKind::*;

        match self.kind {
            Scratch => false,
            Publishing => true,
            PullDefaultPublishing => true,
        }
    }

    pub fn pull_default(&self) -> bool {
        use BookmarkKind::*;

        match self.kind {
            Scratch => false,
            Publishing => false,
            PullDefaultPublishing => true,
        }
    }
}

#[derive(Arbitrary, Clone, Debug, Eq, Hash, PartialEq, Ord, PartialOrd)]
pub struct BookmarkKey {
    name: BookmarkName,
    category: BookmarkCategory,
}

impl FromStr for BookmarkKey {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        BookmarkKey::new(s)
    }
}

impl fmt::Display for BookmarkKey {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.name)
    }
}

impl BookmarkKey {
    pub fn with_name_and_category(name: BookmarkName, category: BookmarkCategory) -> Self {
        Self { name, category }
    }

    pub fn with_name(name: BookmarkName) -> Self {
        Self::with_name_and_category(name, Default::default())
    }

    /// First bookmark key with a given name.  Used for constructing ranges
    /// which include all categories.
    pub fn with_first_name(name: BookmarkName) -> Self {
        Self::with_name_and_category(name, BookmarkCategory::ALL.first().copied().unwrap())
    }

    /// Last bookmark key with a given name.  Used for constructing ranges
    /// which include all categories.
    pub fn with_last_name(name: BookmarkName) -> Self {
        Self::with_name_and_category(name, BookmarkCategory::ALL.last().copied().unwrap())
    }

    pub fn new<B: AsRef<str>>(bookmark: B) -> Result<Self, Error> {
        Ok(Self {
            name: BookmarkName::new(bookmark)?,
            category: Default::default(),
        })
    }

    pub fn new_ascii(bookmark: AsciiString) -> Self {
        Self {
            name: BookmarkName::new_ascii(bookmark),
            category: Default::default(),
        }
    }

    pub fn as_ascii(&self) -> &AsciiString {
        self.name.as_ascii()
    }

    pub fn into_string(self) -> String {
        self.name.into_string()
    }

    pub fn into_byte_vec(self) -> Vec<u8> {
        self.name.into_byte_vec()
    }

    pub fn as_str(&self) -> &str {
        self.name.as_str()
    }

    pub fn name(&self) -> &BookmarkName {
        &self.name
    }

    pub fn category(&self) -> &BookmarkCategory {
        &self.category
    }

    pub fn into_name(self) -> BookmarkName {
        self.name
    }

    /// Determine if the current BookmarkKey represents a tag (since BookmarkCategory
    /// is not reliable)
    pub fn is_tag(&self) -> bool {
        let ref_name = self.name().as_str();
        if let Some(stripped_ref) = ref_name.strip_prefix("namespaces/") {
            stripped_ref.contains("refs/tags/")
        } else {
            ref_name.starts_with("tags/")
        }
    }
}

/// Server-trusted set of annotated-tag bookmarks a push is creating/moving, whose bonsai_tag_mapping row may be written only in the bookmark transaction.
#[derive(Clone, Debug, Default)]
pub struct AnnotatedTags(HashSet<BookmarkKey>);

impl AnnotatedTags {
    pub fn new(tags: HashSet<BookmarkKey>) -> Self {
        Self(tags)
    }

    pub fn contains(&self, bookmark: &BookmarkKey) -> bool {
        self.0.contains(bookmark)
    }
}

impl FromIterator<BookmarkKey> for AnnotatedTags {
    fn from_iter<I: IntoIterator<Item = BookmarkKey>>(iter: I) -> Self {
        Self::new(iter.into_iter().collect())
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, Ord, PartialOrd)]
#[derive(mysql::OptTryFromRowField)]
pub struct BookmarkName {
    bookmark: AsciiString,
}

impl FromStr for BookmarkName {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        BookmarkName::new(s)
    }
}

impl fmt::Display for BookmarkName {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.bookmark)
    }
}

impl Arbitrary for BookmarkName {
    fn arbitrary(g: &mut Gen) -> Self {
        // NOTE: We use a specific large size here because our tests exercise DB Bookmarks, which
        // require unique names in the DB.
        let size = 128;
        let mut bookmark = AsciiString::with_capacity(size);
        for _ in 0..size {
            bookmark.push(ascii_ext::AsciiChar::arbitrary(g).0);
        }
        Self { bookmark }
    }
}

impl BookmarkName {
    pub fn new<B: AsRef<str>>(bookmark: B) -> Result<Self, Error> {
        Ok(Self {
            bookmark: AsciiString::from_ascii(bookmark.as_ref())
                .map_err(|bytes| format_err!("non-ascii bookmark name: {bytes:?}"))?,
        })
    }

    pub fn new_ascii(bookmark: AsciiString) -> Self {
        Self { bookmark }
    }

    pub fn as_ascii(&self) -> &AsciiString {
        &self.bookmark
    }

    pub fn into_string(self) -> String {
        self.bookmark.into()
    }

    pub fn into_byte_vec(self) -> Vec<u8> {
        self.bookmark.into()
    }

    pub fn as_str(&self) -> &str {
        self.bookmark.as_str()
    }
}

impl TryFrom<&str> for BookmarkName {
    type Error = Error;

    fn try_from(s: &str) -> Result<Self, Error> {
        Self::new(s)
    }
}

impl From<BookmarkName> for Value {
    fn from(bookmark: BookmarkName) -> Self {
        Value::Bytes(bookmark.bookmark.into())
    }
}

impl TryFrom<Value> for BookmarkName {
    type Error = FromValueError;

    fn try_from(v: Value) -> Result<Self, FromValueError> {
        match v {
            Value::Bytes(bytes) => AsciiString::from_ascii(bytes)
                .map_err(|err| FromValueError(Value::Bytes(err.into_source())))
                .map(BookmarkName::new_ascii),
            v => Err(FromValueError(v)),
        }
    }
}

impl FromValue for BookmarkName {
    type Intermediate = BookmarkName;
}

impl From<BookmarkPrefix> for Value {
    fn from(bookmark_prefix: BookmarkPrefix) -> Self {
        Value::Bytes(bookmark_prefix.bookmark_prefix.into())
    }
}

/// Describes the behavior of a Bookmark in Mercurial operations.
#[derive(
    Arbitrary,
    Clone,
    Debug,
    Eq,
    Hash,
    PartialEq,
    Copy,
    clap::ValueEnum,
    mysql::OptTryFromRowField
)]
pub enum BookmarkKind {
    Scratch,
    Publishing,
    PullDefaultPublishing,
}

impl BookmarkKind {
    pub const ALL: &'static [BookmarkKind] = &[
        BookmarkKind::Scratch,
        BookmarkKind::Publishing,
        BookmarkKind::PullDefaultPublishing,
    ];
    pub const ALL_PUBLISHING: &'static [BookmarkKind] = &[
        BookmarkKind::Publishing,
        BookmarkKind::PullDefaultPublishing,
    ];

    pub fn is_public(&self) -> bool {
        match self {
            BookmarkKind::Scratch => false,
            BookmarkKind::Publishing | BookmarkKind::PullDefaultPublishing => true,
        }
    }
}

impl std::fmt::Display for BookmarkKind {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use BookmarkKind::*;

        let s = match self {
            Scratch => "scratch",
            Publishing => "publishing",
            PullDefaultPublishing => "pull_default",
        };

        write!(f, "{s}")
    }
}

const SCRATCH_KIND: &[u8] = b"scratch";
const PUBLISHING_KIND: &[u8] = b"publishing";
const PULL_DEFAULT_KIND: &[u8] = b"pull_default";

impl TryFrom<Value> for BookmarkKind {
    type Error = FromValueError;

    fn try_from(v: Value) -> Result<Self, FromValueError> {
        use BookmarkKind::*;

        match v {
            Value::Bytes(ref b) if b == SCRATCH_KIND => Ok(Scratch),
            Value::Bytes(ref b) if b == PUBLISHING_KIND => Ok(Publishing),
            Value::Bytes(ref b) if b == PULL_DEFAULT_KIND => Ok(PullDefaultPublishing),
            v => Err(FromValueError(v)),
        }
    }
}

impl FromValue for BookmarkKind {
    type Intermediate = BookmarkKind;
}

impl From<BookmarkKind> for Value {
    fn from(bookmark_update_reason: BookmarkKind) -> Self {
        use BookmarkKind::*;

        match bookmark_update_reason {
            Scratch => Value::Bytes(SCRATCH_KIND.to_vec()),
            Publishing => Value::Bytes(PUBLISHING_KIND.to_vec()),
            PullDefaultPublishing => Value::Bytes(PULL_DEFAULT_KIND.to_vec()),
        }
    }
}

/// Historically, in Mononoke, bookmark names have been unique.
/// Since we import repositories from Git, this invariant is not true anymore.
/// A tag, a branch or a note can have the same name as they are namespaced in Git.
/// BookmarkCategory allows to differentiate between e.g. a tag called foo and a branch called
/// foo.
/// See <https://docs.rs/git-ref/0.23.1/git_ref/enum.Category.html> for an exhaustive
/// list of what git supports.
/// Here, we explicitly don't support worktrees or the distinction between remote and local
/// branches as these concepts don't fit in the mononoke data model. We also don't support pseudo
/// refs such as HEAD as the server doesn't need to know about this.
#[derive(
    Arbitrary,
    Clone,
    Debug,
    Eq,
    Hash,
    PartialEq,
    Copy,
    Ord,
    PartialOrd,
    clap::ValueEnum,
    mysql::OptTryFromRowField,
    Default
)]
pub enum BookmarkCategory {
    /// A bookmark created in Mononoke or imported from a Git branch (ref living under
    /// `refs/heads` or `refs/remotes/<remote_name>`)
    #[default]
    Branch,
    /// A bookmark created from importing a Git tag (under `refs/tags`)
    Tag,
    /// A bookmark created from importing a Git note (under `refs/notes`)
    Note,
}

impl BookmarkCategory {
    pub const ALL: &'static [BookmarkCategory] = &[Self::Branch, Self::Tag, Self::Note];
}

const BRANCH_CATEGORY: &[u8] = b"branch";
const TAG_CATEGORY: &[u8] = b"tag";
const NOTE_CATEGORY: &[u8] = b"note";

impl From<BookmarkCategory> for Value {
    fn from(category: BookmarkCategory) -> Self {
        use BookmarkCategory::*;

        match category {
            Branch => Value::Bytes(BRANCH_CATEGORY.to_vec()),
            Tag => Value::Bytes(TAG_CATEGORY.to_vec()),
            Note => Value::Bytes(NOTE_CATEGORY.to_vec()),
        }
    }
}

impl TryFrom<Value> for BookmarkCategory {
    type Error = FromValueError;

    fn try_from(v: Value) -> Result<Self, FromValueError> {
        use BookmarkCategory::*;

        match v {
            Value::Bytes(ref b) if b == BRANCH_CATEGORY => Ok(Branch),
            Value::Bytes(ref b) if b == TAG_CATEGORY => Ok(Tag),
            Value::Bytes(ref b) if b == NOTE_CATEGORY => Ok(Note),
            v => Err(FromValueError(v)),
        }
    }
}

impl FromValue for BookmarkCategory {
    type Intermediate = BookmarkCategory;
}

impl std::fmt::Display for BookmarkCategory {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use BookmarkCategory::*;

        let s = match self {
            Branch => "branch",
            Tag => "tag",
            Note => "note",
        };

        write!(f, "{s}")
    }
}

/// Bookmark name filter for pagination.
///
/// If set to `BookmarkPagination::After(name)`, Filters bookmarks to those
/// starting after the given start point (exclusive).
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum BookmarkPagination {
    FromStart,
    After(BookmarkName),
}

/// Bookmark name filter for prefixes.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct BookmarkPrefix {
    bookmark_prefix: AsciiString,
}

impl fmt::Display for BookmarkPrefix {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.bookmark_prefix)
    }
}

impl FromStr for BookmarkPrefix {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        BookmarkPrefix::new(s)
    }
}

pub enum BookmarkPrefixRange {
    /// All bookmarks in the given half-open range.
    Range(Range<BookmarkKey>),

    /// All bookmarks in the given range from an inclusive start.
    RangeFrom(RangeFrom<BookmarkKey>),

    /// All bookmarks.
    RangeFull(RangeFull),

    /// All bookmarks after the given name (exclusive).
    After(BookmarkKey),

    /// All bookmarks between the given names (exclusive on both sides).
    Between(BookmarkKey, BookmarkKey),

    /// No bookmarks.
    ///
    /// The `RangeBounds` methods must still return a value that
    /// includes a reference to a valid bookmark name, and must
    /// provide a valid range.  To do this, we use an arbitrary
    /// name owned by this `BookmarkPrefixRange`, and return
    /// the half-open range `[name, name)`, which is empty.
    Empty(BookmarkKey),
}

impl BookmarkPrefixRange {
    /// Modify a `BookmarkPrefixRange` to only include bookmarks
    /// after a given bookmark page start (exclusively).
    pub fn with_pagination(self, pagination: BookmarkPagination) -> BookmarkPrefixRange {
        use BookmarkPrefixRange::*;
        let last = BookmarkKey::with_last_name;
        // For `Empty` we only need an arbitrary value, so the last one will do.
        match pagination {
            BookmarkPagination::FromStart => self,
            BookmarkPagination::After(name) => match self {
                Range(r) if name >= *r.end.name() => Empty(last(name)),
                Range(r) if name >= *r.start.name() => Between(last(name), r.end),
                RangeFrom(r) if name >= *r.start.name() => After(last(name)),
                RangeFull(_) => After(last(name)),
                Between(_, e) if name >= *e.name() => Empty(last(name)),
                Between(s, e) if name >= *s.name() => Between(last(name), e),
                After(a) if name >= *a.name() => After(last(name)),
                range => range,
            },
        }
    }
}

impl RangeBounds<BookmarkKey> for BookmarkPrefixRange {
    fn start_bound(&self) -> Bound<&BookmarkKey> {
        use BookmarkPrefixRange::*;
        match self {
            Range(r) => r.start_bound(),
            RangeFrom(r) => r.start_bound(),
            RangeFull(r) => r.start_bound(),
            After(a) => Bound::Excluded(a),
            Between(s, _) => Bound::Excluded(s),
            Empty(n) => Bound::Included(n),
        }
    }

    fn end_bound(&self) -> Bound<&BookmarkKey> {
        use BookmarkPrefixRange::*;
        match self {
            Range(r) => r.end_bound(),
            RangeFrom(r) => r.end_bound(),
            RangeFull(r) => r.end_bound(),
            After(_) => Bound::Unbounded,
            Between(_, e) => Bound::Excluded(e),
            Empty(n) => Bound::Excluded(n),
        }
    }
}

impl BookmarkPrefix {
    pub fn new<B: AsRef<str>>(bookmark_prefix: B) -> Result<Self, Error> {
        Ok(Self {
            bookmark_prefix: AsciiString::from_ascii(bookmark_prefix.as_ref())
                .map_err(|bytes| format_err!("non-ascii bookmark prefix: {bytes:?}"))?,
        })
    }

    pub fn new_ascii(bookmark_prefix: AsciiString) -> Self {
        Self { bookmark_prefix }
    }

    pub fn empty() -> Self {
        Self {
            bookmark_prefix: AsciiString::default(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.bookmark_prefix.is_empty()
    }

    pub fn to_range(&self) -> BookmarkPrefixRange {
        match prefix_to_range_end(self.bookmark_prefix.clone()) {
            Some(range_end) => {
                let range = Range {
                    start: BookmarkKey::with_first_name(BookmarkName::new_ascii(
                        self.bookmark_prefix.clone(),
                    )),
                    end: BookmarkKey::with_first_name(BookmarkName::new_ascii(range_end)),
                };
                BookmarkPrefixRange::Range(range)
            }
            None => match self.bookmark_prefix.len() {
                0 => BookmarkPrefixRange::RangeFull(RangeFull),
                _ => {
                    let range = RangeFrom {
                        start: BookmarkKey::with_first_name(BookmarkName::new_ascii(
                            self.bookmark_prefix.clone(),
                        )),
                    };
                    BookmarkPrefixRange::RangeFrom(range)
                }
            },
        }
    }

    /// Convert the bookmark prefix to an escaped SQL pattern suitable for use
    /// in a LIKE expression.
    ///
    /// For example, `my_prefix` is converted to `my\_prefix%`.
    pub fn to_escaped_sql_like_pattern(&self) -> String {
        let mut like_pattern = String::with_capacity(self.bookmark_prefix.len());
        for ch in self.bookmark_prefix.chars() {
            if ch == '\\' || ch == '%' || ch == '_' {
                like_pattern.push('\\');
            }
            like_pattern.push(ch.into());
        }
        like_pattern.push('%');
        like_pattern
    }

    pub fn is_prefix_of(&self, bookmark: &BookmarkName) -> bool {
        bookmark
            .bookmark
            .as_bytes()
            .starts_with(self.bookmark_prefix.as_bytes())
    }
}

fn prefix_to_range_end(mut prefix: AsciiString) -> Option<AsciiString> {
    // If we have a prefix, then we need to take the last character of the prefix, increment it by
    // 1, then take that as an ASCII char. So, if you prefix is foobarA, then the range will be
    // from foobarA (inclusive) to foobarB (exclusive). Basically, what we're trying to implement
    // here is a little bit like Ruby's str#next.
    loop {
        match prefix.pop() {
            Some(chr) => match AsciiChar::from_ascii(chr.as_byte() + 1) {
                Ok(next_chr) => {
                    // Happy path, we found the next character, so just put that in and move on.
                    prefix.push(next_chr);
                    return Some(prefix);
                }
                Err(_) => {
                    // The last character doesn't fit in ASCII (i.e. it's DEL). This means we have
                    // something like foobaA[DEL]. In this case, we need to set the bound to be the
                    // character after the one before the DEL, so we want foobaB
                    continue;
                }
            },
            None => {
                // We exhausted the entire string. This will only happen if the string is 0 or more
                // DEL characters. In this case, return the fact that no next string can be
                // produced.
                return None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use mononoke_macros::mononoke;
    use quickcheck::quickcheck;

    use super::*;

    #[mononoke::test]
    fn bookmark_kind_all_contains_all_kinds() {
        // Test that `BookmarkKind::ALL` does indeed contain all bookmark
        // kinds.  If you need to add a variant here, make sure you add it
        // to `BookmarkKind::ALL` or this test will fail.
        let mut count = 0;
        for kind in BookmarkKind::ALL.iter() {
            match kind {
                BookmarkKind::Scratch
                | BookmarkKind::Publishing
                | BookmarkKind::PullDefaultPublishing => count += 1,
            }
        }
        assert_eq!(count, BookmarkKind::ALL.len());
    }

    quickcheck! {
        fn test_prefix_range_contains_self(bookmark: Bookmark) -> bool {
            let prefix = BookmarkPrefix::new_ascii(bookmark.name().as_ascii().clone());
            prefix.to_range().contains(bookmark.key())
        }

        fn test_prefix_range_contains_its_suffixes(bookmark: Bookmark, more: ascii_ext::AsciiString) -> bool {
            let prefix = BookmarkPrefix::new_ascii(bookmark.name().as_ascii().clone());
            let mut name = bookmark.name().as_ascii().clone();
            name.push_str(&more.0);
            prefix.to_range().contains(&BookmarkKey::new_ascii(name))
        }

        fn test_prefix_range_does_not_contains_its_prefixes(bookmark: Bookmark, chr: ascii_ext::AsciiChar) -> bool {
            let mut prefix = bookmark.name().as_ascii().clone();
            prefix.push(chr.0);
            let prefix = BookmarkPrefix::new_ascii(prefix);

            !prefix.to_range().contains(bookmark.key())
        }

        fn test_empty_range_contains_any(bookmark: Bookmark) -> bool {
            let prefix = BookmarkPrefix::empty();
            prefix.to_range().contains(bookmark.key())
        }

        fn test_pagination_excludes_start(prefix_char: Option<ascii_ext::AsciiChar>, after: BookmarkName) -> bool {
            let prefix = match prefix_char {
                Some(ch) => BookmarkPrefix::new_ascii(AsciiString::from(&[ch.0][..])),
                None => BookmarkPrefix::empty(),
            };
            let pagination = BookmarkPagination::After(after.clone());
            let after_key = BookmarkKey::with_last_name(after);
            !prefix.to_range().with_pagination(pagination).contains(&after_key)
        }
    }
}
