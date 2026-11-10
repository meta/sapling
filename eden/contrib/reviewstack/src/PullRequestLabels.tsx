/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import type {LabelFragment} from './generated/graphql';

import FieldLabel from './FieldLabel';
import RepoLabelsInput from './RepoLabelsInput';
import {
  gitHubClientAtom,
  gitHubPullRequestAtom,
  gitHubPullRequestLabelsAtom,
  gitHubPullRequestViewerCanUpdateAtom,
  notificationMessageAtom,
} from './jotai';
import {GearIcon} from '@primer/octicons-react';
import {ActionMenu, Box, Button, IssueLabelToken} from '@primer/react';
import {useAtom, useAtomValue, useSetAtom} from 'jotai';
import {useCallback, useEffect, useMemo} from 'react';
import {notEmpty} from 'shared/utils';

export default function PullRequestLabels(): React.ReactElement {
  const pullRequest = useAtomValue(gitHubPullRequestAtom);
  const [pullRequestLabels, setPullRequestLabels] = useAtom(gitHubPullRequestLabelsAtom);
  const viewerCanUpdate = useAtomValue(gitHubPullRequestViewerCanUpdateAtom);
  const setNotification = useSetAtom(notificationMessageAtom);
  const existingLabelIDs = useMemo(
    () => new Set(pullRequestLabels.map(({id}) => id)),
    [pullRequestLabels],
  );

  // Client is already loaded by the time we're modifying labels
  const client = useAtomValue(gitHubClientAtom);

  // Initialize pullRequestLabels state using pullRequest once it is available.
  useEffect(() => {
    if (pullRequest != null) {
      const labels = (pullRequest.labels?.nodes ?? []).filter(notEmpty);
      setPullRequestLabels(labels);
    }
  }, [pullRequest, setPullRequestLabels]);

  const updateLabels = useCallback(
    async ({id, name, color}: LabelFragment, isExisting: boolean) => {
      if (client == null) {
        return Promise.reject('client not found');
      }

      const pullRequestId = pullRequest?.id;
      if (pullRequestId == null) {
        return Promise.reject('pull request not found');
      }

      const previousLabels = pullRequestLabels;
      try {
        // When adding or removing a label, optimistically update
        // pullRequestLabels and the UI instead of waiting for the respective
        // API call to return.
        if (!isExisting) {
          const labels = [...pullRequestLabels, {id, name, color}].sort((a, b) =>
            a.name.localeCompare(b.name),
          );
          setPullRequestLabels(labels);
          await client.addLabels({
            labelableId: pullRequestId,
            labelIds: [id],
          });
        } else {
          const labels = pullRequestLabels.filter(label => label.id !== id);
          setPullRequestLabels(labels);
          await client.removeLabels({
            labelableId: pullRequestId,
            labelIds: [id],
          });
        }
      } catch (e) {
        // If there is an error, roll back the update by resetting
        // pullRequestLabels to its previous value.
        setPullRequestLabels(previousLabels);
        const message = e instanceof Error ? e.message : String(e);
        setNotification({
          type: 'error',
          message: `Failed to update labels: ${message}`,
        });
      }
    },
    [client, pullRequest, pullRequestLabels, setPullRequestLabels, setNotification],
  );

  const label = !viewerCanUpdate ? (
    <FieldLabel label="Labels" />
  ) : (
    <ActionMenu>
      <ActionMenu.Anchor>
        <Button trailingIcon={GearIcon}>Labels</Button>
      </ActionMenu.Anchor>
      <ActionMenu.Overlay width="medium">
        <RepoLabelsInput existingLabelIDs={existingLabelIDs} onSelect={updateLabels} />
      </ActionMenu.Overlay>
    </ActionMenu>
  );

  return (
    <Box display="flex" alignItems="center" gridGap={2}>
      {label}
      <Box display="flex" flexWrap="wrap" gridGap={1}>
        {pullRequestLabels.map(({id, name, color}) => (
          <IssueLabelToken
            key={id}
            text={name}
            fillColor={`#${color}`}
            onRemove={!viewerCanUpdate ? undefined : () => updateLabels({id, name, color}, true)}
          />
        ))}
      </Box>
    </Box>
  );
}
