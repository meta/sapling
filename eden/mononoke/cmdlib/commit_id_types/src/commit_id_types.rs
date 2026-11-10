/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::fmt;
use std::marker::PhantomData;
use std::num::NonZeroU64;

use anyhow::Error;
use anyhow::Result;
use clap::Arg;
use clap::ArgAction;
use clap::ArgGroup;
use clap::ArgMatches;
use clap::builder::ValueRange;
use faster_hex::hex_decode_array;
use faster_hex::hex_string;

pub(crate) const ARG_COMMIT_ID: &str = "commit-id";
pub(crate) const ARG_BOOKMARK: &str = "bookmark";
pub(crate) const ARG_HG_COMMIT_ID: &str = "hg-commit-id";
pub(crate) const ARG_BONSAI_ID: &str = "bonsai-id";
pub(crate) const ARG_GIT_SHA1: &str = "git-sha1";
pub(crate) const ARG_GLOBALREV: &str = "globalrev";
pub(crate) const ARG_SVNREV: &str = "svnrev";
pub(crate) const ARG_P4_CHANGELIST: &str = "p4-changelist";
pub(crate) const ARG_BUBBLE_ID: &str = "bubble-id";
pub(crate) const ARG_SNAPSHOT_ID: &str = "snapshot-id";

// Unfortunately we can't use clap derive API here directly because we care about
// the order of arguments, but we still implement proper clap traits so that it
// can be used in conjunction with derive API in other parts of the code.
#[derive(Clone)]
pub struct CommitIdArgs {
    commit_id: CommitId,
}

impl CommitIdArgs {
    pub fn into_commit_id(self) -> CommitId {
        self.commit_id
    }
}

impl clap::FromArgMatches for CommitIdArgs {
    fn from_arg_matches(matches: &ArgMatches) -> Result<Self, clap::Error> {
        let mut commit_ids = get_commit_ids(matches)?;
        if commit_ids.len() == 1 {
            Ok(CommitIdArgs {
                commit_id: commit_ids.pop().unwrap(),
            })
        } else {
            panic!("expected single commit id")
        }
    }

    fn update_from_arg_matches(&mut self, matches: &ArgMatches) -> Result<(), clap::Error> {
        let mut commit_ids = get_commit_ids(matches)?;
        use std::cmp::Ordering;
        match commit_ids.len().cmp(&1) {
            Ordering::Equal => {
                self.commit_id = commit_ids.pop().unwrap();
            }
            Ordering::Less => {}
            Ordering::Greater => panic!("expected single commit id"),
        }

        Ok(())
    }
}

impl clap::Args for CommitIdArgs {
    fn augment_args(cmd: clap::Command) -> clap::Command {
        add_commit_id_args_impl(cmd, true, false, None)
    }

    fn augment_args_for_update(cmd: clap::Command) -> clap::Command {
        add_commit_id_args_impl(cmd, true, false, None)
    }
}

#[derive(Clone)]
pub struct OptionalCommitIdArgs {
    commit_id: Option<CommitId>,
}

impl OptionalCommitIdArgs {
    pub fn into_commit_id(self) -> Option<CommitId> {
        self.commit_id
    }
}

impl clap::FromArgMatches for OptionalCommitIdArgs {
    fn from_arg_matches(matches: &ArgMatches) -> Result<Self, clap::Error> {
        let commit_ids = get_commit_ids(matches)?;
        Ok(Self {
            commit_id: commit_ids.into_iter().next(),
        })
    }

    fn update_from_arg_matches(&mut self, matches: &ArgMatches) -> Result<(), clap::Error> {
        let commit_ids = get_commit_ids(matches)?;
        self.commit_id = commit_ids.into_iter().next();
        Ok(())
    }
}

impl clap::Args for OptionalCommitIdArgs {
    fn augment_args(cmd: clap::Command) -> clap::Command {
        add_commit_id_args_impl(cmd, false, false, None)
    }

