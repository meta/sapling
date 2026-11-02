/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import {atom, useSetAtom} from 'jotai';
import {useEffect, useRef} from 'react';
import {useCommand} from './ISLShortcuts';
import {
  atomWithOnChange,
  configBackedAtom,
  localStorageBackedAtom,
  readAtom,
  writeAtom,
} from './jotaiUtils';

export const mainContentWidthState = atom(500);

export const renderCompactAtom = configBackedAtom<boolean>('isl.render-compact', false);

export const zoomUISettingAtom = atomWithOnChange(
  localStorageBackedAtom<number>('isl.ui-zoom', 1, {
    portable: true,
    validate: zoom => typeof zoom === 'number' && zoom >= 0.5 && zoom <= 3,
  }),
  newValue => {
    document.body?.style.setProperty('--zoom', `${newValue}`);
  },
);

export function useZoomShortcut() {
  useCommand('ZoomIn', () => {
    const old = readAtom(zoomUISettingAtom);
    writeAtom(zoomUISettingAtom, Math.round((old + 0.1) * 100) / 100);
  });
  useCommand('ZoomOut', () => {
    const old = readAtom(zoomUISettingAtom);
    writeAtom(zoomUISettingAtom, Math.round((old - 0.1) * 100) / 100);
  });
}

export function useMainContentWidth() {
  const setMainContentWidth = useSetAtom(mainContentWidthState);

  const mainContentRef = useRef<null | HTMLDivElement>(null);
  useEffect(() => {
    const element = mainContentRef.current;
    if (element == null) {
      return;
    }

    const obs = new ResizeObserver(entries => {
      const [entry] = entries;
      setMainContentWidth(entry.contentRect.width);
    });
    obs.observe(element);
    return () => obs.unobserve(element);
  }, [mainContentRef, setMainContentWidth]);

  return mainContentRef;
}

export const NARROW_COMMIT_TREE_WIDTH = 800;
export const NARROW_COMMIT_TREE_WIDTH_WHEN_COMPACT = 300;
export const VERY_NARROW_COMMIT_TREE_WIDTH = 500;
export const VERY_NARROW_COMMIT_TREE_WIDTH_WHEN_COMPACT = 200;

export type CommitTreeWidth = 'wide' | 'narrow' | 'very-narrow';

/**
 * Categorizes the available commit-tree width so layout consumers can coordinate their behavior
 * without independently combining overlapping breakpoint atoms.
 */
export const commitTreeWidth = atom<CommitTreeWidth>(get => {
  const width = get(mainContentWidthState);
  const compact = get(renderCompactAtom);
  const veryNarrowWidth = compact
    ? VERY_NARROW_COMMIT_TREE_WIDTH_WHEN_COMPACT
    : VERY_NARROW_COMMIT_TREE_WIDTH;
  if (width < veryNarrowWidth) {
    return 'very-narrow';
  }

  const narrowWidth = compact ? NARROW_COMMIT_TREE_WIDTH_WHEN_COMPACT : NARROW_COMMIT_TREE_WIDTH;
  return width < narrowWidth ? 'narrow' : 'wide';
});

/**
 * Tracks the window/viewport width. Unlike mainContentWidthState, this is
 * stable regardless of drawer position changes, making it safe for layout
 * decisions that affect drawer placement (avoids oscillation).
 */
export const windowWidthState = atom(window.innerWidth);
windowWidthState.onMount = set => {
  const listener = () => set(window.innerWidth);
  window.addEventListener('resize', listener);
  return () => window.removeEventListener('resize', listener);
};

export const isNarrowWindow = atom(get => get(windowWidthState) < NARROW_COMMIT_TREE_WIDTH);
