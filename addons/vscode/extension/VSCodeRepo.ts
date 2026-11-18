/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import type {RepositoryReference} from 'isl-server/src/RepositoryCache';
import type {ServerSideTracker} from 'isl-server/src/analytics/serverSideTracker';
import type {Logger} from 'isl-server/src/logger';
import type {ChangedFile, RepoRelativePath} from 'isl/src/types';
import {GeneratedStatus} from 'isl/src/types';
import type {Comparison} from 'shared/Comparison';
import type {Writable} from 'shared/typeUtils';
import type {
  SaplingChangedFile,
  SaplingCommandOutput,
  SaplingCommitInfo,
  SaplingComparison,
  SaplingConflictContext,
  SaplingCurrentCommitDiff,
  SaplingRepository,
} from './api/types';
import type {EnabledSCMApiFeature} from './types';

import {generatedFilesDetector} from 'isl-server/src/GeneratedFiles';
import {Repository} from 'isl-server/src/Repository';
import {repositoryCache} from 'isl-server/src/RepositoryCache';
import type {TrackEventName} from 'isl-server/src/analytics/eventNames';
import {getMainFetchTemplate, parseCommitInfoOutput} from 'isl-server/src/templates';
import {ResolveOperation, ResolveTool} from 'isl/src/operations/ResolveOperation';
import {diffCurrentCommit} from 'isl/src/stackEdit/diffSplit';
import type {DiffCommit} from 'isl/src/stackEdit/diffSplitTypes';
import * as path from 'path';
import {beforeRevsetForComparison, ComparisonType} from 'shared/Comparison';
import {filterFilesFromPatch, parsePatch} from 'shared/patch/parse';
import {notEmpty} from 'shared/utils';
import * as vscode from 'vscode';
import {encodeSaplingDiffUri} from './DiffContentProvider';
import SaplingFileDecorationProvider from './SaplingFileDecorationProvider';
import {executeVSCodeCommand} from './commands';
import {getCLICommand} from './config';
import {t} from './i18n';

const mergeConflictStartRegex = new RegExp('<{7}|>{7}|[|]{7}');

export class VSCodeReposList {
  private knownRepos = new Map</* attached folder root */ string, RepositoryReference>();
  private vscodeRepos = new Map</* repo root path */ string, VSCodeRepo>();
  private disposables: Array<vscode.Disposable> = [];

  private reposByPath = new Map</* arbitrary subpath of repo */ string, VSCodeRepo>();

  constructor(
    private logger: Logger,
    private tracker: ServerSideTracker,
    private enabledFeatures: Set<EnabledSCMApiFeature>,
  ) {
    if (vscode.workspace.workspaceFolders) {
      this.updateRepos(vscode.workspace.workspaceFolders, []);
    }
    this.disposables.push(
      vscode.workspace.onDidChangeWorkspaceFolders(e => {
        this.updateRepos(e.added, e.removed);
      }),
    );
    // TODO: consider also listening for vscode.workspace.onDidOpenTextDocument to support repos
    // for ad-hoc non-workspace-folder files
  }

  private updateRepos(
    added: ReadonlyArray<vscode.WorkspaceFolder>,
    removed: ReadonlyArray<vscode.WorkspaceFolder>,
  ) {
    for (const add of added) {
      const {fsPath} = add.uri;
      if (this.knownRepos.has(fsPath)) {
        throw new Error(`Attempted to add workspace folder path twice: ${fsPath}`);
      }
      const repoReference = repositoryCache.getOrCreate({
        cwd: fsPath,
        cmd: getCLICommand(),
        logger: this.logger,
        tracker: this.tracker,
        // A workspace folder is not a conversation, so nothing here names one.
        agentSessionMetadata: undefined,
      });
      this.knownRepos.set(fsPath, repoReference);
      repoReference.promise.then(repo => {
        if (repo instanceof Repository) {
          const root = repo?.info.repoRoot;
          const existing = this.vscodeRepos.get(root);
          if (existing) {
            return;
          }
          const vscodeRepo = new VSCodeRepo(repo, this.logger, this.enabledFeatures);
          this.vscodeRepos.set(root, vscodeRepo);
          repo.onDidDispose(() => {
            vscodeRepo.dispose();
            this.vscodeRepos.delete(root);
            this.emitActiveRepos();
          });

          this.emitActiveRepos();
        }
      });
    }
    for (const remove of removed) {
      const {fsPath} = remove.uri;
      const repo = this.knownRepos.get(fsPath);
      repo?.unref();
      this.knownRepos.delete(fsPath);
    }

    executeVSCodeCommand('setContext', 'sapling:hasRepo', this.knownRepos.size > 0);

    Promise.all(Array.from(this.knownRepos.values()).map(repo => repo.promise)).then(repos => {
      const hasRemoteLinkRepo = repos.some(
        repo => repo instanceof Repository && repo.codeReviewProvider?.getRemoteFileURL,
      );
      executeVSCodeCommand('setContext', 'sapling:hasRemoteLinkRepo', hasRemoteLinkRepo);
    });
  }

