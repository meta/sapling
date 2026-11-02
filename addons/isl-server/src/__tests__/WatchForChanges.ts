/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import type {Client} from 'fb-watchman';
import type {RepoInfo} from 'isl/src/types';
import type {EdenFSNotifications} from '../edenFsNotifications';
import type {ServerPlatform} from '../serverPlatform';
import type {Watchman} from '../watchman';

import fs from 'node:fs';
import {TypedEventEmitter} from 'shared/TypedEventEmitter';
import {mockLogger} from 'shared/testUtils';
import {PageFocusTracker} from '../PageFocusTracker';
import {WatchForChanges} from '../WatchForChanges';
import {makeServerSideTracker} from '../analytics/serverSideTracker';
import {ONE_MINUTE_MS} from '../constants';

const mockTracker = makeServerSideTracker(
  mockLogger,
  {platformName: 'test'} as ServerPlatform,
  '0.1',
  jest.fn(),
);

jest.mock('fb-watchman', () => {
  // make a fake watchman object which returns () => undefined for every property
  // so we don't need to manually mock every function watchman provides.
  class FakeWatchman {
    constructor() {
      return new Proxy(this, {
        get: () => () => undefined,
      });
    }
  }
  return {
    Client: FakeWatchman,
  };
});

describe('WatchForChanges - watchman', () => {
  const mockInfo: RepoInfo = {
    type: 'success',
    command: 'sl',
    repoRoot: '/testRepo',
    dotdir: '/testRepo/.sl',
    codeReviewSystem: {type: 'unknown'},
    pullRequestDomain: undefined,
    isEdenFs: false,
  };

  let focusTracker: PageFocusTracker;
  const onChange = jest.fn();
  let watch: WatchForChanges;

  beforeEach(() => {
    const ctx = {
      cmd: 'sl',
      cwd: '/path/to/cwd',
      logger: mockLogger,
      tracker: mockTracker,
    };

    jest.useFakeTimers();
    onChange.mockClear();

    jest.spyOn(fs.promises, 'realpath').mockImplementation((path, _opts) => {
      return Promise.resolve(path as string);
    });

    focusTracker = new PageFocusTracker();
    watch = new WatchForChanges(mockInfo, focusTracker, onChange, ctx);
    // pretend watchman is not running for most tests
    (watch.watchman.status as string) = 'errored';
    // change is triggered on first subscription
    expect(onChange).toHaveBeenCalledTimes(1);
    onChange.mockClear();
  });

  afterEach(() => {
    watch.dispose();
  });

  afterAll(() => {
    jest.useRealTimers();
  });

  it('polls for changes on an interval', () => {
    // |-----------1-----------2-----------3-----------4-----------5-----------6-----------7-----------8-----------9----------10----------11----------12----------13----------14----------15----------16 (minutes)
    //                                                                                                                                                                                          ^ poll
    expect(onChange).not.toHaveBeenCalled();
    jest.advanceTimersByTime(15.5 * ONE_MINUTE_MS);
    expect(onChange).toHaveBeenCalledTimes(1);
    expect(onChange).toHaveBeenCalledWith('everything', undefined);
  });

  it('polls more often when the page is visible', () => {
    // |-----------1-----------2-----------3-----------4---- (minutes)
    //       |               ^           |           ^       (poll)
    //       0              poll        2                   (times fetched)
    focusTracker.setState('page0', 'visible');
    onChange.mockClear(); // ignore immediate visibility change poll
    expect(onChange).not.toHaveBeenCalled();
    jest.advanceTimersByTime(1.0 * ONE_MINUTE_MS);
    expect(onChange).not.toHaveBeenCalled();
    jest.advanceTimersByTime(1.25 * ONE_MINUTE_MS);
    expect(onChange).toHaveBeenCalledTimes(1);
  });
  it('polls more often when the page is focused', () => {
    // |-----------1-----------2---- (minutes)
    //  | ^| ^| ^  ^  ^  ^  ^  ^  ^  (poll)
    //  0  1                         (times fetched)
    focusTracker.setState('page0', 'focused');
    onChange.mockClear(); // ignore immediate focus change poll
    expect(onChange).not.toHaveBeenCalled();
    jest.advanceTimersByTime(0.25 * ONE_MINUTE_MS);
    expect(onChange).not.toHaveBeenCalled();
    jest.advanceTimersByTime(0.25 * ONE_MINUTE_MS);
    expect(onChange).toHaveBeenCalledTimes(1);
  });

  it('polls the moment visibility is gained', () => {
    // |-----------1-----*-----2-----------3-----------4----------- (minutes)
    //             |     ||       |           ^       |        ^     (poll)
    //             0     |1       1           2                3     (times fetched)
    //                visible
    //        (resets interval at 2min)
    jest.advanceTimersByTime(1.5 * ONE_MINUTE_MS);
    expect(onChange).not.toHaveBeenCalled();
    focusTracker.setState('page0', 'visible');
    expect(onChange).toHaveBeenCalledTimes(1);
    jest.advanceTimersByTime(0.75 * ONE_MINUTE_MS);
    expect(onChange).toHaveBeenCalledTimes(1);
    jest.advanceTimersByTime(1.25 * ONE_MINUTE_MS);
    expect(onChange).toHaveBeenCalledTimes(2);
  });

  it('debounces additional polling when focus is gained', () => {
    // |-----------1-----*--*--2-*---------3-----------4----------- (minutes)
    //             |     || |    ||           ^       |        ^     (poll)
    //             0     |1 |    |1           2                3     (times fetched)
    //            visible^  |    ^visible (debounce)
    //                     hide
    jest.advanceTimersByTime(1.5 * ONE_MINUTE_MS);
    expect(onChange).not.toHaveBeenCalled();
    focusTracker.setState('page0', 'visible');
    expect(onChange).toHaveBeenCalledTimes(1);
    jest.advanceTimersByTime(0.1 * ONE_MINUTE_MS);
    focusTracker.setState('page0', 'hidden');
    jest.advanceTimersByTime(0.15 * ONE_MINUTE_MS); // 15 seconds (0.25 min) throttle for focus
    focusTracker.setState('page0', 'visible'); // debounced to not fetch again
    expect(onChange).toHaveBeenCalledTimes(1);
    jest.advanceTimersByTime(0.5 * ONE_MINUTE_MS);
    expect(onChange).toHaveBeenCalledTimes(1);
    jest.advanceTimersByTime(1.25 * ONE_MINUTE_MS);
    expect(onChange).toHaveBeenCalledTimes(2);
  });

  it('polls at higher frequency if any page is focused', () => {
    // |-----------1-----*-*---2-----------3-- (minutes)
    //             |     ||| ^  ^  ^  ^  ^     (poll)
    //             0     |1|  2     3          (times fetched)
    //            hidden | |hidden
    //            focused^ |hidden
    //            hidden   ^focused
    jest.advanceTimersByTime(1.5 * ONE_MINUTE_MS);
    expect(onChange).not.toHaveBeenCalled();
    focusTracker.setState('page0', 'hidden');
    focusTracker.setState('page1', 'hidden');
    focusTracker.setState('page2', 'hidden');
    focusTracker.setState('page1', 'focused');
    expect(onChange).toHaveBeenCalledTimes(1);
    jest.advanceTimersByTime(0.5 * ONE_MINUTE_MS);
    expect(onChange).toHaveBeenCalledTimes(2);
    focusTracker.setState('page0', 'focused'); // since 1 is still focused, this does not immediately poll
    focusTracker.setState('page1', 'hidden');
    expect(onChange).toHaveBeenCalledTimes(2);
    jest.advanceTimersByTime(0.5 * ONE_MINUTE_MS);
    expect(onChange).toHaveBeenCalledTimes(3);
  });

  it('clears out previous intervals', () => {
    // |-----------1-----*-----2-----*-----3---   ...   --7-----------8-- (minutes)
    //             |     |     ^     ^                          ^         (poll)
    //             0     1     2     3                          4         (times fetched)
    //            focused^           ^hidden
    jest.advanceTimersByTime(1.5 * ONE_MINUTE_MS);
    expect(onChange).not.toHaveBeenCalled();
    focusTracker.setState('page0', 'focused');
    expect(onChange).toHaveBeenCalledTimes(1);
    jest.advanceTimersByTime(1 * ONE_MINUTE_MS);
    expect(onChange).toHaveBeenCalledTimes(3);
    focusTracker.setState('page0', 'hidden');
    // fast focused interval is removed, and we revert to 15 min interval
    jest.advanceTimersByTime(15 * ONE_MINUTE_MS);
    expect(onChange).toHaveBeenCalledTimes(4);
  });

  it('polls less when watchman appears healthy', () => {
    // |*----------1-----------2-----------3-----------4-----------5-----------6-----------7-----------8-----------9----------10----------11----------12----------13----------14----------15----------16 (minutes)
    //  |                                                                                                                                                                                         ^ poll
    //  focused

    (watch.watchman.status as string) = 'healthy';
    focusTracker.setState('page0', 'focused');
    expect(onChange).toHaveBeenCalledTimes(0);
    jest.advanceTimersByTime(15.5 * ONE_MINUTE_MS);
    expect(onChange).toHaveBeenCalledTimes(1);
  });

  it('results from watchman reset polling timers', async () => {
    // |-----------1-----------2----- ... ----15----------16 (minutes)
    //                  |                            ^       (poll)
    //            watchman result

    const ctx = {
      cmd: 'sl',
      cwd: '/path/to/cwd',
      logger: mockLogger,
      tracker: mockTracker,
    };
    watch.dispose(); // don't use pre-existing WatchForChanges
    const emitter1 = new TypedEventEmitter();
    const emitter2 = new TypedEventEmitter();
    const mockWatchman: Watchman = {
      client: {} as unknown as Client,
      status: 'initializing',
      watchDirectoryRecursive: jest
        .fn()
        .mockImplementationOnce(() => {
          return Promise.resolve({emitter: emitter1});
        })
        .mockImplementationOnce(() => {
          return Promise.resolve({emitter: emitter2});
        }),
      unwatch: jest.fn(),
    } as unknown as Watchman;

    watch = new WatchForChanges(mockInfo, focusTracker, onChange, ctx, mockWatchman);
    await watch.waitForDirstateSubscriptionReady();
    await watch.setupSubscriptions();
    expect(onChange).toHaveBeenCalledTimes(1);
    onChange.mockClear();

    // wait an actual async tick so mock subscriptions are set up
    const setImmediate = jest.requireActual('timers').setImmediate;
    await new Promise(res => setImmediate(() => res(undefined)));

    jest.advanceTimersByTime(1.5 * ONE_MINUTE_MS);
    expect(onChange).toHaveBeenCalledTimes(0);
    emitter2.emit('change', undefined);
    expect(onChange).toHaveBeenCalledTimes(1);
    expect(onChange).toHaveBeenCalledWith('uncommitted changes');
    jest.advanceTimersByTime(14.0 * ONE_MINUTE_MS); // original timer didn't cause a poll
    expect(onChange).toHaveBeenCalledTimes(1);
    jest.advanceTimersByTime(2.0 * ONE_MINUTE_MS); // 15 minutes after watchman change, a new poll occurred
    expect(onChange).toHaveBeenCalledTimes(2);
    expect(onChange).toHaveBeenCalledWith('everything', undefined);
  });
});