    fn augment_args_for_update(cmd: clap::Command) -> clap::Command {
        add_commit_id_args_impl(cmd, false, false, None)
    }
}

#[derive(Clone)]
/// 0 or more commit ids
pub struct CommitIdsArgs {
    commit_ids: Vec<CommitId>,
}

impl CommitIdsArgs {
    pub fn into_commit_ids(self) -> Vec<CommitId> {
        self.commit_ids
    }
}

impl clap::FromArgMatches for CommitIdsArgs {
    fn from_arg_matches(matches: &ArgMatches) -> Result<Self, clap::Error> {
        let commit_ids = get_commit_ids(matches)?;
        Ok(CommitIdsArgs { commit_ids })
    }

    fn update_from_arg_matches(&mut self, matches: &ArgMatches) -> Result<(), clap::Error> {
        self.commit_ids = get_commit_ids(matches)?;
        Ok(())
    }
}

impl clap::Args for CommitIdsArgs {
    fn augment_args(cmd: clap::Command) -> clap::Command {
        add_commit_id_args_impl(cmd, false, true, None)
    }

    fn augment_args_for_update(cmd: clap::Command) -> clap::Command {
        add_commit_id_args_impl(cmd, false, true, None)
    }
}

pub trait CommitIdNames: Clone {
    // List of named commit ids and associated help text
    const NAMES: &'static [(&'static str, &'static str)];
}

#[derive(Clone, Debug)]
/// 0 or more commits IDs, some with names
///
/// To define the names that are used, implement the `CommitIdNames` trait on a dummy struct.
pub struct NamedCommitIdsArgs<Names: CommitIdNames> {
    /// Positional commit ids
    positional_commit_ids: Vec<CommitId>,

    /// Commit ids with names
    named_commit_ids: HashMap<&'static str, CommitId>,

    phantom: PhantomData<Names>,
}

impl<Names: CommitIdNames> NamedCommitIdsArgs<Names> {
    pub fn positional_commit_ids(&self) -> &[CommitId] {
        &self.positional_commit_ids
    }

    pub fn named_commit_ids(&self) -> &HashMap<&'static str, CommitId> {
        &self.named_commit_ids
    }
}

impl<Names: CommitIdNames> clap::FromArgMatches for NamedCommitIdsArgs<Names> {
    fn from_arg_matches(matches: &ArgMatches) -> Result<Self, clap::Error> {
        let (positional_commit_ids, named_commit_ids) =
            get_commit_ids_with_names::<Names>(matches)?;
        Ok(Self {
            positional_commit_ids,
            named_commit_ids,
            phantom: PhantomData,
        })
    }

    fn update_from_arg_matches(&mut self, matches: &ArgMatches) -> Result<(), clap::Error> {
        let (positional_commit_ids, named_commit_ids) =
            get_commit_ids_with_names::<Names>(matches)?;
        self.positional_commit_ids = positional_commit_ids;
        self.named_commit_ids = named_commit_ids;
        Ok(())
    }
}

impl<Names: CommitIdNames> clap::Args for NamedCommitIdsArgs<Names> {
    fn augment_args(cmd: clap::Command) -> clap::Command {
        add_commit_id_args_impl(cmd, false, true, Some(Names::NAMES))
    }

    fn augment_args_for_update(cmd: clap::Command) -> clap::Command {
        add_commit_id_args_impl(cmd, false, true, Some(Names::NAMES))
    }
}

/// A `CommitId` is any of the ways a user can specify a commit.
#[derive(Clone, Debug)]
pub enum CommitId {
    /// Commit ID is of an unknown type that must be resolved.
    Resolve(String),

    /// Bonsai ID.
    BonsaiId([u8; 32]),

    /// Bonsai ID with bubble
    EphemeralBonsai([u8; 32], Option<NonZeroU64>),

    /// Hg commit ID.
    HgId([u8; 20]),

    /// Git SHA-1.
    GitSha1([u8; 20]),

    /// Globalrev.
    Globalrev(u64),

    /// SVN revision.
    Svnrev(u64),

