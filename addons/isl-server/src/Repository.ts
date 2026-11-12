/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import type {
  AbsolutePath,
  Alert,
  ChangedFile,
  CodeReviewSystem,
  CommitCloudSyncState,
  CommitInfo,
  ConfigName,
  CwdInfo,
  DiffId,
  Disposable,
  FetchedCommits,
  FetchedUncommittedChanges,
  Hash,
  MergeConflicts,
  OperationCommandProgressReporter,
  OperationProgress,
  PageVisibility,
  PreferredSubmitCommand,
  RepoInfo,
  RepoRelativePath,
  Revset,
  RunnableOperation,
  SettableConfigName,
  ShelvedChange,
  SlocDelta,
  StableInfo,
  Submodule,
  SubmodulesByRoot,
  UncommittedChanges,
  ValidatedRepoInfo,
  WorktreeInfo,
} from 'isl/src/types';
import type {Comparison} from 'shared/Comparison';
import type {EjecaChildProcess, EjecaOptions} from 'shared/ejeca';
import type {CodeReviewProvider} from './CodeReviewProvider';
import type {KindOfChange, PollKind} from './WatchForChanges';
import type {TrackEventName} from './analytics/eventNames';
import type {ConfigLevel, ResolveCommandConflictOutput} from './commands';
import type {RepositoryContext} from './serverTypes';

import {Set as ImSet} from 'immutable';
import {
  CommandRunner,
  CommitCloudBackupStatus,
  allConfigNames,
  settableConfigNames,
} from 'isl/src/types';
import fs from 'node:fs';
import path from 'node:path';
import {revsetArgsForComparison} from 'shared/Comparison';
import {LRU} from 'shared/LRU';
import {RateLimiter} from 'shared/RateLimiter';
import {TypedEventEmitter} from 'shared/TypedEventEmitter';
import {ejeca, simplifyEjecaError} from 'shared/ejeca';
import {exists} from 'shared/fs';
import {removeLeadingPathSep} from 'shared/pathUtils';
import {isHexHash, notEmpty, nullthrows, pathsAreIdentical, randomId} from 'shared/utils';
import {Internal} from './Internal';
import {OperationQueue} from './OperationQueue';
import {PageFocusTracker} from './PageFocusTracker';
import {WatchForChanges} from './WatchForChanges';
import {parseAlerts} from './alerts';
import {
  MAX_SIMULTANEOUS_CAT_CALLS,
  READ_COMMAND_TIMEOUT_MS,
  computeNewConflicts,
  extractRepoInfoFromUrl,
  findDotDir,
  findRoot,
  findRoots,
  findSharedRoot,
  getConfigs,
  getExecParams,
  listWorktrees,
  narrowToSubmoduleChain,
  runCommand,
  setConfig,
  whereami,
} from './commands';
import {DEFAULT_DAYS_OF_COMMITS_TO_LOAD, ErrorShortMessages} from './constants';
import {GitHubCodeReviewProvider} from './github/githubCodeReviewProvider';
import {isGithubEnterprise} from './github/queryGraphQL';
import {
  CHANGED_FILES_FIELDS,
  CHANGED_FILES_INDEX,
  CHANGED_FILES_TEMPLATE,
  COMMIT_END_MARK,
  SHELVE_FETCH_TEMPLATE,
  attachStableLocations,
  getMainFetchTemplate,
  parseCommitInfoOutput,
  parseShelvedCommitsOutput,
} from './templates';
import {
  canonicalizePath,
  findPublicAncestor,
  handleAbortSignalOnProcess,
  isEjecaError,
  serializeAsyncCall,
} from './utils';

const SMARTLOG_TOO_MANY_COMMITS_WARNING =
  /^smartlog: too many \((\d+)\) commits, not rendering all of them$/m;
// Sapling's 1,000-commit default can include extra heads and connecting ancestors.
const MAX_SMARTLOG_COMMITS_TO_RENDER = 1500;

/**
 * This class is responsible for providing information about the working copy
 * for a Sapling repository.
 *
 * A Repository may be reused by multiple connections, not just one ISL window.
 * This is so we don't duplicate watchman subscriptions and calls to status/log.
 * A Repository does not have a pre-defined `cwd`, so it may be reused across cwds.
 *
 * Prefer using `RepositoryCache.getOrCreate()` to access and dispose `Repository`s.
 */
export class Repository {
  public IGNORE_COMMIT_MESSAGE_LINES_REGEX = /^((?:HG|SL):.*)\n?/gm;

  private mergeConflicts: MergeConflicts | undefined = undefined;
  private uncommittedChanges: FetchedUncommittedChanges | null = null;
  private smartlogCommits: FetchedCommits | null = null;
  private submodulesByRoot: SubmodulesByRoot | undefined = undefined;
  private worktreeInfo: WorktreeInfo | undefined = undefined;
  private submodulePathCache: ImSet<RepoRelativePath> | undefined = undefined;

  private mergeConflictsEmitter = new TypedEventEmitter<'change', MergeConflicts | undefined>();
  private uncommittedChangesEmitter = new TypedEventEmitter<'change', FetchedUncommittedChanges>();
  private smartlogCommitsChangesEmitter = new TypedEventEmitter<'change', FetchedCommits>();
  private submodulesChangesEmitter = new TypedEventEmitter<'change', SubmodulesByRoot>();
  private worktreeInfoEmitter = new TypedEventEmitter<'change', WorktreeInfo | undefined>();

  private smartlogCommitsBeginFetchingEmitter = new TypedEventEmitter<'start', undefined>();
  private uncommittedChangesBeginFetchingEmitter = new TypedEventEmitter<'start', undefined>();

  private disposables: Array<() => void> = [
    () => this.mergeConflictsEmitter.removeAllListeners(),
    () => this.uncommittedChangesEmitter.removeAllListeners(),
    () => this.smartlogCommitsChangesEmitter.removeAllListeners(),
    () => this.smartlogCommitsBeginFetchingEmitter.removeAllListeners(),
    () => this.uncommittedChangesBeginFetchingEmitter.removeAllListeners(),
    () => this.worktreeInfoEmitter.removeAllListeners(),
  ];
  public onDidDispose(callback: () => unknown): void {
    this.disposables.push(callback);
  }

  private operationQueue: OperationQueue;
  private watchForChanges: WatchForChanges;
  private pageFocusTracker = new PageFocusTracker();
  public codeReviewProvider?: CodeReviewProvider;

  /**
   * Config: milliseconds to hold off log/status refresh during the start of a command.
   * This is to avoid showing messy indeterminate states (like millions of files changed
   * during a long distance checkout, or commit graph changed but '.' is out of sync).
   *
   * Default: 10 seconds. Can be set by the `isl.hold-off-refresh-ms` setting.
   */
  public configHoldOffRefreshMs = 10000;

  private configRateLimiter = new RateLimiter(1);

  private currentVisibleCommitRangeIndex = 0;
  /** Used to log `SmartlogTooManyCommits` once per occurrence, rather than on every refetch. */
  private isShowingTooManyCommits = false;
  private visibleCommitRanges: Array<number | undefined> = [
    DEFAULT_DAYS_OF_COMMITS_TO_LOAD,
    60,
    undefined,
  ];

  /**
   * Additional commits to include in batched `log` fetch,
   * used for additional remote bookmarks / known stable commit hashes.
   * After fetching commits, stable names will be added to commits in "stableCommitMetadata"
   */
  public stableLocations: Array<StableInfo> = [];

  /**
   * Recommended remote bookmarks to be include in batched `log` fetch.
   * If a bookmark is not in the subscriptions list yet, then it will be pulled explicitly.
   * Undefined means not yet fetched.
   */
  public recommendedBookmarks: Array<string> | undefined;

  /**
   * The context used when the repository was created.
   * This is needed for subscriptions to have access to ANY logger, etc.
   * Avoid using this, and prefer using the correct context for a given connection.
   */
  public initialConnectionContext: RepositoryContext;

  public fullRepoBranchModule = Internal.RepositoryFullRepoBranchModule?.create(
    this,
    this.smartlogCommitsChangesEmitter,
  );

