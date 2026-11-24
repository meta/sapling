/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use mononoke_types::RepositoryId;
use sql_construct::SqlConstruct;
use sql_ext::SqlConnections;

use crate::mapping::SqlEphemeralCommitMapping;

pub struct EphemeralCommitsBuilder {
    connections: SqlConnections,
}

impl SqlConstruct for EphemeralCommitsBuilder {
    const LABEL: &'static str = "ephemeral_commits";

    const CREATION_QUERY: &'static str = include_str!("../schemas/sqlite-ephemeral-commits.sql");

    fn from_sql_connections(connections: SqlConnections) -> Self {
        Self { connections }
    }
}

impl EphemeralCommitsBuilder {
    pub fn build_mapping(&self, repo_id: RepositoryId) -> SqlEphemeralCommitMapping {
        SqlEphemeralCommitMapping::new(self.connections.clone(), repo_id)
    }
}

#[cfg(test)]
mod test {
    use anyhow::Result;
    use mononoke_macros::mononoke;
    use mononoke_types_mocks::repo::REPO_ZERO;

    use super::*;

    #[mononoke::test]
    fn builds_mapping_on_sqlite_schema_test() -> Result<()> {
        EphemeralCommitsBuilder::with_sqlite_in_memory()?.build_mapping(REPO_ZERO);
        Ok(())
    }
}
