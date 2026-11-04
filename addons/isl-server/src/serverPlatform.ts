/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import type {
  AbsolutePath,
  CwdInfo,
  PlatformSpecificClientToServerMessages,
  RepoRelativePath,
  ServerToClientMessage,
} from 'isl/src/types';
import {Internal} from './Internal';
import {Repository} from './Repository';
import type {RepositoryContext} from './serverTypes';

import {spawn} from 'node:child_process';
import pathModule from 'node:path';
import {nullthrows} from 'shared/utils';

/**
 * Platform-specific server-side API for each target: vscode extension host, electron standalone, browser, ...
 * See also platform.ts
 */
export interface ServerPlatform {
  platformName: string;
  /** Override the analytics Session ID. Should be globally unique. */
  sessionId?: string;
  /** Whether this platform is running inside Basecamp. */
  isBasecamp?: boolean;
  handleMessageFromClient(
    this: ServerPlatform,
    repo: Repository | undefined,
    ctx: RepositoryContext,
    message: PlatformSpecificClientToServerMessages,
    postMessage: (message: ServerToClientMessage) => void,
    onDispose: (disapose: () => unknown) => void,
    onConnectionDispose?: (dispose: () => unknown) => void,
  ): void | Promise<void>;
}

export function makeBrowserServerPlatform(extraCwds?: string[]): ServerPlatform {
  return {
    platformName: 'browser',
    handleMessageFromClient(
      this: ServerPlatform,
      repo: Repository | undefined,
      ctx: RepositoryContext,
      message: PlatformSpecificClientToServerMessages,
      postMessage: (message: ServerToClientMessage) => void,
    ) {
      switch (message.type) {
        case 'platform/subscribeToAvailableCwds': {
          const allCwds = [ctx.cwd, ...(extraCwds ?? [])];
          void Promise.allSettled(allCwds.map(cwd => Repository.getCwdInfo({...ctx, cwd}))).then(
            results => {
              const options = results
                .filter((r): r is PromiseFulfilledResult<CwdInfo> => r.status === 'fulfilled')
                .map(r => r.value);
              postMessage({type: 'platform/availableCwds', options});
            },
          );
          break;
        }
        case 'platform/openContainingFolder': {
          const absPath: AbsolutePath = pathModule.join(
            nullthrows(repo?.info.repoRoot),
            message.path,
          );
          let args: Array<string> = [];
          // use OS-builtin open command to open parent directory
          // (which may open different file extensions with different programs)
          switch (process.platform) {
            case 'darwin':
              args = ['/usr/bin/open', pathModule.dirname(absPath)];
              break;
            case 'win32':
              // On windows, we can select the file in the newly opened explorer window by giving the full path
              args = ['explorer.exe', '/select,', absPath];
              break;
            case 'linux':
              args = ['xdg-open', pathModule.dirname(absPath)];
              break;
          }
          repo?.initialConnectionContext.logger.info('open file', absPath);
          if (args.length > 0) {
            spawnInBackground(repo, args);
          }
          break;
        }
        case 'platform/openFile': {
          openFile(repo, ctx, message.path);
          break;
        }
        case 'platform/openFiles': {
          for (const path of message.paths) {
            openFile(repo, ctx, path);
          }
          break;
        }
        case 'platform/fillCommitMessageWithAI': {
          // Generate a commit message on the server (where `sl web` runs -- a devserver/OnDemand with
          // the `dm` CLI), so this works in standalone ISL, not only inside the VS Code Devmate host.
          const cwd = repo?.info.repoRoot;
          if (cwd == null || Internal.fillCommitMessageWithAI == null) {
            break;
          }
          Internal.fillCommitMessageWithAI(cwd)
            .then((generated: {title: string; description: string} | undefined) => {
              if (generated != null) {
                postMessage({
                  type: 'updateDraftCommitMessage',
                  title: generated.title,
                  description: generated.description,
                  mode: 'commit',
                });
              }
            })
            .catch((err: Error) => {
              ctx.logger.error('failed to generate commit message with AI:', err);
            });
          break;
        }
        case 'platform/openPreview': {
          // Browser has no markdown preview; fall back to opening the raw file.
          openFile(repo, ctx, message.path);
          break;
        }
      }
    },
  };
}

export const browserServerPlatform: ServerPlatform = makeBrowserServerPlatform();

async function openFile(
  repo: Repository | undefined,
  ctx: RepositoryContext | undefined,
  path: RepoRelativePath,
) {
  if (repo == null || ctx == null) {
    return;
  }
  const opener = await repo.getConfig(ctx, 'isl.open-file-cmd');
  const absPath: AbsolutePath = pathModule.join(repo.info.repoRoot, path);
  let args: Array<string> = [];
  if (opener) {
    // opener should be either a JSON string (wrapped in quotes) or a JSON array of strings,
    // to include arguments
    try {
      const jsonOpenerArgs = JSON.parse(opener);
      args = Array.isArray(jsonOpenerArgs)
        ? [...jsonOpenerArgs, absPath]
        : [jsonOpenerArgs, absPath];
    } catch {
      // if it's not JSON, it should be a regular string
      args = [opener, absPath];
    }
  } else {
    // by default, use OS-builtin open command to open files
    // (which may open different file extensions with different programs)
    switch (process.platform) {
      case 'darwin':
        args = ['/usr/bin/open', absPath];
        break;
      case 'win32':
        args = ['notepad.exe', absPath];
        break;
      case 'linux':
        args = ['xdg-open', absPath];
        break;
    }
  }
  repo.initialConnectionContext.logger.info('open file', absPath);
  if (args.length > 0) {
    spawnInBackground(repo, args);
  }
}

/**
 * Because the ISL server is likely running in the background and is
 * no longer attached to a terminal, this is designed for the case
 * where the user opens the file in a windowed editor:
 *
 * - For users using a simple one-window-per-file graphical text
 *   editor, like notepad.exe, this is relatively straightforward.
 * - For users who prefer a terminal-based editor, like Emacs,
 *   a conduit like EmacsClient would be required.
 *
 * Further, killing ISL should not kill the editor, so this follows
 * the pattern for spawning an independent, long-running process in
 * Node.js as described here:
 *
 * https://nodejs.org/docs/latest-v10.x/api/child_process.html#child_process_options_detached
 */
function spawnInBackground(repo: Repository | undefined, args: Array<string>) {
  // TODO: Report error if spawn() fails?
  // TODO: support passing the column/line number to programs that support it? e.g. vscode: `code /path/to/file:10:20`
  const proc = spawn(args[0], args.slice(1), {
    detached: true,
    stdio: 'ignore',
    // The openers we pick on Windows (explorer.exe, notepad.exe) are GUI
    // programs that put up their own window; a console for them is pure
    // noise, and `stdio: 'ignore'` means nothing could read or write it
    // anyway. No-op on other platforms.
    windowsHide: true,
    windowsVerbatimArguments: true,
  });
  // Silent error. Don't crash the server process.
  proc.on('error', err => {
    repo?.initialConnectionContext.logger.info('failed to open', args, err);
  });
  proc.unref();
}
