/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use mononoke_macros::mononoke;

use super::*;

#[mononoke::test]
async fn complete_empty_page_renders_original_aggregate_text() -> Result<()> {
    let mut pages = [(None, Some("a")), (Some("a"), None)].into_iter();
    let output = complete(1, move |after| {
        let (expected, continuation) = pages.next().expect("no request after completion");
        assert_eq!(after.as_deref(), expected);
        future::ok(thrift::CommitRestrictedPathsChangesResponse {
            are_restricted: thrift::PathCoverage::NONE,
            has_access: thrift::PathCoverage::ALL,
            checked_both_sides: Some(true),
            compact: Some(thrift::CommitRestrictedPathsChangesCompact::default()),
            last_path: continuation.map(str::to_owned),
            ..Default::default()
        })
    })
    .await?;
    let mut text = Vec::new();
    output.render(&(), &mut text)?;
    assert_eq!(
        String::from_utf8(text)?,
        "Restricted: none\nHas access: all\n"
    );
    Ok(())
}
