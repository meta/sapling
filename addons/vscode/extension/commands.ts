/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import type {Repository} from 'isl-server/src/Repository';
import type {RepositoryContext} from 'isl-server/src/serverTypes';
import type {Operation} from 'isl/src/operations/Operation';
import type {PartiallySelectedDiffCommit} from 'isl/src/stackEdit/diffSplitTypes';
import type {AbsolutePath, OperationProgress, RepoRelativePath, WorktreeInfo} from 'isl/src/types';
import type {Comparison} from 'shared/Comparison';

import {repoRelativePathForAbsolutePath} from 'isl-server/src/Repository';
import {repositoryCache} from 'isl-server/src/RepositoryCache';
import {findPublicAncestor} from 'isl-server/src/utils';
import {AddWorktreeOperation} from 'isl/src/operations/AddWorktreeOperation';
import {RemoveWorktreeOperation} from 'isl/src/operations/RemoveWorktreeOperation';
import {RenameWorktreeOperation} from 'isl/src/operations/RenameWorktreeOperation';
import {RevertOperation} from 'isl/src/operations/RevertOperation';
import fs from 'node:fs';
import path from 'node:path';
import {
  beforeRevsetForComparison,
  ComparisonType,
  currRevsetForComparison,
  labelForComparison,
} from 'shared/Comparison';
import * as vscode from 'vscode';
import {shouldOpenBeside} from './config';
import {
  DELETED_FILE_DIFF_VIEW_PROVIDER_SCHEME,
  encodeDeletedFileUri,
} from './DeletedFileContentProvider';
import {
  decodeSaplingDiffUri,
  encodeSaplingDiffUri,
  SAPLING_DIFF_PROVIDER_SCHEME,
} from './DiffContentProvider';
import {t} from './i18n';
import {Internal} from './Internal';

/**
 * Open a folder in the current window, a new window, or (inside Basecamp) a new tile
 * in the current window.
 */
export async function openFolderInWindowOrTile(
  path: string,
  forceNewWindow: boolean,
  label?: string,
): Promise<void> {
  if (Internal.isBasecamp?.() === true) {
    const openedTile = await Internal.basecampOpenFolderAsNewTile?.(path, label);
    if (openedTile !== true) {
      vscode.window.showErrorMessage(
        t('Failed to open $path as a new Basecamp tile').replace('$path', path),
      );
    }
    return;
  }
  try {
    await vscode.commands.executeCommand('vscode.openFolder', vscode.Uri.file(path), {
      forceNewWindow,
    });
  } catch (err) {
    vscode.window.showErrorMessage(
      t('Failed to open folder ($path)').replace('$path', path) + `: ${err}`,
    );
  }
}

/**
 * VS Code Commands registered by the Sapling extension.
 */
