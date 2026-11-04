/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashSet;
use std::slice::from_ref;
use std::sync::Arc;

use anyhow::Error;
use bonsai_p4_mapping::AddP4MappingErrorKind;
use bonsai_p4_mapping::BonsaiP4Mapping;
use bonsai_p4_mapping::BonsaiP4MappingEntry;
use bonsai_p4_mapping::BonsaisOrP4ChangelistIds;
use bonsai_p4_mapping::CachingBonsaiP4Mapping;
use bonsai_p4_mapping::SqlBonsaiP4MappingBuilder;
use context::CoreContext;
use fbinit::FacebookInit;
use mononoke_macros::mononoke;
use mononoke_types::BonsaiChangesetMut;
use mononoke_types::DateTime;
use mononoke_types_mocks::changesetid as bonsai;
use mononoke_types_mocks::p4_changelist_id::*;
use mononoke_types_mocks::repo::REPO_ONE;
use mononoke_types_mocks::repo::REPO_ZERO;
use sql_construct::SqlConstruct;
use test_repo_factory::TestRepoFactory;

#[mononoke::fbinit_test]
async fn test_add_and_get(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let mapping = SqlBonsaiP4MappingBuilder::with_sqlite_in_memory()?.build(REPO_ZERO);

    let entry = BonsaiP4MappingEntry {
        bcs_id: bonsai::ONES_CSID,
        p4_changelist_id: P4_CHANGELIST_ONE,
    };

    mapping.bulk_import(&ctx, from_ref(&entry)).await?;

    let result = mapping
        .get(
            &ctx,
            BonsaisOrP4ChangelistIds::Bonsai(vec![bonsai::ONES_CSID]),
        )
        .await?;
    assert_eq!(result, vec![entry.clone()]);

    let result = mapping
        .get_p4_changelist_id_from_bonsai(&ctx, bonsai::ONES_CSID)
        .await?;
    assert_eq!(result, Some(P4_CHANGELIST_ONE));

    let result = mapping
        .get_bonsai_from_p4_changelist_id(&ctx, P4_CHANGELIST_ONE)
        .await?;
    assert_eq!(result, Some(bonsai::ONES_CSID));

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_bulk_import(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let mapping = SqlBonsaiP4MappingBuilder::with_sqlite_in_memory()?.build(REPO_ZERO);

    let entry1 = BonsaiP4MappingEntry {
        bcs_id: bonsai::ONES_CSID,
        p4_changelist_id: P4_CHANGELIST_ONE,
    };
    let entry2 = BonsaiP4MappingEntry {
        bcs_id: bonsai::TWOS_CSID,
        p4_changelist_id: P4_CHANGELIST_TWO,
    };
    let entry3 = BonsaiP4MappingEntry {
        bcs_id: bonsai::THREES_CSID,
        p4_changelist_id: P4_CHANGELIST_THREE,
    };

    mapping
        .bulk_import(&ctx, &[entry1.clone(), entry2.clone(), entry3.clone()])
        .await?;

    // Every entry resolves in bulk from either side, and singly.
    let all = HashSet::from([entry1, entry2, entry3]);
    let by_changelist = mapping
        .get(
            &ctx,
            BonsaisOrP4ChangelistIds::P4ChangelistId(vec![
                P4_CHANGELIST_ONE,
                P4_CHANGELIST_TWO,
                P4_CHANGELIST_THREE,
            ]),
        )
        .await?;
    assert_eq!(HashSet::from_iter(by_changelist), all);
    let by_bonsai = mapping
        .get(
            &ctx,
            BonsaisOrP4ChangelistIds::Bonsai(vec![
                bonsai::ONES_CSID,
                bonsai::TWOS_CSID,
                bonsai::THREES_CSID,
            ]),
        )
        .await?;
    assert_eq!(HashSet::from_iter(by_bonsai), all);

    assert_eq!(
        mapping
            .get_bonsai_from_p4_changelist_id(&ctx, P4_CHANGELIST_TWO)
            .await?,
        Some(bonsai::TWOS_CSID)
    );

    Ok(())
}

/// Writes fail on any conflict rather than being silently ignored, and the existing mapping
/// is left untouched.
#[mononoke::fbinit_test]
async fn test_conflicting_insert_errors(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let mapping = SqlBonsaiP4MappingBuilder::with_sqlite_in_memory()?.build(REPO_ZERO);
    let original = BonsaiP4MappingEntry::new(bonsai::ONES_CSID, P4_CHANGELIST_ONE);
    mapping.bulk_import(&ctx, from_ref(&original)).await?;

    for conflicting in [
        // same changelist, different changeset
        BonsaiP4MappingEntry::new(bonsai::TWOS_CSID, P4_CHANGELIST_ONE),
        // same changeset, different changelist
        BonsaiP4MappingEntry::new(bonsai::ONES_CSID, P4_CHANGELIST_TWO),
        // the identical row again
        original.clone(),
    ] {
        assert!(
            mapping
                .bulk_import(&ctx, from_ref(&conflicting))
                .await
                .is_err()
        );
    }

    assert_eq!(
        mapping
            .get(
                &ctx,
                BonsaisOrP4ChangelistIds::Bonsai(vec![bonsai::ONES_CSID, bonsai::TWOS_CSID])
            )
            .await?,
        vec![original]
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_missing(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let mapping = SqlBonsaiP4MappingBuilder::with_sqlite_in_memory()?.build(REPO_ZERO);

    let result = mapping
        .get(
            &ctx,
            BonsaisOrP4ChangelistIds::Bonsai(vec![bonsai::ONES_CSID]),
        )
        .await?;
    assert_eq!(result, vec![]);

    // A changelist number that was never imported resolves to nothing rather
    // than erroring -- the sequence is sparse, so holes are expected.
    let result = mapping
        .get_bonsai_from_p4_changelist_id(&ctx, P4_CHANGELIST_FOUR)
        .await?;
    assert_eq!(result, None);

    Ok(())
}

/// The highest changelist of this repo, whatever the import order; other repos' don't count.
#[mononoke::fbinit_test]
async fn test_get_max(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let builder = SqlBonsaiP4MappingBuilder::with_sqlite_in_memory()?;
    let mapping = builder.clone().build(REPO_ZERO);
    assert_eq!(None, mapping.get_max(&ctx).await?);

    builder
        .build(REPO_ONE)
        .bulk_import(
            &ctx,
            &[BonsaiP4MappingEntry::new(
                bonsai::FOURS_CSID,
                P4_CHANGELIST_FOUR,
            )],
        )
        .await?;
    mapping
        .bulk_import(
            &ctx,
            &[
                BonsaiP4MappingEntry::new(bonsai::THREES_CSID, P4_CHANGELIST_THREE),
                BonsaiP4MappingEntry::new(bonsai::ONES_CSID, P4_CHANGELIST_ONE),
            ],
        )
        .await?;

    assert_eq!(Some(P4_CHANGELIST_THREE), mapping.get_max(&ctx).await?);
    let caching = CachingBonsaiP4Mapping::new_test(Arc::new(mapping));
    assert_eq!(Some(P4_CHANGELIST_THREE), caching.get_max(&ctx).await?);
    Ok(())
}

/// Changesets carrying the extras the importer writes are mapped to their changelist. A
/// changeset without a valid changelist (here: a bare number in `convert_revision` with no
/// Perforce source tag, or no extras at all) fails the whole batch rather than being skipped.
#[mononoke::fbinit_test]
async fn test_bulk_import_from_bonsai(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let mapping = SqlBonsaiP4MappingBuilder::with_sqlite_in_memory()?.build(REPO_ZERO);

    let changeset = |message: &str, hg_extra: Vec<(String, Vec<u8>)>| {
        BonsaiChangesetMut {
            author: "author".into(),
            author_date: DateTime::from_timestamp(0, 0)?,
            message: message.into(),
            hg_extra: hg_extra.into_iter().collect(),
            ..Default::default()
        }
        .freeze()
    };
    let imported = changeset("imported", P4_CHANGELIST_TWO.to_hg_extras().to_vec())?;
    let untagged = changeset(
        "untagged",
        vec![("convert_revision".to_string(), b"3".to_vec())],
    )?;
    let without_extras = changeset("without extras", vec![])?;

    // One bad changeset fails the whole batch, and nothing is written.
    for bad in [&untagged, &without_extras] {
        assert!(
            mapping
                .bulk_import_from_bonsai(&ctx, &[imported.clone(), (*bad).clone()])
                .await
                .is_err()
        );
    }
    assert_eq!(
        mapping
            .get(
                &ctx,
                BonsaisOrP4ChangelistIds::Bonsai(vec![imported.get_changeset_id()])
            )
            .await?,
        vec![]
    );

    // A batch of valid changesets maps each one to its changelist.
    mapping
        .bulk_import_from_bonsai(&ctx, from_ref(&imported))
        .await?;
    assert_eq!(
        mapping
            .get(
                &ctx,
                BonsaisOrP4ChangelistIds::Bonsai(vec![imported.get_changeset_id()])
            )
            .await?,
        vec![BonsaiP4MappingEntry::new(
            imported.get_changeset_id(),
            P4_CHANGELIST_TWO
        )]
    );

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_caching(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let mapping = Arc::new(SqlBonsaiP4MappingBuilder::with_sqlite_in_memory()?.build(REPO_ZERO));
    let caching = CachingBonsaiP4Mapping::new_test(mapping.clone());

    let store = caching
        .cachelib()
        .mock_store()
        .expect("new_test gives us a MockStore");

    let e0 = BonsaiP4MappingEntry::new(bonsai::ONES_CSID, P4_CHANGELIST_ONE);
    let e1 = BonsaiP4MappingEntry::new(bonsai::TWOS_CSID, P4_CHANGELIST_TWO);
    mapping.bulk_import(&ctx, &[e0, e1]).await?;

    // First lookup misses the cache, reads SQL and fills the cache.
    assert_eq!(
        caching
            .get_p4_changelist_id_from_bonsai(&ctx, bonsai::ONES_CSID)
            .await?,
        Some(P4_CHANGELIST_ONE)
    );
    assert_eq!(store.stats().gets, 1);
    assert_eq!(store.stats().hits, 0);
    assert_eq!(store.stats().sets, 1);

    // Second lookup is served from the cache.
    assert_eq!(
        caching
            .get_p4_changelist_id_from_bonsai(&ctx, bonsai::ONES_CSID)
            .await?,
        Some(P4_CHANGELIST_ONE)
    );
    assert_eq!(store.stats().gets, 2);
    assert_eq!(store.stats().hits, 1);
    assert_eq!(store.stats().sets, 1);

    // Both directions resolve through the cache.
    assert_eq!(
        caching
            .get_p4_changelist_id_from_bonsai(&ctx, bonsai::TWOS_CSID)
            .await?,
        Some(P4_CHANGELIST_TWO)
    );
    assert_eq!(
        caching
            .get_bonsai_from_p4_changelist_id(&ctx, P4_CHANGELIST_ONE)
            .await?,
        Some(bonsai::ONES_CSID)
    );
    assert_eq!(
        caching
            .get_bonsai_from_p4_changelist_id(&ctx, P4_CHANGELIST_TWO)
            .await?,
        Some(bonsai::TWOS_CSID)
    );

    // A changelist that was never imported is not invented by the cache.
    assert_eq!(
        caching
            .get_bonsai_from_p4_changelist_id(&ctx, P4_CHANGELIST_FOUR)
            .await?,
        None
    );

    Ok(())
}

#[facet::container]
struct P4Repo {
    #[facet]
    bonsai_p4_mapping: dyn BonsaiP4Mapping,
}

/// A repo built by the test repo factory gets a working `BonsaiP4Mapping` facet: the table is
/// created and the factory builds the store.
#[mononoke::fbinit_test]
async fn test_repo_factory_builds_facet(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let repo: P4Repo = TestRepoFactory::new(fb)?.build().await?;

    let entry = BonsaiP4MappingEntry::new(bonsai::ONES_CSID, P4_CHANGELIST_ONE);
    repo.bonsai_p4_mapping
        .bulk_import(&ctx, from_ref(&entry))
        .await?;
    assert_eq!(
        repo.bonsai_p4_mapping
            .get_bonsai_from_p4_changelist_id(&ctx, P4_CHANGELIST_ONE)
            .await?,
        Some(bonsai::ONES_CSID)
    );
    Ok(())
}

/// Entries written in a transaction appear only if it commits, so they can share one with a
/// bookmark move.
#[mononoke::fbinit_test]
async fn test_bulk_import_in_transaction(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let factory = TestRepoFactory::new(fb)?;
    let repo: P4Repo = factory.build().await?;
    let start = || {
        factory
            .metadata_db()
            .write_connection
            .start_transaction(ctx.sql_query_telemetry())
    };
    let rolled_back = BonsaiP4MappingEntry::new(bonsai::ONES_CSID, P4_CHANGELIST_ONE);
    let committed = BonsaiP4MappingEntry::new(bonsai::TWOS_CSID, P4_CHANGELIST_TWO);

    repo.bonsai_p4_mapping
        .bulk_import_in_transaction(&ctx, from_ref(&rolled_back), start().await?)
        .await?
        .rollback()
        .await?;
    repo.bonsai_p4_mapping
        .bulk_import_in_transaction(&ctx, from_ref(&committed), start().await?)
        .await?
        .commit()
        .await?;

    assert_eq!(
        repo.bonsai_p4_mapping
            .get(
                &ctx,
                BonsaisOrP4ChangelistIds::P4ChangelistId(vec![
                    P4_CHANGELIST_ONE,
                    P4_CHANGELIST_TWO
                ])
            )
            .await?,
        vec![committed]
    );
    Ok(())
}

/// A conflict fails the write without committing any of it, so the caller's transaction can
/// be dropped to roll everything back.
#[mononoke::fbinit_test]
async fn test_conflict_in_transaction_writes_nothing(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);
    let factory = TestRepoFactory::new(fb)?;
    let repo: P4Repo = factory.build().await?;
    let existing = BonsaiP4MappingEntry::new(bonsai::ONES_CSID, P4_CHANGELIST_ONE);
    repo.bonsai_p4_mapping
        .bulk_import(&ctx, from_ref(&existing))
        .await?;
    let transaction = factory
        .metadata_db()
        .write_connection
        .start_transaction(ctx.sql_query_telemetry())
        .await?;

    let result = repo
        .bonsai_p4_mapping
        .bulk_import_in_transaction(
            &ctx,
            &[
                BonsaiP4MappingEntry::new(bonsai::TWOS_CSID, P4_CHANGELIST_TWO),
                BonsaiP4MappingEntry::new(bonsai::THREES_CSID, P4_CHANGELIST_ONE),
            ],
            transaction,
        )
        .await;

    assert!(matches!(
        result,
        Err(AddP4MappingErrorKind::Conflict {
            conflicting: 1,
            total: 2
        })
    ));
    assert_eq!(
        repo.bonsai_p4_mapping
            .get_bonsai_from_p4_changelist_id(&ctx, P4_CHANGELIST_TWO)
            .await?,
        None
    );
    Ok(())
}
