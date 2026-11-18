/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Strict attribute validation via a `quick_xml::Reader` event pre-scan.
//!
//! [`validate_no_unknown_attributes`] walks the manifest XML event stream and,
//! for every modelled element, checks each attribute name against a per-element
//! allowlist derived from `raw.rs`. An attribute not in the allowlist fails the
//! parse. This is the *attribute* half of strict parsing.
//!
//! Unknown *elements* are intentionally NOT rejected here — they are caught by
//! the permissive parse in `lib.rs` and the per-element `Unknown` checks in
//! `resolver.rs`. Accordingly, attributes on unknown elements are not checked
//! (the element itself is rejected regardless).

use anyhow::Result;
use quick_xml::Reader;
use quick_xml::events::BytesStart;
use quick_xml::events::Event;

/// Allowed attribute names for a modelled element, keyed by XML tag name.
///
/// Returns `None` for elements not modelled by `raw.rs` (their attributes are
/// not checked here).
///
/// Keep in sync with `raw.rs`: each arm mirrors the `@`-renamed attributes on
/// the corresponding `raw` struct. `test_strict_accepts_all_known_attributes`
/// guards against drift — if an attribute is added to a `raw` struct but not
/// here, that test fails because the attribute would be wrongly rejected.
fn allowed_attributes(element: &str) -> Option<&'static [&'static str]> {
    let allowed: &[&str] = match element {
        // raw::Manifest (root): no attributes.
        "manifest" => &[],
        // raw::Remote. `push` is a legacy Meta duplicate of `pushurl` that the git-repo
        // spec neither reads nor emits. It is modelled in `raw.rs` only so the resolver
        // can reject a divergent value; it is never copied to `types` or serialized, so
        // strict parse accepts the real AOSP manifests (~39% of active branches carry it)
        // while the serializer still emits only the spec-standard `pushurl`.
        "remote" => &[
            "name", "alias", "fetch", "pushurl", "review", "revision", "push",
        ],
        // raw::Defaults
        "default" => &[
            "remote",
            "revision",
            "dest-branch",
            "upstream",
            "sync-j",
            "sync-c",
            "sync-s",
            "sync-tags",
        ],
        // raw::Notice: text content only, no attributes.
        "notice" => &[],
        // raw::ManifestServer
        "manifest-server" => &["url"],
        // raw::Annotation
        "annotation" => &["name", "value", "keep"],
        // raw::CopyFile
        "copyfile" => &["src", "dest"],
        // raw::LinkFile
        "linkfile" => &["src", "dest"],
        // raw::Project
        "project" => &[
            "name",
            "remote",
            "revision",
            "path",
            "rebase",
            "sync-c",
            "sync-s",
            "sync-tags",
            "clone-depth",
            "dest-branch",
            "upstream",
            "groups",
            "force-path",
        ],
        // raw::ExtendProject
        "extend-project" => &[
            "name",
            "path",
            "groups",
            "revision",
            "remote",
            "dest-path",
            "dest-branch",
            "upstream",
            "base-rev",
        ],
        // raw::RemoveProject
        "remove-project" => &["name", "path", "optional", "base-rev"],
        // raw::RepoHooks
        "repo-hooks" => &["in-project", "enabled-list"],
        // raw::Superproject
        "superproject" => &["name", "remote", "revision"],
        // raw::Contactinfo
        "contactinfo" => &["bugurl"],
        // raw::Include
        "include" => &["name", "groups", "revision"],
        // raw::Submanifest
        "submanifest" => &[
            "name",
            "remote",
            "project",
            "manifest-name",
            "revision",
            "path",
            "groups",
            "default-groups",
        ],
        _ => return None,
    };
    Some(allowed)
}

/// Reject any attribute not modelled by `raw.rs`.
///
/// Walks `xml` as a stream of events; for every modelled start/empty element,
/// verifies each attribute name is in that element's allowlist. Fails with an
/// error naming the offending attribute and element.
pub(crate) fn validate_no_unknown_attributes(xml: &[u8]) -> Result<()> {
    let mut reader = Reader::from_reader(xml);
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => check_element(&e)?,
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(e) => anyhow::bail!("Failed to parse manifest XML (strict attribute scan): {e}"),
        }
        buf.clear();
    }
    Ok(())
}

