/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import {Set as ImSet} from 'immutable';
import type {Repository} from 'isl-server/src/Repository';
import {repositoryCache} from 'isl-server/src/RepositoryCache';
import type {RepositoryContext} from 'isl-server/src/serverTypes';
import type {RunnableOperation, WorktreeInfo} from 'isl/src/types';
import fs from 'node:fs';
import {ComparisonType, type Comparison} from 'shared/Comparison';
import * as vscode from 'vscode';
import {vscodeCommands} from '../commands';
import {shouldOpenBeside} from '../config';
import {encodeDeletedFileUri} from '../DeletedFileContentProvider';
import {encodeSaplingDiffUri} from '../DiffContentProvider';
import {Internal} from '../Internal';

// Mock vscode command
jest.mock('vscode', () => {
  const actualVscode = jest.requireActual('../../__mocks__/vscode');
  return {
    ...actualVscode,
    commands: {
      executeCommand: jest.fn(),
    },
  };
});
const mockExecuteVSCodeCommand = vscode.commands.executeCommand as jest.MockedFunction<
  typeof vscode.commands.executeCommand
>;

// Mock fs access
jest.mock('node:fs', () => ({
  promises: {
    access: jest.fn(),
  },
}));
const mockFsAccess = fs.promises.access as jest.MockedFunction<typeof fs.promises.access>;

// Mock global config
jest.mock('../config', () => ({
  shouldOpenBeside: jest.fn(),
}));
const mockShouldOpenBeside = shouldOpenBeside as jest.MockedFunction<typeof shouldOpenBeside>;

// Mock Internal (fb-only) API used for Basecamp tile detection.
jest.mock('../Internal', () => ({
  Internal: {
    isBasecamp: jest.fn(),
    basecampOpenFolderAsNewTile: jest.fn(),
  },
}));
const mockIsBasecamp = Internal.isBasecamp as jest.MockedFunction<
  NonNullable<typeof Internal.isBasecamp>
>;
const mockBasecampOpenFolderAsNewTile = Internal.basecampOpenFolderAsNewTile as jest.MockedFunction<
  NonNullable<typeof Internal.basecampOpenFolderAsNewTile>
>;

