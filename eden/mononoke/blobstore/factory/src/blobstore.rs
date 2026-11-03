/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::num::NonZeroU64;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use anyhow::Error;
use anyhow::bail;
use blobstore::Blobstore;
use blobstore::BlobstoreEnumerableWithUnlink;
use blobstore::DEFAULT_PUT_BEHAVIOUR;
use blobstore::DisabledBlob;
use blobstore::PutBehaviour;
use blobstore_sync_queue::SqlBlobstoreWalBuilder;
use cacheblob::CachelibBlobstoreOptions;
use cached_config::ConfigStore;
use chaosblob::ChaosBlobstore;
use chaosblob::ChaosOptions;
use delayblob::DelayOptions;
use delayblob::DelayedBlobstore;
use fbinit::FacebookInit;
use fileblob::Fileblob;
use futures::future;
use futures::future::BoxFuture;
use futures::future::FutureExt;
use futures_watchdog::WatchdogExt;
use logblob::LogBlob;
#[cfg(fbcode_build)]
use manifoldblob::ManifoldOptions;
use metaconfig_types::BlobConfig;
use metaconfig_types::BlobstoreId;
use metaconfig_types::MultiplexId;
use metaconfig_types::MultiplexedStoreType;
use metaconfig_types::PackConfig;
use metaconfig_types::ShardableRemoteDatabaseConfig;
use metaconfig_types::ShardedDatabaseConfig;
use multiplexedblob::ScrubAction;
use multiplexedblob::ScrubHandler;
use multiplexedblob::ScrubOptions;
use multiplexedblob::SrubWriteOnly;
use multiplexedblob_wal::Scuba as WalScuba;
use multiplexedblob_wal::WalMultiplexedBlobstore;
use multiplexedblob_wal::scrub::WalScrubBlobstore;
use packblob::PackBlob;
use packblob::PackOptions;
use readonlyblob::ReadOnlyBlobstore;
use s3blob::awss3client::AwsS3ClientPool;
use s3blob::store::S3Blob;
use samplingblob::ComponentSamplingHandler;
use samplingblob::SamplingBlobstoreSamplingComponentBlobstore;
use scuba_ext::MononokeScubaSampleBuilder;
use sql_construct::SqlConstructFromShardedDatabaseConfig;
use sql_ext::SqlQueryTelemetry;
use sql_ext::facebook::MysqlOptions;
use sqlblob::CountedSqlblob;
use sqlblob::Sqlblob;
use throttledblob::ThrottleOptions;
use throttledblob::ThrottledBlob;

use crate::ReadOnlyStorage;

#[derive(Clone, Debug)]
pub struct BlobstoreOptions {
    pub chaos_options: ChaosOptions,
    pub delay_options: DelayOptions,
    pub throttle_options: ThrottleOptions,
    #[cfg(fbcode_build)]
    pub manifold_options: ManifoldOptions,
    pub pack_options: PackOptions,
    pub cachelib_options: CachelibBlobstoreOptions,
    pub put_behaviour: PutBehaviour,
    pub scrub_options: Option<ScrubOptions>,
    pub sqlblob_mysql_options: MysqlOptions,
}

impl BlobstoreOptions {
    pub fn new(
        chaos_options: ChaosOptions,
        delay_options: DelayOptions,
        throttle_options: ThrottleOptions,
        #[cfg(fbcode_build)] manifold_options: ManifoldOptions,
        pack_options: PackOptions,
        cachelib_options: CachelibBlobstoreOptions,
        put_behaviour: Option<PutBehaviour>,
        sqlblob_mysql_options: MysqlOptions,
    ) -> Self {
        Self {
            chaos_options,
            delay_options,
            throttle_options,
            #[cfg(fbcode_build)]
            manifold_options,
            pack_options,
            cachelib_options,
            // If not specified, maintain status quo, which is overwrite
            put_behaviour: put_behaviour.unwrap_or(DEFAULT_PUT_BEHAVIOUR),
            // These are added via the builder methods
            scrub_options: None,
            sqlblob_mysql_options,
        }
    }

