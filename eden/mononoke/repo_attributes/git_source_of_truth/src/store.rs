/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use async_trait::async_trait;
use context::CoreContext;
use metaconfig_types::OssRemoteDatabaseConfig;
use metaconfig_types::OssRemoteMetadataDatabaseConfig;
use metaconfig_types::RemoteDatabaseConfig;
use metaconfig_types::RemoteMetadataDatabaseConfig;
use mononoke_types::RepositoryId;
use sql_construct::SqlConstruct;
use sql_construct::SqlConstructFromMetadataDatabaseConfig;
use sql_ext::Connection;
use sql_ext::SqlConnections;
use sql_ext::mononoke_queries;

use crate::GitSourceOfTruth;
use crate::GitSourceOfTruthConfig;
use crate::GitSourceOfTruthConfigEntry;
use crate::RepositoryName;
use crate::RowId;
use crate::Staleness;

mononoke_queries! {
    read TestGet(id: RowId) -> (
        RowId,
        RepositoryId,
        RepositoryName,
        GitSourceOfTruth,
        Option<i64>,
        Option<String>,
    ) {
        "SELECT id,
            repo_id,
            repo_name,
            source_of_truth,
            mutation_id,
            storage_config
         FROM git_repositories_source_of_truth
         WHERE id = {id}"
    }

    read GetByRepoName(repo_name: RepositoryName) -> (
        RowId,
        RepositoryId,
        RepositoryName,
        GitSourceOfTruth,
        Option<i64>,
        Option<String>,
    ) {
        "SELECT id,
            repo_id,
            repo_name,
            source_of_truth,
            mutation_id,
            storage_config
         FROM git_repositories_source_of_truth
         WHERE repo_name = {repo_name}"
    }

    read GetMaxId() -> (
        RepositoryId,
    ) {
        "SELECT COALESCE(MAX(repo_id), 0) FROM git_repositories_source_of_truth"
    }

    // One statement, not a loop: auto-increment is contiguous within an INSERT
    // but not across two, and batches reach 1000 on the SourceControl tier.
    write AllocateRepoIds(values: (repo_id: Option<i64>)) {
        none,
        "INSERT INTO repo_id_sequence (repo_id) VALUES {values}"
    }

    // MySQL reports the first id of a multi-row insert, SQLite the last;
    // `ROW_COUNT()` puts both on the last so callers need no dialect branch.
    // Per-connection, so it must share the INSERT's transaction.
    read ReadLastAllocatedRepoId() -> (i64) {
        mysql("SELECT LAST_INSERT_ID() + ROW_COUNT() - 1")
        sqlite("SELECT last_insert_rowid()")
    }

    // Insert-or-ignore, so re-seeding is a no-op.
    write SeedRepoIdSequence(repo_id: i64) {
        insert_or_ignore,
        "{insert_or_ignore} INTO repo_id_sequence (repo_id) VALUES ({repo_id})"
    }

    read ReadMaxRepoIdInSequence() -> (Option<i64>) {
        "SELECT MAX(repo_id) FROM repo_id_sequence"
    }

    read GetByGitSourceOfTruth(source_of_truth: GitSourceOfTruth) -> (
        RowId,
        RepositoryId,
        RepositoryName,
        GitSourceOfTruth,
        Option<i64>,
        Option<String>,
    ) {
        "SELECT id,
            repo_id,
            repo_name,
            source_of_truth,
            mutation_id,
            storage_config
         FROM git_repositories_source_of_truth
         WHERE source_of_truth = {source_of_truth}"
    }

    read GetByGitSourceOfTruthAndMutationId(
        source_of_truth: GitSourceOfTruth,
        mutation_id: i64,
    ) -> (
        RowId,
        RepositoryId,
        RepositoryName,
        GitSourceOfTruth,
        Option<i64>,
        Option<String>,
    ) {
        "SELECT id,
            repo_id,
            repo_name,
            source_of_truth,
            mutation_id,
            storage_config
         FROM git_repositories_source_of_truth
         WHERE source_of_truth = {source_of_truth} AND mutation_id = {mutation_id}"
    }

    read GetAny() -> (
        RowId,
        RepositoryId,
        RepositoryName,
        GitSourceOfTruth,
        Option<i64>,
        Option<String>,
    ) {
        "SELECT id,
            repo_id,
            repo_name,
            source_of_truth,
            mutation_id,
            storage_config
         FROM git_repositories_source_of_truth"
    }

    write InsertOrUpdateRepo(repo_id: RepositoryId, repo_name: RepositoryName, source_of_truth: GitSourceOfTruth) {
        none,
        mysql("INSERT INTO git_repositories_source_of_truth (repo_id, repo_name, source_of_truth) VALUES ({repo_id}, {repo_name}, {source_of_truth}) ON DUPLICATE KEY UPDATE source_of_truth = {source_of_truth}")
        sqlite("REPLACE INTO git_repositories_source_of_truth (repo_id, repo_name, source_of_truth) VALUES ({repo_id}, {repo_name}, {source_of_truth})")
    }

    write InsertRepos(
        values: (repo_id: RepositoryId, repo_name: RepositoryName, source_of_truth: GitSourceOfTruth, storage_config: Option<String>)
    ) {
        none,
        "INSERT INTO git_repositories_source_of_truth (repo_id, repo_name, source_of_truth, storage_config) VALUES {values}"
    }

    write UpdateSourceOfTruthByRepoNames(
        source_of_truth: GitSourceOfTruth,
        >list repo_names: RepositoryName
    ) {
        none,
        "UPDATE git_repositories_source_of_truth
         SET source_of_truth = {source_of_truth}
         WHERE repo_name IN {repo_names}"
    }

    write UpdateSourceOfTruthByMutationId(
        source_of_truth: GitSourceOfTruth,
        mutation_id: i64,
    ) {
        none,
        "UPDATE git_repositories_source_of_truth
         SET source_of_truth = {source_of_truth}
         WHERE mutation_id = {mutation_id}"
    }

    write UpdateMutationIdByRepoNames(
        source_of_truth: GitSourceOfTruth,
        mutation_id: i64,
        >list repo_names: RepositoryName
    ) {
        none,
        "UPDATE git_repositories_source_of_truth
         SET mutation_id = {mutation_id}
         WHERE repo_name IN {repo_names} AND source_of_truth = {source_of_truth}"
    }

    write DeleteSourceOfTruthByRepoNames(
        source_of_truth: GitSourceOfTruth,
        >list repo_names: RepositoryName
    ) {
        none,
        "DELETE FROM git_repositories_source_of_truth
         WHERE source_of_truth = {source_of_truth}
         AND repo_name IN {repo_names}"
    }

    write DeleteSourceOfTruthByMutationId(
        source_of_truth: GitSourceOfTruth,
        mutation_id: &i64,
    ) {
        none,
        "DELETE FROM git_repositories_source_of_truth
         WHERE source_of_truth = {source_of_truth}
         AND mutation_id = {mutation_id}"
    }

    write DeleteReservedRowStamped(
        id: RowId,
        source_of_truth: GitSourceOfTruth,
        mutation_id: i64,
    ) {
        none,
        "DELETE FROM git_repositories_source_of_truth
         WHERE id = {id}
         AND source_of_truth = {source_of_truth}
         AND mutation_id = {mutation_id}"
    }

    write DeleteReservedRowUnstamped(
        id: RowId,
        source_of_truth: GitSourceOfTruth,
    ) {
        none,
        "DELETE FROM git_repositories_source_of_truth
         WHERE id = {id}
         AND source_of_truth = {source_of_truth}
         AND mutation_id IS NULL"
    }

}

