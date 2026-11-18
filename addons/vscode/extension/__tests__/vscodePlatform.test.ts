/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import type {RepositoryContext} from 'isl-server/src/serverTypes';
import type {
  CwdInfo,
  PlatformSpecificClientToServerMessages,
  ServerToClientMessage,
} from 'isl/src/types';

import * as vscode from 'vscode';
import {encodeSaplingDiffUri} from '../DiffContentProvider';
import {Internal} from '../Internal';
import {getVSCodePlatform} from '../vscodePlatform';

jest.mock('../Internal', () => ({
  Internal: {
    isBasecamp: jest.fn(),
    basecampOpenFolderAsNewTile: jest.fn(),
  },
}));

jest.mock('isl-server/src/Repository', () => ({
  Repository: {
    getCwdInfo: jest.fn(),
  },
}));

// eslint-disable-next-line @typescript-eslint/no-var-requires, @typescript-eslint/no-require-imports
const {Repository} = require('isl-server/src/Repository');

const mockCtx: RepositoryContext = {
  agentSessionMetadata: undefined,
  cwd: '/test/cwd',
  cmd: 'sl',
  logger: {log: jest.fn(), info: jest.fn(), warn: jest.fn(), error: jest.fn()} as never,
  tracker: {context: {setRepo: jest.fn()}} as never,
};

describe('platform/subscribeToAvailableCwds', () => {
  const mockExtensionContext = {
    globalState: {
      update: jest.fn(),
    },
  } as unknown as vscode.ExtensionContext;

  beforeEach(() => {
    jest.clearAllMocks();
  });

  it('sends available cwds for all valid folders even when one getCwdInfo rejects', async () => {
    const goodCwd: CwdInfo = {cwd: '/path/to/repo1', repoRoot: '/path/to/repo1'};

    (vscode.workspace as {workspaceFolders?: vscode.WorkspaceFolder[]}).workspaceFolders = [
      {name: 'folder1', index: 0, uri: vscode.Uri.file('/path/to/repo1')},
      {name: 'folder2', index: 1, uri: vscode.Uri.file('/path/to/broken')},
    ];

    Repository.getCwdInfo.mockImplementation((ctx: RepositoryContext) => {
      if (ctx.cwd.includes('broken')) {
        return Promise.reject(new Error('simulated failure'));
      }
      return Promise.resolve(goodCwd);
    });

    const postMessage = jest.fn();
    const platform = getVSCodePlatform(mockExtensionContext);

    const message: PlatformSpecificClientToServerMessages = {
      type: 'platform/subscribeToAvailableCwds',
    } as PlatformSpecificClientToServerMessages;

    await platform.handleMessageFromClient.call(
      platform,
      undefined,
      mockCtx,
      message,
      postMessage as (msg: ServerToClientMessage) => void,
      jest.fn(),
      jest.fn(),
    );

    // Wait for the async postAllAvailableCwds to complete
    await new Promise(resolve => setTimeout(resolve, 0));

    expect(postMessage).toHaveBeenCalledWith({
      type: 'platform/availableCwds',
      options: [goodCwd],
    });
  });

  it('sends all available cwds when all getCwdInfo succeed', async () => {
    const cwd1: CwdInfo = {cwd: '/path/to/repo1', repoRoot: '/path/to/repo1'};
    const cwd2: CwdInfo = {cwd: '/path/to/repo2', repoRoot: '/path/to/repo2'};

    (vscode.workspace as {workspaceFolders?: vscode.WorkspaceFolder[]}).workspaceFolders = [
      {name: 'folder1', index: 0, uri: vscode.Uri.file('/path/to/repo1')},
      {name: 'folder2', index: 1, uri: vscode.Uri.file('/path/to/repo2')},
    ];

    Repository.getCwdInfo.mockResolvedValueOnce(cwd1).mockResolvedValueOnce(cwd2);

    const postMessage = jest.fn();
    const platform = getVSCodePlatform(mockExtensionContext);

    const message: PlatformSpecificClientToServerMessages = {
      type: 'platform/subscribeToAvailableCwds',
    } as PlatformSpecificClientToServerMessages;

    await platform.handleMessageFromClient.call(
      platform,
      undefined,
      mockCtx,
      message,
      postMessage as (msg: ServerToClientMessage) => void,
      jest.fn(),
      jest.fn(),
    );

    await new Promise(resolve => setTimeout(resolve, 0));

    expect(postMessage).toHaveBeenCalledWith({
      type: 'platform/availableCwds',
      options: [cwd1, cwd2],
    });
  });
});

