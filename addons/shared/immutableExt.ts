/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import type {ValueObject} from 'immutable';

/** Wraps a ValueObject so it self-updates on equals. */
export class SelfUpdate<T extends ValueObject> implements ValueObject {
  inner: T;

  constructor(inner: T) {
    this.inner = inner;
  }

  hashCode(): number {
    return this.inner.hashCode() + 1;
  }

  equals(other: unknown): boolean {
    if (!(other instanceof SelfUpdate)) {
      return false;
    }
    if (this === other) {
      return true;
    }
    const otherInner = other.inner;
    const result = this.inner.equals(otherInner);
    if (result && this.inner !== otherInner) {
      this.inner = otherInner;
    }
    return result;
  }
}
