/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Loading and parsing of Mononoke configuration.

#![deny(missing_docs)]

pub mod config;
mod config_hash;
mod convert;
pub mod errors;

pub use convert::Convert;

pub use crate::config::RepoConfigEntry;
pub use crate::config::RepoConfigs;
pub use crate::config::StorageConfigs;
pub use crate::config::configerator_manifest_handle;
pub use crate::config::configerator_repo_config_handle;
pub use crate::config::configerator_repo_spec_handle;
pub use crate::config::load_empty_repo_configs;
pub use crate::config::load_repo_configs_from_manifest;
pub use crate::config::parse_manifest_common_and_storage;
pub use crate::config::parse_repo_spec;
pub use crate::config_hash::SharedGenerations;
pub use crate::config_hash::spec_hash;
pub use crate::errors::ConfigurationError;
