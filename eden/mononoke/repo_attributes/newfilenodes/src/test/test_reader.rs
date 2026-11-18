/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicI64;

use anyhow::Error;
use anyhow::Result;
use anyhow::format_err;
use async_trait::async_trait;
use cloned::cloned;
use context::CoreContext;
use fbinit::FacebookInit;
use filenodes::FilenodeInfo;
use filenodes::FilenodeRange;
use filenodes::PreparedFilenode;
use justknobs::test_helpers::JustKnobsInMemory;
use justknobs::test_helpers::KnobVal;
use justknobs::test_helpers::override_just_knobs;
use mercurial_types::HgFileNodeId;
use mercurial_types_mocks::nodehash::ONES_CSID;
use mercurial_types_mocks::nodehash::ONES_FNID;
use mercurial_types_mocks::nodehash::THREES_CSID;
use mercurial_types_mocks::nodehash::THREES_FNID;
use mercurial_types_mocks::nodehash::TWOS_CSID;
use mercurial_types_mocks::nodehash::TWOS_FNID;
use mononoke_macros::mononoke;
use mononoke_types::NonRootMPath;
use mononoke_types::RepoPath;
use mononoke_types::RepositoryId;
use mononoke_types::Timestamp;
use mononoke_types_mocks::repo::REPO_ONE;
use mononoke_types_mocks::repo::REPO_ZERO;
use sql::sqlite::SqliteCallbacks;
use sql::sqlite::SqliteHlcProvider;
use sql::sqlite::SqliteQueryType;
use sql_ext::Connection;
use sql_ext::mononoke_queries;
use vec1::Vec1;
use vec1::vec1;

use super::util::build_reader_writer;
use super::util::build_shard;
use crate::builder::SQLITE_INSERT_CHUNK_SIZE;
use crate::local_cache::LocalCache;
use crate::reader::FilenodesReader;
use crate::test::util::build_shard_with_callbacks;
use crate::test::util::build_shard_with_hlc_provider;
use crate::writer::FilenodesWriter;

async fn check_roundtrip(
    ctx: &CoreContext,
    repo_id: RepositoryId,
    reader: Arc<FilenodesReader>,
    writer: &FilenodesWriter,
    payload: PreparedFilenode,
) -> Result<(), Error> {
    assert_eq!(
        async {
            let res = reader
                .clone()
                .get_filenode(ctx, repo_id, &payload.path, payload.info.filenode)
                .await?;
            res.do_not_handle_disabled_filenodes()
        }
        .await?,
        None
    );

    writer
        .insert_filenodes(ctx, repo_id, vec![payload.clone()], false)
        .await?
        .do_not_handle_disabled_filenodes()?;

    assert_eq!(
        async {
            let res = reader
                .get_filenode(ctx, repo_id, &payload.path, payload.info.filenode)
                .await?;
            res.do_not_handle_disabled_filenodes()
        }
        .await?,
        Some(payload.info),
    );

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_basic(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);

    let shard = build_shard()?;
    let (reader, writer) = build_reader_writer(vec1![shard]);
    let reader = Arc::new(reader);

    let payload = PreparedFilenode {
        path: RepoPath::FilePath(NonRootMPath::new(b"test")?),
        info: FilenodeInfo {
            filenode: ONES_FNID,
            p1: Some(TWOS_FNID),
            p2: None,
            copyfrom: None,
            linknode: ONES_CSID,
        },
    };

    check_roundtrip(&ctx, REPO_ZERO, reader, &writer, payload).await?;

    Ok(())
}

#[mononoke::fbinit_test]
async fn read_copy_info(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);

    let shard = build_shard()?;
    let (reader, writer) = build_reader_writer(vec1![shard]);
    let reader = Arc::new(reader);

    let from = PreparedFilenode {
        path: RepoPath::FilePath(NonRootMPath::new(b"from")?),
        info: FilenodeInfo {
            filenode: ONES_FNID,
            p1: None,
            p2: None,
            copyfrom: None,
            linknode: ONES_CSID,
        },
    };

    writer
        .insert_filenodes(&ctx, REPO_ZERO, vec![from.clone()], false)
        .await?
        .do_not_handle_disabled_filenodes()?;

    let payload = PreparedFilenode {
        path: RepoPath::FilePath(NonRootMPath::new(b"test")?),
        info: FilenodeInfo {
            filenode: TWOS_FNID,
            p1: None,
            p2: None,
            copyfrom: Some((from.path.clone(), from.info.filenode)),
            linknode: TWOS_CSID,
        },
    };

    check_roundtrip(&ctx, REPO_ZERO, reader, &writer, payload).await?;

    Ok(())
}
#[mononoke::fbinit_test]
async fn test_repo_ids(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);

    let shard = build_shard()?;
    let (reader, writer) = build_reader_writer(vec1![shard]);
    let reader = Arc::new(reader);

    let payload = root_first_filenode();

    writer
        .insert_filenodes(&ctx, REPO_ZERO, vec![payload.clone()], false)
        .await?
        .do_not_handle_disabled_filenodes()?;

    assert_filenode(
        &ctx,
        reader.clone(),
        &payload.path,
        payload.info.filenode,
        REPO_ZERO,
        payload.info.clone(),
    )
    .await?;

    assert_no_filenode(&ctx, reader, &payload.path, payload.info.filenode, REPO_ONE).await?;

    Ok(())
}

