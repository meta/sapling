/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import type {AllDrawersState, DrawerState} from './Drawers';

import {atom} from 'jotai';
import {localStorageBackedAtom, readAtom, writeAtom} from './jotaiUtils';
import {isNarrowWindow} from './responsive';
import {getWindowWidthInPixels, registerCleanup} from './utils';

const AUTO_CLOSE_MAX_SIZE = 700;
const DEFAULT_RIGHT_DRAWER_WIDTH = 500;

export type CommitInfoLocation = 'right' | 'bottom' | 'left' | 'top';
export type CommitInfoLocationWithAuto = CommitInfoLocation | 'auto';

export const commitInfoLocationAtom = localStorageBackedAtom<CommitInfoLocationWithAuto>(
  'isl.commit-info-location',
  'auto',
  {
    portable: true,
    validate: value => ['auto', 'right', 'bottom', 'left', 'top'].includes(value as string),
  },
);

/**
 * Resolves 'auto' to a concrete location based on window width.
 * When 'auto', uses 'bottom' in narrow windows and 'right' otherwise.
 * Uses window width (not main content width) to avoid oscillation —
 * changing drawer position affects main content width but not window width.
 */
export const effectiveCommitInfoLocationAtom = atom<CommitInfoLocation>(get => {
  const preference = get(commitInfoLocationAtom);
  if (preference === 'auto') {
    return get(isNarrowWindow) ? 'bottom' : 'right';
  }
  return preference;
});

/** Expand the commit info drawer at the current configured location. */
export function expandCommitInfoView() {
  const loc = readAtom(effectiveCommitInfoLocationAtom);
  writeAtom(islDrawerState, val => ({...val, [loc]: {...val[loc], collapsed: false}}));
}

export const islDrawerState = localStorageBackedAtom<AllDrawersState>('isl.drawer-state', {
  right: {
    size: DEFAULT_RIGHT_DRAWER_WIDTH,
    collapsed: false,
  },
  left: {size: 200, collapsed: true},
  top: {size: 200, collapsed: true},
  bottom: {size: 200, collapsed: true},
});

// On startup, override existing state to collapse the sidebar if the screen is too small.
// This allows collapsing even if the size has been previous persisted.
function autoCloseBasedOnWindowWidth() {
  const windowWidth = getWindowWidthInPixels();
  if (windowWidth === 0) {
    // window not loaded yet
    return;
  }

  const location = readAtom(effectiveCommitInfoLocationAtom);
  const isVertical = location === 'top' || location === 'bottom';
  if (isVertical) {
    // Only auto-close for horizontal (left/right) drawers based on window width.
    return;
  }

  const current = readAtom(islDrawerState)[location].size;
  const setDrawer = (state: DrawerState) => {
    const oldValue = readAtom(islDrawerState);
    writeAtom(islDrawerState, {
      ...oldValue,
      [location]: state,
    });
  };
  if (windowWidth < AUTO_CLOSE_MAX_SIZE) {
    setDrawer({
      collapsed: true,
      size: Math.min(windowWidth, current ?? DEFAULT_RIGHT_DRAWER_WIDTH),
    });
  }
}

const resizeFn = () => {
  autoCloseBasedOnWindowWidth();
};

// check startup window size
window.addEventListener('load', resizeFn);

registerCleanup(
  islDrawerState,
  () => {
    window.removeEventListener('load', resizeFn);
  },
  import.meta.hot,
);
