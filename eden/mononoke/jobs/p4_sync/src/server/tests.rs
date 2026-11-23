/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::path::PathBuf;

use async_trait::async_trait;
use mononoke_macros::mononoke;
use p4_source::P4Auth;
use p4_source::P4Ticket;
use p4_source::P4Trust;

use super::*;

fn htl() -> P4ServerConfig {
    P4ServerConfig {
        host: "p4-secondary.htl.perforce.metafb.cloud".to_owned(),
        fingerprint: "AB:CD".to_owned(),
        user: "svc-scm-p4mirror".to_owned(),
        ticket_secret: "SCM_P4MIRROR_P4TICKET_HTL".to_owned(),
    }
}

#[mononoke::test]
async fn test_the_port_is_ssl6_on_the_hosts_ipv6_address() {
    assert_eq!(p4port("::1").await.unwrap(), "ssl6:[::1]:1818");

    let error = p4port("127.0.0.1").await.unwrap_err();
    assert!(error.to_string().contains("no IPv6 address"), "{error}");
}

/// Keychain holding one secret.
struct OneSecret {
    name: &'static str,
    value: &'static str,
}

#[async_trait]
impl SecretReader for OneSecret {
    async fn read(&self, name: &str) -> Result<Vec<u8>> {
        anyhow::ensure!(name == self.name, "no secret {name}");
        Ok(self.value.as_bytes().to_vec())
    }
}

#[mononoke::test]
fn test_a_server_gets_p4imports_flags_from_its_entry() {
    let args = p4_args(
        &htl(),
        Path::new("/mononoke/p4"),
        "ssl6:[2001:db8::1]:1818".to_owned(),
    );
    assert_eq!(
        args.p4_auth.p4_ticket_secret.as_deref(),
        Some("SCM_P4MIRROR_P4TICKET_HTL")
    );
    assert_eq!(args.p4_auth.p4_ticket_file, None);
    assert_eq!(args.p4_auth.p4_password_file, None);

    let keychain = Arc::new(OneSecret {
        name: "SCM_P4MIRROR_P4TICKET_HTL",
        value: "ABC123",
    });
    let config = args.config(keychain).unwrap();
    assert_eq!(config.binary, PathBuf::from("/mononoke/p4"));
    assert_eq!(config.port, "ssl6:[2001:db8::1]:1818");
    assert_eq!(config.user, "svc-scm-p4mirror");
    assert_eq!(config.trust, P4Trust::Fingerprint("AB:CD".to_owned()));
    assert!(
        matches!(config.auth, P4Auth::Ticket(_)),
        "expected a ticket, got {:?}",
        config.auth
    );
}

#[mononoke::test]
async fn test_the_ticket_is_the_keychain_secret_the_entry_names() {
    let args = p4_args(
        &htl(),
        Path::new("/mononoke/p4"),
        "ssl6:[2001:db8::1]:1818".to_owned(),
    );
    let keychain = Arc::new(OneSecret {
        name: "SCM_P4MIRROR_P4TICKET_HTL",
        value: "ABC123\n",
    });
    let P4Auth::Ticket(source) = args.config(keychain).unwrap().auth else {
        panic!("expected a ticket");
    };
    assert!(
        source.current_ticket().await.unwrap() == P4Ticket::new("ABC123").unwrap(),
        "the value of the entry's ticket_secret, read through the client's ticket source"
    );
}
