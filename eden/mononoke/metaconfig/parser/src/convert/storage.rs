/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::num::NonZeroU64;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use metaconfig_types::BlobConfig;
use metaconfig_types::BlobstoreId;
use metaconfig_types::BubbleDeletionMode;
use metaconfig_types::DatabaseConfig;
use metaconfig_types::EphemeralBlobstoreConfig;
use metaconfig_types::EphemeralCommitsConfig;
use metaconfig_types::FilestoreParams;
use metaconfig_types::LocalDatabaseConfig;
use metaconfig_types::MetadataDatabaseConfig;
use metaconfig_types::MultiplexId;
use metaconfig_types::MultiplexedStoreType;
use metaconfig_types::OssRemoteDatabaseConfig;
use metaconfig_types::OssRemoteMetadataDatabaseConfig;
use metaconfig_types::PackConfig;
use metaconfig_types::PackFormat;
use metaconfig_types::RemoteDatabaseConfig;
use metaconfig_types::RemoteMetadataDatabaseConfig;
use metaconfig_types::ShardableRemoteDatabaseConfig;
use metaconfig_types::ShardedDatabaseConfig;
use metaconfig_types::ShardedRemoteDatabaseConfig;
use metaconfig_types::StorageConfig;
use nonzero_ext::nonzero;
use repos::RawBlobstoreConfig;
use repos::RawBlobstoreMultiplexedWal;
use repos::RawBlobstorePackConfig;
use repos::RawBlobstorePackFormat;
use repos::RawBubbleDeletionMode;
use repos::RawDbConfig;
use repos::RawDbLocal;
use repos::RawDbRemote;
use repos::RawDbShardableRemote;
use repos::RawDbShardedRemote;
use repos::RawEphemeralBlobstoreConfig;
use repos::RawEphemeralCommitsConfig;
use repos::RawFilestoreParams;
use repos::RawMetadataConfig;
use repos::RawMultiplexedStoreNormal;
use repos::RawMultiplexedStoreType;
use repos::RawMultiplexedStoreWriteOnly;
use repos::RawOssDbRemote;
use repos::RawShardedDbConfig;
use repos::RawStorageConfig;

use crate::convert::Convert;

impl Convert for RawStorageConfig {
    type Output = StorageConfig;

    fn convert(self) -> Result<Self::Output> {
        Ok(StorageConfig {
            metadata: self.metadata.convert()?,
            blobstore: self.blobstore.convert()?,
            ephemeral_blobstore: self
                .ephemeral_blobstore
                .map(RawEphemeralBlobstoreConfig::convert)
                .transpose()?,
            ephemeral_commits: self
                .ephemeral_commits
                .map(RawEphemeralCommitsConfig::convert)
                .transpose()?,
            mutable_blobstore: self.mutable_blobstore.convert()?,
        })
    }
}

impl Convert for RawBubbleDeletionMode {
    type Output = BubbleDeletionMode;

    fn convert(self) -> Result<Self::Output> {
        let deletion_mode = match self {
            RawBubbleDeletionMode::DISABLED => BubbleDeletionMode::Disabled,
            RawBubbleDeletionMode::MARK_ONLY => BubbleDeletionMode::MarkOnly,
            RawBubbleDeletionMode::MARK_AND_DELETE => BubbleDeletionMode::MarkAndDelete,
            v => return Err(anyhow!("Invalid value {v} for enum BubbleDeletionMode")),
        };
        Ok(deletion_mode)
    }
}

impl Convert for RawEphemeralBlobstoreConfig {
    type Output = EphemeralBlobstoreConfig;

    fn convert(self) -> Result<Self::Output> {
        Ok(EphemeralBlobstoreConfig {
            metadata: self.metadata.convert()?,
            blobstore: self.blobstore.convert()?,
            initial_bubble_lifespan: Duration::from_secs(
                self.initial_bubble_lifespan_secs
                    .try_into()
                    .context("Failed to convert initial_bubble_lifespan")?,
            ),
            bubble_expiration_grace: Duration::from_secs(
                self.bubble_expiration_grace_secs
                    .try_into()
                    .context("Failed to convert bubble_expiration_grace")?,
            ),
            bubble_deletion_mode: self.bubble_deletion_mode.convert()?,
        })
    }
}

