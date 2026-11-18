/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Serialize the resolved manifest model back into static git-repo XML.
//!
//! Elements are emitted in git-repo `ToXml` order: `<notice>`, `<remote>`
//! (name-sorted), `<default>`, `<manifest-server>`, `<submanifest>`
//! (name-sorted), top-level `<project>` (name-sorted), then the trailing
//! singletons `<repo-hooks>`, `<superproject>`, `<contactinfo>`.

use anyhow::Result;
use quick_xml::events::BytesEnd;
use quick_xml::events::BytesStart;
use quick_xml::events::BytesText;
use quick_xml::events::Event;
use quick_xml::writer::Writer;

use crate::Manifest;
use crate::StaticOptions;
use crate::hash::is_git_hash;
use crate::types;

pub(crate) fn serialize_static(
    m: &Manifest,
    resolve_fn: impl Fn(&types::Project) -> Result<String>,
    options: &StaticOptions,
) -> Result<String> {
    let mut writer = Writer::new_with_indent(Vec::new(), b' ', 2);

    writer.write_event(Event::Start(BytesStart::new("manifest")))?;

    if let Some(notice) = m.notice.as_deref().filter(|s| !s.is_empty()) {
        writer.write_event(Event::Start(BytesStart::new("notice")))?;
        writer.write_event(Event::Text(BytesText::new(notice)))?;
        writer.write_event(Event::End(BytesEnd::new("notice")))?;
    }

    write_remotes(&mut writer, m, options)?;
    write_default(&mut writer, m)?;
    write_manifest_server(&mut writer, m)?;
    write_submanifests(&mut writer, m)?;
    write_projects(&mut writer, m, &resolve_fn, options)?;
    write_repo_hooks(&mut writer, m)?;
    write_superproject(&mut writer, m)?;
    write_contactinfo(&mut writer, m)?;

    writer.write_event(Event::End(BytesEnd::new("manifest")))?;

    Ok(String::from_utf8(writer.into_inner())?)
}

fn write_remotes(
    writer: &mut Writer<Vec<u8>>,
    m: &Manifest,
    options: &StaticOptions,
) -> Result<()> {
    let mut remotes: Vec<&types::Remote> = m.remotes.values().collect();
    remotes.sort_by(|a, b| a.name.cmp(&b.name));
    for remote in remotes {
        write_remote(writer, remote, options)?;
    }
    Ok(())
}

fn write_default(writer: &mut Writer<Vec<u8>>, m: &Manifest) -> Result<()> {
    let d = &m.defaults;
    let mut start = BytesStart::new("default");
    let mut have_default = false;

    if let Some(remote) = d.remote.as_deref().filter(|s| !s.is_empty()) {
        start.push_attribute(("remote", remote));
        have_default = true;
    }
    if let Some(revision) = d.revision.as_deref().filter(|s| !s.is_empty()) {
        start.push_attribute(("revision", revision));
        have_default = true;
    }
    if let Some(dest_branch) = d.dest_branch.as_deref().filter(|s| !s.is_empty()) {
        start.push_attribute(("dest-branch", dest_branch));
        have_default = true;
    }
    if let Some(upstream) = d.upstream.as_deref().filter(|s| !s.is_empty()) {
        start.push_attribute(("upstream", upstream));
        have_default = true;
    }
    if let Some(sync_j) = d.sync_j {
        let sync_j = sync_j.to_string();
        start.push_attribute(("sync-j", sync_j.as_str()));
        have_default = true;
    }
    if d.sync_c {
        start.push_attribute(("sync-c", "true"));
        have_default = true;
    }
    if d.sync_s {
        start.push_attribute(("sync-s", "true"));
        have_default = true;
    }
    if !d.sync_tags {
        start.push_attribute(("sync-tags", "false"));
        have_default = true;
    }

    if have_default {
        writer.write_event(Event::Empty(start))?;
    }

    Ok(())
}

fn write_manifest_server(writer: &mut Writer<Vec<u8>>, m: &Manifest) -> Result<()> {
    if let Some(url) = m.manifest_server.as_deref().filter(|s| !s.is_empty()) {
        let mut start = BytesStart::new("manifest-server");
        start.push_attribute(("url", url));
        writer.write_event(Event::Empty(start))?;
    }
    Ok(())
}

fn write_submanifests(writer: &mut Writer<Vec<u8>>, m: &Manifest) -> Result<()> {
    let mut submanifests: Vec<&types::Submanifest> = m.submanifests.iter().collect();
    submanifests.sort_by(|a, b| a.name.cmp(&b.name));
    for submanifest in submanifests {
        write_submanifest(writer, submanifest)?;
    }
    Ok(())
}

fn write_submanifest(writer: &mut Writer<Vec<u8>>, submanifest: &types::Submanifest) -> Result<()> {
    let mut start = BytesStart::new("submanifest");
    start.push_attribute(("name", submanifest.name.as_str()));
    if let Some(remote) = submanifest.remote.as_deref().filter(|s| !s.is_empty()) {
        start.push_attribute(("remote", remote));
    }
    if let Some(project) = submanifest.project.as_deref().filter(|s| !s.is_empty()) {
        start.push_attribute(("project", project));
    }
    if let Some(manifest_name) = submanifest
        .manifest_name
        .as_deref()
        .filter(|s| !s.is_empty())
    {
        start.push_attribute(("manifest-name", manifest_name));
    }
    if let Some(revision) = submanifest.revision.as_deref().filter(|s| !s.is_empty()) {
        start.push_attribute(("revision", revision));
    }
    if let Some(path) = submanifest.path.as_deref().filter(|s| !s.is_empty()) {
        start.push_attribute(("path", path));
    }
    if !submanifest.groups.is_empty() {
        let mut groups: Vec<&str> = submanifest.groups.iter().map(|g| g.as_str()).collect();
        groups.sort_unstable();
        let groups = groups.join(",");
        start.push_attribute(("groups", groups.as_str()));
    }
    if !submanifest.default_groups.is_empty() {
        let default_groups = submanifest.default_groups.join(",");
        start.push_attribute(("default-groups", default_groups.as_str()));
    }

    let kept: Vec<&types::Annotation> = submanifest.annotations.iter().filter(|a| a.keep).collect();
    write_element_with_annotations(writer, start, &kept)
}

fn write_remote(
    writer: &mut Writer<Vec<u8>>,
    remote: &types::Remote,
    options: &StaticOptions,
) -> Result<()> {
    let mut start = BytesStart::new("remote");
    start.push_attribute(("name", remote.name.as_str()));
    let fetch = options
        .remote_fetch_override
        .as_deref()
        .unwrap_or(remote.fetch_url.as_str());
    start.push_attribute(("fetch", fetch));
    if let Some(push_url) = remote.push_url.as_deref().filter(|s| !s.is_empty()) {
        start.push_attribute(("pushurl", push_url));
    }
    if let Some(alias) = remote.alias.as_deref().filter(|s| !s.is_empty()) {
        start.push_attribute(("alias", alias));
    }
    if let Some(review) = remote.review.as_deref().filter(|s| !s.is_empty()) {
        start.push_attribute(("review", review));
    }
    if let Some(revision) = remote.revision.as_deref().filter(|s| !s.is_empty()) {
        start.push_attribute(("revision", revision));
    }

    let kept: Vec<&types::Annotation> = remote.annotations.iter().filter(|a| a.keep).collect();
    write_element_with_annotations(writer, start, &kept)
}

/// Emit an element that carries only its kept `<annotation>` children. With no
/// kept annotations the element is written self-closing; otherwise the end tag
/// is derived from `start` so the open and close names cannot diverge.
fn write_element_with_annotations(
    writer: &mut Writer<Vec<u8>>,
    start: BytesStart<'_>,
    annotations: &[&types::Annotation],
) -> Result<()> {
    if annotations.is_empty() {
        writer.write_event(Event::Empty(start))?;
        return Ok(());
    }

    let end = start.to_end().into_owned();
    writer.write_event(Event::Start(start))?;
    for annotation in annotations {
        let mut ann = BytesStart::new("annotation");
        ann.push_attribute(("name", annotation.name.as_str()));
        ann.push_attribute(("value", annotation.value.as_str()));
        writer.write_event(Event::Empty(ann))?;
    }
    writer.write_event(Event::End(end))?;
    Ok(())
}

fn write_projects<F>(
    writer: &mut Writer<Vec<u8>>,
    m: &Manifest,
    resolve_fn: &F,
    options: &StaticOptions,
) -> Result<()>
where
    F: Fn(&types::Project) -> Result<String>,
{
    for project in m.projects() {
        write_project(writer, m, project, None, resolve_fn, options)?;
    }
    Ok(())
}

/// git-repo `_UnjoinName`/`_UnjoinRelpath` (call sites :666-667; `_UnjoinRelpath`
/// definition :2044-2045): strip the parent's prefix, mirroring
/// `os.path.relpath(child, parent)` for the simple `parent/child` names/relpaths
/// the resolver builds.
fn unjoin<'a>(parent: &str, child: &'a str) -> &'a str {
    child
        .strip_prefix(parent)
        .and_then(|rest| rest.strip_prefix('/'))
        .unwrap_or(child)
}

