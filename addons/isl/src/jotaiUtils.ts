/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import type {Atom, Getter, WritableAtom} from 'jotai';
import type {Json} from 'shared/typeUtils';
import type {PersistedStateOptions, Platform} from './platform';
import type {ConfigName, LocalStorageName, SettableConfigName} from './types';

import {atom, getDefaultStore, useStore} from 'jotai';
import {loadable} from 'jotai/utils';
import type {DependencyList} from 'react';
import {useCallback, useRef, useSyncExternalStore} from 'react';
import {RateLimiter} from 'shared/RateLimiter';
import {isPromise} from 'shared/utils';
import serverAPI from './ClientToServerAPI';
import platform from './platform';
import {assert} from './utils';

/** A mutable atom that stores type `T`. */
export type MutAtom<T> = WritableAtom<T, [T | ((prev: T) => T)], void>;

/**
 * The store being used. Do not use this directly. Alternatives are:
 * - use `readAtom` instead of `store.get`.
 * - use `writeAtom` instead of `store.set`.
 * - use `atomWithOnChange` instead of `store.sub`.
 */
let store = getDefaultStore();

/**
 * Replace the current Jotai store used by this module.
 * Practically, this is only useful for tests to reset states.
 */
export function setJotaiStore(newStore: typeof store) {
  store = newStore;
}

/** Define a read-write atom backed by a config. */
export function configBackedAtom<T extends Json>(
  name: SettableConfigName,
  defaultValue: T,
  readonly?: false,
): MutAtom<T>;

/**
 * Define a read-only atom backed by a config.
 *
 * This can be useful for staged rollout features
 * where the config is not supposed to be set by the user.
 * (user config will override the staged rollout config)
 */
export function configBackedAtom<T extends Json>(
  name: ConfigName,
  defaultValue: T,
  readonly: true,
  useRawValue?: boolean,
): Atom<T>;

export function configBackedAtom<T extends Json>(
  name: ConfigName | SettableConfigName,
  defaultValue: T,
  readonly = false,
  useRawValue?: boolean,
): MutAtom<T> | Atom<T> {
  // https://jotai.org/docs/guides/persistence
  const primitiveAtom = atom<T>(defaultValue);

  let lastStrValue: undefined | string = undefined;
  serverAPI.onMessageOfType('gotConfig', event => {
    if (event.name !== name) {
      return;
    }
    lastStrValue = event.value;
    writeAtom(
      primitiveAtom,
      event.value === undefined
        ? defaultValue
        : useRawValue === true
          ? event.value
          : JSON.parse(event.value),
    );
  });
  serverAPI.onConnectOrReconnect(() => {
    serverAPI.postMessage({
      type: 'getConfig',
      name: name as ConfigName,
    });
  });

  return readonly
    ? atom<T>(get => get(primitiveAtom))
    : atom<T, [T | ((prev: T) => T)], void>(
        get => get(primitiveAtom),
        (get, set, update) => {
          const newValue = typeof update === 'function' ? update(get(primitiveAtom)) : update;
          set(primitiveAtom, newValue);
          const strValue = useRawValue ? String(newValue) : JSON.stringify(newValue);
          if (strValue !== lastStrValue) {
            lastStrValue = strValue;
            serverAPI.postMessage({
              type: 'setConfig',
              name: name as SettableConfigName,
              value: strValue,
            });
          }
        },
      );
}

/**
 * Loads this atom from a local persistent cache (usually browser local storage),
 * and persists any changes back to it.
 * Useful for some customizations that don't warrant a user-visible sl config,
 * for example UI expansion state.
 *
 * `portable` marks a preference that means the same on every machine (never per-repo, per-commit
 * or per-machine state), which an embedding platform may keep across machines; `validate` narrows
 * what such a value may be, beyond having the default's JSON type.
 */
export function localStorageBackedAtom<T extends Json>(
  name: LocalStorageName,
  defaultValue: T,
  options?: {portable: true; validate?: (value: Json) => boolean},
): MutAtom<T> {
  const persisted: PersistedStateOptions | undefined =
    options == null ? undefined : {...options, defaultValue};
  const stored =
    persisted == null
      ? platform.getPersistedState<T>(name)
      : platform.getPersistedState<T>(name, persisted);
  const primitiveAtom = atom<T>(stored ?? defaultValue);

  return atom(
    get => get(primitiveAtom),
    (get, set, update) => {
      const newValue = typeof update === 'function' ? update(get(primitiveAtom)) : update;
      set(primitiveAtom, newValue);
      if (persisted == null) {
        platform.setPersistedState(name, newValue);
      } else {
        platform.setPersistedState(name, newValue, persisted);
      }
    },
  );
}

