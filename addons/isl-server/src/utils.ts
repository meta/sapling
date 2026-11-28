/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import type {CommitInfo, SmartlogCommits} from 'isl/src/types';
import type {EjecaChildProcess, EjecaError, EjecaReturn} from 'shared/ejeca';

import fs from 'node:fs';
import os from 'node:os';
import {promisify} from 'node:util';
import {truncate} from 'shared/utils';

/**
 * Resolve symlinks in `p`, falling back to `p` if that fails. Uses the JS `fs.realpath`, not the
 * native `fs.promises.realpath`, which uppercases Windows drive letters and so no longer matches
 * VS Code's `Uri.fsPath` (`c:\`).
 */
export function canonicalizePath(p: string): Promise<string> {
  return promisify(fs.realpath)(p).catch(() => p);
}

export function firstOfIterable<T>(iterable: IterableIterator<T>): T | undefined {
  return iterable.next().value;
}

/**
 * Limits async function execution parallelism to only one at a time.
 * Hence, if a call is already running, it will wait for it to finish,
 * then start the next async execution, but if called again while not finished,
 * it will return the scheduled execution promise.
 *
 * Sample Usage:
 * ```
 * let i = 1;
 * const oneExecAtATime = serializeAsyncCall(() => {
 *   return new Promise((resolve, reject) => {
 *     setTimeout(200, () => resolve(i++));
 *   });
 * });
 *
 * const result1Promise = oneExecAtATime(); // Start an async, and resolve to 1 in 200 ms.
 * const result2Promise = oneExecAtATime(); // Schedule the next async, and resolve to 2 in 400 ms.
 * const result3Promise = oneExecAtATime(); // Reuse scheduled promise and resolve to 2 in 400 ms.
 * ```
 */
export function serializeAsyncCall<T>(asyncFun: () => Promise<T>): () => Promise<T> {
  let scheduledCall: Promise<T> | undefined = undefined;
  let pendingCall: Promise<undefined> | undefined = undefined;
  const startAsyncCall = () => {
    const resultPromise = asyncFun();
    pendingCall = resultPromise.then(
      () => (pendingCall = undefined),
      () => (pendingCall = undefined),
    );
    return resultPromise;
  };
  const callNext = () => {
    scheduledCall = undefined;
    return startAsyncCall();
  };
  const scheduleNextCall = () => {
    if (scheduledCall == null) {
      if (pendingCall == null) {
        throw new Error('pendingCall must not be null!');
      }
      scheduledCall = pendingCall.then(callNext, callNext);
    }
    return scheduledCall;
  };
  return () => {
    if (pendingCall == null && scheduledCall == null) {
      return startAsyncCall();
    } else {
      return scheduleNextCall();
    }
  };
}

/**
 * Kill `child` on `AbortSignal`.
 *
 * This is slightly more robust than execa 6.0 and nodejs' `signal` support:
 * if a process was stopped (by `SIGTSTP` or `SIGSTOP`), it can still be killed.
 */
export function handleAbortSignalOnProcess(child: EjecaChildProcess, signal: AbortSignal) {
  signal.addEventListener('abort', () => {
    if (os.platform() == 'win32') {
      // Signals are ignored on Windows.
      // execa's default forceKillAfterTimeout behavior does not
      // make sense for Windows. Disable it explicitly.
      child.kill('SIGKILL', {forceKillAfterTimeout: false});
    } else {
      // If the process is stopped (ex. Ctrl+Z, kill -STOP), make it
      // continue first so it can respond to signals including SIGKILL.
      child.kill('SIGCONT');
      // A good citizen process should exit soon after receiving SIGTERM.
      // In case it doesn't, send SIGKILL after 5 seconds.
      child.kill('SIGTERM', {forceKillAfterTimeout: 5000});
    }
  });
}

/**
 * Given a list of commits and a starting commit,
 * traverse up the chain of `parents` until we find a public commit
 */
export function findPublicAncestor(
  allCommits: SmartlogCommits | undefined,
  from: CommitInfo,
): CommitInfo | undefined {
  let publicCommit: CommitInfo | undefined;
  if (allCommits != null) {
    const map = new Map(allCommits.map(commit => [commit.hash, commit]));

    let current: CommitInfo | undefined = from;
    while (current != null) {
      if (current.phase === 'public') {
        publicCommit = current;
        break;
      }
      if (current.parents[0] == null) {
        break;
      }
      current = map.get(current.parents[0]);
    }
  }

  return publicCommit;
}

/**
 * Run a command that is expected to produce JSON output.
 * Return a JSON object. On error, the JSON object has property "error".
 */
export function parseExecJson<T>(
  exec: Promise<EjecaReturn>,
  reply: (parsed?: T, error?: string) => void,
) {
  exec
    .then(result => {
      const stdout = result.stdout;
      try {
        const parsed = JSON.parse(stdout);
        if (parsed.error != null) {
          reply(undefined, parsed.error);
        } else {
          reply(parsed as T);
        }
      } catch (err) {
        const msg = `Cannot parse ${truncate(
          result.escapedCommand,
        )} output. (error: ${err}, stdout: ${stdout})`;
        reply(undefined, msg);
      }
    })
    .catch(err => {
      // Try extracting error from stdout '{error: message}'.
      try {
        const parsed = JSON.parse(err.stdout);
        if (parsed.error != null) {
          reply(undefined, parsed.error);
          return;
        }
      } catch {}
      // Fallback to general error.
      const msg = `Cannot run ${truncate(err.escapedCommand)}. (error: ${err})`;
      reply(undefined, msg);
    });
}

export type EjecaSpawnError = Error & {code: string; path: string};

/**
 * True if an Ejeca spawned process exits non-zero or is killed.
 * @see {EjecaSpawnError} for when a process fails to spawn in the first place (e.g. ENOENT).
 */
export function isEjecaError(s: unknown): s is EjecaError {
  return s != null && typeof s === 'object' && 'exitCode' in s;
}

/** True when Ejeca fails to spawn a process, e.g. ENOENT.
 * (as opposed to the command spawning, then exiting non-zero) */
export function isEjecaSpawnError(s: unknown): s is EjecaSpawnError {
  return s != null && typeof s === 'object' && 'code' in s;
}

export function fromEntries<V>(entries: Array<[string, V]>): {
  [key: string]: V;
} {
  // Object.fromEntries() is available in Node v12 and later.
  if (typeof Object.fromEntries === 'function') {
    return Object.fromEntries(entries);
  }

  const obj: {
    [key: string]: V;
  } = {};
  for (const [key, value] of entries) {
    obj[key] = value;
  }
  return obj;
}