    #[cfg(fbcode_build)]
    pub fn manifold_blobstore(
        manifold_options: ManifoldOptions,
        put_behaviour: PutBehaviour,
    ) -> Self {
        Self {
            manifold_options,
            put_behaviour,
            chaos_options: Default::default(),
            delay_options: Default::default(),
            throttle_options: Default::default(),
            pack_options: Default::default(),
            cachelib_options: Default::default(),
            scrub_options: None,
            sqlblob_mysql_options: Default::default(),
        }
    }

    pub fn set_scrub_options(&mut self, scrub_options: ScrubOptions) {
        self.scrub_options = Some(scrub_options);
    }

    pub fn with_scrub_action(self, scrub_action: Option<ScrubAction>) -> Self {
        if let Some(scrub_action) = scrub_action {
            let mut scrub_options = self.scrub_options.unwrap_or_default();
            scrub_options.scrub_action = scrub_action;
            Self {
                scrub_options: Some(scrub_options),
                ..self
            }
        } else {
            self
        }
    }

    pub fn with_scrub_grace(self, scrub_grace: Option<u64>) -> Self {
        if let Some(mut scrub_options) = self.scrub_options {
            scrub_options.scrub_grace = scrub_grace.map(Duration::from_secs);
            Self {
                scrub_options: Some(scrub_options),
                ..self
            }
        } else {
            self
        }
    }

    pub fn with_scrub_action_on_missing_write_only(self, scrub_missing: SrubWriteOnly) -> Self {
        if let Some(mut scrub_options) = self.scrub_options {
            scrub_options.scrub_action_on_missing_write_only = scrub_missing;
            Self {
                scrub_options: Some(scrub_options),
                ..self
            }
        } else {
            self
        }
    }

    pub fn with_scrub_queue_peek_bound(self, queue_peek_bound_secs: u64) -> Self {
        if let Some(mut scrub_options) = self.scrub_options {
            scrub_options.queue_peek_bound = Duration::from_secs(queue_peek_bound_secs);
            Self {
                scrub_options: Some(scrub_options),
                ..self
            }
        } else {
            self
        }
    }
}

pub async fn make_sql_blobstore<'a>(
    fb: FacebookInit,
    blobconfig: BlobConfig,
    readonly_storage: ReadOnlyStorage,
    blobstore_options: &'a BlobstoreOptions,
    config_store: &'a ConfigStore,
) -> Result<CountedSqlblob, Error> {
    use BlobConfig::*;
    match blobconfig {
        Sqlite { path } => Sqlblob::with_sqlite_path(
            path.join("blobs"),
            readonly_storage.0,
            blobstore_options.put_behaviour,
            config_store,
        )
        .context("Error while opening state for blob store"),
        Mysql { remote } => {
            let (tier_name, shard_count) = match remote {
                ShardableRemoteDatabaseConfig::Unsharded(config) => (config.db_address, None),
                ShardableRemoteDatabaseConfig::Sharded(config) => {
                    (config.shard_map.clone(), Some(config.shard_num))
                }
            };
            make_sql_blobstore_xdb(
                fb,
                tier_name,
                shard_count,
                blobstore_options,
                readonly_storage,
                blobstore_options.put_behaviour,
                config_store,
            )
            .await
        }
        _ => bail!("Not an SQL blobstore"),
    }
}

// Most users should call `make_sql_blobstore` instead, however its useful to expose this to reduce duplication with benchmark tools.
pub async fn make_sql_blobstore_xdb<'a>(
    fb: FacebookInit,
    tier_name: String,
    shard_count: Option<NonZeroUsize>,
    blobstore_options: &'a BlobstoreOptions,
    readonly_storage: ReadOnlyStorage,
    put_behaviour: PutBehaviour,
    config_store: &'a ConfigStore,
) -> Result<CountedSqlblob, Error> {
    let mysql_options = blobstore_options.sqlblob_mysql_options.clone();
    match shard_count {
        None => {
            Sqlblob::with_mysql_unsharded(
                fb,
                tier_name,
                mysql_options,
                readonly_storage.0,
                put_behaviour,
                config_store,
            )
            .await
        }
        Some(shard_num) => {
            Sqlblob::with_mysql(
                fb,
                tier_name,
                shard_num,
                mysql_options,
                readonly_storage.0,
                put_behaviour,
                config_store,
            )
            .await
        }
    }
}

