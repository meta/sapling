/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import type {Json} from './typeUtils';

export function notEmpty<T>(value: T | null | undefined): value is T {
  return value !== null && value !== undefined;
}

/**
 * Throw if value is `null` or `undefined`.
 */
export function nullthrows<T>(value: T | undefined | null): T {
  if (value == null) {
    throw new Error(`expected value not to be ${value}`);
  }
  return value;
}

/**
 * generate a small random ID string via time in ms + random number encoded as a [0-9a-z]+ string
 * This should not be used for cryptographic purposes or if universal uniqueness is absolutely necessary
 */
export function randomId(): string {
  return Date.now().toString(36) + Math.random().toString(36);
}

export type Deferred<T> = {
  promise: Promise<T>;
  resolve: (t: T) => void;
  reject: (e: Error) => void;
};
/**
 * Wraps `new Promise<T>()`, so you can access resolve/reject outside of the callback.
 * Useful for externally resolving promises in tests.
 */
export function defer<T>(): Deferred<T> {
  const deferred = {
    promise: undefined as unknown as Promise<T>,
    resolve: undefined as unknown as (t: T) => void,
    reject: undefined as unknown as (e: Error) => void,
  };
  deferred.promise = new Promise<T>((resolve: (t: T) => void, reject: (e: Error) => void) => {
    deferred.resolve = resolve;
    deferred.reject = reject;
  });
  return deferred;
}

export class CancelledError extends Error {
  constructor() {
    super('cancelled');
    this.name = 'CancelledError';
  }
}

export type CancellablePromise<T> = Promise<T> & {dispose(): void};

/**
 * `setTimeout` as an awaitable promise, resolving once `timeMs` has elapsed.
 * `dispose()` cancels the timer and rejects with `CancelledError`; it is a
 * no-op once the timer has already fired.
 */
export function sleep(timeMs: number): CancellablePromise<void> {
  let cancel!: () => void;
  const promise = new Promise<void>((resolve, reject) => {
    const timer = setTimeout(resolve, timeMs);
    cancel = () => {
      clearTimeout(timer);
      reject(new CancelledError());
    };
  }) as CancellablePromise<void>;
  promise.dispose = cancel;
  // Fire-and-forget callers never await, so a dispose() would otherwise surface
  // as an unhandled rejection. Awaiting callers still observe the rejection.
  promise.catch(() => {});
  return promise;
}

/**
 * Run `callback` after `timeMs`, resolving with its result. `dispose()` cancels
 * the timer so the callback never runs, and rejects with `CancelledError`.
 */
export function timeout<T>(
  callback: () => T | PromiseLike<T>,
  timeMs: number,
): CancellablePromise<T> {
  const timer = sleep(timeMs);
  let isDisposed = false;
  const promise = timer.then(() => {
    if (isDisposed) {
      throw new CancelledError();
    }
    return callback();
  }) as CancellablePromise<T>;
  promise.dispose = () => {
    timer.dispose();
    isDisposed = true;
  };
  promise.catch(() => {});
  return promise;
}

/**
 * Returns a Promise which resolves after the current async tick is finished.
 */
export function nextTick(): Promise<void> {
  return sleep(0);
}

/**
 * Returns the part of the string after the last occurrence of delimiter,
 * or the entire string if no matches are found.
 * (default delimiter is '/')
 *
 * ```
 * basename('/path/to/foo.txt', '/') -> 'foo.txt'
 * basename('foo.txt', '/') -> 'foo.txt'
 * basename('/path/', '/') -> ''
 * ```
 */
export function basename(s: string, delimiter = '/') {
  const foundIndex = s.lastIndexOf(delimiter);
  if (foundIndex === -1) {
    return s;
  }
  return s.slice(foundIndex + 1);
}

/**
 * Returns the directory portion of a path (everything before the last delimiter),
 * handling Windows drive roots correctly.
 * (default delimiter is '/')
 *
 * ```
 * dirname('/path/to/foo.txt', '/') -> '/path/to'
 * dirname('foo.txt', '/') -> ''
 * dirname('/foo', '/') -> ''
 * dirname('C:\\repo', '\\') -> 'C:\\'
 * dirname('C:\\Users\\repo', '\\') -> 'C:\\Users'
 * ```
 */
export function dirname(s: string, delimiter = '/'): string {
  const foundIndex = s.lastIndexOf(delimiter);
  if (foundIndex === -1) {
    return '';
  }
  // Handle Windows drive roots like "C:\" - keep the trailing backslash
  if (delimiter === '\\' && foundIndex === 2 && s[1] === ':') {
    return s.slice(0, 3);
  }
  // Handle Unix root
  if (delimiter === '/' && foundIndex === 0) {
    return '';
  }
  return s.slice(0, foundIndex);
}

/**
 * Given a multi-line string, return the first line excluding '\n'.
 * If no newlines in the string, return the whole string.
 */
export function firstLine(s: string): string {
  return s.split('\n', 1)[0];
}

/**
 * Split a serialized commit message into its title and description.
 *
 * The title and description are normally separated by a blank line. Remove that structural
 * separator while preserving any additional leading blank lines that are part of the description.
 */
export function splitCommitMessage(message: string): [title: string, description: string] {
  const title = firstLine(message);
  return [title, message.slice(title.length).replace(/^\n\n?/, '')];
}

/**
 * Applies a function to each key & value in an Object.
 * ```
 * mapObject(
 *   {foo: 1, bar: 2},
 *   ([key, value]) => ['_' + key, value + 1]
 * )
 * => {_foo: 2, _bar: 3}
 * ```
 */