  /**  Prefer using `RepositoryCache.getOrCreate()` to access and dispose `Repository`s. */
  constructor(
    public info: ValidatedRepoInfo,
    ctx: RepositoryContext,
  ) {
    this.initialConnectionContext = ctx;

    const remote = info.codeReviewSystem;
    if (remote.type === 'github') {
      this.codeReviewProvider = new GitHubCodeReviewProvider(remote, ctx.logger);
    }

    if (remote.type === 'phabricator' && Internal?.PhabricatorCodeReviewProvider != null) {
      this.codeReviewProvider = new Internal.PhabricatorCodeReviewProvider(
        remote,
        this.initialConnectionContext,
        this.info.dotdir,
      );
    }

    const shouldWait = (): boolean => {
      const startTime = this.operationQueue.getRunningOperationStartTime();
      if (startTime == null) {
        return false;
      }
      // Prevent auto-refresh during the first 10 seconds of a running command.
      // When a command is running, the intermediate state can be messy:
      // - status errors out (edenfs), is noisy (long distance goto)
      // - commit graph and the `.` are updated separately and hard to predict
      // Let's just rely on optimistic state to provide the "clean" outcome.
      // In case the command takes a long time to run, allow refresh after
      // the time period.
      // Fundamentally, the intermediate states have no choice but have to
      // be messy because filesystems are not transactional (and reading in
      // `sl` is designed to be lock-free).
      const elapsedMs = Date.now() - startTime.valueOf();
      const result = elapsedMs < this.configHoldOffRefreshMs;
      return result;
    };
    const callback = (kind: KindOfChange, pollKind?: PollKind) => {
      if (pollKind !== 'force' && shouldWait()) {
        // Do nothing. This is fine because after the operation
        // there will be a refresh.
        ctx.logger.info('polling prevented from shouldWait');
        return;
      }
      if (kind === 'uncommitted changes') {
        this.fetchUncommittedChanges();
      } else if (kind === 'commits') {
        this.fetchSmartlogCommits();
      } else if (kind === 'merge conflicts') {
        this.checkForMergeConflicts();
      } else if (kind === 'everything') {
        this.fetchUncommittedChanges();
        this.fetchSmartlogCommits();
        this.checkForMergeConflicts();
        this.refreshWorktreeInfo();

        this.codeReviewProvider?.triggerDiffSummariesFetch(
          // We could choose to only fetch the diffs that changed (`newDiffs`) rather than all diffs,
          // but our UI doesn't cache old values, thus all other diffs would appear empty
          this.getAllDiffIds(),
        );
        this.initialConnectionContext.tracker.track('DiffFetchSource', {
          extras: {source: 'watch_for_changes', kind, pollKind},
        });
      }
    };
    this.watchForChanges = new WatchForChanges(info, this.pageFocusTracker, callback, ctx);

    this.operationQueue = new OperationQueue(
      (
        ctx: RepositoryContext,
        operation: RunnableOperation,
        handleCommandProgress,
        signal: AbortSignal,
      ): Promise<unknown> => {
        const {cwd} = ctx;
        if (operation.runner === CommandRunner.Sapling) {
          return this.runOperation(ctx, operation, handleCommandProgress, signal);
        } else if (operation.runner === CommandRunner.CodeReviewProvider) {
          if (this.codeReviewProvider?.runExternalCommand == null) {
            return Promise.reject(
              Error('CodeReviewProvider does not support running external commands'),
            );
          }

          // TODO: support stdin
          return (
            this.codeReviewProvider?.runExternalCommand(
              cwd,
              operation.args,
              handleCommandProgress,
              signal,
            ) ?? Promise.resolve()
          );
        } else if (operation.runner === CommandRunner.Conf) {
          const {args: normalizedArgs} = this.normalizeOperationArgs(ctx, operation);
          if (this.codeReviewProvider?.runConfCommand == null) {
            return Promise.reject(
              Error('CodeReviewProvider does not support running conf commands'),
            );
          }

          return (
            this.codeReviewProvider?.runConfCommand(
              cwd,
              normalizedArgs,
              handleCommandProgress,
              signal,
            ) ?? Promise.resolve()
          );
        } else if (operation.runner === CommandRunner.InternalArcanist) {
          // TODO: support stdin
          const {args: normalizedArgs} = this.normalizeOperationArgs(ctx, operation);
          if (Internal.runArcanistCommand == null) {
            return Promise.reject(Error('InternalArcanist runner is not supported'));
          }
          ctx.logger.info('running arcanist command:', normalizedArgs);
          return Internal.runArcanistCommand(cwd, normalizedArgs, handleCommandProgress, signal);
        }
        return Promise.resolve();
      },
    );

    // refetch summaries whenever we see new diffIds
    const seenDiffs = new Set();
    const subscription = this.subscribeToSmartlogCommitsChanges(fetched => {
      if (fetched.commits.value) {
        const newDiffs = [];
        const diffIds = fetched.commits.value
          .filter(commit => commit.diffId != null)
          .map(commit => commit.diffId);
        for (const diffId of diffIds) {
          if (!seenDiffs.has(diffId)) {
            newDiffs.push(diffId);
            seenDiffs.add(diffId);
          }
        }
        if (newDiffs.length > 0) {
          this.codeReviewProvider?.triggerDiffSummariesFetch(
            // We could choose to only fetch the diffs that changed (`newDiffs`) rather than all diffs,
            // but our UI doesn't cache old values, thus all other diffs would appear empty
            this.getAllDiffIds(),
          );
          this.initialConnectionContext.tracker.track('DiffFetchSource', {
            extras: {source: 'saw_new_diffs'},
          });
        }
      }
    });

    // the repo may already be in a conflict state on startup
    this.checkForMergeConflicts();

    this.disposables.push(() => subscription.dispose());

    this.applyConfigInBackground(ctx);

    const headTracker = this.subscribeToHeadCommit(head => {
      const allCommits = this.getSmartlogCommits();
      const ancestor = findPublicAncestor(allCommits?.commits.value, head);
      this.initialConnectionContext.tracker.track('HeadCommitChanged', {
        extras: {
          hash: head.hash,
          public: ancestor?.hash,
          bookmarks: ancestor?.remoteBookmarks,
        },
      });
    });
    this.disposables.push(headTracker.dispose);

    if (this.fullRepoBranchModule != null) {
      this.disposables.push(() => this.fullRepoBranchModule?.dispose());
    }
  }

  public nextVisibleCommitRangeInDays(): number | undefined {
    if (this.currentVisibleCommitRangeIndex + 1 < this.visibleCommitRanges.length) {
      this.currentVisibleCommitRangeIndex++;
    }
    return this.visibleCommitRanges[this.currentVisibleCommitRangeIndex];
  }

  public isPathInsideRepo(p: AbsolutePath): boolean {
    return path.normalize(p).startsWith(this.info.repoRoot);
  }

  /**
   * Typically, disposing is handled by `RepositoryCache` and not used directly.
   */
  public dispose() {
    this.disposables.forEach(dispose => dispose());
    this.codeReviewProvider?.dispose();
    this.watchForChanges.dispose();
  }

  public onChangeConflictState(
    callback: (conflicts: MergeConflicts | undefined) => unknown,
  ): Disposable {
    this.mergeConflictsEmitter.on('change', callback);

    if (this.mergeConflicts) {
      // if we're already in merge conflicts, let the client know right away
      callback(this.mergeConflicts);
    }

    return {dispose: () => this.mergeConflictsEmitter.off('change', callback)};
  }

  public checkForMergeConflicts = serializeAsyncCall(async () => {
    this.initialConnectionContext.logger.info('checking for merge conflicts');
    // Fast path: check if .sl/merge dir changed
    const wasAlreadyInConflicts = this.mergeConflicts != null;
    if (!wasAlreadyInConflicts) {
      const mergeDirExists = await exists(path.join(this.info.dotdir, 'merge'));
      if (!mergeDirExists) {
        // Not in a conflict
        this.initialConnectionContext.logger.info(
          `conflict state still the same (${
            wasAlreadyInConflicts ? 'IN merge conflict' : 'NOT in conflict'
          })`,
        );
        return;
      }
    }

    if (this.mergeConflicts == null) {
      // notify UI that merge conflicts were detected and full details are loading
      this.mergeConflicts = {state: 'loading'};
      this.mergeConflictsEmitter.emit('change', this.mergeConflicts);
    }

    // More expensive full check for conflicts. Necessary if we see .sl/merge change, or if
    // we're already in a conflict and need to re-check if a conflict was resolved.

    let output: ResolveCommandConflictOutput;
    const fetchStartTimestamp = Date.now();
    try {
      // TODO: is this command fast on large files? it includes full conflicting file contents!
      // `sl resolve --list --all` does not seem to give any way to disambiguate (all conflicts resolved) and (not in merge)
      const proc = await this.runCommand(
        ['resolve', '--tool', 'internal:dumpjson', '--all'],
        'GetConflictsCommand',
        this.initialConnectionContext,
      );
      output = JSON.parse(proc.stdout) as ResolveCommandConflictOutput;
    } catch (err) {
      this.initialConnectionContext.logger.error(`failed to check for merge conflicts: ${err}`);
      // To avoid being stuck in "loading" state forever, let's pretend there's no conflicts.
      this.mergeConflicts = undefined;
      this.mergeConflictsEmitter.emit('change', this.mergeConflicts);
      return;
    }

    this.mergeConflicts = computeNewConflicts(this.mergeConflicts, output, fetchStartTimestamp);
    this.initialConnectionContext.logger.info(
      `repo ${this.mergeConflicts ? 'IS' : 'IS NOT'} in merge conflicts`,
    );
    if (this.mergeConflicts) {
      const maxConflictsToLog = 20;
      const remainingConflicts = (this.mergeConflicts.files ?? [])
        .filter(conflict => conflict.status === 'U')
        .map(conflict => conflict.path)
        .slice(0, maxConflictsToLog);
      this.initialConnectionContext.logger.info(
        'remaining files with conflicts: ',
        remainingConflicts,
      );
    }
    this.mergeConflictsEmitter.emit('change', this.mergeConflicts);

    if (!wasAlreadyInConflicts && this.mergeConflicts) {
      this.initialConnectionContext.tracker.track('EnterMergeConflicts', {
        extras: {numConflicts: this.mergeConflicts.files?.length ?? 0},
      });
    } else if (wasAlreadyInConflicts && !this.mergeConflicts) {
      this.initialConnectionContext.tracker.track('ExitMergeConflicts', {extras: {}});
    }
  });