/**
 * Wraps an atom with an "onChange" callback.
 * Changing the returned atom will trigger the callback.
 * Calling this function will trigger `onChange` with the current value except when `skipInitialCall` is `true`.
 */
export function atomWithOnChange<T>(
  originalAtom: MutAtom<T>,
  onChange: (value: T) => void,
  skipInitialCall?: boolean,
): MutAtom<T> {
  if (skipInitialCall !== true) {
    onChange(readAtom(originalAtom));
  }
  return atom(
    get => get(originalAtom),
    (get, set, args) => {
      const oldValue = get(originalAtom);
      set(originalAtom, args);
      const newValue = get(originalAtom);
      if (oldValue !== newValue) {
        onChange(newValue);
      }
    },
  );
}

/**
 * Creates a lazily initialized atom.
 * On first read, trigger `load` to get the actual value.
 * `fallback` provides the value when the async `load` is running.
 * `original` is an optional nullable atom to provide the value.
 */
export function lazyAtom<T>(
  load: (get: Getter) => Promise<T> | T,
  fallback: T,
  original?: MutAtom<T | undefined>,
): MutAtom<T> {
  const originalAtom = original ?? atom<T | undefined>(undefined);
  const limiter = new RateLimiter(1);
  return atom(
    get => {
      const value = get(originalAtom);
      if (value !== undefined) {
        return value;
      }
      const loaded = load(get);
      if (!isPromise(loaded)) {
        writeAtom(originalAtom, loaded);
        return loaded;
      }
      // Kick off the "load" but rate limit it.
      limiter.enqueueRun(async () => {
        if (get(originalAtom) !== undefined) {
          // A previous "load" was completed.
          return;
        }
        const newValue = await loaded;
        writeAtom(originalAtom, newValue);
      });
      // Use the fallback value while waiting for the promise.
      return fallback;
    },
    (get, set, args) => {
      const newValue =
        typeof args === 'function' ? (args as (prev: T) => T)(get(originalAtom) ?? fallback) : args;
      set(originalAtom, newValue);
    },
  );
}

export function readAtom<T>(atom: Atom<T>): T {
  return store.get(atom);
}

export function writeAtom<T>(atom: MutAtom<T>, value: T | ((prev: T) => T)) {
  store.set(atom, value);
}

export function refreshAtom<T>(atom: WritableAtom<T, [], void>) {
  store.set(atom);
}

/** Create an atom that is automatically reset when the depAtom is changed. */
export function atomResetOnDepChange<T>(defaultValue: T, depAtom: Atom<unknown>): MutAtom<T> {
  assert(
    typeof depAtom !== 'undefined',
    'depAtom should not be undefined (is there a circular dependency?)',
  );
  const primitiveAtom = atom<T>(defaultValue);
  let lastDep = readAtom(depAtom);
  return atom(
    get => {
      const dep = get(depAtom);
      if (dep !== lastDep) {
        lastDep = dep;
        writeAtom(primitiveAtom, defaultValue);
      }
      return get(primitiveAtom);
    },
    (_get, set, update) => {
      set(primitiveAtom, update);
    },
  );
}

/**
 * Creates a derived atom that can be force-refreshed, by using the update function.
 * Uses Suspense for async update functions.
 * ```
 * const postsAtom = atomWithRefresh(get => fetchPostsAsync());
 *   ...
 * const [posts, refreshPosts] = useAtom(postsAtom);
 * ```
 */
export function atomWithRefresh<T>(fn: (get: Getter) => T) {
  const refreshCounter = atom(0);

  return atom(
    get => {
      get(refreshCounter);
      return fn(get);
    },
    (_, set) => set(refreshCounter, i => i + 1),
  );
}

/**
 * Creates a derived atom that can be force-refreshed, by using the update function.
 * The underlying async state is given as a Loadable atom instead of one that suspends.
 * ```
 * const postsAtom = atomWithRefresh(get => fetchPostsAsync());
 *   ...
 * const [postsLoadable, refreshPosts] = useAtom(postsAtom);
 * if (postsLoadable.state === 'hasData') {
 *   const posts = postsLoadable.data;
 * }
 * ```
 */
export function atomLoadableWithRefresh<T>(fn: (get: Getter) => Promise<T>) {
  const refreshCounter = atom(0);
  const loadableAtom = loadable(
    atom(get => {
      get(refreshCounter);
      return fn(get);
    }),
  );

  return atom(
    get => get(loadableAtom),
    (_, set) => set(refreshCounter, i => i + 1),
  );
}