  /** return the VSCodeRepo that contains the given path */
  public repoForPath(path: string): VSCodeRepo | undefined {
    if (this.reposByPath.has(path)) {
      return this.reposByPath.get(path);
    }
    for (const value of this.vscodeRepos.values()) {
      if (path.startsWith(value.rootPath)) {
        return value;
      }
    }
    return undefined;
  }

  public repoForPhabricatorCallsign(callsign: string): VSCodeRepo | undefined {
    for (const repo of this.vscodeRepos.values()) {
      const system = repo.repo.info.codeReviewSystem;
      if (system.type === 'phabricator' && system.callsign === callsign) {
        return repo;
      }
    }
    return undefined;
  }

  private emitActiveRepos() {
    for (const cb of this.updateCallbacks) {
      cb(Array.from(this.vscodeRepos.values()));
    }
  }

  private updateCallbacks: Array<(repos: Array<VSCodeRepo>) => void> = [];
  /** Subscribe to the list of active repositories */
  public observeActiveRepos(cb: (repos: Array<VSCodeRepo>) => void): vscode.Disposable {
    this.updateCallbacks.push(cb);
    return {
      dispose: () => {
        this.updateCallbacks = this.updateCallbacks.filter(c => c !== cb);
      },
    };
  }

  /**
   * Run a callback on each repository once, when it's added (or immediately if it already exists).
   * The callback is given the VSCodeRepo, so you can subscribe to various events within the repo.
   */
  public subscribeWithinAllRepos(cb: (repo: VSCodeRepo) => vscode.Disposable): vscode.Disposable {
    const perRepoDisposables = new Map<VSCodeRepo, vscode.Disposable>();

    const trySubscribe = (repo: VSCodeRepo) => {
      if (!perRepoDisposables.has(repo)) {
        perRepoDisposables.set(repo, cb(repo));
      }
    };

    for (const repo of this.vscodeRepos.values()) {
      trySubscribe(repo);
    }

    const repoObserver = this.observeActiveRepos(repos => {
      const activeSet = new Set(repos);

      for (const repo of repos) {
        trySubscribe(repo);
      }

      for (const [repo, disposable] of perRepoDisposables) {
        if (!activeSet.has(repo)) {
          disposable.dispose();
          perRepoDisposables.delete(repo);
        }
      }
    });

    return new vscode.Disposable(() => {
      repoObserver.dispose();
      for (const disposable of perRepoDisposables.values()) {
        disposable.dispose();
      }
      perRepoDisposables.clear();
    });
  }

  public getCurrentActiveRepos(): Array<VSCodeRepo> {
    return Array.from(this.vscodeRepos.values());
  }

  public dispose() {
    for (const disposable of this.disposables) {
      disposable.dispose();
    }
    for (const repo of this.knownRepos.values()) {
      repo.unref();
    }
    this.knownRepos.clear();
  }
}

type SaplingResourceState = vscode.SourceControlResourceState & {
  status?: string;
};
export type SaplingResourceGroup = vscode.SourceControlResourceGroup & {
  resourceStates: SaplingResourceState[];
};
/**
 * vscode-API-compatible repository.
 * This handles vscode-api integrations, but defers to Repository for any actual work.
 */
