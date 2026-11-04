/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import type {EnsureAssignedTogether} from 'shared/EnsureAssignedTogether';
import type {Operation} from './operations/Operation';
import type {Disposable, Hash, ProgressStep, ServerToClientMessage} from './types';

import {useCallback} from 'react';
import {defer} from 'shared/utils';
import serverAPI from './ClientToServerAPI';
import {tracker} from './analytics';
import {readAtom, useAtomSelect, writeAtom} from './jotaiUtils';
import {atomResetOnCwdChange} from './repositoryData';
import {Timer} from './timer';
import {registerCleanup, registerDisposable, short} from './utils';

export type OperationInfo = {
  operation: Operation;
  startTime?: Date;
  commandOutput?: Array<string>;
  currentProgress?: ProgressStep;
  /** progress message shown next to a commit */
  inlineProgress?: Map<Hash, string>;
  /** if true, we have sent "abort" request, the process might have exited or is going to exit soon */
  aborting?: boolean;
  /** if true, the operation process has exited AND there's no more optimistic commit state to show */
  hasCompletedOptimisticState?: boolean;
  /** if true, the operation process has exited AND there's no more optimistic changes to uncommitted changes to show */
  hasCompletedUncommittedChangesOptimisticState?: boolean;
  /** if true, the operation process has exited AND there's no more optimistic changes to merge conflicts to show */
  hasCompletedMergeConflictsOptimisticState?: boolean;
  warnings?: Array<string>;
} & EnsureAssignedTogether<{
  endTime: Date;
  exitCode: number;
}>;

/**
 * The process has exited but exit code is unknown. Usually exit code is one byte.
 * '-1024' is unlikely to conflict with a valid exit code.
 */
export const EXIT_CODE_FORGET = -1024;

/**
 * Bundle history of previous operations together with the current operation,
 * so we can easily manipulate operations together in one piece of state.
 */
export interface OperationList {
  /** The currently running operation, or the most recently run if not currently running. */
  currentOperation: OperationInfo | undefined;
  /** All previous operations oldest to newest, not including currentOperation */
  operationHistory: Array<OperationInfo>;
}
const defaultOperationList = () => ({currentOperation: undefined, operationHistory: []});

function startNewOperation(newOperation: Operation, list: OperationList): OperationList {
  if (list.currentOperation?.operation.id === newOperation.id) {
    // we already have a new optimistic running operation, don't duplicate it
    return {...list};
  } else {
    // we need to start a new operation
    const operationHistory = [...list.operationHistory];
    if (list.currentOperation != null) {
      operationHistory.push(list.currentOperation);
    }
    const inlineProgress: Array<[string, string]> | undefined = newOperation
      .getInitialInlineProgress?.()
      ?.map(([k, v]) => [short(k), v]); // inline progress is keyed by short hashes, but let's do that conversion on behalf of operations.
    const currentOperation: OperationInfo = {
      operation: newOperation,
      startTime: new Date(),
      inlineProgress: inlineProgress == null ? undefined : new Map(inlineProgress),
    };
    return {...list, operationHistory, currentOperation};
  }
}

/**
 * Ask the server if the current operation is still running.
 * The server might send back a "forgot" progress and we can mark
 * the operation as exited. This is useful when the operation exited
 * during disconnection.
 */
export function maybeRemoveForgottenOperation() {
  const list = readAtom(operationList);
  const operationId = list.currentOperation?.operation.id;
  if (operationId != null) {
    serverAPI.postMessage({
      type: 'requestMissedOperationProgress',
      operationId,
    });
  }
}

