/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import type {Atom} from 'jotai';
import type {AtomFamilyWeak} from '../jotaiUtils';
import type {Platform} from '../platform';
import platform from '../platform';
import type {LocalStorageName} from '../types';

import {act, render} from '@testing-library/react';
import {List} from 'immutable';
import {Provider, atom, createStore, useAtomValue} from 'jotai';
import {StrictMode} from 'react';
import {SelfUpdate} from 'shared/immutableExt';
import {gc} from 'shared/testUtils';
import {nextTick} from 'shared/utils';
import {
  atomFamilyWeak,
  atomResetOnDepChange,
  lazyAtom,
  localStorageBackedAtom,
  localStorageBackedAtomFamily,
  readAtom,
  useAtomGet,
  useAtomHas,
  useAtomSelect,
  writeAtom,
} from '../jotaiUtils';

class Foo extends SelfUpdate<List<number>> {}

const testFooAtom = atom<Foo>(new Foo(List()));

describe('Jotai compatibility', () => {
  it('does not freeze SelfUpdate types', () => {
    function MyTestComponent() {
      const foo2 = useAtomValue(testFooAtom);
      expect(Object.isSealed(foo2)).toBe(false);
      expect(Object.isFrozen(foo2)).toBe(false);

      return null;
    }
    render(
      <Provider>
        <MyTestComponent />
      </Provider>,
    );
  });
});

describe('lazyAtom', () => {
  it('returns sync load() value', () => {
    const a = lazyAtom(() => 1, 2);
    expect(readAtom(a)).toBe(1);
  });

  it('returns fallback value and sets async load() value', async () => {
    const a = lazyAtom(() => Promise.resolve(1), 2);
    expect(readAtom(a)).toBe(2);
    await nextTick();
    expect(readAtom(a)).toBe(1);
  });

  it('can depend on another atom', () => {
    const a = atom<number | undefined>(undefined);
    const b = lazyAtom(() => 2, 2, a);

    expect(readAtom(a)).toBe(undefined);

    // Reading `b` triggers updating `a`.
    expect(readAtom(b)).toBe(2);
    expect(readAtom(a)).toBe(2);

    // If `a` is updated to be not `undefined`, `b` will be the same value.
    writeAtom(a, 3);
    expect(readAtom(b)).toBe(3);

    // Updating `b` updates `a` too.
    writeAtom(b, 4);
    expect(readAtom(a)).toBe(4);

    // If `a` is updated to be `undefined`, `b` will be the fallback value.
    writeAtom(a, undefined);
    expect(readAtom(b)).toBe(2);
  });
});