impl Convert for RawEphemeralCommitsConfig {
    type Output = EphemeralCommitsConfig;

    fn convert(self) -> Result<Self::Output> {
        Ok(EphemeralCommitsConfig {
            metadata: self.metadata.convert()?,
        })
    }
}

impl Convert for RawBlobstoreConfig {
    type Output = BlobConfig;

    fn convert(self) -> Result<Self::Output> {
        let config = match self {
            RawBlobstoreConfig::disabled(_) => BlobConfig::Disabled,
            RawBlobstoreConfig::blob_files(raw) => BlobConfig::Files {
                path: PathBuf::from(raw.path),
            },
            RawBlobstoreConfig::blob_sqlite(raw) => BlobConfig::Sqlite {
                path: PathBuf::from(raw.path),
            },
            RawBlobstoreConfig::manifold(raw) => BlobConfig::Manifold {
                bucket: raw.manifold_bucket,
                prefix: raw.manifold_prefix,
            },
            RawBlobstoreConfig::mysql(raw) => BlobConfig::Mysql {
                remote: raw.remote.convert()?,
            },
            RawBlobstoreConfig::multiplexed_wal(RawBlobstoreMultiplexedWal {
                write_quorum,
                components,
                multiplex_id,
                queue_db,
                inner_blobstores_scuba_table,
                multiplex_scuba_table,
                scuba_sample_rate,
            }) => {
                let write_quorum: usize = write_quorum.try_into()?;
                if write_quorum > components.len() {
                    return Err(anyhow!(
                        "Not enough blobstores for {} write quorum (have {})",
                        write_quorum,
                        components.len()
                    ));
                }

                BlobConfig::MultiplexedWal {
                    multiplex_id: MultiplexId::new(multiplex_id),
                    blobstores: components
                        .into_iter()
                        .map(|comp| {
                            Ok((
                                BlobstoreId::new(comp.blobstore_id.try_into()?),
                                comp.store_type
                                    .convert()?
                                    .unwrap_or(MultiplexedStoreType::Normal),
                                comp.blobstore.convert()?,
                            ))
                        })
                        .collect::<Result<Vec<_>>>()?,
                    write_quorum,
                    queue_db: queue_db.convert()?,
                    inner_blobstores_scuba_table,
                    multiplex_scuba_table,
                    scuba_sample_rate: parse_scuba_sample_rate(scuba_sample_rate)?,
                }
            }

            RawBlobstoreConfig::manifold_with_ttl(raw) => {
                let ttl = Duration::from_secs(raw.ttl_secs.try_into()?);
                BlobConfig::ManifoldWithTtl {
                    bucket: raw.manifold_bucket,
                    prefix: raw.manifold_prefix,
                    ttl,
                }
            }
            RawBlobstoreConfig::logging(raw) => BlobConfig::Logging {
                scuba_table: raw.scuba_table,
                scuba_sample_rate: parse_scuba_sample_rate(raw.scuba_sample_rate)?,
                blobconfig: Box::new(raw.blobstore.convert()?),
            },
            RawBlobstoreConfig::pack(raw) => BlobConfig::Pack {
                blobconfig: Box::new(raw.blobstore.convert()?),
                pack_config: raw.pack_config.map(|c| c.convert()).transpose()?,
            },
            RawBlobstoreConfig::aws_s3(raw) => BlobConfig::AwsS3 {
                bucket: raw.bucket,
                region: rusoto_core::Region::from_str(&raw.region)?,
                num_concurrent_operations: raw
                    .num_concurrent_operations
                    .map(|x| x.try_into())
                    .transpose()?,
            },
            RawBlobstoreConfig::UnknownField(f) => {
                return Err(anyhow!("unsupported blobstore configuration ({f})"));
            }
        };
        Ok(config)
    }
}

