/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashSet;
use std::sync::Arc;

use context::CoreContext;
use ephemeral_blobstore::Bubble;
use ephemeral_blobstore::EphemeralOnlyChangesetStorage;
use mononoke_types::ChangesetId;

use super::DerivationAssigner;
use super::DerivationAssignment;
use super::DerivedDataManager;
use super::DerivedDataManagerInner;
use super::SecondaryManagerData;

struct BubbleAssigner {
    ephemeral_only_changesets_storage: Arc<EphemeralOnlyChangesetStorage>,
}

#[async_trait::async_trait]
impl DerivationAssigner for BubbleAssigner {
    async fn assign(
        &self,
        ctx: &CoreContext,
        cs_ids: Vec<ChangesetId>,
    ) -> anyhow::Result<DerivationAssignment> {
        let in_bubble = self
            .ephemeral_only_changesets_storage
            .known_changesets(ctx, cs_ids.clone())
            .await?
            .into_iter()
            .collect::<HashSet<_>>();
        let (in_bubble, not_in_bubble) = cs_ids
            .into_iter()
            .partition(|cs_id| in_bubble.contains(cs_id));
        Ok(DerivationAssignment {
            primary: not_in_bubble,
            secondary: in_bubble,
        })
    }
}

impl DerivedDataManager {
    pub fn for_bubble(self, bubble: Bubble) -> Self {
        let commit_graph = Arc::new(bubble.commit_graph(
            self.repo_id(),
            self.repo_blobstore().clone(),
            self.commit_graph(),
        ));
        let ephemeral_only_changesets_storage = Arc::new(
            bubble.ephemeral_only_changesets_storage(self.repo_id(), self.repo_blobstore().clone()),
        );
        let wrapped_blobstore = bubble.wrap_repo_blobstore(self.inner.repo_blobstore.clone());
        let mut derivation_context = self.inner.derivation_context.clone();
        derivation_context.bonsai_git_mapping = None;
        derivation_context.bonsai_hg_mapping =
            derivation_context
                .bonsai_hg_mapping
                .take()
                .map(|persistent| {
                    bubble.bonsai_hg_mapping(self.repo_id(), self.repo_name(), persistent)
                });
        derivation_context.filenodes = None;
        derivation_context.blobstore = wrapped_blobstore.boxed();
        let mut scuba = self.inner.scuba.clone();
        scuba.add("bubble_id", bubble.bubble_id());

        Self {
            inner: Arc::new(DerivedDataManagerInner {
                secondary: Some(SecondaryManagerData {
                    manager: Self {
                        inner: Arc::new(DerivedDataManagerInner {
                            bubble_id: Some(bubble.bubble_id()),
                            commit_graph: commit_graph.clone(),
                            repo_blobstore: wrapped_blobstore,
                            derivation_context,
                            scuba,
                            ..self.inner.as_ref().clone()
                        }),
                    },
                    assigner: Arc::new(BubbleAssigner {
                        ephemeral_only_changesets_storage,
                    }),
                }),
                bubble_id: Some(bubble.bubble_id()),
                commit_graph,
                ..self.inner.as_ref().clone()
            }),
        }
    }
}
