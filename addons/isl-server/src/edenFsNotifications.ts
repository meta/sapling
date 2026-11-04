/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import type {Logger} from './logger';

import path from 'node:path';
// Import from the generated files
import {
  EdenFSNotificationsClient,
  type EdenFSSubscription,
  type SubscriptionCallback,
  type SubscriptionOptions,
} from './__generated__/node-edenfs-notifications-client/index.js';

export type EdenFSSubscriptionManager = {
  path: string;
  name: string;
  subscriptionCount: number;
  internalSubscription: EdenFSSubscription;
};

/**
 * Provides directory watching functionality using EdenFS notify API.
 */
export class EdenFSNotifications {
  private client: EdenFSNotificationsClient;
  private subscriptions: Map<string, EdenFSSubscriptionManager> = new Map();

  constructor(
    private logger: Logger,
    private mountPoint: string,
  ) {
    this.client = new EdenFSNotificationsClient({
      mountPoint: this.mountPoint,
      timeout: 30000,
      edenBinaryPath: process.env.EDEN_PATH ? process.env.EDEN_PATH : 'eden',
    });
  }

  /**
   * Watch a directory recursively for changes using EdenFS notifications.
   */
  public async watchDirectoryRecursive(
    localDirectoryPath: string,
    rawSubscriptionName: string,
    subscriptionOptions: SubscriptionOptions,
    callback: SubscriptionCallback,
  ): Promise<EdenFSSubscription> {
    // Subscriptions should be unique by name and by folder
    const subscriptionName = this.fixupName(localDirectoryPath, rawSubscriptionName);
    const existingSubscription = this.getSubscription(subscriptionName);

    if (existingSubscription) {
      existingSubscription.subscriptionCount++;
      return existingSubscription.internalSubscription;
    }

    const internalSubscription = this.client.subscribe(subscriptionOptions, callback);

    const subscription: EdenFSSubscriptionManager = {
      path: localDirectoryPath,
      name: subscriptionName,
      subscriptionCount: 1,
      internalSubscription,
    };

    this.setSubscription(subscriptionName, subscription);
    await internalSubscription.start();
    this.logger.info(`edenfs subscription started: ${subscriptionName}`);
    return internalSubscription;
  }

  private getSubscription(entryPath: string): EdenFSSubscriptionManager | undefined {
    return this.subscriptions.get(path.normalize(entryPath));
  }

  private setSubscription(entryPath: string, subscription: EdenFSSubscriptionManager): void {
    const key = path.normalize(entryPath);
    this.subscriptions.set(key, subscription);
  }

  private deleteSubscription(entryPath: string): void {
    const key = path.normalize(entryPath);
    this.subscriptions.delete(key);
  }

  private fixupName(path: string, name: string): string {
    const refinedPath = path.replace(/\\/g, '-').replace(/\//g, '-');
    return `${refinedPath}-${name}`;
  }

  /**
   * Remove a subscription (unwatch)
   */
  public unwatch(path: string, name: string) {
    const subscriptionName = this.fixupName(path, name);
    const subscription = this.getSubscription(subscriptionName);

    if (subscription == null) {
      this.logger.error(`No watcher entity found with path [${path}] name [${name}]`);
      return;
    }

    if (--subscription.subscriptionCount === 0) {
      subscription.internalSubscription.stop();
      this.deleteSubscription(subscriptionName);
      this.logger.info(`edenfs subscription destroyed: ${subscriptionName}`);
    }
  }
}
