/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use anyhow::Context;
use anyhow::Result;
use mononoke_macros::mononoke;

use super::*;

type Response = thrift::CommitRestrictedPathsChangesResponse;

/// Serves `pages` in order, requiring each request to continue from the
/// previous page and failing any request after the last page.
fn serial(pages: Vec<Response>) -> impl FnMut(Option<String>) -> future::Ready<Result<Response>> {
    let mut pages = pages.into_iter();
    let mut expected = None;
    move |after| {
        assert_eq!(after, expected, "requests continue from the previous page");
        let page = pages.next().context("requested a page after completion");
        expected = page.as_ref().ok().and_then(|page| page.last_path.clone());
        future::ready(page)
    }
}

async fn complete_pages(pages: Vec<Response>, limit: i64) -> Result<ChangesOutput> {
    complete(limit, serial(pages)).await
}

fn checked(root: &str, allowed: bool) -> thrift::CheckedPathRestrictionPolicy {
    thrift::CheckedPathRestrictionPolicy {
        path: root.into(),
        acls: vec![format!("REPO_REGION:{root}")],
        permission_request_group_identity: format!("GROUP:{root}"),
        has_access: allowed,
        ..Default::default()
    }
}

fn info(indices: &[i32]) -> thrift::PathPolicyInfoCompact {
    thrift::PathPolicyInfoCompact {
        restrictions: indices.to_vec(),
        ..Default::default()
    }
}

