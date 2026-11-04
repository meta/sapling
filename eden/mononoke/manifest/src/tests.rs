/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;

use anyhow::Result;
use async_trait::async_trait;
use blobstore::BlobstoreBytes;
use blobstore::BlobstoreGetData;
use blobstore::BlobstoreIsPresent;
use blobstore::KeyedBlobstore;
use blobstore::Loadable;
use blobstore::Storable;
use borrowed::borrowed;
use context::CoreContext;
use fbinit::FacebookInit;
use futures::stream::StreamExt;
use futures::stream::TryStreamExt;
use maplit::btreemap;
use maplit::hashset;
use memblob::KeyedMemblob;
use mononoke_macros::mononoke;
use mononoke_types::BlobstoreValue;
use mononoke_types::ContentManifestId;
use mononoke_types::FileType;
use mononoke_types::MPathElement;
use mononoke_types::NonRootMPath;
use mononoke_types::content_manifest::ContentManifest;
use mononoke_types::content_manifest::ContentManifestEntry;
use mononoke_types::content_manifest::ContentManifestFile;
use mononoke_types::path::MPath;
use mononoke_types::sharded_map_v2::ShardedMapV2Node;
use mononoke_types_mocks::changesetid::ONES_CSID;
use mononoke_types_mocks::changesetid::THREES_CSID;
use mononoke_types_mocks::changesetid::TWOS_CSID;
use mononoke_types_mocks::contentid::ONES_CTID;
use mononoke_types_mocks::contentid::TWOS_CTID;
use pretty_assertions::assert_eq;

pub(crate) use crate::Diff;
pub(crate) use crate::Entry;
pub(crate) use crate::Manifest;
pub(crate) use crate::ManifestOps;
pub(crate) use crate::ManifestOrderedOps;
pub(crate) use crate::PathOrPrefix;
pub(crate) use crate::find_intersection_of_diffs;
use crate::find_intersection_of_diffs_and_parents_pruned;

mod cancellation_tests;
mod replacement_tests;
pub mod test_manifest;

use self::test_manifest::TestLeaf;
use self::test_manifest::TestLeafId;
use self::test_manifest::TestManifestId;
use self::test_manifest::derive_stack_of_test_manifests;
use self::test_manifest::derive_test_manifest;
use self::test_manifest::derive_test_manifest_with_known_entries;
use self::test_manifest::list_test_manifest;

fn files_reference(files: BTreeMap<&str, &str>) -> Result<BTreeMap<NonRootMPath, String>> {
    files
        .into_iter()
        .map(|(path, content)| Ok((NonRootMPath::new(path)?, content.to_string())))
        .collect()
}