fn write_project<F>(
    writer: &mut Writer<Vec<u8>>,
    m: &Manifest,
    p: &types::Project,
    parent: Option<&types::Project>,
    resolve_fn: &F,
    options: &StaticOptions,
) -> Result<()>
where
    F: Fn(&types::Project) -> Result<String>,
{
    // A revision that is already a full sha1 is pinned as-is; git-repo only
    // re-resolves symbolic revisions ("only save the origin if not a sha1").
    let sha = if is_git_hash(&p.revision) {
        p.revision.clone()
    } else {
        resolve_fn(p)?
    };

    // For nested subprojects the resolver stored the parent-joined name/relpath;
    // strip the parent prefix for display, matching git-repo (:665-667).
    let (name, relpath) = match parent {
        Some(par) => (
            unjoin(par.name.as_str(), p.name.as_str()),
            unjoin(par.relpath(), p.relpath()),
        ),
        None => (p.name.as_str(), p.relpath()),
    };

    let mut start = BytesStart::new("project");
    start.push_attribute(("name", name));
    if relpath != name {
        start.push_attribute(("path", relpath));
    }
    // git-repo emits `remote` unless the project uses the default remote.
    if m.defaults.remote.as_deref().filter(|s| !s.is_empty()) != Some(p.remote_name.as_str()) {
        start.push_attribute(("remote", p.remote_name.as_str()));
    }
    start.push_attribute(("revision", sha.as_str()));

    // upstream (git-repo peg_rev :686-692): an explicit or default-inherited
    // upstream is emitted verbatim; otherwise it is derived from the symbolic
    // revision, but only when that revision resolved to a different sha.
    if let Some(upstream) = p.upstream.as_deref().filter(|s| !s.is_empty()) {
        // TODO(audit): confirm git-repo _ParseProject default-upstream inheritance vs design §11 real-manifest audit
        start.push_attribute(("upstream", upstream));
    } else if sha != p.revision {
        start.push_attribute(("upstream", p.revision.as_str()));
    }

    // dest-branch (git-repo peg_rev :694-698): mirrors the upstream rule. The
    // trailing git-repo block :713-714 re-asserts the same p.dest_branch in peg
    // mode, which is idempotent, so it needs no counterpart here.
    if let Some(dest_branch) = p.dest_branch.as_deref().filter(|s| !s.is_empty()) {
        // TODO(audit): confirm git-repo _ParseProject default-dest-branch inheritance vs design §11 real-manifest audit
        start.push_attribute(("dest-branch", dest_branch));
    } else if sha != p.revision && !options.suppress_dest_branch {
        start.push_attribute(("dest-branch", p.revision.as_str()));
    }

    // groups (git-repo :728-731): emit user groups only, dropping the
    // auto-groups git-repo derives (all, name:<name>, path:<relpath>) plus the
    // resolver-injected "default" (git-repo never stores "default" per-project,
    // so persisting it would diverge). Source is a HashSet, so sort for a
    // deterministic ordering.
    let auto_groups = [
        "all".to_string(),
        format!("name:{}", p.name),
        format!("path:{}", p.relpath()),
        "default".to_string(),
    ];
    let mut egroups: Vec<&str> = p
        .groups
        .iter()
        .map(String::as_str)
        .filter(|g| !auto_groups.iter().any(|a| a == g))
        .collect();
    egroups.sort_unstable();
    if !egroups.is_empty() {
        let groups = egroups.join(",");
        start.push_attribute(("groups", groups.as_str()));
    }

    // sync/clone conditionals (git-repo :740-750).
    if p.sync_c {
        start.push_attribute(("sync-c", "true"));
    }
    if p.sync_s {
        start.push_attribute(("sync-s", "true"));
    }
    if !p.sync_tags {
        start.push_attribute(("sync-tags", "false"));
    }
    if let Some(depth) = p.clone_depth {
        let depth = depth.to_string();
        start.push_attribute(("clone-depth", depth.as_str()));
    }

    let kept_annotations: Vec<&types::Annotation> =
        p.annotations.iter().filter(|a| a.keep).collect();
    let has_children = !p.copy_files.is_empty()
        || !p.link_files.is_empty()
        || !kept_annotations.is_empty()
        || !p.subprojects.is_empty();

    if !has_children {
        writer.write_event(Event::Empty(start))?;
        return Ok(());
    }

    // Derive the end tag from the start so the open/close names cannot diverge
    // (same safety as write_element_with_annotations).
    let end = start.to_end().into_owned();
    writer.write_event(Event::Start(start))?;

    // Child order inside <project> (git-repo :716-756): copyfile*, then
    // linkfile*, then keep-only annotation*, then nested subprojects.
    for cf in &p.copy_files {
        let mut ce = BytesStart::new("copyfile");
        ce.push_attribute(("src", cf.src.as_str()));
        ce.push_attribute(("dest", cf.dest.as_str()));
        writer.write_event(Event::Empty(ce))?;
    }
    for lf in &p.link_files {
        let mut le = BytesStart::new("linkfile");
        le.push_attribute(("src", lf.src.as_str()));
        le.push_attribute(("dest", lf.dest.as_str()));
        writer.write_event(Event::Empty(le))?;
    }
    for annotation in &kept_annotations {
        let mut ann = BytesStart::new("annotation");
        ann.push_attribute(("name", annotation.name.as_str()));
        ann.push_attribute(("value", annotation.value.as_str()));
        writer.write_event(Event::Empty(ann))?;
    }

    // Nested subprojects, emitted name-sorted (git-repo :754-756); recurse with
    // this project as the parent so the child names/relpaths are un-joined.
    let mut subprojects: Vec<&types::Project> =
        p.subprojects.iter().map(|id| &m.projects[id.0]).collect();
    subprojects.sort_by(|a, b| a.name.cmp(&b.name));
    for sub in subprojects {
        write_project(writer, m, sub, Some(p), resolve_fn, options)?;
    }

    writer.write_event(Event::End(end))?;
    Ok(())
}

fn write_repo_hooks(writer: &mut Writer<Vec<u8>>, m: &Manifest) -> Result<()> {
    let Some(rh) = m.repo_hooks.as_ref() else {
        return Ok(());
    };
    let mut start = BytesStart::new("repo-hooks");
    start.push_attribute(("in-project", rh.in_project.as_str()));
    // git-repo stores enabled-list as a comma/whitespace-split list (_ParseList
    // :562-568) and space-joins it on output (:767). Our model keeps the raw
    // string, so normalize it the same way here.
    let enabled = rh
        .enabled_list
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    start.push_attribute(("enabled-list", enabled.as_str()));
    writer.write_event(Event::Empty(start))?;
    Ok(())
}

fn write_superproject(writer: &mut Writer<Vec<u8>>, m: &Manifest) -> Result<()> {
    let Some(sp) = m.superproject.as_ref() else {
        return Ok(());
    };
    let mut start = BytesStart::new("superproject");
    start.push_attribute(("name", sp.name.as_str()));

    let default_remote = m.defaults.remote.as_deref().filter(|s| !s.is_empty());
    let sp_remote = sp.remote.as_deref().filter(|s| !s.is_empty());

    // git-repo emits `remote` unless it equals the default remote (:779-781).
    if let Some(remote) = sp_remote {
        if default_remote != Some(remote) {
            start.push_attribute(("remote", remote));
        }
    }

    // revision (git-repo :782-784): the "expected" revision is the superproject
    // remote's revision, falling back to the default remote then the default
    // revision. git-repo's `remote.revision or d.revisionExpr` treats an empty
    // string as falsy, so filter empties out at both stages (matching the remote
    // computation above and every other write_*). Emit `revision` only when that
    // expected value is absent or differs from the superproject's own revision.
    if let Some(revision) = sp.revision.as_deref().filter(|s| !s.is_empty()) {
        let remote_revision = sp_remote
            .or(default_remote)
            .and_then(|name| m.remotes.get(name))
            .and_then(|r| r.revision.as_deref())
            .filter(|s| !s.is_empty());
        let expected = remote_revision.or(m.defaults.revision.as_deref().filter(|s| !s.is_empty()));
        if expected != Some(revision) {
            start.push_attribute(("revision", revision));
        }
    }

    writer.write_event(Event::Empty(start))?;
    Ok(())
}

