/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use super::*;

#[mononoke::fbinit_test]
async fn test_restricted_paths_access_root_is_unrestricted(fb: FacebookInit) -> Result<()> {
    let cs_ctx = create_test_changeset(fb, vec![("restricted", "TIER:my-acl")]).await?;

    let response = restricted_paths_access_impl(&cs_ctx, BTreeSet::from([String::new()]), true)
        .await
        .map_err(|err| anyhow::anyhow!("{err:?}"))?;

    assert_eq!(response.are_restricted, thrift::PathCoverage::NONE);
    assert_eq!(response.has_access, thrift::PathCoverage::ALL);
    assert!(response.restriction_roots.is_empty());
    assert_eq!(response.authorized_paths, vec![String::new()]);

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_restricted_paths_access_root_mixed_with_other_paths(fb: FacebookInit) -> Result<()> {
    let cs_ctx = create_test_changeset_with_acl_provider(
        fb,
        vec![("restricted", "TIER:my-acl")],
        TestAclProvider::new(false),
    )
    .await?;

    let paths = BTreeSet::from([
        String::new(),
        "restricted/file.txt".to_string(),
        "open/file.txt".to_string(),
    ]);
    let response = restricted_paths_access_impl(&cs_ctx, paths, true)
        .await
        .map_err(|err| anyhow::anyhow!("{err:?}"))?;

    assert_eq!(response.are_restricted, thrift::PathCoverage::SOME);
    assert_eq!(response.has_access, thrift::PathCoverage::SOME);
    assert_eq!(
        response.restriction_roots.keys().collect::<Vec<_>>(),
        vec!["restricted/file.txt"]
    );
    assert_eq!(
        response.restriction_roots["restricted/file.txt"][0].path,
        "restricted"
    );
    assert_eq!(
        response.authorized_paths,
        vec![String::new(), "open/file.txt".to_string()]
    );

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_restricted_paths_access_root_without_restrictions_configured(
    fb: FacebookInit,
) -> Result<()> {
    let cs_ctx = create_test_changeset(fb, vec![]).await?;

    let response = restricted_paths_access_impl(&cs_ctx, BTreeSet::from([String::new()]), true)
        .await
        .map_err(|err| anyhow::anyhow!("{err:?}"))?;

    assert_eq!(response.are_restricted, thrift::PathCoverage::NONE);
    assert_eq!(response.has_access, thrift::PathCoverage::ALL);
    assert!(response.restriction_roots.is_empty());
    assert_eq!(response.authorized_paths, vec![String::new()]);

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_restricted_paths_access_invalid_path_is_request_error(
    fb: FacebookInit,
) -> Result<()> {
    let cs_ctx = create_test_changeset(fb, vec![("restricted", "TIER:my-acl")]).await?;

    let result =
        restricted_paths_access_impl(&cs_ctx, BTreeSet::from(["bad\0path".to_string()]), false)
            .await;

    assert!(matches!(result, Err(scs_errors::ServiceError::Request(_))));

    Ok(())
}