/// Check a single element's attributes against its allowlist. Unknown elements
/// are skipped (their attributes are irrelevant; the element itself is rejected
/// elsewhere).
fn check_element(e: &BytesStart) -> Result<()> {
    let name = e.name();
    let Some(allowed) = allowed_attributes(name.as_ref()) else {
        return Ok(());
    };
    for attr in e.attributes() {
        let attr =
            attr.map_err(|err| anyhow::anyhow!("Malformed attribute (strict parse mode): {err}"))?;
        let key = attr.key;
        if !allowed.iter().any(|a| *a == key.as_ref()) {
            anyhow::bail!(
                "Unknown attribute '{}' on <{}> (strict parse mode)",
                key.as_ref(),
                name.as_ref(),
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drift guard: a manifest exercising every element and every attribute
    /// modelled by `raw` must pass strict validation. If an attribute is added
    /// to `raw` but not to `allowed_attributes`, that attribute would be
    /// rejected here and this test would fail.
    #[test]
    fn test_strict_accepts_all_known_attributes() {
        let xml = br#"
            <manifest>
                <remote name="origin" alias="o" fetch="https://e.com" push="https://p.com"
                        pushurl="https://p.com" review="https://r.com" revision="main">
                    <annotation name="a" value="v" keep="true" />
                </remote>
                <default remote="origin" revision="main" dest-branch="db" upstream="us"
                         sync-j="4" sync-c="true" sync-s="false" sync-tags="true" />
                <manifest-server url="https://ms.com" />
                <superproject name="super" remote="origin" revision="deadbeef" />
                <repo-hooks in-project="p" enabled-list="pre-upload" />
                <contactinfo bugurl="https://bugs.com" />
                <notice>hello</notice>
                <project name="p" remote="origin" revision="main" path="p" rebase="true"
                         sync-c="true" sync-s="false" sync-tags="true" clone-depth="1"
                         dest-branch="db" upstream="us" groups="g1,g2" force-path="true">
                    <copyfile src="a" dest="b" />
                    <linkfile src="c" dest="d" />
                    <annotation name="an" value="av" keep="false" />
                    <project name="sub" path="sub" />
                </project>
                <extend-project name="p" path="p" groups="g" revision="v2" remote="origin"
                                dest-path="np" dest-branch="db2" upstream="us2" base-rev="br">
                    <copyfile src="e" dest="f" />
                    <linkfile src="g" dest="h" />
                    <annotation name="ea" value="ev" />
                </extend-project>
                <remove-project name="rp" path="rpath" optional="true" base-rev="rbr" />
                <submanifest name="sm" remote="origin" project="proj" manifest-name="m.xml"
                             revision="v" path="sp" groups="sg" default-groups="dg" />
                <include name="inc.xml" groups="ig" revision="iv" />
            </manifest>
        "#;
        assert!(
            validate_no_unknown_attributes(xml).is_ok(),
            "strict validation should accept all modelled attributes: {:?}",
            validate_no_unknown_attributes(xml).err()
        );
    }

    /// Real AOSP manifests carry a legacy `<remote push="X" pushurl="X">` where `push`
    /// duplicates the standard `pushurl`. git-repo ignores `push`; the crate allowlists
    /// it as accept-and-ignore so strict parse does not reject real manifests.
    #[test]
    fn test_strict_accepts_remote_push() {
        let xml = br#"
            <manifest>
                <remote name="origin" fetch="https://e.com"
                        push="https://p.com" pushurl="https://p.com" />
            </manifest>
        "#;
        assert!(
            validate_no_unknown_attributes(xml).is_ok(),
            "strict validation should accept the legacy remote@push attribute"
        );
    }

    /// An unknown attribute on an element is rejected.
    #[test]
    fn test_strict_rejects_unknown_attribute_on_remote() {
        let xml = br#"
            <manifest>
                <remote name="origin" fetch="https://e.com" x-bogus="1" />
            </manifest>
        "#;
        assert!(validate_no_unknown_attributes(xml).is_err());
    }

    /// Attributes are checked on nested elements too, and the error names both
    /// the offending attribute and the element it appeared on.
    #[test]
    fn test_error_names_attribute_and_element() {
        let xml = br#"
            <manifest>
                <project name="p" path="p">
                    <copyfile src="a" dest="b" x-bogus="1" />
                </project>
            </manifest>
        "#;
        let err = validate_no_unknown_attributes(xml)
            .expect_err("nested unknown attribute should be rejected")
            .to_string();
        assert!(
            err.contains("x-bogus"),
            "error should name the attribute: {err}"
        );
        assert!(
            err.contains("copyfile"),
            "error should name the element: {err}"
        );
    }
}