pub fn make_packblob_wrapper<'a, T>(
    pack_config: Option<PackConfig>,
    blobstore_options: &'a BlobstoreOptions,
    store: T,
) -> Result<PackBlob<T>, Error> {
    // Take the user specified option if provided, otherwise use the config
    let put_format = if let Some(put_format) = blobstore_options.pack_options.override_put_format {
        put_format
    } else {
        pack_config.map(|c| c.put_format).unwrap_or_default()
    };

    Ok(PackBlob::new(store, put_format))
}

/// Construct a PackBlob according to the spec; you are responsible for
/// finding a PackBlob config
pub async fn make_packblob<'a>(
    fb: FacebookInit,
    blobconfig: BlobConfig,
    readonly_storage: ReadOnlyStorage,
    blobstore_options: &'a BlobstoreOptions,
    config_store: &'a ConfigStore,
) -> Result<PackBlob<Arc<dyn Blobstore>>, Error> {
    if let BlobConfig::Pack {
        pack_config,
        blobconfig,
    } = blobconfig
    {
        let store = make_physical_blobstore_unlink_ops(
            fb,
            *blobconfig,
            readonly_storage,
            blobstore_options,
            config_store,
        )
        .watched()
        .await?;

        Ok(make_packblob_wrapper(
            pack_config,
            blobstore_options,
            store,
        )?)
    } else {
        bail!("Not a PackBlob")
    }
}

#[cfg(fbcode_build)]
pub async fn make_manifold_blobstore_enumerable_with_unlink(
    fb: FacebookInit,
    blobconfig: BlobConfig,
    blobstore_options: &BlobstoreOptions,
) -> Result<Arc<dyn BlobstoreEnumerableWithUnlink>, Error> {
    use BlobConfig::*;
    let (bucket, prefix, ttl) = match blobconfig {
        Manifold { bucket, prefix } => (bucket, prefix, None),
        ManifoldWithTtl {
            bucket,
            prefix,
            ttl,
        } => (bucket, prefix, Some(ttl)),
        _ => bail!("Not a Manifold blobstore"),
    };
    crate::facebook::make_manifold_blobstore_enumerable_with_unlink(
        fb,
        &prefix,
        &bucket,
        ttl,
        &blobstore_options.manifold_options,
        blobstore_options.put_behaviour,
    )
}

#[cfg(not(fbcode_build))]
pub async fn make_manifold_blobstore_enumerable_with_unlink(
    _fb: FacebookInit,
    _blobconfig: BlobConfig,
    _blobstore_options: &BlobstoreOptions,
) -> Result<Arc<dyn BlobstoreEnumerableWithUnlink>, Error> {
    unimplemented!("This is implemented only for fbcode_build")
}

async fn make_files_blobstore(
    blobconfig: BlobConfig,
    blobstore_options: &BlobstoreOptions,
) -> Result<Fileblob, Error> {
    if let BlobConfig::Files { path } = blobconfig {
        Fileblob::create(path.join("blobs"), blobstore_options.put_behaviour)
            .context("Error while opening state for blob store")
    } else {
        bail!("Not a file blobstore")
    }
}

