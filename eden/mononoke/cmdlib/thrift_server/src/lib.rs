/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Thrift server builder shared by Mononoke's services.
//!
//! fbcode builds serve through srserver and ServiceFramework. OSS builds
//! serve through the loopback-only `oss_thrift_transport`, a reference
//! transport for local development, testing and demonstration that is not
//! suitable for deployment.

mod config;

#[cfg(fbcode_build)]
mod facebook;
#[cfg(not(fbcode_build))]
mod oss;

use std::net::SocketAddr;
use std::path::Path;

use anyhow::Context;
use anyhow::Result;
pub use config::LoadMonitorConfig;
#[cfg(fbcode_build)]
use facebook as platform;
#[cfg(not(fbcode_build))]
use oss as platform;
pub use platform::DEFAULT_THRIFT_HOST;
pub use platform::MononokeThriftServer;
pub use platform::RequestContext;
#[cfg(fbcode_build)]
pub use platform::ServiceMetadata;
pub use platform::ThriftServerBuilder;
pub use platform::ThriftServerShutdownHandle;

impl MononokeThriftServer {
    /// Write the address the server is listening on to `path`, formatted as
    /// a [`SocketAddr`] (for example `[::]:8000`), and return it.
    pub fn write_bound_address(&mut self, path: impl AsRef<Path>) -> Result<SocketAddr> {
        let path = path.as_ref();
        let address = self.bound_address()?;
        std::fs::write(path, address.to_string())
            .with_context(|| format!("failed to write the bound address to {}", path.display()))?;
        Ok(address)
    }
}
