/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! `StaticOptions::remote_fetch_override`.
//!
//! For callers that own the endpoint rather than inheriting whatever the source
//! manifest declares.

use anyhow::Result;
use repo_manifest::ParseOptions;
use repo_manifest::StaticOptions;
use repo_manifest::canonicalize_static;
use repo_manifest::to_static_with_options;

fn no_includes(name: &str) -> Result<Vec<u8>> {
    anyhow::bail!("Unexpected include: {name}")
}

fn sha() -> String {
    "a".repeat(40)
}

const SYMBOLIC: &[u8] = br#"<manifest>
    <remote name="origin" fetch="ssh://legacy.example.com/data/gitrepos" pushurl="ssh://push.example.com" />
    <remote name="mirror" fetch="ssh://legacy-mirror.example.com/data/gitrepos" />
    <default remote="origin" revision="main" />
    <project name="foo" path="foo" />
</manifest>"#;

fn generate(options: &StaticOptions) -> String {
    to_static_with_options(SYMBOLIC, &mut no_includes, |_p| Ok(sha()), options)
        .expect("the fixture should serialize")
}

#[test]
fn the_source_fetch_is_used_by_default() {
    let out = generate(&StaticOptions::default());
    assert!(
        out.contains(r#"fetch="ssh://legacy.example.com/data/gitrepos""#),
        "without an override the parsed value is emitted: {out}"
    );
}

#[test]
fn the_override_replaces_every_remote() {
    let out = generate(&StaticOptions {
        remote_fetch_override: Some("https://canonical.example.com/".to_string()),
        ..Default::default()
    });
    assert_eq!(
        out.matches(r#"fetch="https://canonical.example.com/""#)
            .count(),
        2,
        "both remotes take the override: {out}"
    );
    assert!(
        !out.contains("legacy.example.com"),
        "no source fetch survives: {out}"
    );
}

/// Push routing is a separate question from where a checkout fetches, so the
/// override deliberately leaves it alone.
#[test]
fn the_override_leaves_pushurl_alone() {
    let out = generate(&StaticOptions {
        remote_fetch_override: Some("https://canonical.example.com/".to_string()),
        ..Default::default()
    });
    assert!(
        out.contains(r#"pushurl="ssh://push.example.com""#),
        "pushurl must survive the fetch override: {out}"
    );
}

/// The parser trims a trailing slash, so an overridden document is still a fixed
/// point -- the parity comparison depends on that.
#[test]
fn an_overridden_document_still_canonicalizes_idempotently() {
    let out = generate(&StaticOptions {
        remote_fetch_override: Some("https://canonical.example.com/".to_string()),
        ..Default::default()
    });
    let once = canonicalize_static(out.as_bytes(), &ParseOptions::default())
        .expect("first pass should canonicalize");
    let twice = canonicalize_static(once.as_bytes(), &ParseOptions::default())
        .expect("second pass should canonicalize");
    assert_eq!(once, twice, "canonicalization must remain a fixed point");
}
