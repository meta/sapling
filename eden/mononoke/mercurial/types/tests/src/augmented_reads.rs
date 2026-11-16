/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashSet;
use std::fmt::Debug;
use std::sync::Arc;

use anyhow::Error;
use anyhow::Result;
use blobstore::KeyedBlobstore;
use blobstore::Loadable;
use context::CoreContext;
use derivation_queue_thrift::DerivationPriority;
use fbinit::FacebookInit;
use futures::FutureExt;
use futures::TryStreamExt;
use justknobs::test_helpers::JustKnobsInMemory;
use justknobs::test_helpers::KnobVal;
use justknobs::test_helpers::with_just_knobs_async;
use manifest::BonsaiDiffFileChange;
use manifest::Diff;
use manifest::Entry;
use maplit::hashmap;
use maplit::hashset;
use memblob::Memblob;
use mercurial_derivation::RootHgAugmentedManifestId;
use mercurial_derivation::derive_hg_changeset;
use mercurial_types::FileType;
use mercurial_types::HgAugmentedManifestId;
use mercurial_types::HgFileNodeId;
use mercurial_types::HgManifestId;
use mercurial_types::NULL_HASH;
use mercurial_types::NonRootMPath;
use mercurial_types::sharded_augmented_manifest::find_hg_entry;
use mercurial_types::sharded_augmented_manifest::hg_manifest_bonsai_diff;
use mercurial_types::sharded_augmented_manifest::hg_manifest_diff;
use mercurial_types::sharded_augmented_manifest::list_all_hg_entries;
use mercurial_types::sharded_augmented_manifest::list_hg_leaf_entries;
use mononoke_macros::mononoke;
use mononoke_types::ChangesetId;
use mononoke_types::MPath;
use repo_blobstore::RepoBlobstoreRef;
use repo_derived_data::RepoDerivedDataRef;
use repo_identity::RepoIdentityRef;
use test_repo_factory::TestRepoFactory;
use tests_utils::BasicTestRepo;
use tests_utils::CreateCommitContext;
use tests_utils::hg_manifest_reads::HgManifestReadBlocker;

type HgEntry = Entry<HgManifestId, (FileType, HgFileNodeId)>;
type BonsaiChange = BonsaiDiffFileChange<(FileType, HgFileNodeId)>;

fn read_from_augmented(on: bool) -> JustKnobsInMemory {
    JustKnobsInMemory::new(hashmap! {
        "scm/mononoke:hgmanifest_read_from_augmented".to_string() => KnobVal::Bool(on),
    })
}

struct Fixture {
    repo: BasicTestRepo,
    blocker: Arc<HgManifestReadBlocker>,
    child: ChangesetId,
    root_mf: HgManifestId,
    child_mf: HgManifestId,
}

/// A repo whose Hg manifest reads fail once the blocker is armed, with a root
/// commit holding `a` and `dir/b`, and a child that removes `a`, changes
/// `dir/b` and adds `dir/c`. Only the root commit has an augmented manifest.
async fn fixture(ctx: &CoreContext) -> Result<Fixture> {
    let (blobstore, blocker) = HgManifestReadBlocker::wrap(Memblob::default());
    let repo: BasicTestRepo = TestRepoFactory::new(ctx.fb)?
        .with_blobstore(blobstore)
        .build()
        .await?;
    let root = CreateCommitContext::new_root(ctx, &repo)
        .add_file("a", "a")
        .add_file("dir/b", "b")
        .commit()
        .await?;
    let child = CreateCommitContext::new(ctx, &repo, vec![root])
        .delete_file("a")
        .add_file("dir/b", "b2")
        .add_file("dir/c", "c")
        .commit()
        .await?;
    // Derive the hg changesets without their augmented manifests.
    let (root_mf, child_mf) = with_just_knobs_async(
        JustKnobsInMemory::new(hashmap! {
            "scm/mononoke:derive_hg_augmented_manifest_with_hg_changeset".to_string() => KnobVal::Bool(false),
        }),
        async {
            // Derivation would also derive augmented manifests, so store the
            // Hg changesets as an upload does.
            fixtures::store_hg_changesets_as_uploaded(ctx, &repo, vec![root, child]).await?;
            anyhow::Ok((
                root_manifest(ctx, &repo, root).await?,
                root_manifest(ctx, &repo, child).await?,
            ))
        }
        .boxed(),
    )
    .await?;
    derive_augmented(ctx, &repo, root).await?;
    Ok(Fixture {
        repo,
        blocker,
        child,
        root_mf,
        child_mf,
    })
}