describe('open-file-diff', () => {
  const openDiffView = vscodeCommands['sapling.open-file-diff'];

  const repoRoot = '/repo/root';
  const filePath = 'path/to/file';
  const submodulePath = 'path/to/submodule';
  const fileUri = vscode.Uri.file(`${repoRoot}/${filePath}`);
  const submoduleUri = vscode.Uri.file(`${repoRoot}/${submodulePath}`);

  // Create a proper mock repository
  const mockRepo = {
    info: {
      repoRoot,
    },
    getSubmodulePathCache: jest.fn(),
  } as unknown as jest.Mocked<Repository>;

  beforeEach(() => {
    jest.clearAllMocks();

    jest.spyOn(repositoryCache, 'cachedRepositoryForPath').mockReturnValue(mockRepo);
    mockRepo.getSubmodulePathCache.mockReturnValue(ImSet([submodulePath]));
    mockShouldOpenBeside.mockReturnValue(false);
  });

  it('uncommitted changes, regular file', async () => {
    mockFsAccess.mockResolvedValue(undefined); // File exists

    const comparison: Comparison = {type: ComparisonType.UncommittedChanges};
    await openDiffView(fileUri, comparison);

    const expectedLeftRev = '.';
    const expectedLeftUri = encodeSaplingDiffUri(fileUri, expectedLeftRev);

    expect(mockExecuteVSCodeCommand).toHaveBeenCalledWith(
      'vscode.diff',
      expectedLeftUri,
      fileUri,
      'file (Uncommitted Changes)',
      {viewColumn: undefined},
    );
  });

  it('uncommitted changes, submodule', async () => {
    mockFsAccess.mockRejectedValue(undefined); // Path exists

    const comparison: Comparison = {type: ComparisonType.UncommittedChanges};
    await openDiffView(submoduleUri, comparison);

    const expectedLeftRev = '.';
    const expectedLeftUri = encodeSaplingDiffUri(submoduleUri, expectedLeftRev);
    const expectedRightRev = 'wdir()';
    const expectedRightUri = encodeSaplingDiffUri(submoduleUri, expectedRightRev);

    expect(mockExecuteVSCodeCommand).toHaveBeenCalledWith(
      'vscode.diff',
      expectedLeftUri,
      expectedRightUri,
      'submodule (Uncommitted Changes)',
      {viewColumn: undefined},
    );
  });

  it('uncommitted changes, file deleted', async () => {
    mockFsAccess.mockRejectedValue(new Error('File not found')); // File doesn't exist

    const comparison: Comparison = {type: ComparisonType.UncommittedChanges};
    await openDiffView(fileUri, comparison);

    const expectedLeftRev = '.';
    const expectedLeftUri = encodeSaplingDiffUri(fileUri, expectedLeftRev);
    const expectedRightUri = encodeDeletedFileUri(fileUri);

    expect(mockExecuteVSCodeCommand).toHaveBeenCalledWith(
      'vscode.diff',
      expectedLeftUri,
      expectedRightUri,
      'file (Uncommitted Changes)',
      {viewColumn: undefined},
    );
  });

  it('head changes, regular file', async () => {
    mockFsAccess.mockResolvedValue(undefined); // File exists

    const comparison: Comparison = {type: ComparisonType.HeadChanges};
    await openDiffView(fileUri, comparison);

    const expectedLeftRev = '.^';
    const expectedLeftUri = encodeSaplingDiffUri(fileUri, expectedLeftRev);

    expect(mockExecuteVSCodeCommand).toHaveBeenCalledWith(
      'vscode.diff',
      expectedLeftUri,
      fileUri,
      'file (Head Changes)',
      {viewColumn: undefined},
    );
  });

  it('head changes, submodule', async () => {
    mockFsAccess.mockRejectedValue(undefined); // Path exists

    const comparison: Comparison = {type: ComparisonType.HeadChanges};
    await openDiffView(submoduleUri, comparison);

    const expectedLeftRev = '.^';
    const expectedLeftUri = encodeSaplingDiffUri(submoduleUri, expectedLeftRev);
    const expectedRightRev = 'wdir()';
    const expectedRightUri = encodeSaplingDiffUri(submoduleUri, expectedRightRev);

    expect(mockExecuteVSCodeCommand).toHaveBeenCalledWith(
      'vscode.diff',
      expectedLeftUri,
      expectedRightUri,
      'submodule (Head Changes)',
      {viewColumn: undefined},
    );
  });

  it('stack changes, regular file', async () => {
    mockFsAccess.mockResolvedValue(undefined); // File exists

    const comparison: Comparison = {type: ComparisonType.StackChanges};
    await openDiffView(fileUri, comparison);

    const expectedLeftRev = 'ancestor(.,interestingmaster())';
    const expectedLeftUri = encodeSaplingDiffUri(fileUri, expectedLeftRev);

    expect(mockExecuteVSCodeCommand).toHaveBeenCalledWith(
      'vscode.diff',
      expectedLeftUri,
      fileUri,
      'file (Stack Changes)',
      {viewColumn: undefined},
    );
  });

  it('stack changes, submodule', async () => {
    mockFsAccess.mockRejectedValue(undefined); // Path exists

    const comparison: Comparison = {type: ComparisonType.StackChanges};
    await openDiffView(submoduleUri, comparison);

    const expectedLeftRev = 'ancestor(.,interestingmaster())';
    const expectedLeftUri = encodeSaplingDiffUri(submoduleUri, expectedLeftRev);
    const expectedRightRev = 'wdir()';
    const expectedRightUri = encodeSaplingDiffUri(submoduleUri, expectedRightRev);

    expect(mockExecuteVSCodeCommand).toHaveBeenCalledWith(
      'vscode.diff',
      expectedLeftUri,
      expectedRightUri,
      'submodule (Stack Changes)',
      {viewColumn: undefined},
    );
  });

  it('committed changes, regular file', async () => {
    const comparison: Comparison = {type: ComparisonType.Committed, hash: 'abc123'};
    await openDiffView(fileUri, comparison);

    const expectedLeftRev = 'abc123^';
    const expectedLeftUri = encodeSaplingDiffUri(fileUri, expectedLeftRev);
    const expectedRightRev = 'abc123';
    const expectedRightUri = encodeSaplingDiffUri(fileUri, expectedRightRev);

    expect(mockExecuteVSCodeCommand).toHaveBeenCalledWith(
      'vscode.diff',
      expectedLeftUri,
      expectedRightUri,
      'file (In abc123)',
      {viewColumn: undefined},
    );
  });

  it('committed changes, submodule', async () => {
    const comparison: Comparison = {type: ComparisonType.Committed, hash: 'abc123'};
    await openDiffView(submoduleUri, comparison);

    const expectedLeftRev = 'abc123^';
    const expectedLeftUri = encodeSaplingDiffUri(submoduleUri, expectedLeftRev);
    const expectedRightRev = 'abc123';
    const expectedRightUri = encodeSaplingDiffUri(submoduleUri, expectedRightRev);

    expect(mockExecuteVSCodeCommand).toHaveBeenCalledWith(
      'vscode.diff',
      expectedLeftUri,
      expectedRightUri,
      'submodule (In abc123)',
      {viewColumn: undefined},
    );
  });

  it('commit range, regular file', async () => {
    const comparison: Comparison = {
      type: ComparisonType.CommitRange,
      hashFrom: 'abc111',
      hashTo: 'def222',
    };
    await openDiffView(fileUri, comparison);

    const expectedLeftRev = 'abc111^';
    const expectedLeftUri = encodeSaplingDiffUri(fileUri, expectedLeftRev);
    const expectedRightRev = 'def222';
    const expectedRightUri = encodeSaplingDiffUri(fileUri, expectedRightRev);

    expect(mockExecuteVSCodeCommand).toHaveBeenCalledWith(
      'vscode.diff',
      expectedLeftUri,
      expectedRightUri,
      'file (abc111 to def222)',
      {viewColumn: undefined},
    );
  });

  it('commit range, submodule', async () => {
    const comparison: Comparison = {
      type: ComparisonType.CommitRange,
      hashFrom: 'abc111',
      hashTo: 'def222',
    };
    await openDiffView(submoduleUri, comparison);

    const expectedLeftRev = 'abc111^';
    const expectedLeftUri = encodeSaplingDiffUri(submoduleUri, expectedLeftRev);
    const expectedRightRev = 'def222';
    const expectedRightUri = encodeSaplingDiffUri(submoduleUri, expectedRightRev);

    expect(mockExecuteVSCodeCommand).toHaveBeenCalledWith(
      'vscode.diff',
      expectedLeftUri,
      expectedRightUri,
      'submodule (abc111 to def222)',
      {viewColumn: undefined},
    );
  });
});