    /// Perforce changelist number (Perforce mirror repos).
    P4Changelist(u64),

    /// A bookmark name.
    Bookmark(String),
}

impl CommitId {
    pub fn into_bookmark_name(self) -> Result<String, Error> {
        match self {
            Self::Bookmark(name) => Ok(name),
            _ => anyhow::bail!("expected bookmark name (got {self})"),
        }
    }
}

impl fmt::Display for CommitId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            CommitId::Resolve(id) => write!(f, "commit id '{id}'"),
            CommitId::BonsaiId(bonsai) => write!(f, "bonsai id '{}'", hex_string(bonsai)),
            CommitId::EphemeralBonsai(bonsai, bubble_id) => write!(
                f,
                "bonsai id '{}' on bubble {}",
                hex_string(bonsai),
                bubble_id.map_or_else(|| "unknown".to_string(), |id| id.to_string()),
            ),
            CommitId::HgId(id) => write!(f, "hg commit id '{}'", hex_string(id)),
            CommitId::GitSha1(id) => write!(f, "git sha1 '{}'", hex_string(id)),
            CommitId::Globalrev(rev) => write!(f, "globalrev '{rev}'"),
            CommitId::Svnrev(rev) => write!(f, "svn revision '{rev}'"),
            CommitId::P4Changelist(cl) => write!(f, "p4 changelist '{cl}'"),
            CommitId::Bookmark(bookmark) => write!(f, "bookmark '{bookmark}'"),
        }
    }
}

/// Add arguments for specifying commit_ids.
///
/// The user can specify commit_ids by bookmark, hg commit ID, bonsai changeset ID, or
/// a generic "commit_id", for which we will try to work out which kind of identifier
/// the user has provided.
fn add_commit_id_args_impl(
    cmd: clap::Command,
    required: bool,
    multiple: bool,
    names: Option<&'static [(&'static str, &'static str)]>,
) -> clap::Command {
    let num_args: ValueRange = if multiple { (1..).into() } else { 1.into() };
    let mut cmd = cmd
        .arg(
            Arg::new(ARG_COMMIT_ID)
                .short('i')
                .long("commit-id")
                .num_args(num_args)
                .number_of_values(1)
                .action(ArgAction::Append)
                .help("Commit ID to query (bonsai, hg or git), can be a prefix"),
        )
        .arg(
            Arg::new(ARG_BOOKMARK)
                .short('B')
                .long("bookmark")
                .num_args(num_args)
                .action(ArgAction::Append)
                .number_of_values(1)
                .help("Bookmark to query"),
        )
        .arg(
            Arg::new(ARG_HG_COMMIT_ID)
                .long("hg-commit-id")
                .num_args(num_args)
                .action(ArgAction::Append)
                .number_of_values(1)
                .help("Hg commit ID to query"),
        )
        .arg(
            Arg::new(ARG_BONSAI_ID)
                .long("bonsai-id")
                .num_args(num_args)
                .action(ArgAction::Append)
                .number_of_values(1)
                .help("Bonsai ID to query"),
        )
        .arg(
            Arg::new(ARG_SNAPSHOT_ID)
                .long("snapshot-id")
                .num_args(num_args)
                .action(ArgAction::Append)
                .number_of_values(1)
                .help("Snapshot ID to query"),
        )
        .arg(
            Arg::new(ARG_GIT_SHA1)
                .long("git")
                .num_args(num_args)
                .action(ArgAction::Append)
                .number_of_values(1)
                .help("Git SHA-1 to query"),
        )
        .arg(
            Arg::new(ARG_GLOBALREV)
                .long("globalrev")
                .num_args(num_args)
                .action(ArgAction::Append)
                .number_of_values(1)
                .help("Globalrev to query"),
        )
        .arg(
            Arg::new(ARG_SVNREV)
                .long("svnrev")
                .num_args(num_args)
                .action(ArgAction::Append)
                .number_of_values(1)
                .help("SVN revision to query"),
        )
        .arg(
            Arg::new(ARG_P4_CHANGELIST)
                .long("p4-changelist")
                .num_args(num_args)
                .action(ArgAction::Append)
                .number_of_values(1)
                .help("Perforce changelist number to query"),
        )
        .arg(
            Arg::new(ARG_BUBBLE_ID)
                .long("bubble-id")
                .num_args(1)
                .number_of_values(1)
                .help("Bubble id on which to check the bonsai commits"),
        )
        .group(
            ArgGroup::new("commit")
                .args([
                    ARG_COMMIT_ID,
                    ARG_BOOKMARK,
                    ARG_HG_COMMIT_ID,
                    ARG_BONSAI_ID,
                    ARG_SNAPSHOT_ID,
                    ARG_GIT_SHA1,
                    ARG_GLOBALREV,
                    ARG_SVNREV,
                    ARG_P4_CHANGELIST,
                ])
                .multiple(multiple)
                .required(required),
        );

    if let Some(names) = names {
        // Add all the names as flags so that `get_commit_ids_with_names` can find them.
        for (name, help) in names {
            cmd = cmd.arg(
                Arg::new(name)
                    .long(name)
                    .help(help)
                    .action(ArgAction::SetTrue),
            );
        }
    }

    cmd
}

