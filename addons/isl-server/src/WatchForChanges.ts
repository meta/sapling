/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import type {PageVisibility, ValidatedRepoInfo} from 'isl/src/types';
import type {PageFocusTracker} from './PageFocusTracker';
import type {Logger} from './logger';

import fs from 'node:fs/promises';
import path from 'node:path';
import {debounce} from 'shared/debounce';
import {stagedThrottler} from './StagedThrottler';
import type {SubscriptionCallback} from './__generated__/node-edenfs-notifications-client';
import {EdenFSUtils} from './__generated__/node-edenfs-notifications-client';
import {type ServerSideTracker} from './analytics/serverSideTracker';
import {ONE_MINUTE_MS} from './constants';
import {EdenFSNotifications} from './edenFsNotifications';
import type {RepositoryContext} from './serverTypes';
import {Watchman} from './watchman';

const DEFAULT_POLL_INTERVAL = 15 * ONE_MINUTE_MS;
// When the page is hidden, aggressively reduce polling.
const HIDDEN_POLL_INTERVAL = 3 * 60 * ONE_MINUTE_MS;
// When visible or focused, poll frequently
const VISIBLE_POLL_INTERVAL = 2 * ONE_MINUTE_MS;
const FOCUSED_POLL_INTERVAL = 0.5 * ONE_MINUTE_MS;
const ON_FOCUS_REFETCH_THROTTLE = 15_000;
const ON_VISIBLE_REFETCH_THROTTLE = 30_000;

export type KindOfChange = 'uncommitted changes' | 'commits' | 'merge conflicts' | 'everything';
export type PollKind = PageVisibility | 'force';

/**
 * Handles watching for changes to files on disk which should trigger refetching data,
 * and polling for changes when watching is not reliable.
 */
export class WatchForChanges {
  static WATCHMAN_DEFER = `hg.update`; // TODO: update to sl
  static WATCHMAN_DEFER_TRANSACTION = `hg.transaction`; // TODO: update to sl
  public watchman: Watchman;
  public edenfs: EdenFSNotifications;

  private dirstateDisposables: Array<() => unknown> = [];
  private watchmanDisposables: Array<() => unknown> = [];
  private edenfsDisposables: Array<() => unknown> = [];
  private logger: Logger;
  private tracker: ServerSideTracker;
  private dirstateSubscriptionPromise: Promise<void>;

  constructor(
    private repoInfo: ValidatedRepoInfo,
    private pageFocusTracker: PageFocusTracker,
    private changeCallback: (kind: KindOfChange, pollKind?: PollKind) => unknown,
    ctx: RepositoryContext,
    watchman?: Watchman | undefined,
    edenfs?: EdenFSNotifications | undefined,
  ) {
    this.logger = ctx.logger;
    this.tracker = ctx.tracker;
    this.watchman = watchman ?? new Watchman(ctx.logger, ctx.tracker);

    const {repoRoot} = this.repoInfo;
    this.edenfs = edenfs ?? new EdenFSNotifications(ctx.logger, repoRoot);

    // Watch dirstate right away for commit changes
    this.dirstateSubscriptionPromise = this.setupDirstateSubscriptions();
    this.setupPolling();
    this.pageFocusTracker.onChange(this.poll.bind(this));
    // poll right away so we get data immediately, without waiting for timeout on startup
    this.poll('force');
  }

  private timeout: NodeJS.Timeout | undefined;
  private lastFetch = new Date().valueOf();

  /**
   * Waits for the dirstate subscription to be set up
   * since we can't await in the constructor
   * Resolves when dirstateSubscriptionPromise is fulfilled.
   */
  public async waitForDirstateSubscriptionReady(): Promise<void> {
    await this.dirstateSubscriptionPromise;
  }

  /**
   * Combine different signals to determine what interval to poll for information
   */
  private setupPolling() {
    this.timeout = setTimeout(this.poll, DEFAULT_POLL_INTERVAL);
  }