export const vscodeCommands = {
  ['sapling.open-file-diff-uncommitted']: commandWithUriOrResourceState((_, uri: vscode.Uri) =>
    openDiffView(uri, {type: ComparisonType.UncommittedChanges}),
  ),
  ['sapling.open-file-diff-head']: commandWithUriOrResourceState((_, uri: vscode.Uri) =>
    openDiffView(uri, {type: ComparisonType.HeadChanges}),
  ),
  ['sapling.open-file-diff-stack']: commandWithUriOrResourceState((_, uri: vscode.Uri) =>
    openDiffView(uri, {type: ComparisonType.StackChanges}),
  ),
  ['sapling.open-file-diff']: (uri: vscode.Uri, comparison: Comparison) =>
    openDiffView(uri, comparison),

  ['sapling.open-remote-file-link']: commandWithUriOrResourceState(
    (repo: Repository, uri, path: RepoRelativePath) => openRemoteFileLink(repo, uri, path),
  ),
  ['sapling.copy-remote-file-link']: commandWithUriOrResourceState(
    (repo: Repository, uri, path: RepoRelativePath) => openRemoteFileLink(repo, uri, path, true),
  ),

  ['sapling.revert-file']: commandWithUriOrResourceState(async function (
    this: RepositoryContext,
    repo: Repository,
    _,
    path: RepoRelativePath,
  ) {
    const choice = await vscode.window.showWarningMessage(
      'Are you sure you want to revert this file?',
      'Cancel',
      'Revert',
    );
    if (choice !== 'Revert') {
      return;
    }
    return runOperation(this, repo, new RevertOperation([path]));
  }),

  ['sapling.worktree.switch']: async function (this: RepositoryContext) {
    const resolved = await resolveWorktreeRepo();
    if (resolved == null) {
      return;
    }
    const {repo, worktreeInfo} = resolved;
    const others = worktreeInfo.worktrees.filter(wt => wt.path !== repo.info.repoRoot);
    if (others.length === 0) {
      vscode.window.showInformationMessage(t('No other worktrees to switch to'));
      return;
    }
    const picked = await vscode.window.showQuickPick(
      others.map(wt => ({
        label: wt.label || path.basename(wt.path),
        description: wt.path,
        worktree: wt,
      })),
      {placeHolder: t('Select a worktree to switch to')},
    );
    if (picked == null) {
      return;
    }
    const isBasecamp = Internal.isBasecamp?.() === true;
    let forceNewWindow: boolean;
    if (isBasecamp) {
      // Basecamp always opens worktrees as a new tile; there's no "current window" to switch.
      forceNewWindow = true;
    } else {
      const choice = await vscode.window.showQuickPick(
        [
          {label: t('Open in Current Window'), forceNewWindow: false},
          {label: t('Open in New Window'), forceNewWindow: true},
        ],
        {placeHolder: t('Opening in current window will reload the editor.')},
      );
      if (choice == null) {
        return;
      }
      forceNewWindow = choice.forceNewWindow;
    }
    await openFolderInWindowOrTile(picked.worktree.path, forceNewWindow, picked.worktree.label);
  },

  ['sapling.worktree.add']: async function (this: RepositoryContext) {
    const resolved = await resolveWorktreeRepo();
    if (resolved == null) {
      return;
    }
    const {repo, worktreeInfo} = resolved;

    const label = await vscode.window.showInputBox({prompt: t('Label (optional)')});
    if (label == null) {
      return;
    }

    const mainRoot =
      worktreeInfo.worktrees.find(wt => wt.role === 'main')?.path ?? repo.info.repoRoot;
    const repoName = path.basename(mainRoot);
    const worktreesDir = path.join(path.dirname(mainRoot), `${repoName}.worktrees`);
    const existingBasenamesInDir = new Set(
      worktreeInfo.worktrees
        .filter(wt => path.dirname(wt.path) === worktreesDir)
        .map(wt => path.basename(wt.path)),
    );
    const MAX_WORKTREE_SUFFIX = 25;
    let suffix = 2;
    let defaultDest = path.join(worktreesDir, `${repoName}_${suffix}`);
    // Find a free suffix, checking both known worktrees in the same dir and on-disk existence.
    // The suffix range 2..MAX_WORKTREE_SUFFIX inclusive is tried.
    while (true) {
      const basename = `${repoName}_${suffix}`;
      if (!existingBasenamesInDir.has(basename)) {
        // eslint-disable-next-line no-await-in-loop
        const existsOnDisk = await fileExists(vscode.Uri.file(defaultDest));
        if (!existsOnDisk) {
          break;
        }
      }
      suffix++;
      if (suffix > MAX_WORKTREE_SUFFIX) {
        throw new Error(t('Could not add worktree, exceeded maximum allowed worktrees.'));
      }
      defaultDest = path.join(worktreesDir, `${repoName}_${suffix}`);
    }

    const destPath = await vscode.window.showInputBox({
      prompt: t('Worktree root path'),
      value: defaultDest,
    });
    if (destPath == null) {
      return;
    }
    const trimmedDestPath = destPath.trim();
    if (trimmedDestPath === '') {
      return;
    }

    await runOperationWithProgress(
      this,
      repo,
      new AddWorktreeOperation(trimmedDestPath, label.trim() || undefined),
      t('Creating worktree...'),
    );

    const isBasecamp = Internal.isBasecamp?.() === true;
    const openChoice = await vscode.window.showQuickPick(
      [
        {
          label: isBasecamp ? t('Open in New Tile') : t('Open in New Window'),
          action: 'open' as const,
          forceNewWindow: true,
        },
        ...(isBasecamp
          ? []
          : [{label: t('Open in Current Window'), action: 'open' as const, forceNewWindow: false}]),
        {label: t("Don't open"), action: 'skip' as const, forceNewWindow: false},
      ],
      {placeHolder: t('Open the new worktree?')},
    );
    if (openChoice?.action === 'open') {
      await openFolderInWindowOrTile(
        trimmedDestPath,
        openChoice.forceNewWindow,
        label.trim() || undefined,
      );
    }
  },

  ['sapling.worktree.remove']: async function (this: RepositoryContext) {
    const resolved = await resolveWorktreeRepo();
    if (resolved == null) {
      return;
    }
    const {repo, worktreeInfo} = resolved;
    const removable = worktreeInfo.worktrees.filter(
      wt => wt.role !== 'main' && wt.path !== repo.info.repoRoot,
    );
    if (removable.length === 0) {
      vscode.window.showInformationMessage(t('No worktrees to remove'));
      return;
    }
    const picked = await vscode.window.showQuickPick(
      removable.map(wt => ({
        label: wt.label || path.basename(wt.path),
        description: wt.path,
        worktree: wt,
      })),
      {placeHolder: t('Select a worktree to remove')},
    );
    if (picked == null) {
      return;
    }
    const choice = await vscode.window.showWarningMessage(
      t(
        'Are you sure you want to remove this worktree? Any uncommitted changes and shelves in this worktree will be lost forever.',
      ),
      t('Cancel'),
      t('Remove'),
    );
    if (choice !== t('Remove')) {
      return;
    }
    return runOperationWithProgress(
      this,
      repo,
      new RemoveWorktreeOperation(picked.worktree.path),
      t('Removing worktree...'),
    );
  },

  ['sapling.worktree.rename']: async function (this: RepositoryContext) {
    const resolved = await resolveWorktreeRepo();
    if (resolved == null) {
      return;
    }
    const {repo, worktreeInfo} = resolved;
    const picked = await vscode.window.showQuickPick(
      worktreeInfo.worktrees.map(wt => ({
        label: wt.label || path.basename(wt.path),
        description: wt.path,
        worktree: wt,
      })),
      {placeHolder: t('Select a worktree to rename')},
    );
    if (picked == null) {
      return;
    }
    const newLabel = await vscode.window.showInputBox({
      prompt: t('Label (leave empty to remove)'),
      value: picked.worktree.label ?? '',
    });
    if (newLabel == null) {
      return;
    }
    return runOperationWithProgress(
      this,
      repo,
      new RenameWorktreeOperation(picked.worktree.path, newLabel.trim() || undefined),
      t('Renaming worktree...'),
    );
  },

  // Open the working-copy / head version of a file row in VS Code's native multi-diff
  // editor. Only contributed for committed comparisons: there the editor's built-in
  // "Open File" button opens the read-only in-diff version, so this provides access to the
  // editable working-copy file. (For uncommitted comparisons the built-in button already
  // opens the working copy, so no extra button is needed.)
  ['sapling.open-multi-diff-file-head']: async (arg: MultiDiffResourceArg) => {
    const modified = modifiedUriFromMultiDiffArg(arg);
    if (modified == null) {
      vscode.window.showErrorMessage(t(`No file found`));
      return;
    }
    const fileUri = workingCopyUriForModifiedUri(modified);
    if (!(await fileExists(fileUri))) {
      vscode.window.showInformationMessage(t(`This file does not exist in the working copy`));
      return;
    }
    return openInPreferredColumn(fileUri);
  },
};