describe('WatchForChanges - edenfs', () => {
  const mockInfo: RepoInfo = {
    type: 'success',
    command: 'sl',
    repoRoot: '/testRepo',
    dotdir: '/testRepo/.sl',
    codeReviewSystem: {type: 'unknown'},
    pullRequestDomain: undefined,
    isEdenFs: true,
  };

  let focusTracker: PageFocusTracker;
  const onChange = jest.fn();
  let watch: WatchForChanges;
  let emitter1: TypedEventEmitter<string, unknown>;

  beforeEach(async () => {
    const ctx = {
      cmd: 'sl',
      cwd: '/path/to/cwd',
      logger: mockLogger,
      tracker: mockTracker,
    };

    jest.useFakeTimers();
    onChange.mockClear();

    jest.spyOn(fs.promises, 'realpath').mockImplementation((path, _opts) => {
      return Promise.resolve(path as string);
    });

    emitter1 = new TypedEventEmitter();
    const mockEdenFS: EdenFSNotifications = {
      watchDirectoryRecursive: jest
        .fn()
        .mockImplementation(
          (_localDirectoryPath, _rawSubscriptionName, _subscriptionOptions, callback) => {
            emitter1.on('change', change => {
              callback(null, change);
            });
            emitter1.on('error', error => {
              callback(error, null);
            });
            emitter1.on('close', () => {
              callback(null, null);
            });
            return Promise.resolve(emitter1);
          },
        ),
      unwatch: jest.fn(),
    } as unknown as EdenFSNotifications;

    focusTracker = new PageFocusTracker();
    watch = new WatchForChanges(mockInfo, focusTracker, onChange, ctx, undefined, mockEdenFS);
    await watch.waitForDirstateSubscriptionReady();

    // change is triggered on first subscription
    expect(onChange).toHaveBeenCalledTimes(1);
    onChange.mockClear();
  });

  afterEach(() => {
    watch.dispose();
  });

  afterAll(() => {
    jest.useRealTimers();
  });

  it('Handles results from edenfs', async () => {
    // |-----------1-----------2-----------3-----------4-----------5-----------6-----------7-----------8-----------9----------10----------11----------12----------13----------14----------15----------16 (minutes)
    //                                                                                                                                                                                          ^ poll
    focusTracker.setState('page', 'visible');
    await watch.setupSubscriptions();

    // wait an actual async tick so mock subscriptions are set up
    const setImmediate = jest.requireActual('timers').setImmediate;
    await new Promise(res => setImmediate(() => res(undefined)));

    jest.advanceTimersByTime(1.5 * ONE_MINUTE_MS);
    expect(onChange).toHaveBeenCalledTimes(0);
    emitter1.emit('change', {changes: [1]});
    expect(onChange).toHaveBeenCalledTimes(1);
    jest.advanceTimersByTime(13.0 * ONE_MINUTE_MS); // original timer didn't cause a poll
    expect(onChange).toHaveBeenCalledTimes(1);
    jest.advanceTimersByTime(3.0 * ONE_MINUTE_MS); // 15 minutes after watchman change, a new poll occurred
    expect(onChange).toHaveBeenCalledTimes(2);
    expect(onChange).toHaveBeenCalledWith('everything', undefined);

    focusTracker.setState('page', 'hidden');
    jest.advanceTimersByTime(180 * ONE_MINUTE_MS);
    expect(onChange).toHaveBeenCalledTimes(3);
    expect(onChange).toHaveBeenCalledWith('everything', undefined);
  });
});
