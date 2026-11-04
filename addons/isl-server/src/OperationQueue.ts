/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import type {
  OperationCommandProgressReporter,
  OperationProgress,
  RunnableOperation,
} from 'isl/src/types';
import type {Deferred} from 'shared/utils';
import type {ServerSideTracker} from './analytics/serverSideTracker';
import type {RepositoryContext} from './serverTypes';

import {clearTrackedCache} from 'shared/LRU';
import {defer} from 'shared/utils';

/**
 * Handle running & queueing all Operations so that only one Operation runs at once.
 * Operations may be run by sl in the Repository or other providers like ghstack in the RemoteRepository.
 */
export class OperationQueue {
  constructor(
    private runCallback: (
      ctx: RepositoryContext,
      operation: RunnableOperation,
      handleProgress: OperationCommandProgressReporter,
      signal: AbortSignal,
    ) => Promise<unknown>,
  ) {}

  private queuedOperations: Array<RunnableOperation & {tracker: ServerSideTracker}> = [];
  private runningOperation: RunnableOperation | undefined = undefined;
  private runningOperationStartTime: Date | undefined = undefined;
  private abortController: AbortController | undefined = undefined;
  private deferredOperations = new Map<string, Deferred<'ran' | 'skipped'>>();

  /**
   * Run an operation, or if one is already running, add it to the queue.
   * Promise resolves with:
   * - 'ran', when the operation exits (no matter success/failure), even if it was enqueued.
   * - 'skipped', when the operation is never going to be run, since an earlier queued command errored.
   */
  async runOrQueueOperation(
    ctx: RepositoryContext,
    operation: RunnableOperation,
    onProgress: (progress: OperationProgress) => void,
  ): Promise<'ran' | 'skipped'> {
    const {tracker, logger} = ctx;
    if (this.runningOperation != null) {
      this.queuedOperations.push({...operation, tracker});
      const deferred = defer<'ran' | 'skipped'>();
      this.deferredOperations.set(operation.id, deferred);
      onProgress({id: operation.id, kind: 'queue', queue: this.queuedOperations.map(o => o.id)});

      logger.info('queued operation:', operation.args.join(' '));
      return deferred.promise;
    }
    this.runningOperation = operation;
    this.runningOperationStartTime = new Date();

    const handleCommandProgress: OperationCommandProgressReporter = (...args) => {
      switch (args[0]) {
        case 'spawn':
          onProgress({
            id: operation.id,
            kind: 'spawn',
            queue: this.queuedOperations.map(op => op.id),
          });
          break;
        case 'stdout':
          onProgress({id: operation.id, kind: 'stdout', message: args[1]});
          break;
        case 'progress':
          onProgress({id: operation.id, kind: 'progress', progress: args[1]});
          break;
        case 'inlineProgress':
          onProgress({id: operation.id, kind: 'inlineProgress', hash: args[1], message: args[2]});
          break;
        case 'warning':
          onProgress({id: operation.id, kind: 'warning', warning: args[1]});
          break;
        case 'stderr':
          onProgress({id: operation.id, kind: 'stderr', message: args[1]});
          break;
        case 'exit':
          onProgress({id: operation.id, kind: 'exit', exitCode: args[1], timestamp: Date.now()});
          break;
      }
    };

    try {
      const controller = new AbortController();
      this.abortController = controller;
      await tracker.operation(
        operation.trackEventName,
        'RunOperationError',
        {extras: {args: operation.args, runner: operation.runner}, operationId: operation.id},
        _p => this.runCallback(ctx, operation, handleCommandProgress, controller.signal),
      );
    } catch (err) {
      const errString = (err as Error).toString();
      logger.info('error running operation: ', operation.args[0], errString);
      onProgress({id: operation.id, kind: 'error', error: errString});

      if (this.queuedOperations.length > 0) {
        logger.info('the above error removed queued operations:');
        // clear queue to run when we hit an error,
        // which also requires resolving all their promises
        for (const queued of this.queuedOperations) {
          logger.info(' >  ', queued.args.join(' ')); // print out commands that are skipped, in case it's useful to debug or recover data
          this.resolveDeferredPromise(queued.id, 'skipped');
        }
      }
      this.queuedOperations = [];
    } finally {
      this.runningOperationStartTime = undefined;
      this.runningOperation = undefined;

      // resolve original enqueuer's promise
      this.resolveDeferredPromise(operation.id, 'ran');
    }

    // now that we successfully ran this operation, dequeue the next
    if (this.queuedOperations.length > 0) {
      const op = this.queuedOperations.shift();
      if (op != null) {
        // don't await this, the caller should resolve when the original operation finishes.
        this.runOrQueueOperation(
          ctx,
          op,
          // TODO: we're using the onProgress from the LAST `runOperation`... should we be keeping the newer onProgress in the queued operation?
          onProgress,
        );
      }
    } else {
      // Attempt to free some memory.
      clearTrackedCache();
    }

    return 'ran';
  }

  private resolveDeferredPromise(id: string, kind: 'ran' | 'skipped') {
    const found = this.deferredOperations.get(id);
    if (found != null) {
      found.resolve(kind);
      this.deferredOperations.delete(id);
    }
  }

  /**
   * Get the running operation start time.
   * Returns `undefined` if there is no running operation.
   */
  getRunningOperationStartTime(): Date | undefined {
    if (this.runningOperation == null) {
      return undefined;
    }
    return this.runningOperationStartTime;
  }

  /**
   * Send kill signal to the running operation if the operationId matches.
   * If the process exits, the exit event will be noticed by the queue.
   * This function does not block on waiting for the operation process to exit.
   */
  abortRunningOperation(operationId: string) {
    if (this.runningOperation?.id == operationId) {
      this.abortController?.abort();
    }
  }

  /** The currently running operation. */
  getRunningOperation(): RunnableOperation | undefined {
    return this.runningOperation;
  }
}