fn row_to_entry(
    row: (
        RowId,
        RepositoryId,
        RepositoryName,
        GitSourceOfTruth,
        Option<i64>,
        Option<String>,
    ),
) -> GitSourceOfTruthConfigEntry {
    let (id, repo_id, repo_name, source_of_truth, mutation_id, storage_config) = row;
    GitSourceOfTruthConfigEntry {
        id,
        repo_id,
        repo_name,
        source_of_truth,
        mutation_id,
        storage_config,
    }
}

pub struct SqlGitSourceOfTruthConfig {
    connections: SqlConnections,
}

impl SqlGitSourceOfTruthConfig {
    pub fn get_connection(&self, staleness: Staleness) -> &Connection {
        match staleness {
            Staleness::MostRecent => &self.connections.read_master_connection,
            Staleness::MaybeStale => &self.connections.read_connection,
        }
    }

    /// Plant `repo_id` so the next allocation lands above it. Production seeds
    /// once via the MySQL schema; this is for tests and admin tooling.
    pub async fn seed_repo_id_sequence(&self, ctx: &CoreContext, repo_id: i64) -> Result<()> {
        SeedRepoIdSequence::query(
            &self.connections.write_connection,
            ctx.sql_query_telemetry(),
            &repo_id,
        )
        .await?;
        Ok(())
    }
}

#[derive(Clone)]
pub struct SqlGitSourceOfTruthConfigBuilder {
    connections: SqlConnections,
}

impl SqlConstruct for SqlGitSourceOfTruthConfigBuilder {
    const LABEL: &'static str = "git_repositories_source_of_truth";

    const CREATION_QUERY: &'static str =
        include_str!("../schemas/sqlite-git-repositories-source-of-truth.sql");

    fn from_sql_connections(connections: SqlConnections) -> Self {
        Self { connections }
    }
}

impl SqlGitSourceOfTruthConfigBuilder {
    pub fn build(self) -> SqlGitSourceOfTruthConfig {
        let SqlGitSourceOfTruthConfigBuilder { connections } = self;

        SqlGitSourceOfTruthConfig { connections }
    }
}

impl SqlConstructFromMetadataDatabaseConfig for SqlGitSourceOfTruthConfigBuilder {
    fn remote_database_config(
        remote: &RemoteMetadataDatabaseConfig,
    ) -> Option<&RemoteDatabaseConfig> {
        Some(&remote.production)
    }
    fn oss_remote_database_config(
        remote: &OssRemoteMetadataDatabaseConfig,
    ) -> Option<&OssRemoteDatabaseConfig> {
        Some(&remote.production)
    }
}

#[async_trait]
impl GitSourceOfTruthConfig for SqlGitSourceOfTruthConfig {
    async fn insert_or_update_repo(
        &self,
        ctx: &CoreContext,
        repo_id: RepositoryId,
        repo_name: RepositoryName,
        source_of_truth: GitSourceOfTruth,
    ) -> Result<()> {
        InsertOrUpdateRepo::query(
            &self.connections.write_connection,
            ctx.sql_query_telemetry(),
            &repo_id,
            &repo_name,
            &source_of_truth,
        )
        .await?;
        Ok(())
    }

    async fn insert_repos(
        &self,
        ctx: &CoreContext,
        repos: &[(
            RepositoryId,
            RepositoryName,
            GitSourceOfTruth,
            Option<String>,
        )],
    ) -> Result<()> {
        let repos_refs = repos
            .iter()
            .map(|(a, b, c, d)| (a, b, c, d))
            .collect::<Vec<_>>();
        InsertRepos::query(
            &self.connections.write_connection,
            ctx.sql_query_telemetry(),
            &repos_refs,
        )
        .await?;
        Ok(())
    }

    async fn update_source_of_truth_by_repo_names(
        &self,
        ctx: &CoreContext,
        source_of_truth: GitSourceOfTruth,
        repo_names: &[RepositoryName],
    ) -> Result<()> {
        UpdateSourceOfTruthByRepoNames::query(
            &self.connections.write_connection,
            ctx.sql_query_telemetry(),
            &source_of_truth,
            repo_names,
        )
        .await?;
        Ok(())
    }

    async fn update_source_of_truth_by_mutation_id(
        &self,
        ctx: &CoreContext,
        source_of_truth: GitSourceOfTruth,
        mutation_id: i64,
    ) -> Result<()> {
        UpdateSourceOfTruthByMutationId::query(
            &self.connections.write_connection,
            ctx.sql_query_telemetry(),
            &source_of_truth,
            &mutation_id,
        )
        .await?;
        Ok(())
    }

    async fn update_mutation_id_by_repo_names_for_reserved_repos(
        &self,
        ctx: &CoreContext,
        repo_names: &[RepositoryName],
        mutation_id: i64,
    ) -> Result<u64> {
        let result = UpdateMutationIdByRepoNames::query(
            &self.connections.write_connection,
            ctx.sql_query_telemetry(),
            &GitSourceOfTruth::Reserved,
            &mutation_id,
            repo_names,
        )
        .await?;
        Ok(result.affected_rows())
    }

    async fn delete_source_of_truth_by_repo_names_for_reserved_repos(
        &self,
        ctx: &CoreContext,
        repo_names: &[RepositoryName],
    ) -> Result<()> {
        DeleteSourceOfTruthByRepoNames::query(
            &self.connections.write_connection,
            ctx.sql_query_telemetry(),
            &GitSourceOfTruth::Reserved,
            repo_names,
        )
        .await?;
        Ok(())
    }

    async fn delete_source_of_truth_for_mutation_id(
        &self,
        ctx: &CoreContext,
        mutation_id: &i64,
    ) -> Result<()> {
        DeleteSourceOfTruthByMutationId::query(
            &self.connections.write_connection,
            ctx.sql_query_telemetry(),
            &GitSourceOfTruth::Reserved,
            &mutation_id,
        )
        .await?;
        Ok(())
    }

