/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Resolves a run's repositories and storage into an immutable plan.
//!
//! The plan names every physical store reachable from the selected repos'
//! storage configs, deduplicated by location, with what the tool can do in
//! each.  Every later step re-resolves the plan from the current
//! configuration and refuses to run if its digest has changed.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::iter;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use derive_more::Display;
use metaconfig_types::BlobConfig;
use metaconfig_types::MetadataDatabaseConfig;
use metaconfig_types::RepoConfig;
use metaconfig_types::ShardableRemoteDatabaseConfig;
use mononoke_app::MononokeApp;
use mononoke_types::RepositoryId;
use mononoke_types::hash::Context as HashContext;
use serde::Deserialize;
use serde::Serialize;

use crate::policy::POLICY_VERSION;
use crate::policy::PhysicalEncoding;
use crate::policy::Selection;

pub const PLAN_FORMAT: &str = "blob-type-annihilator-plan-v2";

/// Version of the election of each pack rewrite's owning key.
pub const PACK_OWNER_ELECTION: &str = "pack-owner-v1";

/// Partitions are formed from up to this many hex digits of the hash after a
/// recipe's stem.  With none, each cell is a single partition.
pub const MAX_PARTITION_DIGITS: u8 = 2;

/// Repositories, components and reference paths listed by name in a plan
/// summary; beyond this they are only counted.
const MAX_LISTED: usize = 20;

/// Which repositories a run covers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RepoSelection {
    /// Every repository in the current configuration.
    AllConfigured,
    /// Only these repositories, so the run covers a partial scope.
    Explicit(Vec<RepositoryId>),
}

/// The operator's choices that, with the configuration, determine a plan.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanRequest {
    pub repos: RepoSelection,
    pub selection: Selection,
    /// Named storage configs scanned in addition to each repo's own, such
    /// as stores a repo used before a migration.
    pub extra_storage_configs: Vec<String>,
    pub partition_digits: u8,
}

impl PlanRequest {
    pub fn new(
        repos: RepoSelection,
        selection: Selection,
        mut extra_storage_configs: Vec<String>,
        partition_digits: u8,
    ) -> Result<Self> {
        if partition_digits > MAX_PARTITION_DIGITS {
            bail!("partition digits must be at most {MAX_PARTITION_DIGITS}");
        }
        extra_storage_configs.sort();
        extra_storage_configs.dedup();
        let repos = match repos {
            RepoSelection::AllConfigured => RepoSelection::AllConfigured,
            RepoSelection::Explicit(mut ids) => {
                if ids.is_empty() {
                    bail!("select at least one repository, or all of them");
                }
                ids.sort_unstable();
                ids.dedup();
                RepoSelection::Explicit(ids)
            }
        };
        Ok(Self {
            repos,
            selection,
            extra_storage_configs,
            partition_digits,
        })
    }
}

/// A physical store, identified by its location rather than by its position
/// in any one multiplex.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum PhysicalStore {
    Manifold { bucket: String, prefix: String },
    Sqlite { path: String },
    Mysql { remote: String },
    Files { path: String },
    AwsS3 { bucket: String, region: String },
    Disabled,
}

/// What the tool can do in a physical store.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Capability {
    /// Bounded enumeration, existence checks and unlink are available.
    Supported,
    /// The store may hold retired blobs, but the tool cannot inventory it.
    Unsupported { reason: String },
    /// Deliberately outside this tool's coverage.
    Excluded { reason: String },
}

impl Capability {
    fn for_store(store: &PhysicalStore) -> Self {
        match store {
            PhysicalStore::Manifold { .. }
            | PhysicalStore::Sqlite { .. }
            | PhysicalStore::Mysql { .. } => Capability::Supported,
            PhysicalStore::Files { .. } => Capability::Unsupported {
                reason: "fileblob enumeration is not authoritative; development stores only"
                    .to_string(),
            },
            PhysicalStore::AwsS3 { .. } => Capability::Excluded {
                reason: "retired types were never enabled on AWS S3 storage; not inventoried"
                    .to_string(),
            },
            PhysicalStore::Disabled => Capability::Excluded {
                reason: "administratively disabled blobstore".to_string(),
            },
        }
    }
}

