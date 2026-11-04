/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import type {Logger} from './logger';

import watchman from 'fb-watchman';
import {EventEmitter} from 'node:events';
import path from 'node:path';
import {sleep} from 'shared/utils';
import {type ServerSideTracker} from './analytics/serverSideTracker';
import {firstOfIterable, serializeAsyncCall} from './utils';

export type WatchmanSubscriptionOptions = {
  fields?: Array<string>;
  expression?: Array<unknown>;
  since?: string;
  defer?: Array<string>;
  defer_vcs?: boolean;
  relative_root?: string;
  empty_on_fresh_instance?: boolean;
};

export type WatchmanSubscription = {
  root: string;
  /**
   * The relative path from subscriptionRoot to subscriptionPath.
   * This is the 'relative_path' as described at
   * https://facebook.github.io/watchman/docs/cmd/watch-project.html#using-watch-project.
   * Notably, this value should be undefined if subscriptionRoot is the same as
   * subscriptionPath.
   */
  pathFromSubscriptionRootToSubscriptionPath: string | undefined;
  path: string;
  name: string;
  subscriptionCount: number;
  options: WatchmanSubscriptionOptions;
  emitter: EventEmitter;
};

export type WatchmanSubscriptionResponse = {
  root: string;
  subscription: string;
  files?: Array<FileChange>;
  'state-enter'?: string;
  'state-leave'?: string;
  canceled?: boolean;
  clock?: string;
  is_fresh_instance?: boolean;
};

export type FileChange = {
  name: string;
  new: boolean;
  exists: boolean;
};

const WATCHMAN_SETTLE_TIME_MS = 2500;
const DEFAULT_WATCHMAN_RECONNECT_DELAY_MS = 100;
const MAXIMUM_WATCHMAN_RECONNECT_DELAY_MS = 60 * 1000;

export class Watchman {
  private client: watchman.Client;

  private serializedReconnect: () => Promise<void>;
  private reconnectDelayMs: number = DEFAULT_WATCHMAN_RECONNECT_DELAY_MS;
  private subscriptions: Map<string, WatchmanSubscription> = new Map();
  private lastKnownClockTimes: Map<string, string> = new Map();

  public readonly status: 'initializing' | 'reconnecting' | 'healthy' | 'ended' | 'errored' =
    'initializing';

  constructor(
    private logger: Logger,
    private tracker: ServerSideTracker,
  ) {
    this.client = new watchman.Client({
      // find watchman using PATH
      watchmanBinaryPath: undefined,
    });
    this.initWatchmanClient();
    this.serializedReconnect = serializeAsyncCall(async () => {
      let tries = 0;
      while (true) {
        try {
          // eslint-disable-next-line no-await-in-loop
          await this.reconnectClient();
          return;
        } catch (error) {
          this.logger.warn(
            `reconnectClient failed (try #${tries}):`,
            error instanceof Error ? error.message : error,
          );
          tries++;

          this.reconnectDelayMs *= 2; // exponential backoff
          if (this.reconnectDelayMs > MAXIMUM_WATCHMAN_RECONNECT_DELAY_MS) {
            this.reconnectDelayMs = MAXIMUM_WATCHMAN_RECONNECT_DELAY_MS;
          }

          this.logger.info(
            'Calling reconnectClient from _serializedReconnect in %dms',
            this.reconnectDelayMs,
          );
          // eslint-disable-next-line no-await-in-loop
          await sleep(this.reconnectDelayMs);
        }
      }
    });
  }

  setStatus(status: typeof this.status): void {
    this.logger.info('Watchman status: ', status);
    (this.status as string) = status;
  }

