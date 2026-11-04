/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use futures::FutureExt;
use justknobs::test_helpers::JustKnobsInMemory;
use justknobs::test_helpers::KnobVal;
use justknobs::test_helpers::with_just_knobs_async;

use super::*;

fn comparison_knobs(enabled: bool) -> JustKnobsInMemory {
    JustKnobsInMemory::new(HashMap::from([(
        COMPARISON_KNOB.to_owned(),
        KnobVal::Bool(enabled),
    )]))
}

#[mononoke::test]
fn comparison_defaults_to_new_implementation() {
    assert!(
        use_new_comparison(&thrift::CommitRestrictedPathsChangesParams::default())
            .expect("default legacy request is valid")
    );
}

#[mononoke::fbinit_test]
async fn rollback_preserves_bonsai_target_access_and_original_empty_response(
    fb: FacebookInit,
) -> Result<()> {
    with_just_knobs_async(
        comparison_knobs(false),
        async move {
            assert!(
                !use_new_comparison(&thrift::CommitRestrictedPathsChangesParams::default())
                    .expect("rollback accepts the original request")
            );
            let base = create_test_changeset_with_acl_provider(
                fb,
                vec![("secret", "REPO_REGION:acl")],
                TestAclProvider::new(false),
            )
            .await?;
            let target_id =
                CreateCommitContext::new(base.ctx(), base.repo_ctx().repo(), vec![base.id()])
                    .add_file("secret/file", "restricted")
                    .add_file("public/file", "unrestricted")
                    .commit()
                    .await?;
            let target = base
                .repo_ctx()
                .changeset(target_id)
                .await?
                .context("target exists")?;
            let response = bonsai_changes_response(&target)
                .await
                .expect("original response");
            assert_eq!(response.changed_paths, vec!["public/file", "secret/file"]);
            assert_eq!(response.authorized_paths, vec!["public/file"]);
            assert_eq!(response.are_restricted, thrift::PathCoverage::SOME);
            assert_eq!(response.has_access, thrift::PathCoverage::SOME);
            assert_eq!(response.restriction_roots.len(), 1);
            assert_eq!(
                response.restriction_roots["secret/file"][0].acls,
                vec!["REPO_REGION:acl"]
            );
            assert_eq!(response.checked_both_sides, None);
            assert_eq!(response.last_path, None);
            assert_eq!(response.compact, None);

            let empty_id =
                CreateCommitContext::new(target.ctx(), target.repo_ctx().repo(), vec![target.id()])
                    .commit()
                    .await?;
            let empty = target
                .repo_ctx()
                .changeset(empty_id)
                .await?
                .context("empty commit exists")?;
            let response = bonsai_changes_response(&empty)
                .await
                .expect("original empty response");
            assert!(response.changed_paths.is_empty());
            assert!(response.authorized_paths.is_empty());
            assert!(response.restriction_roots.is_empty());
            assert_eq!(response.are_restricted, thrift::PathCoverage::NONE);
            assert_eq!(response.has_access, thrift::PathCoverage::NONE);
            assert_eq!(response.checked_both_sides, None);
            assert_eq!(
                changes_restriction_results_into_response(vec![]).has_access,
                thrift::PathCoverage::ALL
            );
            Ok(())
        }
        .boxed(),
    )
    .await
}

#[mononoke::fbinit_test]
async fn rollback_rejects_new_protocol_request_fields(_fb: FacebookInit) {
    with_just_knobs_async(
        comparison_knobs(false),
        async {
            for params in [
                thrift::CommitRestrictedPathsChangesParams {
                    format: Some(thrift::CommitRestrictedPathsChangesFormat::COMPACT),
                    ..Default::default()
                },
                thrift::CommitRestrictedPathsChangesParams {
                    other_commit_id: Some(thrift::CommitId::hg(vec![0; 20])),
                    ..Default::default()
                },
                thrift::CommitRestrictedPathsChangesParams {
                    ordered_params: Some(thrift::CommitRestrictedPathsChangesOrderedParams {
                        limit: 1,
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            ] {
                assert!(
                    use_new_comparison(&params).is_err(),
                    "rollback must not pretend to implement comparison requests"
                );
            }
        }
        .boxed(),
    )
    .await;
}