fn page(
    paths: &[&str],
    policies: Vec<thrift::CheckedPathRestrictionPolicy>,
    roots: &[(&str, &[i32])],
    last: Option<&str>,
) -> Response {
    let roots: BTreeMap<String, Vec<i32>> = roots
        .iter()
        .map(|(path, indices)| (path.to_string(), indices.to_vec()))
        .collect();
    let authorized: Vec<_> = paths
        .iter()
        .filter(|path| {
            roots.get(**path).is_none_or(|indices| {
                indices
                    .iter()
                    .all(|index| policies[*index as usize].has_access)
            })
        })
        .map(|path| path.to_string())
        .collect();
    thrift::CommitRestrictedPathsChangesResponse {
        are_restricted: coverage(roots.len(), paths.len(), thrift::PathCoverage::NONE),
        has_access: coverage(authorized.len(), paths.len(), thrift::PathCoverage::ALL),
        changed_paths: paths.iter().map(|path| path.to_string()).collect(),
        authorized_paths: authorized,
        checked_both_sides: Some(true),
        last_path: last.map(str::to_owned),
        compact: Some(thrift::CommitRestrictedPathsChangesCompact {
            policies,
            restriction_roots: roots,
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn mapping(destination: &str, byte: u8) -> thrift::SubtreeSource {
    thrift::SubtreeSource {
        commit_id: thrift::CommitId::bonsai(vec![byte; 32]),
        source_prefix: "secret".into(),
        destination_prefix: destination.into(),
        ..Default::default()
    }
}

fn change(source_index: i32, policy_index: i32) -> thrift::PathPermissionChangeInfoCompact {
    thrift::PathPermissionChangeInfoCompact {
        base: info(&[]),
        target: info(&[]),
        subtree_source: Some(thrift::SubtreeSourcePolicyInfoCompact {
            subtree_source_index: source_index,
            info: info(&[policy_index]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Two pages whose policy and source tables each start at index 0.
fn two_source_pages() -> Vec<Response> {
    let mut first = page(
        &["d1/file"],
        vec![checked("a", false)],
        &[("d1/file", &[0])],
        Some("d1/file"),
    );
    let c = first.compact.as_mut().expect("compact fixture");
    c.subtree_sources.push(mapping("d1", 1));
    c.permission_changes.insert("d1/file".into(), change(0, 0));
    let mut second = page(
        &["d2/file"],
        vec![checked("b", true)],
        &[("d2/file", &[0])],
        None,
    );
    let c = second.compact.as_mut().expect("compact fixture");
    c.subtree_sources.push(mapping("d2", 2));
    c.permission_changes.insert("d2/file".into(), change(0, 0));
    vec![first, second]
}

#[mononoke::test]
async fn complete_empty_page_renders_original_aggregate_text() -> Result<()> {
    let output = complete_pages(
        vec![
            page(&[], vec![], &[], Some("a")),
            page(&[], vec![], &[], None),
        ],
        1,
    )
    .await?;
    let mut text = Vec::new();
    output.render(&(), &mut text)?;
    assert_eq!(
        String::from_utf8(text)?,
        "Restricted: none\nHas access: all\n"
    );
    Ok(())
}

#[mononoke::test]
async fn page_local_policy_and_source_indices_are_remapped() -> Result<()> {
    let output = complete_pages(two_source_pages(), 1).await?;
    let mut whole = page(
        &["d1/file", "d2/file"],
        vec![checked("b", true), checked("a", false)],
        &[("d1/file", &[1]), ("d2/file", &[0])],
        None,
    );
    let c = whole.compact.as_mut().expect("compact fixture");
    c.subtree_sources = vec![mapping("d2", 2), mapping("d1", 1)];
    c.permission_changes.insert("d1/file".into(), change(1, 1));
    c.permission_changes.insert("d2/file".into(), change(0, 0));
    assert_eq!(
        serde_json::to_string(&output)?,
        serde_json::to_string(&complete_pages(vec![whole], 2).await?)?,
        "paging must not change the complete output"
    );
    assert_eq!(output.paths.compact.subtree_sources.len(), 2);
    assert_eq!(
        output.paths.compact.permission_changes["d2/file"]
            .subtree_source
            .as_ref()
            .expect("source")
            .subtree_source_index,
        SourceIndex(1),
        "the d2 mapping sorts after the d1 mapping"
    );
    Ok(())
}

#[mononoke::test]
async fn access_is_interned_without_merging_distinct_checked_results() -> Result<()> {
    let output = complete_pages(
        vec![page(
            &["a", "b"],
            vec![checked("tent", false), checked("tent", true)],
            &[("a", &[0]), ("b", &[1])],
            None,
        )],
        2,
    )
    .await?;
    assert_eq!(output.paths.compact.policies.len(), 2);
    assert!(!output.paths.compact.policies[0].has_access);
    assert!(output.paths.compact.policies[1].has_access);
    assert_eq!(output.paths.authorized_paths, vec!["b"]);
    Ok(())
}

#[mononoke::test]
async fn sparse_only_policy_does_not_deny_actual_content() -> Result<()> {
    let mut p = page(&["public/file"], vec![checked("secret", false)], &[], None);
    p.compact
        .as_mut()
        .expect("compact fixture")
        .permission_changes
        .insert(
            "public/file".into(),
            thrift::PathPermissionChangeInfoCompact {
                base: info(&[0]),
                target: info(&[]),
                ..Default::default()
            },
        );
    let output = complete_pages(vec![p], 1).await?;
    assert!(output.paths.compact.restriction_roots.is_empty());
    assert_eq!(output.has_access, "all");
    assert_eq!(output.paths.authorized_paths, vec!["public/file"]);
    let json = serde_json::to_value(&output)?;
    assert!(
        json["compact"]["permission_changes"]["public/file"]["base"]
            .get("has_access")
            .is_none()
    );
    Ok(())
}

#[mononoke::test]
fn malformed_pages_are_rejected() {
    let valid = page(&["a"], vec![checked("tent", false)], &[("a", &[0])], None);
    let mut cases = vec![];
    let mut p = valid.clone();
    p.checked_both_sides = None;
    cases.push(p);
    let mut p = valid.clone();
    p.checked_both_sides = Some(false);
    cases.push(p);
    let mut p = valid.clone();
    p.compact = None;
    cases.push(p);
    let mut p = valid.clone();
    p.last_path = Some("b".into());
    cases.push(p);
    let mut p = valid.clone();
    p.authorized_paths = vec!["a".into()];
    p.has_access = thrift::PathCoverage::ALL;
    cases.push(p);
    let mut p = valid.clone();
    p.changed_paths = vec!["a".into(), "a".into()];
    cases.push(p);
    for index in [-1, 1] {
        let mut p = valid.clone();
        p.compact
            .as_mut()
            .expect("fixture")
            .restriction_roots
            .insert("a".into(), vec![index]);
        cases.push(p);
        let mut p = valid.clone();
        p.compact
            .as_mut()
            .expect("fixture")
            .permission_changes
            .insert(
                "a".into(),
                thrift::PathPermissionChangeInfoCompact {
                    base: info(&[index]),
                    ..Default::default()
                },
            );
        cases.push(p);
    }
    let mut p = valid.clone();
    p.compact
        .as_mut()
        .expect("fixture")
        .policies
        .push(checked("unused", true));
    cases.push(p);
    let mut p = valid.clone();
    p.compact
        .as_mut()
        .expect("fixture")
        .subtree_sources
        .push(mapping("a", 1));
    cases.push(p);
    let mut p = valid.clone();
    p.compact
        .as_mut()
        .expect("fixture")
        .permission_changes
        .insert(
            "outside".into(),
            thrift::PathPermissionChangeInfoCompact::default(),
        );
    cases.push(p);
    for p in cases {
        assert!(ValidatedPage::try_new(p, None, 2).is_err());
    }
    assert!(
        ValidatedPage::try_new(valid.clone(), None, 0).is_err(),
        "the limit is at least one path"
    );
    assert!(
        ValidatedPage::try_new(valid, Some("a"), 2).is_err(),
        "a page must start after its cursor"
    );
}

#[mononoke::test]
async fn late_malformed_page_fails_the_complete_comparison() {
    let mut pages = two_source_pages();
    pages[1].checked_both_sides = None;
    assert!(complete_pages(pages, 1).await.is_err());
}

#[mononoke::test]
fn source_indices_and_prefixes_are_checked() {
    let mut valid = page(
        &["ab/file"],
        vec![checked("tent", true)],
        &[("ab/file", &[0])],
        None,
    );
    let c = valid.compact.as_mut().expect("fixture");
    c.subtree_sources.push(mapping("ab", 1));
    c.permission_changes.insert("ab/file".into(), change(0, 0));
    assert!(ValidatedPage::try_new(valid.clone(), None, 1).is_ok());
    for index in [-1, 1] {
        let mut p = valid.clone();
        p.compact
            .as_mut()
            .expect("fixture")
            .permission_changes
            .get_mut("ab/file")
            .expect("fixture")
            .subtree_source
            .as_mut()
            .expect("fixture")
            .subtree_source_index = index;
        assert!(ValidatedPage::try_new(p, None, 1).is_err());
    }
    let mut p = valid.clone();
    p.compact.as_mut().expect("fixture").subtree_sources[0].destination_prefix = "a".into();
    assert!(
        ValidatedPage::try_new(p, None, 1).is_err(),
        "prefix a does not cover ab/file"
    );
    let mut p = valid;
    p.compact.as_mut().expect("fixture").subtree_sources[0].destination_prefix = "".into();
    assert!(ValidatedPage::try_new(p, None, 1).is_ok());
}

#[mononoke::test]
async fn short_and_empty_advancing_pages_are_not_completion() -> Result<()> {
    let first = page(&[], vec![], &[], Some("a"));
    let second = page(&["b"], vec![], &[], Some("b"));
    let last = page(&[], vec![], &[], None);
    let output = complete_pages(vec![first, second, last], 3).await?;
    assert_eq!(output.paths.changed_paths, vec!["b"]);
    Ok(())
}

#[mononoke::test]
fn validated_page_has_canonical_page_local_tables() -> Result<()> {
    let mut p = checked("tent", true);
    p.acls = vec!["GROUP:b".into(), "GROUP:a".into(), "GROUP:a".into()];
    let mut q = p.clone();
    q.acls = vec!["GROUP:a".into(), "GROUP:b".into()];
    let paths = ValidatedPage::try_new(
        page(
            &["a", "b"],
            vec![checked("z", false), p, q],
            &[("a", &[0, 1]), ("b", &[2])],
            None,
        ),
        None,
        2,
    )?
    .into_paths();
    let tables = &paths.compact;
    assert_eq!(
        tables.policies.len(),
        2,
        "ACL sets that differ only in order and repetition are one policy"
    );
    assert_eq!(tables.policies[0].path, "tent", "policies are sorted");
    assert_eq!(tables.policies[0].acls, vec!["GROUP:a", "GROUP:b"]);
    assert_eq!(
        tables.restriction_roots["a"],
        vec![PolicyIndex(0), PolicyIndex(1)]
    );
    assert_eq!(tables.restriction_roots["b"], vec![PolicyIndex(0)]);
    Ok(())
}
