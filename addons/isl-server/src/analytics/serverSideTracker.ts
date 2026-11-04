/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import type {Repository} from '../Repository';
import type {Logger} from '../logger';
import type {ServerPlatform} from '../serverPlatform';
import type {ApplicationInfo, FullTrackData, TrackDataWithEventName} from './types';

import {Internal} from '../Internal';
import {generateAnalyticsInfo} from './environment';
import {Tracker} from './tracker';

export type ServerSideTracker = Tracker<ServerSideContext>;

class ServerSideContext {
  constructor(
    public logger: Logger,
    public data: ApplicationInfo,
  ) {}

  public setRepo(repo: Repository | undefined): void {
    this.data.repo = repo?.codeReviewProvider?.getSummaryName();
  }
}

const noOp = (_data: FullTrackData, _logger: Logger) => {
  /* In open source builds, analytics tracking is completely disabled/removed. */
};

/**
 * Creates a Tracker which includes server-side-only cached application data like platform, username, etc,
 * and sends data to the underlying analytics engine outside of ISL.
 * This can not be global since two client connections may have different cached data.
 */
export function makeServerSideTracker(
  logger: Logger,
  platform: ServerPlatform,
  version: string,
  // prettier-ignore
  writeToServer =
    Internal.trackToScribe ??
    noOp,
): ServerSideTracker {
  const analyticsInfo = generateAnalyticsInfo(platform.platformName, version, platform.sessionId);
  logger.info('Setup analytics, session: ', analyticsInfo.sessionId);
  return new Tracker(
    (data: TrackDataWithEventName, context: ServerSideContext) => {
      const {logger} = context;
      // log track event, since tracking events can be used as datapoints when reviewing logs
      logger.info(
        '[track]',
        data.eventName,
        data.errorName ?? '',
        data.extras != null ? JSON.stringify(data.extras) : '',
      );
      writeToServer({...data, ...context.data}, logger);
    },
    new ServerSideContext(logger, analyticsInfo),
  );
}
