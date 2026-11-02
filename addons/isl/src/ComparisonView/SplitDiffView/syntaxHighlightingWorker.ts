/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import type {ParsedDiff} from 'shared/patch/types';
import type {IGrammar} from 'vscode-textmate';
import type {ThemeColor} from '../../theme';
import type {
  SyntaxWorkerRequest,
  SyntaxWorkerResponse,
  TokenizedDiffHunks,
  TokenizedHunk,
} from './syntaxHighlightingTypes';

import {CancellationToken} from 'shared/CancellationToken';
import FilepathClassifier from 'shared/textmate-lib/FilepathClassifier';
import {tokenizeLines, tokenizeLinesFromState} from 'shared/textmate-lib/tokenize';
import {loadWASM} from 'vscode-oniguruma';
import {INITIAL} from 'vscode-textmate';
import {grammars, languages} from '../../generated/textmate/TextMateGrammarManifest';
import {getGrammar, getGrammarStore} from './grammar';

const URL_TO_ONIG_WASM = './generated/textmate/onig.wasm';

/* This file is intended to be executed in a WebWorker, without access to the DOM. */

/**
 * Fetch requests inside the webworker must be made relative to the base URI.
 * By executing the web worker via a blob: URL, we can't depend on the base being inherited.
 */
let globalBaseUri: string;

/**
 * VS Code's webview service worker doesn't serve resource requests made from a WebWorker,
 * so in VS Code webviews the main thread fetches resources on our behalf.
 */
let fetchThroughHost = false;
let postMessageToHost: ((msg: SyntaxWorkerResponse) => unknown) | undefined;
let nextFetchId = 0;
const pendingFetches = new Map<
  number,
  {resolve: (result: string | ArrayBuffer) => void; reject: (error: Error) => void}
>();

function fetchFromHost(url: URL, as: 'text'): Promise<string>;
function fetchFromHost(url: URL, as: 'arrayBuffer'): Promise<ArrayBuffer>;
function fetchFromHost(url: URL, as: 'text' | 'arrayBuffer'): Promise<string | ArrayBuffer> {
  const fetchId = nextFetchId++;
  return new Promise((resolve, reject) => {
    pendingFetches.set(fetchId, {resolve, reject});
    postMessageToHost?.({type: 'fetchResource', fetchId, url: url.href, as});
  });
}

function fetchText(url: URL): Promise<string> {
  return fetchThroughHost ? fetchFromHost(url, 'text') : fetch(url).then(r => r.text());
}

async function loadGrammar(
  theme: ThemeColor,
  path: string,
  postMessage: (msg: SyntaxWorkerResponse) => void,
): Promise<IGrammar | undefined> {
  await ensureOnigurumaIsLoaded(globalBaseUri);

  const scopeName = getFilepathClassifier().findScopeNameForPath(path);
  if (!scopeName) {
    return undefined;
  }

  const store = getGrammarStore(theme, globalBaseUri, fetchText, colorMap => {
    // tell client the newest colorMap
    postMessage({type: 'cssColorMap', colorMap} as SyntaxWorkerResponse);
  });

  const grammar = await getGrammar(store, scopeName);
  return grammar ?? undefined;
}

const cancellationTokenForId = new Map<number, CancellationToken>();

class WorkQueue {
  private queue: Array<() => Promise<void>> = [];
  private isProcessing = false;

  public push(work: () => Promise<void>) {
    this.queue.push(work);

    if (!this.isProcessing) {
      this.processNext();
    }
  }

  private async processNext() {
    if (this.queue.length > 0) {
      const work = this.queue.shift();
      this.isProcessing = true;
      // Allow the task queue to be emptied before continuing,
      // so we can process cancel messages
      await new Promise(res => setTimeout(res, 0));
      await work?.().catch(err => {
        // eslint-disable-next-line no-console
        console.error(err);
        return null;
      });
      this.isProcessing = false;
      this.processNext();
    }
  }
}

const workQueue = new WorkQueue();

export function handleMessage(
  postMessage: (msg: SyntaxWorkerResponse & {id?: number}) => unknown,
  event: MessageEvent,
) {
  const data = event.data as SyntaxWorkerRequest & {id: number};

  const token = new CancellationToken();
  if (data.id != null) {
    cancellationTokenForId.set(data.id, token);
  }
  switch (data.type) {
    case 'setBaseUri': {
      globalBaseUri = data.base;
      fetchThroughHost = data.fetchThroughHost ?? false;
      postMessageToHost = postMessage;
      break;
    }
    case 'fetchResourceResult': {
      const pending = pendingFetches.get(data.fetchId);
      pendingFetches.delete(data.fetchId);
      if (data.result != null) {
        pending?.resolve(data.result);
      } else {
        pending?.reject(new Error(data.error ?? 'Failed to fetch resource'));
      }
      break;
    }
    case 'tokenizeContents': {
      workQueue.push(async () => {
        const grammar = await loadGrammar(data.theme, data.path, postMessage);
        const result = tokenizeContent(grammar, data.content, token);
        postMessage({type: data.type, id: data.id, result});
        cancellationTokenForId.delete(data.id);
      });
      break;
    }
    case 'tokenizeHunks': {
      workQueue.push(async () => {
        const grammar = await loadGrammar(data.theme, data.path, postMessage);
        const result = tokenizeHunks(grammar, data.hunks, data.contextLinesBefore, token);
        postMessage({type: data.type, id: data.id, result});
        cancellationTokenForId.delete(data.id);
      });
      break;
    }
    case 'cancel': {
      const token = cancellationTokenForId.get(data.idToCancel);
      token?.cancel();
    }
  }
}

