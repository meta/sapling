/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use mononoke_types::RepositoryId;
use sql_ext::SqlConnections;

#[derive(Clone)]
pub(crate) struct SqlBubbleHgMapping {
    connections: SqlConnections,
    repo_id: RepositoryId,
}

impl SqlBubbleHgMapping {
    pub(crate) fn new(connections: SqlConnections, repo_id: RepositoryId) -> Self {
        Self {
            connections,
            repo_id,
        }
    }
}
