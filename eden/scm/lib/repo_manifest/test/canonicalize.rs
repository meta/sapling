/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! `canonicalize_static` and `Manifest::remotes`.
//!
//! Both exist for the AOSP manifest parity instrument: round-tripping a live
//! `static.xml` through this crate isolates serializer normalisation from real
//! differences, and that comparison has to be able to enumerate remotes.

use anyhow::Result;
use repo_manifest::Manifest;
use repo_manifest::ParseOptions;
use repo_manifest::canonicalize_static;

fn no_includes(name: &str) -> Result<Vec<u8>> {
    anyhow::bail!("Unexpected include: {name}")
}

fn sha(seed: char) -> String {
    std::iter::repeat_n(seed, 40).collect()
}

fn pinned_manifest() -> String {
    format!(
        r#"<manifest>
    <remote name="origin" fetch="https://example.com" />
    <default remote="origin" revision="main" />
    <project name="foo" path="foo" revision="{}" upstream="main" />
    <project name="bar" path="bar" revision="{}" upstream="main" />
</manifest>"#,
        sha('a'),
        sha('b')
    )
}

#[test]
fn round_trips_a_pinned_manifest() {
    let out = canonicalize_static(pinned_manifest().as_bytes(), &ParseOptions::default())
        .expect("a fully pinned manifest should canonicalize");
    assert!(
        out.contains(&format!(r#"revision="{}""#, sha('a'))),
        "pins must survive untouched: {out}"
    );
    assert!(
        out.contains(&format!(r#"revision="{}""#, sha('b'))),
        "pins must survive untouched: {out}"
    );
    assert!(
        !out.contains("dest-branch="),
        "an already-pinned project derives no dest-branch: {out}"
    );
}

/// The instrument compares canonical forms, so canonicalization has to be a
/// fixed point or every comparison would drift on each pass.
#[test]
fn canonicalization_is_idempotent() {
    let once = canonicalize_static(pinned_manifest().as_bytes(), &ParseOptions::default())
        .expect("first pass should canonicalize");
    let twice = canonicalize_static(once.as_bytes(), &ParseOptions::default())
        .expect("second pass should canonicalize");
    assert_eq!(once, twice, "canonicalize_static must be a fixed point");
}

/// A symbolic revision means the caller handed us a `default.xml`, not a
/// `static.xml`. Resolving it would invent a pin, so it fails instead.
#[test]
fn rejects_an_unpinned_project() {
    let xml = br#"<manifest>
        <remote name="origin" fetch="https://example.com" />
        <default remote="origin" revision="main" />
        <project name="foo" path="foo" />
    </manifest>"#;
    let err = canonicalize_static(xml, &ParseOptions::default())
        .expect_err("an unpinned project must not be silently resolved");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("not pinned"),
        "the error should name the unpinned-project case, got: {msg}"
    );
}

#[test]
fn rejects_an_include() {
    let xml = br#"<manifest>
        <include name="other.xml" />
    </manifest>"#;
    let err = canonicalize_static(xml, &ParseOptions::default())
        .expect_err("a static manifest is expected to be flat");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("other.xml"),
        "the error should name the include, got: {msg}"
    );
}

#[test]
fn strict_parse_is_caller_selectable() {
    let xml = br#"<manifest>
        <remote name="origin" fetch="https://example.com" />
        <default remote="origin" revision="main" />
        <bogus-element />
    </manifest>"#;
    assert!(
        canonicalize_static(xml, &ParseOptions::default()).is_ok(),
        "the permissive default must tolerate an unknown element"
    );
    assert!(
        canonicalize_static(xml, &ParseOptions { deny_unknown: true }).is_err(),
        "deny_unknown must reject the same document"
    );
}

#[test]
fn remotes_are_returned_name_sorted() {
    let xml = br#"<manifest>
        <remote name="zulu" fetch="https://z.example.com" />
        <remote name="alpha" fetch="https://a.example.com" />
        <remote name="mike" fetch="https://m.example.com" />
        <default remote="alpha" revision="main" />
        <project name="foo" path="foo" />
    </manifest>"#;
    let m = Manifest::from_xml(xml, &mut no_includes).expect("manifest should parse");
    let names: Vec<&str> = m.remotes().map(|r| r.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["alpha", "mike", "zulu"],
        "remotes() must match the serializer's name-sorted order"
    );
}

/// `remote(&name)` was the only accessor, so the parity classifier could not
/// enumerate the domain it needed to compare.
#[test]
fn remotes_agrees_with_lookup_by_name() {
    let xml = br#"<manifest>
        <remote name="origin" fetch="https://example.com" />
        <remote name="mirror" fetch="https://mirror.example.com" />
        <default remote="origin" revision="main" />
        <project name="foo" path="foo" />
    </manifest>"#;
    let m = Manifest::from_xml(xml, &mut no_includes).expect("manifest should parse");
    assert_eq!(m.remotes().count(), 2, "both remotes must be enumerated");
    for remote in m.remotes() {
        let looked_up = m
            .remote(&remote.name)
            .unwrap_or_else(|| panic!("remote {} should be findable by name", remote.name));
        assert_eq!(
            looked_up.fetch_url, remote.fetch_url,
            "enumeration and lookup must agree"
        );
    }
}