/**
 * Argument shape passed to commands invoked from the `multiDiffEditor/resource/title`
 * menu. The Meta VS Code build (T223719719) passes a 2-element array
 * `[modifiedUri ?? originalUri, modifiedUri]`; upstream passes a single `vscode.Uri`.
 * Handle both defensively.
 */
type MultiDiffResourceArg =
  vscode.Uri | [vscode.Uri | undefined, vscode.Uri | undefined] | undefined;

function modifiedUriFromMultiDiffArg(arg: MultiDiffResourceArg): vscode.Uri | undefined {
  if (arg == null) {
    return undefined;
  }
  if (Array.isArray(arg)) {
    return arg[1] ?? arg[0];
  }
  return arg;
}

/** Recover the working-copy `file://` URI from a multi-diff editor's modified resource URI. */
export function workingCopyUriForModifiedUri(uri: vscode.Uri): vscode.Uri {
  if (uri.scheme === SAPLING_DIFF_PROVIDER_SCHEME) {
    return decodeSaplingDiffUri(uri).originalUri;
  }
  if (uri.scheme === DELETED_FILE_DIFF_VIEW_PROVIDER_SCHEME) {
    return uri.with({scheme: 'file', query: ''});
  }
  return uri;
}

function openInPreferredColumn(uri: vscode.Uri): Thenable<unknown> {
  return vscode.window.showTextDocument(uri, {
    viewColumn: shouldOpenBeside() ? vscode.ViewColumn.Beside : undefined,
  });
}

