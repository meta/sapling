/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashSet;
use std::fmt;
use std::num::NonZeroUsize;
use std::sync::Arc;

use anyhow::Result;
use clap::ValueEnum;
use commit_graph::CommitGraphRef;
use context::CoreContext;
use context::SamplingKey;
use futures::stream;
use futures::stream::Stream;
use futures::stream::StreamExt;
use futures::stream::TryStreamExt;
use mononoke_macros::mononoke;
use mononoke_types::ChangesetId;
use mononoke_types::audit::CommitInfo;
use mononoke_types::audit::LoadContext;
use mononoke_types::audit::LoadError;
use mononoke_types::audit::LoadTags;
use mononoke_types::audit::ObjectId;
use mononoke_types::audit::ValidationOutcome;
use mononoke_types::audit::WalkError;
use mononoke_types::audit::WalkStage;
use repo_blobstore::RepoBlobstoreRef;
use tokio_util::task::AbortOnDropHandle;

use crate::families::Family;
use crate::validation::ValidationSummary;

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum Operation {
    Walk,
    Validate,
}

impl Operation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Walk => "walk",
            Self::Validate => "validate",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct StriderLimits {
    /// Commits audited at once.
    pub max_concurrency: NonZeroUsize,
    /// Objects one commit loads at once, separately while enumerating and
    /// while validating, for each kind of read a family makes; see
    /// `Walkable::walk_new`.
    pub max_loads_per_commit: NonZeroUsize,
}

/// Why one family could not validate everything one commit added.
#[derive(Clone, Debug)]
pub struct Failure {
    pub stage: WalkStage,
    pub object: Option<ObjectId>,
    pub class: &'static str,
    pub cause: String,
    /// The object's storage key, when it is stored in the blobstore.
    pub storage_key: Option<String>,
}

impl Failure {
    /// Whether the failure is a problem found in storage: an object it names
    /// is missing or unreadable. Otherwise the family could not be checked,
    /// such as when a commit's parents could not be listed.
    pub fn found_problem(&self) -> bool {
        self.object.is_some() && matches!(self.class, "missing" | "unreadable")
    }

    fn new<R: 'static>(
        stage: WalkStage,
        object: Option<ObjectId>,
        error: &LoadError,
        repo: &R,
    ) -> Self {
        Self {
            stage,
            storage_key: object.as_ref().and_then(|object| object.storage_key(repo)),
            object,
            class: error.class(),
            cause: format!("{error:#}"),
        }
    }
}

#[derive(Debug)]
pub enum FamilyStatus {
    Validated,
    /// The commit's own mapping is absent, so the family added nothing that
    /// can be checked yet.
    NotDerived,
    /// The commit is excluded from the run, so nothing was read for it.
    Excluded,
    /// A failure ended the family's walk.
    Incomplete(Failure),
}

/// The result of one family for one commit.
#[derive(Debug)]
pub struct FamilyReport {
    pub family: &'static str,
    /// Objects that loaded, in the order their loads finished, when the
    /// strider lists them; otherwise empty. Partial when the family is
    /// incomplete.
    pub validated: Vec<ObjectId>,
    /// Number of objects that loaded, listed or not.
    pub validated_objects: u64,
    pub validation: ValidationSummary,
    pub status: FamilyStatus,
}

impl FamilyReport {
    pub(crate) fn new(family: &'static str) -> Self {
        Self {
            family,
            validated: Vec::new(),
            validated_objects: 0,
            validation: ValidationSummary::default(),
            status: FamilyStatus::Validated,
        }
    }

    pub fn failure(&self) -> Option<&Failure> {
        match &self.status {
            FamilyStatus::Incomplete(failure) => Some(failure),
            FamilyStatus::Validated | FamilyStatus::NotDerived | FamilyStatus::Excluded => None,
        }
    }
}

impl FamilyStatus {
    /// Why the walk ended. An absent mapping for the commit itself is not a
    /// failure: Walker also ends such a walk normally, because derivation is
    /// asynchronous and on demand for drafts. It is only counted, so a lost
    /// mapping blob goes unreported; make it `Incomplete` here to report it.
    fn ended(failure: Failure, error: &LoadError) -> Self {
        match error {
            LoadError::NotDerived(_) => Self::NotDerived,
            LoadError::Missing(_) | LoadError::Unreadable(_) => Self::Incomplete(failure),
        }
    }
}

#[derive(Debug)]
pub struct CommitReport {
    pub changeset_id: ChangesetId,
    /// One report per selected family, in selection order.
    pub families: Vec<FamilyReport>,
}

impl CommitReport {
    pub fn is_incomplete(&self) -> bool {
        self.families
            .iter()
            .any(|family| family.failure().is_some())
    }
}

/// The context of an error [`Strider::audit`] yields for a commit whose audit
/// panicked.
#[derive(Debug)]
pub struct AuditPanicked(pub ChangesetId);

impl fmt::Display for AuditPanicked {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "The audit of {} panicked", self.0)
    }
}

/// Whether `error`, yielded by [`Strider::audit`], is a commit's audit that
/// panicked.
pub fn is_panic(error: &anyhow::Error) -> bool {
    error.downcast_ref::<AuditPanicked>().is_some()
}

pub struct Strider<R> {
    ctx: CoreContext,
    /// The context every commit's loads run under.
    load_ctx: LoadContext,
    repo: Arc<R>,
    families: Vec<Family<R>>,
    /// Commits for which no family reads anything.
    excluded: HashSet<ChangesetId>,
    operation: Operation,
    limits: StriderLimits,
    /// Keep each family's validated objects in its report. They are held until
    /// the commit finishes, so memory grows with the commit's size.
    list_validated: bool,
}