  public getMergeConflicts(): MergeConflicts | undefined {
    return this.mergeConflicts;
  }

  public async getMergeTool(ctx: RepositoryContext): Promise<string | null> {
    // treat undefined as "not cached", and null as "not configured"/invalid
    if (ctx.cachedMergeTool !== undefined) {
      return ctx.cachedMergeTool;
    }
    const tool = ctx.knownConfigs?.get('ui.merge') ?? 'internal:merge';
    let usesCustomMerge = tool !== 'internal:merge';

    if (usesCustomMerge) {
      // TODO: we could also check merge-tools.${tool}.disabled here
      const customToolUsesGui =
        (
          await this.forceGetConfig(ctx, `merge-tools.${tool}.gui`).catch(() => undefined)
        )?.toLowerCase() === 'true';
      if (!customToolUsesGui) {
        ctx.logger.warn(
          `configured custom merge tool '${tool}' is not a GUI tool, using :merge3 instead`,
        );
        usesCustomMerge = false;
      } else {
        ctx.logger.info(`using configured custom GUI merge tool ${tool}`);
      }
      ctx.tracker.track('UsingExternalMergeTool', {
        extras: {
          tool,
          isValid: usesCustomMerge,
        },
      });
    } else {
      ctx.logger.info(`using default :merge3 merge tool`);
    }

    const mergeTool = usesCustomMerge ? tool : null;
    ctx.cachedMergeTool = mergeTool;
    return mergeTool;
  }

  /**
   * Determine basic repo info including the root and important config values.
   * Resulting RepoInfo may have null fields if cwd is not a valid repo root.
   * Throws if `command` is not found.
   */
  static async getRepoInfo(ctx: RepositoryContext): Promise<RepoInfo> {
    const {cmd, cwd, logger} = ctx;
    const [repoRoot, allRepoRoots, dotdir, configs] = await Promise.all([
      findRoot(ctx).catch((err: Error) => err),
      findRoots(ctx),
      findDotDir(ctx),
      // TODO: This should actually use expanded paths, since the config won't handle custom schemes.
      // However, `sl debugexpandpaths` is currently too slow and impacts startup time.
      getConfigs(ctx, [
        'paths.default',
        'paths.upstream',
        'github.pull_request_domain',
        'github.preferred_submit_command',
        'phrevset.callsign',
      ]),
    ]);
    const pathsDefault = configs.get('paths.default') ?? '';
    const preferredSubmitCommand = configs.get('github.preferred_submit_command');

    if (repoRoot instanceof Error) {
      // first check that the cwd exists
      const cwdExists = await exists(cwd);
      if (!cwdExists) {
        return {type: 'cwdDoesNotExist', cwd};
      }

      return {
        type: 'invalidCommand',
        command: cmd,
        path: process.env.PATH,
      };
    }
    if (repoRoot == null || dotdir == null) {
      // A seemingly invalid repo may just be from EdenFS not running properly
      if (await isUnhealthyEdenFs(cwd)) {
        return {type: 'edenFsUnhealthy', cwd};
      }
      return {type: 'cwdNotARepository', cwd};
    }

    // `debugroots` reports every repo above the cwd, including unrelated ones that happen to
    // contain this checkout, so keep only the genuine submodule chain.
    const repoRoots =
      allRepoRoots != null && allRepoRoots.length > 1
        ? await narrowToSubmoduleChain(ctx, allRepoRoots)
        : allRepoRoots;

    const isEdenFs = await isEdenFsRepo(repoRoot as AbsolutePath);

    let codeReviewSystem: CodeReviewSystem;
    let pullRequestDomain;
    if (Internal.isMononokePath?.(pathsDefault)) {
      // TODO: where should we be getting this from? arcconfig instead? do we need this?
      const repo = pathsDefault.slice(pathsDefault.lastIndexOf('/') + 1);
      codeReviewSystem = {type: 'phabricator', repo, callsign: configs.get('phrevset.callsign')};
    } else if (pathsDefault === '') {
      codeReviewSystem = {type: 'none'};
    } else {
      // A fork pushes to itself but its pull requests belong upstream, which is already how the CLI
      // behaves: `try_find_upstream` in the github extension resolves a bare pull request number
      // against `paths.upstream` before `paths.default`.
      const candidates = [configs.get('paths.upstream') ?? '', pathsDefault]
        .map(url => extractRepoInfoFromUrl(url))
        .filter(info => info != null);
      // A fork and its upstream share a host, so resolve each host once rather than per candidate:
      // `isGithubEnterprise` shells out to `gh`.
      const hostnames = [...new Set(candidates.map(info => info.hostname))];
      const githubHostnames = new Set(
        (
          await Promise.all(
            hostnames.map(async hostname =>
              hostname === 'github.com' || (await isGithubEnterprise(hostname)) ? hostname : null,
            ),
          )
        ).filter(hostname => hostname != null),
      );
      const repoInfo = candidates.find(info => githubHostnames.has(info.hostname));
      if (repoInfo != null) {
        const {owner, repo, hostname} = repoInfo;
        codeReviewSystem = {
          type: 'github',
          owner,
          repo,
          hostname,
        };
      } else {
        codeReviewSystem = {type: 'unknown', path: pathsDefault};
      }
      pullRequestDomain = configs.get('github.pull_request_domain');
    }

    const result: RepoInfo = {
      type: 'success',
      command: cmd,
      dotdir,
      repoRoot,
      repoRoots,
      codeReviewSystem,
      pullRequestDomain,
      preferredSubmitCommand: preferredSubmitCommand as PreferredSubmitCommand | undefined,
      isEdenFs,
    };
    logger.info('repo info: ', result);
    return result;
  }

  /**
   * Determine basic information about a cwd, without fetching the full RepositoryInfo.
   * Useful to determine if a cwd is valid and find the repo root without constructing a Repository.
   */
  static async getCwdInfo(ctx: RepositoryContext): Promise<CwdInfo> {
    const root = await findRoot(ctx).catch((err: Error) => err);

    if (root instanceof Error || root == null) {
      return {cwd: ctx.cwd};
    }

    const [realCwd, realRoot] = await Promise.all([
      canonicalizePath(ctx.cwd),
      canonicalizePath(root),
    ]);
    // Since we found `root` for this particular `cwd`, we expect realpath(root) is a prefix of realpath(cwd).
    // That is, the relative path does not contain any ".." components.
    const repoRelativeCwd = path.relative(realRoot, realCwd);
    return {
      cwd: realCwd,
      repoRoot: realRoot,
      repoRelativeCwdLabel: path.normalize(path.join(path.basename(realRoot), repoRelativeCwd)),
    };
  }

  /**
   * Run long-lived command which mutates the repository state.
   * Progress is streamed back as it comes in.
   * Operations are run immediately. For queueing, see OperationQueue.
   * This promise resolves when the operation exits.
   */
  async runOrQueueOperation(
    ctx: RepositoryContext,
    operation: RunnableOperation,
    onProgress: (progress: OperationProgress) => void,
  ): Promise<'ran' | 'skipped'> {
    const result = await this.operationQueue.runOrQueueOperation(ctx, operation, onProgress);

    if (result !== 'skipped') {
      // After any operation finishes, make sure we poll right away,
      // so the UI is guaranteed to get the latest data.
      this.watchForChanges.poll('force');
    }

    return result;
  }

  /**
   * Abort the running operation if it matches the given id.
   */
  abortRunningOperation(operationId: string) {
    this.operationQueue.abortRunningOperation(operationId);
  }

  /** The currently running operation tracked by the server. */
  getRunningOperation() {
    return this.operationQueue.getRunningOperation();
  }

