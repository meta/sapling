/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import type {Atom} from 'jotai';
import type {Comparison} from 'shared/Comparison';
import type {Result} from '../../types';

type ContextId = {path: string; comparison: Comparison};

export type DiffViewMode = 'split' | 'unified';

/**
 * Context used to render SplitDiffView
 */
export type Context = {
  id: ContextId;
  copy?: (s: string) => void;
  openFile?: () => unknown;
  openFileToLine?: (line: OneIndexedLineNumber) => unknown;
  collapsed: boolean;
  setCollapsed: (collapsed: boolean) => void;
  fetchAdditionalLines?(
    id: ContextId,
    start: OneIndexedLineNumber,
    numLines: number,
  ): Promise<Result<Array<string>>>;
  displayLineNumbers?: boolean;
  /** An atom whose value is used as an effect dependency. If this value changes, the comparison will be considered invalidated and must be refreshed. */
  comparisonInvalidationKey?: Atom<string>;
  /** Translation function for the current language. */
  t?: (s: string) => string;
  /**
   * Whether to render as a side-by-side diff view, or a unified view where deleted and added lines are interleaved.
   * TODO: make this controllable / configurable / responsive based on screen width
   */
  display: DiffViewMode;
};

export type OneIndexedLineNumber = Exclude<number, 0>;