  /**
   * Re-trigger fetching data from the repository,
   * depending on how recently that data was last fetched,
   * and whether any ISL windows are focused or visible.
   *
   * This function calls itself on an interval to check whether we should fetch changes,
   * but it can also be called in response to events like focus being gained.
   */
  public poll = (kind?: PollKind) => {
    // calculate how long we'd like to be waiting from what we know of the windows.
    let desiredNextTickTime = DEFAULT_POLL_INTERVAL;

    if (this.repoInfo.isEdenFs !== true) {
      if (this.watchman.status !== 'healthy') {
        if (this.pageFocusTracker.hasPageWithFocus()) {
          desiredNextTickTime = FOCUSED_POLL_INTERVAL;
        } else if (this.pageFocusTracker.hasVisiblePage()) {
          desiredNextTickTime = VISIBLE_POLL_INTERVAL;
        }
      } else {
        // if watchman is working normally, and we're not visible, don't poll nearly as often
        if (!this.pageFocusTracker.hasPageWithFocus() && !this.pageFocusTracker.hasVisiblePage()) {
          desiredNextTickTime = HIDDEN_POLL_INTERVAL;
        }
      }
    } else {
      // if using eden and we're not visible, don't poll nearly as often
      if (!this.pageFocusTracker.hasPageWithFocus() && !this.pageFocusTracker.hasVisiblePage()) {
        desiredNextTickTime = HIDDEN_POLL_INTERVAL;
      }
    }

    const now = Date.now();
    const elapsedTickTime = now - this.lastFetch;

    if (
      kind === 'force' ||
      // we've been waiting longer than desired
      elapsedTickTime >= desiredNextTickTime ||
      // the moment a window gains focus or visibility, consider polling immediately
      (kind === 'focused' && elapsedTickTime >= ON_FOCUS_REFETCH_THROTTLE) ||
      (kind === 'visible' && elapsedTickTime >= ON_VISIBLE_REFETCH_THROTTLE)
    ) {
      // it's time to fetch
      this.changeCallback('everything', kind);
      this.lastFetch = Date.now();

      clearTimeout(this.timeout);
      this.timeout = setTimeout(this.poll, desiredNextTickTime);
    } else {
      // we have some time left before we we would expect to need to poll, schedule next poll
      clearTimeout(this.timeout);
      this.timeout = setTimeout(this.poll, desiredNextTickTime - elapsedTickTime);
    }
  };

  private async setupDirstateSubscriptions() {
    if (this.repoInfo.isEdenFs === true) {
      await this.setupEdenDirstateSubscriptions();
    } else {
      await this.setupWatchmanDirstateSubscriptions();
    }
  }

  private async setupWatchmanDirstateSubscriptions() {
    const {repoRoot, dotdir} = this.repoInfo;

    if (repoRoot == null || dotdir == null) {
      this.logger.error(`skipping dirstate subscription since ${repoRoot} is not a repository`);
      return;
    }

    // Resolve the repo dot dir in case it is a symlink. Watchman doesn't follow symlinks,
    // so we must follow it and watch the target.
    const realDotdir = await fs.realpath(dotdir);

    if (realDotdir != dotdir) {
      this.logger.info(`resolved dotdir ${dotdir} to ${realDotdir}`);

      // Write out ".watchmanconfig" so realDotdir passes muster as a watchman "root dir"
      // (otherwise watchman will refuse to watch it).
      await fs.writeFile(path.join(realDotdir, '.watchmanconfig'), '{}');
    }

    const DIRSTATE_WATCHMAN_SUBSCRIPTION = 'sapling-smartlog-dirstate-change';
    try {
      const handleRepositoryStateChange = debounce(() => {
        // if the repo changes, also recheck files. E.g. if you commit, your uncommitted changes will also change.
        this.changeCallback('everything');

        // reset timer for polling
        this.lastFetch = new Date().valueOf();
      }, 100); // debounce so that multiple quick changes don't trigger multiple fetches for no reason

      this.logger.info('setting up dirstate subscription', realDotdir);

      const dirstateSubscription = await this.watchman.watchDirectoryRecursive(
        realDotdir,
        DIRSTATE_WATCHMAN_SUBSCRIPTION,
        {
          fields: ['name'],
          expression: [
            'name',
            ['bookmarks.current', 'bookmarks', 'dirstate', 'merge'],
            'wholename',
          ],
          defer: [WatchForChanges.WATCHMAN_DEFER],
          empty_on_fresh_instance: true,
        },
      );
      dirstateSubscription.emitter.on('change', changes => {
        if (changes.includes('merge')) {
          this.changeCallback('merge conflicts');
        }
        if (changes.includes('dirstate')) {
          handleRepositoryStateChange();
        }
      });
      dirstateSubscription.emitter.on('fresh-instance', handleRepositoryStateChange);

      this.dirstateDisposables.push(() => {
        this.logger.info('unsubscribe dirstate watcher');
        this.watchman.unwatch(realDotdir, DIRSTATE_WATCHMAN_SUBSCRIPTION);
      });
    } catch (err) {
      this.logger.error('failed to setup dirstate subscriptions', err);
      this.tracker.error(
        'WatchmanEvent',
        'WatchmanError',
        `failed to setup watchman dirstate subscriptions ${err}`,
      );
    }
  }