export class VSCodeRepo implements vscode.QuickDiffProvider, SaplingRepository {
  private disposables: Array<vscode.Disposable> = [];
  private sourceControl?: vscode.SourceControl;
  private resourceGroups?: Record<
    'changes' | 'untracked' | 'unresolved' | 'resolved',
    SaplingResourceGroup
  >;
  public rootUri: vscode.Uri;
  public rootPath: string;

  constructor(
    public repo: Repository,
    private logger: Logger,
    private enabledFeatures: Set<EnabledSCMApiFeature>,
  ) {
    repo.onDidDispose(() => this.dispose());
    this.rootUri = vscode.Uri.file(repo.info.repoRoot);
    this.rootPath = repo.info.repoRoot;

    this.autoResolveFilesOnSave();

    if (!this.enabledFeatures.has('sidebar')) {
      // if sidebar is not enabled, VSCodeRepo is mostly useless, but still used for checking which paths can be used for ISL and blame.
      return;
    }

    this.sourceControl = vscode.scm.createSourceControl(
      'sapling',
      t('Sapling'),
      vscode.Uri.file(repo.info.repoRoot),
    );
    this.sourceControl.quickDiffProvider = this;
    this.sourceControl.inputBox.enabled = false;
    this.sourceControl.inputBox.visible = false;
    this.resourceGroups = {
      changes: this.sourceControl.createResourceGroup('changes', t('Uncommitted Changes')),
      untracked: this.sourceControl.createResourceGroup('untracked', t('Untracked Changes')),
      unresolved: this.sourceControl.createResourceGroup(
        'unresolved',
        t('Unresolved Merge Conflicts'),
      ),
      resolved: this.sourceControl.createResourceGroup('resolved', t('Resolved Merge Conflicts')),
    };
    for (const group of Object.values(this.resourceGroups)) {
      group.hideWhenEmpty = true;
    }

    const fileDecorationProvider = new SaplingFileDecorationProvider(this, logger);
    this.disposables.push(
      repo.subscribeToUncommittedChanges(() => {
        this.updateResourceGroups();
      }),
      repo.onChangeConflictState(() => {
        this.updateResourceGroups();
      }),
      fileDecorationProvider,
    );
    this.updateResourceGroups();
  }

  /** If this uri is for file inside the repo or not */
  public containsUri(uri: vscode.Uri): boolean {
    return (
      uri.scheme === this.rootUri.scheme &&
      uri.authority === this.rootUri.authority &&
      uri.fsPath.startsWith(this.rootPath)
    );
  }

  /** If this uri is for a file inside the repo, return the repo-relative path. Otherwise, return undefined.  */
  public repoRelativeFsPath(uri: vscode.Uri): string | undefined {
    return this.containsUri(uri) ? path.relative(this.rootPath, uri.fsPath) : undefined;
  }

  private autoResolveFilesOnSave(): vscode.Disposable {
    return vscode.workspace.onDidSaveTextDocument(document => {
      const repoRelativePath = this.repoRelativeFsPath(document.uri);
      const conflicts = this.repo.getMergeConflicts();
      if (conflicts == null || repoRelativePath == null) {
        return;
      }
      const filesWithConflicts = conflicts.files?.map(file => file.path);
      if (filesWithConflicts?.includes(repoRelativePath) !== true) {
        return;
      }
      const autoResolveEnabled = vscode.workspace
        .getConfiguration('sapling')
        .get<boolean>('markConflictingFilesResolvedOnSave');
      if (!autoResolveEnabled) {
        return;
      }
      const allConflictsThisFileResolved = !mergeConflictStartRegex.test(document.getText());
      if (!allConflictsThisFileResolved) {
        return;
      }
      this.logger.info(
        'auto marking file with no remaining conflicts as resolved:',
        repoRelativePath,
      );

      this.repo.runOrQueueOperation(
        this.repo.initialConnectionContext,
        {
          ...new ResolveOperation(repoRelativePath, ResolveTool.mark).getRunnableOperation(),
          // Distinguish in analytics from manually resolving
          trackEventName: 'AutoMarkResolvedOperation',
        },
        () => null,
      );
    });
  }