/**
 * Drop-in replacement of `atomFamily` that tries to book-keep internal cache
 * periodically to avoid memory leak.
 *
 * There are 2 caches:
 * - "strong" cache: keep atoms alive even if all references are gone.
 * - "weak" cache: keep atoms alive as long as there is a reference to it.
 *
 * Periodically, when the weak cache size reaches a threshold, a "cleanup"
 * process runs to:
 * - Clear the "strong" cache to mitigate memory leak.
 * - Drop dead entries in the "weak" cache.
 * - Update the threshold to 2x the "weak" cache size.
 *
 * Therefore the memory waste is hopefully within 2x of the needed memory.
 *
 * Setting `options.useStrongCache` to `false` disables the "strong" cache
 * to further limit memory usage.
 */
export function atomFamilyWeak<K, A extends Atom<unknown>>(
  fn: (key: K) => A,
  options?: AtomFamilyWeakOptions,
): AtomFamilyWeak<K, A> {
  const {useStrongCache = true, initialCleanupThreshold = 4} = options ?? {};

  // This cache persists through component unmount / remount, therefore
  // it can be memory leaky.
  const strongCache = new Map<K, A>();

  // This cache ensures atoms in use are returned as-is during re-render,
  // to avoid infinite re-render with React StrictMode.
  const weakCache = new Map<K, WeakRef<A>>();

  const cleanup = () => {
    // Clear the strong cache. This allows GC to drop weakRefs.
    strongCache.clear();
    // Clean up weak cache - remove dead entries.
    weakCache.forEach((weakRef, key) => {
      if (weakRef.deref() == null) {
        weakCache.delete(key);
      }
    });
    // Adjust threshold to trigger the next cleanup.
    resultFunc.threshold = weakCache.size * 2;
  };

  const resultFunc: AtomFamilyWeak<K, A> = (key: K) => {
    const cached = strongCache.get(key);
    if (cached != null) {
      // This state was accessed recently.
      return cached;
    }
    const weakCached = weakCache.get(key)?.deref();
    if (weakCached != null) {
      // State is not dead yet.
      return weakCached;
    }
    // Not in cache. Need re-calculate.
    const atom = fn(key);
    if (useStrongCache) {
      strongCache.set(key, atom);
    }
    weakCache.set(key, new WeakRef(atom));
    if (weakCache.size > resultFunc.threshold) {
      cleanup();
    }
    if (resultFunc.debugLabel != null && atom.debugLabel == null) {
      atom.debugLabel = `${resultFunc.debugLabel}:${key}`;
    }
    return atom;
  };

  resultFunc.cleanup = cleanup;
  resultFunc.threshold = initialCleanupThreshold;
  resultFunc.strongCache = strongCache;
  resultFunc.weakCache = weakCache;
  resultFunc.clear = () => {
    weakCache.clear();
    strongCache.clear();
    resultFunc.threshold = initialCleanupThreshold;
  };

  return resultFunc;
}

type AtomFamilyWeakOptions = {
  /**
   * Enable the "strong" cache so unmount / remount can try to reuse the cache.
   *
   * If this is disabled, then only the weakRef cache is used, states that
   * are no longer referred by components might be lost to GC.
   *
   * Default: true.
   */
  useStrongCache?: boolean;

  /**
   * Number of items before triggering an initial cleanup.
   * Default: 4.
   */
  initialCleanupThreshold?: number;
};

export interface AtomFamilyWeak<K, A extends Atom<unknown>> {
  (key: K): A;
  /** The "strong" cache (can be empty). */
  strongCache: Map<K, A>;
  /** The weakRef cache (must contain entries that are still referred elsewhere). */
  weakCache: Map<K, WeakRef<A>>;
  /** Trigger a cleanup ("GC"). */
  cleanup(): void;
  /** Clear the cache */
  clear(): void;
  /** Auto cleanup threshold on weakCache size. */
  threshold: number;
  /** Prefix of debugLabel. */
  debugLabel?: string;
}

function getAllPersistedStateWithPrefix<T>(
  prefix: LocalStorageName,
  islPlatform: Platform,
): Record<string, T> {
  const all = islPlatform.getAllPersistedState();
  if (all == null) {
    return {};
  }
  return Object.fromEntries(
    Object.entries(all)
      .filter(([key]) => key.startsWith(prefix))
      .map(([key, value]) => [key.slice(prefix.length), value]),
  );
}