async fn make_physical_blobstore_unlink_ops<'a>(
    fb: FacebookInit,
    blobconfig: BlobConfig,
    readonly_storage: ReadOnlyStorage,
    blobstore_options: &'a BlobstoreOptions,
    config_store: &'a ConfigStore,
) -> Result<Arc<dyn Blobstore>, Error> {
    use BlobConfig::*;
    match blobconfig {
        Sqlite { .. } | Mysql { .. } => make_sql_blobstore(
            fb,
            blobconfig,
            readonly_storage,
            blobstore_options,
            config_store,
        )
        .watched()
        .await
        .map(|store| Arc::new(store) as Arc<dyn Blobstore>),
        Manifold { .. } | ManifoldWithTtl { .. } => {
            make_manifold_blobstore_enumerable_with_unlink(fb, blobconfig, blobstore_options)
                .watched()
                .await
                .map(|store| Arc::new(store) as Arc<dyn Blobstore>)
        }
        Files { .. } => make_files_blobstore(blobconfig, blobstore_options)
            .await
            .map(|store| Arc::new(store) as Arc<dyn Blobstore>),
        AwsS3 {
            bucket,
            region,
            num_concurrent_operations,
            ..
        } => {
            let client_backend = AwsS3ClientPool::new(region, num_concurrent_operations).await?;

            S3Blob::new(bucket, client_backend, blobstore_options.put_behaviour)
                .watched()
                .await
                .context("Error while opening state for blob store")
                .map(|store| Arc::new(store) as Arc<dyn Blobstore>)
        }
        _ => bail!("Not a physical blobstore"),
    }
}

// Constructs the BlobstoreEnumerableWithUnlink store implementations
// for blobstores. If the blobstore is a wrapper blobstore, the inner
// physical blobstore construction is delegated to another function
// and the result is wrapped up in this function.
pub async fn make_blobstore_enumerable_with_unlink<'a>(
    fb: FacebookInit,
    blobconfig: BlobConfig,
    blobstore_options: &'a BlobstoreOptions,
) -> Result<Arc<dyn BlobstoreEnumerableWithUnlink>, Error> {
    use BlobConfig::*;
    match blobconfig {
        Pack {
            pack_config,
            blobconfig,
        } => {
            let store = raw_blobstore_enumerable_with_unlink(fb, *blobconfig, blobstore_options)
                .watched()
                .await?;
            let pack_store = make_packblob_wrapper(pack_config, blobstore_options, store)?;
            Ok(Arc::new(pack_store) as Arc<dyn BlobstoreEnumerableWithUnlink>)
        }
        _ => raw_blobstore_enumerable_with_unlink(fb, blobconfig, blobstore_options).await,
    }
}

// Constructs the raw BlobstoreEnumerableWithUnlink store implementations for low level
// blobstore access. The blobstore created is NOT a wrapper (e.g. PackBlob)
pub async fn raw_blobstore_enumerable_with_unlink<'a>(
    fb: FacebookInit,
    blobconfig: BlobConfig,
    blobstore_options: &'a BlobstoreOptions,
) -> Result<Arc<dyn BlobstoreEnumerableWithUnlink>, Error> {
    use BlobConfig::*;
    match blobconfig {
        Manifold { .. } | ManifoldWithTtl { .. } => {
            make_manifold_blobstore_enumerable_with_unlink(fb, blobconfig, blobstore_options)
                .watched()
                .await
        }
        Files { .. } => make_files_blobstore(blobconfig, blobstore_options)
            .await
            .map(|store| Arc::new(store) as Arc<dyn BlobstoreEnumerableWithUnlink>),
        _ => bail!("Not a physical blobstore that supports unlink + keysource"),
    }
}