type surveyMetaData = {
  diffId: string | undefined;
};

/** Type definitions for built-in or third-party VS Code commands we want to execute programmatically. */
type ExternalVSCodeCommands = {
  'vscode.diff': (
    left: vscode.Uri,
    right: vscode.Uri,
    title: string,
    opts?: vscode.TextDocumentShowOptions,
  ) => Thenable<unknown>;
  'workbench.action.closeSidebar': () => Thenable<void>;
  'fb.survey.initStateUIByNamespace': (
    surveyID: string,
    namespace: string,
    metadata: surveyMetaData,
  ) => Thenable<void>;
  'workbench.action.pinEditor': () => Thenable<void>;
  'sapling.open-isl': () => Thenable<void>;
  'sapling.close-isl': () => Thenable<void>;
  'sapling.isl.focus': () => Thenable<void>;
  'sapling.open-isl-with-commit-message': (
    title: string,
    description: string,
    mode?: 'commit' | 'amend',
    hash?: string,
  ) => Thenable<void>;
  'sapling.open-split-view-with-commits': (
    commits: Array<PartiallySelectedDiffCommit>,
    commitHash?: string,
  ) => Thenable<void>;
  'sapling.open-comparison-view': (comparison: Comparison) => Thenable<void>;
  setContext: (key: string, value: unknown) => Thenable<void>;
  'fb-hg.open-or-focus-interactive-smartlog': (
    _: unknown,
    __?: unknown,
    forceNoSapling?: boolean,
  ) => Thenable<void>;
};

export type VSCodeCommand = typeof vscodeCommands & ExternalVSCodeCommands;

/**
 * Type-safe programmatic execution of VS Code commands (via `vscode.commands.executeCommand`).
 * Sapling-provided commands are defined in vscodeCommands.
 * Built-in or third-party commands may also be typed through this function,
 * just define them in ExternalVSCodeCommands.
 */
export function executeVSCodeCommand<K extends keyof VSCodeCommand>(
  id: K,
  ...args: Parameters<VSCodeCommand[K]>
): ReturnType<VSCodeCommand[K]> {
  // In tests 'vscode.commands' is not defined.
  return vscode.commands?.executeCommand(id, ...args) as ReturnType<VSCodeCommand[K]>;
}

const runOperation = async (
  ctx: RepositoryContext,
  repo: Repository,
  operation: Operation,
): Promise<void> => {
  let exitCode: number | undefined;
  let errorMessage: string | undefined;

  const result = await repo.runOrQueueOperation(
    ctx,
    {
      args: operation.getArgs(),
      id: operation.id,
      runner: operation.runner,
      trackEventName: operation.trackEventName,
    },
    (progress: OperationProgress) => {
      // TODO: Send this progress info to any existing ISL webview if there is one
      if (progress.kind === 'exit') {
        exitCode = progress.exitCode;
      } else if (progress.kind === 'error') {
        errorMessage = progress.error;
      }
    },
  );

  if (errorMessage != null) {
    throw new Error(errorMessage);
  }
  if (result === 'skipped') {
    throw new Error(t('Operation was skipped because a previous operation failed'));
  }
  if (exitCode != null && exitCode !== 0) {
    throw new Error(t('Command exited with code $code').replace('$code', String(exitCode)));
  }
};