describe('platform/subscribeToVSCodeConfig', () => {
  const mockExtensionContext = {
    globalState: {
      update: jest.fn(),
    },
  } as unknown as vscode.ExtensionContext;

  beforeEach(() => {
    jest.clearAllMocks();
    jest.spyOn(vscode.workspace, 'getConfiguration').mockReturnValue({
      get: jest.fn().mockReturnValue('Auto'),
    } as never);
  });

  afterEach(() => {
    jest.restoreAllMocks();
  });

  it('still reports other message handling errors', async () => {
    const postMessage = jest.fn(() => {
      throw new Error('boom');
    });
    const platform = getVSCodePlatform(mockExtensionContext);

    const message: PlatformSpecificClientToServerMessages = {
      type: 'platform/subscribeToVSCodeConfig',
      config: 'sapling.comparisonPanelMode',
    } as PlatformSpecificClientToServerMessages;

    await platform.handleMessageFromClient.call(
      platform,
      undefined,
      mockCtx,
      message,
      postMessage as (msg: ServerToClientMessage) => void,
      jest.fn(),
      jest.fn(),
    );

    expect(vscode.window.showErrorMessage).toHaveBeenCalledWith(
      expect.stringContaining('error handling message'),
    );
    expect(vscode.window.showErrorMessage).toHaveBeenCalledWith(expect.stringContaining('boom'));
  });
});

describe('platform/investigateFailure', () => {
  async function checkHealth() {
    const platform = getVSCodePlatform({} as vscode.ExtensionContext);
    await platform.handleMessageFromClient(
      undefined,
      mockCtx,
      {
        type: 'platform/investigateFailure',
        failure: {
          operationId: 'id',
          operationName: 'GotoOperation',
          exitCode: 1,
          output: '',
          outputTruncated: false,
        },
      },
      jest.fn(),
      jest.fn(),
    );
  }

  beforeEach(() => jest.clearAllMocks());

  afterEach(() => {
    delete Internal.investigateFailure;
  });

  it('passes the server context and failure to the internal investigation', async () => {
    const check = jest.fn().mockResolvedValue(undefined);
    Internal.investigateFailure = check;
    await checkHealth();
    expect(check.mock.calls).toEqual([[mockCtx, expect.objectContaining({operationId: 'id'})]]);
  });

  it('ignores health requests in OSS without commands, telemetry, or installation prompts', async () => {
    await checkHealth();
    expect(vscode.commands.getCommands).not.toHaveBeenCalled();
    expect(vscode.commands.executeCommand).not.toHaveBeenCalled();
    expect(vscode.window.showErrorMessage).not.toHaveBeenCalled();
  });
});

describe('platform/openFileAtRevset', () => {
  const mockExtensionContext = {
    globalState: {update: jest.fn()},
  } as unknown as vscode.ExtensionContext;

  const repoRoot = '/path/to/repo';
  const mockRepo = {info: {repoRoot}} as never;

  beforeEach(() => {
    jest.clearAllMocks();
  });

  it('opens the in-diff version via a read-only sapling-diff URI', async () => {
    const platform = getVSCodePlatform(mockExtensionContext);
    const message: PlatformSpecificClientToServerMessages = {
      type: 'platform/openFileAtRevset',
      path: 'src/file.ts',
      revset: 'abc123',
    };

    await platform.handleMessageFromClient.call(
      platform,
      mockRepo,
      mockCtx,
      message,
      jest.fn() as (msg: ServerToClientMessage) => void,
      jest.fn(),
      jest.fn(),
    );

    const expectedUri = encodeSaplingDiffUri(vscode.Uri.file(`${repoRoot}/src/file.ts`), 'abc123');
    expect(vscode.window.showTextDocument).toHaveBeenCalledWith(expectedUri, {
      viewColumn: undefined,
    });
  });

  it('does nothing when there is no repo', async () => {
    const platform = getVSCodePlatform(mockExtensionContext);
    const message: PlatformSpecificClientToServerMessages = {
      type: 'platform/openFileAtRevset',
      path: 'src/file.ts',
      revset: 'abc123',
    };

    await platform.handleMessageFromClient.call(
      platform,
      undefined,
      mockCtx,
      message,
      jest.fn() as (msg: ServerToClientMessage) => void,
      jest.fn(),
      jest.fn(),
    );

    expect(vscode.window.showTextDocument).not.toHaveBeenCalled();
  });
});