  private updateResourceGroups() {
    if (this.resourceGroups == null || this.sourceControl == null) {
      return;
    }
    const data = this.repo.getUncommittedChanges();
    const conflicts = this.repo.getMergeConflicts()?.files;

    // only show merge conflicts if they are given
    const fileChanges = conflicts ?? data?.files?.value ?? [];

    const changes: Array<SaplingResourceState> = [];
    const untracked: Array<SaplingResourceState> = [];
    const unresolved: Array<SaplingResourceState> = [];
    const resolved: Array<SaplingResourceState> = [];

    for (const change of fileChanges) {
      const uri = vscode.Uri.joinPath(this.rootUri, change.path);
      const resource: SaplingResourceState = {
        command: {
          command: 'vscode.open',
          title: 'Open',
          arguments: [uri],
        },
        resourceUri: uri,
        decorations: this.decorationForChange(change),
        status: change.status,
      };
      switch (change.status) {
        case '?':
        case '!':
          untracked.push(resource);
          break;
        case 'U':
          unresolved.push(resource);
          break;
        case 'Resolved':
          resolved.push(resource);
          break;
        default:
          changes.push(resource);
          break;
      }
    }
    this.resourceGroups.changes.resourceStates = changes;
    this.resourceGroups.untracked.resourceStates = untracked;
    this.resourceGroups.unresolved.resourceStates = unresolved;
    this.resourceGroups.resolved.resourceStates = resolved;

    // don't include resolved files in count
    this.sourceControl.count = changes.length + untracked.length + unresolved.length;
  }

  public getResourceGroups() {
    return this.resourceGroups;
  }

  public dispose() {
    this.disposables.forEach(d => d?.dispose());
  }

  private decorationForChange(change: ChangedFile): vscode.SourceControlResourceDecorations {
    const decoration: Writable<vscode.SourceControlResourceDecorations> = {};
    switch (change.status) {
      case 'M':
        decoration.iconPath = new vscode.ThemeIcon('diff-modified', themeColors.modified);
        break;
      case 'A':
        decoration.iconPath = new vscode.ThemeIcon('diff-added', themeColors.added);
        break;
      case 'R':
        decoration.iconPath = new vscode.ThemeIcon('diff-removed', themeColors.deleted);
        break;
      case '?':
        decoration.faded = true;
        decoration.iconPath = new vscode.ThemeIcon('question', themeColors.untracked);
        break;
      case '!':
        decoration.faded = true;
        decoration.iconPath = new vscode.ThemeIcon('warning', themeColors.untracked);
        break;
      case 'U':
        decoration.iconPath = new vscode.ThemeIcon('diff-ignored', themeColors.conflicting);
        break;
      case 'Resolved':
        decoration.faded = true;
        decoration.iconPath = new vscode.ThemeIcon('pass', themeColors.added);
        break;
      default:
        break;
    }
    return decoration;
  }

  /**
   * Use ContentProvider + encodeSaplingDiffUri
   */
  provideOriginalResource(uri: vscode.Uri): vscode.Uri | undefined {
    if (uri.scheme !== 'file') {
      return;
    }
    // TODO: make this configurable via vscode setting to allow
    // diff gutters to be either uncommitted changes / head changes / stack changes
    const comparison = {type: ComparisonType.UncommittedChanges} as Comparison;

    return encodeSaplingDiffUri(uri, beforeRevsetForComparison(comparison));
  }

  ////////////////////////////////////////////////////////////////////////////////////

  get info() {
    return this.repo.info;
  }

  getDotCommit(): SaplingCommitInfo | undefined {
    return this.repo.getHeadCommit();
  }
  onChangeDotCommit(callback: (commit: SaplingCommitInfo | undefined) => void): vscode.Disposable {
    return this.repo.subscribeToHeadCommit(callback);
  }
  getUncommittedChanges(): ReadonlyArray<SaplingChangedFile> {
    return this.repo.getUncommittedChanges()?.files?.value ?? [];
  }
  onChangeUncommittedChanges(
    callback: (changes: ReadonlyArray<SaplingChangedFile>) => void,
  ): vscode.Disposable {
    return this.repo.subscribeToUncommittedChanges(result => {
      callback(result.files?.value ?? []);
    });
  }