export const operationList = atomResetOnCwdChange<OperationList>(defaultOperationList());
registerCleanup(
  operationList,
  serverAPI.onSetup(() => maybeRemoveForgottenOperation()),
  import.meta.hot,
);
registerDisposable(
  operationList,
  serverAPI.onMessageOfType('operationProgress', progress => {
    switch (progress.kind) {
      case 'spawn':
        writeAtom(operationList, list => {
          const operation = operationsById.get(progress.id);
          if (operation == null) {
            return list;
          }

          return startNewOperation(operation, list);
        });
        break;
      case 'stdout':
      case 'stderr':
        writeAtom(operationList, current => {
          const currentOperation = current.currentOperation;
          if (currentOperation == null) {
            return current;
          }

          return {
            ...current,
            currentOperation: {
              ...currentOperation,
              commandOutput: [...(currentOperation?.commandOutput ?? []), progress.message],
              currentProgress: undefined, // hide progress on new stdout, so it doesn't appear stuck
            },
          };
        });
        break;
      case 'inlineProgress':
        writeAtom(operationList, current => {
          const currentOperation = current.currentOperation;
          if (currentOperation == null) {
            return current;
          }

          let inlineProgress: undefined | Map<string, string> =
            current.currentOperation?.inlineProgress ?? new Map();
          if (progress.hash) {
            if (progress.message) {
              inlineProgress.set(progress.hash, progress.message);
            } else {
              inlineProgress.delete(progress.hash);
            }
          } else {
            inlineProgress = undefined;
          }

          return {
            ...current,
            currentOperation: {
              ...currentOperation,
              inlineProgress,
            },
          };
        });
        break;
      case 'progress':
        writeAtom(operationList, current => {
          const currentOperation = current.currentOperation;
          if (currentOperation == null) {
            return current;
          }

          const newCommandOutput = [...(currentOperation?.commandOutput ?? [])];
          if (newCommandOutput.at(-1)?.trim() !== progress.progress.message) {
            // also add progress message as if it was on stdout,
            // so you can see it when reading back the final output,
            // but only if it's a different progress message than we've seen.
            newCommandOutput.push(progress.progress.message + '\n');
          }

          return {
            ...current,
            currentOperation: {
              ...currentOperation,
              commandOutput: newCommandOutput,
              currentProgress: progress.progress,
            },
          };
        });
        break;
      case 'warning':
        writeAtom(operationList, current => {
          const currentOperation = current.currentOperation;
          if (currentOperation == null) {
            return current;
          }
          const warnings = [...(currentOperation?.warnings ?? []), progress.warning];
          return {
            ...current,
            currentOperation: {
              ...currentOperation,
              warnings,
            },
          };
        });
        break;
      case 'exit':
      case 'forgot':
        writeAtom(operationList, current => {
          const currentOperation = current.currentOperation;

          let operationThatExited: OperationInfo | undefined;

          if (
            currentOperation == null ||
            currentOperation.exitCode != null ||
            currentOperation.operation.id !== progress.id
          ) {
            // We've seen cases where we somehow got this exit out of order.
            // Instead of updating the currentOperation, we need to find the matching historical operation.
            // (which has the matching ID, and as long as it hasn't already been marked as exited)

            operationThatExited = current.operationHistory.find(
              op => op.operation.id === progress.id && op.exitCode == null,
            );

            tracker.track('ExitMessageOutOfOrder', {
              extras: {
                operationThatExited: operationThatExited?.operation.trackEventName,
              },
            });
          }

          if (operationThatExited == null) {
            operationThatExited = currentOperation;
          }

          if (operationThatExited == null) {
            // We can't do anything about this.
            return current;
          }

          const {exitCode, timestamp} =
            progress.kind === 'exit'
              ? progress
              : {exitCode: EXIT_CODE_FORGET, timestamp: Date.now()};
          const complete = operationCompletionCallbacks.get(operationThatExited.operation.id);
          complete?.(
            exitCode === 0 ? undefined : new Error(`Process exited with code ${exitCode}`),
          );
          operationCompletionCallbacks.delete(operationThatExited.operation.id);

          const updatedOperation = {
            ...operationThatExited,
            exitCode,
            endTime: new Date(timestamp),
            inlineProgress: undefined, // inline progress never lasts after exiting
          };

          if (operationThatExited === currentOperation) {
            return {
              ...current,
              currentOperation: updatedOperation,
            };
          } else {
            return {
              ...current,
              operationHistory: current.operationHistory.map(op => {
                if (op === operationThatExited) {
                  return updatedOperation;
                }
                return op;
              }),
            };
          }
        });
        break;
    }
  }),
  import.meta.hot,
);

/** If an operation in the queue fails, it will remove all further queued operations.
 * On such an error, we move the remaining operations into this separate state to be shown in the UI as a warning.
 * This lets you see and understand what actions you took that were "reverted", so you might recreate those steps. */
export const queuedOperationsErrorAtom = atomResetOnCwdChange<
  | {error: Error; operationThatErrored: Operation | undefined; operations: Array<Operation>}
  | undefined
>(undefined);

/** The running operation's progress message for `hash`, if it reports one. */
export function useInlineProgress(hash: Hash): string | undefined {
  return useAtomSelect(
    operationList,
    // progress messages come indexed by short hash
    list => list.currentOperation?.inlineProgress?.get(short(hash)),
    [hash],
  );
}

export const operationBeingPreviewed = atomResetOnCwdChange<Operation | undefined>(undefined);

/** We don't send entire operations to the server, since not all fields are serializable.
 * Thus, when the server tells us about the queue of operations, we need to know which operation it's talking about.
 * Store recently run operations by id. Add to this map whenever a new operation is run. Remove when an operation process exits (successfully or unsuccessfully)
 */