/**
 * An atom family that loads and persists data from local storage.
 * Each key is stored in a separate local storage entry, using the `storageKeyPrefix`.
 * Each stored value includes a timestamp so that stale data can be evicted,
 * on next startup.
 * Data is loaded once on startup, but written to local storage on every change.
 * Write `undefined` to any atom to explicitly remove it from local storage.
 */
export function localStorageBackedAtomFamily<K extends string, T extends Json | Partial<Json>>(
  storageKeyPrefix: LocalStorageName,
  getDefault: (key: K) => T,
  maxAgeDays = 14,
  islPlatform = platform,
): AtomFamilyWeak<K, WritableAtom<T, [T | undefined | ((prev: T) => T | undefined)], void>> {
  type StoredData = {
    data: T;
    date: number;
  };
  const initialData = getAllPersistedStateWithPrefix<StoredData>(storageKeyPrefix, islPlatform);

  const ONE_DAY_MS = 1000 * 60 * 60 * 24;
  // evict previously stored old data
  for (const key in initialData) {
    const data = initialData[key];
    if (data?.date != null && Date.now() - data?.date > ONE_DAY_MS * maxAgeDays) {
      islPlatform.setPersistedState(storageKeyPrefix + key, undefined);
      delete initialData[key];
    }
  }

  return atomFamilyWeak((key: K) => {
    // We use the full getPersistedState instead of initialData, as this atom may have been evicted from the weak cache,
    // and is now being recreated and requires checking the actual cache to get any changes after initialization.
    const data = islPlatform.getPersistedState(storageKeyPrefix + key) as StoredData | null;
    const initial = data?.data ?? getDefault(key);
    const storageKey = storageKeyPrefix + key;

    const inner = atom<T>(initial);
    return atom(
      get => get(inner),
      (get, set, value) => {
        const oldValue = get(inner);
        const result = typeof value === 'function' ? value(oldValue) : value;
        set(inner, result === undefined ? getDefault(key) : result);
        const newValue = get(inner);
        if (oldValue !== newValue) {
          // TODO: debounce?
          islPlatform.setPersistedState(
            storageKey,
            result == null
              ? undefined
              : ({
                  data: newValue as Json,
                  date: Date.now(),
                } as StoredData as Json),
          );
        }
      },
    );
  });
}

/**
 * `selector(useAtomValue(sourceAtom))`, re-rendering only when the selected value changes.
 *
 * Subscribes as a listener instead of through a derived atom. Every derived atom is a dependent
 * that Jotai walks on each write to its sources, so one derived atom per smartlog row (tens of
 * thousands across the row components) made a single smartlog update cost seconds. Prefer this to
 * `atomFamilyWeak` for values read once per row. `selector` runs again whenever `sourceAtom`
 * changes, so keep it cheap and pure, with everything it closes over in `deps`.
 */
export function useAtomSelect<S, T>(
  sourceAtom: Atom<S>,
  selector: (source: S) => T,
  deps: DependencyList,
): T {
  const store = useStore();
  const subscribe = useCallback(
    (onChange: () => void) => store.sub(sourceAtom, onChange),
    [store, sourceAtom],
  );
  // eslint-disable-next-line react-hooks/exhaustive-deps
  const select = useCallback(selector, deps);
  // useSyncExternalStore requires an unchanged snapshot while nothing changed, and selectors may
  // build a new object on every call.
  const last = useRef<{source: S; select: (source: S) => T; value: T} | null>(null);
  const getSnapshot = () => {
    const source = store.get(sourceAtom);
    const cached = last.current;
    if (cached != null && Object.is(cached.source, source) && cached.select === select) {
      return cached.value;
    }
    const value = select(source);
    last.current = {source, select, value};
    return value;
  };
  return useSyncExternalStore(subscribe, getSnapshot);
}

/**
 * Similar to `useAtomValue(mapAtom).get(key)` but avoids re-render if the map
 * is changed but the `get(key)` does not change.
 */
export function useAtomGet<K, V>(mapAtom: Atom<{get(k: K): V | undefined}>, key: K): V | undefined {
  return useAtomSelect(mapAtom, map => map.get(key), [key]);
}

/**
 * Similar to `useAtomValue(setAtom).has(key)` but avoids re-render if the set
 * is changed but the `has(key)` does not change.
 */
export function useAtomHas<K>(setAtom: Atom<{has(k: K): boolean}>, key: K): boolean {
  return useAtomSelect(setAtom, set => set.has(key), [key]);
}
