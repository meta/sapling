/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import {localStorageBackedAtom} from '../jotaiUtils';

/**
 * Atom that stores the last selected review option id, persisted to localStorage.
 */
export const lastSelectedReviewOptionIdAtom = localStorageBackedAtom<string | null>(
  'isl.ai-code-review-selected-option',
  null,
);