/// Construct a blobstore according to the specification. The multiplexed blobstore
/// needs an SQL DB for its queue, as does the MySQL blobstore.
/// If `throttling.read_qps` or `throttling.write_qps` are Some then ThrottledBlob will be used to limit
/// QPS to the underlying blobstore
pub fn make_blobstore<'a>(
    fb: FacebookInit,
    blobconfig: BlobConfig,
    mysql_options: &'a MysqlOptions,
    readonly_storage: ReadOnlyStorage,
    blobstore_options: &'a BlobstoreOptions,
    config_store: &'a ConfigStore,
    scrub_handler: &'a Arc<dyn ScrubHandler>,
    component_sampler: Option<&'a Arc<dyn ComponentSamplingHandler>>,
    blobstore_id: Option<BlobstoreId>,
) -> BoxFuture<'a, Result<Arc<dyn Blobstore>, Error>> {
    // NOTE: This needs to return a BoxFuture because it recurses.
    async move {
        use BlobConfig::*;

        let mut needs_wrappers = true;
        let store = match blobconfig {
            // Physical blobstores
            Sqlite { .. }
            | Mysql { .. }
            | Manifold { .. }
            | ManifoldWithTtl { .. }
            | Files { .. } => make_physical_blobstore_unlink_ops(
                fb,
                blobconfig,
                readonly_storage,
                blobstore_options,
                config_store,
            )
            .await
            .map(|store| Arc::new(store) as Arc<dyn Blobstore>)?,
            AwsS3 {
                bucket,
                region,
                num_concurrent_operations,
                ..
            } => {
                let client_backend =
                    AwsS3ClientPool::new(region, num_concurrent_operations).await?;

                S3Blob::new(bucket, client_backend, blobstore_options.put_behaviour)
                    .watched()
                    .await
                    .context("Error while opening state for blob store")
                    .map(|store| Arc::new(store) as Arc<dyn Blobstore>)?
            }

            // Special case
            Disabled => {
                Arc::new(DisabledBlob::new("Disabled by configuration")) as Arc<dyn Blobstore>
            }

            // Wrapper blobstores
            MultiplexedWal {
                multiplex_id,
                blobstores,
                write_quorum,
                queue_db,
                inner_blobstores_scuba_table,
                multiplex_scuba_table,
                scuba_sample_rate,
            } => {
                needs_wrappers = false;
                make_multiplexed_wal(
                    fb,
                    multiplex_id,
                    queue_db,
                    inner_blobstores_scuba_table,
                    multiplex_scuba_table,
                    scuba_sample_rate,
                    blobstores,
                    write_quorum,
                    mysql_options,
                    readonly_storage,
                    blobstore_options,
                    config_store,
                    scrub_handler,
                    component_sampler,
                )
                .watched()
                .await?
            }
            Logging {
                blobconfig,
                scuba_table,
                scuba_sample_rate,
            } => {
                needs_wrappers = false;
                let store = make_blobstore(
                    fb,
                    *blobconfig,
                    mysql_options,
                    readonly_storage,
                    blobstore_options,
                    config_store,
                    scrub_handler,
                    component_sampler,
                    None,
                )
                .watched()
                .await?;

                let scuba = scuba_table
                    .map_or(Ok(MononokeScubaSampleBuilder::with_discard()), |table| {
                        MononokeScubaSampleBuilder::new(fb, &table)
                    })?;
                Arc::new(LogBlob::new(store, scuba, scuba_sample_rate)) as Arc<dyn Blobstore>
            }
            Pack { .. } => {
                // NB packblob does not apply the wrappers internally
                make_packblob(
                    fb,
                    blobconfig,
                    readonly_storage,
                    blobstore_options,
                    config_store,
                )
                .watched()
                .await
                .map(|store| Arc::new(store) as Arc<dyn Blobstore>)?
            }
        };

        let store = if needs_wrappers {
            let store = if let Some(component_sampler) = component_sampler {
                Arc::new(SamplingBlobstoreSamplingComponentBlobstore::new(
                    store,
                    blobstore_id,
                    component_sampler.clone(),
                )) as Arc<dyn Blobstore>
            } else {
                store
            };

            let store = if readonly_storage.0 {
                Arc::new(ReadOnlyBlobstore::new(store)) as Arc<dyn Blobstore>
            } else {
                store
            };

            let store = if blobstore_options.throttle_options.has_throttle() {
                Arc::new(ThrottledBlob::new(
                    store,
                    blobstore_options.throttle_options,
                )) as Arc<dyn Blobstore>
            } else {
                store
            };

            let store = if blobstore_options.chaos_options.has_chaos() {
                Arc::new(ChaosBlobstore::new(store, blobstore_options.chaos_options))
                    as Arc<dyn Blobstore>
            } else {
                store
            };

            if blobstore_options.delay_options.has_delay() {
                Arc::new(DelayedBlobstore::from_options(
                    store,
                    blobstore_options.delay_options,
                )) as Arc<dyn Blobstore>
            } else {
                store
            }
        } else {
            // Already applied the wrappers inside the store
            store
        };

        // NOTE: Do not add wrappers here that should only be added once per repository, since this
        // function will get called recursively for each member of a Multiplex! For those, use
        // RepoBlobstore::new instead.

        Ok(store)
    }
    .boxed()
}