/// Get commit IDs that are exclusively positional.  Returns the commit IDs in the order the user
/// specified them on the command line (even if they are specified in different ways).
fn get_commit_ids(matches: &ArgMatches) -> Result<Vec<CommitId>, clap::Error> {
    get_commit_ids_impl(matches)
        .map_err(|err| clap::Error::raw(clap::error::ErrorKind::ValueValidation, err))
        .map(|commit_ids| commit_ids.into_values().collect())
}

/// Get commit IDs that may include commit IDs with names.
///
/// Commits are specified by command line arguments of different types (e.g `-B` for bookmark,
/// `--hg-commit-id` for Mercurial, etc.).
///
/// If we want to allow the user to specify multiple different commits for different reasons,
/// we need a second layer of argument flag, which clap doesn't natively support.  Instead, we
/// emulate this by adding the names as flag arguments, and associating commit specifiers with
/// their nearest preceding name flag.  If a commit specifier doesn't have a preceding name
/// flag, then it is a positional commit id.
///
/// The names are specified by implementing the `CommitIdNames` trait.
fn get_commit_ids_with_names<Names: CommitIdNames>(
    matches: &ArgMatches,
) -> Result<(Vec<CommitId>, HashMap<&'static str, CommitId>), clap::Error> {
    let commit_ids = get_commit_ids_impl(matches)
        .map_err(|err| clap::Error::raw(clap::error::ErrorKind::ValueValidation, err))?;
    let mut positional_commit_ids = Vec::new();
    let mut named_commit_ids = HashMap::new();
    let mut name_indexes = BTreeMap::new();

    // Collect the locations of the name flags, if they have been specified.
    for (name, _help) in Names::NAMES {
        if matches.get_flag(name) {
            if let Some(index) = matches.index_of(name) {
                name_indexes.insert(index, *name);
            }
        }
    }

    // Allocate commit ids to their nearest name flag.  If there is no such flag, it is a positional commit id.
    for (index, commit_id) in commit_ids.into_iter() {
        let remaining_name_indexes = name_indexes.split_off(&index);
        if let Some((_, name)) = name_indexes.pop_last() {
            named_commit_ids.insert(name, commit_id);
        } else {
            positional_commit_ids.push(commit_id);
        }
        // If we still have a name index left, then the user has specified a name
        // flag without specifying a commit id.  This is an error.
        if let Some((_, name)) = name_indexes.pop_first() {
            return Err(clap::Error::raw(
                clap::error::ErrorKind::ValueValidation,
                format!("--{name} should be followed by a commit specifier, e.g. --{name} -i HASH"),
            ));
        }
        name_indexes = remaining_name_indexes;
    }
    // If we still have a name index left, then the user has specified a name
    // flag without specifying a commit id.  This is an error.
    if let Some((_, name)) = name_indexes.pop_first() {
        return Err(clap::Error::raw(
            clap::error::ErrorKind::ValueValidation,
            format!("--{name} should be followed by a commit specifier, e.g. --{name} -i HASH"),
        ));
    }

    Ok((positional_commit_ids, named_commit_ids))
}