if (typeof self.document === 'undefined') {
  // inside WebWorker, use global onmessage and postMessage
  onmessage = handleMessage.bind(undefined, postMessage);
  // outside of a WebWorker, the exported `handleMessage` function should be used instead.
}

function tokenizeHunks(
  grammar: IGrammar | undefined,
  hunks: ParsedDiff['hunks'],
  contextLinesBefore: Array<string> | undefined,
  cancellationToken: CancellationToken,
): TokenizedDiffHunks | undefined {
  if (grammar == null) {
    return undefined;
  }

  if (cancellationToken.isCancelled) {
    // check for cancellation before doing expensive highlighting
    return undefined;
  }

  if (contextLinesBefore == null) {
    return hunks
      .map(hunk => recoverFileContentsFromPatchLines(hunk.lines))
      .map(([before, after]) => [tokenizeLines(before, grammar), tokenizeLines(after, grammar)]);
  }

  let beforeState = INITIAL;
  let afterState = INITIAL;
  // 1-indexed line number in the "before" file of the first line not yet tokenized.
  let nextLine = 1;
  return hunks.map(hunk => {
    // A hunk that only adds lines comes after line `oldStart`, rather than starting at it.
    const hunkStart = hunk.oldLines === 0 ? hunk.oldStart + 1 : hunk.oldStart;
    // Lines between hunks are unchanged, so they are the same on both sides.
    const unchangedLines = contextLinesBefore.slice(nextLine - 1, hunkStart - 1);
    beforeState = tokenizeLinesFromState(unchangedLines, grammar, beforeState).state;
    afterState = tokenizeLinesFromState(unchangedLines, grammar, afterState).state;
    nextLine = hunkStart + hunk.oldLines;

    const [before, after] = recoverFileContentsFromPatchLines(hunk.lines);
    const tokenizedBefore = tokenizeLinesFromState(before, grammar, beforeState);
    const tokenizedAfter = tokenizeLinesFromState(after, grammar, afterState);
    beforeState = tokenizedBefore.state;
    afterState = tokenizedAfter.state;
    return [tokenizedBefore.tokens, tokenizedAfter.tokens];
  });
}

function tokenizeContent(
  grammar: IGrammar | undefined,
  content: Array<string>,
  cancellationToken: CancellationToken,
): TokenizedHunk | undefined {
  if (grammar == null) {
    return undefined;
  }

  if (cancellationToken.isCancelled) {
    // check for cancellation before doing expensive highlighting
    return undefined;
  }

  return tokenizeLines(content, grammar);
}

/**
 * Patch lines start with ' ', '+', or '-'. From this we can reconstruct before & after file contents as strings,
 * which we can actually use in the syntax highlighting.
 */
function recoverFileContentsFromPatchLines(
  lines: Array<string>,
): [before: Array<string>, after: Array<string>] {
  const linesBefore = [];
  const linesAfter = [];
  for (const line of lines) {
    if (line[0] === ' ') {
      linesBefore.push(line.slice(1));
      linesAfter.push(line.slice(1));
    } else if (line[0] === '+') {
      linesAfter.push(line.slice(1));
    } else if (line[0] === '-') {
      linesBefore.push(line.slice(1));
    }
  }

  return [linesBefore, linesAfter];
}

let onigurumaLoadingJob: Promise<void> | null = null;
function ensureOnigurumaIsLoaded(base: string): Promise<void> {
  if (onigurumaLoadingJob === null) {
    onigurumaLoadingJob = loadOniguruma(base);
  }
  return onigurumaLoadingJob;
}

async function loadOniguruma(base: string): Promise<void> {
  const url = new URL(URL_TO_ONIG_WASM, base);
  if (fetchThroughHost) {
    await loadWASM({data: await fetchFromHost(url, 'arrayBuffer')});
    return;
  }
  const onigurumaWASMRequest = fetch(url);
  const response = await onigurumaWASMRequest;

  const contentType = response.headers.get('content-type');
  const useStreamingParser = contentType === 'application/wasm';

  if (useStreamingParser) {
    await loadWASM(response);
  } else {
    const dataOrOptions = {
      data: await response.arrayBuffer(),
    };
    await loadWASM(dataOrOptions);
  }
}

let _classifier: FilepathClassifier | null = null;

function getFilepathClassifier(): FilepathClassifier {
  if (_classifier == null) {
    _classifier = new FilepathClassifier(grammars, languages);
  }
  return _classifier;
}