describe('atomFamilyWeak', () => {
  let recalcFamilyCount = 0;
  let recalcAtomCount = 0;
  const family = atomFamilyWeak((key: number) => {
    recalcFamilyCount += 1;
    return atom(_get => {
      recalcAtomCount += 1;
      return key;
    });
  });

  let store = createStore();

  function TestComponent({k, f}: {k: number; f?: AtomFamilyWeak<number, Atom<number>>}) {
    const value = useAtomValue((f ?? family)(k));
    return <div>{value}</div>;
  }
  function TestApp({
    keys,
    family,
  }: {
    keys: Array<number>;
    family?: AtomFamilyWeak<number, Atom<number>>;
  }) {
    return (
      <Provider store={store}>
        {keys.map(k => (
          <TestComponent k={k} key={k} f={family} />
        ))}
      </Provider>
    );
  }

  beforeEach(() => {
    store = createStore();
    family.clear();
    recalcFamilyCount = 0;
    recalcAtomCount = 0;
  });

  it('with "strong" cache enabled (default), unmount/remount skips re-calculate', async () => {
    const rendered = render(<TestApp keys={[1, 2, 3]} />);
    expect(recalcFamilyCount).toBe(3);
    expect(recalcAtomCount).toBe(3);

    rendered.rerender(<TestApp keys={[1, 2, 3]} />);
    expect(recalcFamilyCount).toBe(3);
    expect(recalcAtomCount).toBe(3);

    // After unmount, part of the atomFamily cache can still be used when re-mounting.
    rendered.unmount();
    await gc();

    render(<TestApp keys={[3, 2, 1]} />);
    expect(recalcFamilyCount).toBeLessThan(6);
    expect(recalcAtomCount).toBeLessThan(6);
  });

  it('with "strong" cache disabled, unmount/remount might re-calculate', async () => {
    let count = 0;
    const family = atomFamilyWeak(
      (key: number) => {
        count += 1;
        return atom(_get => key);
      },
      {useStrongCache: false},
    );

    const rendered = render(<TestApp keys={[1, 2, 3]} family={family} />);
    expect(count).toBe(3);

    rendered.unmount();
    await gc();

    render(<TestApp keys={[1, 2, 3]} family={family} />);
    expect(count).toBe(6);
  });

  it('"cleanup" does not clean in-use states', () => {
    const rendered = render(<TestApp keys={[1, 2, 3]} />);
    expect(recalcFamilyCount).toBe(3);
    expect(recalcAtomCount).toBe(3);
    family.cleanup();

    // re-render can still use cached state after "cleanup" (count remains the same).
    rendered.rerender(<TestApp keys={[1, 2, 3]} />);
    expect(recalcFamilyCount).toBe(3);
    expect(recalcAtomCount).toBe(3);
  });

  it('"cleanup" can release memory', async () => {
    const rendered = render(<TestApp keys={[1, 2, 3]} />);
    expect(recalcFamilyCount).toBe(3);
    expect(recalcAtomCount).toBe(3);
    rendered.unmount();

    await gc();
    family.cleanup();
    await gc();
    family.cleanup();

    // umount, then re-render will recalculate all atoms (count increases).
    render(<TestApp keys={[3, 2, 1]} />);
    expect(recalcFamilyCount).toBe(6);
    expect(recalcAtomCount).toBe(6);
  });

  it('"clear" releases memory', () => {
    const rendered = render(<TestApp keys={[1, 2, 3]} />);
    expect(recalcFamilyCount).toBe(3);
    expect(recalcAtomCount).toBe(3);
    family.clear();

    // re-render will recalculate all atoms.
    rendered.rerender(<TestApp keys={[1, 2, 3]} />);
    expect(recalcFamilyCount).toBe(6);
    expect(recalcAtomCount).toBe(6);
    rendered.unmount();
    family.clear();

    // umount, then re-render will recalculate all atoms (count increases).
    render(<TestApp keys={[3, 2, 1]} />);
    expect(recalcFamilyCount).toBe(9);
    expect(recalcAtomCount).toBe(9);
  });

  it('"cleanup" runs automatically to reduce cache size', async () => {
    const N = 10;
    const M = 30;

    // Render N items.
    const rendered = render(<TestApp keys={Array.from({length: N}, (_, i) => i)} />);

    // Umount to drop references to the atoms.
    rendered.unmount();

    // Force GC to run to stabilize the test.
    await gc();

    // After GC, render M items with different keys.
    // This would trigger `family.cleanup` transparently.
    render(<TestApp keys={Array.from({length: M}, (_, i) => N + i)} />);

    // Neither of the caches should have N + M items (which means no cleanup).
    expect(family.weakCache.size).toBeLessThan(N + M);
    expect(family.strongCache.size).toBeLessThan(N + M);
  });

  it('provides debugLabel', () => {
    const family = atomFamilyWeak((v: string) => atom(v));
    family.debugLabel = 'prefix1';
    const atom1 = family('a');
    expect(atom1.debugLabel).toBe('prefix1:a');
  });
});