mononoke_queries! {
    write DeleteCopyInfo() {
        none,
        "DELETE FROM fixedcopyinfo"
    }

    write DeletePaths() {
        none,
        "DELETE FROM paths"
    }
}

/// Keeps track of how many read and write queries ran through a Sqlite connection
#[derive(Clone, Debug)]
struct QueryCounter {
    _label: String,
    reads: Arc<AtomicI64>,
    writes: Arc<AtomicI64>,
}

impl QueryCounter {
    fn new(label: &str) -> Self {
        Self {
            _label: label.to_string(),
            reads: Arc::new(AtomicI64::new(0)),
            writes: Arc::new(AtomicI64::new(0)),
        }
    }

    fn reads(&self) -> i64 {
        self.reads.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn writes(&self) -> i64 {
        self.writes.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn values(&self) -> (i64, i64) {
        (self.reads(), self.writes())
    }
}

#[async_trait]
impl SqliteCallbacks for QueryCounter {
    async fn query_start(&self, query_type: SqliteQueryType) -> Result<(), Error> {
        match query_type {
            SqliteQueryType::Read => {
                let _ = self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            SqliteQueryType::Write => {
                let _ = self
                    .writes
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            _ => (),
        }
        Ok(())
    }
}

fn setup_fallback_test() -> Result<(
    Arc<AtomicI64>,
    // master and replica connections
    (Connection, Connection),
    // master and replica query counters
    (QueryCounter, QueryCounter),
)> {
    // Create a shared timestamp that can be updated
    let last_write_time = Arc::new(AtomicI64::new(Timestamp::now().timestamp_nanos()));

    // Create an HLC provider that reads from the shared timestamp
    let replica_hlc_provider: Arc<Box<SqliteHlcProvider>> = {
        let time_ref = last_write_time.clone();
        Arc::new(Box::new(move || {
            time_ref.load(std::sync::atomic::Ordering::SeqCst)
        }))
    };

    println!("Building master");
    let master_counter = QueryCounter::new("master");
    let master = build_shard_with_hlc_provider(
        replica_hlc_provider.clone(),
        Box::new(master_counter.clone()),
    )?;

    println!("Building replica");
    let replica_counter = QueryCounter::new("replica");
    let replica = build_shard_with_hlc_provider(
        replica_hlc_provider.clone(),
        Box::new(replica_counter.clone()),
    )?;

    Ok((
        last_write_time,
        (master, replica),
        (master_counter, replica_counter),
    ))
}

#[mononoke::fbinit_test]
async fn test_primary_fallback_without_consistent_reads(fb: FacebookInit) -> Result<()> {
    override_just_knobs(JustKnobsInMemory::new(HashMap::from([(
        "scm/mononoke:retry_query_from_replica_with_consistency_check".to_owned(),
        KnobVal::Bool(false),
    )])));
    let ctx = CoreContext::test_mock(fb);
    let master = build_shard()?;
    let replica = build_shard()?;
    let (_, master_writer) = build_reader_writer(vec1![master.clone()]);
    let (_, replica_writer) = build_reader_writer(vec1![replica.clone()]);
    let reader = Arc::new(FilenodesReader::new(vec1![replica.clone()], vec1![master])?);
    let records = vec![
        root_first_filenode(),
        copied_from_filenode(),
        copied_filenode(),
    ];
    do_add_filenodes(&ctx, &master_writer, records.clone(), REPO_ZERO).await?;
    for record in &records {
        assert_filenode(
            &ctx,
            reader.clone(),
            &record.path,
            record.info.filenode,
            REPO_ZERO,
            record.info.clone(),
        )
        .await?;
    }

    do_add_filenodes(&ctx, &replica_writer, records, REPO_ZERO).await?;
    DeleteCopyInfo::query(&replica, ctx.sql_query_telemetry()).await?;
    let copied = copied_filenode();
    assert_filenode(
        &ctx,
        reader.clone(),
        &copied.path,
        copied.info.filenode,
        REPO_ZERO,
        copied.info.clone(),
    )
    .await?;

    do_add_filenode(&ctx, &replica_writer, copied.clone(), REPO_ZERO).await?;
    DeletePaths::query(&replica, ctx.sql_query_telemetry()).await?;
    assert_filenode(
        &ctx,
        reader,
        &copied.path,
        copied.info.filenode,
        REPO_ZERO,
        copied.info,
    )
    .await?;
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_fallback_on_missing_copy_info(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);

    let (last_write_time, (master, replica), (master_counter, replica_counter)) =
        setup_fallback_test()?;

    // Populate both master and replica with the same filenodes (to simulate replication).
    FilenodesWriter::new(
        SQLITE_INSERT_CHUNK_SIZE,
        vec1![master.clone()],
        vec1![master.clone()],
    )
    .insert_filenodes(
        &ctx,
        REPO_ZERO,
        vec![copied_from_filenode(), copied_filenode()],
        false,
    )
    .await?
    .do_not_handle_disabled_filenodes()?;

    // Spawn a separate task to write to replica, introducing an artificial
    // replication lag of ~10ms.
    let replica_write_future = {
        cloned!(ctx, replica, last_write_time);
        async move {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            println!("Writing to replica");
            FilenodesWriter::new(
                SQLITE_INSERT_CHUNK_SIZE,
                vec1![replica.clone()],
                vec1![replica],
            )
            .insert_filenodes(
                &ctx,
                REPO_ZERO,
                vec![copied_from_filenode(), copied_filenode()],
                false,
            )
            .await?
            .do_not_handle_disabled_filenodes()?;

            // Update the timestamp after writing to replica
            last_write_time.store(
                Timestamp::now().timestamp_nanos(),
                std::sync::atomic::Ordering::SeqCst,
            );

            Ok::<_, Error>(())
        }
    };

    // Spawn the future as a separate task (don't await it)
    tokio::spawn(replica_write_future);

    println!("Reading filenodes");
    let reader = Arc::new(FilenodesReader::new(vec1![replica], vec1![master])?);
    let prepared = copied_filenode();
    let res = assert_filenode(
        &ctx,
        reader,
        &prepared.path,
        prepared.info.filenode,
        REPO_ZERO,
        prepared.info.clone(),
    )
    .await;

    println!("master counter: {0:?}", master_counter.values());
    println!("replica counter: {0:?}", replica_counter.values());

    assert_eq!(
        master_counter.values(),
        // No need to do extra reads from master
        (1, 3),
        "unexpected number of reads/writes to master"
    );
    // Because of the retries with exponential backoff, the writes to the
    // replica always finish before the read_with_consistency query finishes.
    assert_eq!(
        replica_counter.writes(),
        3,
        "unexpected number of writes to replica"
    );
    assert!(
        replica_counter.reads() > 1,
        "unexpected number of reads to replica"
    );

    res?;
    Ok(())
}

/// Tests that if HLC is not provided, `query_with_consistency` falls back to
/// the primary
#[mononoke::fbinit_test]
async fn test_fallback_on_missing_copy_info_no_hlc(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);

    println!("Building master");
    let master_counter = QueryCounter::new("master");
    let master = build_shard_with_callbacks(Box::new(master_counter.clone()))?;

    println!("Building replica");
    let replica_counter = QueryCounter::new("replica");
    let replica = build_shard_with_callbacks(Box::new(replica_counter.clone()))?;

    println!("Writing to master");
    // Populate only master. The query to the replica will fail with MissingHLC,
    // so the query should fallback to master.
    FilenodesWriter::new(
        SQLITE_INSERT_CHUNK_SIZE,
        vec1![master.clone()],
        vec1![master.clone()],
    )
    .insert_filenodes(
        &ctx,
        REPO_ZERO,
        vec![copied_from_filenode(), copied_filenode()],
        false,
    )
    .await?
    .do_not_handle_disabled_filenodes()?;

    println!("Reading filenodes");
    let reader = Arc::new(FilenodesReader::new(vec1![replica], vec1![master])?);
    let prepared = copied_filenode();

    // Intentionally ignoring the result, because it will be an error:
    // `Error: Filenodes internal error: path is not found: PathHashBytes(..)`
    // This error will be from the `SelectPaths` run by the reader after the
    // `SelectFilenode`, which will success since it will fallback to master.
    let _res = assert_filenode(
        &ctx,
        reader,
        &prepared.path,
        prepared.info.filenode,
        REPO_ZERO,
        prepared.info.clone(),
    )
    .await;

    println!("master counter: {0:?}", master_counter.values());
    println!("replica counter: {0:?}", replica_counter.values());

    assert_eq!(
        master_counter.values(),
        // Every filenode write should run one `SelectAllPaths` read query.
        // The other read should be from the fallback.
        // The other read is the `SelectFilenode` query, which is a fallback
        // from the replica
        // Writes queries are InsertPaths, InsertFilenodes,InsertFixedcopyinfo.
        (2, 3),
        "unexpected number of reads/writes to master"
    );

    assert_eq!(
        replica_counter.reads(),
        // One if `SelectFilenode` query_with_consistency, which falls back to
        // master. The other is `SelectPaths` with normal `query` method and is
        // always expected.
        2,
        "unexpected number of reads to replica"
    );
    assert_eq!(
        replica_counter.writes(),
        0, // We didn't write anything to the replica
        "unexpected number of writes to replica"
    );

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_fallback_on_missing_paths(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);

    let (last_write_time, (master, replica), (master_counter, replica_counter)) =
        setup_fallback_test()?;

    // Populate both master and replica with the same filenodes (to simulate replication).
    FilenodesWriter::new(
        SQLITE_INSERT_CHUNK_SIZE,
        vec1![master.clone()],
        vec1![master.clone()],
    )
    .insert_filenodes(
        &ctx,
        REPO_ZERO,
        vec![copied_from_filenode(), copied_filenode()],
        false,
    )
    .await?
    .do_not_handle_disabled_filenodes()?;

    // Spawn a separate task to write to replica, introducing an artificial
    // replication lag of ~10ms.
    let replica_write_future = {
        cloned!(ctx, replica, last_write_time);
        async move {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            FilenodesWriter::new(
                SQLITE_INSERT_CHUNK_SIZE,
                vec1![replica.clone()],
                vec1![replica.clone()],
            )
            .insert_filenodes(
                &ctx,
                REPO_ZERO,
                vec![copied_from_filenode(), copied_filenode()],
                false,
            )
            .await?
            .do_not_handle_disabled_filenodes()?;

            // Update the timestamp after writing to replica
            last_write_time.store(
                Timestamp::now().timestamp_nanos(),
                std::sync::atomic::Ordering::SeqCst,
            );

            Ok::<_, Error>(())
        }
    };

    // Spawn the future as a separate task
    tokio::spawn(replica_write_future);

    let reader = Arc::new(FilenodesReader::new(vec1![replica], vec1![master])?);
    let prepared = copied_filenode();
    let res = assert_filenode(
        &ctx,
        reader,
        &prepared.path,
        prepared.info.filenode,
        REPO_ZERO,
        prepared.info.clone(),
    )
    .await;

    assert_eq!(
        master_counter.values(),
        // No need to do extra reads from master
        (1, 3),
        "unexpected number of reads/writes to master"
    );
    // Because of the retries with exponential backoff, the writes to the
    // replica always finish before the read_with_consistency query finishes.
    assert_eq!(
        replica_counter.writes(),
        3,
        "unexpected number of writes to replica"
    );
    assert!(
        replica_counter.reads() > 1,
        "unexpected number of reads to replica"
    );

    res?;
    Ok(())
}

fn root_first_filenode() -> PreparedFilenode {
    PreparedFilenode {
        path: RepoPath::root(),
        info: FilenodeInfo {
            filenode: ONES_FNID,
            p1: None,
            p2: None,
            copyfrom: None,
            linknode: ONES_CSID,
        },
    }
}

fn root_second_filenode() -> PreparedFilenode {
    PreparedFilenode {
        path: RepoPath::root(),
        info: FilenodeInfo {
            filenode: TWOS_FNID,
            p1: Some(ONES_FNID),
            p2: None,
            copyfrom: None,
            linknode: TWOS_CSID,
        },
    }
}

fn root_merge_filenode() -> PreparedFilenode {
    PreparedFilenode {
        path: RepoPath::root(),
        info: FilenodeInfo {
            filenode: THREES_FNID,
            p1: Some(ONES_FNID),
            p2: Some(TWOS_FNID),
            copyfrom: None,
            linknode: THREES_CSID,
        },
    }
}

fn dir_a_first_filenode() -> PreparedFilenode {
    PreparedFilenode {
        path: RepoPath::dir("a").unwrap(),
        info: FilenodeInfo {
            filenode: ONES_FNID,
            p1: None,
            p2: None,
            copyfrom: None,
            linknode: ONES_CSID,
        },
    }
}

fn file_a_first_filenode() -> PreparedFilenode {
    PreparedFilenode {
        path: RepoPath::file("a").unwrap(),
        info: FilenodeInfo {
            filenode: ONES_FNID,
            p1: None,
            p2: None,
            copyfrom: None,
            linknode: ONES_CSID,
        },
    }
}

fn file_b_first_filenode() -> PreparedFilenode {
    PreparedFilenode {
        path: RepoPath::file("b").unwrap(),
        info: FilenodeInfo {
            filenode: TWOS_FNID,
            p1: None,
            p2: None,
            copyfrom: None,
            linknode: TWOS_CSID,
        },
    }
}

fn copied_from_filenode() -> PreparedFilenode {
    PreparedFilenode {
        path: RepoPath::file("copiedfrom").unwrap(),
        info: FilenodeInfo {
            filenode: ONES_FNID,
            p1: None,
            p2: None,
            copyfrom: None,
            linknode: TWOS_CSID,
        },
    }
}

fn copied_filenode() -> PreparedFilenode {
    PreparedFilenode {
        path: RepoPath::file("copiedto").unwrap(),
        info: FilenodeInfo {
            filenode: TWOS_FNID,
            p1: None,
            p2: None,
            copyfrom: Some((RepoPath::file("copiedfrom").unwrap(), ONES_FNID)),
            linknode: TWOS_CSID,
        },
    }
}

async fn do_add_filenodes(
    ctx: &CoreContext,
    writer: &FilenodesWriter,
    to_insert: Vec<PreparedFilenode>,
    repo_id: RepositoryId,
) -> Result<(), Error> {
    writer
        .insert_filenodes(ctx, repo_id, to_insert, false)
        .await?
        .do_not_handle_disabled_filenodes()?;
    Ok(())
}

async fn do_add_filenode(
    ctx: &CoreContext,
    writer: &FilenodesWriter,
    node: PreparedFilenode,
    repo_id: RepositoryId,
) -> Result<(), Error> {
    do_add_filenodes(ctx, writer, vec![node], repo_id).await?;
    Ok(())
}

async fn assert_no_filenode(
    ctx: &CoreContext,
    reader: Arc<FilenodesReader>,
    path: &RepoPath,
    hash: HgFileNodeId,
    repo_id: RepositoryId,
) -> Result<(), Error> {
    let res = reader.get_filenode(ctx, repo_id, path, hash).await?;
    let res = res.do_not_handle_disabled_filenodes()?;
    assert!(res.is_none());
    Ok(())
}

async fn assert_filenode(
    ctx: &CoreContext,
    reader: Arc<FilenodesReader>,
    path: &RepoPath,
    hash: HgFileNodeId,
    repo_id: RepositoryId,
    expected: FilenodeInfo,
) -> Result<(), Error> {
    let res = reader
        .get_filenode(ctx, repo_id, path, hash)
        .await?
        .do_not_handle_disabled_filenodes()?
        .ok_or_else(|| format_err!("not found: {hash}"))?;
    assert_eq!(res, expected);
    Ok(())
}

async fn assert_all_filenodes(
    ctx: &CoreContext,
    reader: Arc<FilenodesReader>,
    path: &RepoPath,
    repo_id: RepositoryId,
    expected: &Vec<FilenodeInfo>,
    limit: Option<u64>,
) -> Result<(), Error> {
    let res = reader
        .get_all_filenodes_for_path(ctx, repo_id, path, limit)
        .await?;
    match res.do_not_handle_disabled_filenodes()? {
        FilenodeRange::Filenodes(ref filenodes) => assert_eq!(filenodes, expected),
        FilenodeRange::TooBig => panic!("Unexpected FilenodeRange::TooBig"),
    }
    Ok(())
}

macro_rules! filenodes_tests {
    ($test_suite_name:ident, $create_db:ident, $enable_caching:ident) => {
        mod $test_suite_name {
            use fbinit::FacebookInit;

            use super::*;

            #[mononoke::fbinit_test]
            async fn test_simple_filenode_insert_and_get(fb: FacebookInit) -> Result<(), Error> {
                let ctx = CoreContext::test_mock(fb);
                let (mut reader, writer) = build_reader_writer($create_db()?);
                $enable_caching(&mut reader);
                let reader = Arc::new(reader);

                do_add_filenode(&ctx, &writer, root_first_filenode(), REPO_ZERO).await?;

                assert_filenode(
                    &ctx,
                    reader.clone(),
                    &RepoPath::root(),
                    ONES_FNID,
                    REPO_ZERO,
                    root_first_filenode().info,
                )
                .await?;

                assert_no_filenode(
                    &ctx,
                    reader.clone(),
                    &RepoPath::root(),
                    TWOS_FNID,
                    REPO_ZERO,
                )
                .await?;

                assert_no_filenode(&ctx, reader, &RepoPath::root(), ONES_FNID, REPO_ONE).await?;

                Ok(())
            }

            #[mononoke::fbinit_test]
            async fn test_insert_identical_in_batch(fb: FacebookInit) -> Result<(), Error> {
                let ctx = CoreContext::test_mock(fb);
                let (_reader, writer) = build_reader_writer($create_db()?);
                do_add_filenodes(
                    &ctx,
                    &writer,
                    vec![root_first_filenode(), root_first_filenode()],
                    REPO_ZERO,
                )
                .await?;
                Ok(())
            }

            #[mononoke::fbinit_test]
            async fn test_filenode_insert_twice(fb: FacebookInit) -> Result<(), Error> {
                let ctx = CoreContext::test_mock(fb);
                let (_reader, writer) = build_reader_writer($create_db()?);
                do_add_filenode(&ctx, &writer, root_first_filenode(), REPO_ZERO).await?;
                do_add_filenode(&ctx, &writer, root_first_filenode(), REPO_ZERO).await?;
                Ok(())
            }

            #[mononoke::fbinit_test]
            async fn test_insert_filenode_with_parent(fb: FacebookInit) -> Result<(), Error> {
                let ctx = CoreContext::test_mock(fb);
                let (mut reader, writer) = build_reader_writer($create_db()?);
                $enable_caching(&mut reader);
                let reader = Arc::new(reader);
                do_add_filenode(&ctx, &writer, root_first_filenode(), REPO_ZERO).await?;
                do_add_filenode(&ctx, &writer, root_second_filenode(), REPO_ZERO).await?;
                assert_filenode(
                    &ctx,
                    reader.clone(),
                    &RepoPath::root(),
                    ONES_FNID,
                    REPO_ZERO,
                    root_first_filenode().info,
                )
                .await?;
                assert_filenode(
                    &ctx,
                    reader,
                    &RepoPath::root(),
                    TWOS_FNID,
                    REPO_ZERO,
                    root_second_filenode().info,
                )
                .await?;
                Ok(())
            }

            #[mononoke::fbinit_test]
            async fn test_insert_root_filenode_with_two_parents(
                fb: FacebookInit,
            ) -> Result<(), Error> {
                let ctx = CoreContext::test_mock(fb);
                let (mut reader, writer) = build_reader_writer($create_db()?);
                $enable_caching(&mut reader);
                let reader = Arc::new(reader);
                do_add_filenode(&ctx, &writer, root_first_filenode(), REPO_ZERO).await?;
                do_add_filenode(&ctx, &writer, root_second_filenode(), REPO_ZERO).await?;
                do_add_filenode(&ctx, &writer, root_merge_filenode(), REPO_ZERO).await?;
                assert_filenode(
                    &ctx,
                    reader,
                    &RepoPath::root(),
                    THREES_FNID,
                    REPO_ZERO,
                    root_merge_filenode().info,
                )
                .await?;
                Ok(())
            }

            #[mononoke::fbinit_test]
            async fn test_insert_file_filenode(fb: FacebookInit) -> Result<(), Error> {
                let ctx = CoreContext::test_mock(fb);
                let (mut reader, writer) = build_reader_writer($create_db()?);
                $enable_caching(&mut reader);
                let reader = Arc::new(reader);
                do_add_filenode(&ctx, &writer, file_a_first_filenode(), REPO_ZERO).await?;
                do_add_filenode(&ctx, &writer, file_b_first_filenode(), REPO_ZERO).await?;

                assert_no_filenode(
                    &ctx,
                    reader.clone(),
                    &RepoPath::file("non-existent").unwrap(),
                    ONES_FNID,
                    REPO_ZERO,
                )
                .await?;
                assert_filenode(
                    &ctx,
                    reader.clone(),
                    &RepoPath::file("a").unwrap(),
                    ONES_FNID,
                    REPO_ZERO,
                    file_a_first_filenode().info,
                )
                .await?;
                assert_filenode(
                    &ctx,
                    reader,
                    &RepoPath::file("b").unwrap(),
                    TWOS_FNID,
                    REPO_ZERO,
                    file_b_first_filenode().info,
                )
                .await?;
                Ok(())
            }

            #[mononoke::fbinit_test]
            async fn test_insert_different_repo(fb: FacebookInit) -> Result<(), Error> {
                let ctx = CoreContext::test_mock(fb);
                let (mut reader, writer) = build_reader_writer($create_db()?);
                $enable_caching(&mut reader);
                let reader = Arc::new(reader);
                do_add_filenode(&ctx, &writer, root_first_filenode(), REPO_ZERO).await?;
                do_add_filenode(&ctx, &writer, root_second_filenode(), REPO_ONE).await?;

                assert_filenode(
                    &ctx,
                    reader.clone(),
                    &RepoPath::root(),
                    ONES_FNID,
                    REPO_ZERO,
                    root_first_filenode().info,
                )
                .await?;

                assert_no_filenode(&ctx, reader.clone(), &RepoPath::root(), ONES_FNID, REPO_ONE)
                    .await?;

                assert_filenode(
                    &ctx,
                    reader,
                    &RepoPath::root(),
                    TWOS_FNID,
                    REPO_ONE,
                    root_second_filenode().info,
                )
                .await?;
                Ok(())
            }

            #[mononoke::fbinit_test]
            async fn test_insert_parent_and_child_in_same_batch(
                fb: FacebookInit,
            ) -> Result<(), Error> {
                let ctx = CoreContext::test_mock(fb);
                let (mut reader, writer) = build_reader_writer($create_db()?);
                $enable_caching(&mut reader);
                let reader = Arc::new(reader);

                do_add_filenodes(
                    &ctx,
                    &writer,
                    vec![root_first_filenode(), root_second_filenode()],
                    REPO_ZERO,
                )
                .await?;

                assert_filenode(
                    &ctx,
                    reader.clone(),
                    &RepoPath::root(),
                    ONES_FNID,
                    REPO_ZERO,
                    root_first_filenode().info,
                )
                .await?;

                assert_filenode(
                    &ctx,
                    reader,
                    &RepoPath::root(),
                    TWOS_FNID,
                    REPO_ZERO,
                    root_second_filenode().info,
                )
                .await?;
                Ok(())
            }

            #[mononoke::fbinit_test]
            async fn insert_copied_file(fb: FacebookInit) -> Result<(), Error> {
                let ctx = CoreContext::test_mock(fb);
                let (mut reader, writer) = build_reader_writer($create_db()?);
                $enable_caching(&mut reader);
                let reader = Arc::new(reader);

                do_add_filenodes(
                    &ctx,
                    &writer,
                    vec![copied_from_filenode(), copied_filenode()],
                    REPO_ZERO,
                )
                .await?;
                assert_filenode(
                    &ctx,
                    reader,
                    &RepoPath::file("copiedto").unwrap(),
                    TWOS_FNID,
                    REPO_ZERO,
                    copied_filenode().info,
                )
                .await?;
                Ok(())
            }

            #[mononoke::fbinit_test]
            async fn insert_same_copied_file(fb: FacebookInit) -> Result<(), Error> {
                let ctx = CoreContext::test_mock(fb);
                let (_reader, writer) = build_reader_writer($create_db()?);

                do_add_filenodes(&ctx, &writer, vec![copied_from_filenode()], REPO_ZERO).await?;
                do_add_filenodes(
                    &ctx,
                    &writer,
                    vec![copied_filenode(), copied_filenode()],
                    REPO_ZERO,
                )
                .await?;
                Ok(())
            }

            #[mononoke::fbinit_test]
            async fn insert_copied_file_to_different_repo(fb: FacebookInit) -> Result<(), Error> {
                let ctx = CoreContext::test_mock(fb);
                let (mut reader, writer) = build_reader_writer($create_db()?);
                $enable_caching(&mut reader);
                let reader = Arc::new(reader);

                let copied = PreparedFilenode {
                    path: RepoPath::file("copiedto").unwrap(),
                    info: FilenodeInfo {
                        filenode: TWOS_FNID,
                        p1: None,
                        p2: None,
                        copyfrom: Some((RepoPath::file("copiedfrom").unwrap(), ONES_FNID)),
                        linknode: TWOS_CSID,
                    },
                };

                let notcopied = PreparedFilenode {
                    path: RepoPath::file("copiedto").unwrap(),
                    info: FilenodeInfo {
                        filenode: TWOS_FNID,
                        p1: None,
                        p2: None,
                        copyfrom: None,
                        linknode: TWOS_CSID,
                    },
                };

                do_add_filenodes(
                    &ctx,
                    &writer,
                    vec![copied_from_filenode(), copied.clone()],
                    REPO_ZERO,
                )
                .await?;

                do_add_filenodes(&ctx, &writer, vec![notcopied.clone()], REPO_ONE).await?;

                assert_filenode(
                    &ctx,
                    reader.clone(),
                    &RepoPath::file("copiedto").unwrap(),
                    TWOS_FNID,
                    REPO_ZERO,
                    copied.info,
                )
                .await?;

                assert_filenode(
                    &ctx,
                    reader,
                    &RepoPath::file("copiedto").unwrap(),
                    TWOS_FNID,
                    REPO_ONE,
                    notcopied.info,
                )
                .await?;
                Ok(())
            }

            #[mononoke::fbinit_test]
            async fn get_all_filenodes_maybe_stale(fb: FacebookInit) -> Result<(), Error> {
                let ctx = CoreContext::test_mock(fb);
                let (mut reader, writer) = build_reader_writer($create_db()?);
                $enable_caching(&mut reader);
                let reader = Arc::new(reader);
                do_add_filenodes(
                    &ctx,
                    &writer,
                    vec![
                        root_first_filenode(),
                        root_second_filenode(),
                        root_merge_filenode(),
                    ],
                    REPO_ZERO,
                )
                .await?;
                do_add_filenodes(
                    &ctx,
                    &writer,
                    vec![file_a_first_filenode(), file_b_first_filenode()],
                    REPO_ZERO,
                )
                .await?;

                let root_filenodes = vec![
                    root_first_filenode().info,
                    root_second_filenode().info,
                    root_merge_filenode().info,
                ];

                assert_all_filenodes(
                    &ctx,
                    reader.clone(),
                    &RepoPath::RootPath,
                    REPO_ZERO,
                    &root_filenodes,
                    None,
                )
                .await?;

                assert_all_filenodes(
                    &ctx,
                    reader.clone(),
                    &RepoPath::file("a").unwrap(),
                    REPO_ZERO,
                    &vec![file_a_first_filenode().info],
                    None,
                )
                .await?;

                assert_all_filenodes(
                    &ctx,
                    reader,
                    &RepoPath::file("b").unwrap(),
                    REPO_ZERO,
                    &vec![file_b_first_filenode().info],
                    None,
                )
                .await?;
                Ok(())
            }

            #[mononoke::fbinit_test]
            async fn get_all_filenodes_maybe_stale_limited(fb: FacebookInit) -> Result<(), Error> {
                let ctx = CoreContext::test_mock(fb);
                let (mut reader, writer) = build_reader_writer($create_db()?);
                $enable_caching(&mut reader);
                let reader = Arc::new(reader);
                do_add_filenodes(
                    &ctx,
                    &writer,
                    vec![
                        root_first_filenode(),
                        root_second_filenode(),
                        root_merge_filenode(),
                    ],
                    REPO_ZERO,
                )
                .await?;
                do_add_filenodes(
                    &ctx,
                    &writer,
                    vec![file_a_first_filenode(), file_b_first_filenode()],
                    REPO_ZERO,
                )
                .await?;

                let root_filenodes = vec![
                    root_first_filenode().info,
                    root_second_filenode().info,
                    root_merge_filenode().info,
                ];

                assert_all_filenodes(
                    &ctx,
                    reader.clone(),
                    &RepoPath::RootPath,
                    REPO_ZERO,
                    &root_filenodes,
                    Some(3),
                )
                .await?;

                let res = reader
                    .clone()
                    .get_all_filenodes_for_path(&ctx, REPO_ZERO, &RepoPath::RootPath, Some(1))
                    .await?;
                let res = res.do_not_handle_disabled_filenodes()?;
                assert_eq!(FilenodeRange::TooBig, res);

                let res = reader
                    .get_all_filenodes_for_path(&ctx, REPO_ZERO, &RepoPath::RootPath, Some(2))
                    .await?;
                let res = res.do_not_handle_disabled_filenodes()?;
                assert_eq!(FilenodeRange::TooBig, res);

                Ok(())
            }

            #[mononoke::fbinit_test]
            async fn test_mixed_path_insert_and_get(fb: FacebookInit) -> Result<(), Error> {
                let ctx = CoreContext::test_mock(fb);
                let (mut reader, writer) = build_reader_writer($create_db()?);
                $enable_caching(&mut reader);
                let reader = Arc::new(reader);

                do_add_filenode(&ctx, &writer, file_a_first_filenode(), REPO_ZERO).await?;
                do_add_filenode(&ctx, &writer, dir_a_first_filenode(), REPO_ZERO).await?;

                assert_filenode(
                    &ctx,
                    reader.clone(),
                    &RepoPath::file("a").unwrap(),
                    ONES_FNID,
                    REPO_ZERO,
                    file_a_first_filenode().info,
                )
                .await?;

                assert_filenode(
                    &ctx,
                    reader.clone(),
                    &RepoPath::dir("a").unwrap(),
                    ONES_FNID,
                    REPO_ZERO,
                    dir_a_first_filenode().info,
                )
                .await?;

                assert_all_filenodes(
                    &ctx,
                    reader.clone(),
                    &RepoPath::file("a").unwrap(),
                    REPO_ZERO,
                    &vec![file_a_first_filenode().info],
                    None,
                )
                .await?;

                assert_all_filenodes(
                    &ctx,
                    reader,
                    &RepoPath::dir("a").unwrap(),
                    REPO_ZERO,
                    &vec![dir_a_first_filenode().info],
                    None,
                )
                .await?;

                Ok(())
            }
        }
    };
}

fn create_unsharded() -> Result<Vec1<Connection>, Error> {
    Ok(vec1![build_shard()?])
}

fn create_sharded() -> Result<Vec1<Connection>, Error> {
    Ok((0..16)
        .map(|_| build_shard())
        .collect::<Result<Vec<_>, Error>>()?
        .try_into()?)
}

fn no_caching(_reader: &mut FilenodesReader) {}

fn with_caching(reader: &mut FilenodesReader) {
    reader.local_cache = LocalCache::new_mock();
}

filenodes_tests!(uncached_unsharded_test, create_unsharded, no_caching);
filenodes_tests!(uncached_sharded_test, create_sharded, no_caching);

filenodes_tests!(cached_unsharded_test, create_unsharded, with_caching);
filenodes_tests!(cached_sharded_test, create_sharded, with_caching);

#[mononoke::fbinit_test]
async fn test_all_filenodes_caching(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);

    let (mut reader, writer) = build_reader_writer(create_unsharded()?);
    with_caching(&mut reader);
    let reader = Arc::new(reader);
    do_add_filenode(&ctx, &writer, file_a_first_filenode(), REPO_ZERO).await?;

    let dir_a_second_filenode = PreparedFilenode {
        path: RepoPath::dir("a").unwrap(),
        info: FilenodeInfo {
            filenode: TWOS_FNID,
            p1: None,
            p2: None,
            copyfrom: None,
            linknode: TWOS_CSID,
        },
    };
    do_add_filenode(&ctx, &writer, dir_a_second_filenode.clone(), REPO_ZERO).await?;

    assert_all_filenodes(
        &ctx,
        reader.clone(),
        &RepoPath::file("a")?,
        REPO_ZERO,
        &vec![file_a_first_filenode().info],
        None,
    )
    .await?;

    assert_all_filenodes(
        &ctx,
        reader,
        &RepoPath::dir("a")?,
        REPO_ZERO,
        &vec![dir_a_second_filenode.info],
        None,
    )
    .await?;

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_point_filenode_caching(fb: FacebookInit) -> Result<(), Error> {
    let ctx = CoreContext::test_mock(fb);

    let (mut reader, writer) = build_reader_writer(create_unsharded()?);
    with_caching(&mut reader);
    let reader = Arc::new(reader);
    do_add_filenode(&ctx, &writer, file_a_first_filenode(), REPO_ZERO).await?;

    let dir_a_second_filenode = PreparedFilenode {
        path: RepoPath::dir("a").unwrap(),
        info: FilenodeInfo {
            filenode: ONES_FNID,
            p1: None,
            p2: None,
            copyfrom: None,
            linknode: TWOS_CSID,
        },
    };
    do_add_filenode(&ctx, &writer, dir_a_second_filenode.clone(), REPO_ZERO).await?;

    assert_filenode(
        &ctx,
        reader.clone(),
        &RepoPath::file("a")?,
        ONES_FNID,
        REPO_ZERO,
        file_a_first_filenode().info,
    )
    .await?;

    assert_filenode(
        &ctx,
        reader,
        &RepoPath::dir("a")?,
        ONES_FNID,
        REPO_ZERO,
        dir_a_second_filenode.info,
    )
    .await?;

    Ok(())
}