    async fn get_by_repo_name(
        &self,
        ctx: &CoreContext,
        repo_name: &RepositoryName,
        staleness: Staleness,
    ) -> Result<Option<GitSourceOfTruthConfigEntry>> {
        let rows = GetByRepoName::query(
            self.get_connection(staleness),
            ctx.sql_query_telemetry(),
            repo_name,
        )
        .await?;
        Ok(rows.into_iter().next().map(row_to_entry))
    }

    async fn get_redirected_to_mononoke(
        &self,
        ctx: &CoreContext,
    ) -> Result<Vec<GitSourceOfTruthConfigEntry>> {
        let rows = GetByGitSourceOfTruth::query(
            &self.connections.read_master_connection,
            ctx.sql_query_telemetry(),
            &GitSourceOfTruth::Mononoke,
        )
        .await?;
        Ok(rows.into_iter().map(row_to_entry).collect())
    }

    async fn get_redirected_to_metagit(
        &self,
        ctx: &CoreContext,
    ) -> Result<Vec<GitSourceOfTruthConfigEntry>> {
        let rows = GetByGitSourceOfTruth::query(
            &self.connections.read_master_connection,
            ctx.sql_query_telemetry(),
            &GitSourceOfTruth::Metagit,
        )
        .await?;
        Ok(rows.into_iter().map(row_to_entry).collect())
    }

    async fn get_locked(&self, ctx: &CoreContext) -> Result<Vec<GitSourceOfTruthConfigEntry>> {
        let rows = GetByGitSourceOfTruth::query(
            &self.connections.read_master_connection,
            ctx.sql_query_telemetry(),
            &GitSourceOfTruth::Locked,
        )
        .await?;
        Ok(rows.into_iter().map(row_to_entry).collect())
    }

    async fn get_reserved(&self, ctx: &CoreContext) -> Result<Vec<GitSourceOfTruthConfigEntry>> {
        let rows = GetByGitSourceOfTruth::query(
            &self.connections.read_master_connection,
            ctx.sql_query_telemetry(),
            &GitSourceOfTruth::Reserved,
        )
        .await?;
        Ok(rows.into_iter().map(row_to_entry).collect())
    }

    async fn get_reserved_by_mutation_id(
        &self,
        ctx: &CoreContext,
        mutation_id: i64,
    ) -> Result<Vec<GitSourceOfTruthConfigEntry>> {
        let rows = GetByGitSourceOfTruthAndMutationId::query(
            &self.connections.read_master_connection,
            ctx.sql_query_telemetry(),
            &GitSourceOfTruth::Reserved,
            &mutation_id,
        )
        .await?;
        Ok(rows.into_iter().map(row_to_entry).collect())
    }

    async fn get_redirected_to_mononoke_by_mutation_id(
        &self,
        ctx: &CoreContext,
        mutation_id: i64,
    ) -> Result<Vec<GitSourceOfTruthConfigEntry>> {
        let rows = GetByGitSourceOfTruthAndMutationId::query(
            &self.connections.read_master_connection,
            ctx.sql_query_telemetry(),
            &GitSourceOfTruth::Mononoke,
            &mutation_id,
        )
        .await?;
        Ok(rows.into_iter().map(row_to_entry).collect())
    }

    async fn get_any(&self, ctx: &CoreContext) -> Result<Vec<GitSourceOfTruthConfigEntry>> {
        let rows = GetAny::query(
            &self.connections.read_master_connection,
            ctx.sql_query_telemetry(),
        )
        .await?;
        Ok(rows.into_iter().map(row_to_entry).collect())
    }

    async fn delete_reserved_row(
        &self,
        ctx: &CoreContext,
        id: RowId,
        mutation_id: Option<i64>,
    ) -> Result<u64> {
        let result = match mutation_id {
            Some(mutation_id) => {
                DeleteReservedRowStamped::query(
                    &self.connections.write_connection,
                    ctx.sql_query_telemetry(),
                    &id,
                    &GitSourceOfTruth::Reserved,
                    &mutation_id,
                )
                .await?
            }
            None => {
                DeleteReservedRowUnstamped::query(
                    &self.connections.write_connection,
                    ctx.sql_query_telemetry(),
                    &id,
                    &GitSourceOfTruth::Reserved,
                )
                .await?
            }
        };
        Ok(result.affected_rows())
    }

    async fn get_max_id(&self, ctx: &CoreContext) -> Result<Option<RepositoryId>> {
        let from_db = GetMaxId::query(
            &self.connections.read_master_connection,
            ctx.sql_query_telemetry(),
        )
        .await?
        .first()
        .map(|(id,)| *id);
        if let Some(repo_id) = from_db
            && repo_id == RepositoryId::new(0)
        {
            Ok(None)
        } else {
            Ok(from_db)
        }
    }

    async fn allocate_repo_ids(
        &self,
        ctx: &CoreContext,
        count: usize,
    ) -> Result<Vec<RepositoryId>> {
        if count == 0 {
            return Ok(vec![]);
        }

        // The transaction pins INSERT and read to one connection; from the pool
        // they could land apart and read another allocation's id.
        let txn = self
            .connections
            .write_connection
            .start_transaction(ctx.sql_query_telemetry())
            .await?;

        // The seed is a schema change landing separately. Unseeded, this starts
        // at 1 and collides with every live repo, so refuse instead.
        let (txn, seeded) = ReadMaxRepoIdInSequence::query_with_transaction(txn).await?;
        if seeded.first().and_then(|(max,)| *max).is_none() {
            return Err(anyhow!(
                "repo_id_sequence is empty; it must be seeded above every repo id \
                 that has ever existed before repo creation can allocate from it"
            ));
        }

        let placeholder: Option<i64> = None;
        let new_rows = vec![(&placeholder,); count];
        let (txn, _) = AllocateRepoIds::query_with_transaction(txn, &new_rows).await?;

        let count = i64::try_from(count).context("repo id batch size does not fit in i64")?;
        let (txn, rows) = ReadLastAllocatedRepoId::query_with_transaction(txn).await?;
        txn.commit().await?;

        let last = rows
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("ReadLastAllocatedRepoId returned no rows"))?
            .0;
        let first = last - count + 1;

        (first..=last)
            .map(|id| {
                let id = i32::try_from(id)
                    .with_context(|| format!("allocated repo id {id} does not fit in i32"))?;
                Ok(RepositoryId::new(id))
            })
            .collect()
    }
}

#[cfg(all(fbcode_build, test))]
mod test {
    use fbinit::FacebookInit;
    use mononoke_macros::mononoke;

    use super::*;