fn parse_scuba_sample_rate(sample_rate: Option<i64>) -> Result<NonZeroU64> {
    let rate = sample_rate
        .map(|rate| {
            NonZeroU64::new(rate.try_into()?)
                .ok_or_else(|| anyhow!("scuba_sample_rate must be an integer larger than zero"))
        })
        .transpose()?
        .unwrap_or(nonzero!(100_u64));

    Ok(rate)
}

impl Convert for RawBlobstorePackFormat {
    type Output = PackFormat;

    fn convert(self) -> Result<Self::Output> {
        let pack_format = match self {
            RawBlobstorePackFormat::Raw(_) => PackFormat::Raw,
            RawBlobstorePackFormat::ZstdIndividual(zstd) => {
                PackFormat::ZstdIndividual(zstd.compression_level)
            }
            RawBlobstorePackFormat::UnknownField(f) => bail!("Unsupported PackFormat {f}"),
        };
        Ok(pack_format)
    }
}

impl Convert for RawBlobstorePackConfig {
    type Output = PackConfig;

    fn convert(self) -> Result<Self::Output> {
        let put_format = self.put_format.convert()?;
        Ok(PackConfig { put_format })
    }
}

impl Convert for RawDbLocal {
    type Output = LocalDatabaseConfig;

    fn convert(self) -> Result<Self::Output> {
        Ok(LocalDatabaseConfig {
            path: PathBuf::from(self.local_db_path),
        })
    }
}

impl Convert for RawDbRemote {
    type Output = RemoteDatabaseConfig;

    fn convert(self) -> Result<Self::Output> {
        Ok(RemoteDatabaseConfig {
            db_address: self.db_address,
        })
    }
}

impl Convert for RawOssDbRemote {
    type Output = OssRemoteDatabaseConfig;

    fn convert(self) -> Result<Self::Output> {
        Ok(OssRemoteDatabaseConfig {
            host: self.host,
            port: self.port,
            database: self.database,
            user_secret: self.user_secret,
            password_secret: self.password_secret,
        })
    }
}

impl Convert for RawDbShardedRemote {
    type Output = ShardedRemoteDatabaseConfig;

    fn convert(self) -> Result<Self::Output> {
        let shard_num = NonZeroUsize::new(self.shard_num.try_into()?)
            .ok_or_else(|| anyhow!("sharded remote shard_num must be > 0"))?;

        Ok(ShardedRemoteDatabaseConfig {
            shard_map: self.shard_map,
            shard_num,
        })
    }
}

impl Convert for RawDbShardableRemote {
    type Output = ShardableRemoteDatabaseConfig;

    fn convert(self) -> Result<Self::Output> {
        match self {
            RawDbShardableRemote::unsharded(raw) => {
                Ok(ShardableRemoteDatabaseConfig::Unsharded(raw.convert()?))
            }
            RawDbShardableRemote::sharded(raw) => {
                Ok(ShardableRemoteDatabaseConfig::Sharded(raw.convert()?))
            }
            RawDbShardableRemote::UnknownField(f) => {
                Err(anyhow!("unsupported database configuration ({f})"))
            }
        }
    }
}

impl Convert for RawDbConfig {
    type Output = DatabaseConfig;

    fn convert(self) -> Result<Self::Output> {
        match self {
            RawDbConfig::local(raw) => Ok(DatabaseConfig::Local(raw.convert()?)),
            RawDbConfig::remote(raw) => Ok(DatabaseConfig::Remote(raw.convert()?)),
            RawDbConfig::UnknownField(f) => {
                Err(anyhow!("unsupported database configuration ({f})"))
            }
        }
    }
}

impl Convert for RawShardedDbConfig {
    type Output = ShardedDatabaseConfig;