  private normalizeOperationArgs(
    ctx: RepositoryContext,
    operation: RunnableOperation,
  ): {args: Array<string>; stdin?: string | undefined} {
    const {cwd} = ctx;
    const repoRoot = nullthrows(this.info.repoRoot);
    let stdin = operation.stdin;
    const args = [];
    for (const arg of operation.args) {
      if (typeof arg === 'object') {
        switch (arg.type) {
          case 'config':
            if (!(settableConfigNames as ReadonlyArray<string>).includes(arg.key)) {
              throw new Error(`config ${arg.key} not allowed`);
            }
            args.push('--config', `${arg.key}=${arg.value}`);
            continue;
          case 'repo-relative-file':
            args.push(path.normalize(path.relative(cwd, path.join(repoRoot, arg.path))));
            continue;
          case 'repo-relative-file-list':
            // pass long lists of files as stdin via fileset patterns
            // this is passed as an arg instead of directly in stdin so that we can do path normalization
            args.push('listfile0:-');
            if (stdin != null) {
              throw new Error('stdin already set when using repo-relative-file-list');
            }
            stdin = arg.paths
              .map(p => path.normalize(path.relative(cwd, path.join(repoRoot, p))))
              .join('\0');
            continue;
          case 'exact-revset':
            if (arg.revset.startsWith('-')) {
              // don't allow revsets to be used as flags
              throw new Error('invalid revset');
            }
            args.push(arg.revset);
            continue;
          case 'succeedable-revset':
            args.push(`max(successors(${arg.revset}))`);
            continue;
          case 'optimistic-revset':
            args.push(`max(successors(${arg.revset}))`);
            continue;
        }
      }
      if (isIllegalOperationFlag(arg)) {
        throw new Error(`argument '${arg}' is not allowed`);
      }
      args.push(arg);
    }
    validateMergeToolArgs(args, ctx.knownConfigs?.get('ui.merge'));
    return {args, stdin};
  }

  private async operationIPC(
    ctx: RepositoryContext,
    onProgress: OperationCommandProgressReporter,
    child: EjecaChildProcess,
    options: EjecaOptions,
  ): Promise<void> {
    if (!options.ipc) {
      return;
    }

    interface IpcProgressBar {
      id: number;
      topic: string;
      unit: string;
      total: number;
      position: number;
      parent_id?: number;
    }

    while (true) {
      try {
        // eslint-disable-next-line no-await-in-loop
        const message = await child.getOneMessage();
        if (message === null || typeof message !== 'object') {
          break;
        }
        if ('progress_bar_update' in message) {
          const bars = message.progress_bar_update as IpcProgressBar[];
          const blen = bars.length;
          if (blen > 0) {
            const msg = bars[blen - 1];
            onProgress('progress', {
              message: msg.topic,
              progress: msg.position,
              progressTotal: msg.total,
              unit: msg.unit,
            });
          }
        } else if ('warning' in message) {
          onProgress('warning', message.warning as string);
        } else {
          break;
        }
      } catch (_err) {
        break;
      }
    }
  }

  /**
   * Called by this.operationQueue in response to runOrQueueOperation when an operation is ready to actually run.
   */
  private async runOperation(
    ctx: RepositoryContext,
    operation: RunnableOperation,
    onProgress: OperationCommandProgressReporter,
    signal: AbortSignal,
  ): Promise<void> {
    const {cwd} = ctx;
    const {args: cwdRelativeArgs, stdin} = this.normalizeOperationArgs(ctx, operation);

    const env = await Promise.all([
      Internal.additionalEnvForCommand?.(operation),
      this.getMergeToolEnvVars(ctx),
    ]);

    const ipc = (ctx.knownConfigs?.get('isl.sl-progress-enabled') ?? 'false') === 'true';
    const fullArgs = [...cwdRelativeArgs];
    if (ctx.debug) {
      fullArgs.unshift('--debug');
    }
    if (ctx.verbose) {
      fullArgs.unshift('--verbose');
    }
    const {command, args, options} = getExecParams(
      this.info.command,
      fullArgs,
      cwd,
      stdin ? {input: stdin, ipc} : {ipc},
      {
        ...env[0],
        ...env[1],
      },
    );

    ctx.logger.info('run operation: ', command, fullArgs.join(' '));

    const commandBlocklist = new Set([
      'debugshell',
      'dbsh',
      'debugsh',
      'debugpython',
      'debugpy',
      'debugrunshell',
    ]);
    if (args.some(arg => commandBlocklist.has(arg))) {
      throw new Error(`command "${args.join(' ')}" is not allowed`);
    }

    const execution = ejeca(command, args, options);
    // It would be more appropriate to call this in response to execution.on('spawn'), but
    // this seems to be inconsistent about firing in all versions of node.
    // Just send spawn immediately. Errors during spawn like ENOENT will still be reported by `exit`.
    onProgress('spawn');
    execution.stdout?.on('data', data => {
      onProgress('stdout', data.toString());
    });
    execution.stderr?.on('data', data => {
      onProgress('stderr', data.toString());
    });
    signal.addEventListener('abort', () => {
      ctx.logger.info('kill operation: ', command, fullArgs.join(' '));
    });
    handleAbortSignalOnProcess(execution, signal);
    try {
      this.operationIPC(ctx, onProgress, execution, options);
      const result = await execution;
      onProgress('exit', result.exitCode || 0);
    } catch (err) {
      onProgress('exit', isEjecaError(err) ? err.exitCode : -1);
      throw err;
    }
  }

  /**
   * Get environment variables to set up which merge tool to use during an operation.
   * If you're using the default merge tool, use :merge3 instead for slightly better merge information.
   * If you've configured a custom merge tool, make sure we don't overwrite it...
   * ...unless the custom merge tool is *not* a GUI tool, like vimdiff, which would not be interactable in ISL.
   */
  async getMergeToolEnvVars(ctx: RepositoryContext): Promise<Record<string, string> | undefined> {
    const tool = await this.getMergeTool(ctx);
    return tool != null
      ? // allow sl to use the already configured merge tool
        {}
      : // otherwise, use 3-way merge
        {
          HGMERGE: ':merge3',
          SL_MERGE: ':merge3',
        };
  }

  setPageFocus(page: string, state: PageVisibility) {
    this.pageFocusTracker.setState(page, state);
    this.initialConnectionContext.tracker.track('FocusChanged', {extras: {state}});
  }

  private refcount = 0;
  ref() {
    this.refcount++;
    if (this.refcount === 1) {
      this.watchForChanges.setupSubscriptions();
    }
  }
  unref() {
    this.refcount--;
    if (this.refcount === 0) {
      this.watchForChanges.disposeWatchmanSubscriptions();
    }
  }

  /** Return the latest fetched value for UncommittedChanges. */
  getUncommittedChanges(): FetchedUncommittedChanges | null {
    return this.uncommittedChanges;
  }

  subscribeToUncommittedChanges(
    callback: (result: FetchedUncommittedChanges) => unknown,
  ): Disposable {
    this.uncommittedChangesEmitter.on('change', callback);
    return {
      dispose: () => {
        this.uncommittedChangesEmitter.off('change', callback);
      },
    };
  }

  fetchUncommittedChanges = serializeAsyncCall(async () => {
    const fetchStartTimestamp = Date.now();
    try {
      this.uncommittedChangesBeginFetchingEmitter.emit('start');
      // Note `status -tjson` run with PLAIN are repo-relative
      const proc = await this.runCommand(
        ['status', '-Tjson', '--copies'],
        'StatusCommand',
        this.initialConnectionContext,
      );
      const files = (JSON.parse(proc.stdout) as UncommittedChanges).map(change => ({
        ...change,
        path: removeLeadingPathSep(change.path),
      }));

      this.uncommittedChanges = {
        fetchStartTimestamp,
        fetchCompletedTimestamp: Date.now(),
        files: {value: files},
      };
      this.uncommittedChangesEmitter.emit('change', this.uncommittedChanges);
    } catch (err) {
      let error = err;
      if (isEjecaError(error)) {
        if (error.stderr.includes('checkout is currently in progress')) {
          this.initialConnectionContext.logger.info(
            'Ignoring `sl status` error caused by in-progress checkout',
          );
          return;
        }
      }

      this.initialConnectionContext.logger.error('Error fetching files: ', error);
      if (isEjecaError(error)) {
        error = simplifyEjecaError(error);
      }

      // emit an error, but don't save it to this.uncommittedChanges
      this.uncommittedChangesEmitter.emit('change', {
        fetchStartTimestamp,
        fetchCompletedTimestamp: Date.now(),
        files: {error: error instanceof Error ? error : new Error(error as string)},
      });
    }
  });

  getWorktreeInfo(): WorktreeInfo | undefined {
    return this.worktreeInfo;
  }

  /** Hashes checked out (`.`) in sibling worktrees, excluding this worktree, deduped. */
  getOtherWorktreeDotHashes(): Hash[] {
    const repoRoot = this.info.repoRoot;
    const hashes = (this.worktreeInfo?.worktrees ?? []).flatMap(entry => {
      if (pathsAreIdentical(entry.path, repoRoot) || entry.node == null) {
        return [];
      }
      const trimmed = entry.node.trim();
      if (trimmed === '' || !isHexHash(trimmed)) {
        return [];
      }
      return [trimmed as Hash];
    });
    return [...new Set(hashes)];
  }