    #[mononoke::fbinit_test]
    async fn test_insert_or_update_repo(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let builder = SqlGitSourceOfTruthConfigBuilder::with_sqlite_in_memory()?;
        let push = builder.clone().build();

        // insert one
        let repo_id = RepositoryId::new(1);
        let repo_name = RepositoryName("test1".to_string());
        push.insert_or_update_repo(&ctx, repo_id, repo_name.clone(), GitSourceOfTruth::Mononoke)
            .await?;
        let entry = push
            .get_by_repo_name(&ctx, &repo_name, Staleness::MostRecent)
            .await?;
        assert!(entry.is_some());
        let entry = entry.unwrap();
        assert_eq!(entry.source_of_truth, GitSourceOfTruth::Mononoke);

        let push = builder.build();

        // insert another
        let repo_id = RepositoryId::new(2);
        let repo_name = RepositoryName("test2".to_string());
        push.insert_or_update_repo(&ctx, repo_id, repo_name.clone(), GitSourceOfTruth::Metagit)
            .await?;
        let entry = push
            .get_by_repo_name(&ctx, &repo_name, Staleness::MostRecent)
            .await?;
        assert!(entry.is_some());
        let entry = entry.unwrap();
        assert_eq!(entry.source_of_truth, GitSourceOfTruth::Metagit);

        // update it
        push.insert_or_update_repo(&ctx, repo_id, repo_name.clone(), GitSourceOfTruth::Mononoke)
            .await?;
        let entry = push
            .get_by_repo_name(&ctx, &repo_name, Staleness::MostRecent)
            .await?;
        assert!(entry.is_some());
        let entry = entry.unwrap();
        assert_eq!(entry.source_of_truth, GitSourceOfTruth::Mononoke);

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_insert_repos(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let builder = SqlGitSourceOfTruthConfigBuilder::with_sqlite_in_memory()?;
        let push = builder.clone().build();

        // insert many
        push.insert_repos(
            &ctx,
            &[
                (
                    RepositoryId::new(1),
                    RepositoryName("test1".to_string()),
                    GitSourceOfTruth::Mononoke,
                    None,
                ),
                (
                    RepositoryId::new(2),
                    RepositoryName("test2".to_string()),
                    GitSourceOfTruth::Metagit,
                    None,
                ),
                (
                    RepositoryId::new(3),
                    RepositoryName("test3".to_string()),
                    GitSourceOfTruth::Mononoke,
                    None,
                ),
            ],
        )
        .await?;

        // get each and confirm it worked
        let entry = push
            .get_by_repo_name(
                &ctx,
                &RepositoryName("test1".to_string()),
                Staleness::MostRecent,
            )
            .await?;
        assert!(entry.is_some());
        let entry = entry.unwrap();
        assert_eq!(entry.source_of_truth, GitSourceOfTruth::Mononoke);

        let entry = push
            .get_by_repo_name(
                &ctx,
                &RepositoryName("test2".to_string()),
                Staleness::MostRecent,
            )
            .await?;
        assert!(entry.is_some());
        let entry = entry.unwrap();
        assert_eq!(entry.source_of_truth, GitSourceOfTruth::Metagit);

        let entry = push
            .get_by_repo_name(
                &ctx,
                &RepositoryName("test3".to_string()),
                Staleness::MostRecent,
            )
            .await?;
        assert!(entry.is_some());
        let entry = entry.unwrap();
        assert_eq!(entry.source_of_truth, GitSourceOfTruth::Mononoke);

        // update many should fail if any id is not unique as we don't allow duplicate ids
        let result = push
            .insert_repos(
                &ctx,
                &[
                    (
                        RepositoryId::new(2),
                        RepositoryName("test2_renamed".to_string()),
                        GitSourceOfTruth::Mononoke,
                        None,
                    ),
                    (
                        RepositoryId::new(4),
                        RepositoryName("test4".to_string()),
                        GitSourceOfTruth::Metagit,
                        None,
                    ),
                ],
            )
            .await;
        assert!(result.is_err());
        let error_trace = format!("{:#}", result.unwrap_err());
        assert!(error_trace.contains("UNIQUE constraint failed"));
        assert!(error_trace.contains("git_repositories_source_of_truth.repo_id"));

        // update many should fail if any name is not unique
        let result = push
            .insert_repos(
                &ctx,
                &[(
                    RepositoryId::new(4),
                    RepositoryName("test2".to_string()),
                    GitSourceOfTruth::Mononoke,
                    None,
                )],
            )
            .await;
        assert!(result.is_err());
        let error_trace = format!("{:#}", result.unwrap_err());
        assert!(error_trace.contains("UNIQUE constraint failed"));
        assert!(error_trace.contains("git_repositories_source_of_truth.repo_name"));

        // insert many new repos should succeed
        push.insert_repos(
            &ctx,
            &[
                (
                    RepositoryId::new(4),
                    RepositoryName("test4".to_string()),
                    GitSourceOfTruth::Mononoke,
                    None,
                ),
                (
                    RepositoryId::new(5),
                    RepositoryName("test5".to_string()),
                    GitSourceOfTruth::Metagit,
                    None,
                ),
            ],
        )
        .await?;

        // get each and confirm it worked
        let entry = push
            .get_by_repo_name(
                &ctx,
                &RepositoryName("test4".to_string()),
                Staleness::MostRecent,
            )
            .await?;
        assert!(entry.is_some());
        let entry = entry.unwrap();
        assert_eq!(entry.source_of_truth, GitSourceOfTruth::Mononoke);

        let entry = push
            .get_by_repo_name(
                &ctx,
                &RepositoryName("test5".to_string()),
                Staleness::MostRecent,
            )
            .await?;
        assert!(entry.is_some());
        let entry = entry.unwrap();
        assert_eq!(entry.source_of_truth, GitSourceOfTruth::Metagit);

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_update_source_of_truth_by_repo_names(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let builder = SqlGitSourceOfTruthConfigBuilder::with_sqlite_in_memory()?;
        let push = builder.clone().build();

        // insert many
        push.insert_repos(
            &ctx,
            &[
                (
                    RepositoryId::new(1),
                    RepositoryName("test1".to_string()),
                    GitSourceOfTruth::Mononoke,
                    None,
                ),
                (
                    RepositoryId::new(2),
                    RepositoryName("test2".to_string()),
                    GitSourceOfTruth::Metagit,
                    None,
                ),
                (
                    RepositoryId::new(3),
                    RepositoryName("test3".to_string()),
                    GitSourceOfTruth::Mononoke,
                    None,
                ),
                (
                    RepositoryId::new(4),
                    RepositoryName("test4".to_string()),
                    GitSourceOfTruth::Mononoke,
                    None,
                ),
            ],
        )
        .await?;

        push.update_source_of_truth_by_repo_names(
            &ctx,
            GitSourceOfTruth::Metagit,
            &[
                RepositoryName("test2".to_string()),
                RepositoryName("test4".to_string()),
            ],
        )
        .await?;

        let entry = push
            .get_by_repo_name(
                &ctx,
                &RepositoryName("test1".to_string()),
                Staleness::MostRecent,
            )
            .await?;
        assert!(entry.is_some());
        let entry = entry.unwrap();
        assert_eq!(entry.source_of_truth, GitSourceOfTruth::Mononoke);

        let entry = push
            .get_by_repo_name(
                &ctx,
                &RepositoryName("test2".to_string()),
                Staleness::MostRecent,
            )
            .await?;
        assert!(entry.is_some());
        let entry = entry.unwrap();
        assert_eq!(entry.source_of_truth, GitSourceOfTruth::Metagit);

        let entry = push
            .get_by_repo_name(
                &ctx,
                &RepositoryName("test3".to_string()),
                Staleness::MostRecent,
            )
            .await?;
        assert!(entry.is_some());
        let entry = entry.unwrap();
        assert_eq!(entry.source_of_truth, GitSourceOfTruth::Mononoke);

        let entry = push
            .get_by_repo_name(
                &ctx,
                &RepositoryName("test4".to_string()),
                Staleness::MostRecent,
            )
            .await?;
        assert!(entry.is_some());
        let entry = entry.unwrap();
        assert_eq!(entry.source_of_truth, GitSourceOfTruth::Metagit);

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_get_by_repo_name(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let builder = SqlGitSourceOfTruthConfigBuilder::with_sqlite_in_memory()?;
        let push = builder.build();

        let repo_id = RepositoryId::new(1);
        let repo_name = RepositoryName("test1".to_string());
        let entry = push
            .get_by_repo_name(&ctx, &repo_name, Staleness::MostRecent)
            .await?;
        assert!(entry.is_none());

        push.insert_or_update_repo(&ctx, repo_id, repo_name.clone(), GitSourceOfTruth::Mononoke)
            .await?;
        let entry = push
            .get_by_repo_name(&ctx, &repo_name, Staleness::MostRecent)
            .await?;
        assert!(entry.is_some());
        let entry = entry.unwrap();
        assert_eq!(entry.source_of_truth, GitSourceOfTruth::Mononoke);

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_get_redirected(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let builder = SqlGitSourceOfTruthConfigBuilder::with_sqlite_in_memory()?;
        let push = builder.build();

        let to_be_redirected_to_mononoke_repo_id = RepositoryId::new(1);
        let to_be_redirected_to_mononoke_repo_name = RepositoryName("test1".to_string());
        push.insert_or_update_repo(
            &ctx,
            to_be_redirected_to_mononoke_repo_id,
            to_be_redirected_to_mononoke_repo_name.clone(),
            GitSourceOfTruth::Metagit,
        )
        .await?;
        push.insert_or_update_repo(
            &ctx,
            RepositoryId::new(2),
            RepositoryName("test2".to_string()),
            GitSourceOfTruth::Mononoke,
        )
        .await?;
        push.insert_or_update_repo(
            &ctx,
            RepositoryId::new(3),
            RepositoryName("test3".to_string()),
            GitSourceOfTruth::Metagit,
        )
        .await?;

        assert_eq!(push.get_redirected_to_mononoke(&ctx).await?.len(), 1);
        assert_eq!(push.get_redirected_to_metagit(&ctx).await?.len(), 2);

        push.insert_or_update_repo(
            &ctx,
            to_be_redirected_to_mononoke_repo_id,
            to_be_redirected_to_mononoke_repo_name,
            GitSourceOfTruth::Mononoke,
        )
        .await?;

        assert_eq!(push.get_redirected_to_mononoke(&ctx).await?.len(), 2);
        assert_eq!(push.get_redirected_to_metagit(&ctx).await?.len(), 1);

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_get_locked(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let builder = SqlGitSourceOfTruthConfigBuilder::with_sqlite_in_memory()?;
        let push = builder.build();

        let to_be_locked_repo_id = RepositoryId::new(1);
        let to_be_locked_repo_name = RepositoryName("test1".to_string());
        push.insert_or_update_repo(
            &ctx,
            to_be_locked_repo_id,
            to_be_locked_repo_name.clone(),
            GitSourceOfTruth::Locked,
        )
        .await?;
        push.insert_or_update_repo(
            &ctx,
            RepositoryId::new(2),
            RepositoryName("test2".to_string()),
            GitSourceOfTruth::Locked,
        )
        .await?;

        assert_eq!(push.get_locked(&ctx).await?.len(), 2);
        Ok(())
    }

    // The engine a reservation names travels with the row through every
    // read, so a cleanup on another host opens the store the symref is on.
    #[mononoke::fbinit_test]
    async fn test_storage_config_round_trips(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let push = SqlGitSourceOfTruthConfigBuilder::with_sqlite_in_memory()?.build();
        push.insert_repos(
            &ctx,
            &[
                (
                    RepositoryId::new(1),
                    RepositoryName("org/a".to_string()),
                    GitSourceOfTruth::Reserved,
                    Some("org_org".to_string()),
                ),
                (
                    RepositoryId::new(2),
                    RepositoryName("plain/b".to_string()),
                    GitSourceOfTruth::Reserved,
                    None,
                ),
            ],
        )
        .await?;
        push.update_mutation_id_by_repo_names_for_reserved_repos(
            &ctx,
            &[
                RepositoryName("org/a".to_string()),
                RepositoryName("plain/b".to_string()),
            ],
            7,
        )
        .await?;

        let by_name = push
            .get_by_repo_name(
                &ctx,
                &RepositoryName("org/a".to_string()),
                Staleness::MostRecent,
            )
            .await?
            .unwrap();
        assert_eq!(by_name.storage_config.as_deref(), Some("org_org"));

        let mut reserved = push.get_reserved_by_mutation_id(&ctx, 7).await?;
        reserved.sort_by_key(|entry| entry.repo_id.id());
        assert_eq!(
            reserved
                .iter()
                .map(|entry| entry.storage_config.as_deref())
                .collect::<Vec<_>>(),
            vec![Some("org_org"), None]
        );
        assert_eq!(
            push.get_reserved(&ctx).await?.len(),
            2,
            "every row read carries the column"
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_insert_repos_with_mutation_id(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let builder = SqlGitSourceOfTruthConfigBuilder::with_sqlite_in_memory()?;
        let push = builder.clone().build();

        // insert many
        push.insert_repos(
            &ctx,
            &[(
                RepositoryId::new(1),
                RepositoryName("test1".to_string()),
                GitSourceOfTruth::Reserved,
                None,
            )],
        )
        .await?;

        // get each and confirm it worked
        let entry = push
            .get_by_repo_name(
                &ctx,
                &RepositoryName("test1".to_string()),
                Staleness::MostRecent,
            )
            .await?;
        assert!(entry.is_some());
        let entry = entry.unwrap();
        assert_eq!(entry.source_of_truth, GitSourceOfTruth::Reserved);

        let mutation_id = 123;
        push.update_mutation_id_by_repo_names_for_reserved_repos(
            &ctx,
            &[RepositoryName("test1".to_string())],
            mutation_id,
        )
        .await?;

        push.update_source_of_truth_by_mutation_id(&ctx, GitSourceOfTruth::Mononoke, mutation_id)
            .await?;

        let entry = push
            .get_by_repo_name(
                &ctx,
                &RepositoryName("test1".to_string()),
                Staleness::MostRecent,
            )
            .await?;
        assert!(entry.is_some());
        let entry = entry.unwrap();
        assert_eq!(entry.source_of_truth, GitSourceOfTruth::Mononoke);
        assert_eq!(entry.mutation_id.unwrap(), 123);
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_update_mutation_id_returns_affected_row_count(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let builder = SqlGitSourceOfTruthConfigBuilder::with_sqlite_in_memory()?;
        let push = builder.build();

        push.insert_repos(
            &ctx,
            &[
                (
                    RepositoryId::new(1),
                    RepositoryName("reserved_a".to_string()),
                    GitSourceOfTruth::Reserved,
                    None,
                ),
                (
                    RepositoryId::new(2),
                    RepositoryName("reserved_b".to_string()),
                    GitSourceOfTruth::Reserved,
                    None,
                ),
                (
                    RepositoryId::new(3),
                    RepositoryName("already_mononoke".to_string()),
                    GitSourceOfTruth::Mononoke,
                    None,
                ),
            ],
        )
        .await?;

        assert_eq!(
            push.update_mutation_id_by_repo_names_for_reserved_repos(
                &ctx,
                &[
                    RepositoryName("reserved_a".to_string()),
                    RepositoryName("reserved_b".to_string()),
                ],
                7,
            )
            .await?,
            2,
            "both reserved rows should be stamped and counted",
        );
        assert_eq!(
            push.update_mutation_id_by_repo_names_for_reserved_repos(
                &ctx,
                &[
                    RepositoryName("reserved_a".to_string()),
                    RepositoryName("already_mononoke".to_string()),
                    RepositoryName("missing".to_string()),
                ],
                8,
            )
            .await?,
            1,
            "non-reserved and missing rows must not be stamped or counted",
        );

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_get_reserved_by_mutation_id(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let builder = SqlGitSourceOfTruthConfigBuilder::with_sqlite_in_memory()?;
        let push = builder.build();

        push.insert_repos(
            &ctx,
            &[
                (
                    RepositoryId::new(1),
                    RepositoryName("reserved_wanted_a".to_string()),
                    GitSourceOfTruth::Reserved,
                    None,
                ),
                (
                    RepositoryId::new(2),
                    RepositoryName("reserved_wanted_b".to_string()),
                    GitSourceOfTruth::Reserved,
                    None,
                ),
                (
                    RepositoryId::new(3),
                    RepositoryName("reserved_other_mutation".to_string()),
                    GitSourceOfTruth::Reserved,
                    None,
                ),
                (
                    RepositoryId::new(4),
                    RepositoryName("reserved_unstamped".to_string()),
                    GitSourceOfTruth::Reserved,
                    None,
                ),
                (
                    RepositoryId::new(5),
                    RepositoryName("landed_same_mutation".to_string()),
                    GitSourceOfTruth::Reserved,
                    None,
                ),
            ],
        )
        .await?;
        push.update_mutation_id_by_repo_names_for_reserved_repos(
            &ctx,
            &[
                RepositoryName("reserved_wanted_a".to_string()),
                RepositoryName("reserved_wanted_b".to_string()),
                RepositoryName("landed_same_mutation".to_string()),
            ],
            42,
        )
        .await?;
        push.update_mutation_id_by_repo_names_for_reserved_repos(
            &ctx,
            &[RepositoryName("reserved_other_mutation".to_string())],
            43,
        )
        .await?;
        push.update_source_of_truth_by_repo_names(
            &ctx,
            GitSourceOfTruth::Mononoke,
            &[RepositoryName("landed_same_mutation".to_string())],
        )
        .await?;

        let entries = push.get_reserved_by_mutation_id(&ctx, 42).await?;
        let mut names = entries
            .iter()
            .map(|entry| entry.repo_name.0.as_str())
            .collect::<Vec<_>>();
        names.sort_unstable();
        assert_eq!(
            names,
            vec!["reserved_wanted_a", "reserved_wanted_b"],
            "only Reserved rows stamped with mutation_id 42 should be returned",
        );
        assert!(
            entries.iter().all(|entry| {
                entry.source_of_truth == GitSourceOfTruth::Reserved && entry.mutation_id == Some(42)
            }),
            "returned entries must all be Reserved and stamped with the requested mutation_id",
        );

        let mononoke_entries = push
            .get_redirected_to_mononoke_by_mutation_id(&ctx, 42)
            .await?;
        assert_eq!(
            mononoke_entries
                .iter()
                .map(|entry| entry.repo_name.0.as_str())
                .collect::<Vec<_>>(),
            vec!["landed_same_mutation"],
            "only Mononoke rows stamped with mutation_id 42 should be returned",
        );
        assert!(
            push.get_redirected_to_mononoke_by_mutation_id(&ctx, 43)
                .await?
                .is_empty(),
            "no Mononoke row is stamped with mutation_id 43",
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_delete_reserved_row(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let builder = SqlGitSourceOfTruthConfigBuilder::with_sqlite_in_memory()?;
        let push = builder.build();

        push.insert_repos(
            &ctx,
            &[
                (
                    RepositoryId::new(1),
                    RepositoryName("unstamped".to_string()),
                    GitSourceOfTruth::Reserved,
                    None,
                ),
                (
                    RepositoryId::new(2),
                    RepositoryName("stamped".to_string()),
                    GitSourceOfTruth::Reserved,
                    None,
                ),
                (
                    RepositoryId::new(3),
                    RepositoryName("landed".to_string()),
                    GitSourceOfTruth::Reserved,
                    None,
                ),
            ],
        )
        .await?;
        push.update_mutation_id_by_repo_names_for_reserved_repos(
            &ctx,
            &[
                RepositoryName("stamped".to_string()),
                RepositoryName("landed".to_string()),
            ],
            7,
        )
        .await?;
        push.update_source_of_truth_by_repo_names(
            &ctx,
            GitSourceOfTruth::Mononoke,
            &[RepositoryName("landed".to_string())],
        )
        .await?;

        let row_id_of = |name: &str| {
            let name = RepositoryName(name.to_string());
            let push = &push;
            let ctx = &ctx;
            async move {
                anyhow::Ok(
                    push.get_by_repo_name(ctx, &name, Staleness::MostRecent)
                        .await?
                        .expect("row should exist")
                        .id,
                )
            }
        };

        // Wrong stamp never deletes.
        let unstamped_id = row_id_of("unstamped").await?;
        assert_eq!(
            push.delete_reserved_row(&ctx, unstamped_id, Some(7))
                .await?,
            0,
            "unstamped row must not match a stamped delete",
        );
        let stamped_id = row_id_of("stamped").await?;
        assert_eq!(
            push.delete_reserved_row(&ctx, stamped_id, None).await?,
            0,
            "stamped row must not match an unstamped delete",
        );
        assert_eq!(
            push.delete_reserved_row(&ctx, stamped_id, Some(8)).await?,
            0,
            "stamped row must not match a different mutation_id",
        );

        // Non-reserved rows are never touched, even with a matching stamp.
        let landed_id = row_id_of("landed").await?;
        assert_eq!(
            push.delete_reserved_row(&ctx, landed_id, Some(7)).await?,
            0,
            "non-reserved row must never be deleted",
        );
        assert!(
            push.get_by_repo_name(
                &ctx,
                &RepositoryName("landed".to_string()),
                Staleness::MostRecent,
            )
            .await?
            .is_some(),
            "non-reserved row should still exist",
        );

        // Exact matches delete exactly one row.
        assert_eq!(
            push.delete_reserved_row(&ctx, unstamped_id, None).await?,
            1,
            "exact unstamped match should delete",
        );
        assert_eq!(
            push.delete_reserved_row(&ctx, stamped_id, Some(7)).await?,
            1,
            "exact stamped match should delete",
        );
        assert!(
            push.get_by_repo_name(
                &ctx,
                &RepositoryName("unstamped".to_string()),
                Staleness::MostRecent,
            )
            .await?
            .is_none(),
            "deleted row should be gone",
        );

        // Idempotent: a second delete is a no-op.
        assert_eq!(
            push.delete_reserved_row(&ctx, stamped_id, Some(7)).await?,
            0
        );

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_allocate_repo_ids_never_reissues_after_delete(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let builder = SqlGitSourceOfTruthConfigBuilder::with_sqlite_in_memory()?;
        let push = builder.build();
        push.seed_repo_id_sequence(&ctx, 100_902).await?;

        let first_batch = push.allocate_repo_ids(&ctx, 3).await?;
        assert_eq!(first_batch.len(), 3, "asked for 3 ids, got a short batch");

        // Reserve and then delete them, the way a failed creation does. This is
        // what made `MAX(repo_id) + 1` rewind and hand out live ids again.
        let repos = first_batch
            .iter()
            .enumerate()
            .map(|(i, id)| {
                (
                    *id,
                    RepositoryName(format!("doomed{i}")),
                    GitSourceOfTruth::Reserved,
                    None,
                )
            })
            .collect::<Vec<_>>();
        push.insert_repos(&ctx, &repos).await?;
        push.delete_source_of_truth_by_repo_names_for_reserved_repos(
            &ctx,
            &repos
                .iter()
                .map(|(_, name, _, _)| name.clone())
                .collect::<Vec<_>>(),
        )
        .await?;

        // The table is empty again, so the old allocator would restart at 1.
        assert_eq!(push.get_max_id(&ctx).await?, None);

        let second_batch = push.allocate_repo_ids(&ctx, 3).await?;
        for id in &second_batch {
            assert!(
                !first_batch.contains(id),
                "repo id {id} was reissued after its row was deleted"
            );
        }

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_allocate_repo_ids_starts_above_the_seed(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let builder = SqlGitSourceOfTruthConfigBuilder::with_sqlite_in_memory()?;
        let push = builder.build();

        // Production seeds above every id that has ever existed, including the
        // hand-assigned ids in `repo_index.cinc` that the allocator never issued.
        push.seed_repo_id_sequence(&ctx, 100_902).await?;

        let ids = push.allocate_repo_ids(&ctx, 2).await?;
        assert_eq!(
            ids,
            vec![RepositoryId::new(100_903), RepositoryId::new(100_904)]
        );

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_allocate_repo_ids_refuses_an_unseeded_sequence(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let builder = SqlGitSourceOfTruthConfigBuilder::with_sqlite_in_memory()?;
        let push = builder.build();

        // Without this guard an unseeded sequence starts at 1 and quietly hands
        // out ids that thousands of live repos already hold.
        let err = push
            .allocate_repo_ids(&ctx, 1)
            .await
            .expect_err("allocating from an unseeded sequence should fail");
        assert!(
            format!("{err:#}").contains("must be seeded"),
            "unexpected error: {err:#}"
        );

        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_get_max_id(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let builder = SqlGitSourceOfTruthConfigBuilder::with_sqlite_in_memory()?;
        let push = builder.build();

        assert_eq!(push.get_max_id(&ctx).await?, None);
        let to_be_locked_repo_id = RepositoryId::new(1);
        let to_be_locked_repo_name = RepositoryName("test1".to_string());
        push.insert_or_update_repo(
            &ctx,
            to_be_locked_repo_id,
            to_be_locked_repo_name.clone(),
            GitSourceOfTruth::Metagit,
        )
        .await?;
        push.insert_or_update_repo(
            &ctx,
            RepositoryId::new(2),
            RepositoryName("test2".to_string()),
            GitSourceOfTruth::Locked,
        )
        .await?;
        assert_eq!(push.get_max_id(&ctx).await?, Some(RepositoryId::new(2)));
        push.insert_or_update_repo(
            &ctx,
            RepositoryId::new(42),
            RepositoryName("test3".to_string()),
            GitSourceOfTruth::Mononoke,
        )
        .await?;
        assert_eq!(push.get_max_id(&ctx).await?, Some(RepositoryId::new(42)));
        Ok(())
    }

    /// Repo names of `entries`, sorted so assertions do not depend on row order.
    fn sorted_names(entries: &[GitSourceOfTruthConfigEntry]) -> Vec<&str> {
        let mut names = entries
            .iter()
            .map(|entry| entry.repo_name.0.as_str())
            .collect::<Vec<_>>();
        names.sort_unstable();
        names
    }

    /// One row in each state. Only the reserved row has a mutation id and a
    /// storage config, so a read that drops either column shows up.
    async fn insert_one_repo_per_state(
        ctx: &CoreContext,
        push: &SqlGitSourceOfTruthConfig,
    ) -> Result<()> {
        push.insert_repos(
            ctx,
            &[
                (
                    RepositoryId::new(1),
                    RepositoryName("mononoke_repo".to_string()),
                    GitSourceOfTruth::Mononoke,
                    None,
                ),
                (
                    RepositoryId::new(2),
                    RepositoryName("metagit_repo".to_string()),
                    GitSourceOfTruth::Metagit,
                    None,
                ),
                (
                    RepositoryId::new(3),
                    RepositoryName("locked_repo".to_string()),
                    GitSourceOfTruth::Locked,
                    None,
                ),
                (
                    RepositoryId::new(4),
                    RepositoryName("reserved_repo".to_string()),
                    GitSourceOfTruth::Reserved,
                    Some("org_org".to_string()),
                ),
            ],
        )
        .await?;
        push.update_mutation_id_by_repo_names_for_reserved_repos(
            ctx,
            &[RepositoryName("reserved_repo".to_string())],
            9,
        )
        .await?;
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_get_any_returns_every_row_with_every_column(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let push = SqlGitSourceOfTruthConfigBuilder::with_sqlite_in_memory()?.build();
        insert_one_repo_per_state(&ctx, &push).await?;

        let mut entries = push.get_any(&ctx).await?;
        entries.sort_by_key(|entry| entry.repo_id.id());
        let rows = entries
            .iter()
            .map(|entry| {
                (
                    entry.repo_id.id(),
                    entry.repo_name.0.as_str(),
                    entry.source_of_truth,
                    entry.mutation_id,
                    entry.storage_config.as_deref(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            rows,
            vec![
                (1, "mononoke_repo", GitSourceOfTruth::Mononoke, None, None),
                (2, "metagit_repo", GitSourceOfTruth::Metagit, None, None),
                (3, "locked_repo", GitSourceOfTruth::Locked, None, None),
                (
                    4,
                    "reserved_repo",
                    GitSourceOfTruth::Reserved,
                    Some(9),
                    Some("org_org")
                ),
            ],
            "get_any should return all four rows, whatever their state, with every column",
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_state_reads_return_only_their_state(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let push = SqlGitSourceOfTruthConfigBuilder::with_sqlite_in_memory()?.build();
        insert_one_repo_per_state(&ctx, &push).await?;

        assert_eq!(
            sorted_names(&push.get_redirected_to_mononoke(&ctx).await?),
            vec!["mononoke_repo"]
        );
        assert_eq!(
            sorted_names(&push.get_redirected_to_metagit(&ctx).await?),
            vec!["metagit_repo"]
        );
        assert_eq!(
            sorted_names(&push.get_locked(&ctx).await?),
            vec!["locked_repo"]
        );
        assert_eq!(
            sorted_names(&push.get_reserved(&ctx).await?),
            vec!["reserved_repo"]
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_update_source_of_truth_by_mutation_id_only_changes_that_mutation(
        fb: FacebookInit,
    ) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let push = SqlGitSourceOfTruthConfigBuilder::with_sqlite_in_memory()?.build();
        push.insert_repos(
            &ctx,
            &[
                (
                    RepositoryId::new(1),
                    RepositoryName("mutation_5".to_string()),
                    GitSourceOfTruth::Reserved,
                    None,
                ),
                (
                    RepositoryId::new(2),
                    RepositoryName("mutation_6".to_string()),
                    GitSourceOfTruth::Reserved,
                    None,
                ),
                (
                    RepositoryId::new(3),
                    RepositoryName("unstamped".to_string()),
                    GitSourceOfTruth::Reserved,
                    None,
                ),
            ],
        )
        .await?;
        push.update_mutation_id_by_repo_names_for_reserved_repos(
            &ctx,
            &[RepositoryName("mutation_5".to_string())],
            5,
        )
        .await?;
        push.update_mutation_id_by_repo_names_for_reserved_repos(
            &ctx,
            &[RepositoryName("mutation_6".to_string())],
            6,
        )
        .await?;

        push.update_source_of_truth_by_mutation_id(&ctx, GitSourceOfTruth::Mononoke, 5)
            .await?;

        assert_eq!(
            sorted_names(&push.get_redirected_to_mononoke(&ctx).await?),
            vec!["mutation_5"],
            "only the row stamped with mutation 5 should move to Mononoke",
        );
        assert_eq!(
            sorted_names(&push.get_reserved(&ctx).await?),
            vec!["mutation_6", "unstamped"],
            "rows with another mutation id, or none, should stay Reserved",
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_delete_for_mutation_id_only_deletes_reserved_rows_of_that_mutation(
        fb: FacebookInit,
    ) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let push = SqlGitSourceOfTruthConfigBuilder::with_sqlite_in_memory()?.build();
        push.insert_repos(
            &ctx,
            &[
                (
                    RepositoryId::new(1),
                    RepositoryName("reserved_mutation_5".to_string()),
                    GitSourceOfTruth::Reserved,
                    None,
                ),
                (
                    RepositoryId::new(2),
                    RepositoryName("reserved_mutation_6".to_string()),
                    GitSourceOfTruth::Reserved,
                    None,
                ),
                (
                    RepositoryId::new(3),
                    RepositoryName("reserved_unstamped".to_string()),
                    GitSourceOfTruth::Reserved,
                    None,
                ),
                (
                    RepositoryId::new(4),
                    RepositoryName("landed_mutation_5".to_string()),
                    GitSourceOfTruth::Reserved,
                    None,
                ),
            ],
        )
        .await?;
        push.update_mutation_id_by_repo_names_for_reserved_repos(
            &ctx,
            &[
                RepositoryName("reserved_mutation_5".to_string()),
                RepositoryName("landed_mutation_5".to_string()),
            ],
            5,
        )
        .await?;
        push.update_mutation_id_by_repo_names_for_reserved_repos(
            &ctx,
            &[RepositoryName("reserved_mutation_6".to_string())],
            6,
        )
        .await?;
        push.update_source_of_truth_by_repo_names(
            &ctx,
            GitSourceOfTruth::Mononoke,
            &[RepositoryName("landed_mutation_5".to_string())],
        )
        .await?;

        push.delete_source_of_truth_for_mutation_id(&ctx, &5)
            .await?;

        assert_eq!(
            sorted_names(&push.get_any(&ctx).await?),
            vec![
                "landed_mutation_5",
                "reserved_mutation_6",
                "reserved_unstamped"
            ],
            "only the Reserved row stamped with mutation 5 should be deleted",
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn test_delete_by_repo_names_only_deletes_listed_reserved_rows(
        fb: FacebookInit,
    ) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let push = SqlGitSourceOfTruthConfigBuilder::with_sqlite_in_memory()?.build();
        push.insert_repos(
            &ctx,
            &[
                (
                    RepositoryId::new(1),
                    RepositoryName("listed_reserved".to_string()),
                    GitSourceOfTruth::Reserved,
                    None,
                ),
                (
                    RepositoryId::new(2),
                    RepositoryName("listed_mononoke".to_string()),
                    GitSourceOfTruth::Mononoke,
                    None,
                ),
                (
                    RepositoryId::new(3),
                    RepositoryName("unlisted_reserved".to_string()),
                    GitSourceOfTruth::Reserved,
                    None,
                ),
            ],
        )
        .await?;

        push.delete_source_of_truth_by_repo_names_for_reserved_repos(
            &ctx,
            &[
                RepositoryName("listed_reserved".to_string()),
                RepositoryName("listed_mononoke".to_string()),
            ],
        )
        .await?;

        assert_eq!(
            sorted_names(&push.get_any(&ctx).await?),
            vec!["listed_mononoke", "unlisted_reserved"],
            "a listed non-Reserved row and an unlisted Reserved row should both survive",
        );
        Ok(())
    }
}
