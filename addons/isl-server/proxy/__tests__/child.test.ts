/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import type {StartServerResult} from '../server';

jest.mock('../server', () => ({startServer: jest.fn()}));
jest.mock('../uncaughtException', () => ({registerUncaughtExceptionHandler: jest.fn()}));

let mockFsData: {[key: string]: string} = {};
jest.mock('node:fs', () => ({
  promises: {
    writeFile: jest.fn(async (path: string, data: string) => {
      mockFsData[path] = data;
    }),
    readFile: jest.fn(async (path: string) => mockFsData[path]),
    stat: jest.fn(async () => ({mode: 0o700, isSymbolicLink: () => false})),
    rm: jest.fn(async (path: string) => {
      delete mockFsData[path];
    }),
    mkdir: jest.fn(async () => {}),
  },
}));

/* eslint-disable require-await */

describe('background server launcher connection', () => {
  const send = jest.fn();
  const result: StartServerResult = {type: 'success', port: 3001, pid: 123};
  const descriptors = Object.getOwnPropertyDescriptors(process);
  const serverArgs = process.env.ISL_SERVER_ARGS;

  beforeEach(() => {
    jest.resetModules();
    mockFsData = {};
    send.mockReset();
    Object.defineProperty(process, 'send', {configurable: true, value: send});
    Object.defineProperty(process, 'connected', {configurable: true, value: true});
    process.env.ISL_SERVER_ARGS = JSON.stringify({logFileLocation: 'isl-server.log'});
  });

  afterEach(() => {
    for (const key of ['send', 'connected'] as const) {
      if (descriptors[key]) {
        Object.defineProperty(process, key, descriptors[key]);
      } else {
        Reflect.deleteProperty(process, key);
      }
    }
    if (serverArgs == null) {
      delete process.env.ISL_SERVER_ARGS;
    } else {
      process.env.ISL_SERVER_ARGS = serverArgs;
    }
  });

  async function start() {
    const {startServer} = await import('../server');
    jest.mocked(startServer).mockResolvedValue(result);
    await import('../child');
    await new Promise(resolve => setImmediate(resolve));
    return jest.mocked(startServer).mock.calls[0][0].logInfo;
  }

  it('delivers startup results while the launcher is connected', async () => {
    await start();
    expect(send).toHaveBeenCalledWith(
      {type: 'result', result},
      undefined,
      {swallowErrors: true},
      expect.any(Function),
    );
  });

  it('can reject stale clients after the launcher exits', async () => {
    const logInfo = await start();
    send.mockClear();
    Object.defineProperty(process, 'connected', {value: false});

    logInfo('closing ws:', 'Invalid token');

    expect(send).not.toHaveBeenCalled();
  });

  it('handles the launcher disconnecting during a send', async () => {
    const logInfo = await start();
    send.mockImplementation((_message, _handle, _options, callback) => {
      callback(Object.assign(new Error('Channel closed'), {code: 'ERR_IPC_CHANNEL_CLOSED'}));
    });

    expect(() => logInfo('closing ws:', 'Invalid token')).not.toThrow();
  });

  it('saves the server for reuse even when the launcher exited before startup finished', async () => {
    const saved = {
      sensitiveToken: 'sensitive',
      challengeToken: 'challenge',
      logFileLocation: 'isl-server.log',
      command: 'sl',
      slVersion: '1.0',
      bind: '::',
      tlsCert: '/cert.pem',
      tlsKey: '/cert.pem',
      persist: true,
    };
    process.env.ISL_SERVER_ARGS = JSON.stringify({...saved, port: 3001, foreground: false});
    Object.defineProperty(process, 'connected', {value: false});

    await start();

    const {readExistingServerFile} = await import('../existingServerStateFiles');
    await expect(readExistingServerFile(3001)).resolves.toEqual(saved);
  });
});