/**
 * Like `runOperation`, but shows a progress notification while the operation runs.
 * Palette-invoked worktree operations otherwise give no feedback until they finish.
 */
async function runOperationWithProgress(
  ctx: RepositoryContext,
  repo: Repository,
  operation: Operation,
  title: string,
): Promise<void> {
  await vscode.window.withProgress({location: vscode.ProgressLocation.Notification, title}, () =>
    runOperation(ctx, repo, operation),
  );
}

export function registerCommands(ctx: RepositoryContext): Array<vscode.Disposable> {
  const disposables: Array<vscode.Disposable> = Object.entries(vscodeCommands).map(
    ([id, handler]) =>
      vscode.commands.registerCommand(id, (...args: Parameters<typeof handler>) =>
        ctx.tracker.operation(
          'RunVSCodeCommand',
          'VSCodeCommandError',
          {extras: {command: id}},
          () => {
            return (handler as (...args: Array<unknown>) => unknown).apply(ctx, args);
          },
        ),
      ),
  );
  return disposables;
}

function fileExists(uri: vscode.Uri): Promise<boolean> {
  return fs.promises
    .access(uri.fsPath)
    .then(() => true)
    .catch(() => false);
}

async function openDiffView(uri: vscode.Uri, comparison: Comparison): Promise<unknown> {
  const leftUri = getLeftUri(uri, comparison);
  const rightUri = await getRightUri(uri, comparison);
  const title = `${path.basename(uri.fsPath)} (${t(labelForComparison(comparison))})`;
  const opts = {viewColumn: shouldOpenBeside() ? vscode.ViewColumn.Beside : undefined};
  return executeVSCodeCommand('vscode.diff', leftUri, rightUri, title, opts);
}

function getLeftUri(uri: vscode.Uri, comparison: Comparison): vscode.Uri {
  const leftRev = beforeRevsetForComparison(comparison);
  return encodeSaplingDiffUri(uri, leftRev);
}

/**
 * Get the right side URI of the diff view.
 *
 * A raw file:// URI without encoding lets vscode directly open the file on disk.
 * This is desirable as users can edit the file on the right side of the diff view.
 *
 * There are cases, however, where editable right side does NOT make sense:
 * - comparing against a history commit (since changes on the right side may not be present)
 * - comparing submodule changes (since both side are commit hashes instead of file content)
 */
async function getRightUri(uri: vscode.Uri, comparison: Comparison): Promise<vscode.Uri> {
  const rightRev = currRevsetForComparison(comparison);
  if (
    comparison.type === ComparisonType.Committed ||
    comparison.type === ComparisonType.CommitRange ||
    isSubmodule(uri.fsPath)
  ) {
    return encodeSaplingDiffUri(uri, rightRev);
  }
  return (await fileExists(uri)) ? uri : encodeDeletedFileUri(uri);
}

function isSubmodule(path: AbsolutePath): boolean {
  const repo = repositoryCache.cachedRepositoryForPath(path);
  if (repo === undefined) {
    return false;
  }
  const submodulePaths = repo.getSubmodulePathCache();
  const relPath = repoRelativePathForAbsolutePath(path, repo);
  return submodulePaths?.has(relPath) ?? false;
}

function openRemoteFileLink(
  repo: Repository,
  uri: vscode.Uri,
  path: RepoRelativePath,
  copyToClipboard = false,
): void {
  {
    if (!repo.codeReviewProvider?.getRemoteFileURL) {
      vscode.window.showErrorMessage(
        t(`Remote link unsupported for this code review provider ($provider)`).replace(
          '$provider',
          repo.codeReviewProvider?.getSummaryName() ?? t('none'),
        ),
      );
      return;
    }

    // Grab the selection if the command is for the active file (may not be true if triggered via file explorer)
    const selection =
      vscode.window.activeTextEditor?.document.uri.fsPath === uri.fsPath
        ? vscode.window.activeTextEditor?.selection
        : null;

    const commits = repo.getSmartlogCommits()?.commits.value;
    const head = repo.getHeadCommit();
    if (!commits || !head) {
      vscode.window.showErrorMessage(t(`No commits loaded in this repository yet`));
      return;
    }
    const publicCommit = findPublicAncestor(commits, head);
    const url = repo.codeReviewProvider.getRemoteFileURL(
      path,
      publicCommit?.hash ?? null,
      selection ? {line: selection.start.line, char: selection.start.character} : undefined,
      selection ? {line: selection.end.line, char: selection.end.character} : undefined,
    );

    if (copyToClipboard) {
      vscode.env.clipboard.writeText(url);
    } else {
      vscode.env.openExternal(vscode.Uri.parse(url));
    }
  }
}