async fn make_multiplexed_wal<'a>(
    fb: FacebookInit,
    multiplex_id: MultiplexId,
    queue_db: ShardedDatabaseConfig,
    inner_blobstores_scuba_table: Option<String>,
    multiplex_scuba_table: Option<String>,
    scuba_sample_rate: NonZeroU64,
    inner_config: Vec<(BlobstoreId, MultiplexedStoreType, BlobConfig)>,
    write_quorum: usize,
    mysql_options: &'a MysqlOptions,
    readonly_storage: ReadOnlyStorage,
    blobstore_options: &'a BlobstoreOptions,
    config_store: &'a ConfigStore,
    scrub_handler: &'a Arc<dyn ScrubHandler>,
    component_sampler: Option<&'a Arc<dyn ComponentSamplingHandler>>,
) -> Result<Arc<dyn Blobstore>, Error> {
    let (normal_components, write_only_components) = setup_inner_blobstores(
        fb,
        inner_config,
        mysql_options,
        blobstore_options,
        config_store,
        scrub_handler,
        component_sampler,
    )
    .await?;

    let scuba = WalScuba::new_from_raw(
        fb,
        inner_blobstores_scuba_table,
        multiplex_scuba_table,
        scuba_sample_rate,
    )?;

    let sql_blob_wal = SqlBlobstoreWalBuilder::with_sharded_database_config(
        fb,
        &queue_db,
        mysql_options,
        readonly_storage.0,
    )?
    .build(SqlQueryTelemetry::new(fb, Default::default(), None));
    let wal_queue = Arc::new(sql_blob_wal);

    let blobstore = match &blobstore_options.scrub_options {
        Some(scrub_options) => {
            Arc::new(WalScrubBlobstore::new(
                multiplex_id,
                wal_queue,
                normal_components,
                write_only_components,
                write_quorum,
                None, // use default timeouts
                scuba,
                scrub_options.clone(),
                scrub_handler.clone(),
            )?) as Arc<dyn Blobstore>
        }
        None => Arc::new(WalMultiplexedBlobstore::new(
            multiplex_id,
            wal_queue,
            normal_components,
            write_only_components,
            write_quorum,
            None, // use default timeouts
            scuba,
        )?) as Arc<dyn Blobstore>,
    };

    Ok(blobstore)
}

type InnerBlobstore = (BlobstoreId, Arc<dyn Blobstore>);