  runSlCommand(
    args: Array<string>,
    eventName: TrackEventName | undefined = undefined,
  ): Promise<SaplingCommandOutput> {
    return this.repo.runCommand(args, eventName, this.repo.initialConnectionContext);
  }

  async getCurrentStack(): Promise<ReadonlyArray<SaplingCommitInfo>> {
    const revset = 'sort(draft() and ancestors(.), topo)';
    const result = await this.runSlCommand(
      ['log', '--rev', revset, '--template', getMainFetchTemplate(this.info.codeReviewSystem)],
      'GetCurrentStack',
    );
    if (result.exitCode === 0) {
      return parseCommitInfoOutput(this.logger, result.stdout, this.repo.info.codeReviewSystem);
    } else {
      throw new Error(result.stderr);
    }
  }

  async getFullFocusedBranch(): Promise<ReadonlyArray<SaplingCommitInfo>> {
    const revset = 'sort(focusedbranch(.), topo)';
    const result = await this.runSlCommand(
      ['log', '--rev', revset, '--template', getMainFetchTemplate(this.info.codeReviewSystem)],
      'GetFullFocusedBranch',
    );
    if (result.exitCode === 0) {
      return parseCommitInfoOutput(this.logger, result.stdout, this.repo.info.codeReviewSystem);
    } else {
      throw new Error(result.stderr);
    }
  }

  /** @deprecated - prefer `diff({type: 'Commit', hash: commit || '.'})` */
  async getDiff(commit?: string): Promise<string> {
    const result = await this.runSlCommand(['diff', '-c', commit || '.']);

    if (result.exitCode === 0) {
      return result.stdout;
    } else {
      throw new Error(result.stderr);
    }
  }

  async diff(
    comparison: Comparison | SaplingComparison,
    options?: {excludeGenerated?: boolean},
  ): Promise<string> {
    const output = await this.repo.runDiff(
      this.repo.initialConnectionContext,
      comparison as Comparison,
      undefined,
    );

    if (options?.excludeGenerated === true) {
      const filenames = parsePatch(output)
        .map(diff => diff.newFileName ?? diff.oldFileName)
        .filter(notEmpty);
      const generatedFiles = await this.getGeneratedPaths(filenames);
      if (generatedFiles) {
        return filterFilesFromPatch(output, generatedFiles);
      }
    }
    return output;
  }

  async getGeneratedPaths(paths: Array<RepoRelativePath>): Promise<Array<RepoRelativePath>> {
    const generatedMap = await generatedFilesDetector.queryFilesGenerated(
      this.repo,
      this.repo.initialConnectionContext,
      this.repo.info.repoRoot,
      paths,
    );
    return paths.filter(
      path =>
        generatedMap[path] === GeneratedStatus.Generated ||
        generatedMap[path] === GeneratedStatus.PartiallyGenerated,
    );
  }

  async commit(title: string, commitMessage: string): Promise<void> {
    const message = `${title}\n\n${commitMessage}`;
    const result = await this.runSlCommand(['commit', '-m', message]);

    if (result.exitCode !== 0) {
      throw new Error(result.stderr);
    }
  }

  async getMergeConflictContext(): Promise<SaplingConflictContext[]> {
    const result = await this.runSlCommand(['debugconflictcontext']);
    if (result.exitCode !== 0) {
      throw new Error(result.stderr);
    }

    return JSON.parse(result.stdout) as SaplingConflictContext[];
  }

  async getCurrentCommitDiff(): Promise<SaplingCurrentCommitDiff> {
    const diff = (await diffCurrentCommit(
      this.repo,
      this.repo.initialConnectionContext,
    )) as DiffCommit;

    return {message: diff.message, files: diff.files};
  }
}

const themeColors = {
  deleted: new vscode.ThemeColor('gitDecoration.deletedResourceForeground'),
  modified: new vscode.ThemeColor('gitDecoration.modifiedResourceForeground'),
  added: new vscode.ThemeColor('gitDecoration.addedResourceForeground'),
  untracked: new vscode.ThemeColor('gitDecoration.untrackedResourceForeground'),
  conflicting: new vscode.ThemeColor('gitDecoration.conflictingResourceForeground'),
};