const operationsById = new Map<string, Operation>();
/** Store callbacks to run when an operation completes. This is stored outside of the operation since Operations are typically Immutable. */
const operationCompletionCallbacks = new Map<string, (error?: Error) => void>();

/**
 * Subscribe to an operation exiting. Useful for handling cases where an operation fails
 * and it should reset the UI to try again.
 */
export function onOperationExited(
  cb: (
    message: ServerToClientMessage & {type: 'operationProgress'; kind: 'exit'},
    operation: Operation,
  ) => unknown,
): Disposable {
  return serverAPI.onMessageOfType('operationProgress', progress => {
    if (progress.kind === 'exit') {
      const op = operationsById.get(progress.id);
      if (op) {
        cb(progress, op);
      }
    }
  });
}

/**
 * If no operations are running or queued, returns undefined.
 * If something is running or queued, return a Promise that resolves
 * when there's no operation running and nothing remains queued (the UI is "idle")
 * Does not wait for optimistic state to be resolved, only for commands to finish.
 */
export function waitForNothingRunning(): Promise<void> | undefined {
  const currentOperation = readAtom(operationList).currentOperation;
  const somethingRunning = currentOperation != null && currentOperation?.exitCode == null;
  const anythingQueued = readAtom(queuedOperations).length > 0;
  if (!somethingRunning && !anythingQueued) {
    // nothing running, nothing queued -> return undefined immediately
    return undefined;
  }
  return serverAPI
    .nextMessageMatching(
      'operationProgress',
      // something running but nothing queued -> resolve when the operation exits
      // something queued -> resolve when the next operation exits, but only once the queue is empty
      // something running but exits non-zero -> everything queue'd will be cancelled anyway, resolve immediately
      msg => msg.kind === 'exit' && (msg.exitCode !== 0 || readAtom(queuedOperations).length === 0),
    )
    .then(() => undefined);
}

export const queuedOperations = atomResetOnCwdChange<Array<Operation>>([]);
registerDisposable(
  queuedOperations,
  serverAPI.onMessageOfType('operationProgress', progress => {
    switch (progress.kind) {
      case 'queue':
      case 'spawn': // spawning doubles as our notification to dequeue the next operation, and includes the new queue state.
        // Update with the latest queue state. We expect this to be sent whenever we try to run a command but it gets queued.
        writeAtom(queuedOperations, () => {
          return progress.queue
            .map(opId => operationsById.get(opId))
            .filter((op): op is Operation => op != null);
        });
        // On spawn, we can clear the queued commands error. The error would have already been shown and then further acted on.
        // This wouldn't happen automatically, so we consider this an explicit user acknowledgement.
        // This also means this error state and the queuedOperations state should be mutually exclusive.
        writeAtom(queuedOperationsErrorAtom, undefined);
        break;
      case 'error': {
        saveQueuedOperationsOnError(progress.id, new Error(progress.error));

        writeAtom(queuedOperations, []); // empty queue when a command hits an error
        break;
      }
      case 'exit': {
        setTimeout(() => {
          // we don't need to care about this operation anymore after this tick,
          // once all other callsites processing 'operationProgress' messages have run.
          operationsById.delete(progress.id);
        });
        if (progress.exitCode != null && progress.exitCode !== 0) {
          saveQueuedOperationsOnError(progress.id, new Error('command exited with non-zero code'));

          // if any process in the queue exits with an error, the entire queue is cleared.
          writeAtom(queuedOperations, []);
        }
        break;
      }
    }
  }),
  import.meta.hot,
);

function saveQueuedOperationsOnError(operationIdThatErrored: string, error: Error) {
  const queued = readAtom(queuedOperations);
  // This may be called twice for the same operation (error, then also exit).
  // Don't clear the error state if it's for the same operation, even if the queue is now empty.
  if (readAtom(queuedOperationsErrorAtom)?.operationThatErrored?.id !== operationIdThatErrored) {
    writeAtom(
      queuedOperationsErrorAtom,
      queued.length === 0
        ? undefined // invariant: queuedOperationsError.operations should never be [], rather the whole thing is undefined
        : {
            operationThatErrored: operationsById.get(operationIdThatErrored),
            error,
            operations: readAtom(queuedOperations),
          },
    );
  }
}

export function getLatestOperationInfo(operation: Operation): OperationInfo | undefined {
  const list = readAtom(operationList);
  const info =
    list.currentOperation?.operation === operation
      ? list.currentOperation
      : list.operationHistory.find(op => op.operation === operation);

  return info;
}