    fn convert(self) -> Result<Self::Output> {
        match self {
            RawShardedDbConfig::local(raw) => Ok(ShardedDatabaseConfig::Local(raw.convert()?)),
            RawShardedDbConfig::remote(raw) => Ok(ShardedDatabaseConfig::Sharded(raw.convert()?)),
            RawShardedDbConfig::unsharded(raw) => {
                Ok(ShardedDatabaseConfig::Unsharded(raw.convert()?))
            }
            RawShardedDbConfig::UnknownField(f) => {
                Err(anyhow!("unsupported database configuration ({f})"))
            }
        }
    }
}

impl Convert for RawMetadataConfig {
    type Output = MetadataDatabaseConfig;

    fn convert(self) -> Result<Self::Output> {
        match self {
            RawMetadataConfig::local(raw) => Ok(MetadataDatabaseConfig::Local(raw.convert()?)),
            RawMetadataConfig::remote(raw) => Ok(MetadataDatabaseConfig::Remote(
                RemoteMetadataDatabaseConfig {
                    production: raw.production.unwrap_or(raw.primary.clone()).convert()?,
                    commit_graph: raw.commit_graph.unwrap_or(raw.primary.clone()).convert()?,
                    bookmarks: raw.bookmarks.unwrap_or(raw.primary.clone()).convert()?,
                    bonsai_hg_mapping: raw
                        .bonsai_hg_mapping
                        .unwrap_or(raw.primary.clone())
                        .convert()?,
                    filenodes: raw.filenodes.convert()?,
                    mutation: raw.mutation.convert()?,
                    sparse_profiles: raw.sparse_profiles.convert()?,
                    bonsai_blob_mapping: raw.bonsai_blob_mapping.convert()?,
                    deletion_log: raw.deletion_log.convert()?,
                    commit_cloud: raw.commit_cloud.convert()?,
                    git_bundle_metadata: raw.git_bundles.convert()?,
                    repo_metadata: raw.repo_metadata.convert()?,
                    restricted_paths: raw.restricted_paths.convert()?,
                    commit_derived_data_mapping: raw.derived_data_mapping.convert()?,
                    repo_manifest_mapping: raw.repo_manifest_mapping.convert()?,
                },
            )),
            RawMetadataConfig::oss_remote(raw) => Ok(MetadataDatabaseConfig::OssRemote(
                OssRemoteMetadataDatabaseConfig {
                    production: raw.production.unwrap_or(raw.primary.clone()).convert()?,
                    commit_graph: raw.commit_graph.unwrap_or(raw.primary.clone()).convert()?,
                    bookmarks: raw.bookmarks.unwrap_or(raw.primary.clone()).convert()?,
                    bonsai_hg_mapping: raw
                        .bonsai_hg_mapping
                        .unwrap_or(raw.primary.clone())
                        .convert()?,
                    filenodes: raw.filenodes.convert()?,
                    mutation: raw.mutation.convert()?,
                    sparse_profiles: raw.sparse_profiles.convert()?,
                    bonsai_blob_mapping: raw.bonsai_blob_mapping.convert()?,
                    deletion_log: raw.deletion_log.convert()?,
                    commit_derived_data_mapping: raw.derived_data_mapping.convert()?,
                },
            )),
            RawMetadataConfig::UnknownField(f) => {
                Err(anyhow!("unsupported metadata database configuration ({f})"))
            }
        }
    }
}

impl Convert for RawFilestoreParams {
    type Output = FilestoreParams;

    fn convert(self) -> Result<Self::Output> {
        Ok(FilestoreParams {
            chunk_size: self.chunk_size.try_into()?,
            concurrency: self.concurrency.try_into()?,
        })
    }
}

impl Convert for RawMultiplexedStoreType {
    type Output = MultiplexedStoreType;

    fn convert(self) -> Result<Self::Output> {
        match self {
            RawMultiplexedStoreType::normal(RawMultiplexedStoreNormal {}) => {
                Ok(MultiplexedStoreType::Normal)
            }
            RawMultiplexedStoreType::write_only(RawMultiplexedStoreWriteOnly {}) => {
                Ok(MultiplexedStoreType::WriteOnly)
            }
            RawMultiplexedStoreType::UnknownField(field) => {
                Err(anyhow!("unknown store type {field}"))
            }
        }
    }
}
