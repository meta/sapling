/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import {localStorageBackedAtom} from '../jotaiUtils';

export const focusMode = localStorageBackedAtom<boolean>('isl.focus-mode', false, {portable: true});