/// Which of a repository's storage configs reaches a store.
#[derive(
    Clone,
    Debug,
    Display,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize
)]
pub enum StorageRef {
    /// The repository's own storage config.
    #[display("main")]
    Main,
    /// A named storage config requested in addition.
    #[display("extra:{_0}")]
    Extra(String),
}

/// Where a repository's storage config reaches a physical store.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComponentReference {
    pub repo_id: RepositoryId,
    pub storage: StorageRef,
    /// Wrappers and multiplex positions leading to the store.
    pub path: String,
    /// `PackEnvelope` inside a packblob wrapper.
    pub encoding: PhysicalEncoding,
}

/// The physical encodings a component stores one repository's keys under.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Encodings {
    plain: bool,
    pack_envelope: bool,
}

impl Encodings {
    fn with(self, encoding: PhysicalEncoding) -> Self {
        match encoding {
            PhysicalEncoding::Plain => Self {
                plain: true,
                ..self
            },
            PhysicalEncoding::PackEnvelope => Self {
                pack_envelope: true,
                ..self
            },
        }
    }

    pub fn iter(self) -> impl Iterator<Item = PhysicalEncoding> {
        [
            (self.plain, PhysicalEncoding::Plain),
            (self.pack_envelope, PhysicalEncoding::PackEnvelope),
        ]
        .into_iter()
        .filter_map(|(used, encoding)| used.then_some(encoding))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedComponent {
    pub id: String,
    pub store: PhysicalStore,
    pub capability: Capability,
    pub references: Vec<ComponentReference>,
}

impl PlannedComponent {
    /// Every repository referencing the component, in order, with the
    /// encodings it stores their keys under.
    pub fn repo_encodings(&self) -> BTreeMap<RepositoryId, Encodings> {
        self.references
            .iter()
            .fold(BTreeMap::new(), |mut repos, reference| {
                let encodings = repos.entry(reference.repo_id).or_default();
                *encodings = encodings.with(reference.encoding);
                repos
            })
    }

    pub fn repo_ids(&self) -> impl Iterator<Item = RepositoryId> {
        self.repo_encodings().into_keys()
    }

    pub fn is_supported(&self) -> bool {
        self.capability == Capability::Supported
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedRepo {
    pub id: RepositoryId,
    pub name: String,
    pub metadata_db: String,
    /// Storage of this repo the plan does not cover, with the reason.
    pub excluded_storage: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub format: String,
    pub policy_version: String,
    pub pack_owner_election: String,
    pub request: PlanRequest,
    pub recipes: Vec<String>,
    /// Key families, after the repository prefix, of the selected types.
    pub families: Vec<String>,
    pub repos: Vec<PlannedRepo>,
    pub components: Vec<PlannedComponent>,
    /// BLAKE2b over the canonical JSON of every other field.
    pub digest: String,
}

impl Plan {
    pub fn component(&self, id: &str) -> Result<&PlannedComponent> {
        self.components
            .iter()
            .find(|component| component.id == id)
            .with_context(|| format!("component {id} is not in the plan"))
    }

    pub fn repo(&self, repo_id: RepositoryId) -> Result<&PlannedRepo> {
        self.repos
            .iter()
            .find(|repo| repo.id == repo_id)
            .with_context(|| format!("repo {repo_id} is not in the plan"))
    }

    /// Fails unless `current`, freshly resolved, is the same plan.
    pub fn ensure_unchanged(&self, current: &Plan) -> Result<()> {
        if self.digest != current.digest {
            bail!(
                "configuration no longer matches plan {}: it now resolves to {}; review a new plan",
                self.digest,
                current.digest
            );
        }
        Ok(())
    }

    fn compute_digest(&self) -> Result<String> {
        let unsigned = Plan {
            digest: String::new(),
            ..self.clone()
        };
        let mut context = HashContext::new(b"blob-type-annihilator-plan");
        context.update(serde_json::to_vec(&unsigned)?);
        Ok(context.finish().to_hex().to_string())
    }
}

/// A plan plus the configs needed to open its supported components.
pub struct ResolvedPlan {
    pub plan: Plan,
    pub blob_configs: BTreeMap<String, BlobConfig>,
}

struct Leaf<'a> {
    store: PhysicalStore,
    config: &'a BlobConfig,
    path: String,
    encoding: PhysicalEncoding,
}

fn describe_remote(remote: &ShardableRemoteDatabaseConfig) -> String {
    match remote {
        ShardableRemoteDatabaseConfig::Unsharded(config) => config.db_address.clone(),
        ShardableRemoteDatabaseConfig::Sharded(config) => {
            format!("{}/{}", config.shard_map, config.shard_num)
        }
    }
}

fn collect_leaves<'a>(
    config: &'a BlobConfig,
    path: &str,
    encoding: PhysicalEncoding,
) -> Vec<Leaf<'a>> {
    let leaf = |store| {
        vec![Leaf {
            store,
            config,
            path: path.to_string(),
            encoding,
        }]
    };
    match config {
        BlobConfig::Disabled => leaf(PhysicalStore::Disabled),
        BlobConfig::Files { path: p } => leaf(PhysicalStore::Files {
            path: p.display().to_string(),
        }),
        BlobConfig::Sqlite { path: p } => leaf(PhysicalStore::Sqlite {
            path: p.display().to_string(),
        }),
        BlobConfig::Manifold { bucket, prefix }
        | BlobConfig::ManifoldWithTtl { bucket, prefix, .. } => leaf(PhysicalStore::Manifold {
            bucket: bucket.clone(),
            prefix: prefix.clone(),
        }),
        BlobConfig::Mysql { remote } => leaf(PhysicalStore::Mysql {
            remote: describe_remote(remote),
        }),
        BlobConfig::AwsS3 { bucket, region, .. } => leaf(PhysicalStore::AwsS3 {
            bucket: bucket.clone(),
            region: region.name().to_string(),
        }),
        BlobConfig::MultiplexedWal {
            multiplex_id,
            blobstores,
            ..
        } => blobstores
            .iter()
            .flat_map(|(blobstore_id, store_type, inner)| {
                let inner_path = format!(
                    "{path}multiplex {multiplex_id} component {blobstore_id} ({store_type:?})/"
                );
                collect_leaves(inner, &inner_path, encoding)
            })
            .collect(),
        BlobConfig::Logging { blobconfig, .. } => {
            collect_leaves(blobconfig, &format!("{path}logging/"), encoding)
        }
        BlobConfig::Pack { blobconfig, .. } => collect_leaves(
            blobconfig,
            &format!("{path}pack/"),
            PhysicalEncoding::PackEnvelope,
        ),
    }
}

fn component_id(store: &PhysicalStore) -> Result<String> {
    let mut context = HashContext::new(b"blob-type-annihilator-component");
    context.update(serde_json::to_vec(store)?);
    let hex = context.finish().to_hex();
    Ok(format!("c{}", &hex[..16]))
}

fn describe_metadata_db(config: &MetadataDatabaseConfig) -> String {
    match config {
        MetadataDatabaseConfig::Local(local) => format!("sqlite:{}", local.path.display()),
        MetadataDatabaseConfig::Remote(remote) => format!(
            "mysql:production={};commit_derived_data_mapping={}",
            remote.production.db_address,
            remote
                .commit_derived_data_mapping
                .as_ref()
                .map_or_else(|| "production".to_string(), describe_remote)
        ),
        MetadataDatabaseConfig::OssRemote(remote) => {
            let describe = |db: &metaconfig_types::OssRemoteDatabaseConfig| {
                format!("{}:{}/{}", db.host, db.port, db.database)
            };
            format!(
                "oss-mysql:production={};commit_derived_data_mapping={}",
                describe(&remote.production),
                remote
                    .commit_derived_data_mapping
                    .as_ref()
                    .map_or_else(|| "production".to_string(), describe)
            )
        }
    }
}

fn excluded_storage(config: &RepoConfig) -> Vec<String> {
    let ephemeral = config
        .storage_config
        .ephemeral_blobstore
        .is_some()
        .then(|| {
            "ephemeral blobstore: bubble keys are scoped per bubble and expire; not inventoried"
                .to_string()
        });
    let mutable = (config.storage_config.mutable_blobstore != BlobConfig::Disabled).then(|| {
        "mutable blobstore: holds mutable state, not derived data; not inventoried".to_string()
    });
    ephemeral.into_iter().chain(mutable).collect()
}

/// Loads the configs of `repo_ids` in one batch, failing on any id the
/// configuration does not know or any config that fails to load.
fn load_explicit(
    app: &MononokeApp,
    repo_ids: &[RepositoryId],
) -> Result<Vec<(String, RepoConfig)>> {
    let configs = app.configs();
    let cached = configs.repo_configs();
    let manifest: HashMap<i32, String> = configs
        .manifest()
        .map(|manifest| {
            manifest
                .repos
                .iter()
                .map(|entry| (entry.repo_id, entry.repo_name.clone()))
                .collect()
        })
        .unwrap_or_default();
    let names = repo_ids
        .iter()
        .map(|repo_id| {
            cached
                .get_repo_config_by_raw_id(repo_id.id())
                .map(|(name, _)| name.clone())
                .or_else(|| manifest.get(&repo_id.id()).cloned())
                .with_context(|| format!("repository {repo_id} is not in the configuration"))
        })
        .collect::<Result<Vec<_>>>()?;
    let outcome = configs.batch_load_repo_configs_checked(&names)?;
    loaded_or_fail(outcome.loaded, &outcome.failed)
}

fn loaded_or_fail(
    loaded: Vec<(String, RepoConfig)>,
    failed: &[(String, anyhow::Error)],
) -> Result<Vec<(String, RepoConfig)>> {
    if let Some((name, error)) = failed.first() {
        bail!(
            "{} repository configs failed to load, so the scope is unknown; first: {name}: {error:#}",
            failed.len()
        );
    }
    Ok(loaded)
}

/// Resolves `request` against the app's current configuration.
pub fn resolve(app: &MononokeApp, request: PlanRequest) -> Result<ResolvedPlan> {
    let storage_configs = app.storage_configs();

    let mut selected = match &request.repos {
        RepoSelection::AllConfigured => {
            let outcome = app.configs().load_all_repo_configs_checked()?;
            loaded_or_fail(outcome.loaded, &outcome.failed)?
        }
        RepoSelection::Explicit(ids) => load_explicit(app, ids)?,
    };
    if selected.is_empty() {
        bail!("no repositories were resolved");
    }
    selected.sort_by_key(|(_, config)| config.repoid);

    let extra_storage: Vec<(StorageRef, &BlobConfig)> = request
        .extra_storage_configs
        .iter()
        .map(|name| {
            let config = storage_configs
                .storage
                .get(name)
                .with_context(|| format!("storage config {name} is not in the configuration"))?;
            Ok((StorageRef::Extra(name.clone()), &config.blobstore))
        })
        .collect::<Result<_>>()?;

    let mut components: BTreeMap<String, PlannedComponent> = BTreeMap::new();
    let mut blob_configs = BTreeMap::new();
    for (_, config) in &selected {
        let storages = iter::once((StorageRef::Main, &config.storage_config.blobstore))
            .chain(extra_storage.iter().cloned());
        for (storage, blob_config) in storages {
            for leaf in collect_leaves(blob_config, "", PhysicalEncoding::Plain) {
                let id = component_id(&leaf.store)?;
                let component = components
                    .entry(id.clone())
                    .or_insert_with(|| PlannedComponent {
                        id: id.clone(),
                        capability: Capability::for_store(&leaf.store),
                        store: leaf.store.clone(),
                        references: Vec::new(),
                    });
                component.references.push(ComponentReference {
                    repo_id: config.repoid,
                    storage: storage.clone(),
                    path: leaf.path,
                    encoding: leaf.encoding,
                });
                blob_configs
                    .entry(id)
                    .or_insert_with(|| leaf.config.clone());
            }
        }
    }
    let repos = selected
        .iter()
        .map(|(name, config)| PlannedRepo {
            id: config.repoid,
            name: name.clone(),
            metadata_db: describe_metadata_db(&config.storage_config.metadata),
            excluded_storage: excluded_storage(config),
        })
        .collect();

    let recipes = request
        .selection
        .recipes()
        .map(|recipe| recipe.id.to_string())
        .collect();
    let families = request
        .selection
        .types()
        .flat_map(|blob_type| blob_type.audit_families())
        .map(|family| family.to_string())
        .collect();
    let mut components: Vec<PlannedComponent> = components.into_values().collect();
    components.sort_by(|a, b| (&a.store, &a.id).cmp(&(&b.store, &b.id)));
    let mut plan = Plan {
        format: PLAN_FORMAT.to_string(),
        policy_version: POLICY_VERSION.to_string(),
        pack_owner_election: PACK_OWNER_ELECTION.to_string(),
        request,
        recipes,
        families,
        repos,
        components,
        digest: String::new(),
    };
    plan.digest = plan.compute_digest()?;
    Ok(ResolvedPlan { plan, blob_configs })
}

fn count_repos(n: usize) -> String {
    match n {
        1 => "1 repo".to_string(),
        n => format!("{n} repos"),
    }
}

/// Lists `items` when there are few of them; otherwise only counts them.
fn listed(items: impl ExactSizeIterator<Item = String>) -> String {
    let count = items.len();
    if count > MAX_LISTED {
        return format!("{count} (listed in plan.json)");
    }
    items.collect::<Vec<_>>().join(", ")
}

/// Describes the plan for an operator; `plan.json` holds the full detail.
pub fn summarise(plan: &Plan) -> String {
    let scope = match plan.request.repos {
        RepoSelection::AllConfigured => "all configured",
        RepoSelection::Explicit(_) => "partial scope",
    };
    let types: Vec<String> = plan
        .request
        .selection
        .types()
        .map(|t| t.to_string())
        .collect();
    let header = [
        format!("Plan {}", plan.digest),
        format!("Retired types: {}", types.join(", ")),
        format!(
            "Repositories ({scope}, {}): {}",
            plan.repos.len(),
            listed(
                plan.repos
                    .iter()
                    .map(|repo| format!("{} ({})", repo.name, repo.id))
            )
        ),
    ];
    let components = plan.components.iter().flat_map(|component| {
        let capability = match &component.capability {
            Capability::Supported => "supported".to_string(),
            Capability::Unsupported { reason } => format!("UNSUPPORTED: {reason}"),
            Capability::Excluded { reason } => format!("EXCLUDED: {reason}"),
        };
        let mut paths: BTreeMap<(&StorageRef, &str, PhysicalEncoding), BTreeSet<RepositoryId>> =
            BTreeMap::new();
        for reference in &component.references {
            paths
                .entry((&reference.storage, &reference.path, reference.encoding))
                .or_default()
                .insert(reference.repo_id);
        }
        let title = format!(
            "Component {} {:?}: {capability}, {}",
            component.id,
            component.store,
            count_repos(component.repo_ids().count())
        );
        iter::once(title).chain(paths.into_iter().map(|((storage, path, encoding), repos)| {
            let packed = match encoding {
                PhysicalEncoding::Plain => "",
                PhysicalEncoding::PackEnvelope => " (packed)",
            };
            format!("  {storage} {path}{packed}: {}", count_repos(repos.len()))
        }))
    });
    let mut excluded: BTreeMap<&str, Vec<RepositoryId>> = BTreeMap::new();
    for repo in &plan.repos {
        for reason in &repo.excluded_storage {
            excluded.entry(reason).or_default().push(repo.id);
        }
    }
    let excluded = excluded.into_iter().map(|(reason, repos)| {
        format!(
            "Not covered in {} ({}): {reason}",
            count_repos(repos.len()),
            listed(repos.iter().map(|id| id.to_string()))
        )
    });
    header
        .into_iter()
        .chain(components)
        .chain(excluded)
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;
    use std::path::PathBuf;

    use metaconfig_types::BlobstoreId;
    use metaconfig_types::LocalDatabaseConfig;
    use metaconfig_types::MultiplexId;
    use metaconfig_types::MultiplexedStoreType;
    use metaconfig_types::ShardedDatabaseConfig;
    use mononoke_macros::mononoke;

    use super::*;
    use crate::policy::RetiredBlobType;

    fn sqlite(path: &str) -> BlobConfig {
        BlobConfig::Sqlite {
            path: PathBuf::from(path),
        }
    }

    fn multiplex(components: Vec<(u64, MultiplexedStoreType, BlobConfig)>) -> BlobConfig {
        BlobConfig::MultiplexedWal {
            multiplex_id: MultiplexId::new(1),
            blobstores: components
                .into_iter()
                .map(|(id, store_type, config)| (BlobstoreId::new(id), store_type, config))
                .collect(),
            write_quorum: 1,
            queue_db: ShardedDatabaseConfig::Local(LocalDatabaseConfig {
                path: PathBuf::from("/queue"),
            }),
            inner_blobstores_scuba_table: None,
            multiplex_scuba_table: None,
            scuba_sample_rate: NonZeroU64::new(1).expect("1 is non-zero"),
        }
    }

    #[mononoke::test]
    fn leaves_cover_every_multiplex_component_and_wrapper() -> Result<()> {
        let config = BlobConfig::Pack {
            pack_config: None,
            blobconfig: Box::new(multiplex(vec![
                (0, MultiplexedStoreType::Normal, sqlite("/a")),
                (
                    1,
                    MultiplexedStoreType::WriteOnly,
                    BlobConfig::Logging {
                        blobconfig: Box::new(BlobConfig::Manifold {
                            bucket: "bucket".to_string(),
                            prefix: String::new(),
                        }),
                        scuba_table: None,
                        scuba_sample_rate: NonZeroU64::new(1).expect("1 is non-zero"),
                    },
                ),
                (
                    2,
                    MultiplexedStoreType::Normal,
                    BlobConfig::AwsS3 {
                        bucket: "s3".to_string(),
                        region: Default::default(),
                        num_concurrent_operations: None,
                    },
                ),
            ])),
        };
        let described: Vec<_> = collect_leaves(&config, "", PhysicalEncoding::Plain)
            .into_iter()
            .map(|leaf| (leaf.store, leaf.path, leaf.encoding))
            .collect();
        assert_eq!(described.len(), 3, "every component, write-only included");
        assert!(
            described
                .iter()
                .all(|(_, _, encoding)| *encoding == PhysicalEncoding::PackEnvelope)
        );
        assert_eq!(
            described[1].1, "pack/multiplex 1 component 1 (WriteOnly)/logging/",
            "the path records how the store was reached"
        );
        assert!(Capability::for_store(&described[0].0) == Capability::Supported);
        assert!(Capability::for_store(&described[1].0) == Capability::Supported);
        assert!(
            matches!(
                Capability::for_store(&described[2].0),
                Capability::Excluded { .. }
            ),
            "S3 is reported as excluded, never as clean"
        );
        Ok(())
    }

    #[mononoke::test]
    fn component_ids_depend_only_on_location() -> Result<()> {
        let a = component_id(&PhysicalStore::Sqlite {
            path: "/a".to_string(),
        })?;
        let a_again = component_id(&PhysicalStore::Sqlite {
            path: "/a".to_string(),
        })?;
        let b = component_id(&PhysicalStore::Sqlite {
            path: "/b".to_string(),
        })?;
        let manifold = component_id(&PhysicalStore::Manifold {
            bucket: "/a".to_string(),
            prefix: String::new(),
        })?;
        assert_eq!(a, a_again);
        assert_ne!(a, b);
        assert_ne!(a, manifold);
        Ok(())
    }

    #[mononoke::test]
    fn mysql_components_are_identified_by_their_shard_map() {
        let sharded =
            ShardableRemoteDatabaseConfig::Sharded(metaconfig_types::ShardedRemoteDatabaseConfig {
                shard_map: "xdb.blobstore".to_string(),
                shard_num: std::num::NonZeroUsize::new(4000).expect("4000 is non-zero"),
            });
        assert_eq!(describe_remote(&sharded), "xdb.blobstore/4000");
        let unsharded =
            ShardableRemoteDatabaseConfig::Unsharded(metaconfig_types::RemoteDatabaseConfig {
                db_address: "xdb.single".to_string(),
            });
        assert_eq!(describe_remote(&unsharded), "xdb.single");
    }

    #[mononoke::test]
    fn encodings_are_gathered_per_repo() {
        let reference = |repo_id, encoding| ComponentReference {
            repo_id: RepositoryId::new(repo_id),
            storage: StorageRef::Main,
            path: String::new(),
            encoding,
        };
        let component = PlannedComponent {
            id: "c".to_string(),
            store: PhysicalStore::Disabled,
            capability: Capability::Supported,
            references: vec![
                reference(2, PhysicalEncoding::PackEnvelope),
                reference(1, PhysicalEncoding::Plain),
                reference(2, PhysicalEncoding::Plain),
            ],
        };
        let repos = component.repo_encodings();
        assert_eq!(
            repos.keys().copied().collect::<Vec<_>>(),
            vec![RepositoryId::new(1), RepositoryId::new(2)]
        );
        assert_eq!(
            repos[&RepositoryId::new(1)].iter().collect::<Vec<_>>(),
            vec![PhysicalEncoding::Plain]
        );
        assert_eq!(
            repos[&RepositoryId::new(2)].iter().collect::<Vec<_>>(),
            vec![PhysicalEncoding::Plain, PhysicalEncoding::PackEnvelope]
        );
    }

    #[mononoke::test]
    fn storage_refs_display_their_config() {
        assert_eq!(StorageRef::Main.to_string(), "main");
        assert_eq!(
            StorageRef::Extra("old".to_string()).to_string(),
            "extra:old"
        );
    }

    #[mononoke::test]
    fn request_validation() -> Result<()> {
        let selection = Selection::new(RetiredBlobType::all())?;
        assert!(
            PlanRequest::new(RepoSelection::AllConfigured, selection.clone(), vec![], 0).is_ok(),
            "no digits is one partition per cell"
        );
        assert!(
            PlanRequest::new(RepoSelection::AllConfigured, selection.clone(), vec![], 3).is_err()
        );
        assert!(
            PlanRequest::new(
                RepoSelection::Explicit(vec![]),
                selection.clone(),
                vec![],
                2
            )
            .is_err()
        );
        let ids = |ids: &[i32]| ids.iter().copied().map(RepositoryId::new).collect();
        let request = PlanRequest::new(
            RepoSelection::Explicit(ids(&[3, 1, 3])),
            selection,
            vec!["b".to_string(), "a".to_string(), "b".to_string()],
            2,
        )?;
        assert_eq!(request.repos, RepoSelection::Explicit(ids(&[1, 3])));
        assert_eq!(request.extra_storage_configs, vec!["a", "b"]);
        Ok(())
    }
}
