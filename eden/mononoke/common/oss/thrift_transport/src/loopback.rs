/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::net::SocketAddr;
use std::net::ToSocketAddrs;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;

/// Resolve `address`, failing unless it resolves to at least one address and
/// every address it resolves to is a loopback address. `description` names
/// the address in errors.
pub(crate) fn resolve_loopback(
    address: impl ToSocketAddrs,
    description: &str,
) -> Result<Vec<SocketAddr>> {
    let resolved = address
        .to_socket_addrs()
        .with_context(|| format!("failed to resolve OSS thrift address {description}"))?
        .collect::<Vec<_>>();
    if resolved.is_empty() {
        bail!("OSS thrift address {description} did not resolve to an address");
    }
    if let Some(remote) = resolved
        .iter()
        .find(|resolved| !resolved.ip().is_loopback())
    {
        bail!(
            "the OSS thrift transport is plaintext and unauthenticated; refusing non-loopback address {remote}"
        );
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use mononoke_macros::mononoke;

    use super::*;

    #[mononoke::test]
    fn accepts_loopback_addresses() {
        resolve_loopback("127.0.0.1:0", "ipv4").expect("IPv4 loopback should be accepted");
        resolve_loopback("[::1]:0", "ipv6").expect("IPv6 loopback should be accepted");
        resolve_loopback(("::1", 0), "host and port")
            .expect("a host and port pair should be accepted");
    }

    #[mononoke::test]
    fn rejects_non_loopback_addresses() {
        assert!(resolve_loopback("0.0.0.0:0", "0.0.0.0:0").is_err());
        assert!(resolve_loopback("[::]:0", "[::]:0").is_err());
        assert!(resolve_loopback("192.0.2.1:1234", "192.0.2.1:1234").is_err());
    }
}