export function mapObject<K1 extends string | number, V1, K2 extends string | number, V2>(
  o: Record<K1, V1>,
  func: (param: [K1, V1]) => [K2, V2],
): Record<K2, V2> {
  return Object.fromEntries((Object.entries(o) as Array<[K1, V1]>).map(func)) as Record<K2, V2>;
}

/**
 * Test if a generator yields the given value.
 * `value` can be either a value to test equality, or a function to customize the equality test.
 */
export function generatorContains<V>(
  gen: IterableIterator<V>,
  value: V | ((v: V) => boolean),
): boolean {
  const test = typeof value === 'function' ? (value as (v: V) => boolean) : (v: V) => v === value;
  for (const v of gen) {
    if (test(v)) {
      return true;
    }
  }
  return false;
}

/**
 * Zip 2 iterators.
 */
export function* zip<T, U>(iter1: Iterable<T>, iter2: Iterable<U>): IterableIterator<[T, U]> {
  const iterator1 = iter1[Symbol.iterator]();
  const iterator2 = iter2[Symbol.iterator]();
  while (true) {
    const result1 = iterator1.next();
    const result2 = iterator2.next();
    if (result1.done || result2.done) {
      break;
    }
    yield [result1.value, result2.value];
  }
}

/** Truncate a long string. */
export function truncate(text: string, maxLength = 100): string {
  return text.length > maxLength ? text.substring(0, Math.max(0, maxLength - 1)) + '…' : text;
}

export function isPromise<T>(o: unknown): o is Promise<T> {
  return typeof (o as {then?: () => void})?.then === 'function';
}

export function tryJsonParse(s: string): Json | undefined {
  try {
    return JSON.parse(s);
  } catch {
    return undefined;
  }
}

/**
 * Like Array.filter, but separates elements that pass from those that don't pass and return both arrays.
 * For example, partition([1, 2, 3], n => n % 2 === 0) returns [[2], [1, 3]]
 */
export function partition<T>(a: Array<T>, predicate: (item: T) => boolean): [Array<T>, Array<T>] {
  const [passed, failed] = [[], []] as [Array<T>, Array<T>];
  for (const item of a) {
    (predicate(item) ? passed : failed).push(item);
  }
  return [passed, failed];
}

/**
 * Like Array.filter, but separates elements that pass from those that don't pass and return both arrays.
 * For example, partition([1, 2, 3], n => n % 2 === 0) returns [[2], [1, 3]]
 */
export function group<ArrayType, BucketType extends string | number>(
  a: ReadonlyArray<ArrayType>,
  bucket: (item: ArrayType) => BucketType,
): Record<BucketType, Array<ArrayType> | undefined> {
  const result = {} as Record<BucketType, Array<ArrayType>>;
  for (const item of a) {
    const b = bucket(item);
    const existing = result[b] ?? [];
    existing.push(item);
    result[b] = existing;
  }
  return result;
}

/**
 * Split string `s` with the `sep` once.
 * If `s` does not contain `sep`, return undefined.
 */
export function splitOnce(s: string, sep: string): [string, string] | undefined {
  const index = s.indexOf(sep);
  if (index < 0) {
    return undefined;
  }
  return [s.substring(0, index), s.substring(index + sep.length)];
}

/**
 * Like Array's .map() but for iterators.
 * Returns a new iterator applying a function to each value in the input.
 */
export function* mapIterable<T, R>(iterable: Iterable<T>, mapFn: (t: T) => R): IterableIterator<R> {
  for (const item of iterable) {
    yield mapFn(item);
  }
}

export function base64Decode(data: string): ArrayBuffer {
  // Copy into a standalone ArrayBuffer: a Buffer's backing store may be a shared pool.
  return new Uint8Array(Buffer.from(data, 'base64')).buffer as ArrayBuffer;
}

/** Deduplicate items in an array. */
export function dedup<T>(arr: Array<T>): Array<T> {
  return Array.from(new Set(arr));
}

/** Normalize a filesystem path for comparison (slash, trailing slash). */
export function normalizeForComparison(p: string): string {
  // Drive root `C:/` must stay `C:/` (or `C:\`) — not `C:` — so Windows
  // detection `^[A-Za-z]:/` still matches after trailing-slash removal.
  const slashed = p.replace(/\\/g, '/');
  if (/^[A-Za-z]:\/$/.test(slashed)) {
    return slashed;
  }
  return slashed.replace(/\/+$/, '') || '/';
}

/**
 * Compare two filesystem paths for equality, tolerating '/' vs '\\' separators,
 * trailing slashes, and (on Windows-style absolute paths only) drive-letter
 * case differences.
 */
export function pathsAreIdentical(path1: string, path2: string): boolean {
  const normalizedPath1 = normalizeForComparison(path1);
  const normalizedPath2 = normalizeForComparison(path2);

  const isWindowsAbsolutePath = (path: string) => /^[A-Za-z]:\//.test(path);
  if (isWindowsAbsolutePath(normalizedPath1) && isWindowsAbsolutePath(normalizedPath2)) {
    return normalizedPath1.toLowerCase() === normalizedPath2.toLowerCase();
  }

  return normalizedPath1 === normalizedPath2;
}

/** Whether a string looks like a Sapling commit hash (hex, 6-40 chars). */
export const HEX_HASH_RE = /^[0-9a-f]{6,40}$/i;
export function isHexHash(s: string): boolean {
  return HEX_HASH_RE.test(s);
}

/** Guess whether a path uses '/' or '\\' as its separator, based on its contents. */
export function guessPathSep(path: string): '/' | '\\' {
  return path.includes('\\') ? '\\' : '/';
}
