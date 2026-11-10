/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import type {UserFragment} from './generated/graphql';

import FieldLabel from './FieldLabel';
import RepoAssignableUsersInput from './RepoAssignableUsersInput';
import {
  gitHubClientAtom,
  gitHubPullRequestAtom,
  gitHubPullRequestReviewersAtom,
  gitHubPullRequestViewerCanUpdateAtom,
  gitHubUsernameAtom,
  notificationMessageAtom,
} from './jotai';
import useRefreshPullRequest from './useRefreshPullRequest';
import {GearIcon} from '@primer/octicons-react';
import {ActionMenu, AvatarToken, Box, Button} from '@primer/react';
import {useAtom, useAtomValue, useSetAtom} from 'jotai';
import {loadable} from 'jotai/utils';
import {useCallback, useEffect, useMemo} from 'react';

export default function PullRequestReviewers(): React.ReactElement {
  const refreshPullRequest = useRefreshPullRequest();
  const pullRequest = useAtomValue(gitHubPullRequestAtom);
  const [pullRequestReviewers, setPullRequestReviewers] = useAtom(gitHubPullRequestReviewersAtom);
  const viewerCanUpdate = useAtomValue(gitHubPullRequestViewerCanUpdateAtom);
  const setNotification = useSetAtom(notificationMessageAtom);

  // Get username via loadable to handle async state
  const loadableUsernameAtom = useMemo(() => loadable(gitHubUsernameAtom), []);
  const usernameLoadable = useAtomValue(loadableUsernameAtom);
  const username = usernameLoadable.state === 'hasData' ? usernameLoadable.data : null;

  // Client is already loaded by the time we're modifying reviewers
  const client = useAtomValue(gitHubClientAtom);

  // Initialize pullRequestReviewers state using pullRequest once it is available.
  useEffect(() => {
    if (pullRequest != null) {
      // From https://docs.github.com/en/rest/pulls/review-requests:
      //
      // > Gets the users or teams whose review is requested for a pull request.
      // > Once a requested reviewer submits a review, they are no longer
      // > considered a requested reviewer.
      //
      // As such, both the `reviews` and `reviewRequests` fields must be
      // consulted to get the full list of reviewers.
      const reviewers: Array<UserFragment> = [];
      const reviewerIDs: Set<string> = new Set();
      for (const user of pullRequest.reviews?.nodes ?? []) {
        const author = user?.author;
        if (author?.__typename === 'User' && author.login !== username) {
          if (!reviewerIDs.has(author.id)) {
            reviewerIDs.add(author.id);
            reviewers.push(author);
          }
        }
      }
      for (const node of pullRequest.reviewRequests?.nodes ?? []) {
        const reviewer = node?.requestedReviewer;
        if (reviewer?.__typename === 'User') {
          if (!reviewerIDs.has(reviewer.id)) {
            reviewerIDs.add(reviewer.id);
            reviewers.push(reviewer);
          }
        }
      }

      setPullRequestReviewers({reviewers, reviewerIDs});
    }
  }, [pullRequest, setPullRequestReviewers, username]);

  const updateReviewers = useCallback(
    async (user: UserFragment, isExisting: boolean) => {
      if (client == null) {
        return Promise.reject('client not found');
      }

      const pullRequestId = pullRequest?.id;
      if (pullRequestId == null) {
        return Promise.reject('pull request not found');
      }

      const previousReviewers = pullRequestReviewers;
      try {
        // When adding or removing a reviewer, optimistically update
        // pullRequestReviewers and the UI instead of waiting for the respective
        // API call to return.
        const reviewerIDs = new Set(pullRequestReviewers.reviewerIDs);
        let reviewers: UserFragment[];
        if (!isExisting) {
          reviewers = pullRequestReviewers.reviewers.concat(user);
          reviewers.sort((a, b) => a.login.localeCompare(b.login));
          reviewerIDs.add(user.id);
        } else {
          reviewers = pullRequestReviewers.reviewers.filter(({id}) => user.id !== id);
          reviewerIDs.delete(user.id);
        }
        setPullRequestReviewers({reviewers, reviewerIDs});
        await client.requestReviews({
          pullRequestId,
          userIds: [...reviewerIDs],
        });
        refreshPullRequest();
      } catch (e) {
        // If there is an error, roll back the update by resetting
        // pullRequestReviewers to its previous value.
        setPullRequestReviewers(previousReviewers);
        const message = e instanceof Error ? e.message : String(e);
        setNotification({
          type: 'error',
          message: `Failed to update reviewers: ${message}`,
        });
      }
    },
    [client, pullRequest, pullRequestReviewers, refreshPullRequest, setPullRequestReviewers, setNotification],
  );

  const label = !viewerCanUpdate ? (
    <FieldLabel label="Reviewers" />
  ) : (
    <ActionMenu>
      <ActionMenu.Anchor>
        <Button trailingIcon={GearIcon}>Reviewers</Button>
      </ActionMenu.Anchor>
      <ActionMenu.Overlay width="medium">
        <RepoAssignableUsersInput
          existingUserIDs={pullRequestReviewers.reviewerIDs}
          onSelect={updateReviewers}
        />
      </ActionMenu.Overlay>
    </ActionMenu>
  );

  return (
    <Box display="flex" alignItems="center" gridGap={2}>
      {label}
      <Box display="flex" flexWrap="wrap" gridGap={1}>
        {pullRequestReviewers.reviewers.map(user => (
          <AvatarToken
            key={user.id}
            avatarSrc={user.avatarUrl}
            text={user.login}
            size="large"
            onRemove={!viewerCanUpdate ? undefined : () => updateReviewers(user, true)}
          />
        ))}
      </Box>
    </Box>
  );
}
