/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::sync::Arc;

use anyhow::Error;
use fbinit::FacebookInit;
use git_source_of_truth::GitSourceOfTruthConfig;
use git_source_of_truth::SqlGitSourceOfTruthConfigBuilder;
use mononoke_app::MononokeApp;
use mononoke_thrift_server::ThriftServerBuilder;
use sql_construct::SqlConstruct;

pub(crate) async fn create_git_source_of_truth_config(
    _fb: FacebookInit,
) -> Result<Arc<dyn GitSourceOfTruthConfig>, Error> {
    Ok(Arc::new(
        SqlGitSourceOfTruthConfigBuilder::with_sqlite_in_memory()?.build(),
    ))
}

pub(crate) fn configure_server(
    builder: ThriftServerBuilder,
    _app: &MononokeApp,
) -> ThriftServerBuilder {
    builder
}

pub(crate) struct ServerFactory;

impl ServerFactory {
    pub(crate) async fn new(
        _fb: FacebookInit,
        _workers: usize,
        _queue_size: usize,
        _queue_timeout: std::time::Duration,
    ) -> Self {
        Self
    }

    pub(crate) fn executor(&self) {}
}