  private async setupEdenDirstateSubscriptions() {
    const {repoRoot, dotdir} = this.repoInfo;

    if (repoRoot == null || dotdir == null) {
      this.logger.error(`skipping dirstate subscription since ${repoRoot} is not a repository`);
      return;
    }

    const relativeRoot = path.relative(repoRoot, dotdir);

    const DIRSTATE_EDENFS_SUBSCRIPTION = 'sapling-smartlog-dirstate-change-edenfs';
    try {
      const handleRepositoryStateChange = debounce(() => {
        // if the repo changes, also recheck files. E.g. if you commit, your uncommitted changes will also change.
        this.changeCallback('everything');

        // reset timer for polling
        this.lastFetch = new Date().valueOf();
      }, 100); // debounce so that multiple quick changes don't trigger multiple fetches for no reason

      this.logger.info(
        'setting up dirstate edenfs subscription in root',
        repoRoot,
        'at',
        relativeRoot,
      );

      const subscriptionCallback: SubscriptionCallback = (error, resp) => {
        if (error) {
          this.logger.error('EdenFS dirstate subscription error:', error.message);
          this.tracker.error('EdenWatcherEvent', 'EdenWatcherError', error);
          return;
        } else if (resp === null) {
          // EdenFS subscription closed
          return;
        } else {
          if (resp.changes && resp.changes.length > 0) {
            resp.changes.forEach(change => {
              if (change.SmallChange) {
                const paths = EdenFSUtils.extractPaths([change]);
                if (paths.includes('merge')) {
                  this.changeCallback('merge conflicts');
                  return;
                }
                if (paths.includes('dirstate')) {
                  handleRepositoryStateChange();
                  return;
                }
              } else if (change.LargeChange) {
                handleRepositoryStateChange();
                return;
              }
            });
          }
          return;
        }
      };

      await this.edenfs.watchDirectoryRecursive(
        repoRoot,
        DIRSTATE_EDENFS_SUBSCRIPTION,
        {
          useCase: 'isl-server-node',
          mountPoint: repoRoot,
          throttle: 100,
          relativeRoot,
          deferredStates: [
            WatchForChanges.WATCHMAN_DEFER,
            WatchForChanges.WATCHMAN_DEFER_TRANSACTION,
          ],
          includeVcsRoots: true,
        },
        subscriptionCallback,
      );
      this.dirstateDisposables.push(() => {
        this.logger.info('unsubscribe dirstate edenfs watcher');
        this.edenfs.unwatch(repoRoot, DIRSTATE_EDENFS_SUBSCRIPTION);
      });
    } catch (err) {
      this.logger.error('failed to setup dirstate edenfs subscriptions', err);
      this.tracker.error(
        'EdenWatcherEvent',
        'EdenWatcherError',
        `failed to setup dirstate edenfs subscriptions ${err}`,
      );
    }
  }

  public async setupSubscriptions() {
    await this.waitForDirstateSubscriptionReady();
    if (this.repoInfo.isEdenFs === true) {
      await this.setupEdenSubscriptions();
    } else {
      await this.setupWatchmanSubscriptions();
    }
  }