describe('multi-diff editor resource open commands', () => {
  const openHead = vscodeCommands['sapling.open-multi-diff-file-head'];

  const repoRoot = '/repo/root';
  const fileUri = vscode.Uri.file(`${repoRoot}/path/to/file`);
  const mockShowTextDocument = vscode.window.showTextDocument as jest.MockedFunction<
    typeof vscode.window.showTextDocument
  >;

  beforeEach(() => {
    jest.clearAllMocks();
    mockShouldOpenBeside.mockReturnValue(false);
  });

  describe('open-multi-diff-file-head', () => {
    it('decodes a sapling-diff URI to the working-copy file URI', async () => {
      mockFsAccess.mockResolvedValue(undefined); // file exists
      const modifiedUri = encodeSaplingDiffUri(fileUri, 'abc123');
      await openHead([modifiedUri, modifiedUri]);

      expect(mockShowTextDocument).toHaveBeenCalledTimes(1);
      const [openedUri, opts] = mockShowTextDocument.mock.calls[0];
      expect((openedUri as vscode.Uri).toString()).toBe(fileUri.toString());
      expect(opts).toEqual({viewColumn: undefined});
    });

    it('decodes a deleted-file URI to the working-copy file URI', async () => {
      mockFsAccess.mockResolvedValue(undefined); // file exists
      const deletedUri = encodeDeletedFileUri(fileUri);
      await openHead([deletedUri, deletedUri]);

      expect(mockShowTextDocument).toHaveBeenCalledTimes(1);
      const [openedUri, opts] = mockShowTextDocument.mock.calls[0];
      expect((openedUri as vscode.Uri).toString()).toBe(fileUri.toString());
      expect(opts).toEqual({viewColumn: undefined});
    });

    it('opens a raw file URI as-is', async () => {
      mockFsAccess.mockResolvedValue(undefined); // file exists
      await openHead([fileUri, fileUri]);

      expect(mockShowTextDocument).toHaveBeenCalledWith(fileUri, {viewColumn: undefined});
    });

    it('does not open when the working-copy file does not exist', async () => {
      mockFsAccess.mockRejectedValue(new Error('File not found'));
      const modifiedUri = encodeSaplingDiffUri(fileUri, 'abc123');
      await openHead([modifiedUri, modifiedUri]);

      expect(mockShowTextDocument).not.toHaveBeenCalled();
    });
  });
});