impl<R> Strider<R>
where
    R: CommitGraphRef + RepoBlobstoreRef + Send + Sync + 'static,
{
    pub fn new(
        ctx: CoreContext,
        repo: Arc<R>,
        families: Vec<Family<R>>,
        excluded: HashSet<ChangesetId>,
        operation: Operation,
        limits: StriderLimits,
        list_validated: bool,
        tags: Arc<LoadTags>,
    ) -> Self {
        // Every audit context carries a sampling key, so a read that misses
        // its load's tag is told apart from reads outside the audit, which
        // carry none.
        let ctx = ctx.clone_and_sample(SamplingKey::new());
        let load_ctx = LoadContext::new(ctx.clone(), Arc::new(repo.repo_blobstore().clone()), tags);
        Self {
            ctx,
            load_ctx,
            repo,
            families,
            excluded,
            operation,
            limits,
            list_validated,
        }
    }

    /// Audits each commit's additions for every selected family, each commit
    /// on its own task. Reports are yielded as commits finish, not in input
    /// order. A commit whose audit panicked is yielded as an error in its
    /// place (see [`is_panic`]), and the other commits are still audited. An
    /// error from `commits` is yielded last: it ends the stream.
    /// Dropping the stream, or its end on such an error, aborts the audits
    /// still running.
    pub fn audit<'a>(
        self: &Arc<Self>,
        commits: impl Stream<Item = Result<ChangesetId>> + 'a,
    ) -> impl Stream<Item = Result<CommitReport>> + 'a {
        let strider = self.clone();
        let reports = Box::pin(
            commits
                .map_ok(move |changeset_id| {
                    let strider = strider.clone();
                    // Spawning inside the future keeps at most `max_concurrency`
                    // audits running while the stream is read, and dropping
                    // the future aborts its audit.
                    async move {
                        AbortOnDropHandle::new(mononoke::spawn_task(async move {
                            strider.audit_commit(changeset_id).await
                        }))
                        .await
                        .map_err(|error| {
                            let panicked = error.is_panic();
                            let error = anyhow::Error::from(error);
                            if panicked {
                                error.context(AuditPanicked(changeset_id))
                            } else {
                                error.context(format!("The audit of {changeset_id} did not finish"))
                            }
                        })
                    }
                })
                .try_buffer_unordered(self.limits.max_concurrency.get()),
        );
        stream::unfold(Some(reports), |reports| async move {
            let mut reports = reports?;
            let report = reports.next().await?;
            let rest = report
                .as_ref()
                .err()
                .is_none_or(is_panic)
                .then_some(reports);
            Some((report, rest))
        })
    }

    async fn audit_commit(&self, changeset_id: ChangesetId) -> CommitReport {
        // Nobody knows why a changeset was excluded, so no family reads
        // anything for its commit.
        if self.excluded.contains(&changeset_id) {
            return CommitReport {
                changeset_id,
                families: self.unaudited(|| FamilyStatus::Excluded),
            };
        }
        let parents = self
            .repo
            .commit_graph()
            .changeset_parents(&self.ctx, changeset_id)
            .await;
        let families = match parents {
            Ok(parents) => {
                let commit = Arc::new(CommitInfo {
                    changeset_id,
                    parents: parents.into_vec(),
                });
                let ctx = self.load_ctx.clone().with_changeset_id(changeset_id);
                let mut reports = Vec::with_capacity(self.families.len());
                for family in &self.families {
                    reports.push(self.audit_family(&ctx, family, &commit).await);
                }
                reports
            }
            // Without its parents the commit cannot be diffed, so no family runs.
            Err(error) => {
                let failure = Failure::new(
                    WalkStage::Enumerate,
                    None,
                    &LoadError::from(error),
                    &*self.repo,
                );
                self.unaudited(|| FamilyStatus::Incomplete(failure.clone()))
            }
        };
        CommitReport {
            changeset_id,
            families,
        }
    }

    /// A report with `status` for every family, when none of them runs.
    fn unaudited(&self, status: impl Fn() -> FamilyStatus) -> Vec<FamilyReport> {
        self.families
            .iter()
            .map(|family| FamilyReport {
                status: status(),
                ..FamilyReport::new(family.name)
            })
            .collect()
    }

    /// Loads up to `max_loads_per_commit` of the family's objects at once.
    /// An object emitted more than once is loaded and counted each time. The
    /// first failure ends the family and drops the loads still running.
    async fn audit_family(
        &self,
        ctx: &LoadContext,
        family: &Family<R>,
        commit: &Arc<CommitInfo>,
    ) -> FamilyReport {
        let repo = &*self.repo;
        let run_checks = self.operation == Operation::Validate;
        let mut results = family
            .walk_new(
                ctx.clone(),
                self.repo.clone(),
                commit.clone(),
                self.limits.max_loads_per_commit,
            )
            .map(|object| async move {
                let object = object?;
                let id = object.id().clone();
                let result = object.validate(ctx, repo, run_checks).await;
                Ok::<_, WalkError>((id, result))
            })
            .buffer_unordered(self.limits.max_loads_per_commit.get());

        let mut report = FamilyReport::new(family.name);
        while let Some(result) = results.next().await {
            let (id, result) = match result {
                Ok(loaded) => loaded,
                Err(error) => {
                    let (stage, object, source) = error.into_parts();
                    let failure = Failure::new(stage, object, &source, repo);
                    report.status = FamilyStatus::ended(failure, &source);
                    break;
                }
            };
            match result {
                Ok(outcomes) => {
                    for outcome in outcomes {
                        if let ValidationOutcome::Failed { check, message } = &outcome {
                            tracing::warn!(
                                changeset_id = %commit.changeset_id,
                                family = family.name,
                                object = %id,
                                storage_key = id.storage_key(repo).as_deref().unwrap_or("none"),
                                check,
                                message,
                                "Strider validation failed"
                            );
                        }
                        report.validation.record(&outcome);
                    }
                    if self.list_validated {
                        report.validated.push(id);
                    }
                    report.validated_objects += 1;
                }
                Err(error) => {
                    let failure = Failure::new(WalkStage::Load, Some(id), &error, repo);
                    report.status = FamilyStatus::ended(failure, &error);
                    break;
                }
            }
        }
        report
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::collections::BTreeSet;
    use std::collections::HashMap;
    use std::collections::HashSet;
    use std::fmt;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::task::Poll;

    use anyhow::Result;
    use async_trait::async_trait;
    use blobstore::Blobstore;
    use blobstore::BlobstoreGetData;
    use blobstore::Loadable;
    use context::CoreContext;
    use deleted_manifest::RootDeletedManifestIdCommon;
    use deleted_manifest::RootDeletedManifestV2Id;
    use derivation_queue_thrift::DerivationPriority;
    use fbinit::FacebookInit;
    use futures::TryStreamExt;
    use futures::future;
    use futures::stream;
    use memblob::Memblob;
    use metaconfig_types::BlobstoreId;
    use mononoke_macros::mononoke;
    use mononoke_types::BlobstoreKey;
    use mononoke_types::ChangesetId;
    use mononoke_types::DerivableType;
    use mononoke_types::audit::LoadTag;
    use mononoke_types::audit::StoredObject;
    use mononoke_types::audit::TaggedContext;
    use mononoke_types::audit::Validatable;
    use mononoke_types::audit::WalkStream;
    use mononoke_types::audit::Walkable;
    use repo_derived_data::RepoDerivedDataRef;
    use repo_derived_data::audit::Mapping;
    use repo_identity::RepoIdentityRef;
    use samplingblob::SamplingBlobstore;
    use samplingblob::SamplingBlobstoreSamplingComponentBlobstore;
    use samplingblob::SamplingHandler;
    use test_repo_factory::TestRepoFactory;
    use tests_utils::BasicTestRepo;
    use tests_utils::CreateCommitContext;
    use tests_utils::audit::walk;
    use unodes::RootUnodeManifestId;

    use super::*;
    use crate::families::families;
    use crate::reads::ReadSampler;
    use crate::reads::ReadTotals;
    use crate::test_utils::test_repo;

    type Repo = BasicTestRepo;

    /// While `recording` is set, records the tag of each storage read, or the
    /// key of a read whose context carries no live tag.
    #[derive(Debug)]
    struct TagRecorder {
        tags: Arc<LoadTags>,
        recording: AtomicBool,
        tagged: Mutex<Vec<LoadTag>>,
        /// The key of each tagged read, with the object its tag names.
        tagged_keys: Mutex<Vec<(ObjectId, String)>>,
        untagged: Mutex<Vec<String>>,
    }

    impl SamplingHandler for TagRecorder {
        fn sample_get(
            &self,
            ctx: &CoreContext,
            key: &str,
            _value: Option<&BlobstoreGetData>,
        ) -> Result<()> {
            if self.recording.load(Ordering::SeqCst) {
                match ctx.sampling_key().and_then(|key| self.tags.get(key)) {
                    Some(tag) => {
                        self.tagged_keys
                            .lock()
                            .unwrap()
                            .push((tag.object.clone(), key.to_owned()));
                        self.tagged.lock().unwrap().push(tag);
                    }
                    None => self.untagged.lock().unwrap().push(key.to_owned()),
                }
            }
            Ok(())
        }
    }

    /// A repository whose store records each read's tag, with the registry
    /// the tags are in.
    async fn tag_recording_repo(
        fb: FacebookInit,
    ) -> Result<(Arc<Repo>, Memblob, Arc<LoadTags>, Arc<TagRecorder>)> {
        let blobstore = Memblob::default();
        let tags = Arc::new(LoadTags::default());
        let recorder = Arc::new(TagRecorder {
            tags: tags.clone(),
            recording: AtomicBool::new(false),
            tagged: Mutex::new(Vec::new()),
            tagged_keys: Mutex::new(Vec::new()),
            untagged: Mutex::new(Vec::new()),
        });
        let repo: Repo = TestRepoFactory::new(fb)?
            .with_blobstore(Arc::new(SamplingBlobstore::new(
                blobstore.clone(),
                recorder.clone(),
            )))
            .build()
            .await?;
        Ok((Arc::new(repo), blobstore, tags, recorder))
    }

    /// A repository whose store's reads go to a `ReadSampler`, as Strider's
    /// do, with the registry the sampler reads the tags from.
    async fn read_sampling_repo(
        fb: FacebookInit,
    ) -> Result<(Arc<Repo>, Memblob, Arc<LoadTags>, Arc<ReadSampler>)> {
        let blobstore = Memblob::default();
        let tags = Arc::new(LoadTags::default());
        let sampler = Arc::new(ReadSampler::new(tags.clone(), false));
        let repo: Repo = TestRepoFactory::new(fb)?
            .with_blobstore(Arc::new(SamplingBlobstoreSamplingComponentBlobstore::new(
                blobstore.clone(),
                Some(BlobstoreId::new(1)),
                sampler.clone(),
            )))
            .build()
            .await?;
        Ok((Arc::new(repo), blobstore, tags, sampler))
    }

    /// The sampler's totals, by type name.
    fn totals_by_name(sampler: &ReadSampler) -> BTreeMap<String, ReadTotals> {
        sampler
            .totals()
            .into_iter()
            .map(|(object_type, totals)| (object_type.to_string(), totals))
            .collect()
    }

    fn is_validated(family: &FamilyReport) -> bool {
        matches!(family.status, FamilyStatus::Validated)
    }

    fn all_validated(report: &CommitReport) -> bool {
        report.families.iter().all(is_validated)
    }

    fn strider(ctx: &CoreContext, repo: &Arc<Repo>, family_names: &[&str]) -> Arc<Strider<Repo>> {
        tagging_strider(ctx, repo, family_names, Arc::new(LoadTags::default()))
    }

    fn tagging_strider(
        ctx: &CoreContext,
        repo: &Arc<Repo>,
        family_names: &[&str],
        tags: Arc<LoadTags>,
    ) -> Arc<Strider<Repo>> {
        Arc::new(Strider::new(
            ctx.clone(),
            repo.clone(),
            families()
                .into_iter()
                .filter(|family| family_names.contains(&family.name))
                .collect(),
            HashSet::new(),
            Operation::Validate,
            StriderLimits {
                max_concurrency: NonZeroUsize::new(2).unwrap(),
                max_loads_per_commit: NonZeroUsize::new(4).unwrap(),
            },
            true,
            tags,
        ))
    }

    /// Audits `commits` and returns the reports keyed by commit, in input order.
    async fn audit(
        strider: &Arc<Strider<Repo>>,
        commits: &[ChangesetId],
    ) -> Result<Vec<CommitReport>> {
        let mut reports = strider
            .audit(stream::iter(commits.iter().copied().map(Ok)))
            .try_collect::<Vec<_>>()
            .await?;
        reports.sort_by_key(|report| {
            commits
                .iter()
                .position(|commit| *commit == report.changeset_id)
        });
        assert_eq!(
            reports
                .iter()
                .map(|report| report.changeset_id)
                .collect::<Vec<_>>(),
            commits
        );
        Ok(reports)
    }

    async fn derive(
        ctx: &CoreContext,
        repo: &Repo,
        changeset_id: ChangesetId,
    ) -> Result<RootDeletedManifestV2Id> {
        Ok(repo
            .repo_derived_data()
            .derive::<RootDeletedManifestV2Id>(ctx, changeset_id, DerivationPriority::LOW)
            .await?)
    }

    async fn unlink(repo: &Repo, blobstore: &Memblob, key: impl BlobstoreKey) -> Result<()> {
        let key = format!(
            "{}{}",
            repo.repo_identity().id().prefix(),
            key.blobstore_key()
        );
        assert_eq!(blobstore.unlink(key).await?, Some(()));
        Ok(())
    }

    /// A adds `dir/a` and `live`; B deletes `dir/a`. Both are derived.
    async fn two_commits(ctx: &CoreContext, repo: &Repo) -> Result<(ChangesetId, ChangesetId)> {
        let a = CreateCommitContext::new_root(ctx, repo)
            .add_file("dir/a", "a")
            .add_file("live", "live")
            .commit()
            .await?;
        let b = CreateCommitContext::new(ctx, repo, vec![a])
            .delete_file("dir/a")
            .commit()
            .await?;
        derive(ctx, repo, a).await?;
        derive(ctx, repo, b).await?;
        Ok((a, b))
    }

    #[mononoke::fbinit_test]
    async fn each_commit_validates_what_its_families_added(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let (repo, _blobstore) = test_repo(fb).await?;
        let (a, b) = two_commits(&ctx, &repo).await?;
        let strider = strider(&ctx, &repo, &["core", "deleted-manifest"]);

        let reports = audit(&strider, &[a, b]).await?;

        for (report, commit) in reports.iter().zip([a, b]) {
            assert_eq!(
                report
                    .families
                    .iter()
                    .map(|family| (family.family, is_validated(family)))
                    .collect::<Vec<_>>(),
                vec![("core", true), ("deleted-manifest", true)]
            );
            let expected = walk::<RootDeletedManifestV2Id, _>(&ctx, &repo, commit, 4)
                .await??
                .iter()
                .map(|object| object.id().clone())
                .collect::<HashSet<_>>();
            assert_eq!(
                report.families[1]
                    .validated
                    .iter()
                    .cloned()
                    .collect::<HashSet<_>>(),
                expected
            );
        }
        // A commits two files and B none: each changeset plus its contents.
        assert_eq!(reports[0].families[0].validated.len(), 3);
        assert_eq!(reports[1].families[0].validated.len(), 1);
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn unlisted_objects_are_counted_but_not_kept(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let (repo, _blobstore) = test_repo(fb).await?;
        let (a, _b) = two_commits(&ctx, &repo).await?;
        let strider = Arc::new(Strider::new(
            ctx.clone(),
            repo.clone(),
            families()
                .into_iter()
                .filter(|family| family.name == "core")
                .collect(),
            HashSet::new(),
            Operation::Validate,
            StriderLimits {
                max_concurrency: NonZeroUsize::new(2).unwrap(),
                max_loads_per_commit: NonZeroUsize::new(4).unwrap(),
            },
            false,
            Arc::new(LoadTags::default()),
        ));

        let reports = audit(&strider, &[a]).await?;

        assert!(all_validated(&reports[0]));
        // A's changeset and its two file contents.
        assert_eq!(reports[0].families[0].validated_objects, 3);
        assert!(reports[0].families[0].validated.is_empty());
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn failure_in_one_family_leaves_other_families_and_commits(
        fb: FacebookInit,
    ) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let (repo, blobstore) = test_repo(fb).await?;
        let (a, b) = two_commits(&ctx, &repo).await?;
        let root_b = *derive(&ctx, &repo, b).await?.id();
        unlink(&repo, &blobstore, root_b).await?;
        let strider = strider(&ctx, &repo, &["core", "deleted-manifest"]);

        let reports = audit(&strider, &[a, b]).await?;

        assert!(all_validated(&reports[0]));
        assert!(is_validated(&reports[1].families[0]));
        let failure = reports[1].families[1]
            .failure()
            .cloned()
            .expect("B's root manifest node was removed");
        assert_eq!(
            (failure.stage, failure.class, failure.object),
            (
                WalkStage::Load,
                "missing",
                Some(ObjectId::of::<Repo, _>(&root_b))
            )
        );
        assert!(
            !reports[1].families[1]
                .validated
                .contains(&ObjectId::of::<Repo, _>(&root_b))
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn missing_changeset_fails_only_core(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let (repo, blobstore) = test_repo(fb).await?;
        let (_a, b) = two_commits(&ctx, &repo).await?;
        // C deletes nothing, so its deleted manifest is B's and C adds only
        // its mapping. Walked as a root, it would add all of B's nodes.
        let c = CreateCommitContext::new(&ctx, &*repo, vec![b])
            .add_file("live", "live-c")
            .commit()
            .await?;
        derive(&ctx, &repo, c).await?;
        unlink(&repo, &blobstore, c).await?;
        let strider = strider(&ctx, &repo, &["core", "deleted-manifest"]);

        let reports = audit(&strider, &[b, c]).await?;

        assert!(all_validated(&reports[0]));
        let core = &reports[1].families[0];
        let failure = core.failure().cloned().expect("C's changeset was removed");
        assert_eq!(
            (failure.stage, failure.class, failure.object),
            (
                WalkStage::Load,
                "missing",
                Some(ObjectId::of::<Repo, _>(&c))
            )
        );
        assert!(core.validated.is_empty());
        let deleted_manifest = &reports[1].families[1];
        assert!(is_validated(deleted_manifest), "{deleted_manifest:?}");
        assert_eq!(
            deleted_manifest.validated,
            vec![ObjectId::of::<Repo, _>(
                &Mapping::<RootDeletedManifestV2Id>::new(c)
            )]
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn failed_parent_lookup_leaves_every_family_incomplete(fb: FacebookInit) -> Result<()> {
        // Given a commit that is not in the commit graph
        let ctx = CoreContext::test_mock(fb);
        let (repo, _blobstore) = test_repo(fb).await?;
        let unknown = ChangesetId::from_bytes([1; 32])?;
        let strider = strider(&ctx, &repo, &["core", "deleted-manifest"]);

        // When it is audited
        let reports = audit(&strider, &[unknown]).await?;

        // Then no family runs, and each is incomplete at `enumerate`
        assert!(reports[0].is_incomplete());
        assert_eq!(
            reports[0]
                .families
                .iter()
                .map(|family| {
                    let failure = family.failure().expect("the family is incomplete");
                    (
                        family.family,
                        failure.stage,
                        failure.class,
                        failure.object.clone(),
                    )
                })
                .collect::<Vec<_>>(),
            vec![
                ("core", WalkStage::Enumerate, "unreadable", None),
                ("deleted-manifest", WalkStage::Enumerate, "unreadable", None),
            ]
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn commit_stream_error_ends_the_reports(fb: FacebookInit) -> Result<()> {
        // Given a commit stream that fails before a valid commit
        let ctx = CoreContext::test_mock(fb);
        let (repo, _blobstore) = test_repo(fb).await?;
        let (a, _b) = two_commits(&ctx, &repo).await?;
        let strider = strider(&ctx, &repo, &["core"]);
        let commits = stream::iter([Err(anyhow::anyhow!("lost the commit source")), Ok(a)]);

        // When it is audited
        let reports = strider.audit(commits).collect::<Vec<_>>().await;

        // Then the error is the last item
        assert_eq!(reports.len(), 1, "{reports:?}");
        assert!(reports[0].is_err());
        Ok(())
    }

    /// A family that emits the commit's changeset twice.
    struct Repeats;

    impl Walkable<Repo> for Repeats {
        const NAME: &'static str = "repeats";
        const DERIVED_TYPE: Option<DerivableType> = None;

        fn walk_new(
            _ctx: LoadContext,
            _repo: Arc<Repo>,
            commit: Arc<CommitInfo>,
            _max_loads: NonZeroUsize,
        ) -> WalkStream<Repo> {
            let changeset = commit.changeset_id;
            stream::iter([changeset, changeset].map(|id| Ok(StoredObject::new(id)))).boxed()
        }
    }

    /// A family whose walk panics on root commits only.
    struct PanicsOnRoots;

    impl Walkable<Repo> for PanicsOnRoots {
        const NAME: &'static str = "panics_on_roots";
        const DERIVED_TYPE: Option<DerivableType> = None;

        fn walk_new(
            _ctx: LoadContext,
            _repo: Arc<Repo>,
            commit: Arc<CommitInfo>,
            _max_loads: NonZeroUsize,
        ) -> WalkStream<Repo> {
            assert!(!commit.parents.is_empty(), "the walk panicked");
            stream::iter([Ok(StoredObject::new(commit.changeset_id))]).boxed()
        }
    }

    fn strider_of(ctx: &CoreContext, repo: &Arc<Repo>, family: Family<Repo>) -> Arc<Strider<Repo>> {
        strider_with(ctx, repo, family, 2)
    }

    fn strider_with(
        ctx: &CoreContext,
        repo: &Arc<Repo>,
        family: Family<Repo>,
        max_concurrency: usize,
    ) -> Arc<Strider<Repo>> {
        Arc::new(Strider::new(
            ctx.clone(),
            repo.clone(),
            vec![family],
            HashSet::new(),
            Operation::Validate,
            StriderLimits {
                max_concurrency: NonZeroUsize::new(max_concurrency).unwrap(),
                max_loads_per_commit: NonZeroUsize::new(4).unwrap(),
            },
            true,
            Arc::new(LoadTags::default()),
        ))
    }

    #[mononoke::fbinit_test]
    async fn repeated_object_is_validated_each_time_it_is_emitted(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let (repo, _blobstore) = test_repo(fb).await?;
        let a = CreateCommitContext::new_root(&ctx, &*repo)
            .add_file("one", "one")
            .commit()
            .await?;
        let strider = strider_of(&ctx, &repo, Family::of::<Repeats>());

        let reports = audit(&strider, &[a]).await?;

        assert!(all_validated(&reports[0]));
        assert_eq!(
            reports[0].families[0].validated,
            vec![ObjectId::of::<Repo, _>(&a), ObjectId::of::<Repo, _>(&a)]
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn a_panicked_audit_does_not_stop_the_others(fb: FacebookInit) -> Result<()> {
        // Given a root commit whose audit panics and a child whose audit does
        // not, audited one at a time with the root first
        let ctx = CoreContext::test_mock(fb);
        let (repo, _blobstore) = test_repo(fb).await?;
        let a = CreateCommitContext::new_root(&ctx, &*repo)
            .add_file("a", "a")
            .commit()
            .await?;
        let b = CreateCommitContext::new(&ctx, &*repo, vec![a])
            .add_file("b", "b")
            .commit()
            .await?;
        let strider = strider_with(&ctx, &repo, Family::of::<PanicsOnRoots>(), 1);

        // When both are audited
        let results = strider
            .audit(stream::iter([Ok(a), Ok(b)]))
            .collect::<Vec<_>>()
            .await;

        // Then the panic is yielded as a panic that names its changeset, and
        // the child is still audited
        let [Err(error), Ok(report)] = results.as_slice() else {
            panic!("expected the panic, then the child's report: {results:?}");
        };
        assert!(is_panic(error), "{error:#}");
        assert!(format!("{error:#}").contains(&a.to_string()), "{error:#}");
        assert_eq!(report.changeset_id, b);
        assert!(all_validated(report));
        Ok(())
    }

    static HANGING_LOADS: AtomicUsize = AtomicUsize::new(0);
    static HANGING_LOADS_DROPPED: AtomicUsize = AtomicUsize::new(0);

    /// Counts a hanging load that was dropped.
    struct CountsDrop;

    impl Drop for CountsDrop {
        fn drop(&mut self) {
            HANGING_LOADS_DROPPED.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// An object whose load never finishes.
    #[derive(Clone, Debug, PartialEq, Eq, Hash)]
    struct Hangs(u32);

    impl fmt::Display for Hangs {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(formatter, "{}", self.0)
        }
    }

    #[async_trait]
    impl Validatable<Repo> for Hangs {
        const NAME: &'static str = "Hangs";

        type Loaded = ();

        async fn load(&self, _ctx: &TaggedContext, _repo: &Repo) -> Result<(), LoadError> {
            HANGING_LOADS.fetch_add(1, Ordering::SeqCst);
            let _dropped = CountsDrop;
            future::pending().await
        }

        fn storage_key(&self, _repo: &Repo) -> Option<String> {
            None
        }
    }

    /// A family that adds eight hanging objects to every commit.
    struct HangingFamily;

    impl Walkable<Repo> for HangingFamily {
        const NAME: &'static str = "hangs";
        const DERIVED_TYPE: Option<DerivableType> = None;

        fn walk_new(
            _ctx: LoadContext,
            _repo: Arc<Repo>,
            _commit: Arc<CommitInfo>,
            _max_loads: NonZeroUsize,
        ) -> WalkStream<Repo> {
            stream::iter((0..8).map(|n| Ok(StoredObject::new(Hangs(n))))).boxed()
        }
    }

    /// Gives the runtime turns until `done` holds, or a bounded number of
    /// turns have passed; returns whether it holds.
    async fn turns_until(done: impl Fn() -> bool) -> bool {
        for _ in 0..10_000 {
            if done() {
                return true;
            }
            mononoke::spawn_task(async {})
                .await
                .expect("an empty task finishes");
        }
        done()
    }

    #[mononoke::fbinit_test]
    async fn dropping_the_reports_ends_the_audits_already_started(fb: FacebookInit) -> Result<()> {
        // Given two commits whose audits are loading objects that never
        // finish loading, four at a time each
        let ctx = CoreContext::test_mock(fb);
        let (repo, _blobstore) = test_repo(fb).await?;
        let (a, b) = two_commits(&ctx, &repo).await?;
        let strider = strider_with(&ctx, &repo, Family::of::<HangingFamily>(), 2);
        let mut reports = Box::pin(strider.audit(stream::iter([Ok(a), Ok(b)])));
        let started = turns_until(|| HANGING_LOADS.load(Ordering::SeqCst) == 8);
        match future::select(reports.next(), Box::pin(started)).await {
            future::Either::Right((true, _)) => {}
            _ => panic!("both audits should be loading"),
        }

        // When the reports are dropped
        drop(reports);

        // Then each load in flight is dropped, and none starts after
        assert!(
            turns_until(|| HANGING_LOADS_DROPPED.load(Ordering::SeqCst) == 8).await,
            "{} of the 8 loads were dropped",
            HANGING_LOADS_DROPPED.load(Ordering::SeqCst)
        );
        assert_eq!(HANGING_LOADS.load(Ordering::SeqCst), 8);
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn absent_own_mapping_is_counted_not_failed(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let (repo, _blobstore) = test_repo(fb).await?;
        let a = CreateCommitContext::new_root(&ctx, &*repo)
            .add_file("a", "a")
            .commit()
            .await?;
        derive(&ctx, &repo, a).await?;
        // B is never derived.
        let b = CreateCommitContext::new(&ctx, &*repo, vec![a])
            .delete_file("a")
            .commit()
            .await?;
        let strider = strider(&ctx, &repo, &["core", "deleted-manifest"]);

        let reports = audit(&strider, &[a, b]).await?;

        assert!(all_validated(&reports[0]));
        assert!(is_validated(&reports[1].families[0]));
        assert!(matches!(
            reports[1].families[1].status,
            FamilyStatus::NotDerived
        ));
        assert!(reports[1].families[1].validated.is_empty());
        assert!(!reports[1].is_incomplete());
        Ok(())
    }

    /// A merge of two branches that each delete a file and add or change one,
    /// with unodes and deleted manifests derived for every commit. The merge
    /// gives `both/c` content neither parent has, so its unode is not reused.
    async fn derived_merge(ctx: &CoreContext, repo: &Repo) -> Result<ChangesetId> {
        let base = CreateCommitContext::new_root(ctx, repo)
            .add_file("left/a", "a")
            .add_file("right/b", "b")
            .add_file("both/c", "c")
            .commit()
            .await?;
        let left = CreateCommitContext::new(ctx, repo, vec![base])
            .delete_file("left/a")
            .add_file("left/d", "d")
            .commit()
            .await?;
        let right = CreateCommitContext::new(ctx, repo, vec![base])
            .delete_file("right/b")
            .add_file("both/c", "right")
            .commit()
            .await?;
        let merge = CreateCommitContext::new(ctx, repo, vec![left, right])
            .delete_file("left/a")
            .delete_file("right/b")
            .add_file("both/c", "merged")
            .add_file("merge", "m")
            .commit()
            .await?;
        for commit in [base, left, right, merge] {
            derive(ctx, repo, commit).await?;
            repo.repo_derived_data()
                .derive::<RootUnodeManifestId>(ctx, commit, DerivationPriority::LOW)
                .await?;
        }
        Ok(merge)
    }

    #[mononoke::fbinit_test]
    async fn every_read_auditing_a_merge_carries_a_live_tag(fb: FacebookInit) -> Result<()> {
        // Given a derived merge, in a repository whose store records each
        // read's tag
        let ctx = CoreContext::test_mock(fb);
        let (repo, _blobstore, tags, recorder) = tag_recording_repo(fb).await?;
        let merge = derived_merge(&ctx, &repo).await?;
        let strider = tagging_strider(
            &ctx,
            &repo,
            &["core", "deleted-manifest", "unode"],
            tags.clone(),
        );

        // When every family audits the merge
        recorder.recording.store(true, Ordering::SeqCst);
        let reports = audit(&strider, &[merge]).await?;
        recorder.recording.store(false, Ordering::SeqCst);

        // Then every read, of each family's objects and of the parents' data,
        // carried a tag, and no tag outlived its load
        assert!(all_validated(&reports[0]), "{reports:?}");
        assert_eq!(*recorder.untagged.lock().unwrap(), Vec::<String>::new());
        assert_eq!(
            recorder
                .tagged
                .lock()
                .unwrap()
                .iter()
                .map(|tag| tag.object.name())
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([
                "Changeset",
                "DeletedManifestV2",
                "FileContent",
                "Mapping",
                "UnodeFile",
                "UnodeManifest",
            ])
        );
        assert!(tags.is_empty());
        Ok(())
    }

    /// Checks that the storage key of every object the audit validated or
    /// read is a key its own load read, and returns the object types with a
    /// key and those without.
    fn storage_keys_read(
        repo: &Repo,
        recorder: &TagRecorder,
        reports: &[CommitReport],
    ) -> (BTreeSet<String>, BTreeSet<String>) {
        let prefix = repo.repo_identity().id().prefix();
        let mut keys_read: HashMap<ObjectId, HashSet<String>> = HashMap::new();
        for (object, key) in recorder.tagged_keys.lock().unwrap().iter() {
            keys_read
                .entry(object.clone())
                .or_default()
                .insert(key.clone());
        }
        let validated = reports
            .iter()
            .flat_map(|report| &report.families)
            .flat_map(|family| family.validated.iter().cloned());
        let objects = keys_read
            .keys()
            .cloned()
            .chain(validated)
            .collect::<HashSet<_>>();
        let mut with_key = BTreeSet::new();
        let mut without_key = BTreeSet::new();
        for object in &objects {
            match object.storage_key(repo) {
                Some(key) => {
                    let read = keys_read.get(object).cloned().unwrap_or_default();
                    assert!(
                        read.contains(&format!("{prefix}{key}")),
                        "{object} has key {key}, but its load read {read:?}"
                    );
                    with_key.insert(object.object_type().to_string());
                }
                None => {
                    without_key.insert(object.object_type().to_string());
                }
            }
        }
        (with_key, without_key)
    }

    #[mononoke::fbinit_test]
    async fn every_objects_storage_key_is_a_key_its_load_read(fb: FacebookInit) -> Result<()> {
        // Given a derived merge, in a repository whose store records the key
        // of each read with its tag
        let ctx = CoreContext::test_mock(fb);
        let (repo, _blobstore, tags, recorder) = tag_recording_repo(fb).await?;
        let merge = derived_merge(&ctx, &repo).await?;
        let strider = tagging_strider(
            &ctx,
            &repo,
            &families::<Repo>()
                .iter()
                .map(|family| family.name)
                .collect::<Vec<_>>(),
            tags,
        );

        // When every family audits the merge
        recorder.recording.store(true, Ordering::SeqCst);
        let reports = audit(&strider, &[merge]).await?;
        recorder.recording.store(false, Ordering::SeqCst);

        // Then the storage key of every object that was validated or read,
        // the parents' included, is a key its own load read from the store,
        // and the objects without one are the rows the SQL databases hold
        assert!(all_validated(&reports[0]), "{reports:?}");
        let (with_key, without_key) = storage_keys_read(&repo, &recorder, &reports);
        assert_eq!(
            with_key,
            BTreeSet::from([
                "Changeset".to_string(),
                "DeletedManifestV2".to_string(),
                "FileContent".to_string(),
                "Mapping:deleted_manifest".to_string(),
                "Mapping:unodes".to_string(),
                "UnodeFile".to_string(),
                "UnodeManifest".to_string(),
            ])
        );
        assert_eq!(without_key, BTreeSet::<String>::new());
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn sharded_nodes_have_the_keys_their_loads_read(fb: FacebookInit) -> Result<()> {
        // Given a root adding a directory large enough to store sharded-map
        // nodes, and a child deleting it
        let ctx = CoreContext::test_mock(fb);
        let (repo, _blobstore, tags, recorder) = tag_recording_repo(fb).await?;
        let names = (0..2100)
            .map(|index| format!("many/f{index:04}"))
            .collect::<Vec<_>>();
        let root = CreateCommitContext::new_root(&ctx, &*repo)
            .add_files(names.iter().map(|name| (name.as_str(), name.clone())))
            .commit()
            .await?;
        let deleted = names
            .iter()
            .fold(
                CreateCommitContext::new(&ctx, &*repo, vec![root]),
                |commit, name| commit.delete_file(name.as_str()),
            )
            .commit()
            .await?;
        let line = vec![root, deleted];
        derive(&ctx, &repo, deleted).await?;
        let strider = tagging_strider(&ctx, &repo, &["deleted-manifest"], tags);

        // When the deleted manifest of every commit is audited
        recorder.recording.store(true, Ordering::SeqCst);
        let reports = audit(&strider, &line).await?;
        recorder.recording.store(false, Ordering::SeqCst);

        // Then each object's storage key is a key its own load read, the
        // sharded-map nodes' among them
        assert!(reports.iter().all(all_validated), "{reports:?}");
        let (with_key, _without_key) = storage_keys_read(&repo, &recorder, &reports);
        assert!(
            with_key.contains("DeletedManifestV2MapNode"),
            "{with_key:?}"
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn excluded_commit_is_excluded_for_every_family_and_read_nothing(
        fb: FacebookInit,
    ) -> Result<()> {
        // Given A and its child B, with A excluded, and every family selected
        let ctx = CoreContext::test_mock(fb);
        let (repo, _blobstore, tags, recorder) = tag_recording_repo(fb).await?;
        let (a, b) = two_commits(&ctx, &repo).await?;
        let strider = Arc::new(Strider::new(
            ctx.clone(),
            repo.clone(),
            families(),
            HashSet::from([a]),
            Operation::Validate,
            StriderLimits {
                max_concurrency: NonZeroUsize::new(2).unwrap(),
                max_loads_per_commit: NonZeroUsize::new(4).unwrap(),
            },
            true,
            tags,
        ));

        // When A is audited
        recorder.recording.store(true, Ordering::SeqCst);
        let reports = audit(&strider, &[a]).await?;
        recorder.recording.store(false, Ordering::SeqCst);

        // Then every family reports it excluded, and nothing was read
        assert_eq!(reports[0].families.len(), families::<Repo>().len());
        assert!(
            reports[0]
                .families
                .iter()
                .all(|family| matches!(family.status, FamilyStatus::Excluded)),
            "{reports:?}"
        );
        let tagged = recorder.tagged.lock().unwrap().clone();
        assert!(tagged.is_empty(), "{tagged:?}");
        assert_eq!(*recorder.untagged.lock().unwrap(), Vec::<String>::new());

        // And B, audited next, runs normally
        let reports = audit(&strider, &[b]).await?;
        assert!(!reports[0].is_incomplete(), "{reports:?}");
        assert!(
            reports[0]
                .families
                .iter()
                .all(|family| !matches!(family.status, FamilyStatus::Excluded)),
            "{reports:?}"
        );
        assert!(is_validated(&reports[0].families[0]), "{reports:?}");
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn every_read_names_the_audited_commit(fb: FacebookInit) -> Result<()> {
        // Given a derived merge, in a repository whose store records each
        // read's tag
        let ctx = CoreContext::test_mock(fb);
        let (repo, _blobstore, tags, recorder) = tag_recording_repo(fb).await?;
        let merge = derived_merge(&ctx, &repo).await?;
        let strider = tagging_strider(&ctx, &repo, &["core", "unode"], tags);

        // When the merge is audited
        recorder.recording.store(true, Ordering::SeqCst);
        let reports = audit(&strider, &[merge]).await?;
        recorder.recording.store(false, Ordering::SeqCst);

        // Then every read, the parents' included, names the merge
        assert!(all_validated(&reports[0]), "{reports:?}");
        assert_eq!(
            recorder
                .tagged
                .lock()
                .unwrap()
                .iter()
                .map(|tag| tag.changeset_id)
                .collect::<HashSet<_>>(),
            HashSet::from([Some(merge)]),
        );
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn failed_read_is_tagged_and_leaves_no_tag(fb: FacebookInit) -> Result<()> {
        // Given B's parent A, whose deleted-manifest root node is removed
        let ctx = CoreContext::test_mock(fb);
        let (repo, blobstore, tags, recorder) = tag_recording_repo(fb).await?;
        let (a, b) = two_commits(&ctx, &repo).await?;
        let root_a = *derive(&ctx, &repo, a).await?.id();
        unlink(&repo, &blobstore, root_a).await?;
        let strider = tagging_strider(&ctx, &repo, &["core", "deleted-manifest"], tags.clone());

        // When B is audited
        recorder.recording.store(true, Ordering::SeqCst);
        let reports = audit(&strider, &[b]).await?;
        recorder.recording.store(false, Ordering::SeqCst);

        // Then the deleted manifest fails reading A's root, that read was
        // tagged, and no tag is left
        let failure = reports[0].families[1]
            .failure()
            .cloned()
            .expect("A's root manifest node was removed");
        assert_eq!(
            (failure.stage, failure.object),
            (WalkStage::Enumerate, Some(ObjectId::of::<Repo, _>(&root_a)))
        );
        assert_eq!(*recorder.untagged.lock().unwrap(), Vec::<String>::new());
        assert!(
            recorder
                .tagged
                .lock()
                .unwrap()
                .iter()
                .any(|tag| tag.object == ObjectId::of::<Repo, _>(&root_a))
        );
        assert!(tags.is_empty());
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn reads_of_sharded_deleted_manifests_carry_live_tags(fb: FacebookInit) -> Result<()> {
        // Given B deletes 2,100 files from `many`, so its node stores them in
        // sharded-map nodes; C restores one, changing one shard; and D
        // restores 200, so its node holds the rest itself
        let ctx = CoreContext::test_mock(fb);
        let (repo, _blobstore, tags, recorder) = tag_recording_repo(fb).await?;
        let names = (0..2100)
            .map(|index| format!("many/f{index:04}"))
            .collect::<Vec<_>>();
        let a = CreateCommitContext::new_root(&ctx, &*repo)
            .add_files(names.iter().map(|name| (name.as_str(), name.clone())))
            .commit()
            .await?;
        let b = names
            .iter()
            .fold(
                CreateCommitContext::new(&ctx, &*repo, vec![a]),
                |commit, name| commit.delete_file(name.as_str()),
            )
            .commit()
            .await?;
        let c = CreateCommitContext::new(&ctx, &*repo, vec![b])
            .add_file("many/f0000", "back")
            .commit()
            .await?;
        let d = names[..200]
            .iter()
            .fold(
                CreateCommitContext::new(&ctx, &*repo, vec![b]),
                |commit, name| commit.add_file(name.as_str(), "back"),
            )
            .commit()
            .await?;
        for commit in [a, b, c, d] {
            derive(&ctx, &repo, commit).await?;
        }
        let strider = tagging_strider(&ctx, &repo, &["deleted-manifest"], tags.clone());

        // When their deleted manifests are audited
        recorder.recording.store(true, Ordering::SeqCst);
        let reports = audit(&strider, &[b, c, d]).await?;
        recorder.recording.store(false, Ordering::SeqCst);

        // Then every read, including the parents' shards, carried a tag, and
        // no tag outlived its load
        assert!(reports.iter().all(all_validated), "{reports:?}");
        assert_eq!(*recorder.untagged.lock().unwrap(), Vec::<String>::new());
        assert!(
            recorder
                .tagged
                .lock()
                .unwrap()
                .iter()
                .any(|tag| tag.object.name() == "DeletedManifestV2MapNode")
        );
        assert!(tags.is_empty());
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn no_family_reads_storage_untagged(fb: FacebookInit) -> Result<()> {
        // Given a derived merge, in a repository whose store's reads go to
        // the sampler Strider installs
        let ctx = CoreContext::test_mock(fb);
        let (repo, _blobstore, tags, sampler) = read_sampling_repo(fb).await?;
        let merge = derived_merge(&ctx, &repo).await?;
        let strider = tagging_strider(
            &ctx,
            &repo,
            &["core", "deleted-manifest", "unode"],
            tags.clone(),
        );

        // Creating and deriving the commits read outside the audit.
        let outside_before = sampler.reads_outside_audit();

        // When every family audits the merge
        let reports = audit(&strider, &[merge]).await?;

        // Then every read the audit made was tagged, and each family's types
        // were counted
        assert!(all_validated(&reports[0]), "{reports:?}");
        assert_eq!(sampler.untagged_reads(), 0);
        assert_eq!(sampler.reads_outside_audit(), outside_before);
        assert_eq!(
            totals_by_name(&sampler).into_keys().collect::<Vec<_>>(),
            vec![
                "Changeset",
                "DeletedManifestV2",
                "FileContent",
                "Mapping:deleted_manifest",
                "Mapping:unodes",
                "UnodeFile",
                "UnodeManifest",
            ]
        );
        assert!(tags.is_empty());
        Ok(())
    }

    #[mononoke::fbinit_test]
    async fn read_totals_count_each_types_keys_and_bytes(fb: FacebookInit) -> Result<()> {
        // Given a root commit adding two files, in a repository whose store's
        // reads go to the sampler
        let ctx = CoreContext::test_mock(fb);
        let (repo, blobstore, tags, sampler) = read_sampling_repo(fb).await?;
        let a = CreateCommitContext::new_root(&ctx, &*repo)
            .add_file("one", "first")
            .add_file("two", "second file")
            .commit()
            .await?;
        let changeset = Loadable::load(&a, &ctx, repo.repo_blobstore()).await?;
        let contents = changeset
            .simplified_file_changes()
            .filter_map(|(_path, change)| Some(change?.content_id()))
            .collect::<Vec<_>>();
        let stored_size = |key: String| {
            let blobstore = blobstore.clone();
            let ctx = ctx.clone();
            let key = format!("{}{}", repo.repo_identity().id().prefix(), key);
            async move {
                let data = blobstore.get(&ctx, &key).await?;
                Ok::<_, anyhow::Error>(data.expect("the commit's blobs are stored").len() as u64)
            }
        };
        let changeset_bytes = stored_size(a.blobstore_key()).await?;
        let mut content_bytes = 0;
        for content in &contents {
            content_bytes += stored_size(content.blobstore_key()).await?;
        }
        let strider = tagging_strider(&ctx, &repo, &["core"], tags);

        // When core audits it
        let reports = audit(&strider, &[a]).await?;

        // Then the changeset, read once, and both contents are counted with
        // their sizes, uncompressed as Memblob stores them
        assert!(all_validated(&reports[0]), "{reports:?}");
        assert_eq!(
            totals_by_name(&sampler),
            BTreeMap::from([
                (
                    "Changeset".to_string(),
                    ReadTotals {
                        keys: 1,
                        bytes: changeset_bytes,
                    }
                ),
                (
                    "FileContent".to_string(),
                    ReadTotals {
                        keys: 2,
                        bytes: content_bytes,
                    }
                ),
            ])
        );
        Ok(())
    }

    static PROBES_LOADING: AtomicUsize = AtomicUsize::new(0);
    static PROBES_LOADING_PEAK: AtomicUsize = AtomicUsize::new(0);

    /// An object whose load records how many probe loads overlap it.
    #[derive(Clone, Debug, PartialEq, Eq, Hash)]
    struct Probe(u32);

    impl fmt::Display for Probe {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(formatter, "{}", self.0)
        }
    }

    #[async_trait]
    impl Validatable<Repo> for Probe {
        const NAME: &'static str = "Probe";

        type Loaded = ();

        async fn load(&self, _ctx: &TaggedContext, _repo: &Repo) -> Result<(), LoadError> {
            let loading = PROBES_LOADING.fetch_add(1, Ordering::SeqCst) + 1;
            PROBES_LOADING_PEAK.fetch_max(loading, Ordering::SeqCst);
            let mut yielded = false;
            future::poll_fn(|cx| {
                if yielded {
                    Poll::Ready(())
                } else {
                    yielded = true;
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            })
            .await;
            PROBES_LOADING.fetch_sub(1, Ordering::SeqCst);
            Ok(())
        }

        fn storage_key(&self, _repo: &Repo) -> Option<String> {
            None
        }
    }

    /// A family that adds eight probes to every commit.
    struct Probes;

    impl Walkable<Repo> for Probes {
        const NAME: &'static str = "probes";
        const DERIVED_TYPE: Option<DerivableType> = None;

        fn walk_new(
            _ctx: LoadContext,
            _repo: Arc<Repo>,
            _commit: Arc<CommitInfo>,
            _max_loads: NonZeroUsize,
        ) -> WalkStream<Repo> {
            stream::iter((0..8).map(|n| Ok(StoredObject::new(Probe(n))))).boxed()
        }
    }

    #[mononoke::fbinit_test]
    async fn one_commit_loads_up_to_its_limit_at_once(fb: FacebookInit) -> Result<()> {
        let ctx = CoreContext::test_mock(fb);
        let (repo, _blobstore) = test_repo(fb).await?;
        let a = CreateCommitContext::new_root(&ctx, &*repo)
            .add_file("a", "a")
            .commit()
            .await?;
        let strider = Arc::new(Strider::new(
            ctx,
            repo,
            vec![Family::of::<Probes>()],
            HashSet::new(),
            Operation::Validate,
            StriderLimits {
                max_concurrency: NonZeroUsize::new(1).unwrap(),
                max_loads_per_commit: NonZeroUsize::new(4).unwrap(),
            },
            true,
            Arc::new(LoadTags::default()),
        ));

        let reports = audit(&strider, &[a]).await?;

        assert!(all_validated(&reports[0]));
        assert_eq!(reports[0].families[0].validated.len(), 8);
        assert_eq!(PROBES_LOADING_PEAK.load(Ordering::SeqCst), 4);
        Ok(())
    }
}