async fn root_manifest(
    ctx: &CoreContext,
    repo: &BasicTestRepo,
    cs_id: ChangesetId,
) -> Result<HgManifestId> {
    let hg_cs_id = derive_hg_changeset(ctx, repo.repo_derived_data(), cs_id).await?;
    Ok(hg_cs_id
        .load(ctx, repo.repo_blobstore())
        .await?
        .manifestid())
}

async fn derive_augmented(
    ctx: &CoreContext,
    repo: &BasicTestRepo,
    cs_id: ChangesetId,
) -> Result<()> {
    repo.repo_derived_data()
        .derive::<RootHgAugmentedManifestId>(ctx, cs_id, DerivationPriority::LOW)
        .await?;
    Ok(())
}

/// What the helpers read for a pair of manifests, with unordered results
/// sorted.
#[derive(Debug, PartialEq)]
struct Reads {
    /// Every entry of the old manifest.
    all: Vec<(MPath, HgEntry)>,
    /// Every file of the old manifest.
    leaves: Vec<(NonRootMPath, (FileType, HgFileNodeId))>,
    /// The diff from the old manifest to the new one.
    diff: Vec<Diff<HgEntry>>,
    /// The old manifest's entries at the root, `dir` and `dir/b`.
    found: Vec<Option<HgEntry>>,
    /// The Bonsai changes of the new manifest against the old one, and against
    /// the old one and the null manifest.
    bonsai: Vec<Vec<BonsaiChange>>,
}

fn sorted<T: Debug>(mut items: Vec<T>) -> Vec<T> {
    items.sort_by_key(|item| format!("{item:?}"));
    items
}

async fn bonsai_changes(
    ctx: &CoreContext,
    repo: &BasicTestRepo,
    mf_id: HgManifestId,
    parents: HashSet<HgManifestId>,
) -> Result<Vec<BonsaiChange>> {
    Ok(sorted(
        hg_manifest_bonsai_diff(
            ctx,
            repo.repo_blobstore().clone(),
            repo.repo_identity().name(),
            mf_id,
            parents,
        )
        .try_collect()
        .await?,
    ))
}

async fn reads(
    ctx: &CoreContext,
    repo: &BasicTestRepo,
    old: HgManifestId,
    new: HgManifestId,
) -> Result<Reads> {
    let blobstore = repo.repo_blobstore().clone();
    let name = repo.repo_identity().name();
    let null = HgManifestId::new(NULL_HASH);
    let mut found = Vec::new();
    for path in [MPath::ROOT, MPath::new("dir")?, MPath::new("dir/b")?] {
        found.push(find_hg_entry(ctx, blobstore.clone(), name, old, path).await?);
    }
    Ok(Reads {
        all: sorted(
            list_all_hg_entries(ctx, blobstore.clone(), name, old)
                .try_collect()
                .await?,
        ),
        leaves: sorted(
            list_hg_leaf_entries(ctx, blobstore.clone(), name, old)
                .try_collect()
                .await?,
        ),
        diff: sorted(
            hg_manifest_diff(ctx, blobstore, name, old, new)
                .try_collect()
                .await?,
        ),
        found,
        bonsai: vec![
            bonsai_changes(ctx, repo, new, hashset! {old}).await?,
            bonsai_changes(ctx, repo, new, hashset! {old, null}).await?,
        ],
    })
}

