/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

pub(crate) mod action;
pub mod config;
pub mod error;
pub mod receiver;
mod slapi;
pub mod subscriber;
pub(crate) mod util;

pub use config::CommitCloudConfig;
pub use receiver::TcpReceiverService as CommitCloudTcpReceiverService;
pub use subscriber::WorkspaceSubscriberService as CommitCloudWorkspaceSubscriberService;

pub(crate) type ActionsMap =
    std::collections::HashMap<receiver::CommandName, Box<dyn Fn() + Send + Sync>>;

#[cfg(test)]
pub mod tests;
