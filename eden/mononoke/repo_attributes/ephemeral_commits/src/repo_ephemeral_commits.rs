/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::sync::Arc;

use crate::mapping::SqlEphemeralCommitMapping;

#[facet::facet]
pub struct RepoEphemeralCommits {
    mapping: Option<Arc<SqlEphemeralCommitMapping>>,
}

impl RepoEphemeralCommits {
    pub fn new(mapping: SqlEphemeralCommitMapping) -> Self {
        Self {
            mapping: Some(Arc::new(mapping)),
        }
    }

    pub fn disabled() -> Self {
        Self { mapping: None }
    }

    pub fn is_enabled(&self) -> bool {
        self.mapping.is_some()
    }
}
