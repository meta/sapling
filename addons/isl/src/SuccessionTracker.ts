/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import type {Dag} from './dag/dag';
import type {SmartlogCommits} from './types';

import {atom} from 'jotai';
import {tracker} from './analytics';
import {MutationDag} from './dag/mutation_dag';
import {writeAtom} from './jotaiUtils';
import {registerCleanup} from './utils';

type Successions = Array<[oldHash: string, newHash: string]>;
type SuccessionCallback = (successions: Successions) => unknown;

/**
 * When a commit is amended or rebased or otherwise modified, the old commit
 * is marked obsolete and "succeeded" by a new commit.
 * Some state in the UI is keyed by hash, so a succession event can cause the UI
 * to show stale data. For example, if you select a commit and amend a commit earlier in the stack,
 * your selection will now disappear.
 *
 * To avoid this, we keep track of commits being succeeded, and any atom keyed on hashes
 * can listen to this event and update itself with the new oldHash -> newHash.
 *
 * Succession is tracked by looking at each new batch of commits we get, each of which may have
 * a closestPredecessors field. Technically, it's probably possible that a commit is succeeded twice
 * between results from `sl log`, which would cause us to miss a succession. We'll ignore this case for now,
 * and assume it's rare.
 *
 * Note that successions could also be detected on the server by stdout or other means from sl,
 * but by doing it on the client we know that all successions are dealt with at the exact moment the
 * UI state gets a new list of commits, reducing any race between succession events and new commits rendering.
 */
export class SuccessionTracker {
  private callbacks: Set<SuccessionCallback> = new Set();
  /**
   * Run a callback when a succession is detected for the first time.
   * Returns a dispose function.
   */
  public onSuccessions(cb: SuccessionCallback): () => void {
    this.callbacks.add(cb);
    return () => {
      this.callbacks.delete(cb);
    };
  }

  private seenHashes = new Set<string>();
  /**
   * Called once in the app each time a new batch of commits is fetched,
   * in order to find successions and run callbacks on them.
   */
  public findNewSuccessionsFromCommits(previousDag: Dag, commits: SmartlogCommits) {
    const successions: Successions = [];
    for (const commit of commits) {
      if (commit.phase === 'public') {
        continue;
      }

      const {hash: newHash, closestPredecessors: oldHashes} = commit;

      // Commits we've seen before should have already had their successions computed, so they are skipped

      // Commits we've never seen before, who have predecessors we've never seen are just entirely new commits
      // or from our first time fetching commits. Skip computing predecessors for these.

      // Commits we've *never* seen before, who have predecessors that we *have* seen before are actually successions.
      if (oldHashes != null && !this.seenHashes.has(newHash)) {
        for (const oldHash of oldHashes) {
          if (this.seenHashes.has(oldHash)) {
            // HACKY: When we see a succession, we want to persist data forward.
            // However, we've seen a bug where commit messages get mixed up between commits.
            // As a precaution, let's not consider commits that change their commit messages
            // to have different attached diffs.
            // There may be false positives from this, but they should be rare,
            // and the cost of successions wrong is relatively small:
            // commit messages and selection wouldn't be persisted correctly.
            // TODO: use this for debugging, then find a proper fix or legitimize this.
            const previousCommit = previousDag.get(oldHash);
            if (
              previousCommit != null &&
              previousCommit.diffId &&
              previousCommit.diffId !== commit.diffId
            ) {
              tracker.track('BuggySuccessionDetected', {
                extras: {
                  oldHash,
                  newHash,
                  old: previousCommit.title + '\n' + previousCommit.description,
                  new: commit.title + '\n' + commit.description,
                },
              });
              continue;
            }

            successions.push([oldHash, newHash]);
          }
        }
      }

      this.seenHashes.add(newHash);
    }

    if (successions.length > 0) {
      tracker.track('SuccessionsDetected', {extras: {successions}});
      for (const cb of this.callbacks) {
        cb(successions);
      }
    }
  }

  /** Clear all known hashes, useful for resetting between tests */
  public clear() {
    this.seenHashes.clear();
  }
}

export const successionTracker = new SuccessionTracker();

export const latestSuccessorsMapAtom = atom<MutationDag>(new MutationDag());

registerCleanup(
  successionTracker,
  successionTracker.onSuccessions(successions => {
    writeAtom(latestSuccessorsMapAtom, dag => {
      return dag.addMutations(successions);
    });
  }),
  import.meta.hot,
);