describe('useAtomGet and useAtomSet', () => {
  const initialMap = new Map([
    ['a', 1],
    ['b', 2],
    ['c', 3],
  ]);
  const initialSet = new Set(['a', 'b']);

  // Render an App, change the map and set, check what
  // Runs a test and report re-render and atom states.
  // insertMap specifies changes to the map (initially {a: 1, b: 2, c: 3}).
  // changeSet specifies changes to the set (initially {a, b}).
  function findRerender(props: {
    insertMap?: Iterable<[string, number]>;
    replaceSet?: Iterable<string>;
  }): Array<string> {
    // container types
    const map = atom<Map<string, number>>(initialMap);
    const set = atom<Set<string>>(initialSet);
    const rerenderKeys = new Set<string>();

    // test UI components
    function Item({k}: {k: string}) {
      const mapValue = useAtomGet(map, k);
      const setValue = useAtomHas(set, k);
      rerenderKeys.add(k);
      return (
        <span>
          {mapValue} {setValue}
        </span>
      );
    }

    const store = createStore();

    function TestApp({keys}: {keys: Array<string>}) {
      return (
        <StrictMode>
          <Provider store={store}>
            {keys.map(k => (
              <Item k={k} key={k} />
            ))}
          </Provider>
        </StrictMode>
      );
    }

    const keys = ['a', 'b', 'c'];
    render(<TestApp keys={keys} />);

    rerenderKeys.clear();

    const {insertMap, replaceSet} = props;

    act(() => {
      if (insertMap) {
        store.set(map, oldMap => new Map([...oldMap, ...insertMap]));
      }
      if (replaceSet) {
        const newSet = new Set([...replaceSet]);
        store.set(set, newSet);
      }
    });

    return [...rerenderKeys];
  }

  it('avoids re-rendering with changing to unrelated keys', () => {
    expect(findRerender({insertMap: [['unrelated-key', 3]]})).toEqual([]);
    expect(findRerender({replaceSet: [...initialSet, 'unrelated-key']})).toEqual([]);
  });

  it('only re-render changed items', () => {
    const replaceSet = [...initialSet, 'c']; // add 'c' to the set.
    expect(findRerender({insertMap: [['b', 5]]})).toEqual(['b']);
    expect(findRerender({replaceSet})).toEqual(['c']);
    expect(findRerender({insertMap: [['b', 5]], replaceSet})).toEqual(['b', 'c']);
  });
});

describe('useAtomSelect', () => {
  // A derived atom per row is a dependent Jotai walks on every write to the shared atom, which made
  // writes cost seconds with thousands of rows. Rows must subscribe as listeners instead.
  it('subscribes rows as listeners, not as dependent atoms', () => {
    const sizes = atom(new Map([['a', 1]]));
    const store = createStore();
    function Row({k}: {k: string}) {
      const size = useAtomSelect(sizes, map => map.get(k) ?? 0, [k]);
      return <span>{size}</span>;
    }
    render(
      <Provider store={store}>
        {['a', 'b', 'c'].map(k => (
          <Row k={k} key={k} />
        ))}
      </Provider>,
    );

    const mounted = store.dev_get_mounted?.(sizes);
    // A subscribed atom counts among its own dependents.
    expect([...(mounted?.t ?? [])].filter(dependent => dependent !== sizes)).toEqual([]);
    expect(mounted?.l.size).toBe(3);
  });

  it('re-renders when the selected value or a dependency changes', () => {
    const counts = atom(new Map([['a', 1]]));
    const store = createStore();
    const renders: Array<string> = [];
    function Row({k, scale}: {k: string; scale: number}) {
      const value = useAtomSelect(counts, map => (map.get(k) ?? 0) * scale, [k, scale]);
      renders.push(`${k}=${value}`);
      return <span>{value}</span>;
    }
    const {rerender} = render(
      <Provider store={store}>
        <Row k="a" scale={1} />
      </Provider>,
    );
    renders.length = 0;

    act(() =>
      store.set(
        counts,
        new Map([
          ['a', 1],
          ['b', 2],
        ]),
      ),
    );
    expect(renders).toEqual([]);

    act(() => store.set(counts, new Map([['a', 3]])));
    expect(renders).toEqual(['a=3']);

    rerender(
      <Provider store={store}>
        <Row k="a" scale={2} />
      </Provider>,
    );
    expect(renders.at(-1)).toBe('a=6');
  });
});

describe('atomResetOnDepChange', () => {
  it('works like a primitive atom', () => {
    const depAtom = atom(0);
    const testAtom = atomResetOnDepChange(1, depAtom);
    const doubleAtom = atom(get => get(testAtom) * 2);
    expect(readAtom(doubleAtom)).toBe(2);
    expect(readAtom(testAtom)).toBe(1);
    writeAtom(testAtom, 2);
    expect(readAtom(doubleAtom)).toBe(4);
    expect(readAtom(testAtom)).toBe(2);
  });

  it('gets reset on dependency change', () => {
    const depAtom = atom(0);
    const testAtom = atomResetOnDepChange(1, depAtom);

    writeAtom(testAtom, 2);

    // Change depAtom should reset testAtom.
    writeAtom(depAtom, 10);
    expect(readAtom(testAtom)).toBe(1);

    // Set depAtom to the same value does not reset testAtom.
    writeAtom(testAtom, 3);
    writeAtom(depAtom, 10);
    expect(readAtom(testAtom)).toBe(3);
  });
});

