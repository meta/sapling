/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import type {OperationCommandProgressReporter} from 'isl/src/types';
import type {ServerPlatform} from '../serverPlatform';
import type {RepositoryContext} from '../serverTypes';

import {CommandRunner} from 'isl/src/types';
import {mockLogger} from 'shared/testUtils';
import {defer} from 'shared/utils';
import {OperationQueue} from '../OperationQueue';
import {makeServerSideTracker} from '../analytics/serverSideTracker';

const mockTracker = makeServerSideTracker(
  mockLogger,
  {platformName: 'test'} as ServerPlatform,
  '0.1',
  jest.fn(),
);

const mockCtx: RepositoryContext = {
  agentSessionMetadata: undefined,
  cwd: 'cwd',
  cmd: 'sl',
  logger: mockLogger,
  tracker: mockTracker,
};

describe('OperationQueue', () => {
  it('runs command directly when nothing queued', async () => {
    const p = defer();
    const runCallback = jest.fn().mockImplementation(() => p.promise);
    const queue = new OperationQueue(runCallback);

    const onProgress = jest.fn();

    const runPromise = queue.runOrQueueOperation(
      mockCtx,
      {
        args: ['pull'],
        id: '1',
        runner: CommandRunner.Sapling,
        trackEventName: 'PullOperation',
      },
      onProgress,
    );
    // calls synchronously
    expect(runCallback).toHaveBeenCalledTimes(1);

    p.resolve(undefined);
    const result = await runPromise;
    expect(result).toEqual('ran');

    expect(runCallback).toHaveBeenCalledTimes(1);

    expect(onProgress).not.toHaveBeenCalledWith(expect.objectContaining({kind: 'queue'}));
  });

  it('sends spawn and info messages', async () => {
    const runCallback = jest
      .fn()
      .mockImplementation((_op, _cwd, prog: OperationCommandProgressReporter) => {
        prog('spawn');
        prog('stdout', 'hello');
        prog('stderr', 'err');
        prog('exit', 0);

        return Promise.resolve(undefined);
      });
    const queue = new OperationQueue(runCallback);

    const onProgress = jest.fn();
    const runPromise = queue.runOrQueueOperation(
      mockCtx,
      {
        args: ['pull'],
        id: '1',
        runner: CommandRunner.Sapling,
        trackEventName: 'PullOperation',
      },
      onProgress,
    );

    const result = await runPromise;
    expect(result).toEqual('ran');

    expect(onProgress).toHaveBeenCalledWith(
      expect.objectContaining({id: '1', kind: 'spawn', queue: []}),
    );
    expect(onProgress).toHaveBeenCalledWith(
      expect.objectContaining({id: '1', kind: 'stdout', message: 'hello'}),
    );
    expect(onProgress).toHaveBeenCalledWith(
      expect.objectContaining({id: '1', kind: 'stderr', message: 'err'}),
    );
    expect(onProgress).toHaveBeenCalledWith(
      expect.objectContaining({id: '1', kind: 'exit', exitCode: 0}),
    );
  });

  it('sends abort signal', async () => {
    const runCallback = jest.fn().mockImplementation((_op, _cwd, prog, signal: AbortSignal) => {
      const p = defer();
      signal.addEventListener('abort', () => {
        p.resolve(null);
        prog('exit', 130);
      });
      return p;
    });
    const onProgress = jest.fn();
    const queue = new OperationQueue(runCallback);
    const id = 'abc';
    const op = queue.runOrQueueOperation(
      mockCtx,
      {args: [], id, runner: CommandRunner.Sapling, trackEventName: 'RunOperation'},
      onProgress,
    );
    queue.abortRunningOperation('wrong-id');
    expect(onProgress).not.toHaveBeenCalled();
    queue.abortRunningOperation(id);
    const result = await op;
    expect(result).toEqual('ran');

    expect(onProgress).toHaveBeenCalledWith(
      expect.objectContaining({id, kind: 'exit', exitCode: 130}),
    );
  });

  it('queues up commands', async () => {
    const p1 = defer();
    const p2 = defer();
    const runP1 = jest.fn(() => p1.promise);
    const runP2 = jest.fn(() => p2.promise);
    const runCallback = jest.fn().mockImplementationOnce(runP1).mockImplementationOnce(runP2);
    const queue = new OperationQueue(runCallback);

    const onProgress = jest.fn();
    expect(runP1).not.toHaveBeenCalled();
    expect(runP2).not.toHaveBeenCalled();

    const runPromise1 = queue.runOrQueueOperation(
      mockCtx,
      {
        args: ['pull'],
        id: '1',
        runner: CommandRunner.Sapling,
        trackEventName: 'PullOperation',
      },
      onProgress,
    );
    expect(runP1).toHaveBeenCalled();
    expect(runP2).not.toHaveBeenCalled();

    const runPromise2 = queue.runOrQueueOperation(
      mockCtx,
      {
        args: ['rebase'],
        id: '2',
        runner: CommandRunner.Sapling,
        trackEventName: 'RebaseOperation',
      },
      onProgress,
    );
    expect(runP1).toHaveBeenCalled();
    expect(runP2).not.toHaveBeenCalled(); // it's queued up
    // should notify that the command queued when it is attempted to be run
    expect(onProgress).toHaveBeenCalledWith(expect.objectContaining({kind: 'queue', queue: ['2']}));

    p1.resolve(undefined);
    const result1 = await runPromise1;
    expect(result1).toEqual('ran');

    // now it's dequeued
    expect(runP2).toHaveBeenCalled();

    p2.resolve(undefined);
    const result2 = await runPromise2;
    expect(result2).toEqual('ran');

    expect(runCallback).toHaveBeenCalledTimes(2);
  });

  it('clears queue when an operation fails', async () => {
    const p1 = defer();
    const p2 = defer();
    const runP1 = jest.fn(() => p1.promise);
    const runP2 = jest.fn(() => p2.promise);
    const runCallback = jest.fn().mockImplementationOnce(runP1).mockImplementationOnce(runP2);
    const queue = new OperationQueue(runCallback);

    const onProgress = jest.fn();
    expect(runP1).not.toHaveBeenCalled();
    expect(runP2).not.toHaveBeenCalled();

    const runPromise1 = queue.runOrQueueOperation(
      mockCtx,
      {
        args: ['pull'],
        id: '1',
        runner: CommandRunner.Sapling,
        trackEventName: 'PullOperation',
      },
      onProgress,
    );
    expect(runP1).toHaveBeenCalled();
    expect(runP2).not.toHaveBeenCalled();
    const runPromise2 = queue.runOrQueueOperation(
      mockCtx,
      {
        args: ['rebase'],
        id: '2',
        runner: CommandRunner.Sapling,
        trackEventName: 'RebaseOperation',
      },
      onProgress,
    );
    expect(runP1).toHaveBeenCalled();
    expect(runP2).not.toHaveBeenCalled(); // it's queued up

    p1.reject(new Error('fake error'));
    // run promise still resolves, but error message was sent
    const result1 = await runPromise1;
    expect(result1).toEqual('ran');
    expect(onProgress).toHaveBeenCalledWith(
      expect.objectContaining({id: '1', kind: 'error', error: 'Error: fake error'}),
    );

    // p2 was cancelled by p1 failing
    expect(runP2).not.toHaveBeenCalled();
    const result2 = await runPromise2;
    expect(result2).toEqual('skipped');
    expect(runCallback).toHaveBeenCalledTimes(1);
  });

  it('can run commands again after an error', async () => {
    const p1 = defer();
    const p2 = defer();
    const runP1 = jest.fn(() => p1.promise);
    const runP2 = jest.fn(() => p2.promise);
    const runCallback = jest.fn().mockImplementationOnce(runP1).mockImplementationOnce(runP2);
    const queue = new OperationQueue(runCallback);

    const onProgress = jest.fn();
    expect(runP1).not.toHaveBeenCalled();
    expect(runP2).not.toHaveBeenCalled();

    const runPromise1 = queue.runOrQueueOperation(
      mockCtx,
      {
        args: ['pull'],
        id: '1',
        runner: CommandRunner.Sapling,
        trackEventName: 'PullOperation',
      },
      onProgress,
    );

    p1.reject(new Error('fake error'));
    await runPromise1;

    // after p1 errors, run another operation
    const runPromise2 = queue.runOrQueueOperation(
      mockCtx,
      {
        args: ['rebase'],
        id: '2',
        runner: CommandRunner.Sapling,
        trackEventName: 'RebaseOperation',
      },
      onProgress,
    );

    // p2 runs immediately
    p2.resolve(undefined);
    await runPromise2;
    expect(runP2).toHaveBeenCalled();
    expect(runCallback).toHaveBeenCalledTimes(2);
  });
});
