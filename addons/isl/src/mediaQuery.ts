/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import {registerCleanup} from './utils';

let prefersReducedMotionValue = false;

try {
  const prefersReducedMotionQuery = window.matchMedia('(prefers-reduced-motion: reduce)');
  const handleChange = () => {
    prefersReducedMotionValue = prefersReducedMotionQuery.matches;
  };
  prefersReducedMotionQuery.addEventListener('change', handleChange);
  registerCleanup(
    prefersReducedMotionQuery,
    () => prefersReducedMotionQuery.removeEventListener('change', handleChange),
    import.meta.hot,
  );
  prefersReducedMotionValue = prefersReducedMotionQuery.matches;
} catch (_e) {
  // testing-library does not define "window.matchMedia".
}

/** Returns `true` if the user wants reduced animation. */
export function prefersReducedMotion() {
  return prefersReducedMotionValue;
}