  /**
   * Some Watchmans subscriptions should only activate when ISL is actually opened.
   * On platforms like vscode, it's possible to create a Repository without actually opening ISL.
   * In those cases, we only want the minimum set of subscriptions to be active.
   * We care about the dirstate watcher, but not the watchman subscriptions in that case.
   */
  public async setupWatchmanSubscriptions() {
    const {repoRoot, dotdir} = this.repoInfo;

    if (repoRoot == null || dotdir == null) {
      this.logger.error(`skipping watchman subscription since ${repoRoot} is not a repository`);
      return;
    }
    const relativeDotdir = path.relative(repoRoot, dotdir);
    // if working from a git clone, the dotdir lives in .git/sl,
    // but we need to ignore changes in .git in our watchman subscriptions
    const outerDotDir =
      relativeDotdir.indexOf(path.sep) >= 0 ? path.dirname(relativeDotdir) : relativeDotdir;

    await this.maybeModifyGitignore(repoRoot, outerDotDir);

    const FILE_CHANGE_WATCHMAN_SUBSCRIPTION = 'sapling-smartlog-file-change';
    try {
      // In some bad cases, a file may not be getting ignored by watchman properly,
      // and ends up constantly triggering the watchman subscription.
      // Incrementally increase the throttling of events to avoid spamming `status`.
      // This does mean "legit" changes will start being missed.
      // TODO: can we scan the list of changes and build a list of files that are overfiring, then send those to the UI as a warning?
      // This would allow a user to know it's happening and possibly fix it for their repo by adding it to a .watchmanconfig.
      const handleUncommittedChanges = stagedThrottler(
        [
          {
            throttleMs: 0,
            numToNextStage: 5,
            resetAfterMs: 5_000,
            onEnter: () => {
              this.logger.info('no longer throttling uncommitted changes');
            },
          },
          {
            throttleMs: 5_000,
            numToNextStage: 10,
            resetAfterMs: 20_000,
            onEnter: () => {
              this.logger.info('slightly throttling uncommitted changes');
            },
          },
          {
            throttleMs: 30_000,
            resetAfterMs: 30_000,
            onEnter: () => {
              this.logger.info('aggressively throttling uncommitted changes');
            },
          },
        ],
        () => {
          this.changeCallback('uncommitted changes');

          // reset timer for polling
          this.lastFetch = new Date().valueOf();
        },
      );
      const uncommittedChangesSubscription = await this.watchman.watchDirectoryRecursive(
        repoRoot,
        FILE_CHANGE_WATCHMAN_SUBSCRIPTION,
        {
          // We only need to know that a change happened (not the list of files) so that we can trigger `status`
          fields: ['name'],
          expression: [
            'allof',
            // This watchman subscription is used to determine when and which
            // files to fetch new statuses for. There is no reason to include
            // directories in these updates, and in fact they may make us overfetch
            // statuses.
            // This line restricts this subscription to only return files.
            ['type', 'f'],
            ['not', ['dirname', outerDotDir]],
            // Even though we tell it not to match .sl, modifying a file inside .sl
            // will emit an event for the folder itself, which we want to ignore.
            ['not', ['match', outerDotDir, 'basename']],
            // Exclude edenfs notifications directory - EdenFS notifications can modify hidden files
            // in this directory which would otherwise trigger watchman events.
            ['not', ['dirname', '.edenfs-notifications-state']],
            ['not', ['match', '.edenfs-notifications-state', 'basename']],
          ],
          defer: [WatchForChanges.WATCHMAN_DEFER],
          empty_on_fresh_instance: true,
        },
      );
      uncommittedChangesSubscription.emitter.on('change', handleUncommittedChanges);
      uncommittedChangesSubscription.emitter.on('fresh-instance', handleUncommittedChanges);

      this.watchmanDisposables.push(() => {
        this.logger.info('unsubscribe watchman');
        this.watchman.unwatch(repoRoot, FILE_CHANGE_WATCHMAN_SUBSCRIPTION);
      });
    } catch (err) {
      this.logger.error('failed to setup watchman subscriptions', err);
      this.tracker.error(
        'WatchmanEvent',
        'WatchmanError',
        `failed to setup watchman subscriptions ${err}`,
      );
    }
  }