  subscribeToWorktreeInfoChanges(
    callback: (result: WorktreeInfo | undefined) => unknown,
  ): Disposable {
    this.worktreeInfoEmitter.on('change', callback);
    return {
      dispose: () => {
        this.worktreeInfoEmitter.off('change', callback);
      },
    };
  }

  refreshWorktreeInfo = serializeAsyncCall(async () => {
    try {
      const ctx = this.initialConnectionContext;
      const [sharedRoot, worktrees] = await Promise.all([findSharedRoot(ctx), listWorktrees(ctx)]);
      const repoRoot = this.info.repoRoot;
      const worktreeEntries =
        worktrees.length > 0 ? worktrees : [{path: repoRoot, role: 'main' as const}];
      // Fetch the checked-out hash for sibling worktrees only; our own is already
      // tracked via `.` in the smartlog revset.
      const worktreeEntriesWithNodes = await Promise.all(
        worktreeEntries.map(async entry => {
          if (pathsAreIdentical(entry.path, repoRoot)) {
            return entry;
          }
          const node = await whereami(ctx, entry.path);
          return node == null ? entry : {...entry, node};
        }),
      );
      const worktreeInfo: WorktreeInfo | undefined =
        sharedRoot != null
          ? {
              sharedRoot,
              worktrees: worktreeEntriesWithNodes,
              supportsMutation: this.info.isEdenFs,
            }
          : undefined;
      this.worktreeInfo = worktreeInfo;
      this.worktreeInfoEmitter.emit('change', worktreeInfo);
    } catch (err) {
      this.initialConnectionContext.logger.error('Failed to refresh worktree info:', err);
    }
  });

  /** Return the latest fetched value for SmartlogCommits. */
  getSmartlogCommits(): FetchedCommits | null {
    return this.smartlogCommits;
  }

  subscribeToSmartlogCommitsChanges(callback: (result: FetchedCommits) => unknown) {
    this.smartlogCommitsChangesEmitter.on('change', callback);
    return {
      dispose: () => {
        this.smartlogCommitsChangesEmitter.off('change', callback);
      },
    };
  }

  subscribeToSmartlogCommitsBeginFetching(callback: (isFetching: boolean) => unknown) {
    const onStart = () => callback(true);
    this.smartlogCommitsBeginFetchingEmitter.on('start', onStart);
    return {
      dispose: () => {
        this.smartlogCommitsBeginFetchingEmitter.off('start', onStart);
      },
    };
  }

  subscribeToUncommittedChangesBeginFetching(callback: (isFetching: boolean) => unknown) {
    const onStart = () => callback(true);
    this.uncommittedChangesBeginFetchingEmitter.on('start', onStart);
    return {
      dispose: () => {
        this.uncommittedChangesBeginFetchingEmitter.off('start', onStart);
      },
    };
  }

  subscribeToSubmodulesChanges(callback: (result: SubmodulesByRoot) => unknown) {
    this.submodulesChangesEmitter.on('change', callback);
    return {
      dispose: () => {
        this.submodulesChangesEmitter.off('change', callback);
      },
    };
  }

  fetchSmartlogCommits = serializeAsyncCall(async () => {
    const fetchStartTimestamp = Date.now();
    try {
      this.smartlogCommitsBeginFetchingEmitter.emit('start');

      const visibleCommitDayRange = this.visibleCommitRanges[this.currentVisibleCommitRangeIndex];

      const primaryRevset = '(interestingbookmarks() + heads(draft()))';

      // Revset to fetch for commits, e.g.:
      // smartlog(interestingbookmarks() + heads(draft()) + .)
      // smartlog((interestingbookmarks() + heads(draft()) & date(-14)) + .)
      // smartlog((interestingbookmarks() + heads(draft()) & date(-14)) + . + present(a1b2c3d4))
      const revset = `smartlog(${[
        !visibleCommitDayRange
          ? primaryRevset
          : // filter default smartlog query by date range
            `(${primaryRevset} & date(-${visibleCommitDayRange}))`,
        '.', // always include wdir parent
        // stable locations hashes may be newer than the repo has, wrap in `present()` to only include if available.
        ...this.stableLocations.map(location => `present(${location.hash})`),
        ...(this.recommendedBookmarks ?? []).map(bookmark => `present(${bookmark})`),
        ...(this.fullRepoBranchModule?.genRevset() ?? []),
        // sibling worktrees' checkouts happen outside this process's file watcher,
        // so they may not otherwise be "interesting"; include them explicitly.
        ...this.getOtherWorktreeDotHashes().map(hash => `present(${hash})`),
      ]
        .filter(notEmpty)
        .join(' + ')})`;

      const template = getMainFetchTemplate(this.info.codeReviewSystem);

      const proc = await this.runCommand(
        ['log', '--template', template, '--rev', revset],
        'LogCommand',
        this.initialConnectionContext,
      );
      const tooManyCommitsMatch = SMARTLOG_TOO_MANY_COMMITS_WARNING.exec(proc.stderr);
      let commits = parseCommitInfoOutput(
        this.initialConnectionContext.logger,
        proc.stdout.trim(),
        this.info.codeReviewSystem,
      );
      // This describes the requested graph; a focused retry still omits its other stacks.
      const requestedGraphLimitWarning = tooManyCommitsMatch
        ? {numCommits: parseInt(tooManyCommitsMatch[1], 10)}
        : commits.length > MAX_SMARTLOG_COMMITS_TO_RENDER
          ? {numCommits: commits.length}
          : undefined;
      if (requestedGraphLimitWarning != null) {
        if (!this.isShowingTooManyCommits) {
          this.isShowingTooManyCommits = true;
          this.initialConnectionContext.tracker.track('SmartlogTooManyCommits', {
            extras: {
              numCommits: requestedGraphLimitWarning.numCommits,
              visibleCommitDayRange: visibleCommitDayRange ?? null,
            },
          });
        }
      } else {
        this.isShowingTooManyCommits = false;
      }
      if (commits.length > MAX_SMARTLOG_COMMITS_TO_RENDER) {
        // Smartlog truncation retains every requested head, so many stacks can exceed its limit.
        // Refetch the current stack rather than slicing away parents or the checkout.
        const focusedProc = await this.runCommand(
          ['log', '--template', template, '--rev', 'smartlog(.)'],
          'LogCommand',
          this.initialConnectionContext,
        );
        commits = parseCommitInfoOutput(
          this.initialConnectionContext.logger,
          focusedProc.stdout.trim(),
          this.info.codeReviewSystem,
        );
      }
      if (commits.length === 0) {
        throw new Error(
          requestedGraphLimitWarning
            ? ErrorShortMessages.TooManyCommits
            : ErrorShortMessages.NoCommitsFetched,
        );
      }
      if (commits.length > MAX_SMARTLOG_COMMITS_TO_RENDER) {
        throw new Error(ErrorShortMessages.TooManyCommits);
      }

      attachStableLocations(commits, this.stableLocations);
      if (this.fullRepoBranchModule) {
        this.fullRepoBranchModule.populateSmartlogCommits(commits);
      }

      this.smartlogCommits = {
        fetchStartTimestamp,
        fetchCompletedTimestamp: Date.now(),
        commits: {value: commits},
        tooManyCommits: requestedGraphLimitWarning,
      };
      this.smartlogCommitsChangesEmitter.emit('change', this.smartlogCommits);
    } catch (err) {
      let error = err;
      const internalError = Internal.checkInternalError?.(err);
      if (internalError) {
        error = internalError;
      }
      if (isEjecaError(error) && error.stderr.includes('Please check your internet connection')) {
        error = Error('Network request failed. Please check your internet connection.');
      }

      this.initialConnectionContext.logger.error('Error fetching commits: ', error);
      if (isEjecaError(error)) {
        error = simplifyEjecaError(error);
      }

      this.smartlogCommitsChangesEmitter.emit('change', {
        fetchStartTimestamp,
        fetchCompletedTimestamp: Date.now(),
        commits: {error: error instanceof Error ? error : new Error(error as string)},
      });
    }
  });

  public async fetchAndSetRecommendedBookmarks(onFetched?: (bookmarks: Array<string>) => void) {
    if (!Internal.getRecommendedBookmarks) {
      return;
    }

    // Recommended bookmarks (e.g., fbcode/warm_opt) are specific to fbsource.
    // Skip for other repos to avoid pulling bookmarks that don't exist on the remote.
    if (!this.info.repoRoot.includes('fbsource')) {
      onFetched?.([]);
      return;
    }

    try {
      const bookmarks = await Internal.getRecommendedBookmarks(this.initialConnectionContext);
      onFetched?.((this.recommendedBookmarks = bookmarks.map((b: string) => `remote/${b}`)));
      void this.pullRecommendedBookmarks(this.initialConnectionContext);
    } catch (err) {
      this.initialConnectionContext.logger.error('Error fetching recommended bookmarks:', err);
      onFetched?.([]);
    }
  }

