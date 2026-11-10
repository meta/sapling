/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import type {StackPullRequestFragment} from './generated/graphql';

import {useCommand} from './KeyboardShortcuts';
import PullRequestStackItem from './PullRequestStackItem';
import {gitHubPullRequestIDAtom, stackedPullRequestFragmentsAtom} from './jotai';
import useNavigateToPullRequest from './useNavigateToPullRequest';
import {ArrowDownIcon, ArrowUpIcon} from '@primer/octicons-react';
import {ActionList, ActionMenu, ButtonGroup, IconButton} from '@primer/react';
import {useAtomValue} from 'jotai';
import {loadable} from 'jotai/utils';
import {useCallback, useEffect, useState} from 'react';

const loadableStackedPullRequestFragmentsAtom = loadable(stackedPullRequestFragmentsAtom);

export default function PullRequestStack(): React.ReactElement | null {
  const navigateToPullRequest = useNavigateToPullRequest();
  const pullRequestNumber = useAtomValue(gitHubPullRequestIDAtom);

  // Our goal is to ensure this component *always* renders synchronously [and
  // never suspends] to support the common case where the user is toggling the
  // arrows in the stack selector, in which case we don't want it to temporarily
  // disappear while we are loading the pull request for the newly selected
  // item in the list. To that end, we employ the following strategy:
  //
  // - If `stackedPullRequestFragmentsAtom` is available immediately, assume it is
  //   the source of truth and use it.
  // - Whenever we receive a value for `stackedPullRequestFragmentsAtom`, stuff it
  //   in the state for this component via `setLastStack()`.
  // - If `stackedPullRequestFragmentsAtom` is not available immediately, use
  //   `lastStack` if both of the following are true:
  //   - `lastStack` is non-null
  //   - `pullRequestNumber` is in `lastStack`.
  // - Otherwise, we assume that `lastStack` is stale (or the pull request is
  //   not part of a stack), in which case we do not render anything at all.
  const stackLoadable = useAtomValue(loadableStackedPullRequestFragmentsAtom);
  const [lastStack, setLastStack] = useState<StackPullRequestFragment[] | null>(null);
  const availableStack = stackLoadable.state === 'hasData' ? stackLoadable.data : null;
  useEffect(() => {
    if (availableStack != null) {
      setLastStack(availableStack);
    }
  }, [availableStack, setLastStack]);

  const stack = availableStack ?? lastStack;
  const index = stack != null ? stack.findIndex(({number}) => number === pullRequestNumber) : -1;

  const onNavigate = useCallback(
    (index: number) => {
      if (stack == null || index === -1) {
        // The user may have clicked a link in a comment or pull request body
        // that took them to a pull request that is part of a separate stack,
        // in which case availableStack may be non-null, but index is -1.
        return;
      }

      const pullRequest = stack[index];
      if (pullRequest != null) {
        navigateToPullRequest(pullRequest.number);
      }
    },
    [navigateToPullRequest, stack],
  );

  useCommand('NextInStack', () => {
    if (stack == null || index === -1) {
      return;
    }
    if (index > 0) {
      onNavigate(index - 1);
    }
  });
  useCommand('PreviousInStack', () => {
    if (stack == null || index === -1) {
      return;
    }
    if (index < stack.length - 1) {
      onNavigate(index + 1);
    }
  });

  if (
    // In this case, we have nothing we can possibly show the user.
    stack == null ||
    // Note that if availableStack is non-null but index is -1, then we are in a
    // weird state where the pull request body describes a stack that this
    // pull request is not part of, so do not show the dropdown.
    index === -1
  ) {
    return null;
  }

  // In this case, the pull request does not appear to be part of a stack.
  const total = stack.length;
  if (total === 0) {
    return null;
  }

  const hasPrev = index < total - 1;
  const hasNext = index > 0;

  return (
    <ButtonGroup>
      <ActionMenu>
        <ActionMenu.Button sx={{display: 'inline-block'}}>
          Pull Request {total - index} of {total}
        </ActionMenu.Button>
        <ActionMenu.Overlay width="xxlarge">
          <ActionList selectionVariant="single">
            {stack.map((pullRequest, stackIndex) => (
              <PullRequestStackItem
                key={pullRequest.number}
                isSelected={index === stackIndex}
                {...pullRequest}
              />
            ))}
          </ActionList>
        </ActionMenu.Overlay>
      </ActionMenu>
      <IconButton disabled={!hasPrev} icon={ArrowDownIcon} onClick={() => onNavigate(index + 1)} aria-label="Prev">
        Prev
      </IconButton>
      <IconButton disabled={!hasNext} icon={ArrowUpIcon} onClick={() => onNavigate(index - 1)} aria-label="Next">
        Next
      </IconButton>
    </ButtonGroup>
  );
}
