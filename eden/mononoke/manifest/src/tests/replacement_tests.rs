/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use pretty_assertions::assert_eq;

use super::*;

#[mononoke::fbinit_test]
async fn descendant_replacements_are_compared_under_identical_roots(
    fb: FacebookInit,
) -> Result<()> {
    let store: Arc<dyn KeyedBlobstore> = Arc::new(KeyedMemblob::default());
    let ctx = CoreContext::test_mock(fb);
    let root = derive_test_manifest(
        &ctx,
        &store,
        vec![],
        btreemap! {
            "copy/dir/file" => Some("old"), "unrelated" => Some("same"),
        },
    )
    .await?
    .expect("nonempty manifest");
    let source = derive_test_manifest(
        &ctx,
        &store,
        vec![],
        btreemap! { "dir/file" => Some("new") },
    )
    .await?
    .expect("nonempty manifest");
    let entry = source
        .find_entry(ctx.clone(), store.clone(), MPath::new("dir")?)
        .await?
        .expect("source dir");
    let plain = HashMap::from([(MPath::new("copy/dir")?, entry.clone())]);
    let tree = entry.into_tree().expect("source is a directory");
    let weighted = HashMap::from([(MPath::new("copy/dir")?, Entry::Tree((1, tree)))]);
    let leaves = |diff| match diff {
        Diff::Added(path, Entry::Leaf(_))
        | Diff::Removed(path, Entry::Leaf(_))
        | Diff::Changed(path, Entry::Leaf(_), Entry::Leaf(_)) => Some(path.to_string()),
        _ => None,
    };
    let unordered = root
        .filtered_diff(
            ctx.clone(),
            store.clone(),
            root,
            store.clone(),
            leaves,
            |_| true,
            plain,
        )
        .try_collect::<Vec<_>>()
        .await?;
    let ordered = root
        .filtered_diff_ordered(
            ctx.clone(),
            store.clone(),
            root,
            store.clone(),
            None,
            leaves,
            |_| true,
            weighted.clone(),
        )
        .try_collect::<Vec<_>>()
        .await?;
    assert_eq!(unordered, vec!["copy/dir/file"]);
    assert_eq!(ordered, vec!["copy/dir/file"]);
    let after = root
        .filtered_diff_ordered(
            ctx.clone(),
            store.clone(),
            root,
            store.clone(),
            Some(MPath::new("copy/dir/file")?),
            leaves,
            |_| true,
            weighted,
        )
        .try_collect::<Vec<_>>()
        .await?;
    assert!(after.is_empty());
    let unchanged = root
        .diff_ordered(ctx.clone(), store.clone(), root, None)
        .try_collect::<Vec<_>>()
        .await?;
    assert!(unchanged.is_empty());
    Ok(())
}