export function runOperation(operation: Operation): Promise<undefined | Error> {
  // TODO: check for hashes in arguments that are known to be obsolete already,
  // and mark those to not be rewritten.
  serverAPI.postMessage({
    type: 'runOperation',
    operation: operation.getRunnableOperation(),
  });
  const deferred = defer<undefined | Error>();
  operationCompletionCallbacks.set(operation.id, (err?: Error) => {
    deferred.resolve(err);
  });

  operationsById.set(operation.id, operation);
  const ongoing = readAtom(operationList);

  if (ongoing?.currentOperation != null && ongoing.currentOperation.exitCode == null) {
    // Add to the queue optimistically. The server will tell us the real state of the queue when it gets our run request.
    writeAtom(queuedOperations, prev => [...(prev || []), operation]);
  } else {
    // start a new operation. We need to manage the previous operations
    writeAtom(operationList, list => startNewOperation(operation, list));
  }

  // Check periodically with the server that the process is still running.
  // This is a fallback in case the server cannot send us "exit" messages.
  // This timer will auto disable when currentOperation becomes null.
  currentOperationHeartbeatTimer.enabled = true;

  return deferred.promise;
}

const currentOperationHeartbeatTimer = new Timer(() => {
  const currentOp = readAtom(operationList).currentOperation;
  if (currentOp == null || currentOp.endTime != null) {
    // Stop the timer.
    return false;
  }
  maybeRemoveForgottenOperation();
}, 5000);

/**
 * Returns callback to run an operation.
 * Will be queued by the server if other operations are already running.
 * This returns a promise that resolves when this operation has exited
 * (though its optimistic state may not have finished resolving yet).
 * Note: Most callsites won't await this promise, and just use queueing. If you do, you should probably use `throwOnError = true` to detect errors.
 * TODO: should we refactor this into a separate function if you want to await the result, which always throws?
 * Note: There's no need to wait for this promise to resolve before starting another operation,
 * successive operations will queue up with a nicer UX than if you awaited each one.
 */
export function useRunOperation() {
  return useCallback(async (operation: Operation, throwOnError?: boolean): Promise<void> => {
    const result = await runOperation(operation);
    if (result != null && throwOnError) {
      throw result;
    }
  }, []);
}

/**
 * Returns callback to abort the running operation.
 */
export function useAbortRunningOperation() {
  return useCallback((operationId: string) => {
    serverAPI.postMessage({
      type: 'abortRunningOperation',
      operationId,
    });
    const ongoing = readAtom(operationList);
    if (ongoing?.currentOperation?.operation?.id === operationId) {
      // Mark 'aborting' as true.
      writeAtom(operationList, list => {
        const currentOperation = list.currentOperation;
        if (currentOperation != null) {
          return {...list, currentOperation: {aborting: true, ...currentOperation}};
        }
        return list;
      });
    }
  }, []);
}

/**
 * Returns callback to run the operation currently being previewed, or cancel the preview.
 * Set operationBeingPreviewed to start a preview.
 */
export function useRunPreviewedOperation() {
  return useCallback((isCancel: boolean, operation?: Operation) => {
    if (isCancel) {
      writeAtom(operationBeingPreviewed, undefined);
      return;
    }

    const operationToRun = operation ?? readAtom(operationBeingPreviewed);
    writeAtom(operationBeingPreviewed, undefined);
    if (operationToRun) {
      runOperation(operationToRun);
    }
  }, []);
}

/**
 * It's possible for optimistic state to be incorrect, e.g. if some assumption about a command is incorrect in an edge case
 * but the command doesn't exit non-zero. This provides a backdoor to clear out all ongoing optimistic state from *previous* commands.
 * Queued commands and the currently running command will not be affected.
 */
export function useClearAllOptimisticState() {
  return useCallback(() => {
    writeAtom(operationList, list => {
      const operationHistory = [...list.operationHistory];
      for (let i = 0; i < operationHistory.length; i++) {
        if (operationHistory[i].exitCode != null) {
          if (!operationHistory[i].hasCompletedOptimisticState) {
            operationHistory[i] = {...operationHistory[i], hasCompletedOptimisticState: true};
          }
          if (!operationHistory[i].hasCompletedUncommittedChangesOptimisticState) {
            operationHistory[i] = {
              ...operationHistory[i],
              hasCompletedUncommittedChangesOptimisticState: true,
            };
          }
          if (!operationHistory[i].hasCompletedMergeConflictsOptimisticState) {
            operationHistory[i] = {
              ...operationHistory[i],
              hasCompletedMergeConflictsOptimisticState: true,
            };
          }
        }
      }
      const currentOperation =
        list.currentOperation == null ? undefined : {...list.currentOperation};
      if (currentOperation?.exitCode != null) {
        currentOperation.hasCompletedOptimisticState = true;
        currentOperation.hasCompletedUncommittedChangesOptimisticState = true;
        currentOperation.hasCompletedMergeConflictsOptimisticState = true;
      }
      return {currentOperation, operationHistory};
    });
  }, []);
}
