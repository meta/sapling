/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import type {ParsedDiff} from 'shared/patch/types';
import type {
  SyntaxWorkerRequest,
  SyntaxWorkerResponse,
  TokenizedDiffHunks,
  TokenizedHunk,
} from './syntaxHighlightingTypes';

import {useAtomValue} from 'jotai';
import {useEffect, useState} from 'react';
import {CancellationToken} from 'shared/CancellationToken';
import {updateTextMateGrammarCSS} from 'shared/textmate-lib/textmateStyles';
import {isVscode} from '../../environment';
import {themeState} from '../../theme';
// Vite generates this module at build time, so the import plugin can't see its default export.
// eslint-disable-next-line import/default
import syntaxHighlightingWorkerUrl from './syntaxHighlightingWorker?worker&url';
import {SynchronousWorker, WorkerApi} from './workerApi';

// Syntax highlighting is done in a WebWorker. This file contains APIs
// to be called from the main thread, which are delegated to the worker.
// In some environments, WebWorker is not available. In that case,
// we fall back to a synchronous worker.

// Useful for testing the non-WebWorker implementation
const forceDisableWorkers = false;

let cachedWorkerPromise: Promise<WorkerApi<SyntaxWorkerRequest, SyntaxWorkerResponse>>;
function getWorker(): Promise<WorkerApi<SyntaxWorkerRequest, SyntaxWorkerResponse>> {
  if (cachedWorkerPromise) {
    return cachedWorkerPromise;
  }
  cachedWorkerPromise = (async () => {
    let worker: WorkerApi<SyntaxWorkerRequest, SyntaxWorkerResponse>;
    let fetchThroughHost = false;
    if (isVscode()) {
      if (process.env.NODE_ENV === 'development') {
        // NOTE: when using vscode in dev mode, because the web worker is not compiled to a single file,
        // the webview can't use it properly.
        // Fall back to a synchronous worker (note that this may have perf issues)
        worker = new WorkerApi(
          new SynchronousWorker(() => import('./syntaxHighlightingWorker')) as unknown as Worker,
        );
      } else {
        // Production vscode build: webworkers in vscode webviews
        // are very particular and can only be loaded via blob: URL.
        const blobUrl = await fetch(syntaxHighlightingWorkerUrl)
          .then(r => r.blob())
          .then(b => URL.createObjectURL(b));

        try {
          worker = new WorkerApi(new Worker(blobUrl));
        } finally {
          URL.revokeObjectURL(blobUrl);
        }
        // VS Code's webview service worker never answers resource requests from a blob worker,
        // so the worker can't fetch its wasm or grammars itself.
        fetchThroughHost = true;
      }
    } else if (window.Worker && !forceDisableWorkers) {
      // Non-vscode environments: web workers should work normally
      worker = new WorkerApi(
        new Worker(new URL('./syntaxHighlightingWorker', import.meta.url), {type: 'module'}),
      );
    } else if (process.env.NODE_ENV !== 'production') {
      // Tests (jsdom) have no WebWorker. Gated on NODE_ENV so production builds don't
      // emit a second, non-worker copy of syntaxHighlightingWorker just for this fallback.
      worker = new WorkerApi(
        new SynchronousWorker(() => import('./syntaxHighlightingWorker')) as unknown as Worker,
      );
    } else {
      throw new Error('WebWorkers are not available; syntax highlighting is disabled');
    }

    // Explicitly set the base URI so the worker can make fetch requests.
    worker.worker.postMessage({
      type: 'setBaseUri',
      base: document.baseURI,
      fetchThroughHost,
    } as SyntaxWorkerRequest);

    if (fetchThroughHost) {
      worker.listen('fetchResource', async ({fetchId, url, as}) => {
        try {
          const response = await fetch(url);
          if (!response.ok) {
            throw new Error(`Failed to fetch ${url}: ${response.status}`);
          }
          const result = as === 'text' ? await response.text() : await response.arrayBuffer();
          worker.worker.postMessage({type: 'fetchResourceResult', fetchId, result});
        } catch (err) {
          worker.worker.postMessage({type: 'fetchResourceResult', fetchId, error: String(err)});
        }
      });
    }

    worker.listen('cssColorMap', msg => {
      // During testing-library tear down (ex. syntax highlighting was canceled),
      // `document` may be null. Abort here to avoid errors.
      if (document == null) {
        return undefined;
      }
      updateTextMateGrammarCSS(msg.colorMap);
    });

    return worker;
  })();
  return cachedWorkerPromise;
}

/**
 * Given a set of hunks from a diff view,
 * asynchronously provide syntax highlighting by tokenizing.
 *
 * Note that we reconstruct the file contents from the diff. Without `contextLinesBefore`,
 * syntax highlighting can be inaccurate since it's missing the lines above each hunk.
 */