  public async fetchAndSetHiddenMasterConfig(
    onFetched?: (config: Record<string, Array<string>> | null, odType: string | null) => void,
  ) {
    if (!Internal.fetchHiddenMasterBranchConfig) {
      return;
    }

    try {
      const [config, odType] = await Promise.all([
        Internal.fetchHiddenMasterBranchConfig(this.initialConnectionContext).catch(
          (err: unknown) => {
            this.initialConnectionContext.logger.warn(
              'Failed to fetch hidden master branch config:',
              err,
            );
            return null;
          },
        ),
        Internal.getDevEnvType?.().catch((err: unknown) => {
          this.initialConnectionContext.logger.warn('Failed to fetch OD type:', err);
          return null;
        }),
      ]);

      onFetched?.(config ?? {}, odType ?? '');
    } catch (err) {
      this.initialConnectionContext.logger.error(
        'Error fetching hidden master branch config:',
        err,
      );
      onFetched?.({}, '');
    }
  }

  async pullRecommendedBookmarks(ctx: RepositoryContext): Promise<void> {
    if (!this.recommendedBookmarks || !this.recommendedBookmarks.length) {
      return;
    }

    try {
      const result = await this.runCommand(
        ['bookmarks', '--list-subscriptions'],
        'BookmarksCommand',
        ctx,
      );
      const subscribed = this.parseSubscribedBookmarks(result.stdout);
      const missingBookmarks = this.recommendedBookmarks.filter(
        bookmark => !subscribed.has(bookmark),
      );

      if (missingBookmarks.length > 0) {
        // We need to strip to pull the remote names
        const missingRemoteNames = missingBookmarks.map(bookmark =>
          bookmark.replace(/^remote\//, ''),
        );

        const pullBookmarkOperation = this.createPullBookmarksOperation(missingRemoteNames);
        await this.runOrQueueOperation(ctx, pullBookmarkOperation, () => {});
        ctx.logger.info(`Ran pull on new recommended bookmarks: ${missingRemoteNames.join(', ')}`);
      } else {
        // Fetch again as recommended bookmarks likely would not have been set before the startup fetch
        // If bookmarks were pulled, this is automatically called
        this.fetchSmartlogCommits();
      }
    } catch (err) {
      let error = err;
      if (isEjecaError(error)) {
        error = simplifyEjecaError(error);
      }

      ctx.logger.error('Unable to pull new recommended bookmark(s): ', error);
    }
  }

  /** Get the current head commit if loaded */
  getHeadCommit(): CommitInfo | undefined {
    return this.smartlogCommits?.commits.value?.find(commit => commit.isDot);
  }

  /** Watch for changes to the head commit, e.g. from checking out a new commit */
  subscribeToHeadCommit(callback: (head: CommitInfo) => unknown) {
    let headCommit = this.getHeadCommit();
    if (headCommit != null) {
      callback(headCommit);
    }
    const onData = (data: FetchedCommits) => {
      const newHead = data?.commits.value?.find(commit => commit.isDot);
      if (newHead != null && newHead.hash !== headCommit?.hash) {
        callback(newHead);
        headCommit = newHead;
      }
    };
    this.smartlogCommitsChangesEmitter.on('change', onData);
    return {
      dispose: () => {
        this.smartlogCommitsChangesEmitter.off('change', onData);
      },
    };
  }

  getSubmoduleMap(): SubmodulesByRoot | undefined {
    return this.submodulesByRoot;
  }

  getSubmodulePathCache(): ImSet<RepoRelativePath> | undefined {
    if (this.submodulePathCache === undefined) {
      const paths = this.submodulesByRoot?.get(this.info.repoRoot)?.value?.map(m => m.path);
      this.submodulePathCache = paths ? ImSet(paths) : undefined;
    }
    return this.submodulePathCache;
  }

  async fetchSubmoduleMap(): Promise<void> {
    if (this.info.repoRoots == null) {
      return;
    }
    const submoduleMap = new Map();
    await Promise.all(
      this.info.repoRoots?.map(async root => {
        try {
          const proc = await this.runCommand(
            ['debuggitmodules', '--json', '--repo', root],
            'LogCommand',
            this.initialConnectionContext,
          );
          const submodules = JSON.parse(proc.stdout) as Submodule[];
          submoduleMap.set(root, {value: submodules?.length === 0 ? undefined : submodules});
        } catch (err) {
          let error = err;
          if (isEjecaError(error)) {
            // debuggitmodules may not be supported by older versions of Sapling
            error = error.stderr.includes('unknown command')
              ? Error('debuggitmodules command is not supported by your sapling version.')
              : simplifyEjecaError(error);
          }
          this.initialConnectionContext.logger.error('Error fetching submodules: ', error);

          submoduleMap.set(root, {error: new Error(err as string)});
        }
      }),
    );

    this.submodulesByRoot = submoduleMap;
    this.submodulePathCache = undefined; // Invalidate path cache
    this.submodulesChangesEmitter.emit('change', submoduleMap);
  }

  private catLimiter = new RateLimiter(MAX_SIMULTANEOUS_CAT_CALLS, s =>
    this.initialConnectionContext.logger.info('[cat]', s),
  );
  /** Return file content at a given revset, e.g. hash or `.` */
  public cat(ctx: RepositoryContext, file: AbsolutePath, rev: Revset): Promise<string> {
    return this.catLimiter.enqueueRun(async () => {
      // For `sl cat`, we want the output of the command verbatim.
      const options = {stripFinalNewline: false};
      return (await this.runCommand(['cat', file, '--rev', rev], 'CatCommand', ctx, options))
        .stdout;
    });
  }

  /**
   * Returns line-by-line blame information for a file at a given commit.
   * Returns the line content and commit info.
   * Note: the line will including trailing newline.
   */
  public async blame(
    ctx: RepositoryContext,
    filePath: string,
    hash: string,
  ): Promise<Array<[line: string, info: CommitInfo | undefined]>> {
    const t1 = Date.now();
    const output = await this.runCommand(
      ['blame', filePath, '-Tjson', '--change', '--rev', hash],
      'BlameCommand',
      ctx,
      undefined,
      /* don't timeout */ 0,
    );
    const blame = JSON.parse(output.stdout) as Array<{lines: Array<{line: string; node: string}>}>;
    const t2 = Date.now();

    if (blame.length === 0) {
      // no blame for file, perhaps it was added or untracked
      return [];
    }

    const hashes = new Set<string>();
    for (const line of blame[0].lines) {
      hashes.add(line.node);
    }
    // We don't get all the info we need from the  blame command, so we run `sl log` on the hashes.
    // TODO: we could make the blame command return this directly, which is probably faster.
    // TODO: We don't actually need all the fields in FETCH_TEMPLATE for blame. Reducing this template may speed it up as well.
    const commits = await this.lookupCommits(ctx, [...hashes]);
    const t3 = Date.now();
    ctx.logger.info(
      `Fetched ${commits.size} commits for blame. Blame took ${(t2 - t1) / 1000}s, commits took ${
        (t3 - t2) / 1000
      }s`,
    );
    return blame[0].lines.map(({node, line}) => [line, commits.get(node)]);
  }

  public async getCommitCloudState(ctx: RepositoryContext): Promise<CommitCloudSyncState> {
    const lastChecked = new Date();

    const [extension, backupStatuses, cloudStatus] = await Promise.allSettled([
      this.forceGetConfig(ctx, 'extensions.commitcloud'),
      this.fetchCommitCloudBackupStatuses(ctx),
      this.fetchCommitCloudStatus(ctx),
    ]);
    if (extension.status === 'fulfilled' && extension.value !== '') {
      return {
        lastChecked,
        isDisabled: true,
      };
    }

    if (backupStatuses.status === 'rejected') {
      return {
        lastChecked,
        syncError: backupStatuses.reason,
      };
    } else if (cloudStatus.status === 'rejected') {
      return {
        lastChecked,
        workspaceError: cloudStatus.reason,
      };
    }

    return {
      lastChecked,
      ...cloudStatus.value,
      commitStatuses: backupStatuses.value,
    };
  }

  private async fetchCommitCloudBackupStatuses(
    ctx: RepositoryContext,
  ): Promise<Map<Hash, CommitCloudBackupStatus>> {
    const revset = 'draft() - backedup()';
    const commitCloudBackupStatusTemplate = `{dict(
      hash="{node}",
      backingup="{backingup}",
      date="{date|isodatesec}"
      )|json}\n`;

    const output = await this.runCommand(
      ['log', '--rev', revset, '--template', commitCloudBackupStatusTemplate],
      'CommitCloudSyncBackupStatusCommand',
      ctx,
    );

    const rawObjects = output.stdout.trim().split('\n');
    const parsedObjects = rawObjects
      .map(rawObject => {
        try {
          return JSON.parse(rawObject) as {hash: Hash; backingup: 'True' | 'False'; date: string};
        } catch (_err) {
          return null;
        }
      })
      .filter(notEmpty);

    const now = new Date();
    const TEN_MIN = 10 * 60 * 1000;
    const statuses = new Map<Hash, CommitCloudBackupStatus>(
      parsedObjects.map(obj => [
        obj.hash,
        obj.backingup === 'True'
          ? CommitCloudBackupStatus.InProgress
          : now.valueOf() - new Date(obj.date).valueOf() < TEN_MIN
            ? CommitCloudBackupStatus.Pending
            : CommitCloudBackupStatus.Failed,
      ]),
    );
    return statuses;
  }

  private async fetchCommitCloudStatus(ctx: RepositoryContext): Promise<{
    lastBackup: Date | undefined;
    currentWorkspace: string;
    workspaceChoices: Array<string>;
  }> {
    const [cloudStatusOutput, cloudListOutput] = await Promise.all([
      this.runCommand(['cloud', 'status'], 'CommitCloudStatusCommand', ctx),
      this.runCommand(['cloud', 'list'], 'CommitCloudListCommand', ctx),
    ]);

    const currentWorkspace =
      /Workspace: ([a-zA-Z/0-9._-]+)/.exec(cloudStatusOutput.stdout)?.[1] ?? 'default';
    const lastSyncTimeStr = /Last Sync Time: (.*)/.exec(cloudStatusOutput.stdout)?.[1];
    const lastBackup = lastSyncTimeStr != null ? new Date(lastSyncTimeStr) : undefined;
    const workspaceChoices = cloudListOutput.stdout
      .split('\n')
      .map(line => /^ {8}([a-zA-Z/0-9._-]+)(?: \(connected\))?/.exec(line)?.[1] as string)
      .filter(l => l != null);

    return {
      lastBackup,
      currentWorkspace,
      workspaceChoices,
    };
  }

  private commitCache = new LRU<string, CommitInfo>(100); // TODO: normal commits fetched from smartlog() aren't put in this cache---this is mostly for blame right now.
  public async lookupCommits(
    ctx: RepositoryContext,
    hashes: Array<string>,
  ): Promise<Map<string, CommitInfo>> {
    const hashesToFetch = hashes.filter(hash => this.commitCache.get(hash) == undefined);

    const commits =
      hashesToFetch.length === 0
        ? [] // don't bother running log
        : await this.runCommand(
            [
              'log',
              '--template',
              getMainFetchTemplate(this.info.codeReviewSystem),
              '--rev',
              hashesToFetch.join('+'),
            ],
            'LookupCommitsCommand',
            ctx,
          ).then(output => {
            return parseCommitInfoOutput(
              ctx.logger,
              output.stdout.trim(),
              this.info.codeReviewSystem,
            );
          });

    const result = new Map();
    for (const hash of hashes) {
      const found = this.commitCache.get(hash);
      if (found != undefined) {
        result.set(hash, found);
      }
    }

    for (const commit of commits) {
      if (commit) {
        this.commitCache.set(commit.hash, commit);
        result.set(commit.hash, commit);
      }
    }

    return result;
  }

  public async fetchSignificantLinesOfCode(
    ctx: RepositoryContext,
    hash: Hash,
    excludedFiles: string[],
  ): Promise<SlocDelta | undefined> {
    const exclusions = excludedFiles.flatMap(file => [
      '-X',
      absolutePathForFileInRepo(file, this) ?? file,
    ]);

    const output = (
      await this.runCommand(
        ['diff', '--stat', '-B', '-X', '**__generated__**', ...exclusions, '-c', hash],
        'SlocCommand',
        ctx,
      )
    ).stdout;

    const sloc = this.parseSlocFrom(output);

    ctx.logger.info(
      'Fetched SLOC for commit:',
      hash,
      output,
      `SLOC: +${sloc.insertions} -${sloc.deletions}`,
    );
    return sloc;
  }

  public async fetchPendingAmendSignificantLinesOfCode(
    ctx: RepositoryContext,
    hash: Hash,
    includedFiles: string[],
  ): Promise<SlocDelta | undefined> {
    if (includedFiles.length === 0) {
      return undefined;
    }
    const inclusions = includedFiles.flatMap(file => [
      '-I',
      absolutePathForFileInRepo(file, this) ?? file,
    ]);

    const output = (
      await this.runCommand(
        ['diff', '--stat', '-B', '-X', '**__generated__**', ...inclusions, '-r', '.^'],
        'PendingSlocCommand',
        ctx,
      )
    ).stdout;

    if (output.trim() === '') {
      return undefined;
    }

    const sloc = this.parseSlocFrom(output);

    ctx.logger.info(
      'Fetched Pending AMEND SLOC for commit:',
      hash,
      output,
      `SLOC: +${sloc.insertions} -${sloc.deletions}`,
    );
    return sloc;
  }

  public async fetchPendingSignificantLinesOfCode(
    ctx: RepositoryContext,
    hash: Hash,
    includedFiles: string[],
  ): Promise<SlocDelta | undefined> {
    if (includedFiles.length === 0) {
      return undefined; // don't bother running sl diff if there are no files to include
    }
    const inclusions = includedFiles.flatMap(file => [
      '-I',
      absolutePathForFileInRepo(file, this) ?? file,
    ]);

    const output = (
      await this.runCommand(
        ['diff', '--stat', '-B', '-X', '**__generated__**', ...inclusions],
        'PendingSlocCommand',
        ctx,
      )
    ).stdout;

    const sloc = this.parseSlocFrom(output);

    ctx.logger.info(
      'Fetched Pending SLOC for commit:',
      hash,
      output,
      `SLOC: +${sloc.insertions} -${sloc.deletions}`,
    );
    return sloc;
  }

  private parseSlocFrom(output: string): SlocDelta {
    const lines = output.trim().split('\n');
    const changes = lines[lines.length - 1];
    const diffStatRe = /\d+ files changed, (\d+) insertions\(\+\), (\d+) deletions\(-\)/;
    const diffStatMatch = changes.match(diffStatRe);
    return {
      insertions: parseInt(diffStatMatch?.[1] ?? '0', 10),
      deletions: parseInt(diffStatMatch?.[2] ?? '0', 10),
    };
  }

  private parseSubscribedBookmarks(output: string): Set<string> {
    return new Set(
      output
        .split('\n')
        .filter(line => line.trim())
        .map(line => line.trim().split(/\s+/)[0]),
    );
  }

  /**
   * Create a runnable operation for pulling bookmarks.
   */
  private createPullBookmarksOperation(bookmarks: Array<string>): RunnableOperation {
    const args = ['pull'];
    for (const bookmark of bookmarks) {
      args.push('-B', bookmark);
    }

    return {
      args,
      id: randomId(),
      runner: CommandRunner.Sapling,
      trackEventName: 'PullOperation',
    };
  }

  public async getAllChangedFiles(ctx: RepositoryContext, hash: Hash): Promise<Array<ChangedFile>> {
    const output = (
      await this.runCommand(
        ['log', '--template', CHANGED_FILES_TEMPLATE, '--rev', hash],
        'LookupAllCommitChangedFilesCommand',
        ctx,
      )
    ).stdout;

    const [chunk] = output.split(COMMIT_END_MARK, 1);

    const lines = chunk.trim().split('\n');
    if (lines.length < Object.keys(CHANGED_FILES_FIELDS).length) {
      return [];
    }

    const files: Array<ChangedFile> = [
      ...(JSON.parse(lines[CHANGED_FILES_INDEX.filesModified]) as Array<string>).map(path => ({
        path,
        status: 'M' as const,
      })),
      ...(JSON.parse(lines[CHANGED_FILES_INDEX.filesAdded]) as Array<string>).map(path => ({
        path,
        status: 'A' as const,
      })),
      ...(JSON.parse(lines[CHANGED_FILES_INDEX.filesRemoved]) as Array<string>).map(path => ({
        path,
        status: 'R' as const,
      })),
    ];

    return files;
  }

  public async getShelvedChanges(ctx: RepositoryContext): Promise<Array<ShelvedChange>> {
    const output = (
      await this.runCommand(
        ['log', '--rev', 'shelved()', '--template', SHELVE_FETCH_TEMPLATE],
        'GetShelvesCommand',
        ctx,
      )
    ).stdout;

    const shelves = parseShelvedCommitsOutput(ctx.logger, output.trim());
    // sort by date ascending
    shelves.sort((a, b) => b.date.getTime() - a.date.getTime());
    return shelves;
  }

  public getAllDiffIds(): Array<DiffId> {
    return (
      this.getSmartlogCommits()
        ?.commits.value?.map(commit => commit.diffId)
        .filter(notEmpty) ?? []
    );
  }

  /**
   * Use the code review provider to fetch authored diff commit hashes,
   * then unhide them so they are visible in the smartlog.
   */
  public async pullFetchedDiffs(): Promise<void> {
    const hashes = await this.codeReviewProvider?.fetchAuthoredDiffs?.();
    if (hashes == null || hashes.length === 0) {
      return;
    }

    const revset = hashes.join(' + ');
    // Use `sl unhide` instead of `sl pull` since pull doesn't support using '- hidden()'
    await runCommand(this.initialConnectionContext, ['unhide', '-r', `(${revset}) - hidden()`]);
  }

  public async getActiveAlerts(ctx: RepositoryContext): Promise<Array<Alert>> {
    const result = await this.runCommand(['config', '-Tjson', 'alerts'], 'GetAlertsCommand', ctx, {
      reject: false,
    });
    if (result.exitCode !== 0 || !result.stdout) {
      return [];
    }
    try {
      const configs = JSON.parse(result.stdout) as [{name: string; value: unknown}];
      const alerts = parseAlerts(configs);
      ctx.logger.info('Found active alerts:', alerts);
      return alerts;
    } catch (_e) {
      return [];
    }
  }

  public async getRagePaste(ctx: RepositoryContext): Promise<string> {
    const output = await this.runCommand(['rage'], 'RageCommand', ctx, undefined, 90_000);
    const match = /P\d{9,}/.exec(output.stdout);
    if (match) {
      return match[0];
    }
    throw new Error('No paste found in rage output: ' + output.stdout);
  }

  public async runDiff(
    ctx: RepositoryContext,
    comparison: Comparison,
    contextLines = 4,
    ignoreWhitespace?: boolean,
  ): Promise<string> {
    const output = await this.runCommand(
      [
        'diff',
        ...revsetArgsForComparison(comparison),
        // don't include a/ and b/ prefixes on files
        '--noprefix',
        '--no-binary',
        '--nodate',
        '--unified',
        String(contextLines),
        ...(ignoreWhitespace ? ['-w'] : []),
      ],
      'DiffCommand',
      ctx,
    );
    return output.stdout;
  }

  /**
   * Get the list of changed files for a given comparison.
   * Uses `sl status` with the appropriate revset arguments.
   */
  public async getFilesChangedForComparison(
    ctx: RepositoryContext,
    comparison: Comparison,
  ): Promise<Array<ChangedFile>> {
    const output = await this.runCommand(
      ['status', ...revsetArgsForComparison(comparison), '-Tjson', '--copies'],
      'StatusForComparisonCommand',
      ctx,
    );
    if (!output.stdout.trim()) {
      return [];
    }
    return (JSON.parse(output.stdout) as Array<ChangedFile>).map(change => ({
      ...change,
      path: removeLeadingPathSep(change.path),
    }));
  }

  public runCommand(
    args: Array<string>,
    /** Which event name to track for this command. If undefined, generic 'RunCommand' is used. */
    eventName: TrackEventName | undefined,
    ctx: RepositoryContext,
    options?: EjecaOptions,
    timeout?: number,
  ) {
    const id = randomId();
    return ctx.tracker.operation(
      eventName ?? 'RunCommand',
      'RunCommandError',
      {
        // if we don't specify a specific eventName, provide the command arguments in logging
        extras: eventName == null ? {args} : undefined,
        operationId: `isl:${id}`,
      },
      async () =>
        runCommand(
          ctx,
          args,
          {
            ...options,
            env: {
              ...options?.env,
              ...((await Internal.additionalEnvForCommand?.(id)) ?? {}),
            } as NodeJS.ProcessEnv,
          },
          timeout ?? READ_COMMAND_TIMEOUT_MS,
        ),
    );
  }

  /** Read a config. The config name must be part of `allConfigNames`. */
  public async getConfig(
    ctx: RepositoryContext,
    configName: ConfigName,
  ): Promise<string | undefined> {
    return (await this.getKnownConfigs(ctx)).get(configName);
  }

  /**
   * Read a single config, forcing a new dedicated call to `sl config`.
   * Prefer `getConfig` to batch fetches when possible.
   */
  public async forceGetConfig(
    ctx: RepositoryContext,
    configName: string,
  ): Promise<string | undefined> {
    const result = (await runCommand(ctx, ['config', configName])).stdout;
    this.initialConnectionContext.logger.info(
      `loaded configs from ${ctx.cwd}: ${configName} => ${result}`,
    );
    return result;
  }

  /** Load all "known" configs. Cached on `this`. */
  public getKnownConfigs(
    ctx: RepositoryContext,
  ): Promise<ReadonlyMap<ConfigName, string | undefined>> {
    if (ctx.knownConfigs != null) {
      return Promise.resolve(ctx.knownConfigs);
    }
    return this.configRateLimiter.enqueueRun(async () => {
      if (ctx.knownConfigs == null) {
        // Fetch all configs using one command.
        const knownConfig = new Map<ConfigName, string>(
          await getConfigs<ConfigName>(ctx, allConfigNames),
        );
        ctx.knownConfigs = knownConfig;
      }
      return ctx.knownConfigs;
    });
  }

  public setConfig(
    ctx: RepositoryContext,
    level: ConfigLevel,
    configName: SettableConfigName,
    configValue: string,
  ): Promise<void> {
    if (!settableConfigNames.includes(configName)) {
      return Promise.reject(
        new Error(`config ${configName} not in allowlist for settable configs`),
      );
    }
    // Attempt to avoid racy config read/write.
    return this.configRateLimiter.enqueueRun(() => setConfig(ctx, level, configName, configValue));
  }

  /** Load and apply configs to `this` in background. */
  private applyConfigInBackground(ctx: RepositoryContext) {
    this.getConfig(ctx, 'isl.hold-off-refresh-ms').then(configValue => {
      if (configValue != null) {
        const numberValue = parseInt(configValue, 10);
        if (numberValue >= 0) {
          this.configHoldOffRefreshMs = numberValue;
        }
      }
    });
  }
}

export function repoRelativePathForAbsolutePath(
  absolutePath: AbsolutePath,
  repo: Repository,
  pathMod = path,
): RepoRelativePath {
  return pathMod.relative(repo.info.repoRoot, absolutePath);
}

/**
 * Returns absolute path for a repo-relative file path.
 * If the path "escapes" the repository's root dir, returns null
 * Used to validate that a file path does not "escape" the repo, and the file can safely be modified on the filesystem.
 * absolutePathForFileInRepo("foo/bar/file.txt", repo) -> /path/to/repo/foo/bar/file.txt
 * absolutePathForFileInRepo("../file.txt", repo) -> null
 */
export function absolutePathForFileInRepo(
  filePath: RepoRelativePath,
  repo: Repository,
  pathMod = path,
): AbsolutePath | null {
  // Note that resolve() is contractually obligated to return an absolute path.
  const fullPath = pathMod.resolve(repo.info.repoRoot, filePath);
  // Prefix checks on paths can be footguns on Windows for C:\\ vs c:\\, but since
  // we use the same exact path check here and in the resolve, there should be
  // no incompatibility here.
  if (fullPath.startsWith(repo.info.repoRoot + pathMod.sep)) {
    return fullPath;
  } else {
    return null;
  }
}

function isUnhealthyEdenFs(cwd: string): Promise<boolean> {
  return exists(path.join(cwd, 'README_EDEN.txt'));
}

/**
 * Global `sl` flags that change which config or repository a command uses.
 * Client-supplied operations must use structured `config` args instead.
 */
const ILLEGAL_LONG_FLAGS = ['--config', '--configfile', '--cwd', '--insecure', '--repository'];

/**
 * Whether an operation arg sets one of {@link ILLEGAL_LONG_FLAGS} or `-R`.
 * `sl` accepts `--flag=value`, unique prefixes of some long flags (`--repo`),
 * and short flags with attached values (`-R/path`), so exact matching is not enough.
 */
export function isIllegalOperationFlag(arg: string): boolean {
  if (arg.startsWith('--')) {
    const name = arg.split('=', 1)[0];
    return name.length > 2 && ILLEGAL_LONG_FLAGS.some(flag => flag.startsWith(name));
  }
  return /^-[a-zA-Z]*R/.test(arg);
}

/**
 * `--tool` (`-t`) names an external program to run for merges, which is as powerful as `--config`.
 * Allow only built-in tools and the merge tool the user configured in `ui.merge`.
 */
export function validateMergeToolArgs(args: ReadonlyArray<string>, configuredTool?: string) {
  const isAllowedTool = (tool: string | undefined) =>
    tool != null && (/^(internal)?:[a-z0-9-]+$/i.test(tool) || tool === configuredTool);
  args.forEach((arg, i) => {
    let tool: string | undefined;
    if (arg === '--tool') {
      tool = args[i + 1];
    } else if (arg.startsWith('--tool=')) {
      tool = arg.slice('--tool='.length);
    } else if (/^-[a-zA-Z]*t/.test(arg)) {
      // ISL operations always use `--tool`; `-t` would also hide the value from this check.
      throw new Error(`argument '${arg}' is not allowed`);
    } else {
      return;
    }
    if (!isAllowedTool(tool)) {
      throw new Error(`merge tool '${tool}' is not allowed`);
    }
  });
}

async function isEdenFsRepo(repoRoot: AbsolutePath): Promise<boolean> {
  try {
    await fs.promises.access(path.join(repoRoot, '.eden'));
    return true;
  } catch {}
  return false;
}