  public async setupEdenSubscriptions() {
    const {repoRoot, dotdir} = this.repoInfo;

    if (repoRoot == null || dotdir == null) {
      this.logger.error(`skipping edenfs subscription since ${repoRoot} is not a repository`);
      return;
    }
    const relativeDotdir = path.relative(repoRoot, dotdir);
    // if working from a git clone, the dotdir lives in .git/sl,
    // but we need to ignore changes in .git in our watchman subscriptions
    const outerDotDir =
      relativeDotdir.indexOf(path.sep) >= 0 ? path.dirname(relativeDotdir) : relativeDotdir;

    this.logger.info(
      'setting up edenfs subscription in',
      repoRoot,
      'at',
      outerDotDir,
      'relativeDotdir',
      relativeDotdir,
    );

    const FILE_CHANGE_EDENFS_SUBSCRIPTION = 'sapling-smartlog-file-change-edenfs';
    try {
      // In some bad cases, a file that has a lot of activity can constantly trigger the subscription.
      // Incrementally increase the throttling of events to avoid spamming `status`.
      // This does mean "legit" changes will start being missed.
      // TODO: can we scan the list of changes and build a list of files that are overfiring, then send those to the UI as a warning?
      // This would allow a user to know it's happening and possibly fix it for their repo.
      const handleUncommittedChanges = stagedThrottler(
        [
          {
            throttleMs: 0,
            numToNextStage: 5,
            resetAfterMs: 5_000,
            onEnter: () => {
              this.logger.info('no longer throttling uncommitted changes');
            },
          },
          {
            throttleMs: 5_000,
            numToNextStage: 10,
            resetAfterMs: 20_000,
            onEnter: () => {
              this.logger.info('slightly throttling uncommitted changes');
            },
          },
          {
            throttleMs: 30_000,
            resetAfterMs: 30_000,
            onEnter: () => {
              this.logger.info('aggressively throttling uncommitted changes');
            },
          },
        ],
        () => {
          this.changeCallback('uncommitted changes');

          // reset timer for polling
          this.lastFetch = new Date().valueOf();
        },
      );
      const subscriptionCallback: SubscriptionCallback = (error, resp) => {
        if (error) {
          this.logger.error('EdenFS subscription error:', error.message);
          this.tracker.error('EdenWatcherEvent', 'EdenWatcherError', error);
          return;
        } else if (resp === null) {
          // EdenFS subscription closed
          return;
        } else {
          if (resp.changes && resp.changes.length > 0) {
            handleUncommittedChanges();
          }
        }
      };
      await this.edenfs.watchDirectoryRecursive(
        repoRoot,
        FILE_CHANGE_EDENFS_SUBSCRIPTION,
        {
          useCase: 'isl-server-node',
          mountPoint: repoRoot,
          throttle: 100,
          deferredStates: [
            WatchForChanges.WATCHMAN_DEFER,
            WatchForChanges.WATCHMAN_DEFER_TRANSACTION,
          ],
          excludedRoots: [outerDotDir, relativeDotdir],
        },
        subscriptionCallback,
      );

      this.edenfsDisposables.push(() => {
        this.logger.info('unsubscribe edenfs');
        this.edenfs.unwatch(repoRoot, FILE_CHANGE_EDENFS_SUBSCRIPTION);
      });
    } catch (err) {
      this.logger.error('failed to setup edenfs subscriptions', err);
      this.tracker.error(
        'EdenWatcherEvent',
        'EdenWatcherError',
        `failed to setup edenfs subscriptions ${err}`,
      );
    }
  }

  /**
   * Modify gitignore to ignore watchman cookie files. This is needed when using ISL
   * with git repos. `git status` does not exclude watchman cookie files by default.
   * `sl` does not use watchman in dotgit mode.
   */
  private async maybeModifyGitignore(repoRoot: string, outerDotDir: string) {
    if (outerDotDir !== '.git') {
      return;
    }
    const gitIgnorePath = path.join(repoRoot, outerDotDir, 'info', 'exclude');
    // https://github.com/facebook/watchman/blob/76bd924b1169dae9cb9f5371845ab44ea1f836bf/watchman/Cookie.h#L15
    const rule = '/.watchman-cookie-*';
    try {
      const gitIgnoreContent = await fs.readFile(gitIgnorePath, 'utf8');
      if (!gitIgnoreContent.includes(rule)) {
        await fs.appendFile(gitIgnorePath, `\n${rule}\n`, 'utf8');
      }
    } catch (err) {
      this.logger.error(`failed to read or write ${gitIgnorePath}`, err);
    }
  }

  public disposeWatchmanSubscriptions() {
    this.watchmanDisposables.forEach(dispose => dispose());
  }

  public disposeEdenFSSubscriptions() {
    this.edenfsDisposables.forEach(dispose => dispose());
  }

  public dispose() {
    this.dirstateDisposables.forEach(dispose => dispose());
    this.disposeWatchmanSubscriptions();
    this.disposeEdenFSSubscriptions();
    if (this.timeout) {
      clearTimeout(this.timeout);
      this.timeout = undefined;
    }
  }
}