  public async watchDirectoryRecursive(
    localDirectoryPath: string,
    rawSubscriptionName: string,
    subscriptionOptions?: WatchmanSubscriptionOptions,
  ): Promise<WatchmanSubscription> {
    // Subscriptions should be unique by name and by folder
    const subscriptionName = this.fixupName(localDirectoryPath, rawSubscriptionName);
    const existingSubscription = this.getSubscription(subscriptionName);
    if (existingSubscription) {
      existingSubscription.subscriptionCount++;

      return existingSubscription;
    } else {
      const {watch: watchRoot, relative_path: relativePath} =
        await this.watchProject(localDirectoryPath);
      const clock = await this.clock(watchRoot);
      const options: WatchmanSubscriptionOptions = {
        ...subscriptionOptions,
        // Do not add `mode` here, it is very unfriendly to watches on Eden (see https://fburl.com/0z023yy0)
        fields:
          subscriptionOptions != null && subscriptionOptions.fields != null
            ? subscriptionOptions.fields
            : ['name', 'new', 'exists'],
        since: clock,
      };
      if (relativePath) {
        options.relative_root = relativePath;
      }
      // Try this thing out where we always set empty_on_fresh_instance. Eden will be a lot happier
      // if we never ask Watchman to do something that results in a glob(**) near the root.
      options.empty_on_fresh_instance = true;

      // relativePath is undefined if watchRoot is the same as directoryPath.
      const subscription: WatchmanSubscription = {
        root: watchRoot,
        pathFromSubscriptionRootToSubscriptionPath: relativePath,
        path: localDirectoryPath,
        name: subscriptionName,
        subscriptionCount: 1,
        options,
        emitter: new EventEmitter(),
      };
      this.setSubscription(subscriptionName, subscription);
      await this.subscribe(watchRoot, subscriptionName, options);
      this.logger.info('watchman subscription %s established', subscriptionName);
      this.setStatus('healthy');

      return subscription;
    }
  }

  public async unwatch(path: string, name: string): Promise<void> {
    const subscriptionName = this.fixupName(path, name);
    const subscription = this.getSubscription(subscriptionName);

    if (subscription == null) {
      this.logger.error(`No watcher entity found with path [${path}] name [${name}]`);
      return;
    }

    if (--subscription.subscriptionCount === 0) {
      await this.unsubscribe(subscription.path, subscription.name).catch(err => {
        // Directory maybe doesn't exist anymore. Don't propagate error - we want to clean
        // up subscription anyway so we don't get stuck reconnecting.
        this.logger.error(`error unsubscribing watchman subscription ${subscriptionName}: ${err}`);
      });
      this.deleteSubscription(subscriptionName);
      this.logger.info(`watchman subscription destroyed: ${subscriptionName}`);
    }
  }

  private initWatchmanClient(): void {
    this.client.on('end', () => {
      this.setStatus('ended');
      this.logger.info('Watchman client ended');
      this.client.removeAllListeners();
      this.serializedReconnect();
    });
    this.client.on('error', (error: Error) => {
      const statusBeforeError = this.status;
      this.logger.error('Error while talking to watchman: ', error);
      this.tracker.error(
        'WatchmanEvent',
        'WatchmanError',
        `Error while talking to watchman ${error}`,
      );
      this.setStatus('errored');
      // If Watchman encounters an error in the middle of a command, it may never finish!
      // The client must be immediately killed here so that the command fails and
      // `serializeAsyncCall` can be unblocked. Otherwise, we end up in a deadlock.
      this.client.removeAllListeners();
      this.client.end();
      if (statusBeforeError === 'initializing') {
        // If we get an error while we're first initializing watchman, it probably means
        // it's not installed properly. No use spamming reconnection failures too.
        return;
      }
      // Those are errors in deserializing a stream of changes.
      // The only possible recovery here is reconnecting a new client,
      // but the failed to serialize events will be missed.
      this.serializedReconnect();
    });
    this.client.on('subscription', this.onSubscriptionResult.bind(this));
  }

  private async reconnectClient(): Promise<void> {
    // If we got an error after making a subscription, the reconnect needs to
    // remove that subscription to try again, so it doesn't keep leaking subscriptions.
    this.logger.info('Ending existing watchman client to reconnect a new one');
    this.setStatus('reconnecting');
    this.client.removeAllListeners();
    this.client.end();
    this.client = new watchman.Client({
      // find watchman using PATH
      watchmanBinaryPath: undefined,
    });
    this.logger.error('Watchman client disconnected, reconnecting a new client!');
    this.initWatchmanClient();
    this.logger.info('Watchman client re-initialized, restoring subscriptions');
    await this.restoreSubscriptions();
  }