export function useTokenizedHunks(
  path: string,
  hunks: ParsedDiff['hunks'],
  contextLinesBefore?: Array<string>,
): TokenizedDiffHunks | undefined {
  const theme = useAtomValue(themeState);

  const [tokenized, setTokenized] = useState<TokenizedDiffHunks | undefined>(undefined);

  useEffect(() => {
    const token = newTrackedCancellationToken();
    getWorker().then(worker =>
      worker
        .request({type: 'tokenizeHunks', theme, path, hunks, contextLinesBefore}, token)
        .then(result => {
          if (!token.isCancelled) {
            setTokenized(result.result);
          }
        }),
    );
    return () => token.cancel();
  }, [theme, path, hunks, contextLinesBefore]);
  return tokenized;
}

/**
 * Given a chunk of a file as an array of lines, asynchronously provide syntax highlighting by tokenizing.
 */
export function useTokenizedContents(
  path: string,
  content: Array<string> | undefined,
): TokenizedHunk | undefined {
  const theme = useAtomValue(themeState);

  const [tokenized, setTokenized] = useState<TokenizedHunk | undefined>(undefined);

  useEffect(() => {
    if (content == null) {
      return;
    }
    const token = newTrackedCancellationToken();
    getWorker().then(worker =>
      worker.request({type: 'tokenizeContents', theme, path, content}, token).then(result => {
        if (!token.isCancelled) {
          setTokenized(result.result);
        }
      }),
    );
    return () => token.cancel();
  }, [theme, path, content]);
  return tokenized;
}

/**
 * Given file content of a change before & after, return syntax highlighted versions of those changes.
 * Also takes a parent HTML Element. Sets up an interaction observer to only try syntax highlighting once
 * the container is visible.
 * Note: if parsing contentBefore/After in the caller, it's easy for these to change each render, causing
 * an infinite loop. Memoize contentBefore/contentAfter from the string content in the caller to avoid this.
 */
export function useTokenizedContentsOnceVisible(
  path: string,
  contentBefore: Array<string> | undefined,
  contentAfter: Array<string> | undefined,
  parentNode: React.MutableRefObject<HTMLElement | null>,
): [TokenizedHunk, TokenizedHunk] | undefined {
  const theme = useAtomValue(themeState);
  const [tokenized, setTokenized] = useState<[TokenizedHunk, TokenizedHunk] | undefined>(undefined);
  const [hasBeenVisible, setHasBeenVisible] = useState(false);

  useEffect(() => {
    if (hasBeenVisible || parentNode.current == null) {
      // no need to start observing again after we've been visible.
      return;
    }
    const observer = new IntersectionObserver((entries, observer) => {
      entries.forEach(entry => {
        if (entry.intersectionRatio > 0) {
          setHasBeenVisible(true);
          // no need to keep observing once we've been visible once and computed the highlights.
          observer.disconnect();
        }
      });
    }, {});
    observer.observe(parentNode.current);
    return () => observer.disconnect();
  }, [parentNode, hasBeenVisible]);

  useEffect(() => {
    if (!hasBeenVisible || contentBefore == null || contentAfter == null) {
      return;
    }
    const token = newTrackedCancellationToken();

    Promise.all([
      getWorker().then(worker =>
        worker.request({type: 'tokenizeContents', theme, path, content: contentBefore}, token),
      ),
      getWorker().then(worker =>
        worker.request({type: 'tokenizeContents', theme, path, content: contentAfter}, token),
      ),
    ]).then(([a, b]) => {
      if (a?.result == null || b?.result == null) {
        return;
      }
      if (!token.isCancelled) {
        setTokenized([a.result, b.result]);
      }
    });
    return () => token.cancel();
  }, [hasBeenVisible, theme, path, contentBefore, contentAfter]);
  return tokenized?.[0].length === contentBefore?.length &&
    tokenized?.[1].length === contentAfter?.length
    ? tokenized
    : undefined;
}

/** Track the `CancellationToken`s so they can be cancelled immediately in tests. */
const cancellationTokens: Set<CancellationToken> = new Set();

/**
 * Cancel all syntax highlighting tasks immediately. This is useful in tests
 * that do not wait for the highlighting to complete and want to avoid the
 * React "act" warning.
 */
export function cancelAllHighlightingTasks() {
  cancellationTokens.forEach(token => token.cancel());
  cancellationTokens.clear();
}

function newTrackedCancellationToken(): CancellationToken {
  const token = new CancellationToken();
  cancellationTokens.add(token);
  token.onCancel(() => cancellationTokens.delete(token));
  return token;
}
