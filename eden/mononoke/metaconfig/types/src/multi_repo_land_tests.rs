/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use mononoke_macros::mononoke;

use crate::CommonConfig;

fn common(markers: &[(&str, &str)], legacy_prefix: Option<&str>) -> CommonConfig {
    CommonConfig {
        multi_repo_land_manifest_repos: markers
            .iter()
            .map(|(marker, manifest)| (marker.to_string(), manifest.to_string()))
            .collect(),
        rl_land_service_repo_prefix: legacy_prefix.map(str::to_string),
        ..Default::default()
    }
}

/// Longest marker wins; manifest repos route to themselves; no match is None.
#[mononoke::test]
fn manifest_repo_routing() {
    let aosp = common(
        &[
            ("aosp/", "aosp/manifest"),
            ("aosp/vendor/", "aosp/vendor-manifest"),
        ],
        None,
    );
    assert_eq!(
        aosp.multi_repo_land_manifest_repo("aosp/platform/build"),
        Some("aosp/manifest")
    );
    assert_eq!(
        aosp.multi_repo_land_manifest_repo("oculus/aosp/vendor/oculus"),
        Some("aosp/vendor-manifest"),
        "the longer, more specific marker must win, wherever it sits in the name"
    );
    assert_eq!(
        aosp.multi_repo_land_manifest_repo("aosp/manifest"),
        Some("aosp/manifest"),
        "a manifest repo routes to itself"
    );
    assert_eq!(
        aosp.multi_repo_land_manifest_repo("aosp/vendor-manifest"),
        Some("aosp/vendor-manifest"),
        "a manifest repo routes to itself even when another marker matches its name"
    );
    assert_eq!(aosp.multi_repo_land_manifest_repo("zephyr/kernel"), None);
    assert_eq!(
        aosp.multi_repo_land_manifest_repo("aosp"),
        None,
        "`aosp` does not contain `aosp/`"
    );
    assert_eq!(
        aosp.multi_repo_land_manifest_repo("aosp_extras/foo"),
        None,
        "`aosp_extras/` shares a prefix with `aosp/` but does not contain it"
    );
}

/// An exact marker takes one repo, not its name-extended neighbours; a
/// substring marker still takes the family.
#[mononoke::test]
fn exact_marker_names_one_repo() {
    let west = common(
        &[
            ("vr/firmware/", "vr/firmware/megaman"),
            ("zephyrproject-rtos/zephyr", "vr/firmware/megaman"),
        ],
        None,
    );
    assert_eq!(
        west.multi_repo_land_manifest_repo("vr/firmware/omega"),
        Some("vr/firmware/megaman")
    );
    assert_eq!(
        west.multi_repo_land_manifest_repo("zephyrproject-rtos/zephyr"),
        Some("vr/firmware/megaman")
    );
    assert_eq!(
        west.multi_repo_land_manifest_repo("zephyrproject-rtos/zephyr-lang-rust"),
        None
    );
    assert_eq!(
        west.multi_repo_land_manifest_repo("zephyrproject-rtos/cmsis"),
        None
    );
    assert_eq!(
        west.multi_repo_land_manifest_repo("mirror/zephyrproject-rtos/zephyr"),
        None,
        "an exact marker is the whole name, not a suffix"
    );
}

/// A manifest repo outside every marker is still diverted: it is named as a
/// value.
#[mononoke::test]
fn manifest_repo_named_only_as_a_value_diverts() {
    let config = common(&[("members/", "manifests/main")], None);
    assert_eq!(
        config.multi_repo_land_manifest_repo("manifests/main"),
        Some("manifests/main")
    );
    assert!(config.diverts_to_multi_repo_land("manifests/main"));
    assert!(config.diverts_to_multi_repo_land("members/a"));
    assert!(!config.diverts_to_multi_repo_land("manifests/other"));
}

/// The legacy prefix diverts on its own, as a substring, without routing the
/// repo to a manifest repo.
#[mononoke::test]
fn legacy_prefix_diverts_without_a_manifest_repo() {
    let legacy = common(&[], Some("aosp/"));
    assert!(legacy.diverts_to_multi_repo_land("aosp/platform/build"));
    assert!(legacy.diverts_to_multi_repo_land("oculus/aosp/vendor/oculus"));
    assert!(!legacy.diverts_to_multi_repo_land("vr/firmware/omega"));
    assert_eq!(
        legacy.multi_repo_land_manifest_repo("aosp/platform/build"),
        None
    );

    let both = common(&[("vr/firmware/", "vr/firmware/megaman")], Some("aosp/"));
    assert!(both.diverts_to_multi_repo_land("aosp/platform/build"));
    assert!(both.diverts_to_multi_repo_land("vr/firmware/omega"));
    assert!(!both.diverts_to_multi_repo_land("fbsource"));
}

/// No markers divert nothing; an empty legacy prefix is no prefix, not a
/// match for every repo.
#[mononoke::test]
fn no_markers_divert_nothing() {
    for config in [common(&[], None), common(&[], Some(""))] {
        for name in ["aosp/platform/build", "vr/firmware/omega", "fbsource", ""] {
            assert!(!config.diverts_to_multi_repo_land(name), "{name:?}");
            assert_eq!(config.multi_repo_land_manifest_repo(name), None, "{name:?}");
        }
    }
}