#[mononoke::fbinit_test]
async fn test_reads_from_augmented(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let f = fixture(&ctx).await?;
    derive_augmented(&ctx, &f.repo, f.child).await?;

    // Given: what the helpers read from Hg manifests.
    let from_hg = with_just_knobs_async(
        read_from_augmented(false),
        reads(&ctx, &f.repo, f.root_mf, f.child_mf).boxed(),
    )
    .await?;
    assert_eq!(from_hg.all.len(), 4, "the root, `a`, `dir` and `dir/b`");
    assert_eq!(from_hg.leaves.len(), 2, "`a` and `dir/b`");
    assert_eq!(
        from_hg.diff.len(),
        5,
        "the root and `dir` change, `a` is removed, `dir/b` changes, `dir/c` is added"
    );
    assert!(matches!(
        from_hg.found.as_slice(),
        [
            Some(Entry::Tree(_)),
            Some(Entry::Tree(_)),
            Some(Entry::Leaf((FileType::Regular, _)))
        ]
    ));
    assert_eq!(
        from_hg.bonsai[0].len(),
        3,
        "`a` is deleted, `dir/b` changes and `dir/c` is added"
    );
    assert_eq!(
        from_hg.bonsai[1], from_hg.bonsai[0],
        "a null second parent changes nothing"
    );

    // When: Hg manifests can no longer be read.
    f.blocker.arm();

    // Then: augmented manifests give the same results.
    let from_augmented = with_just_knobs_async(
        read_from_augmented(true),
        reads(&ctx, &f.repo, f.root_mf, f.child_mf).boxed(),
    )
    .await?;
    assert_eq!(from_augmented, from_hg);
    assert_eq!(
        f.blocker.reads(),
        0,
        "no Hg manifest is read with the knob on"
    );

    // And: with the knob off, the Hg manifests are still read.
    let knob_off = with_just_knobs_async(
        read_from_augmented(false),
        reads(&ctx, &f.repo, f.root_mf, f.child_mf).boxed(),
    )
    .await;
    assert!(knob_off.is_err(), "the knob off reads blocked Hg manifests");
    assert!(f.blocker.reads() > 0);
    Ok(())
}

/// What the helpers read when the null manifest is the old side, the new
/// side, and both sides of a diff with `mf_id`, and the Bonsai changes of the
/// null manifest against `mf_id` and `other` together.
async fn null_manifest_reads(
    ctx: &CoreContext,
    repo: &BasicTestRepo,
    mf_id: HgManifestId,
    other: HgManifestId,
) -> Result<(Reads, Reads, Reads, Vec<BonsaiChange>)> {
    let null = HgManifestId::new(NULL_HASH);
    Ok((
        reads(ctx, repo, null, mf_id).await?,
        reads(ctx, repo, mf_id, null).await?,
        reads(ctx, repo, null, null).await?,
        bonsai_changes(ctx, repo, null, hashset! {mf_id, other}).await?,
    ))
}

