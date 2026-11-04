/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import type {ServerPlatform} from '../src/serverPlatform';

export const platform: ServerPlatform = {
  platformName: 'obsidian',

  async handleMessageFromClient(this: ServerPlatform, repo, _ctx, message, _postMessage) {
    switch (message.type) {
      case 'platform/openFile': {
        // For Obsidian, file opening is handled client-side via postMessage
        // Log for debugging purposes
        repo?.initialConnectionContext.logger.info(
          'Obsidian platform: openFile request (handled client-side)',
          message.path,
        );
        break;
      }
      case 'platform/openFiles': {
        repo?.initialConnectionContext.logger.info(
          'Obsidian platform: openFiles request (handled client-side)',
          message.paths,
        );
        break;
      }
      case 'platform/openExternal': {
        repo?.initialConnectionContext.logger.info(
          'Obsidian platform: openExternal request (handled client-side)',
          message.url,
        );
        break;
      }
      // Other platform messages handled by default behavior
    }
  },
};
