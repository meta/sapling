/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! `StaticOptions::suppress_dest_branch`.
//!
//! git-repo derives both `upstream` and `dest-branch` from a symbolic revision.
//! AOSP's incumbent generator only ever wrote `upstream`, so every live
//! `static.xml` has zero `dest-branch` attributes and parity with that corpus
//! needs the derived one suppressible.

use anyhow::Result;
use repo_manifest::StaticOptions;
use repo_manifest::to_static;
use repo_manifest::to_static_with_options;

fn no_includes(name: &str) -> Result<Vec<u8>> {
    anyhow::bail!("Unexpected include: {name}")
}

fn sha() -> String {
    "a".repeat(40)
}

const SYMBOLIC: &[u8] = br#"<manifest>
    <remote name="origin" fetch="https://example.com" />
    <default remote="origin" revision="main" />
    <project name="foo" path="foo" />
</manifest>"#;

#[test]
fn derives_dest_branch_by_default() {
    let out = to_static(SYMBOLIC, &mut no_includes, |_p| Ok(sha()))
        .expect("to_static should serialize a symbolic manifest");
    assert!(
        out.contains(r#"dest-branch="main""#),
        "the default must keep git-repo parity and derive dest-branch: {out}"
    );
    assert!(
        out.contains(r#"upstream="main""#),
        "upstream is always derived: {out}"
    );
}

#[test]
fn suppression_drops_only_the_derived_attribute() {
    let options = StaticOptions {
        suppress_dest_branch: true,
        ..Default::default()
    };
    let out = to_static_with_options(SYMBOLIC, &mut no_includes, |_p| Ok(sha()), &options)
        .expect("to_static should serialize a symbolic manifest");
    assert!(
        !out.contains("dest-branch="),
        "the derived dest-branch must be suppressed: {out}"
    );
    assert!(
        out.contains(r#"upstream="main""#),
        "upstream must survive suppression -- repo sync needs it to fetch a branch ref: {out}"
    );
    assert!(
        out.contains(&format!(r#"revision="{}""#, sha())),
        "the resolved pin must still be written: {out}"
    );
}

/// Suppression targets the attribute git-repo invents, not one the manifest author
/// wrote down.
#[test]
fn suppression_keeps_an_explicit_dest_branch() {
    let xml = br#"<manifest>
        <remote name="origin" fetch="https://example.com" />
        <default remote="origin" revision="main" />
        <project name="foo" path="foo" dest-branch="release" />
    </manifest>"#;
    let options = StaticOptions {
        suppress_dest_branch: true,
        ..Default::default()
    };
    let out = to_static_with_options(xml, &mut no_includes, |_p| Ok(sha()), &options)
        .expect("to_static should serialize a symbolic manifest");
    assert!(
        out.contains(r#"dest-branch="release""#),
        "an explicitly declared dest-branch is author intent and must survive: {out}"
    );
}

/// An already-pinned project has no symbolic revision to derive from, so the option
/// is inert on the reformat path the reconciler takes.
#[test]
fn suppression_is_a_noop_on_an_already_pinned_project() {
    let xml = format!(
        r#"<manifest>
        <remote name="origin" fetch="https://example.com" />
        <default remote="origin" revision="main" />
        <project name="foo" path="foo" revision="{}" />
    </manifest>"#,
        sha()
    );
    let emitted = to_static(xml.as_bytes(), &mut no_includes, |_p| Ok(sha()))
        .expect("to_static should serialize a pinned manifest");
    let suppressed = to_static_with_options(
        xml.as_bytes(),
        &mut no_includes,
        |_p| Ok(sha()),
        &StaticOptions {
            suppress_dest_branch: true,
            ..Default::default()
        },
    )
    .expect("to_static should serialize a pinned manifest");
    assert_eq!(
        emitted, suppressed,
        "with no symbolic revision there is nothing to derive, so the option changes nothing"
    );
    assert!(
        !emitted.contains("dest-branch="),
        "a pinned project derives no dest-branch either way: {emitted}"
    );
}