#[mononoke::fbinit_test]
async fn test_null_manifest_reads_from_augmented(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let f = fixture(&ctx).await?;
    derive_augmented(&ctx, &f.repo, f.child).await?;
    let null = HgManifestId::new(NULL_HASH);

    // Given: what the helpers read from Hg manifests, which read the null
    // manifest as an empty tree.
    let from_hg = with_just_knobs_async(
        read_from_augmented(false),
        null_manifest_reads(&ctx, &f.repo, f.root_mf, f.child_mf).boxed(),
    )
    .await?;
    let (from_null, to_null, null_to_null, null_merge) = &from_hg;
    assert_eq!(from_null.all, vec![(MPath::ROOT, Entry::Tree(null))]);
    assert!(from_null.leaves.is_empty());
    assert_eq!(from_null.found, vec![Some(Entry::Tree(null)), None, None]);
    assert_eq!(
        from_null.diff.len(),
        4,
        "the root changes and `a`, `dir` and `dir/b` are added"
    );
    assert!(from_null.diff.contains(&Diff::Changed(
        MPath::ROOT,
        Entry::Tree(null),
        Entry::Tree(f.root_mf)
    )));
    assert_eq!(
        to_null.diff.len(),
        4,
        "the root changes and `a`, `dir` and `dir/b` are removed"
    );
    assert!(to_null.diff.contains(&Diff::Changed(
        MPath::ROOT,
        Entry::Tree(f.root_mf),
        Entry::Tree(null)
    )));
    assert!(null_to_null.diff.is_empty());
    let deleted = |paths: &[&str]| -> Result<Vec<BonsaiChange>> {
        paths
            .iter()
            .map(|path| Ok(BonsaiDiffFileChange::Deleted(NonRootMPath::new(path)?)))
            .collect()
    };
    let added: Vec<BonsaiChange> = to_null
        .leaves
        .iter()
        .map(|(path, leaf)| BonsaiDiffFileChange::Changed(path.clone(), *leaf))
        .collect();
    assert_eq!(added.len(), 2, "`a` and `dir/b`");
    assert_eq!(
        from_null.bonsai,
        vec![added; 2],
        "the files of the new manifest are added"
    );
    assert_eq!(
        to_null.bonsai,
        vec![deleted(&["a", "dir/b"])?; 2],
        "the files of the old manifest are deleted"
    );
    assert!(null_to_null.bonsai.iter().all(Vec::is_empty));
    assert_eq!(
        *null_merge,
        deleted(&["a", "dir/b", "dir/c"])?,
        "the files of both parents are deleted, `dir/b` once"
    );

    // When: Hg manifests can no longer be read.
    f.blocker.arm();

    // Then: augmented manifests give the same results.
    let from_augmented = with_just_knobs_async(
        read_from_augmented(true),
        null_manifest_reads(&ctx, &f.repo, f.root_mf, f.child_mf).boxed(),
    )
    .await?;
    assert_eq!(from_augmented, from_hg);
    assert_eq!(
        f.blocker.reads(),
        0,
        "no Hg manifest is read with the knob on"
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_missing_augmented_manifest(fb: FacebookInit) -> Result<()> {
    let ctx = CoreContext::test_mock(fb);
    let f = fixture(&ctx).await?;
    let child_key = HgAugmentedManifestId::from(f.child_mf).blobstore_key();
    assert!(
        f.repo
            .repo_blobstore()
            .get(&ctx, &child_key)
            .await?
            .is_none(),
        "the child has no augmented manifest"
    );
    f.blocker.arm();

    let blobstore = f.repo.repo_blobstore().clone();
    let name = f.repo.repo_identity().name();
    let errors: Vec<Option<Error>> = with_just_knobs_async(
        read_from_augmented(true),
        async {
            vec![
                list_all_hg_entries(&ctx, blobstore.clone(), name, f.child_mf)
                    .try_collect::<Vec<_>>()
                    .await
                    .err(),
                list_hg_leaf_entries(&ctx, blobstore.clone(), name, f.child_mf)
                    .try_collect::<Vec<_>>()
                    .await
                    .err(),
                hg_manifest_diff(&ctx, blobstore.clone(), name, f.root_mf, f.child_mf)
                    .try_collect::<Vec<_>>()
                    .await
                    .err(),
                find_hg_entry(&ctx, blobstore.clone(), name, f.child_mf, MPath::ROOT)
                    .await
                    .err(),
                hg_manifest_bonsai_diff(
                    &ctx,
                    blobstore.clone(),
                    name,
                    f.child_mf,
                    hashset! {f.root_mf},
                )
                .try_collect::<Vec<_>>()
                .await
                .err(),
            ]
        }
        .boxed(),
    )
    .await;
    for error in errors {
        let error = error.expect("a missing augmented manifest is an error");
        assert!(
            format!("{error:#}").contains(&child_key),
            "the error names the missing augmented manifest: {error:#}"
        );
    }
    assert_eq!(
        f.blocker.reads(),
        0,
        "there is no fallback to the Hg manifest"
    );
    Ok(())
}