describe('localStorageBackedAtomFamily', () => {
  function setupTestPlatform<T>(initialValues: Record<string, T>): Platform {
    const state = {...initialValues};
    const mockPlatform = {
      getAllPersistedState: jest.fn().mockImplementation((): Record<string, T> => {
        return state;
      }),
      getPersistedState: jest.fn().mockImplementation((key: string): T => {
        return state[key];
      }),
      setPersistedState: jest.fn().mockImplementation((key: string, value: T) => {
        state[key] = value;
      }),
    } as Partial<Platform> as Platform;
    return mockPlatform;
  }

  const testKey = 'test_' as LocalStorageName;

  it('loads initial state from storage', () => {
    const mockPlatform = setupTestPlatform({test_a: {data: 1, date: Date.now()}});
    const family = localStorageBackedAtomFamily(testKey, () => 0, 1, mockPlatform);

    expect(readAtom(family('a'))).toEqual(1);
    expect(readAtom(family('b'))).toEqual(0);
  });

  it('evicts old initial state from storage', () => {
    const mockPlatform = setupTestPlatform({
      test_a: {data: 1, date: 0},
      test_b: {data: 2, date: Date.now()},
    });
    const family = localStorageBackedAtomFamily(testKey, () => 0, 1, mockPlatform);

    expect(readAtom(family('a'))).toEqual(0);
    expect(readAtom(family('b'))).toEqual(2);
  });

  it('writes to persisted state', () => {
    const mockPlatform = setupTestPlatform<number>({});
    const family = localStorageBackedAtomFamily(testKey, (): number => 0, 1, mockPlatform);

    expect(readAtom(family('a'))).toEqual(0);
    writeAtom(family('a'), 2);
    expect(readAtom(family('a'))).toEqual(2);
    expect(mockPlatform.setPersistedState).toHaveBeenCalledWith('test_a', {
      data: 2,
      date: expect.any(Number),
    });
  });

  it("gets the latest data even after writing and then being gc'd", () => {
    const mockPlatform = setupTestPlatform({test_a: {data: 1, date: Date.now()}});
    const family = localStorageBackedAtomFamily(testKey, (): number => 0, 1, mockPlatform);

    expect(readAtom(family('a'))).toEqual(1);
    writeAtom(family('a'), 2);
    expect(readAtom(family('a'))).toEqual(2);
    family.clear();
    expect(readAtom(family('a'))).toEqual(2);
  });

  it('writing undefined clears from persisted storage', () => {
    const mockPlatform = setupTestPlatform({test_a: {data: 1, date: Date.now()}});
    const family = localStorageBackedAtomFamily(testKey, () => 0, 1, mockPlatform);

    // type check that although we can SET to undefined, we can't READ undefined
    const returnTypeShouldBeNonNull: number = readAtom(family('a'));
    expect(returnTypeShouldBeNonNull).toEqual(1);

    writeAtom(family('a'), undefined);

    expect(mockPlatform.setPersistedState).toHaveBeenCalledWith('test_a', undefined);
    expect(mockPlatform.getPersistedState('test_a')).toBeUndefined(); // nothing is stored
    expect(readAtom(family('a'))).toEqual(0); // read still gives default value
  });
});

describe('localStorageBackedAtom', () => {
  afterEach(() => {
    jest.restoreAllMocks();
  });

  it('tells the platform which settings are portable, with their default, and nothing for others', () => {
    const get = jest.spyOn(platform, 'getPersistedState').mockReturnValue(null);
    const set = jest.spyOn(platform, 'setPersistedState').mockImplementation(() => {});
    const validate = (zoom: unknown) => zoom !== 9;
    const zoom = localStorageBackedAtom<number>('isl.ui-zoom', 1, {portable: true, validate});
    const focus = localStorageBackedAtom<boolean>('isl.focus-mode', false);

    expect(get.mock.calls).toEqual([
      ['isl.ui-zoom', {portable: true, validate, defaultValue: 1}],
      ['isl.focus-mode'],
    ]);
    writeAtom(zoom, 1.2);
    writeAtom(focus, true);
    expect(set.mock.calls).toEqual([
      ['isl.ui-zoom', 1.2, {portable: true, validate, defaultValue: 1}],
      ['isl.focus-mode', true],
    ]);
  });
});