describe('platform/openPreview', () => {
  const mockExtensionContext = {
    globalState: {update: jest.fn()},
  } as unknown as vscode.ExtensionContext;

  const repoRoot = '/path/to/repo';
  const mockRepo = {info: {repoRoot}} as never;

  const mockExecuteCommand = vscode.commands.executeCommand as jest.MockedFunction<
    typeof vscode.commands.executeCommand
  >;

  beforeEach(() => {
    jest.clearAllMocks();
    mockExecuteCommand.mockReturnValue(Promise.resolve(undefined) as never);
  });

  it('opens markdown preview for the file', async () => {
    const platform = getVSCodePlatform(mockExtensionContext);
    const message: PlatformSpecificClientToServerMessages = {
      type: 'platform/openPreview',
      path: 'NOTES.md',
    };

    await platform.handleMessageFromClient.call(
      platform,
      mockRepo,
      mockCtx,
      message,
      jest.fn() as (msg: ServerToClientMessage) => void,
      jest.fn(),
      jest.fn(),
    );

    expect(mockExecuteCommand).toHaveBeenCalledWith(
      'markdown.showPreview',
      vscode.Uri.file(`${repoRoot}/NOTES.md`),
    );
  });

  it('does nothing when there is no repo', async () => {
    const platform = getVSCodePlatform(mockExtensionContext);
    const message: PlatformSpecificClientToServerMessages = {
      type: 'platform/openPreview',
      path: 'NOTES.md',
    };

    await platform.handleMessageFromClient.call(
      platform,
      undefined,
      mockCtx,
      message,
      jest.fn() as (msg: ServerToClientMessage) => void,
      jest.fn(),
      jest.fn(),
    );

    expect(mockExecuteCommand).not.toHaveBeenCalled();
  });
});

describe('platform/openInNewWindow', () => {
  const mockExtensionContext = {
    globalState: {update: jest.fn()},
  } as unknown as vscode.ExtensionContext;

  const mockIsBasecamp = Internal.isBasecamp as jest.MockedFunction<
    NonNullable<typeof Internal.isBasecamp>
  >;
  const mockBasecampOpenFolderAsNewTile =
    Internal.basecampOpenFolderAsNewTile as jest.MockedFunction<
      NonNullable<typeof Internal.basecampOpenFolderAsNewTile>
    >;
  const mockExecuteCommand = vscode.commands.executeCommand as jest.MockedFunction<
    typeof vscode.commands.executeCommand
  >;

  const sendOpenInNewWindow = (platform: ReturnType<typeof getVSCodePlatform>) => {
    const message: PlatformSpecificClientToServerMessages = {
      type: 'platform/openInNewWindow',
      path: '/repo/root.worktrees/root_2',
    };
    return platform.handleMessageFromClient.call(
      platform,
      undefined,
      mockCtx,
      message,
      jest.fn() as (msg: ServerToClientMessage) => void,
      jest.fn(),
      jest.fn(),
    );
  };

  beforeEach(() => {
    jest.clearAllMocks();
    mockIsBasecamp.mockReturnValue(false);
  });

  it('opens a plain new window outside of Basecamp', async () => {
    const platform = getVSCodePlatform(mockExtensionContext);
    await sendOpenInNewWindow(platform);

    expect(mockBasecampOpenFolderAsNewTile).not.toHaveBeenCalled();
    expect(mockExecuteCommand).toHaveBeenCalledWith(
      'vscode.openFolder',
      vscode.Uri.file('/repo/root.worktrees/root_2'),
      {forceNewWindow: true},
    );
  });

  it('opens a new tile inside Basecamp without falling back to a new window', async () => {
    mockIsBasecamp.mockReturnValue(true);
    mockBasecampOpenFolderAsNewTile.mockResolvedValue(true);
    const platform = getVSCodePlatform(mockExtensionContext);
    await sendOpenInNewWindow(platform);

    expect(mockBasecampOpenFolderAsNewTile).toHaveBeenCalledWith(
      '/repo/root.worktrees/root_2',
      undefined,
    );
    expect(mockExecuteCommand).not.toHaveBeenCalledWith(
      'vscode.openFolder',
      expect.anything(),
      expect.anything(),
    );
  });

  it('shows an error and does not fall back to a new window when Basecamp tile creation fails', async () => {
    mockIsBasecamp.mockReturnValue(true);
    mockBasecampOpenFolderAsNewTile.mockResolvedValue(false);
    const platform = getVSCodePlatform(mockExtensionContext);
    await sendOpenInNewWindow(platform);

    expect(mockExecuteCommand).not.toHaveBeenCalledWith(
      'vscode.openFolder',
      expect.anything(),
      expect.anything(),
    );
    expect(vscode.window.showErrorMessage).toHaveBeenCalledWith(
      expect.stringContaining('/repo/root.worktrees/root_2'),
    );
  });
});
