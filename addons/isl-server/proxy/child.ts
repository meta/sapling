/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

/**
 * This file is expected to be run via child_process.fork() where:
 *
 * - The arguments to `startServer()` are JSON-serialized as the value
 *   of the ISL_SERVER_ARGS environment variable.
 * - Communication to the parent process must be done via process.send()
 *   and the parent process expects messages that conform to
 *   `ChildProcessResponse`.
 */

import type {MessageOptions} from 'node:child_process';
import type {StartServerArgs, StartServerResult} from './server';

import {registerUncaughtExceptionHandler} from './uncaughtException';

/**
 * This defines the shape of the messages that the parent process accepts.
 * As such, it is imperative that these types are serializable.
 */
export type ChildProcessResponse =
  | {
      type: 'message';
      args: Parameters<typeof console.log>;
    }
  | {
      type: 'result';
      result: StartServerResult;
    };

function sendMessageToParentProcess(msg: ChildProcessResponse): void {
  if (process.connected) {
    // The background launcher can exit between the check and the send.
    // `swallowErrors` is supported by Node.js but missing from `MessageOptions` in @types/node.
    process.send?.(msg, undefined, {swallowErrors: true} as MessageOptions, () => {});
  }
}

function info(...args: Parameters<typeof console.log>): void {
  const msg = {
    type: 'message',
    args,
  } as ChildProcessResponse;
  sendMessageToParentProcess(msg);
}

const args: StartServerArgs = JSON.parse(process.env.ISL_SERVER_ARGS as string);
args.logInfo = info;
import('./server')
  .then(({startServer}) => startServer(args))
  .then((result: StartServerResult) => {
    sendMessageToParentProcess({type: 'result', result});
  })
  .catch(error =>
    sendMessageToParentProcess({type: 'result', result: {type: 'error', error: String(error)}}),
  );

registerUncaughtExceptionHandler(args.logFileLocation);