async fn setup_inner_blobstores<'a>(
    fb: FacebookInit,
    inner_config: Vec<(BlobstoreId, MultiplexedStoreType, BlobConfig)>,
    mysql_options: &'a MysqlOptions,
    blobstore_options: &'a BlobstoreOptions,
    config_store: &'a ConfigStore,
    scrub_handler: &'a Arc<dyn ScrubHandler>,
    component_sampler: Option<&'a Arc<dyn ComponentSamplingHandler>>,
) -> Result<(Vec<InnerBlobstore>, Vec<InnerBlobstore>), Error> {
    let component_readonly = blobstore_options
        .scrub_options
        .as_ref()
        .map_or(ReadOnlyStorage(false), |v| {
            ReadOnlyStorage(v.scrub_action != ScrubAction::Repair)
        });

    let mut applied_chaos = false;
    let components = future::try_join_all(inner_config.into_iter().map({
        move |(blobstoreid, store_type, config)| {
            let mut blobstore_options = blobstore_options.clone();

            if blobstore_options.chaos_options.has_chaos() {
                if applied_chaos {
                    blobstore_options = BlobstoreOptions {
                        chaos_options: ChaosOptions::new(None, None),
                        ..blobstore_options
                    };
                } else {
                    applied_chaos = true;
                }
            }

            async move {
                let store = make_blobstore(
                    fb,
                    config,
                    mysql_options,
                    component_readonly,
                    &blobstore_options,
                    config_store,
                    scrub_handler,
                    component_sampler,
                    Some(blobstoreid),
                )
                .watched()
                .await?;

                Result::<_, Error>::Ok((blobstoreid, store_type, store))
            }
        }
    }))
    .await?;

    // For now, `partition` could do this, but this will be easier to extend when we introduce more store types
    let mut normal_components = vec![];
    let mut write_only_components = vec![];
    for (blobstore_id, store_type, store) in components.into_iter() {
        match store_type {
            MultiplexedStoreType::Normal => normal_components.push((blobstore_id, store)),
            MultiplexedStoreType::WriteOnly => write_only_components.push((blobstore_id, store)),
        }
    }
    Ok((normal_components, write_only_components))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use blobstore::BlobstoreBytes;
    use context::CoreContext;
    use fbinit::FacebookInit;
    use metaconfig_types::LocalDatabaseConfig;
    use mononoke_macros::mononoke;

    use super::*;

    fn multiplex_config(dir: &Path) -> BlobConfig {
        BlobConfig::MultiplexedWal {
            multiplex_id: MultiplexId::new(1),
            blobstores: vec![(
                BlobstoreId::new(0),
                MultiplexedStoreType::Normal,
                BlobConfig::Files {
                    path: dir.join("files"),
                },
            )],
            write_quorum: 1,
            queue_db: ShardedDatabaseConfig::Local(LocalDatabaseConfig {
                path: dir.join("queue"),
            }),
            inner_blobstores_scuba_table: None,
            multiplex_scuba_table: None,
            scuba_sample_rate: NonZeroU64::new(1).unwrap(),
        }
    }

    async fn make(
        fb: FacebookInit,
        dir: &Path,
        readonly: bool,
    ) -> Result<Arc<dyn Blobstore>, Error> {
        let (_source, config_store) = sqlblob::get_test_config_store();
        let options = BlobstoreOptions::new(
            ChaosOptions::new(None, None),
            DelayOptions::default(),
            ThrottleOptions::default(),
            #[cfg(fbcode_build)]
            ManifoldOptions::default(),
            PackOptions::default(),
            CachelibBlobstoreOptions::default(),
            Some(PutBehaviour::Overwrite),
            MysqlOptions::default(),
        );
        make_blobstore(
            fb,
            multiplex_config(dir),
            &MysqlOptions::default(),
            ReadOnlyStorage(readonly),
            &options,
            &config_store,
            &multiplexedblob::scrub::default_scrub_handler(),
            None,
            None,
        )
        .await
    }

    /// FIXME: BUG! `make_blobstore(.., ReadOnlyStorage(true), ..)` on a MultiplexedWal config did
    /// not make the multiplex read-only for `unlink`: the inner components were built writable and
    /// no ReadOnlyBlobstore wrapped the multiplex, so `unlink` deleted the blob while `put` was
    /// refused. `with_readonly_storage` promises to error on any write. This test pins the current
    /// behaviour; the next diff fixes it.
    #[mononoke::fbinit_test]
    async fn readonly_storage_multiplex_unlink(fb: FacebookInit) -> Result<(), Error> {
        let ctx = CoreContext::test_mock(fb);
        let dir = tempfile::TempDir::new()?;

        // Writable instance: store a blob.
        let rw = make(fb, dir.path(), false).await?;
        rw.put(&ctx, "key".to_string(), BlobstoreBytes::from_bytes("value"))
            .await?;
        assert!(rw.get(&ctx, "key").await?.is_some());

        // Read-only instance over the same storage.
        let ro = make(fb, dir.path(), true).await?;
        // `put` is refused (the WAL queue connection is read-only) ...
        assert!(
            ro.put(&ctx, "key2".to_string(), BlobstoreBytes::from_bytes("v"))
                .await
                .is_err()
        );
        // FIXME: BUG! ... but `unlink` is not: with_readonly_storage promises to "Error on any
        // attempts to write to storage", yet the blob is deleted from the inner Files store.
        ro.unlink(&ctx, "key").await?;
        assert!(rw.get(&ctx, "key").await?.is_none());
        Ok(())
    }
}
