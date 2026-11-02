/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import type {ParsedDiff} from 'shared/patch/types';
import type {HighlightedToken} from 'shared/textmate-lib/tokenize';

export type ThemeColor = 'light' | 'dark';

export type TokenizedHunk = Array<Array<HighlightedToken>>;
export type TokenizedDiffHunk = [before: TokenizedHunk, after: TokenizedHunk];
export type TokenizedDiffHunks = Array<TokenizedDiffHunk>;

export type SyntaxWorkerRequest =
  | {
      type: 'setBaseUri';
      base: string;
      /** Ask the main thread to fetch resources instead of fetching them in the worker. */
      fetchThroughHost?: boolean;
    }
  | {
      type: 'cancel';
      idToCancel: number;
    }
  | {
      type: 'fetchResourceResult';
      fetchId: number;
      result?: string | ArrayBuffer;
      error?: string;
    }
  | {
      type: 'tokenizeContents';
      theme: ThemeColor;
      path: string;
      content: Array<string>;
    }
  | {
      type: 'tokenizeHunks';
      theme: ThemeColor;
      path: string;
      hunks: ParsedDiff['hunks'];
      /**
       * Lines of the "before" file, starting at line 1, that come before the last hunk.
       * Lets each hunk start from the grammar state the lines above it leave, for example inside a multi-line string.
       */
      contextLinesBefore?: Array<string>;
    };

export type SyntaxWorkerResponse =
  | {
      type: 'cssColorMap';
      colorMap: string[];
    }
  | {
      type: 'fetchResource';
      fetchId: number;
      url: string;
      as: 'text' | 'arrayBuffer';
    }
  | {
      type: 'tokenizeContents';
      result: TokenizedHunk | undefined;
    }
  | {
      type: 'tokenizeHunks';
      result: TokenizedDiffHunks | undefined;
    };
