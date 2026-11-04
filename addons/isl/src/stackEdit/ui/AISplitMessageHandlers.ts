/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import type {CommitRev} from '../common';
import type {PartiallySelectedDiffCommit} from '../diffSplitTypes';

import {List} from 'immutable';
import serverAPI from '../../ClientToServerAPI';
import {readAtom, subscribeAtom, writeAtom} from '../../jotaiUtils';
import {registerDisposable} from '../../utils';
import {applyDiffSplit} from '../diffSplit';
import {next} from '../revMath';
import {editingStackIntentionHashes, SplitRangeRecord, stackEditState} from './stackEditState';

// Helper function to apply the AI split commits to the stack edit state
function applyAISplitCommits(commits: PartiallySelectedDiffCommit[]) {
  // Get the current stack edit state
  const state = readAtom(stackEditState);

  if (state.history.state !== 'hasValue') {
    throw new Error('Stack edit state is not loaded');
  }

  const history = state.history.value;
  const commitStack = history.current.state;

  // When the intention is 'split', we're splitting a single commit
  const intention = state.intention;
  if (intention !== 'split') {
    throw new Error('Cannot apply AI split when not in split mode');
  }

  // In split mode, startRev is 1 and endRev is size-1
  const startRev = 1 as CommitRev;
  const endRev = (commitStack.size - 1) as CommitRev;

  // Extract a dense substack containing only the commit(s) to be split
  // For split mode with a single commit, this is just [startRev]
  const subStack = commitStack.denseSubStack(List([startRev]));

  // Apply the diff split to the first commit in the subStack (position 0 relative to subStack)
  const newSubStack = applyDiffSplit(subStack, 0 as CommitRev, commits);

  // Replace the [start, end+1] range with the new stack in the commit stack
  const newCommitStack = commitStack.applySubStack(startRev, next(endRev), newSubStack);

  // Calculate the split range for UI selection
  const endOffset = newCommitStack.size - commitStack.size;

  // The split commits start at position startRev (the first new split commit)
  // and end at position startRev + endOffset
  const startKey = newCommitStack.get(startRev)?.key ?? '';
  const endKey = newCommitStack.get(next(startRev, endOffset))?.key ?? '';
  const splitRange = SplitRangeRecord({startKey, endKey});

  // Update the state with the new split
  writeAtom(stackEditState, prev => {
    if (prev.history.state !== 'hasValue') {
      return prev;
    }

    return {
      ...prev,
      history: {
        state: 'hasValue' as const,
        value: prev.history.value.push(newCommitStack, {name: 'splitWithAI'}, {splitRange}),
      },
    };
  });
}

// Handle openSplitViewForCommit message - opens the split UI for a specific commit
// and optionally applies AI split commits after the stack is loaded
registerDisposable(
  serverAPI,
  serverAPI.onMessageOfType('openSplitViewForCommit', event => {
    // Open the split panel by setting the editing stack intention
    writeAtom(editingStackIntentionHashes, ['split', new Set([event.commitHash])]);

    // If commits are provided, wait for the stack to load and then apply the split
    if (event.commits && event.commits.length > 0) {
      const commitsToApply = event.commits;
      let hasApplied = false;
      const unsubscribe = subscribeAtom(stackEditState, () => {
        if (hasApplied) {
          return;
        }
        const state = readAtom(stackEditState);
        if (state.history.state === 'hasValue' && state.intention === 'split') {
          // Stack is loaded, apply the AI split commits
          hasApplied = true;
          unsubscribe();
          applyAISplitCommits(commitsToApply);
        }
      });
    }
  }),
);