#[mononoke::fbinit_test]
async fn test_derive_manifest(fb: FacebookInit) -> Result<()> {
    let blobstore: Arc<dyn KeyedBlobstore> = Arc::new(KeyedMemblob::default());
    let ctx = CoreContext::test_mock(fb);
    borrowed!(ctx, blobstore);

    // derive manifest
    let derive = {
        move |parents, changes| async move {
            derive_test_manifest(ctx, blobstore, parents, changes)
                .await
                .map(|mf| mf.expect("expect non empty manifest"))
        }
    };

    // load all files for specified manifest
    let files = move |manifest_id| list_test_manifest(ctx, blobstore, manifest_id);

    // clean merge of two directories
    {
        let mf0 = derive(
            vec![],
            btreemap! {
                "/one/two" => Some("two"),
                "/one/three" => Some("three"),
                "/five/six" => Some("six"),
            },
        )
        .await?;
        let mf1 = derive(
            vec![],
            btreemap! {
                "/one/four" => Some("four"),
                "/one/three" => Some("three"),
                "/five/six" => Some("six"),
            },
        )
        .await?;
        let mf2 = derive(vec![mf0, mf1], btreemap! {}).await?;
        assert_eq!(
            files_reference(btreemap! {
                "/one/two" => "two",     // from mf0
                "/one/three" => "three", // from mf1
                "/one/four" => "four",   // shared leaf mf{0|1}
                "/five/six" => "six",    // shared tree mf{0|1}
            })?,
            files(mf2).await?,
        );
    }

    // remove all files
    {
        let mf0 = derive(
            vec![],
            btreemap! {
                "/one/two" => Some("two"),
                "/one/three" => Some("three"),
                "/five/six" => Some("six"),
            },
        )
        .await?;
        let mf1 = derive_test_manifest(
            ctx,
            blobstore,
            vec![mf0],
            btreemap! {
                "/one/two" => None,
                "/one/three" => None,
                "five/six" => None,
            },
        )
        .await?;
        assert!(mf1.is_none(), "empty manifest expected");
    }

    // file-file conflict
    {
        let mf0 = derive(
            vec![],
            btreemap! {
                "/one/two" => Some("conflict"),
                "/three" => Some("not a conflict")
            },
        )
        .await?;
        let mf1 = derive(
            vec![],
            btreemap! {
                "/one/two" => Some("CONFLICT"),
                "/three" => Some("not a conflict"),
            },
        )
        .await?;

        // make sure conflict is detected
        assert!(
            derive(vec![mf0, mf1], btreemap! {},).await.is_err(),
            "should contain file-file conflict"
        );

        // resolve by overriding file content
        let mf2 = derive(
            vec![mf0, mf1],
            btreemap! {
                "/one/two" => Some("resolved"),
            },
        )
        .await?;
        assert_eq!(
            files_reference(btreemap! {
                "/one/two" => "resolved", // new file replaces conflict
                "/three" => "not a conflict",
            })?,
            files(mf2).await?,
        );

        // resolve by deletion
        let mf3 = derive(
            vec![mf0, mf1],
            btreemap! {
                "/one/two" => None,
            },
        )
        .await?;
        assert_eq!(
            files_reference(btreemap! {
                // file is deleted
                "/three" => "not a conflict",
            })?,
            files(mf3).await?,
        );

        // resolve by overriding directory
        let mf4 = derive(
            vec![mf0, mf1],
            btreemap! {
                "/one" => Some("resolved"),
            },
        )
        .await?;
        assert_eq!(
            files_reference(btreemap! {
                "/one" => "resolved", // whole subdirectory replace by file
                "/three" => "not a conflict",
            })?,
            files(mf4).await?,
        );
    }

    // file-directory conflict
    {
        let mf0 = derive(
            vec![],
            btreemap! {
                "/one" => Some("file"),
                "/three" => Some("not a conflict")
            },
        )
        .await?;
        let mf1 = derive(
            vec![],
            btreemap! {
                "/one/two" => Some("directory"),
                "/three" => Some("not a conflict"),
            },
        )
        .await?;

        // make sure conflict is detected
        assert!(
            derive(vec![mf0, mf1], btreemap! {}).await.is_err(),
            "should contain file-file conflict"
        );

        // resolve by deleting file
        let mf2 = derive(
            vec![mf0, mf1],
            btreemap! {
                "/one" => None,
            },
        )
        .await?;
        assert_eq!(
            files_reference(btreemap! {
                "/one/two" => "directory",
                "/three" => "not a conflict",
            })?,
            files(mf2).await?,
        );

        // resolve by deleting of directory
        // NOTE: if you want to resolve file-directory conflict in favour of a file
        //       the only way to do this, is place file with same content. Deletion
        //       of all files in directory is not a valid resolution of a conflict.
        let mf3 = derive(
            vec![mf0, mf1],
            btreemap! {
                "/one" => Some("file"),
            },
        )
        .await?;
        assert_eq!(
            files_reference(btreemap! {
                "/one" => "file",
                "/three" => "not a conflict",
            })?,
            files(mf3).await?,
        );
    }

    // Weird case - if a directory is deleted as a file, then
    // it should just be ignored
    {
        let mf0 = derive(
            vec![],
            btreemap! {
                "/one/one" => Some("one"),
            },
        )
        .await?;
        let mf1 = derive(
            vec![mf0],
            btreemap! {
                "/one" => None,
            },
        )
        .await?;
        assert_eq!(
            files_reference(btreemap! {
                "/one/one" => "one",
            })?,
            files(mf0).await?,
        );
        assert_eq!(
            files_reference(btreemap! {
                "/one/one" => "one",
            })?,
            files(mf1).await?,
        );
    }

    // Weird case - deleting non-existing file is fine
    {
        let mf0 = derive(
            vec![],
            btreemap! {
                "/file" => Some("content"),
                "/one/one" => None,
            },
        )
        .await?;
        assert_eq!(
            files_reference(btreemap! {
                "/file" => "content",
            })?,
            files(mf0).await?,
        );
    }

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_derive_stack_of_manifests(fb: FacebookInit) -> Result<()> {
    let blobstore: Arc<dyn KeyedBlobstore> = Arc::new(KeyedMemblob::default());
    let ctx = CoreContext::test_mock(fb);
    borrowed!(ctx, blobstore);

    // derive manifest
    let derive = {
        move |parents, changes| async move {
            derive_stack_of_test_manifests(ctx, blobstore, parents, changes).await
        }
    };

    // load all files for specified manifest
    let files = move |manifest_id| list_test_manifest(ctx, blobstore, manifest_id);

    // Simple case - add files one after another
    {
        let mfs = derive(
            None,
            btreemap! {
                ONES_CSID => btreemap! {
                    "/one/one" => Some("one"),
                },
                TWOS_CSID => btreemap! {
                    "/two/two" => Some("two"),
                },
                THREES_CSID => btreemap! {
                    "/three/three" => Some("three"),
                },
            },
        )
        .await?;

        let mfid1 = *mfs.get(&ONES_CSID).unwrap();
        assert_eq!(
            files_reference(btreemap! {
                "/one/one" => "one",
            })?,
            files(mfid1).await?,
        );
        let mfid2 = *mfs.get(&TWOS_CSID).unwrap();
        assert_eq!(
            files_reference(btreemap! {
                "/one/one" => "one",
                "/two/two" => "two",
            })?,
            files(mfid2).await?,
        );
        let mfid3 = *mfs.get(&THREES_CSID).unwrap();
        assert_eq!(
            files_reference(btreemap! {
                "/one/one" => "one",
                "/two/two" => "two",
                "/three/three" => "three",
            })?,
            files(mfid3).await?,
        );
    }

    // Stack which creates/deleted and recreates a few files
    {
        let mfs = derive(
            None,
            btreemap! {
                ONES_CSID => btreemap! {
                    "/one/two" => Some("two"),
                    "/one/three" => Some("three"),
                    "/five/six" => Some("six"),
                },
                TWOS_CSID => btreemap! {
                    "/one/two" => None,
                    "/one/three" => None,
                },
                THREES_CSID => btreemap! {
                    "/one/two" => Some("two_recreated"),
                    "/one/three" => Some("three_recreated"),
                    "/one/four" => Some("four"),
                },
            },
        )
        .await?;

        let mfid1 = *mfs.get(&ONES_CSID).unwrap();
        assert_eq!(
            files_reference(btreemap! {
                "/one/two" => "two",
                "/one/three" => "three",
                "/five/six" => "six",
            })?,
            files(mfid1).await?,
        );
        let mfid2 = *mfs.get(&TWOS_CSID).unwrap();
        assert_eq!(
            files_reference(btreemap! {
                "/five/six" => "six",
            })?,
            files(mfid2).await?,
        );
        let mfid3 = *mfs.get(&THREES_CSID).unwrap();
        assert_eq!(
            files_reference(btreemap! {
                "/one/two" => "two_recreated",
                "/one/three" => "three_recreated",
                "/one/four" => "four",
                "/five/six" => "six",
            })?,
            files(mfid3).await?,
        );
    }

    // Check that parents are processed correctly
    {
        let mfs = derive(
            None,
            btreemap! {
                ONES_CSID => btreemap! {
                    "/one/two" => Some("two"),
                    "/one/three" => Some("three"),
                    "/five/six" => Some("six"),
                },
            },
        )
        .await?;
        let mfid1 = *mfs.get(&ONES_CSID).unwrap();

        let mfs = derive(
            Some(mfid1),
            btreemap! {
                TWOS_CSID => btreemap! {
                    "/one/two" => None,
                    "/one/three" => None,
                },
                THREES_CSID => btreemap! {
                    "/one/two" => Some("two_recreated"),
                    "/one/three" => Some("three_recreated"),
                    "/one/four" => Some("four"),
                },
            },
        )
        .await?;
        let mfid2 = *mfs.get(&TWOS_CSID).unwrap();
        assert_eq!(
            files_reference(btreemap! {
                "/five/six" => "six",
            })?,
            files(mfid2).await?,
        );
        let mfid3 = *mfs.get(&THREES_CSID).unwrap();
        assert_eq!(
            files_reference(btreemap! {
                "/one/two" => "two_recreated",
                "/one/three" => "three_recreated",
                "/one/four" => "four",
                "/five/six" => "six",
            })?,
            files(mfid3).await?,
        );
    }

    // Simple file/dir conflict is resolved
    {
        let mfs = derive(
            None,
            btreemap! {
                ONES_CSID => btreemap! {
                    "/one/two" => Some("two"),
                    "/one/three" => Some("three"),
                },
            },
        )
        .await?;

        let mfid1 = *mfs.get(&ONES_CSID).unwrap();

        let mfs = derive(
            Some(mfid1),
            btreemap! {
                TWOS_CSID => btreemap! {
                    "/one" => Some("resolved_file_dir"),
                },
            },
        )
        .await?;
        let mfid2 = *mfs.get(&TWOS_CSID).unwrap();
        assert_eq!(
            files_reference(btreemap! {
                "/one" => "resolved_file_dir",
            })?,
            files(mfid2).await?,
        );
    }

    // We don't allow paths that are prefixes of each other - raise an error
    {
        let res = derive(
            None,
            btreemap! {
                ONES_CSID => btreemap! {
                    "/one/two" => Some("two"),
                },
                TWOS_CSID => btreemap! {
                    "/one" => Some("overridden"),
                },
            },
        )
        .await;

        assert!(res.is_err());
    }

    // Now let's do an empty commit
    {
        let mfs = derive(
            None,
            btreemap! {
                ONES_CSID => btreemap! {
                    "/one/two" => Some("two"),
                },
                TWOS_CSID => btreemap!{}
            },
        )
        .await?;
        let mfid1 = *mfs.get(&ONES_CSID).unwrap();
        let mfid2 = *mfs.get(&TWOS_CSID).unwrap();
        assert_eq!(mfid1, mfid2);
    }

    // Weird case - if a directory is deleted as a file, then
    // it should just be ignored
    {
        let mfs = derive(
            None,
            btreemap! {
                ONES_CSID => btreemap! {
                    "/one/one" => Some("one"),
                },
            },
        )
        .await?;

        let mfid1 = *mfs.get(&ONES_CSID).unwrap();
        let mfs = derive(
            Some(mfid1),
            btreemap! {
                TWOS_CSID => btreemap! {
                    "/one" => None,
                },
            },
        )
        .await?;

        assert_eq!(
            files_reference(btreemap! {
                "/one/one" => "one",
            })?,
            files(mfid1).await?,
        );
        let mfid2 = *mfs.get(&TWOS_CSID).unwrap();
        assert_eq!(
            files_reference(btreemap! {
                "/one/one" => "one",
            })?,
            files(mfid2).await?,
        );
    }

    // Weird case - deleting non-existing file is fine
    {
        let mfs = derive(
            None,
            btreemap! {
                ONES_CSID => btreemap! {
                    "/file" => Some("content"),
                    "/one/one" => None,
                },
            },
        )
        .await?;

        let mfid1 = *mfs.get(&ONES_CSID).unwrap();
        assert_eq!(
            files_reference(btreemap! {
                "/file" => "content",
            })?,
            files(mfid1).await?,
        );
    }

    // Replace a dir with a file in a single commit
    {
        let mfs = derive(
            None,
            btreemap! {
                ONES_CSID => btreemap! {
                    "/one" => Some("one"),
                },
            },
        )
        .await?;

        let mfid1 = *mfs.get(&ONES_CSID).unwrap();

        let mfs = derive(
            Some(mfid1),
            btreemap! {
                TWOS_CSID => btreemap! {
                    "/one" => None,
                    "/one/one" => Some("one"),
                },
            },
        )
        .await?;
        let mfid2 = *mfs.get(&TWOS_CSID).unwrap();
        assert_eq!(
            files_reference(btreemap! {
                "/one/one" => "one",
            })?,
            files(mfid2).await?,
        );
    }

    // Replace a dir with a file in a single commit
    {
        let mfs = derive(
            None,
            btreemap! {
                ONES_CSID => btreemap! {
                    "/dir/one" => Some("one"),
                },
            },
        )
        .await?;

        let mfid1 = *mfs.get(&ONES_CSID).unwrap();

        let mfs = derive(
            Some(mfid1),
            btreemap! {
                TWOS_CSID => btreemap! {
                    "/dir/anotherfile" => Some("anotherfile"),
                },
                THREES_CSID => btreemap! {
                    "/dir/one" => None,
                    "/dir/one/one" => Some("one"),
                },
            },
        )
        .await?;

        let mfid2 = *mfs.get(&TWOS_CSID).unwrap();
        assert_eq!(
            files_reference(btreemap! {
                "/dir/one" => "one",
                "/dir/anotherfile" => "anotherfile",
            })?,
            files(mfid2).await?,
        );

        let mfid3 = *mfs.get(&THREES_CSID).unwrap();
        assert_eq!(
            files_reference(btreemap! {
                "/dir/one/one" => "one",
                "/dir/anotherfile" => "anotherfile",
            })?,
            files(mfid3).await?,
        );
    }

    Ok(())
}

fn make_paths(paths_str: &[&str]) -> Result<Vec<MPath>> {
    let mut paths = paths_str
        .iter()
        .map(|path_str| match path_str {
            &"/" => Ok(MPath::ROOT),
            _ => MPath::new(path_str),
        })
        .collect::<Result<Vec<_>>>()?;
    paths.sort();
    Ok(paths)
}

fn describe_diff_item(diff: Diff<Entry<TestManifestId, (FileType, TestLeafId)>>) -> String {
    match diff {
        Diff::Added(path, entry) => format!(
            "A {}{}",
            MPath::display_opt(&path),
            if entry.is_tree() { "/" } else { "" }
        ),
        Diff::Removed(path, entry) => format!(
            "R {}{}",
            MPath::display_opt(&path),
            if entry.is_tree() { "/" } else { "" }
        ),
        Diff::Changed(path, entry, _) => format!(
            "C {}{}",
            MPath::display_opt(&path),
            if entry.is_tree() { "/" } else { "" }
        ),
    }
}

#[mononoke::fbinit_test]
async fn test_find_entries(fb: FacebookInit) -> Result<()> {
    let blobstore: Arc<dyn KeyedBlobstore> = Arc::new(KeyedMemblob::default());
    let ctx = CoreContext::test_mock(fb);
    borrowed!(ctx, blobstore);

    let mf0 = derive_test_manifest(
        ctx,
        blobstore,
        vec![],
        btreemap! {
            "one/1" => Some("1"),
            "one/2" => Some("2"),
            "3" => Some("3"),
            "two/three/4" => Some("4"),
            "two/three/5" => Some("5"),
            "two/three/four/6" => Some("6"),
            "two/three/four/7" => Some("7"),
            "five/six/8" => Some("8"),
            "five/seven/eight/9" => Some("9"),
            "five/seven/eight/nine/10" => Some("10"),
            "five/seven/11" => Some("11"),
        },
    )
    .await?
    .expect("expect non empty manifest");

    // use single select
    {
        let paths = make_paths(&[
            "one/1",
            "two/three",
            "none",
            "two/three/four/7",
            "two/three/6",
        ])?;

        let results: Vec<_> = mf0
            .find_entries(ctx.clone(), blobstore.clone(), paths)
            .try_collect()
            .await?;

        let mut leafs = Vec::new();
        let mut trees = Vec::new();
        for (path, entry) in results {
            match entry {
                Entry::Tree(_) => trees.push(path),
                Entry::Leaf(_) => leafs.push(path),
            };
        }
        leafs.sort();
        trees.sort();

        assert_eq!(leafs, make_paths(&["one/1", "two/three/four/7",])?);
        assert_eq!(trees, make_paths(&["two/three"])?);
    }

    // use prefix + single select
    {
        let paths = vec![
            PathOrPrefix::Path(MPath::new("two/three/5")?),
            PathOrPrefix::Path(MPath::new("five/seven/11")?),
            PathOrPrefix::Prefix(MPath::new("five/seven/eight")?),
        ];

        let results: Vec<_> = mf0
            .find_entries(ctx.clone(), blobstore.clone(), paths)
            .try_collect()
            .await?;

        let mut leafs = Vec::new();
        let mut trees = Vec::new();
        for (path, entry) in results {
            match entry {
                Entry::Tree(_) => trees.push(path),
                Entry::Leaf(_) => leafs.push(path),
            };
        }
        leafs.sort();
        trees.sort();

        assert_eq!(
            leafs,
            make_paths(&[
                "two/three/5",
                "five/seven/11",
                "five/seven/eight/9",
                "five/seven/eight/nine/10"
            ])?
        );
        assert_eq!(
            trees,
            make_paths(&["five/seven/eight", "five/seven/eight/nine"])?
        );
    }

    // find entry
    {
        let mf_root = mf0
            .find_entry(ctx.clone(), blobstore.clone(), MPath::ROOT)
            .await?;
        assert_eq!(mf_root, Some(Entry::Tree(mf0)));

        let paths = make_paths(&[
            "one/1",
            "two/three",
            "none",
            "two/three/four/7",
            "two/three/6",
            "five/seven/11",
        ])?;

        let mut leafs = Vec::new();
        let mut trees = Vec::new();
        for path in paths {
            let entry = mf0
                .find_entry(ctx.clone(), blobstore.clone(), path.clone())
                .await?;
            match entry {
                Some(Entry::Tree(_)) => {
                    trees.push(path);
                }
                Some(Entry::Leaf(_)) => {
                    leafs.push(path);
                }
                _ => {}
            };
        }
        leafs.sort();
        trees.sort();

        assert_eq!(
            leafs,
            make_paths(&["one/1", "two/three/four/7", "five/seven/11"])?
        );
        assert_eq!(trees, make_paths(&["two/three"])?);
    }

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_diff(fb: FacebookInit) -> Result<()> {
    let blobstore: Arc<dyn KeyedBlobstore> = Arc::new(KeyedMemblob::default());
    let ctx = CoreContext::test_mock(fb);
    borrowed!(ctx, blobstore);

    let mf0 = derive_test_manifest(
        ctx,
        blobstore,
        vec![],
        btreemap! {
            "same_dir/1" => Some("1"),
            "dir/same_dir/2" => Some("2"),
            "dir/removed_dir/3" => Some("3"),
            "dir/changed_file" => Some("before"),
            "dir/removed_file" => Some("removed_file"),
            "same_file" => Some("4"),
            "dir_file_conflict/5" => Some("5"),
        },
    )
    .await?
    .expect("expect non empty manifest");

    let mf1 = derive_test_manifest(
        ctx,
        blobstore,
        vec![],
        btreemap! {
            "same_dir/1" => Some("1"),
            "dir/same_dir/2" => Some("2"),
            "dir/added_dir/3" => Some("3"),
            "dir/changed_file" => Some("after"),
            "added_file" => Some("added_file"),
            "same_file" => Some("4"),
            "dir_file_conflict" => Some("5"),
        },
    )
    .await?
    .expect("expect non empty manifest");

    let diffs: Vec<_> = mf0
        .diff(ctx.clone(), blobstore.clone(), mf1)
        .try_collect()
        .await?;

    let mut added = Vec::new();
    let mut removed = Vec::new();
    let mut changed = Vec::new();
    for diff in diffs {
        match diff {
            Diff::Added(path, _) => {
                added.push(path);
            }
            Diff::Removed(path, _) => {
                removed.push(path);
            }
            Diff::Changed(path, _, _) => {
                changed.push(path);
            }
        };
    }
    added.sort();
    removed.sort();
    changed.sort();

    assert_eq!(
        added,
        make_paths(&[
            "added_file",
            "dir/added_dir",
            "dir/added_dir/3",
            "dir_file_conflict"
        ])?
    );
    assert_eq!(
        removed,
        make_paths(&[
            "dir/removed_file",
            "dir/removed_dir",
            "dir/removed_dir/3",
            "dir_file_conflict",
            "dir_file_conflict/5"
        ])?
    );
    assert_eq!(changed, make_paths(&["/", "dir", "dir/changed_file"])?);

    let diffs: Vec<_> = mf0
        .diff_ordered(ctx.clone(), blobstore.clone(), mf1, None)
        .map_ok(describe_diff_item)
        .try_collect()
        .await?;

    assert_eq!(
        diffs,
        vec![
            "C (none)/",
            "A added_file",
            "C dir/",
            "A dir/added_dir/",
            "A dir/added_dir/3",
            "C dir/changed_file",
            "R dir/removed_dir/",
            "R dir/removed_dir/3",
            "R dir/removed_file",
            "A dir_file_conflict",
            "R dir_file_conflict/",
            "R dir_file_conflict/5",
        ]
    );

    let diffs: Vec<_> = mf0
        .filtered_diff(
            ctx.clone(),
            blobstore.clone(),
            mf1,
            blobstore.clone(),
            |diff| {
                let path = match &diff {
                    Diff::Added(path, ..) | Diff::Removed(path, ..) | Diff::Changed(path, ..) => {
                        path
                    }
                };

                let badpath = MPath::new("dir/added_dir/3").unwrap();
                if &badpath != path { Some(diff) } else { None }
            },
            |diff| {
                let path = match diff {
                    Diff::Added(path, ..) | Diff::Removed(path, ..) | Diff::Changed(path, ..) => {
                        path
                    }
                };

                let badpath = MPath::new("dir/removed_dir").unwrap();
                &badpath != path
            },
            Default::default(),
        )
        .try_collect()
        .await?;

    let mut added = Vec::new();
    let mut removed = Vec::new();
    let mut changed = Vec::new();
    for diff in diffs {
        match diff {
            Diff::Added(path, _) => {
                added.push(path);
            }
            Diff::Removed(path, _) => {
                removed.push(path);
            }
            Diff::Changed(path, _, _) => {
                changed.push(path);
            }
        };
    }
    added.sort();
    removed.sort();
    changed.sort();

    assert_eq!(
        added,
        make_paths(&["added_file", "dir/added_dir", "dir_file_conflict"])?
    );
    assert_eq!(
        removed,
        make_paths(&[
            "dir/removed_file",
            "dir_file_conflict",
            "dir_file_conflict/5"
        ])?
    );
    assert_eq!(changed, make_paths(&["/", "dir", "dir/changed_file"])?);

    let diffs: Vec<_> = mf0
        .filtered_diff_ordered(
            ctx.clone(),
            blobstore.clone(),
            mf1,
            blobstore.clone(),
            Some(MPath::new("dir")?),
            |diff| {
                let path = match &diff {
                    Diff::Added(path, ..) | Diff::Removed(path, ..) | Diff::Changed(path, ..) => {
                        path
                    }
                };

                let badpath = MPath::new("dir/added_dir/3").unwrap();
                if &badpath != path { Some(diff) } else { None }
            },
            |diff| {
                let path = match diff {
                    Diff::Added(path, ..) | Diff::Removed(path, ..) | Diff::Changed(path, ..) => {
                        path
                    }
                };

                let badpath = MPath::new("dir/removed_dir").unwrap();
                &badpath != path
            },
            Default::default(),
        )
        .map_ok(describe_diff_item)
        .try_collect()
        .await?;

    assert_eq!(
        diffs,
        vec![
            "A dir/added_dir/",
            "C dir/changed_file",
            "R dir/removed_file",
            "A dir_file_conflict",
            "R dir_file_conflict/",
            "R dir_file_conflict/5",
        ]
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_diff_ordered_one_sided_replacements(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let old_store: Arc<dyn KeyedBlobstore> = Arc::new(KeyedMemblob::default());
    let new_store: Arc<dyn KeyedBlobstore> = Arc::new(KeyedMemblob::default());
    let old = derive_test_manifest(
        &ctx,
        &old_store,
        vec![],
        btreemap! {
            "removed/original" => Some("original"),
            "removed/unchanged" => Some("unchanged"),
        },
    )
    .await?
    .expect("expect non empty manifest");
    let new = derive_test_manifest(
        &ctx,
        &new_store,
        vec![],
        btreemap! {
            "added/new" => Some("new"),
            "added/same" => Some("same"),
        },
    )
    .await?
    .expect("expect non empty manifest");
    let replacement = Entry::Leaf((
        FileType::Regular,
        TestLeaf::new("replacement").store(&ctx, &old_store).await?,
    ));
    let same = Entry::Leaf((
        FileType::Regular,
        TestLeaf::new("same").store(&ctx, &old_store).await?,
    ));

    let diffs: Vec<_> = old
        .filtered_diff_ordered(
            ctx,
            old_store,
            new,
            new_store,
            None,
            Some,
            |_| true,
            HashMap::from([
                (MPath::new("added/injected")?, replacement.clone()),
                (MPath::new("added/same")?, same),
                (MPath::new("removed/injected")?, replacement.clone()),
                (MPath::new("removed/original")?, replacement.clone()),
            ]),
        )
        .try_collect()
        .await?;

    assert_eq!(
        diffs
            .iter()
            .cloned()
            .map(describe_diff_item)
            .collect::<Vec<_>>(),
        vec![
            "C (none)/",
            "A added/",
            "R added/injected",
            "A added/new",
            "R removed/",
            "R removed/injected",
            "R removed/original",
            "R removed/unchanged",
        ]
    );
    assert_eq!(
        diffs[6],
        Diff::Removed(
            MPath::new("removed/original")?,
            replacement.map_tree(|(_, tree)| tree),
        )
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_find_intersection_of_diffs(fb: FacebookInit) -> Result<()> {
    let blobstore: Arc<dyn KeyedBlobstore> = Arc::new(KeyedMemblob::default());
    let ctx = CoreContext::test_mock(fb);
    borrowed!(ctx, blobstore);

    let mf0 = derive_test_manifest(
        ctx,
        blobstore,
        vec![],
        btreemap! {
            "same_dir/1" => Some("1"),
            "dir/same_dir/2" => Some("2"),
            "dir/removed_dir/3" => Some("3"),
            "dir/changed_file" => Some("before"),
            "dir/removed_file" => Some("removed_file"),
            "same_file" => Some("4"),
            "dir_file_conflict/5" => Some("5"),
        },
    )
    .await?
    .expect("expect non empty manifest");

    let mf1 = derive_test_manifest(
        ctx,
        blobstore,
        vec![],
        btreemap! {
            "same_dir/1" => Some("1"),
            "dir/same_dir/2" => Some("2"),
            "dir/added_dir/3" => Some("3"),
            "dir/changed_file" => Some("after"),
            "added_file" => Some("added_file"),
            "same_file" => Some("4"),
            "dir_file_conflict" => Some("5"),
        },
    )
    .await?
    .expect("expect non empty manifest");

    let mf2 = derive_test_manifest(
        ctx,
        blobstore,
        vec![],
        btreemap! {
            "added_file" => Some("added_file"),
            "same_file" => Some("4"),
            "dir_file_conflict" => Some("5"),
        },
    )
    .await?
    .expect("expect non empty manifest");

    let intersection: Vec<_> =
        find_intersection_of_diffs(ctx.clone(), blobstore.clone(), mf0, vec![mf0])
            .try_collect()
            .await?;

    assert_eq!(intersection, vec![]);

    let mut intersection: Vec<_> =
        find_intersection_of_diffs(ctx.clone(), blobstore.clone(), mf1, vec![mf0])
            .map_ok(|(path, _)| path)
            .try_collect()
            .await?;
    intersection.sort();

    assert_eq!(
        intersection,
        make_paths(&[
            "/",
            "added_file",
            "dir",
            "dir/added_dir",
            "dir/added_dir/3",
            "dir/changed_file",
            "dir_file_conflict"
        ])?,
    );

    // Diff against two manifests
    let mut intersection: Vec<_> =
        find_intersection_of_diffs(ctx.clone(), blobstore.clone(), mf1, vec![mf0, mf2])
            .map_ok(|(path, _)| path)
            .try_collect()
            .await?;
    intersection.sort();

    assert_eq!(
        intersection,
        make_paths(&[
            "/",
            "dir",
            "dir/added_dir",
            "dir/added_dir/3",
            "dir/changed_file",
        ])?,
    );

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_diff_replacements_in_added_and_removed_subtrees(fb: FacebookInit) -> Result<()> {
    let blobstore: Arc<dyn KeyedBlobstore> = Arc::new(KeyedMemblob::default());
    let ctx = CoreContext::test_mock(fb);
    let old = derive_test_manifest(
        &ctx,
        &blobstore,
        vec![],
        btreemap! { "anchor" => Some("same"), "removed/original" => Some("old") },
    )
    .await?
    .unwrap();
    let new = derive_test_manifest(
        &ctx,
        &blobstore,
        vec![],
        btreemap! { "anchor" => Some("same"), "added/original" => Some("new") },
    )
    .await?
    .unwrap();
    let replacement = new
        .find_entry(
            ctx.clone(),
            blobstore.clone(),
            MPath::new("added/original")?,
        )
        .await?
        .unwrap();
    let replacements: HashMap<_, _> = [
        "added/original",
        "added/injected",
        "removed/original",
        "removed/injected",
    ]
    .into_iter()
    .map(|path| Ok((MPath::new(path)?, replacement)))
    .collect::<Result<_>>()?;
    let expected = vec![
        "C (none)/",
        "A added/",
        "R added/injected",
        "R removed/",
        "R removed/injected",
        "R removed/original",
    ];

    let unordered: Vec<_> = old
        .filtered_diff(
            ctx.clone(),
            blobstore.clone(),
            new,
            blobstore.clone(),
            Some,
            |_| true,
            replacements.clone(),
        )
        .try_collect()
        .await?;
    for path in ["added/injected", "removed/injected", "removed/original"] {
        assert!(unordered.contains(&Diff::Removed(MPath::new(path)?, replacement)));
    }
    let mut unordered: Vec<_> = unordered.into_iter().map(describe_diff_item).collect();
    unordered.sort();
    let mut sorted_expected = expected.clone();
    sorted_expected.sort();
    assert_eq!(unordered, sorted_expected);

    for (after, expected) in [
        (None, expected.as_slice()),
        (Some(MPath::new("added")?), &expected[2..]),
        (Some(MPath::new("removed/injected")?), &expected[5..]),
    ] {
        let ordered: Vec<_> = old
            .filtered_diff_ordered(
                ctx.clone(),
                blobstore.clone(),
                new,
                blobstore.clone(),
                after,
                Some,
                |_| true,
                replacements
                    .iter()
                    .map(|(path, entry)| (path.clone(), entry.map_tree(|id| (10, id))))
                    .collect(),
            )
            .map_ok(describe_diff_item)
            .try_collect()
            .await?;
        assert_eq!(ordered, expected);
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_diff_intersection_parent_entries_and_pruning(fb: FacebookInit) -> Result<()> {
    let blobstore: Arc<dyn KeyedBlobstore> = Arc::new(KeyedMemblob::default());
    let ctx = CoreContext::test_mock(fb);
    let old = derive_test_manifest(
        &ctx,
        &blobstore,
        vec![],
        btreemap! {
            "changed" => Some("old"),
            "deleted/child" => Some("old"),
            "dir_to_file/child" => Some("old"),
            "file_to_dir" => Some("old"),
            "pruned/child" => Some("old"),
        },
    )
    .await?
    .unwrap();
    let new = derive_test_manifest(
        &ctx,
        &blobstore,
        vec![],
        btreemap! {
            "added" => Some("new"),
            "changed" => Some("new"),
            "dir_to_file" => Some("new"),
            "file_to_dir/child" => Some("new"),
            "pruned/child" => Some("new"),
        },
    )
    .await?
    .unwrap();
    let pruned = MPath::new("pruned")?;
    let entries: BTreeMap<_, _> = find_intersection_of_diffs_and_parents_pruned(
        ctx.clone(),
        blobstore.clone(),
        new,
        vec![old],
        move |diff| diff.path() != &pruned,
    )
    .map_ok(|(path, entry, parents)| (path, (entry, parents)))
    .try_collect()
    .await?;
    assert_eq!(
        entries.keys().cloned().collect::<Vec<_>>(),
        make_paths(&[
            "/",
            "added",
            "changed",
            "dir_to_file",
            "file_to_dir",
            "file_to_dir/child"
        ])?,
    );
    assert_eq!(entries[&MPath::ROOT].1, vec![Entry::Tree(old)]);
    assert_eq!(
        entries[&MPath::new("changed")?].1,
        vec![lookup_entry(&ctx, &blobstore, old, "changed").await?],
    );
    for path in ["added", "dir_to_file", "file_to_dir", "file_to_dir/child"] {
        assert!(entries[&MPath::new(path)?].1.is_empty());
    }
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_bonsai_diff_sharded_matches_bonsai_diff(fb: FacebookInit) -> Result<()> {
    let blobstore: Arc<dyn KeyedBlobstore> = Arc::new(KeyedMemblob::default());
    let ctx = CoreContext::test_mock(fb);
    let base = derive_test_manifest(
        &ctx,
        &blobstore,
        vec![],
        btreemap! {
            "dir/same" => Some("base"),
            "dir/changed" => Some("base"),
            "file_to_dir" => Some("base"),
            "dir_to_file/child" => Some("base"),
            "deleted/child" => Some("base"),
        },
    )
    .await?
    .unwrap();
    let left = derive_test_manifest(
        &ctx,
        &blobstore,
        vec![base],
        btreemap! {
            "dir/changed" => Some("left"),
            "file_to_dir" => None,
            "file_to_dir/child" => Some("left"),
            "deleted/child" => None,
            "added" => Some("left"),
        },
    )
    .await?
    .unwrap();
    let right = derive_test_manifest(
        &ctx,
        &blobstore,
        vec![base],
        btreemap! {
            "dir/changed" => Some("right"),
            "dir_to_file/child" => None,
            "dir_to_file" => Some("right"),
        },
    )
    .await?
    .unwrap();

    for (node, parents) in [
        (left, vec![base]),
        (right, vec![base]),
        (base, vec![left]),
        (left, vec![base, right]),
        (base, vec![left, right]),
        (left, vec![left]),
        (left, vec![]),
    ] {
        let diff = |sharded: bool| {
            let parents = parents.iter().cloned().collect();
            let changes = if sharded {
                crate::bonsai_diff_sharded(ctx.clone(), blobstore.clone(), node, parents).boxed()
            } else {
                crate::bonsai_diff(ctx.clone(), blobstore.clone(), node, parents).boxed()
            };
            changes.try_collect::<std::collections::HashSet<_>>()
        };
        assert_eq!(
            diff(true).await?,
            diff(false).await?,
            "the sharded diff of {node:?} against {parents:?} matches the full diff"
        );
    }
    Ok(())
}

/// Records the key of every `get`, so a test can see which shards a diff loads.
#[derive(Debug, Default)]
struct RecordingBlobstore {
    inner: KeyedMemblob,
    gets: Mutex<Vec<String>>,
}

impl RecordingBlobstore {
    /// The content manifest shards read since the last call.
    fn take_shard_reads(&self) -> HashSet<String> {
        self.gets
            .lock()
            .expect("lock poisoned")
            .drain(..)
            .filter(|key| key.starts_with("contentmf.map2node."))
            .collect()
    }
}

impl std::fmt::Display for RecordingBlobstore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RecordingBlobstore")
    }
}

#[async_trait]
impl KeyedBlobstore for RecordingBlobstore {
    async fn get<'a>(
        &'a self,
        ctx: &'a CoreContext,
        key: &'a str,
    ) -> Result<Option<BlobstoreGetData>> {
        self.gets
            .lock()
            .expect("lock poisoned")
            .push(key.to_string());
        self.inner.get(ctx, key).await
    }

    async fn put<'a>(
        &'a self,
        ctx: &'a CoreContext,
        key: String,
        value: BlobstoreBytes,
    ) -> Result<()> {
        self.inner.put(ctx, key, value).await
    }

    async fn is_present<'a>(
        &'a self,
        ctx: &'a CoreContext,
        key: &'a str,
    ) -> Result<BlobstoreIsPresent> {
        self.inner.is_present(ctx, key).await
    }

    async fn copy<'a>(
        &'a self,
        ctx: &'a CoreContext,
        old_key: &'a str,
        new_key: String,
    ) -> Result<()> {
        self.inner.copy(ctx, old_key, new_key).await
    }

    async fn unlink<'a>(&'a self, ctx: &'a CoreContext, key: &'a str) -> Result<()> {
        self.inner.unlink(ctx, key).await
    }
}

#[mononoke::fbinit_test]
async fn test_bonsai_diff_sharded_skips_shards_shared_by_a_merge_and_both_parents(
    fb: FacebookInit,
) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let blobstore = Arc::new(RecordingBlobstore::default());
    // Every file has a distinct size, so no two shards have the same content and
    // a shard id stands for one position in the directory.
    let file = |i: u64, content_id| ContentManifestFile {
        content_id,
        file_type: FileType::Regular,
        size: i,
    };
    // A root directory of 5,000 files, stored as many shards.
    let store_manifest = async |changed: &[u64]| {
        let entries = (0..5_000).map(|i| {
            let content_id = if changed.contains(&i) {
                TWOS_CTID
            } else {
                ONES_CTID
            };
            (
                format!("f{i:04}"),
                ContentManifestEntry::File(file(i, content_id)),
            )
        });
        let subentries = ShardedMapV2Node::from_entries(&ctx, &blobstore, entries).await?;
        ContentManifest { subentries }
            .into_blob()
            .store(&ctx, &blobstore)
            .await
    };
    // Each parent changes a file in a different shard, and the merge keeps both changes.
    let left = store_manifest(&[1_999]).await?;
    let right = store_manifest(&[4_999]).await?;
    let merge = store_manifest(&[1_999, 4_999]).await?;

    let list_shards = async |id: ContentManifestId| {
        blobstore.take_shard_reads();
        id.load(&ctx, &blobstore)
            .await?
            .into_subentries(&ctx, &blobstore)
            .try_collect::<Vec<_>>()
            .await?;
        anyhow::Ok(blobstore.take_shard_reads())
    };
    let merge_shards = list_shards(merge).await?;
    let left_shards = list_shards(left).await?;
    let right_shards = list_shards(right).await?;
    let all_shards: HashSet<String> = merge_shards
        .iter()
        .chain(&left_shards)
        .chain(&right_shards)
        .cloned()
        .collect();
    let shared_shards: HashSet<String> = merge_shards
        .iter()
        .filter(|key| left_shards.contains(*key) && right_shards.contains(*key))
        .cloned()
        .collect();
    assert!(
        !shared_shards.is_empty(),
        "the merge and both parents should share shards"
    );

    let parents = hashset! {left, right};
    blobstore.take_shard_reads();
    let full: HashSet<_> =
        crate::bonsai_diff(ctx.clone(), blobstore.clone(), merge, parents.clone())
            .try_collect()
            .await?;
    let full_reads = blobstore.take_shard_reads();
    let sharded: HashSet<_> =
        crate::bonsai_diff_sharded(ctx.clone(), blobstore.clone(), merge, parents)
            .try_collect()
            .await?;
    let sharded_reads = blobstore.take_shard_reads();

    let expected = hashset! {
        crate::BonsaiDiffFileChange::ChangedReusedId(NonRootMPath::new("f1999")?, file(1_999, TWOS_CTID)),
        crate::BonsaiDiffFileChange::ChangedReusedId(NonRootMPath::new("f4999")?, file(4_999, TWOS_CTID)),
    };
    assert_eq!(full, expected);
    assert_eq!(sharded, expected);
    assert_eq!(
        full_reads, all_shards,
        "bonsai_diff reads every shard of the merge and both parents"
    );
    assert_eq!(
        sharded_reads,
        all_shards.difference(&shared_shards).cloned().collect(),
        "bonsai_diff_sharded reads every shard that differs from either parent, and none the merge shares with both"
    );
    Ok(())
}

/// Look up an entry at a given single-element path in a manifest.
async fn lookup_entry(
    ctx: &CoreContext,
    blobstore: &Arc<dyn KeyedBlobstore>,
    mf_id: TestManifestId,
    name: &str,
) -> Result<Entry<TestManifestId, (FileType, TestLeafId)>> {
    let root = mf_id.load(ctx, blobstore).await?;
    let entry = root
        .lookup(
            ctx,
            blobstore,
            &MPathElement::new_from_slice(name.as_bytes())?,
        )
        .await?;
    Ok(entry.expect("entry should exist"))
}

#[mononoke::fbinit_test]
async fn test_derive_manifest_with_known_entries(fb: FacebookInit) -> Result<()> {
    let blobstore: Arc<dyn KeyedBlobstore> = Arc::new(KeyedMemblob::default());
    let ctx = CoreContext::test_mock(fb);
    borrowed!(ctx, blobstore);

    let derive = {
        move |parents, changes| async move {
            derive_test_manifest(ctx, blobstore, parents, changes)
                .await
                .map(|mf| mf.expect("expect non empty manifest"))
        }
    };

    let files = move |manifest_id| list_test_manifest(ctx, blobstore, manifest_id);

    // 1. known_entries produces same result as normal derivation
    //
    // Derive a manifest normally, then derive it again but providing one
    // subtree via known_entries. The results should be identical.
    {
        let parent_mf = derive(
            vec![],
            btreemap! {
                "/dir/a" => Some("a"),
                "/dir/b" => Some("b"),
                "/other/c" => Some("c"),
            },
        )
        .await?;

        // Derive normally: add a file under "other/", "dir/" is unchanged from parent.
        let normal_result = derive(
            vec![parent_mf],
            btreemap! {
                "/other/d" => Some("d"),
            },
        )
        .await?;

        // Extract the "dir/" subtree from the parent (it's unchanged, so this
        // is exactly what normal derivation would produce for "dir/").
        let dir_entry = lookup_entry(ctx, blobstore, parent_mf, "dir").await?;

        let mut known = HashMap::new();
        known.insert(MPath::new("dir")?, Some(dir_entry));

        let known_result = derive_test_manifest_with_known_entries(
            ctx,
            blobstore,
            vec![Entry::Tree(parent_mf)],
            btreemap! {
                "/other/d" => Some("d"),
            },
            known,
            MPath::ROOT,
        )
        .await?
        .and_then(|entry| entry.into_tree())
        .expect("expect non empty manifest");

        assert_eq!(files(normal_result).await?, files(known_result).await?);
    }

    // 2. Multiple known_entries
    //
    // Same idea but with two subtrees provided via known_entries.
    {
        let parent_mf = derive(
            vec![],
            btreemap! {
                "/aa/x" => Some("ax"),
                "/bb/y" => Some("by"),
                "/cc/z" => Some("cz"),
            },
        )
        .await?;

        let normal_result = derive(
            vec![parent_mf],
            btreemap! {
                "/cc/w" => Some("cw"),
            },
        )
        .await?;

        let entry_aa = lookup_entry(ctx, blobstore, parent_mf, "aa").await?;
        let entry_bb = lookup_entry(ctx, blobstore, parent_mf, "bb").await?;

        let mut known = HashMap::new();
        known.insert(MPath::new("aa")?, Some(entry_aa));
        known.insert(MPath::new("bb")?, Some(entry_bb));

        let known_result = derive_test_manifest_with_known_entries(
            ctx,
            blobstore,
            vec![Entry::Tree(parent_mf)],
            btreemap! {
                "/cc/w" => Some("cw"),
            },
            known,
            MPath::ROOT,
        )
        .await?
        .and_then(|entry| entry.into_tree())
        .expect("expect non empty manifest");

        assert_eq!(files(normal_result).await?, files(known_result).await?);
    }

    // 3. known_entries with deletion (None value)
    //
    // When a subtree is deleted, providing None in known_entries should
    // produce the same result as normal derivation that deletes those files.
    {
        let parent_mf = derive(
            vec![],
            btreemap! {
                "/removed/a" => Some("a"),
                "/removed/b" => Some("b"),
                "/kept/c" => Some("c"),
            },
        )
        .await?;

        // Derive normally: delete all files under "removed/".
        let normal_result = derive(
            vec![parent_mf],
            btreemap! {
                "/removed/a" => None,
                "/removed/b" => None,
            },
        )
        .await?;

        // Derive with known_entries: signal that "removed/" is deleted.
        // A change under "removed/" is needed to force the traversal to
        // recurse there, where the known subtree short-circuits.
        let mut known = HashMap::new();
        known.insert(MPath::new("removed")?, None);

        let known_result = derive_test_manifest_with_known_entries(
            ctx,
            blobstore,
            vec![Entry::Tree(parent_mf)],
            btreemap! {
                "/removed/a" => None,
                "/removed/b" => None,
            },
            known,
            MPath::ROOT,
        )
        .await?
        .and_then(|entry| entry.into_tree())
        .expect("expect non empty manifest");

        assert_eq!(files(normal_result).await?, files(known_result).await?);
    }

    // 4. known_entries with a leaf entry
    //
    // When a path has a file (leaf) instead of a directory, known_entries
    // should accept Entry::Leaf and produce the same result as normal derivation.
    {
        let parent_mf = derive(
            vec![],
            btreemap! {
                "/file" => Some("content"),
                "/other/a" => Some("a"),
            },
        )
        .await?;

        // Derive normally: change a different file, "file" is unchanged from parent.
        let normal_result = derive(
            vec![parent_mf],
            btreemap! {
                "/other/b" => Some("b"),
            },
        )
        .await?;

        // Look up the "file" leaf entry from the parent manifest.
        let file_entry = lookup_entry(ctx, blobstore, parent_mf, "file").await?;

        let mut known = HashMap::new();
        known.insert(MPath::new("file")?, Some(file_entry));

        let known_result = derive_test_manifest_with_known_entries(
            ctx,
            blobstore,
            vec![Entry::Tree(parent_mf)],
            btreemap! {
                "/other/b" => Some("b"),
            },
            known,
            MPath::ROOT,
        )
        .await?
        .and_then(|entry| entry.into_tree())
        .expect("expect non empty manifest");

        assert_eq!(files(normal_result).await?, files(known_result).await?);
    }

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_derive_manifest_with_prefix(fb: FacebookInit) -> Result<()> {
    let blobstore: Arc<dyn KeyedBlobstore> = Arc::new(KeyedMemblob::default());
    let ctx = CoreContext::test_mock(fb);
    borrowed!(ctx, blobstore);

    let derive = {
        move |parents, changes| async move {
            derive_test_manifest(ctx, blobstore, parents, changes)
                .await
                .map(|mf| mf.expect("expect non empty manifest"))
        }
    };

    let files = move |manifest_id| list_test_manifest(ctx, blobstore, manifest_id);

    // 1. Prefix filters changes to a subtree
    //
    // Derive with prefix "dir" — only changes under "dir/" should be included,
    // producing the same subtree as normal derivation's "dir/" entry.
    {
        let normal_mf = derive(
            vec![],
            btreemap! {
                "/dir/a" => Some("a"),
                "/dir/b" => Some("b"),
                "/other/c" => Some("c"),
            },
        )
        .await?;

        let dir_entry = lookup_entry(ctx, blobstore, normal_mf, "dir").await?;
        let expected_id = dir_entry.into_tree().expect("dir should be a tree");

        // Derive with prefix "dir" — "other/c" should be filtered out.
        let prefix_result = derive_test_manifest_with_known_entries(
            ctx,
            blobstore,
            vec![],
            btreemap! {
                "/dir/a" => Some("a"),
                "/dir/b" => Some("b"),
                "/other/c" => Some("c"),
            },
            HashMap::new(),
            MPath::new("dir")?,
        )
        .await?
        .and_then(|entry| entry.into_tree())
        .expect("expect non empty manifest");

        assert_eq!(files(expected_id).await?, files(prefix_result).await?);
    }

    // 2. Prefix with known_entries (derivation pipeline pattern)
    //
    // Derive dir1/ and dir2/ subtrees separately, then assemble root with
    // known_entries. Uses prefix for subtree stages, ROOT for root stage.
    {
        let parent_mf = derive(
            vec![],
            btreemap! {
                "/dir1/a" => Some("a"),
                "/dir2/b" => Some("b"),
            },
        )
        .await?;

        // Normal derivation of the full tree.
        let normal_result = derive(
            vec![parent_mf],
            btreemap! {
                "/dir1/c" => Some("c"),
                "/dir2/d" => Some("d"),
            },
        )
        .await?;

        let all_changes = btreemap! {
            "/dir1/c" => Some("c"),
            "/dir2/d" => Some("d"),
        };

        // Stage dir1: derive with prefix "dir1", passing all changes.
        let dir1_parent = lookup_entry(ctx, blobstore, parent_mf, "dir1").await?;
        let dir1_entry = derive_test_manifest_with_known_entries(
            ctx,
            blobstore,
            vec![dir1_parent],
            all_changes.clone(),
            HashMap::new(),
            MPath::new("dir1")?,
        )
        .await?
        .expect("dir1 should produce an entry");

        // Stage dir2: derive with prefix "dir2", passing all changes.
        let dir2_parent = lookup_entry(ctx, blobstore, parent_mf, "dir2").await?;
        let dir2_entry = derive_test_manifest_with_known_entries(
            ctx,
            blobstore,
            vec![dir2_parent],
            all_changes.clone(),
            HashMap::new(),
            MPath::new("dir2")?,
        )
        .await?
        .expect("dir2 should produce an entry");

        // Root stage: assemble with known_entries, prefix ROOT.
        let mut known = HashMap::new();
        known.insert(MPath::new("dir1")?, Some(dir1_entry));
        known.insert(MPath::new("dir2")?, Some(dir2_entry));

        let pipeline_result = derive_test_manifest_with_known_entries(
            ctx,
            blobstore,
            vec![Entry::Tree(parent_mf)],
            all_changes,
            known,
            MPath::ROOT,
        )
        .await?
        .and_then(|entry| entry.into_tree())
        .expect("expect non empty manifest");

        assert_eq!(files(normal_result).await?, files(pipeline_result).await?);
    }

    // 3. Prefix with implicit delete
    //
    // When a file is added at a strict prefix of the stage path,
    // the entire subtree is implicitly deleted (returns None).
    {
        let parent_mf = derive(
            vec![],
            btreemap! {
                "/dir/sub/a" => Some("a"),
            },
        )
        .await?;

        let dir_parent = lookup_entry(ctx, blobstore, parent_mf, "dir").await?;

        // Adding a file at "dir" (which is a strict prefix of "dir/sub")
        // should cause the "dir/sub" stage to return None.
        let result = derive_test_manifest_with_known_entries(
            ctx,
            blobstore,
            vec![dir_parent],
            btreemap! {
                "/dir" => Some("now_a_file"),
            },
            HashMap::new(),
            MPath::new("dir/sub")?,
        )
        .await?;

        assert!(result.is_none(), "implicit delete should return None");
    }

    // 4. Nested prefix (derivation pipeline with deep nesting)
    //
    // Derive dir/sub/ first, then dir/ with known_entries for sub/.
    {
        let normal_mf = derive(
            vec![],
            btreemap! {
                "/dir/sub/a" => Some("a"),
                "/dir/sub/b" => Some("b"),
                "/dir/c" => Some("c"),
            },
        )
        .await?;

        let all_changes = btreemap! {
            "/dir/sub/a" => Some("a"),
            "/dir/sub/b" => Some("b"),
            "/dir/c" => Some("c"),
        };

        // Stage dir/sub: derive with prefix "dir/sub".
        let sub_entry = derive_test_manifest_with_known_entries(
            ctx,
            blobstore,
            vec![],
            all_changes.clone(),
            HashMap::new(),
            MPath::new("dir/sub")?,
        )
        .await?
        .expect("dir/sub should produce an entry");

        // Stage dir: derive with prefix "dir", known_entries for "dir/sub".
        let mut known = HashMap::new();
        known.insert(MPath::new("dir/sub")?, Some(sub_entry));

        let dir_entry = derive_test_manifest_with_known_entries(
            ctx,
            blobstore,
            vec![],
            all_changes,
            known,
            MPath::new("dir")?,
        )
        .await?
        .and_then(|entry| entry.into_tree())
        .expect("dir should produce a tree");

        let expected_dir_entry = lookup_entry(ctx, blobstore, normal_mf, "dir").await?;
        let expected_dir_id = expected_dir_entry
            .into_tree()
            .expect("dir should be a tree");

        assert_eq!(files(expected_dir_id).await?, files(dir_entry).await?);
    }

    Ok(())
}
