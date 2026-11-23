/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! A p4 client for an entry of the tier's `p4_servers`.

use std::net::SocketAddr;
use std::path::Path;

use anyhow::Context;
use anyhow::Result;
use metaconfig_types::P4ServerConfig;
use p4_source::P4Args;
use p4_source::P4AuthArgs;
use p4_source::P4Cli;
use scuba_ext::MononokeScubaSampleBuilder;

/// Every server's, and the ticket minter's default.
const P4_PORT_NUMBER: u16 = 1818;
/// Where the job's TW spec mounts each alias's ticket, as `p4ticket-<alias>`.
const TICKET_DIR: &str = "/run/secrets";

/// A logged-in-on-demand client for `server`, at the address its host resolves to now.
pub(crate) async fn client(
    alias: &str,
    server: &P4ServerConfig,
    binary: &Path,
    scuba: MononokeScubaSampleBuilder,
) -> Result<P4Cli> {
    let p4port = p4port(&server.host).await?;
    p4_args(alias, server, binary, p4port).client(scuba)
}

/// `ssl6:[addr]:1818` for the host's first IPv6 address. The job resolves it, not p4: the
/// static p4 binary does not see TTLS's synthetic DNS answer for the Perforce domain.
pub(crate) async fn p4port(host: &str) -> Result<String> {
    let address = tokio::net::lookup_host((host, P4_PORT_NUMBER))
        .await
        .with_context(|| format!("cannot resolve {host}"))?
        .find(SocketAddr::is_ipv6)
        .with_context(|| format!("{host} has no IPv6 address"))?;
    Ok(format!("ssl6:{address}"))
}

/// p4import's flags for `server` at `p4port`, so both get their client the same way.
pub(crate) fn p4_args(
    alias: &str,
    server: &P4ServerConfig,
    binary: &Path,
    p4port: String,
) -> P4Args {
    P4Args {
        p4_binary: binary.to_owned(),
        p4_port: p4port,
        p4_user: server.user.clone(),
        p4_auth: P4AuthArgs {
            p4_password_file: None,
            p4_ticket_file: Some(Path::new(TICKET_DIR).join(format!("p4ticket-{alias}"))),
        },
        p4_fingerprint: Some(server.fingerprint.clone()),
    }
}

#[cfg(test)]
mod tests;
