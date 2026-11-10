/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import fs from 'node:fs';
import {defer, sleep} from 'shared/utils';
import {canonicalizePath, serializeAsyncCall} from '../utils';

function flushPromises() {
  return new Promise(res => jest.requireActual('timers').setTimeout(res, 0));
}

describe('canonicalizePath', () => {
  function mockRealpath(impl: (p: string) => string) {
    return jest
      .spyOn(fs, 'realpath')
      .mockImplementation(((p: string, cb: (err: null, resolved: string) => void) =>
        cb(null, impl(p))) as unknown as typeof fs.realpath);
  }

  it('resolves symlinks via realpath', async () => {
    mockRealpath(p => p.replace(/^\/home\/name\//, '/data/users/name/'));
    await expect(canonicalizePath('/home/name/myRepo')).resolves.toBe('/data/users/name/myRepo');
  });

  it('falls back to the input path when realpath fails', async () => {
    jest
      .spyOn(fs, 'realpath')
      .mockImplementation(((_p: string, cb: (err: Error) => void) =>
        cb(new Error('ENOENT'))) as unknown as typeof fs.realpath);
    await expect(canonicalizePath('/nonexistent/path')).resolves.toBe('/nonexistent/path');
  });
});

describe('serializeAsyncCall', () => {
  it.each(['resolve', 'reject'] as const)(
    'keeps calls serialized when the running call settles via %s',
    async settlement => {
      const firstResult = defer<number>();
      const secondResult = defer<number>();
      const thirdResult = defer<number>();
      const results = [firstResult, secondResult, thirdResult];
      let running = 0;
      let maxRunning = 0;
      let nextId = 0;
      const runner = jest.fn(() => {
        running++;
        maxRunning = Math.max(maxRunning, running);
        return results[nextId++].promise.finally(() => running--);
      });
      const testFn = serializeAsyncCall(runner);
      const first = testFn();
      const queued = testFn();
      let reentered: Promise<number> | undefined;
      const callAgain = () => {
        reentered = testFn();
      };
      const reentry = first.then(callAgain, callAgain);

      if (settlement === 'resolve') {
        firstResult.resolve(1);
      } else {
        firstResult.reject(new Error('failed'));
      }
      await reentry;
      secondResult.resolve(2);
      thirdResult.resolve(3);
      await Promise.all([queued, reentered]);

      expect(maxRunning).toBe(1);
      expect(runner).toHaveBeenCalledTimes(2);
      expect(reentered).toBe(queued);
      await expect(queued).resolves.toBe(2);
    },
  );

  it('only runs one at a time', async () => {
    jest.useFakeTimers();
    const runTime = async (t: number) => {
      jest.advanceTimersByTime(t);
      await flushPromises();
    };
    let nextId = 0;
    const started: Array<number> = [];
    const finished: Array<number> = [];
    const testFn = serializeAsyncCall(async () => {
      const id = nextId++;
      started.push(id);
      await sleep(40);
      finished.push(id);
    });

    testFn(); // This one is run immediately
    expect(started).toEqual([0]);
    expect(finished).toEqual([]); // not finished running
    await runTime(10);
    testFn(); // this one queues up while the first is still running
    expect(started).toEqual([0]); // 1 not running yet
    expect(finished).toEqual([]);
    await runTime(10);
    testFn(); // we already have an invocation queued,
    expect(started).toEqual([0]);
    expect(finished).toEqual([]);

    await runTime(60);
    expect(started).toEqual([0, 1]);
    expect(finished).toEqual([0]);

    await runTime(60);
    expect(started).toEqual([0, 1]);
    expect(finished).toEqual([0, 1]);

    // nothing more to run
    await runTime(100);
    expect(started).toEqual([0, 1]);
    expect(finished).toEqual([0, 1]);
  });

  it('returns the same result beyond being queued once', async () => {
    jest.useFakeTimers();
    let callNumber = 1;
    const testFn = serializeAsyncCall(() => {
      return Promise.resolve(callNumber++);
    });

    const promise1 = testFn();
    const promise2 = testFn();
    const promise3 = testFn();

    const values = await Promise.all([promise1, promise2, promise3]);
    expect(values).toEqual([1, 2, 2]);
  });
});
