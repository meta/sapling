/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import type {Hash, WorktreeEntry} from './types';

import {atom} from 'jotai';
import {basename, guessPathSep, isHexHash, pathsAreIdentical} from 'shared/utils';
import {focusMode} from './atoms/FocusModeState';
import {atomFamilyWeak, localStorageBackedAtom} from './jotaiUtils';
import {repositoryInfo, worktreeInfoData} from './serverAPIState';

/** Whether to show worktree name labels ("You are here" label, checked-out-elsewhere badges). */
export const showWorktreeLabels = localStorageBackedAtom<boolean>(
  'isl.show-worktree-labels',
  true,
  {portable: true},
);

/**
 * `{info, worktreeInfo}` when worktree data has loaded -- `undefined` otherwise.
 */
const loadedWorktreeInfo = atom(get => {
  const info = get(repositoryInfo);
  const worktreeInfo = get(worktreeInfoData);
  if (info == null || worktreeInfo == null) {
    return undefined;
  }

  return {info, worktreeInfo};
});

/**
 * Like `loadedWorktreeInfo`, but only when there are sibling worktrees to show.
 * Shared gating for the read-only worktree features, which work in any repo
 * `sl worktree list` supports (EdenFS and native Git).
 */
const enabledWorktreeInfo = atom(get => {
  const loaded = get(loadedWorktreeInfo);
  if (loaded == null || loaded.worktreeInfo.worktrees.length <= 1) {
    return undefined;
  }
  return loaded;
});

/** Whether worktrees can be added, renamed, and removed, as reported by the server. */
export const worktreeMutationsSupported = atom(
  get => get(loadedWorktreeInfo)?.worktreeInfo.supportsMutation === true,
);

/**
 * Whether to show the worktree list: when there are sibling worktrees, or when
 * the user could add one.
 */
export const worktreeListEnabled = atom(
  get => get(enabledWorktreeInfo) != null || get(worktreeMutationsSupported),
);

/** All sibling-worktree checkout locations, independent of whether labels are visible. */
export const allOtherWorktreeCheckoutsByHash = atom(get => {
  const empty = new Map<Hash, Array<WorktreeEntry>>();

  const enabled = get(enabledWorktreeInfo);
  if (enabled == null) {
    return empty;
  }
  const {info, worktreeInfo} = enabled;

  const map = new Map<Hash, Array<WorktreeEntry>>();
  for (const worktree of worktreeInfo.worktrees) {
    if (worktree.node == null || pathsAreIdentical(worktree.path, info.repoRoot)) {
      continue;
    }
    const trimmed = worktree.node.trim();
    if (trimmed === '' || !isHexHash(trimmed)) {
      continue;
    }
    const key = trimmed as Hash;
    // Store normalized trimmed node so key and entry stay in sync
    const normalized: WorktreeEntry = {...worktree, node: trimmed as Hash};
    const existing = map.get(key);
    if (existing != null) {
      existing.push(normalized);
    } else {
      map.set(key, [normalized]);
    }
  }
  return map;
});

/**
 * Sibling-worktree checkout locations that should be rendered as labels. Focus
 * mode and the display preference suppress the labels without discarding the
 * underlying checkout identity used by focus-mode filtering.
 */
export const otherWorktreeCheckoutsByHash = atom(get => {
  if (!get(showWorktreeLabels) || get(focusMode)) {
    return new Map<Hash, Array<WorktreeEntry>>();
  }
  return get(allOtherWorktreeCheckoutsByHash);
});

/** The sibling worktree(s) currently checked out at this commit, if any. */
export const isCheckedOutElsewhere = atomFamilyWeak((hash: Hash) =>
  atom(get => get(otherWorktreeCheckoutsByHash).get(hash)),
);

export const currentWorktree = atom(get => {
  const enabled = get(enabledWorktreeInfo);
  if (enabled == null) {
    return undefined;
  }
  const {info, worktreeInfo} = enabled;
  return worktreeInfo.worktrees.find(wt => pathsAreIdentical(wt.path, info.repoRoot));
});

/**
 * This repo's own worktree name (label, or basename of its path), but only when
 * this checkout *is* a linked worktree (not the main one) -- `undefined` for a
 * solo checkout or the main worktree, so "You are here" stays plain there.
 */
export const currentWorktreeName = atom(get => {
  if (!get(showWorktreeLabels)) {
    return undefined;
  }
  const mine = get(currentWorktree);
  if (mine == null || mine.role !== 'linked') {
    return undefined;
  }
  return mine.label ?? basename(mine.path, guessPathSep(mine.path));
});