describe('worktree commands', () => {
  const repoRoot = '/repo/root';
  const mainWorktree = {path: repoRoot, role: 'main' as const};
  const siblingWorktree = {
    path: '/repo/root.worktrees/root_2',
    role: 'linked' as const,
    label: 'sibling',
  };
  const worktreeInfoFixture: WorktreeInfo = {
    sharedRoot: repoRoot,
    worktrees: [mainWorktree, siblingWorktree],
    supportsMutation: true,
  };

  const mockRepo = {
    info: {
      repoRoot,
      isEdenFs: true,
      codeReviewSystem: {type: 'phabricator'},
    },
    refreshWorktreeInfo: jest.fn().mockResolvedValue(undefined),
    getWorktreeInfo: jest.fn().mockReturnValue(worktreeInfoFixture),
    // Simulate a successful command exit by default, matching what
    // `Repository.runOrQueueOperation` reports for a real, successful run.
    runOrQueueOperation: jest.fn(
      (
        _ctx: RepositoryContext,
        operation: RunnableOperation,
        onProgress: (progress: {
          id: string;
          kind: 'exit';
          exitCode: number;
          timestamp: number;
        }) => void,
      ) => {
        onProgress({id: operation.id, kind: 'exit', exitCode: 0, timestamp: Date.now()});
        return Promise.resolve('ran' as const);
      },
    ),
  } as unknown as jest.Mocked<Repository>;

  const mockShowQuickPick = vscode.window.showQuickPick as jest.MockedFunction<
    typeof vscode.window.showQuickPick
  >;
  const mockShowInputBox = vscode.window.showInputBox as jest.MockedFunction<
    typeof vscode.window.showInputBox
  >;
  const mockShowWarningMessage = vscode.window.showWarningMessage as jest.MockedFunction<
    typeof vscode.window.showWarningMessage
  >;
  const mockShowErrorMessage = vscode.window.showErrorMessage as jest.MockedFunction<
    typeof vscode.window.showErrorMessage
  >;
  const mockWithProgress = vscode.window.withProgress as jest.MockedFunction<
    typeof vscode.window.withProgress
  >;
  const ctx = {} as never;

  beforeEach(() => {
    jest.clearAllMocks();
    jest.spyOn(repositoryCache, 'getAllRepositories').mockReturnValue([mockRepo]);
    mockRepo.getWorktreeInfo.mockReturnValue(worktreeInfoFixture);
    // Default to "nothing exists on disk" so default destination path computation
    // in sapling.worktree.add isn't affected unless a test overrides this.
    mockFsAccess.mockRejectedValue(new Error('ENOENT'));
    mockIsBasecamp.mockReturnValue(false);
  });

  describe('sapling.worktree.switch', () => {
    const switchCommand = vscodeCommands['sapling.worktree.switch'];

    it('opens the selected worktree in a new window', async () => {
      mockShowQuickPick
        .mockResolvedValueOnce({
          label: 'sibling',
          description: siblingWorktree.path,
          worktree: siblingWorktree,
        } as never)
        .mockResolvedValueOnce({label: 'Open in New Window', forceNewWindow: true} as never);

      await switchCommand.apply(ctx);

      expect(mockExecuteVSCodeCommand).toHaveBeenCalledWith(
        'vscode.openFolder',
        vscode.Uri.file(siblingWorktree.path),
        {forceNewWindow: true},
      );
    });

    it('does nothing when the worktree picker is cancelled', async () => {
      mockShowQuickPick.mockResolvedValueOnce(undefined);

      await switchCommand.apply(ctx);

      expect(mockExecuteVSCodeCommand).not.toHaveBeenCalledWith(
        'vscode.openFolder',
        expect.anything(),
        expect.anything(),
      );
    });

    it('opens the selected worktree as a new tile inside Basecamp without asking which window', async () => {
      mockIsBasecamp.mockReturnValue(true);
      mockBasecampOpenFolderAsNewTile.mockResolvedValue(true);
      mockShowQuickPick.mockResolvedValueOnce({
        label: 'sibling',
        description: siblingWorktree.path,
        worktree: siblingWorktree,
      } as never);

      await switchCommand.apply(ctx);

      // Only the worktree picker is shown; there's no "current window" choice in Basecamp.
      expect(mockShowQuickPick).toHaveBeenCalledTimes(1);
      expect(mockBasecampOpenFolderAsNewTile).toHaveBeenCalledWith(
        siblingWorktree.path,
        siblingWorktree.label,
      );
      expect(mockExecuteVSCodeCommand).not.toHaveBeenCalledWith(
        'vscode.openFolder',
        expect.anything(),
        expect.anything(),
      );
    });

    it('shows an error and does not fall back to a new window when Basecamp tile creation fails', async () => {
      mockIsBasecamp.mockReturnValue(true);
      mockBasecampOpenFolderAsNewTile.mockResolvedValue(false);
      mockShowQuickPick.mockResolvedValueOnce({
        label: 'sibling',
        description: siblingWorktree.path,
        worktree: siblingWorktree,
      } as never);

      await switchCommand.apply(ctx);

      expect(mockBasecampOpenFolderAsNewTile).toHaveBeenCalledWith(
        siblingWorktree.path,
        siblingWorktree.label,
      );
      expect(mockExecuteVSCodeCommand).not.toHaveBeenCalledWith(
        'vscode.openFolder',
        expect.anything(),
        expect.anything(),
      );
      expect(mockShowErrorMessage).toHaveBeenCalledWith(
        expect.stringContaining(siblingWorktree.path),
      );
    });
  });

  describe('sapling.worktree.add', () => {
    const addCommand = vscodeCommands['sapling.worktree.add'];

    it('runs AddWorktreeOperation with the expected args', async () => {
      mockShowInputBox
        .mockResolvedValueOnce('my-label')
        .mockResolvedValueOnce('/repo/root.worktrees/root_3');
      mockShowQuickPick.mockResolvedValueOnce(undefined);

      await addCommand.apply(ctx);

      expect(mockRepo.runOrQueueOperation).toHaveBeenCalledWith(
        ctx,
        expect.objectContaining({
          args: [
            {type: 'config', key: 'worktree.enabled', value: 'true'},
            'worktree',
            'add',
            '/repo/root.worktrees/root_3',
            '--label',
            'my-label',
          ],
        }),
        expect.anything(),
      );
    });

    it('skips a default destination that already exists on disk', async () => {
      mockShowInputBox.mockResolvedValueOnce('my-label').mockResolvedValueOnce(undefined);
      mockFsAccess.mockImplementation(p =>
        p === '/repo/root.worktrees/root_3'
          ? Promise.resolve()
          : Promise.reject(new Error('ENOENT')),
      );

      await addCommand.apply(ctx);

      expect(mockShowInputBox).toHaveBeenLastCalledWith(
        expect.objectContaining({value: '/repo/root.worktrees/root_4'}),
      );
    });

    it('shows a progress notification while the worktree is being created', async () => {
      mockShowInputBox
        .mockResolvedValueOnce('my-label')
        .mockResolvedValueOnce('/repo/root.worktrees/root_3');
      mockShowQuickPick.mockResolvedValueOnce(undefined);

      await addCommand.apply(ctx);

      expect(mockWithProgress).toHaveBeenCalledWith(
        expect.objectContaining({title: 'Creating worktree...'}),
        expect.anything(),
      );
    });

    it('does not prompt to open the worktree when creation fails', async () => {
      mockShowInputBox
        .mockResolvedValueOnce('my-label')
        .mockResolvedValueOnce('/repo/root.worktrees/root_3');
      mockRepo.runOrQueueOperation.mockImplementationOnce(
        (_ctx: RepositoryContext, operation: RunnableOperation, onProgress) => {
          onProgress({id: operation.id, kind: 'exit', exitCode: 1, timestamp: Date.now()});
          return Promise.resolve('ran' as const);
        },
      );

      await expect(addCommand.apply(ctx)).rejects.toThrow();

      expect(mockShowQuickPick).not.toHaveBeenCalled();
      expect(mockExecuteVSCodeCommand).not.toHaveBeenCalledWith(
        'vscode.openFolder',
        expect.anything(),
        expect.anything(),
      );
    });

    it('opens the new worktree as a new tile inside Basecamp, without offering current window', async () => {
      mockIsBasecamp.mockReturnValue(true);
      mockBasecampOpenFolderAsNewTile.mockResolvedValue(true);
      mockShowInputBox
        .mockResolvedValueOnce('my-label')
        .mockResolvedValueOnce('/repo/root.worktrees/root_3');
      mockShowQuickPick.mockResolvedValueOnce({
        label: 'Open in New Tile',
        action: 'open',
        forceNewWindow: true,
      } as never);

      await addCommand.apply(ctx);

      expect(mockShowQuickPick.mock.calls[0][0]).not.toEqual(
        expect.arrayContaining([expect.objectContaining({label: 'Open in Current Window'})]),
      );
      expect(mockBasecampOpenFolderAsNewTile).toHaveBeenCalledWith(
        '/repo/root.worktrees/root_3',
        'my-label',
      );
      expect(mockExecuteVSCodeCommand).not.toHaveBeenCalledWith(
        'vscode.openFolder',
        expect.anything(),
        expect.anything(),
      );
    });

    it('shows an error and does not fall back to a new window when Basecamp tile creation is unavailable', async () => {
      mockIsBasecamp.mockReturnValue(true);
      mockBasecampOpenFolderAsNewTile.mockResolvedValue(false);
      mockShowInputBox
        .mockResolvedValueOnce('my-label')
        .mockResolvedValueOnce('/repo/root.worktrees/root_3');
      mockShowQuickPick.mockResolvedValueOnce({
        label: 'Open in New Tile',
        action: 'open',
        forceNewWindow: true,
      } as never);

      await addCommand.apply(ctx);

      expect(mockExecuteVSCodeCommand).not.toHaveBeenCalledWith(
        'vscode.openFolder',
        expect.anything(),
        expect.anything(),
      );
      expect(mockShowErrorMessage).toHaveBeenCalledWith(
        expect.stringContaining('/repo/root.worktrees/root_3'),
      );
    });
  });

  describe('sapling.worktree.remove', () => {
    const removeCommand = vscodeCommands['sapling.worktree.remove'];

    it('excludes the main worktree from the picker and requires confirmation', async () => {
      mockShowQuickPick.mockResolvedValueOnce({
        label: 'sibling',
        description: siblingWorktree.path,
        worktree: siblingWorktree,
      } as never);
      mockShowWarningMessage.mockResolvedValueOnce('Remove' as never);

      await removeCommand.apply(ctx);

      expect(mockShowQuickPick.mock.calls[0][0]).not.toEqual(
        expect.arrayContaining([expect.objectContaining({worktree: mainWorktree})]),
      );
      expect(mockRepo.runOrQueueOperation).toHaveBeenCalledWith(
        ctx,
        expect.objectContaining({
          args: [
            {type: 'config', key: 'worktree.enabled', value: 'true'},
            'worktree',
            'remove',
            siblingWorktree.path,
            '--force',
          ],
        }),
        expect.anything(),
      );
    });

    it('shows a progress notification while the worktree is being removed', async () => {
      mockShowQuickPick.mockResolvedValueOnce({
        label: 'sibling',
        description: siblingWorktree.path,
        worktree: siblingWorktree,
      } as never);
      mockShowWarningMessage.mockResolvedValueOnce('Remove' as never);

      await removeCommand.apply(ctx);

      expect(mockWithProgress).toHaveBeenCalledWith(
        expect.objectContaining({title: 'Removing worktree...'}),
        expect.anything(),
      );
    });

    it('does not remove when the confirmation is declined', async () => {
      mockShowQuickPick.mockResolvedValueOnce({
        label: 'sibling',
        description: siblingWorktree.path,
        worktree: siblingWorktree,
      } as never);
      mockShowWarningMessage.mockResolvedValueOnce('Cancel' as never);

      await removeCommand.apply(ctx);

      expect(mockRepo.runOrQueueOperation).not.toHaveBeenCalled();
    });
  });

  describe('sapling.worktree.rename', () => {
    const renameCommand = vscodeCommands['sapling.worktree.rename'];

    it('removes the label when an empty label is submitted', async () => {
      mockShowQuickPick.mockResolvedValueOnce({
        label: 'sibling',
        description: siblingWorktree.path,
        worktree: siblingWorktree,
      } as never);
      mockShowInputBox.mockResolvedValueOnce('');

      await renameCommand.apply(ctx);

      expect(mockRepo.runOrQueueOperation).toHaveBeenCalledWith(
        ctx,
        expect.objectContaining({
          args: [
            {type: 'config', key: 'worktree.enabled', value: 'true'},
            'worktree',
            'label',
            siblingWorktree.path,
            '--remove',
          ],
        }),
        expect.anything(),
      );
    });

    it('shows a progress notification while the worktree is being renamed', async () => {
      mockShowQuickPick.mockResolvedValueOnce({
        label: 'sibling',
        description: siblingWorktree.path,
        worktree: siblingWorktree,
      } as never);
      mockShowInputBox.mockResolvedValueOnce('');

      await renameCommand.apply(ctx);

      expect(mockWithProgress).toHaveBeenCalledWith(
        expect.objectContaining({title: 'Renaming worktree...'}),
        expect.anything(),
      );
    });
  });

  it('shows an error for non-EdenFS repos', async () => {
    jest.spyOn(repositoryCache, 'getAllRepositories').mockReturnValue([
      {
        ...mockRepo,
        info: {...mockRepo.info, isEdenFs: false},
      } as unknown as Repository,
    ]);

    await vscodeCommands['sapling.worktree.switch'].apply(ctx);

    expect(mockShowErrorMessage).toHaveBeenCalledWith(
      'Worktrees require EdenFS. This repository is not backed by EdenFS',
    );
    expect(mockRepo.runOrQueueOperation).not.toHaveBeenCalled();
  });

  it('shows an error when worktree info is unavailable (GK off)', async () => {
    mockRepo.getWorktreeInfo.mockReturnValueOnce(undefined);

    await vscodeCommands['sapling.worktree.switch'].apply(ctx);

    expect(mockShowErrorMessage).toHaveBeenCalledWith(
      'Worktrees are not available for this repository',
    );
    expect(mockRepo.runOrQueueOperation).not.toHaveBeenCalled();
  });

  it('shows an error when no repositories are known', async () => {
    jest.spyOn(repositoryCache, 'getAllRepositories').mockReturnValue([]);

    await vscodeCommands['sapling.worktree.switch'].apply(ctx);

    expect(mockShowErrorMessage).toHaveBeenCalledWith(
      'No Sapling repository found in the current workspace',
    );
    expect(mockRepo.runOrQueueOperation).not.toHaveBeenCalled();
  });

  it('prompts to pick a repository when more than one is known', async () => {
    const otherRepo = {
      ...mockRepo,
      info: {...mockRepo.info, repoRoot: '/other/repo'},
    } as unknown as Repository;
    jest.spyOn(repositoryCache, 'getAllRepositories').mockReturnValue([mockRepo, otherRepo]);
    mockShowQuickPick
      .mockResolvedValueOnce({label: 'root', description: repoRoot, repo: mockRepo} as never)
      .mockResolvedValueOnce({
        label: 'sibling',
        description: siblingWorktree.path,
        worktree: siblingWorktree,
      } as never)
      .mockResolvedValueOnce({label: 'Open in New Window', forceNewWindow: true} as never);

    await vscodeCommands['sapling.worktree.switch'].apply(ctx);

    expect(mockShowQuickPick.mock.calls[0][0]).toEqual(
      expect.arrayContaining([
        expect.objectContaining({repo: mockRepo}),
        expect.objectContaining({repo: otherRepo}),
      ]),
    );
    expect(mockExecuteVSCodeCommand).toHaveBeenCalledWith(
      'vscode.openFolder',
      vscode.Uri.file(siblingWorktree.path),
      {forceNewWindow: true},
    );
  });
});