/// Get the commit ids specified by the user.  They are returned in the order specified.
fn get_commit_ids_impl(matches: &ArgMatches) -> Result<BTreeMap<usize, CommitId>, Error> {
    let bubble_id = matches
        .get_one::<String>(ARG_BUBBLE_ID)
        .map(|id| id.parse::<NonZeroU64>())
        .transpose()?;
    let mut commit_ids = BTreeMap::new();
    if let (Some(indices), Some(values)) = (
        matches.indices_of(ARG_BOOKMARK),
        matches.get_many::<String>(ARG_BOOKMARK),
    ) {
        for (index, value) in indices.zip(values) {
            commit_ids.insert(index, CommitId::Bookmark(value.clone()));
        }
    }
    if let (Some(indices), Some(values)) = (
        matches.indices_of(ARG_HG_COMMIT_ID),
        matches.get_many::<String>(ARG_HG_COMMIT_ID),
    ) {
        for (index, value) in indices.zip(values) {
            let id = hex_decode_array::<20>(value.as_bytes())?;
            commit_ids.insert(index, CommitId::HgId(id));
        }
    }
    if let (Some(indices), Some(values)) = (
        matches.indices_of(ARG_BONSAI_ID),
        matches.get_many::<String>(ARG_BONSAI_ID),
    ) {
        for (index, value) in indices.zip(values) {
            let id = hex_decode_array::<32>(value.as_bytes())?;
            commit_ids.insert(index, CommitId::BonsaiId(id));
        }
    }
    if let (Some(indices), Some(values)) = (
        matches.indices_of(ARG_SNAPSHOT_ID),
        matches.get_many::<String>(ARG_SNAPSHOT_ID),
    ) {
        for (index, value) in indices.zip(values) {
            let id = hex_decode_array::<32>(value.as_bytes())?;
            commit_ids.insert(index, CommitId::EphemeralBonsai(id, bubble_id));
        }
    }
    if let (Some(indices), Some(values)) = (
        matches.indices_of(ARG_GIT_SHA1),
        matches.get_many::<String>(ARG_GIT_SHA1),
    ) {
        for (index, value) in indices.zip(values) {
            let hash = hex_decode_array::<20>(value.as_bytes())?;
            commit_ids.insert(index, CommitId::GitSha1(hash));
        }
    }
    if let (Some(indices), Some(values)) = (
        matches.indices_of(ARG_GLOBALREV),
        matches.get_many::<String>(ARG_GLOBALREV),
    ) {
        for (index, value) in indices.zip(values) {
            commit_ids.insert(index, CommitId::Globalrev(value.parse::<u64>()?));
        }
    }
    if let (Some(indices), Some(values)) = (
        matches.indices_of(ARG_SVNREV),
        matches.get_many::<String>(ARG_SVNREV),
    ) {
        for (index, value) in indices.zip(values) {
            commit_ids.insert(index, CommitId::Svnrev(value.parse::<u64>()?));
        }
    }
    if let (Some(indices), Some(values)) = (
        matches.indices_of(ARG_P4_CHANGELIST),
        matches.get_many::<String>(ARG_P4_CHANGELIST),
    ) {
        for (index, value) in indices.zip(values) {
            commit_ids.insert(index, CommitId::P4Changelist(value.parse::<u64>()?));
        }
    }
    if let (Some(indices), Some(values)) = (
        matches.indices_of(ARG_COMMIT_ID),
        matches.get_many::<String>(ARG_COMMIT_ID),
    ) {
        for (index, value) in indices.zip(values) {
            commit_ids.insert(index, CommitId::Resolve(value.to_string()));
        }
    }
    Ok(commit_ids)
}