fn write_contactinfo(writer: &mut Writer<Vec<u8>>, m: &Manifest) -> Result<()> {
    let Some(ci) = m.contactinfo.as_ref() else {
        return Ok(());
    };
    // NOTE: git-repo emits <contactinfo> only when bugurl != Wrapper().BUG_URL
    // (:787); we emit whenever present. Benign difference, flagged for the G4
    // conformance step.
    let mut start = BytesStart::new("contactinfo");
    start.push_attribute(("bugurl", ci.bugurl.as_str()));
    writer.write_event(Event::Empty(start))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_includes(name: &str) -> Result<Vec<u8>> {
        anyhow::bail!("unexpected include: {name}")
    }

    fn dummy_resolve(_p: &types::Project) -> Result<String> {
        Ok("0".repeat(40))
    }

    #[test]
    fn test_serialize_skeleton_contains_manifest() {
        let xml = br#"<manifest></manifest>"#;
        let out = crate::to_static(xml, &mut no_includes, dummy_resolve).unwrap();
        assert!(
            out.contains("<manifest"),
            "output should contain a <manifest> element: {out}"
        );
    }

    #[test]
    fn test_serialize_notice_precedes_remotes() {
        let xml = br#"<manifest>
            <notice>Hi</notice>
            <remote name="origin" fetch="https://example.com/" />
        </manifest>"#;
        let out = crate::to_static(xml, &mut no_includes, dummy_resolve).unwrap();
        assert!(
            out.contains("<notice>Hi</notice>"),
            "output should contain the notice text: {out}"
        );
        let notice_pos = out.find("<notice").expect("notice element present");
        let remote_pos = out.find("<remote").expect("remote element present");
        assert!(
            notice_pos < remote_pos,
            "<notice> must be emitted before <remote>: {out}"
        );
    }

    #[test]
    fn test_serialize_remotes_name_sorted() {
        let xml = br#"<manifest>
            <remote name="zebra" fetch="https://z/" />
            <remote name="aardvark" fetch="https://a/" />
        </manifest>"#;
        let out = crate::to_static(xml, &mut no_includes, dummy_resolve).unwrap();
        let a_pos = out
            .find(r#"name="aardvark""#)
            .expect("aardvark remote present");
        let z_pos = out.find(r#"name="zebra""#).expect("zebra remote present");
        assert!(a_pos < z_pos, "remotes must be emitted name-sorted: {out}");
    }

    #[test]
    fn test_serialize_remote_attr_order() {
        let xml = br#"<manifest>
            <remote name="aosp" fetch="ssh://x" pushurl="ssh://y" alias="a" review="r" revision="rev" />
        </manifest>"#;
        let out = crate::to_static(xml, &mut no_includes, dummy_resolve).unwrap();
        assert!(
            out.contains(
                r#"<remote name="aosp" fetch="ssh://x" pushurl="ssh://y" alias="a" review="r" revision="rev""#
            ),
            "remote attributes must be in exact git-repo order: {out}"
        );
    }

    #[test]
    fn test_serialize_remote_keep_only_annotations() {
        let xml = br#"<manifest>
            <remote name="origin" fetch="https://example.com/">
                <annotation name="keepme" value="v1" keep="true" />
                <annotation name="dropme" value="v2" keep="false" />
            </remote>
        </manifest>"#;
        let out = crate::to_static(xml, &mut no_includes, dummy_resolve).unwrap();
        assert!(
            out.contains(r#"<annotation name="keepme" value="v1""#),
            "kept annotation must be emitted: {out}"
        );
        assert!(
            !out.contains("dropme"),
            "keep=false annotation must be dropped: {out}"
        );
    }

    #[test]
    fn test_serialize_default_attrs_and_order() {
        let xml = br#"<manifest>
            <remote name="aosp" fetch="https://example.com/" />
            <default remote="aosp" revision="main" dest-branch="db" upstream="us"
                     sync-j="4" sync-c="true" sync-s="true" sync-tags="false" />
        </manifest>"#;
        let out = crate::to_static(xml, &mut no_includes, dummy_resolve).unwrap();
        assert!(
            out.contains(r#"<default remote="aosp" revision="main""#),
            "default must carry remote and revision: {out}"
        );
        assert!(
            out.contains(
                r#"<default remote="aosp" revision="main" dest-branch="db" upstream="us" sync-j="4" sync-c="true" sync-s="true" sync-tags="false""#
            ),
            "default attributes must be in exact git-repo order: {out}"
        );
    }

    #[test]
    fn test_serialize_default_omits_false_and_none_conditionals() {
        let xml = br#"<manifest>
            <remote name="aosp" fetch="https://example.com/" />
            <default remote="aosp" revision="main" />
        </manifest>"#;
        let out = crate::to_static(xml, &mut no_includes, dummy_resolve).unwrap();
        assert!(
            out.contains(r#"<default remote="aosp" revision="main""#),
            "default should still be emitted: {out}"
        );
        assert!(!out.contains("sync-c"), "sync-c omitted when false: {out}");
        assert!(!out.contains("sync-s"), "sync-s omitted when false: {out}");
        assert!(
            !out.contains("sync-tags"),
            "sync-tags omitted when true (the default): {out}"
        );
        assert!(!out.contains("sync-j"), "sync-j omitted when unset: {out}");
    }

    #[test]
    fn test_serialize_manifest_server_emitted() {
        let xml = br#"<manifest>
            <manifest-server url="http://s/" />
        </manifest>"#;
        let out = crate::to_static(xml, &mut no_includes, dummy_resolve).unwrap();
        assert!(
            out.contains(r#"<manifest-server url="http://s/""#),
            "manifest-server must be emitted with its url: {out}"
        );
    }

    #[test]
    fn test_serialize_submanifests_sorted_with_attrs_and_annotations() {
        let xml = br#"<manifest>
            <submanifest name="s2" remote="origin" path="p2" />
            <submanifest name="s" remote="origin" project="proj" manifest-name="m.xml"
                         revision="v" path="p" groups="g2,g1" default-groups="dg">
                <annotation name="k" value="v" keep="true" />
                <annotation name="drop" value="x" keep="false" />
            </submanifest>
        </manifest>"#;
        let out = crate::to_static(xml, &mut no_includes, dummy_resolve).unwrap();
        // name-sorted: "s" before "s2"
        let s_pos = out.find(r#"name="s""#).expect("submanifest s present");
        let s2_pos = out.find(r#"name="s2""#).expect("submanifest s2 present");
        assert!(
            s_pos < s2_pos,
            "submanifests must be emitted name-sorted: {out}"
        );
        // attr order, groups sorted+comma-joined, default-groups joined
        assert!(
            out.contains(
                r#"<submanifest name="s" remote="origin" project="proj" manifest-name="m.xml" revision="v" path="p" groups="g1,g2" default-groups="dg""#
            ),
            "submanifest attributes must be in exact git-repo order: {out}"
        );
        // keep-only annotations
        assert!(
            out.contains(r#"<annotation name="k" value="v""#),
            "kept submanifest annotation must be emitted: {out}"
        );
        assert!(
            !out.contains(r#"name="drop""#),
            "keep=false submanifest annotation must be dropped: {out}"
        );
    }

    #[test]
    fn test_serialize_empty_notice_omits_element() {
        let xml = br#"<manifest><notice></notice></manifest>"#;
        let out = crate::to_static(xml, &mut no_includes, dummy_resolve).unwrap();
        assert!(
            !out.contains("<notice"),
            "an empty notice must not be emitted: {out}"
        );
    }

    #[test]
    fn test_serialize_remote_omits_empty_optional_attr() {
        let xml = br#"<manifest>
            <remote name="origin" fetch="https://example.com/" review="" />
        </manifest>"#;
        let out = crate::to_static(xml, &mut no_includes, dummy_resolve).unwrap();
        assert!(
            out.contains(r#"<remote name="origin""#),
            "remote should still be emitted: {out}"
        );
        assert!(
            !out.contains("review="),
            "an empty remote optional attr must be omitted: {out}"
        );
    }

    #[test]
    fn test_serialize_submanifest_omits_empty_optional_attr() {
        let xml = br#"<manifest>
            <submanifest name="s" path="p" project="" />
        </manifest>"#;
        let out = crate::to_static(xml, &mut no_includes, dummy_resolve).unwrap();
        assert!(
            out.contains(r#"<submanifest name="s""#),
            "submanifest should still be emitted: {out}"
        );
        assert!(
            !out.contains("project="),
            "an empty submanifest optional attr must be omitted: {out}"
        );
    }

    #[test]
    fn test_serialize_manifest_server_omits_empty_url() {
        let xml = br#"<manifest><manifest-server url="" /></manifest>"#;
        let out = crate::to_static(xml, &mut no_includes, dummy_resolve).unwrap();
        assert!(
            !out.contains("<manifest-server"),
            "manifest-server must not be emitted when url is empty: {out}"
        );
    }

    // --- G3.5: <project> core opening tag (pinned revision) ---

    /// A fixed 40-hex SHA a resolve_fn can return so assertions can pin an
    /// exact resolved revision value.
    const PINNED_SHA: &str = "1234567890abcdef1234567890abcdef12345678";

    fn resolve_pinned(_p: &types::Project) -> Result<String> {
        Ok(PINNED_SHA.to_string())
    }

    #[test]
    fn test_serialize_project_core_pinned_revision() {
        let xml = br#"<manifest>
            <remote name="origin" fetch="https://example.com/" />
            <default remote="origin" revision="main" />
            <project name="platform/build" path="build" />
        </manifest>"#;
        let out = crate::to_static(xml, &mut no_includes, resolve_pinned).unwrap();
        assert!(
            out.contains(&format!(
                r#"<project name="platform/build" path="build" revision="{PINNED_SHA}""#
            )),
            "project must emit name, path, and the resolve_fn-provided revision: {out}"
        );
    }

    #[test]
    fn test_serialize_project_omits_path_when_relpath_equals_name() {
        // No `path` attribute → path defaults to the name, so relpath == name.
        let xml = br#"<manifest>
            <remote name="origin" fetch="https://example.com/" />
            <default remote="origin" revision="main" />
            <project name="platform/build" />
        </manifest>"#;
        let out = crate::to_static(xml, &mut no_includes, resolve_pinned).unwrap();
        // revision immediately follows name → no path attribute emitted.
        assert!(
            out.contains(&format!(
                r#"<project name="platform/build" revision="{PINNED_SHA}""#
            )),
            "path must be omitted when relpath == name: {out}"
        );
    }

    #[test]
    fn test_serialize_project_omits_default_remote() {
        let xml = br#"<manifest>
            <remote name="origin" fetch="https://example.com/" />
            <default remote="origin" revision="main" />
            <project name="build" />
        </manifest>"#;
        let out = crate::to_static(xml, &mut no_includes, resolve_pinned).unwrap();
        // remote == default remote → no remote attribute between name and revision.
        assert!(
            out.contains(&format!(r#"<project name="build" revision="{PINNED_SHA}""#)),
            "a project on the default remote must omit the remote attribute: {out}"
        );
    }

    #[test]
    fn test_serialize_project_emits_non_default_remote() {
        let xml = br#"<manifest>
            <remote name="origin" fetch="https://example.com/" />
            <remote name="upstream" fetch="https://upstream.example.com/" />
            <default remote="origin" revision="main" />
            <project name="build" remote="upstream" />
        </manifest>"#;
        let out = crate::to_static(xml, &mut no_includes, resolve_pinned).unwrap();
        assert!(
            out.contains(&format!(
                r#"<project name="build" remote="upstream" revision="{PINNED_SHA}""#
            )),
            "a non-default remote must be emitted between name and revision: {out}"
        );
    }

    // --- G3.6: upstream rule (peg_rev) + default-upstream inheritance ---

    #[test]
    fn test_serialize_project_derives_upstream_from_symbolic_revision() {
        // revision="main" is symbolic; resolve_fn pins it to a SHA != "main",
        // so git-repo derives upstream="main" (the original revisionExpr).
        let xml = br#"<manifest>
            <remote name="origin" fetch="https://example.com/" />
            <default remote="origin" revision="main" />
            <project name="build" />
        </manifest>"#;
        let out = crate::to_static(xml, &mut no_includes, resolve_pinned).unwrap();
        assert!(
            out.contains(&format!(
                r#"<project name="build" revision="{PINNED_SHA}" upstream="main""#
            )),
            "a branch-tracked project must derive upstream from its symbolic revision: {out}"
        );
    }

    #[test]
    fn test_serialize_project_emits_inherited_default_upstream() {
        // <default upstream="..."> is inherited onto every project by the
        // resolver, and the inherited value wins over the derived one.
        let xml = br#"<manifest>
            <remote name="origin" fetch="https://example.com/" />
            <default remote="origin" revision="main" upstream="refs/heads/x" />
            <project name="build" />
        </manifest>"#;
        let out = crate::to_static(xml, &mut no_includes, resolve_pinned).unwrap();
        assert!(
            out.contains(&format!(
                r#"<project name="build" revision="{PINNED_SHA}" upstream="refs/heads/x""#
            )),
            "an inherited default upstream must be emitted: {out}"
        );
        assert!(
            !out.contains(r#"upstream="main""#),
            "the inherited upstream must win over the derived revisionExpr: {out}"
        );
    }

    // --- G3.7: dest-branch rule (peg_rev) ---

    #[test]
    fn test_serialize_project_emits_explicit_dest_branch() {
        let xml = br#"<manifest>
            <remote name="origin" fetch="https://example.com/" />
            <default remote="origin" revision="main" />
            <project name="build" dest-branch="release-1" />
        </manifest>"#;
        let out = crate::to_static(xml, &mut no_includes, resolve_pinned).unwrap();
        assert!(
            out.contains(&format!(
                r#"<project name="build" revision="{PINNED_SHA}" upstream="main" dest-branch="release-1""#
            )),
            "an explicit dest-branch must be emitted after upstream: {out}"
        );
        assert!(
            !out.contains(r#"dest-branch="main""#),
            "an explicit dest-branch must win over the derived revisionExpr: {out}"
        );
    }

    #[test]
    fn test_serialize_project_derives_dest_branch_from_symbolic_revision() {
        // No dest-branch on project or default → derived from the symbolic
        // revision, mirroring the upstream derivation.
        let xml = br#"<manifest>
            <remote name="origin" fetch="https://example.com/" />
            <default remote="origin" revision="stable" />
            <project name="build" />
        </manifest>"#;
        let out = crate::to_static(xml, &mut no_includes, resolve_pinned).unwrap();
        assert!(
            out.contains(&format!(
                r#"<project name="build" revision="{PINNED_SHA}" upstream="stable" dest-branch="stable""#
            )),
            "a branch-tracked project without dest-branch must derive it from its symbolic revision: {out}"
        );
    }

    // --- G3.8: is_git_hash hash-pinned exemption ---

    #[test]
    fn test_serialize_hash_pinned_project_bypasses_resolve_fn() {
        let pinned = "c".repeat(40);
        let tracked_sha = "d".repeat(40);
        let xml = format!(
            r#"<manifest>
            <remote name="origin" fetch="https://example.com/" />
            <default remote="origin" revision="main" />
            <project name="pinned" revision="{pinned}" />
            <project name="tracked" />
        </manifest>"#
        );
        // Panics if the serializer ever calls back for the hash-pinned project,
        // proving the is_git_hash short-circuit bypasses resolve_fn entirely.
        let resolve_fn = |p: &types::Project| -> Result<String> {
            assert_ne!(
                p.name, "pinned",
                "resolve_fn must not be called for a hash-pinned project"
            );
            Ok("d".repeat(40))
        };
        let out = crate::to_static(xml.as_bytes(), &mut no_includes, resolve_fn).unwrap();

        // Hash-pinned project keeps its revision verbatim, with no derived attrs.
        let pinned_start = out
            .find(r#"<project name="pinned""#)
            .expect("pinned project emitted");
        let rel = &out[pinned_start..];
        let pinned_tag = &rel[..rel.find("/>").expect("pinned tag is self-closing")];
        assert!(
            pinned_tag.contains(&format!(r#"revision="{pinned}""#)),
            "hash-pinned revision must be emitted verbatim: {out}"
        );
        assert!(
            !pinned_tag.contains("upstream"),
            "hash-pinned project must not derive an upstream: {out}"
        );
        assert!(
            !pinned_tag.contains("dest-branch"),
            "hash-pinned project must not derive a dest-branch: {out}"
        );

        // Branch-tracked project is resolved via resolve_fn, with derived attrs.
        assert!(
            out.contains(&format!(
                r#"<project name="tracked" revision="{tracked_sha}" upstream="main" dest-branch="main""#
            )),
            "branch-tracked project must be resolved via resolve_fn: {out}"
        );
    }

    #[test]
    fn test_serialize_hash_pinned_project_keeps_explicit_upstream() {
        let pinned = "e".repeat(40);
        let xml = format!(
            r#"<manifest>
            <remote name="origin" fetch="https://example.com/" />
            <default remote="origin" revision="main" />
            <project name="pinned" revision="{pinned}" upstream="main" />
        </manifest>"#
        );
        // resolve_pinned would return a different sha; a hash-pinned revision
        // bypasses it, and the explicit upstream must still be honored.
        let out = crate::to_static(xml.as_bytes(), &mut no_includes, resolve_pinned).unwrap();
        assert!(
            out.contains(&format!(
                r#"<project name="pinned" revision="{pinned}" upstream="main""#
            )),
            "a hash-pinned project must still emit its explicit upstream: {out}"
        );
    }

    // --- G3.9: project groups/sync/clone-depth attrs ---

    /// Return the start tag (`<project ... >` or `<project ... />`) for the
    /// named project, so assertions can be scoped to a single project.
    fn project_tag<'a>(out: &'a str, name: &str) -> &'a str {
        let needle = format!(r#"<project name="{name}""#);
        let start = out
            .find(&needle)
            .unwrap_or_else(|| panic!("project {name:?} not found in {out}"));
        let rel = &out[start..];
        let end = rel.find('>').expect("project start tag must close");
        &rel[..=end]
    }

    #[test]
    fn test_serialize_project_groups_user_only_sorted() {
        // User groups are emitted sorted and comma-joined, excluding the
        // auto-groups (all, name:X, path:Y) and the resolver-injected "default".
        let xml = r#"<manifest>
            <remote name="origin" fetch="https://example.com/" />
            <default remote="origin" revision="main" />
            <project name="build" groups="zeta,alpha,mu" />
        </manifest>"#;
        let out = crate::to_static(xml.as_bytes(), &mut no_includes, resolve_pinned).unwrap();
        assert!(
            out.contains(r#"groups="alpha,mu,zeta""#),
            "user groups must be sorted and comma-joined, excluding auto-groups: {out}"
        );
    }

    #[test]
    fn test_serialize_project_groupless_omits_groups_attr() {
        // NEGATIVE (critical): a groupless project must emit NO groups attribute.
        // The resolver injects "default" into every project's group set, so
        // without excluding it the serializer would wrongly emit groups="default".
        let xml = r#"<manifest>
            <remote name="origin" fetch="https://example.com/" />
            <default remote="origin" revision="main" />
            <project name="build" />
        </manifest>"#;
        let out = crate::to_static(xml.as_bytes(), &mut no_includes, resolve_pinned).unwrap();
        assert!(
            !out.contains("groups="),
            "a groupless project must not emit a groups attribute (proves the \"default\" exclusion): {out}"
        );
    }

    #[test]
    fn test_serialize_project_preserves_notdefault_group() {
        // "notdefault" is a real user group (it also suppresses the injected
        // "default"), so it must survive into the emitted groups attribute.
        let xml = r#"<manifest>
            <remote name="origin" fetch="https://example.com/" />
            <default remote="origin" revision="main" />
            <project name="build" groups="notdefault" />
        </manifest>"#;
        let out = crate::to_static(xml.as_bytes(), &mut no_includes, resolve_pinned).unwrap();
        assert!(
            out.contains(r#"groups="notdefault""#),
            "a group literally named \"notdefault\" must be preserved: {out}"
        );
    }

    #[test]
    fn test_serialize_project_sync_c_conditional() {
        let xml = r#"<manifest>
            <remote name="origin" fetch="https://example.com/" />
            <default remote="origin" revision="main" />
            <project name="on" sync-c="true" />
            <project name="off" />
        </manifest>"#;
        let out = crate::to_static(xml.as_bytes(), &mut no_includes, resolve_pinned).unwrap();
        assert!(
            project_tag(&out, "on").contains(r#"sync-c="true""#),
            "sync-c=\"true\" must be emitted when set: {out}"
        );
        assert!(
            !project_tag(&out, "off").contains("sync-c"),
            "sync-c must be omitted when false: {out}"
        );
    }

    #[test]
    fn test_serialize_project_sync_s_conditional() {
        let xml = r#"<manifest>
            <remote name="origin" fetch="https://example.com/" />
            <default remote="origin" revision="main" />
            <project name="on" sync-s="true" />
            <project name="off" />
        </manifest>"#;
        let out = crate::to_static(xml.as_bytes(), &mut no_includes, resolve_pinned).unwrap();
        assert!(
            project_tag(&out, "on").contains(r#"sync-s="true""#),
            "sync-s=\"true\" must be emitted when set: {out}"
        );
        assert!(
            !project_tag(&out, "off").contains("sync-s"),
            "sync-s must be omitted when false: {out}"
        );
    }

    #[test]
    fn test_serialize_project_sync_tags_conditional() {
        // sync-tags defaults to true; only the explicit "false" is emitted.
        let xml = r#"<manifest>
            <remote name="origin" fetch="https://example.com/" />
            <default remote="origin" revision="main" />
            <project name="off" sync-tags="false" />
            <project name="on" />
        </manifest>"#;
        let out = crate::to_static(xml.as_bytes(), &mut no_includes, resolve_pinned).unwrap();
        assert!(
            project_tag(&out, "off").contains(r#"sync-tags="false""#),
            "sync-tags=\"false\" must be emitted when sync_tags is false: {out}"
        );
        assert!(
            !project_tag(&out, "on").contains("sync-tags"),
            "sync-tags must be omitted when true (the default): {out}"
        );
    }

    #[test]
    fn test_serialize_project_clone_depth_conditional() {
        let xml = r#"<manifest>
            <remote name="origin" fetch="https://example.com/" />
            <default remote="origin" revision="main" />
            <project name="withdepth" clone-depth="3" />
            <project name="nodepth" />
        </manifest>"#;
        let out = crate::to_static(xml.as_bytes(), &mut no_includes, resolve_pinned).unwrap();
        assert!(
            project_tag(&out, "withdepth").contains(r#"clone-depth="3""#),
            "clone-depth must be emitted with its value when set: {out}"
        );
        assert!(
            !project_tag(&out, "nodepth").contains("clone-depth"),
            "clone-depth must be omitted when unset: {out}"
        );
    }

    // --- G3.10: project child elements + nested subprojects ---

    #[test]
    fn test_serialize_project_copyfile_then_linkfile_as_children() {
        let xml = r#"<manifest>
            <remote name="origin" fetch="https://example.com/" />
            <default remote="origin" revision="main" />
            <project name="build">
                <copyfile src="core/root.mk" dest="Makefile" />
                <linkfile src="scripts/foo" dest="foo" />
            </project>
        </manifest>"#;
        let out = crate::to_static(xml.as_bytes(), &mut no_includes, resolve_pinned).unwrap();
        // A project with children must be emitted open/close, not self-closing.
        assert!(
            out.contains("</project>"),
            "a project with children must have a close tag: {out}"
        );
        let cp = out
            .find(r#"<copyfile src="core/root.mk" dest="Makefile""#)
            .expect("copyfile present");
        let lp = out
            .find(r#"<linkfile src="scripts/foo" dest="foo""#)
            .expect("linkfile present");
        assert!(cp < lp, "copyfile must be emitted before linkfile: {out}");
        let open = out
            .find(r#"<project name="build""#)
            .expect("project present");
        let close = out.find("</project>").expect("project close present");
        assert!(
            open < cp && lp < close,
            "copyfile/linkfile must be children inside <project>...</project>: {out}"
        );
    }

    #[test]
    fn test_serialize_project_annotation_keep_only_after_copy_link() {
        let xml = r#"<manifest>
            <remote name="origin" fetch="https://example.com/" />
            <default remote="origin" revision="main" />
            <project name="build">
                <copyfile src="a" dest="b" />
                <annotation name="keepme" value="v1" keep="true" />
                <annotation name="dropme" value="v2" keep="false" />
            </project>
        </manifest>"#;
        let out = crate::to_static(xml.as_bytes(), &mut no_includes, resolve_pinned).unwrap();
        assert!(
            out.contains(r#"<annotation name="keepme" value="v1""#),
            "a kept annotation must be emitted: {out}"
        );
        assert!(
            !out.contains("dropme"),
            "a keep=false annotation must be dropped: {out}"
        );
        let cp = out.find("<copyfile").expect("copyfile present");
        let ap = out.find("<annotation").expect("annotation present");
        assert!(
            cp < ap,
            "annotation must be emitted after copyfile/linkfile: {out}"
        );
    }

    #[test]
    fn test_serialize_nested_subproject_unjoined() {
        let xml = r#"<manifest>
            <remote name="origin" fetch="https://example.com/" />
            <default remote="origin" revision="main" />
            <project name="parent" path="parent-path">
                <project name="child" path="child-path" />
            </project>
        </manifest>"#;
        let out = crate::to_static(xml.as_bytes(), &mut no_includes, resolve_pinned).unwrap();
        // The child is emitted with its parent prefix stripped (git-repo
        // _UnjoinName/_UnjoinRelpath) — short name "child", relpath "child-path".
        assert!(
            out.contains(r#"<project name="child" path="child-path""#),
            "a nested subproject must round-trip with un-joined name/relpath: {out}"
        );
        assert!(
            !out.contains(r#"name="parent/child""#),
            "a subproject name must be un-joined, not the full parent/child: {out}"
        );
        // The auto-group exclusion must key off the FULL name/relpath (not the
        // un-joined ones); otherwise name:parent/child / path:... would leak.
        assert!(
            !project_tag(&out, "child").contains("groups="),
            "a subproject's auto-groups must be excluded using its full name/relpath: {out}"
        );
        let parent_open = out
            .find(r#"<project name="parent""#)
            .expect("parent project present");
        let child = out
            .find(r#"<project name="child""#)
            .expect("child project present");
        let parent_close = out.find("</project>").expect("parent close present");
        assert!(
            parent_open < child && child < parent_close,
            "a subproject must be nested inside the parent <project>: {out}"
        );
    }

    // --- G3.11: post-project singletons (<repo-hooks>, <superproject>, <contactinfo>) ---

    /// Return the `<superproject ... />` start tag so assertions can be scoped to
    /// it (the manifest carries `revision`/`remote` on other elements too).
    fn superproject_tag(out: &str) -> &str {
        let start = out.find("<superproject").expect("superproject present");
        let rel = &out[start..];
        let end = rel.find('>').expect("superproject tag must close");
        &rel[..=end]
    }

    #[test]
    fn test_serialize_repo_hooks_space_joined_enabled_list() {
        let xml = r#"<manifest>
            <remote name="origin" fetch="https://example.com/" />
            <default remote="origin" revision="main" />
            <project name="hooks" />
            <repo-hooks in-project="hooks" enabled-list="pre-upload, commit-msg" />
        </manifest>"#;
        let out = crate::to_static(xml.as_bytes(), &mut no_includes, resolve_pinned).unwrap();
        assert!(
            out.contains(r#"<repo-hooks in-project="hooks" enabled-list="pre-upload commit-msg""#),
            "repo-hooks must emit in-project and a space-joined enabled-list: {out}"
        );
        assert!(
            !out.contains("pre-upload, commit-msg"),
            "the comma-separated enabled-list must be normalized to space-joined: {out}"
        );
    }

    #[test]
    fn test_serialize_superproject_omits_default_remote_and_matching_revision() {
        // remote "origin" == default remote → remote omitted. The resolved
        // revision (default "main") equals the superproject revision → omitted.
        let xml = r#"<manifest>
            <remote name="origin" fetch="https://example.com/" />
            <default remote="origin" revision="main" />
            <superproject name="s" remote="origin" revision="main" />
        </manifest>"#;
        let out = crate::to_static(xml.as_bytes(), &mut no_includes, resolve_pinned).unwrap();
        let tag = superproject_tag(&out);
        assert!(
            tag.contains(r#"name="s""#),
            "superproject name must be emitted: {out}"
        );
        assert!(
            !tag.contains("remote="),
            "remote must be omitted when it equals the default remote: {out}"
        );
        assert!(
            !tag.contains("revision="),
            "revision must be omitted when it equals the remote/default revision: {out}"
        );
    }

    #[test]
    fn test_serialize_superproject_emits_revision_when_differs() {
        // remote "origin" == default → remote omitted; the superproject revision
        // differs from the resolved default ("main") → revision emitted verbatim.
        let xml = r#"<manifest>
            <remote name="origin" fetch="https://example.com/" />
            <default remote="origin" revision="main" />
            <superproject name="s" remote="origin" revision="refs/heads/release" />
        </manifest>"#;
        let out = crate::to_static(xml.as_bytes(), &mut no_includes, resolve_pinned).unwrap();
        let tag = superproject_tag(&out);
        assert!(
            tag.contains(r#"revision="refs/heads/release""#),
            "a differing revision must be emitted: {out}"
        );
        assert!(
            !tag.contains("remote="),
            "remote must be omitted when it equals the default remote: {out}"
        );
    }

    #[test]
    fn test_serialize_superproject_emits_non_default_remote() {
        // remote "other" != default "origin" → remote emitted.
        let xml = r#"<manifest>
            <remote name="origin" fetch="https://example.com/" />
            <remote name="other" fetch="https://other.example.com/" />
            <default remote="origin" revision="main" />
            <superproject name="s" remote="other" revision="main" />
        </manifest>"#;
        let out = crate::to_static(xml.as_bytes(), &mut no_includes, resolve_pinned).unwrap();
        let tag = superproject_tag(&out);
        assert!(
            tag.contains(r#"remote="other""#),
            "a non-default superproject remote must be emitted: {out}"
        );
    }

    #[test]
    fn test_serialize_superproject_treats_empty_remote_revision_as_absent() {
        // The effective remote's revision is empty (falsy in git-repo), so the
        // expected revision falls back to the default ("main"), which equals the
        // superproject revision → the revision attribute is omitted.
        let xml = r#"<manifest>
            <remote name="origin" fetch="https://example.com/" revision="" />
            <default remote="origin" revision="main" />
            <superproject name="s" remote="origin" revision="main" />
        </manifest>"#;
        let out = crate::to_static(xml.as_bytes(), &mut no_includes, resolve_pinned).unwrap();
        let tag = superproject_tag(&out);
        assert!(
            !tag.contains("revision="),
            "an empty remote revision must be treated as absent so the default fallback applies: {out}"
        );
    }

    #[test]
    fn test_serialize_contactinfo_bugurl() {
        let xml = r#"<manifest>
            <remote name="origin" fetch="https://example.com/" />
            <default remote="origin" revision="main" />
            <contactinfo bugurl="https://bugs.example.com/repo" />
        </manifest>"#;
        let out = crate::to_static(xml.as_bytes(), &mut no_includes, resolve_pinned).unwrap();
        assert!(
            out.contains(r#"<contactinfo bugurl="https://bugs.example.com/repo""#),
            "contactinfo must be emitted with its bugurl: {out}"
        );
    }

    /// Locks our serializer's `<contactinfo>` divergence from git-repo. The
    /// bugurl below is git-repo's OWN default `BUG_URL`
    /// (third-party/git-repo-xml-parser/repo:129); git-repo SUPPRESSES
    /// `<contactinfo>` for exactly this value (`bugurl != Wrapper().BUG_URL`,
    /// manifest_xml.py:787), whereas we emit whenever `bugurl` is present (see the
    /// NOTE in `write_contactinfo`). Using the real default `BUG_URL` is what
    /// makes this a genuine guard: if `write_contactinfo` were changed to adopt
    /// git-repo's suppression, this test would fail. A stale or unrelated URL
    /// would leave both behaviors identical and pass vacuously.
    #[test]
    fn test_serialize_contactinfo_emitted_for_default_bug_url() {
        // git-repo's default BUG_URL, verbatim (repo:129) — the one value
        // git-repo drops <contactinfo> for, but which we keep.
        let xml = r#"<manifest>
            <remote name="origin" fetch="https://example.com/" />
            <default remote="origin" revision="main" />
            <contactinfo bugurl="https://issues.gerritcodereview.com/issues/new?component=1370071" />
        </manifest>"#;
        let out = crate::to_static(xml.as_bytes(), &mut no_includes, resolve_pinned).unwrap();
        assert!(
            out.contains(
                r#"<contactinfo bugurl="https://issues.gerritcodereview.com/issues/new?component=1370071""#
            ),
            "contactinfo must be emitted even for git-repo's default BUG_URL, which git-repo itself suppresses (manifest_xml.py:787): {out}"
        );
    }

    #[test]
    fn test_serialize_post_project_singletons_order() {
        // repo-hooks -> superproject -> contactinfo, all after the projects.
        let xml = r#"<manifest>
            <remote name="origin" fetch="https://example.com/" />
            <default remote="origin" revision="main" />
            <project name="hooks" />
            <repo-hooks in-project="hooks" enabled-list="pre-upload" />
            <superproject name="s" remote="origin" revision="main" />
            <contactinfo bugurl="https://bugs.example.com/" />
        </manifest>"#;
        let out = crate::to_static(xml.as_bytes(), &mut no_includes, resolve_pinned).unwrap();
        let project = out
            .find(r#"<project name="hooks""#)
            .expect("project present");
        let hooks = out.find("<repo-hooks").expect("repo-hooks present");
        let superp = out.find("<superproject").expect("superproject present");
        let contact = out.find("<contactinfo").expect("contactinfo present");
        assert!(
            project < hooks && hooks < superp && superp < contact,
            "singletons must follow projects in order repo-hooks -> superproject -> contactinfo: {out}"
        );
    }

    // --- G3.12: <include> flattening flows through to_static ---

    #[test]
    fn test_serialize_flattens_includes_end_to_end() {
        // Includes are flattened at parse; this proves the flattened projects
        // reach the serialized static.xml (name-sorted alongside the root ones).
        let included = br#"<manifest>
            <project name="aaa-included" />
        </manifest>"#;
        let root = br#"<manifest>
            <remote name="origin" fetch="https://example.com/" />
            <default remote="origin" revision="main" />
            <project name="zzz-root" />
            <include name="other.xml" />
        </manifest>"#;
        let mut open_fn = |name: &str| -> Result<Vec<u8>> {
            assert_eq!(name, "other.xml");
            Ok(included.to_vec())
        };
        let out = crate::to_static(root, &mut open_fn, resolve_pinned).unwrap();
        assert!(
            out.contains(r#"name="aaa-included""#),
            "an included project must appear in the serialized static.xml: {out}"
        );
        assert!(
            out.contains(r#"name="zzz-root""#),
            "the root project must appear in the serialized static.xml: {out}"
        );
        let included_pos = out.find(r#"name="aaa-included""#).unwrap();
        let root_pos = out.find(r#"name="zzz-root""#).unwrap();
        assert!(
            included_pos < root_pos,
            "flattened projects must be name-sorted (aaa-included before zzz-root): {out}"
        );
    }

    // --- G4.1: semantic conformance vs git-repo peg-pinning (test_set_revision_id) ---

    /// Semantic-conformance port of git-repo's peg-pinning `ToXml` behavior,
    /// modeled on `third-party/git-repo-xml-parser/tests/test_manifest_xml.py`
    /// `test_set_revision_id`: a branch-tracked project (revision inherited from
    /// `<default>`) pinned to a concrete sha emits that sha as `revision` and
    /// derives `upstream` from the original symbolic ref (revisionExpr).
    ///
    /// `to_static` models git-repo's peg path (`manifest_xml.py` `ToXml`, lines
    /// ~680-698) under git-repo's own default flags `peg_rev_upstream=True` and
    /// `peg_rev_dest_branch=True` (the `ToXml` signature, lines 573-574), so it
    /// derives BOTH `upstream` and `dest-branch` from the revisionExpr. git-repo's
    /// `test_set_revision_id` itself exercises the *non*-peg `ToXml()` +
    /// `SetRevisionId` path, so its expected XML omits `dest-branch`; the peg path
    /// this crate models includes it, matching the peg default flags (the same
    /// contract G3.7/G3.8 lock in). Conformance here is SEMANTIC — attribute set,
    /// values, and order plus an empty child set — and byte-exact equality is
    /// explicitly NOT required (no `<?xml>` prolog, no minidom blank-line text
    /// nodes, no alphabetized attributes).
    #[test]
    fn test_conformance_peg_pinned_branch_tracked_project() {
        // Mirrors test_set_revision_id: default remote + refs/heads/main and one
        // branch-tracked project with no explicit path/upstream/dest-branch.
        let xml = r#"<manifest>
            <remote name="default-remote" fetch="http://localhost" />
            <default remote="default-remote" revision="refs/heads/main" />
            <project name="test-name" />
        </manifest>"#;

        // resolve_fn stands in for git-repo's rev_parse: map the tracked branch to
        // a fixed 40-hex sha, asserting the project is genuinely branch-tracked so
        // peg-pinning actually fires.
        let sha = "1111111111111111111111111111111111111111";
        let resolve = |p: &types::Project| -> Result<String> {
            assert_eq!(
                p.revision, "refs/heads/main",
                "the project must be tracking the symbolic default revision"
            );
            Ok(sha.to_string())
        };
        let out = crate::to_static(xml.as_bytes(), &mut no_includes, resolve).unwrap();

        // Attribute SET + VALUES + ORDER: name, then the pinned revision, then the
        // derived upstream, then the derived dest-branch — contiguous, in git-repo
        // attribute order.
        assert!(
            out.contains(&format!(
                r#"<project name="test-name" revision="{sha}" upstream="refs/heads/main" dest-branch="refs/heads/main""#
            )),
            "peg-pinned project must emit name, pinned revision, and derived upstream/dest-branch in git-repo order: {out}"
        );

        let tag = project_tag(&out, "test-name");
        // The pinned revision is the resolved sha, never the symbolic ref.
        assert!(
            !tag.contains(r#"revision="refs/heads/main""#),
            "revision must be the resolved sha, not the symbolic ref: {out}"
        );
        // Attribute-set conformance: no path (relpath == name), no remote (default
        // remote), no groups (groupless).
        assert!(
            !tag.contains("path="),
            "no path attr when relpath == name: {out}"
        );
        assert!(
            !tag.contains("remote="),
            "no remote attr on the default remote: {out}"
        );
        assert!(
            !tag.contains("groups="),
            "a groupless project emits no groups: {out}"
        );
        // Child-set conformance: the element is self-closing (empty child set).
        assert!(
            tag.ends_with("/>"),
            "a childless peg-pinned project must be self-closing: {out}"
        );
    }

    // --- G4.2: add/remove reflected ---

    /// Collect the `name` attribute of every `<project>` element in `out` as a
    /// set, so a static.xml's project set can be compared order-independently.
    fn project_names(out: &str) -> std::collections::BTreeSet<String> {
        let needle = r#"<project name=""#;
        let mut names = std::collections::BTreeSet::new();
        let mut rest = out;
        while let Some(pos) = rest.find(needle) {
            let after = &rest[pos + needle.len()..];
            let end = after
                .find('"')
                .expect("a <project name=\"...\"> attribute must be closed");
            names.insert(after[..end].to_string());
            rest = &after[end..];
        }
        names
    }

    /// Two symbolic inputs differing by exactly one `<project>` each convert to a
    /// static.xml whose project set equals that input's own project set. This
    /// proves the conversion re-derives projects from `default.xml` every time
    /// (design §6 "add/remove inherent"), never from a prior static output — an
    /// added project appears and a removed one is absent purely as a function of
    /// the current input.
    #[test]
    fn test_conformance_add_remove_reflected() {
        let head = r#"
            <remote name="origin" fetch="https://example.com/" />
            <default remote="origin" revision="main" />"#;

        // WITH gamma: {alpha, beta, gamma}.
        let with_gamma = format!(
            r#"<manifest>{head}
                <project name="alpha" />
                <project name="beta" />
                <project name="gamma" />
            </manifest>"#
        );
        // WITHOUT gamma: identical except the single <project name="gamma"> line.
        let without_gamma = format!(
            r#"<manifest>{head}
                <project name="alpha" />
                <project name="beta" />
            </manifest>"#
        );

        let with_out =
            crate::to_static(with_gamma.as_bytes(), &mut no_includes, resolve_pinned).unwrap();
        let without_out =
            crate::to_static(without_gamma.as_bytes(), &mut no_includes, resolve_pinned).unwrap();

        let expect_with: std::collections::BTreeSet<String> = ["alpha", "beta", "gamma"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let expect_without: std::collections::BTreeSet<String> =
            ["alpha", "beta"].iter().map(|s| s.to_string()).collect();

        // Each static's project set equals its own input's set.
        assert_eq!(
            project_names(&with_out),
            expect_with,
            "the with-gamma static must contain exactly its three input projects: {with_out}"
        );
        assert_eq!(
            project_names(&without_out),
            expect_without,
            "the without-gamma static must contain exactly its two input projects: {without_out}"
        );

        // The single differing project is reflected: present in one, absent in the
        // other.
        assert!(
            project_names(&with_out).contains("gamma"),
            "the added project must appear in the static derived from its input: {with_out}"
        );
        assert!(
            !project_names(&without_out).contains("gamma"),
            "the removed project must be absent from the static derived from its input: {without_out}"
        );
    }

    // --- G4.3: fail-loud on non-standard content (at parse, via to_static) ---

    /// `to_static` parses in strict mode (`deny_unknown: true`), so an unknown
    /// *element* must fail the conversion loudly rather than be silently dropped.
    /// This exercises the fail-loud contract at the `to_static` boundary (G1.1
    /// covers the same rejection through `from_xml_with_options`).
    #[test]
    fn test_conformance_to_static_rejects_unknown_element() {
        let xml = r#"<manifest>
            <remote name="origin" fetch="https://example.com/" />
            <default remote="origin" revision="main" />
            <x-future />
            <project name="build" />
        </manifest>"#;
        let err = crate::to_static(xml.as_bytes(), &mut no_includes, resolve_pinned)
            .expect_err("to_static must reject an unknown element");
        let msg = err.to_string();
        assert!(
            msg.to_lowercase().contains("unknown"),
            "the error must flag the content as unknown: {msg}"
        );
    }

    /// `to_static` must also reject a non-standard *attribute* (G1.2's attr_check
    /// allowlist), and the error must name the offending attribute.
    #[test]
    fn test_conformance_to_static_rejects_unknown_attribute() {
        let xml = r#"<manifest>
            <remote name="origin" fetch="https://example.com/" />
            <default remote="origin" revision="main" />
            <project name="build" x-custom="v" />
        </manifest>"#;
        let err = crate::to_static(xml.as_bytes(), &mut no_includes, resolve_pinned)
            .expect_err("to_static must reject an unknown attribute");
        let msg = err.to_string();
        assert!(
            msg.to_lowercase().contains("unknown") && msg.contains("x-custom"),
            "the error must flag 'unknown' and name the offending attribute: {msg}"
        );
    }

    // --- G4.4: default upstream inheritance + groups default-exclusion (composed) ---

    /// One realistic manifest exercising BOTH edges together: a `<default
    /// upstream=...>` inherited onto EVERY project, and the resolver-injected
    /// "default" group excluded so a groupless project emits no `groups` while a
    /// user group survives. G3.6 (`test_serialize_project_emits_inherited_default_upstream`)
    /// and G3.9 (`test_serialize_project_groupless_omits_groups_attr` /
    /// `test_serialize_project_preserves_notdefault_group`) cover these in
    /// isolation on single-project manifests; this asserts they COMPOSE across a
    /// multi-project manifest in one `to_static` pass — the inherited upstream
    /// lands on every project while each project's groups are independently
    /// default-excluded.
    #[test]
    fn test_conformance_default_upstream_and_group_exclusion_composed() {
        let xml = r#"<manifest>
            <remote name="origin" fetch="https://example.com/" />
            <default remote="origin" revision="main" upstream="refs/heads/upstream" />
            <project name="grouped" groups="tools" />
            <project name="plain" />
        </manifest>"#;
        let out = crate::to_static(xml.as_bytes(), &mut no_includes, resolve_pinned).unwrap();

        // The default upstream is inherited onto EVERY project and wins over the
        // upstream that would otherwise be derived from the revisionExpr ("main").
        for name in ["grouped", "plain"] {
            let tag = project_tag(&out, name);
            assert!(
                tag.contains(r#"upstream="refs/heads/upstream""#),
                "project {name:?} must inherit the default upstream: {out}"
            );
            assert!(
                !tag.contains(r#"upstream="main""#),
                "the inherited default upstream must win over the derived revisionExpr for {name:?}: {out}"
            );
        }

        // groups default-exclusion, per project: the groupless project emits no
        // groups attribute (the injected "default" is excluded), while the user
        // group survives on the other project.
        assert!(
            !project_tag(&out, "plain").contains("groups="),
            "a groupless project must emit no groups attribute: {out}"
        );
        assert!(
            project_tag(&out, "grouped").contains(r#"groups="tools""#),
            "a user group must survive with the auto/default groups excluded: {out}"
        );
    }

    // --- G5.1: realistic end-to-end fixture (the capstone) ---

    /// End-to-end proof of the whole symbolic->static conversion in a single
    /// `to_static` pass. One realistic multi-project, multi-remote manifest
    /// exercises every stage together: an `<include>` flattened via `open_fn`, a
    /// `<manifest-server>`, name-sorted multi-remotes, a `<copyfile>` child, and
    /// the three project revision modes side by side: a hash-pinned project
    /// (40-hex revision passed through verbatim, `resolve_fn` never consulted), a
    /// branch-tracked project on the default remote (symbolic revision resolved
    /// to a sha, with `upstream`/`dest-branch` derived from the original symbolic
    /// ref), and a branch-tracked project on a NON-default remote carrying a
    /// per-project symbolic revision. It also proves the groups auto/"default"
    /// exclusion (a groupless project emits no `groups`, a user group survives)
    /// and that the included project is name-sorted INTO the global project order
    /// (not appended).
    ///
    /// Modeled on the `test_parse_real_diff_manifest` fixture shape (lib.rs) but
    /// driven all the way through serialization and asserted SEMANTICALLY —
    /// element/attribute set, values, order, and child order — per the crate's
    /// non-byte-exact conformance contract (no `<?xml>` prolog, self-closing form
    /// and whitespace elided). Expected static.xml:
    ///
    /// ```text
    /// <manifest>
    ///   <remote name="origin" fetch="ssh://git-ro.example.com/data/gitrepos" pushurl="ssh://git.example.com/data/gitrepos"/>
    ///   <remote name="upstream" fetch="ssh://upstream.example.com/data/gitrepos"/>
    ///   <default remote="origin" revision="main" sync-j="12"/>
    ///   <manifest-server url="http://smart-sync.example.com/"/>
    ///   <project name="platform/art" path="art" revision="a{40}" upstream="main" dest-branch="main" groups="tools"/>
    ///   <project name="platform/build" path="build" revision="b{40}" upstream="main" dest-branch="main">
    ///     <copyfile src="core/root.mk" dest="Makefile"/>
    ///   </project>
    ///   <project name="system/core" revision="c{40}" upstream="main" dest-branch="main"/>
    ///   <project name="tools/pinned" revision="e{40}"/>
    ///   <project name="vendor/soong" remote="upstream" revision="d{40}" upstream="stable" dest-branch="stable"/>
    /// </manifest>
    /// ```
    #[test]
    fn test_conformance_realistic_end_to_end_fixture() {
        // The included manifest contributes one branch-tracked project that must
        // be name-sorted into the MIDDLE of the root projects on output.
        let extra = br#"<?xml version="1.0" encoding="UTF-8"?>
<manifest>
  <project name="system/core" path="system/core" />
</manifest>"#;

        let pinned = "e".repeat(40);
        let root = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<manifest>
  <manifest-server url="http://smart-sync.example.com/" />
  <remote name="origin" fetch="ssh://git-ro.example.com/data/gitrepos"
    pushurl="ssh://git.example.com/data/gitrepos" />
  <remote name="upstream" fetch="ssh://upstream.example.com/data/gitrepos" />
  <default remote="origin" revision="main" sync-j="12" />
  <project name="platform/build" path="build">
    <copyfile src="core/root.mk" dest="Makefile" />
  </project>
  <project name="platform/art" path="art" groups="tools" />
  <project name="tools/pinned" path="tools/pinned" revision="{pinned}" />
  <project name="vendor/soong" path="vendor/soong" remote="upstream" revision="stable" />
  <include name="extra.xml" />
</manifest>"#
        );

        let sha_art = "a".repeat(40);
        let sha_build = "b".repeat(40);
        let sha_core = "c".repeat(40);
        let sha_soong = "d".repeat(40);

        // resolve_fn maps each branch-tracked project to a distinct sha. The
        // hash-pinned "tools/pinned" must bypass it entirely — the panic arm
        // proves the is_git_hash short-circuit never calls back for it.
        let resolve = |p: &types::Project| -> Result<String> {
            Ok(match p.name.as_str() {
                "platform/art" => "a".repeat(40),
                "platform/build" => "b".repeat(40),
                "system/core" => "c".repeat(40),
                "vendor/soong" => "d".repeat(40),
                other => panic!(
                    "resolve_fn must not be called for the hash-pinned or an unexpected project: {other}"
                ),
            })
        };

        let mut open_fn = |name: &str| -> Result<Vec<u8>> {
            assert_eq!(
                name, "extra.xml",
                "the manifest's only include is extra.xml"
            );
            Ok(extra.to_vec())
        };

        let out = crate::to_static(root.as_bytes(), &mut open_fn, resolve).unwrap();

        // Top-level element order: remotes -> default -> manifest-server -> projects.
        let remote_pos = out.find("<remote").expect("a remote is present");
        let default_pos = out.find("<default").expect("the default is present");
        let ms_pos = out
            .find("<manifest-server")
            .expect("the manifest-server is present");
        let first_project_pos = out.find("<project").expect("a project is present");
        assert!(
            remote_pos < default_pos && default_pos < ms_pos && ms_pos < first_project_pos,
            "top-level order must be remotes, default, manifest-server, projects: {out}"
        );

        // Remotes: name-sorted, with the full attribute set/order/values.
        assert!(
            out.contains(
                r#"<remote name="origin" fetch="ssh://git-ro.example.com/data/gitrepos" pushurl="ssh://git.example.com/data/gitrepos""#
            ),
            "origin remote must emit name, fetch, pushurl in order: {out}"
        );
        assert!(
            out.contains(
                r#"<remote name="upstream" fetch="ssh://upstream.example.com/data/gitrepos""#
            ),
            "upstream remote must emit name and fetch: {out}"
        );
        let origin_pos = out.find(r#"name="origin""#).expect("origin remote present");
        let upstream_pos = out
            .find(r#"name="upstream""#)
            .expect("upstream remote present");
        assert!(
            origin_pos < upstream_pos,
            "remotes must be name-sorted (origin before upstream): {out}"
        );

        // Default + manifest-server, full attribute strings.
        assert!(
            out.contains(r#"<default remote="origin" revision="main" sync-j="12""#),
            "default must emit remote, revision, sync-j in order: {out}"
        );
        assert!(
            out.contains(r#"<manifest-server url="http://smart-sync.example.com/""#),
            "manifest-server must emit its url: {out}"
        );

        // Each project's full attribute set/order/values.
        // platform/art: default remote (omitted), branch-tracked (derived
        // upstream/dest-branch from "main"), plus a surviving user group.
        assert!(
            out.contains(&format!(
                r#"<project name="platform/art" path="art" revision="{sha_art}" upstream="main" dest-branch="main" groups="tools""#
            )),
            "platform/art must emit path, resolved revision, derived upstream/dest-branch, and its user group: {out}"
        );
        // platform/build: branch-tracked, carries a copyfile child (open/close form).
        assert!(
            out.contains(&format!(
                r#"<project name="platform/build" path="build" revision="{sha_build}" upstream="main" dest-branch="main""#
            )),
            "platform/build must emit path, resolved revision, and derived upstream/dest-branch: {out}"
        );
        // system/core: from the include; name==path (no path attr), default remote,
        // branch-tracked. Its presence proves include-flattening reaches serialization.
        assert!(
            out.contains(&format!(
                r#"<project name="system/core" revision="{sha_core}" upstream="main" dest-branch="main""#
            )),
            "the included system/core must emit resolved revision + derived attrs, with no path (name==path): {out}"
        );
        // tools/pinned: hash-pinned -> revision verbatim, no derived attrs.
        assert!(
            out.contains(&format!(
                r#"<project name="tools/pinned" revision="{pinned}""#
            )),
            "the hash-pinned project must emit its 40-hex revision verbatim: {out}"
        );
        // vendor/soong: non-default remote + per-project symbolic revision "stable",
        // so upstream/dest-branch derive from "stable".
        assert!(
            out.contains(&format!(
                r#"<project name="vendor/soong" remote="upstream" revision="{sha_soong}" upstream="stable" dest-branch="stable""#
            )),
            "vendor/soong must emit its non-default remote, resolved revision, and upstream/dest-branch derived from its own symbolic revision: {out}"
        );

        // The copyfile is a child of platform/build (open/close, not self-closing).
        let build_open = out
            .find(r#"<project name="platform/build""#)
            .expect("build project present");
        let copyfile = out
            .find(r#"<copyfile src="core/root.mk" dest="Makefile""#)
            .expect("copyfile present");
        let build_close = out[build_open..]
            .find("</project>")
            .map(|i| build_open + i)
            .expect("build must have a close tag");
        assert!(
            build_open < copyfile && copyfile < build_close,
            "the copyfile must be a child inside the platform/build project element: {out}"
        );

        // Top-level projects name-sorted, with the included system/core interleaved
        // between platform/build and tools/pinned (proves the include is flattened
        // and sorted IN, not appended at the end).
        let p_art = out
            .find(r#"name="platform/art""#)
            .expect("platform/art present");
        let p_build = out
            .find(r#"name="platform/build""#)
            .expect("platform/build present");
        let p_core = out
            .find(r#"name="system/core""#)
            .expect("system/core present");
        let p_pinned = out
            .find(r#"name="tools/pinned""#)
            .expect("tools/pinned present");
        let p_soong = out
            .find(r#"name="vendor/soong""#)
            .expect("vendor/soong present");
        assert!(
            p_art < p_build && p_build < p_core && p_core < p_pinned && p_pinned < p_soong,
            "top-level projects must be name-sorted with the included system/core interleaved: {out}"
        );

        // Exactly the five expected projects, no more (no stray subprojects).
        let expected: std::collections::BTreeSet<String> = [
            "platform/art",
            "platform/build",
            "system/core",
            "tools/pinned",
            "vendor/soong",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(
            project_names(&out),
            expected,
            "the static must contain exactly the four symbolic projects plus the one included project: {out}"
        );

        // Hash-pinned project: scoped negatives (verbatim, no derived/inherited attrs).
        let pinned_tag = project_tag(&out, "tools/pinned");
        assert!(
            pinned_tag.contains(&format!(r#"revision="{pinned}""#)),
            "hash-pinned revision must be verbatim: {out}"
        );
        assert!(
            !pinned_tag.contains("upstream"),
            "a hash-pinned project must not derive an upstream: {out}"
        );
        assert!(
            !pinned_tag.contains("dest-branch"),
            "a hash-pinned project must not derive a dest-branch: {out}"
        );
        assert!(
            !pinned_tag.contains("path="),
            "tools/pinned has name==path, so no path attribute: {out}"
        );
        assert!(
            !pinned_tag.contains("remote="),
            "tools/pinned is on the default remote, so no remote attribute: {out}"
        );

        // Groups auto/"default" exclusion, per project: only platform/art's user
        // group survives; every other project is groupless.
        assert!(
            project_tag(&out, "platform/art").contains(r#"groups="tools""#),
            "platform/art's user group must survive the auto/default exclusion: {out}"
        );
        for name in [
            "platform/build",
            "system/core",
            "tools/pinned",
            "vendor/soong",
        ] {
            assert!(
                !project_tag(&out, name).contains("groups="),
                "the groupless project {name:?} must emit no groups attribute: {out}"
            );
        }
    }
}