/**
 * Resolve a `Repository` for a command invoked from the palette with no file/URI argument,
 * by listing every repository currently known to the cache and prompting via quick pick
 * if there's more than one.
 */
async function resolveRepoForWorktreeCommand(): Promise<Repository | undefined> {
  const repos = repositoryCache.getAllRepositories();

  if (repos.length === 0) {
    vscode.window.showErrorMessage(t('No Sapling repository found in the current workspace'));
    return undefined;
  }
  if (repos.length === 1) {
    return repos[0];
  }

  const picked = await vscode.window.showQuickPick(
    repos.map(repo => ({
      label: path.basename(repo.info.repoRoot),
      description: repo.info.repoRoot,
      repo,
    })),
    {placeHolder: t('Select a repository')},
  );
  return picked?.repo;
}

/**
 * Resolve a `Repository` and its `WorktreeInfo` for a worktree palette command, checking
 * that worktrees are supported/available for the resolved repo and showing an error
 * message otherwise.
 */
async function resolveWorktreeRepo(): Promise<
  {repo: Repository; worktreeInfo: WorktreeInfo} | undefined
> {
  const repo = await resolveRepoForWorktreeCommand();
  if (repo == null) {
    return undefined;
  }
  if (repo.info.isEdenFs !== true) {
    vscode.window.showErrorMessage(
      t('Worktrees require EdenFS. This repository is not backed by EdenFS'),
    );
    return undefined;
  }
  if (repo.info.codeReviewSystem.type === 'github') {
    vscode.window.showErrorMessage(t('Worktrees are not supported for GitHub repositories'));
    return undefined;
  }
  try {
    await repo.refreshWorktreeInfo();
  } catch (err) {
    repo.initialConnectionContext.logger.error('Failed to refresh worktree info:', err);
    vscode.window.showErrorMessage(t('Failed to refresh worktree info'));
    return undefined;
  }
  const worktreeInfo = repo.getWorktreeInfo();
  if (worktreeInfo == null) {
    vscode.window.showErrorMessage(t('Worktrees are not available for this repository'));
    return undefined;
  }
  return {repo, worktreeInfo};
}

/**
 * Wrap a command implementation so it can be called with any of:
 * - current active file Uri for use from the command palette
 * - a vscode Uri for programmatic invocations
 * - a SourceControlResourceState for use from the VS Code SCM sidebar API
 */
function commandWithUriOrResourceState(
  handler: (
    repo: Repository,
    uri: vscode.Uri,
    path: RepoRelativePath,
  ) => unknown | Thenable<unknown>,
) {
  return function (
    this: RepositoryContext,
    uriOrResource: vscode.Uri | vscode.SourceControlResourceState | undefined,
  ) {
    const uri =
      uriOrResource == null
        ? vscode.window.activeTextEditor?.document.uri
        : uriOrResource instanceof vscode.Uri
          ? uriOrResource
          : uriOrResource.resourceUri;
    if (uri == null) {
      vscode.window.showErrorMessage(t(`No active file found`));
      return;
    }

    const {fsPath} = uri;
    const repo = repositoryCache.cachedRepositoryForPath(fsPath);
    if (repo == null) {
      vscode.window.showErrorMessage(t(`No repository found for file ${fsPath}`));
      return;
    }

    const repoRelativePath = repoRelativePathForAbsolutePath(uri.fsPath, repo);
    return handler.apply(this, [repo, uri, repoRelativePath]);
  };
}