  private async restoreSubscriptions(): Promise<void> {
    const watchSubscriptions = Array.from(this.subscriptions.values());
    const numSubscriptions = watchSubscriptions.length;
    this.logger.info(`Attempting to restore ${numSubscriptions} Watchman subscriptions.`);
    let numRestored = 0;
    await Promise.all(
      watchSubscriptions.map(async (subscription: WatchmanSubscription, index: number) => {
        // Note that this call to `watchman watch-project` could fail if the
        // subscription.path has been unmounted/deleted.
        await this.watchProject(subscription.path);

        // We have already missed the change events from the disconnect time,
        // watchman could have died, so the last clock result is not valid.
        await sleep(WATCHMAN_SETTLE_TIME_MS);

        // Register the subscriptions after the filesystem settles.
        const {name, options, root} = subscription;

        // Assuming we had previously connected and gotten an event, we can
        // reconnect `since` that time, so that we get any events we missed.
        subscription.options.since = this.lastKnownClockTimes.get(root) || (await this.clock(root));

        this.logger.info(`Subscribing to ${name}: (${index + 1}/${numSubscriptions})`);
        await this.subscribe(root, name, options);
        ++numRestored;
        this.logger.info(`Subscribed to ${name}: (${numRestored}/${numSubscriptions}) complete.`);
      }),
    );
    if (numRestored > 0 && numRestored === numSubscriptions) {
      this.logger.info('Successfully reconnected all %d subscriptions.', numRestored);
      // if everything got restored, reset the reconnect backoff time
      this.reconnectDelayMs = DEFAULT_WATCHMAN_RECONNECT_DELAY_MS;
      this.setStatus('healthy');
    }
  }

  private getSubscription(entryPath: string): WatchmanSubscription | undefined {
    return this.subscriptions.get(path.normalize(entryPath));
  }

  private setSubscription(entryPath: string, subscription: WatchmanSubscription): void {
    const key = path.normalize(entryPath);
    this.subscriptions.set(key, subscription);
  }

  private deleteSubscription(entryPath: string): void {
    const key = path.normalize(entryPath);
    this.subscriptions.delete(key);
  }

  private onSubscriptionResult(response: WatchmanSubscriptionResponse): void {
    const subscription = this.getSubscription(response.subscription);
    if (subscription == null) {
      this.logger.error('Subscription not found for response:!', response);
      return;
    }

    // save the clock time of this event in case we disconnect in the future
    if (response != null && response.root != null && response.clock != null) {
      this.lastKnownClockTimes.set(response.root, response.clock);
    }
    if (response.is_fresh_instance === true) {
      this.logger.warn(
        `Watch for ${response.root} (${response.subscription}) returned an empty fresh instance.`,
      );
      subscription.emitter.emit('fresh-instance');
    } else if (Array.isArray(response.files)) {
      subscription.emitter.emit('change', response.files);
    } else if (response.canceled === true) {
      this.logger.info(`Watch for ${response.root} was deleted: triggering a reconnect.`);
      // Ending the client will trigger a reconnect.
      this.client.end();
    } else {
      // Only log state transitions once per watchmanClient, since otherwise we'll just get 8x of this message spammed
      if (firstOfIterable(this.subscriptions.values()) === subscription) {
        // TODO(most): use state messages to decide on when to send updates.
        const stateEnter = response['state-enter'];
        const stateLeave = response['state-leave'];
        const stateMessage =
          stateEnter != null ? `Entering ${stateEnter}` : `Leaving ${stateLeave}`;
        const numSubscriptions = this.subscriptions.size;
        this.logger.info(`Subscription state: ${stateMessage} (${numSubscriptions})`);
      }
    }
  }

  private fixupName(path: string, name: string): string {
    const refinedPath = path.replace(/\\/g, '-').replace(/\//g, '-');
    return `${refinedPath}-${name}`;
  }

  private unsubscribe(subscriptionPath: string, subscriptionName: string): Promise<unknown> {
    return this.command('unsubscribe', subscriptionPath, subscriptionName);
  }

  private async watchProject(
    directoryPath: string,
  ): Promise<{watch: string; relative_path: string}> {
    const response = (await this.command('watch-project', directoryPath)) as {
      watch: string;
      relative_path: string;
      warning?: string;
    };
    if (response.warning) {
      this.logger.error('watchman warning: ', response.warning);
    }
    return response;
  }

  private async clock(directoryPath: string): Promise<string> {
    const {clock} = (await this.command('clock', directoryPath)) as {clock: string};
    return clock;
  }

  private subscribe(
    watchRoot: string,
    subscriptionName: string | undefined,
    options: WatchmanSubscriptionOptions,
  ): Promise<WatchmanSubscription> {
    this.logger.info(
      `Creating Watchman subscription ${String(subscriptionName)} under ${watchRoot}`,
      JSON.stringify(options),
    );
    return this.command(
      'subscribe',
      watchRoot,
      subscriptionName,
      options,
    ) as Promise<WatchmanSubscription>;
  }

  /**
   * Promisify calls to watchman client.
   */
  private async command(...args: Array<unknown>): Promise<unknown> {
    try {
      return await new Promise((resolve, reject) => {
        this.client.command(args, (error, response) => (error ? reject(error) : resolve(response)));
      });
    } catch (error) {
      this.logger.error('Watchman command error: ', args, error);
      throw error;
    }
  }
}
