/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import type {StartServerArgs} from './server';

import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import {nullthrows} from 'shared/utils';
import rmtree from './rmtree';

export type ExistingServerInfo = {
  sensitiveToken: string;
  challengeToken: string;
  logFileLocation: string;
  /** Which command name was used to launch this server instance,
   * so it can be propagated to run further sl commands by the server.
   * Usually, "sl". */
  command: string;
  /**
   * `sl version` string. If the version of sl changes, we shouldn't reuse that server instance,
   * due to potential incompatibilities between the old running server javascript and the new client javascript.
   */
  slVersion: string;
  /** Hostname/IP the server is bound to. Optional for backwards compat. */
  bind?: string;
  /** Path to TLS certificate file. Optional for backwards compat. */
  tlsCert?: string;
  /** Path to TLS key file. Optional for backwards compat. */
  tlsKey?: string;
  /** Whether the server keeps running when idle. Optional for backwards compat. */
  persist?: boolean;
};

const cacheDir =
  process.platform == 'win32'
    ? path.join(nullthrows(process.env.LOCALAPPDATA), 'cache')
    : process.platform == 'darwin'
      ? path.join(os.homedir(), 'Library/Caches')
      : process.env.XDG_CACHE_HOME || path.join(os.homedir(), '.cache');

/**
 * Per-user cache dir with restrictive permissions.
 * Inside this folder will be a number of files, one per port for an active ISL server.
 */
const savedActiveServerUrlsDirectory = path.join(cacheDir, 'sapling-isl');

function fileNameForPort(port: number): string {
  return `reusable_server_${port}`;
}

function isMode700(stat: fs.Stats): boolean {
  // eslint-disable-next-line no-bitwise
  return (stat.mode & 0o777) === 0o700;
}

/**
 * Make a temp directory with restrictive permissions where we can write existing server information.
 * Ensures directory has proper restrictive mode if the directory already exists.
 */
export async function ensureExistingServerFolder(): Promise<void> {
  await fs.promises.mkdir(savedActiveServerUrlsDirectory, {
    // directory needs rwx
    mode: 0o700,
    recursive: true,
  });

  const stat = await fs.promises.stat(savedActiveServerUrlsDirectory);
  if (process.platform !== 'win32' && !isMode700(stat)) {
    throw new Error(
      `active servers folder ${savedActiveServerUrlsDirectory} has the wrong permissions: ${stat.mode}`,
    );
  }
  if (stat.isSymbolicLink()) {
    throw new Error(`active servers folder ${savedActiveServerUrlsDirectory} is a symlink`);
  }
}

export function deleteExistingServerFile(port: number): Promise<void> {
  const folder = path.join(savedActiveServerUrlsDirectory, fileNameForPort(port));
  if (typeof fs.promises.rm === 'function') {
    return fs.promises.rm(folder, {force: true});
  } else {
    return rmtree(folder);
  }
}

export async function writeExistingServerFile(
  port: number,
  data: ExistingServerInfo,
): Promise<void> {
  await fs.promises.writeFile(
    path.join(savedActiveServerUrlsDirectory, fileNameForPort(port)),
    JSON.stringify(data),
    {encoding: 'utf-8', flag: 'w', mode: 0o600},
  );
}

export async function readExistingServerFile(port: number): Promise<ExistingServerInfo> {
  // TODO: do we need to verify the permissions of this file?
  const data: string = await fs.promises.readFile(
    path.join(savedActiveServerUrlsDirectory, fileNameForPort(port)),
    {encoding: 'utf-8', flag: 'r'},
  );
  return JSON.parse(data) as ExistingServerInfo;
}

/**
 * Record a running server so a later launch on the same port can reuse it.
 * The server process calls this itself once it is listening, so the record
 * exists even if the process that launched it has already exited.
 */
export async function saveExistingServerInfo(args: StartServerArgs): Promise<void> {
  await ensureExistingServerFolder();
  await deleteExistingServerFile(args.port);
  await writeExistingServerFile(args.port, {
    sensitiveToken: args.sensitiveToken,
    challengeToken: args.challengeToken,
    logFileLocation: args.logFileLocation,
    command: args.command,
    slVersion: args.slVersion,
    bind: args.bind,
    tlsCert: args.tlsCert,
    tlsKey: args.tlsKey,
    persist: args.persist,
  });
}
