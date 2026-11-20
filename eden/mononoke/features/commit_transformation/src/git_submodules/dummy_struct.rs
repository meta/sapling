/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use ::sql_ext::Transaction;
use anyhow::Error;
use anyhow::Result;
use async_trait::async_trait;
use bonsai_git_mapping::AddGitMappingErrorKind;
use bonsai_git_mapping::BonsaiGitMapping;
use bonsai_git_mapping::BonsaiGitMappingEntry;
use bonsai_git_mapping::BonsaisOrGitShas;
use context::CoreContext;
use filenodes::FilenodeInfo;
use filenodes::FilenodeRange;
use filenodes::FilenodeResult;
use filenodes::Filenodes;
use filenodes::PreparedFilenode;
use mercurial_types::HgFileNodeId;
use mononoke_types::RepoPath;
use mononoke_types::RepositoryId;
use mononoke_types::hash::GitSha1;

/// Struct created to satisfy the type system when creating a `RepoDerivedData`
/// for the `InMemoryRepo`.
pub(crate) struct DummyStruct;

#[async_trait]
impl Filenodes for DummyStruct {
    async fn add_filenodes(
        &self,
        _ctx: &CoreContext,
        _info: Vec<PreparedFilenode>,
    ) -> Result<FilenodeResult<()>> {
        unimplemented!()
    }

    async fn add_or_replace_filenodes(
        &self,
        _ctx: &CoreContext,
        _info: Vec<PreparedFilenode>,
    ) -> Result<FilenodeResult<()>> {
        unimplemented!()
    }

    async fn get_filenode(
        &self,
        _ctx: &CoreContext,
        _path: &RepoPath,
        _filenode: HgFileNodeId,
    ) -> Result<FilenodeResult<Option<FilenodeInfo>>> {
        unimplemented!()
    }

    async fn get_all_filenodes_maybe_stale(
        &self,
        _ctx: &CoreContext,
        _path: &RepoPath,
        _limit: Option<u64>,
    ) -> Result<FilenodeResult<FilenodeRange>> {
        unimplemented!()
    }
}

#[async_trait]
impl BonsaiGitMapping for DummyStruct {
    fn repo_id(&self) -> RepositoryId {
        unimplemented!()
    }

    async fn add(
        &self,
        _ctx: &CoreContext,
        _entry: BonsaiGitMappingEntry,
    ) -> Result<(), AddGitMappingErrorKind> {
        unimplemented!()
    }

    async fn bulk_add(
        &self,
        _ctx: &CoreContext,
        _entries: &[BonsaiGitMappingEntry],
    ) -> Result<(), AddGitMappingErrorKind> {
        unimplemented!()
    }

    async fn bulk_add_git_mapping_in_transaction(
        &self,
        _ctx: &CoreContext,
        _entries: &[BonsaiGitMappingEntry],
        _transaction: Transaction,
    ) -> Result<Transaction, AddGitMappingErrorKind> {
        unimplemented!()
    }

    async fn get(
        &self,
        _ctx: &CoreContext,
        _cs: BonsaisOrGitShas,
    ) -> Result<Vec<BonsaiGitMappingEntry>, Error> {
        unimplemented!()
    }

    /// Use caching for the ranges of one element, use slower path otherwise.
    async fn get_in_range(
        &self,
        _ctx: &CoreContext,
        _low: GitSha1,
        _high: GitSha1,
        _limit: usize,
    ) -> Result<Vec<GitSha1>, Error> {
        unimplemented!()
    }
}
