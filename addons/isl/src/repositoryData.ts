/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import type {CommitInfo, RepoInfo, RepoRelativePath} from './types';

import {atom, useAtomValue} from 'jotai';
import {atomResetOnDepChange, localStorageBackedAtom} from './jotaiUtils';
import platform from './platform';

export const repositoryData = atom<{info?: RepoInfo; cwd?: string}>({});

/** "cwd" is not always the repo root. It can be a path inside the repo. */
export const serverCwd = atom<string>(get => {
  const data = get(repositoryData);
  if (data.info?.type === 'cwdNotARepository') {
    return data.info.cwd;
  }
  return data?.cwd ?? platform.initialUrlParams?.get('cwd') ?? '';
});

export const repoRootAtom = atom(get => {
  const data = get(repositoryData);
  return data.info?.type === 'success' ? data.info.repoRoot : '';
});

export const repoRelativeCwd = atom<RepoRelativePath>(get => {
  const cwd = get(serverCwd);
  const root = get(repoRootAtom);
  return cwd.startsWith(root) ? cwd.slice(root.length + 1) + '/' : cwd;
});

/**
 * Returns true if the commit is irrelevant to the current cwd,
 * due to it modifying only files in folders that are not under the cwd.
 * If the max prefix is "inside" the cwd, it is NOT irrelevant.
 *   > if the max prefix is `addons/isl` and the cwd is `addons`, it is NOT irrelevant.
 * If the max prefix is "above" the cwd, it is NOT irrelevant.
 *   > if the max prefix is `addons` and the cwd is `addons/isl`, it is NOT irrelevant.
 * Only if the max prefix contains portions that do not match the cwd is it irrelevant.
 *   > if the max prefix is `addons/isl` and the cwd is `www`, it IS irrelevant.
 * Thus, if the cwd is the repo root, it is never irrelevant.
 *
 * If a commit has only irrelevant files, but then a relevant file is added, the commit
 * is guaranteed to become relevant, since the common portion of the paths will
 * be a prefix of the relevant file.
 */
export const useIsIrrelevantToCwd = (commit: CommitInfo) => {
  const isEnabled = useAtomValue(irrelevantCwdDeemphasisEnabled);
  const cwd = useAtomValue(repoRelativeCwd);
  if (!isEnabled) {
    return false;
  }
  return isIrrelevantToCwd(commit, cwd);
};

export function isIrrelevantToCwd(commit: CommitInfo, repoRelativeCwd: RepoRelativePath): boolean {
  return (
    repoRelativeCwd !== '/' &&
    !commit.maxCommonPathPrefix.startsWith(repoRelativeCwd) &&
    !repoRelativeCwd.startsWith(commit.maxCommonPathPrefix)
  );
}
export const __TEST__ = {isIrrelevantToCwd};

export const irrelevantCwdDeemphasisEnabled = localStorageBackedAtom<boolean>(
  'isl.deemphasize-cwd-irrelevant-commits',
  true,
  {portable: true},
);

export const hideIrrelevantCwdStacks = localStorageBackedAtom<boolean>(
  'isl.hide-cwd-irrelevant-stacks',
  false,
  {portable: true},
);

/**
 * Derived atom that combines the irrelevant CWD display settings into a single value.
 * - 'show': Show irrelevant commits normally (deemphasis disabled)
 * - 'deemphasize': Show irrelevant commits but with reduced visual prominence
 * - 'hide': Don't show irrelevant commits at all
 */
export const irrelevantCwdDisplayModeAtom = atom(
  get => {
    const deemphasizeEnabled = get(irrelevantCwdDeemphasisEnabled);
    const hideEnabled = get(hideIrrelevantCwdStacks);

    if (hideEnabled) {
      return 'hide';
    }
    if (deemphasizeEnabled) {
      return 'deemphasize';
    }
    return 'show';
  },
  (_get, set, newValue: 'show' | 'deemphasize' | 'hide') => {
    switch (newValue) {
      case 'show':
        set(irrelevantCwdDeemphasisEnabled, false);
        set(hideIrrelevantCwdStacks, false);
        break;
      case 'deemphasize':
        set(irrelevantCwdDeemphasisEnabled, true);
        set(hideIrrelevantCwdStacks, false);
        break;
      case 'hide':
        set(irrelevantCwdDeemphasisEnabled, true);
        set(hideIrrelevantCwdStacks, true);
        break;
    }
  },
);

/**
 * A string of repo root and the "cwd". Note a same "cwd" does not infer the same repo,
 * when there are nested (ex. submodule) repos.
 */
export const repoRootAndCwd = atom<string>(get => `${get(serverCwd)}\n${get(repoRootAtom)}`);

/** A specific version of `atomResetOnDepChange`. */
export function atomResetOnCwdChange<T>(defaultValue: T) {
  return atomResetOnDepChange(defaultValue, repoRootAndCwd);
}

/** Like `atomResetOnCwdChange`, but only resets when the repo root changes, not subdirectory. */
export function atomResetOnRepoChange<T>(defaultValue: T) {
  return atomResetOnDepChange(defaultValue, repoRootAtom);
}
