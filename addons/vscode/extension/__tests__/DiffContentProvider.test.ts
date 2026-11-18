/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import type {Repository} from 'isl-server/src/Repository';
import type {ServerPlatform} from 'isl-server/src/serverPlatform';
import type {RepositoryContext} from 'isl-server/src/serverTypes';
import type {CommitInfo} from 'isl/src/types';

import {makeServerSideTracker} from 'isl-server/src/analytics/serverSideTracker';
import {
  beforeRevsetForComparison,
  ComparisonType,
  currRevsetForComparison,
} from 'shared/Comparison';
import {mockLogger} from 'shared/testUtils';
import {nullthrows} from 'shared/utils';
import * as vscode from 'vscode';
import {
  decodeSaplingDiffUri,
  encodeSaplingDiffUri,
  SaplingDiffContentProvider,
} from '../DiffContentProvider';

const mockCancelToken = {} as vscode.CancellationToken;

let activeReposCallback: undefined | ((repos: Array<Repository>) => unknown) = undefined;
let activeRepo: (Repository & {mockChangeHeadCommit: (commit: CommitInfo) => void}) | undefined;
jest.mock('isl-server/src/RepositoryCache', () => {
  return {
    repositoryCache: {
      onChangeActiveRepos(cb: (repos: Array<Repository>) => unknown) {
        activeReposCallback = cb;
        return () => (activeReposCallback = undefined);
      },
      cachedRepositoryForPath(path: string): Repository | undefined {
        if (path.startsWith('/path/to/repo')) {
          return activeRepo;
        }
        return undefined;
      },
    },
  };
});

const FILE1_CONTENT_HEAD = `
hello
world
`;

function mockRepoAdded(): NonNullable<typeof activeRepo> {
  let savedOnChangeHeadCommit: (commit: CommitInfo) => unknown;
  activeRepo = {
    // eslint-disable-next-line require-await
    cat: jest.fn(async (_ctx: RepositoryContext, file: string, rev: string) => {
      if (rev === '.' && file === '/path/to/repo/file1.txt') {
        return FILE1_CONTENT_HEAD;
      }
      throw new Error('unknown file');
    }),
    info: {
      command: 'sl',
      repoRoot: '/path/to/repo',
      dotdir: '/path/to/repo/.sl',
      remoteRepo: {type: 'unknown', path: ''},
      pullRequestDomain: undefined,
    },
    subscribeToHeadCommit: jest.fn().mockImplementation(cb => {
      savedOnChangeHeadCommit = cb;
      return {dispose: jest.fn()};
    }),
    mockChangeHeadCommit(commit: CommitInfo) {
      savedOnChangeHeadCommit(commit);
    },
    initialConnectionContext: {
      cwd: '/path/to/repo',
    },
  } as unknown as typeof activeRepo;
  activeReposCallback?.([nullthrows(activeRepo)]);
  return nullthrows(activeRepo);
}
function mockNoActiveRepo() {
  activeRepo = undefined;
  activeReposCallback?.([]);
}

const mockTracker = makeServerSideTracker(
  mockLogger,
  {platformName: 'test'} as ServerPlatform,
  '0.1',
  jest.fn(),
);

describe('DiffContentProvider', () => {
  let ctx: RepositoryContext;
  beforeEach(() => {
    ctx = {
      agentSessionMetadata: undefined,
      cwd: 'cwd',
      cmd: 'sl',
      logger: mockLogger,
      tracker: mockTracker,
    };
  });

  const encodedFile1 = encodeSaplingDiffUri(
    vscode.Uri.file('/path/to/repo/file1.txt'),
    beforeRevsetForComparison({type: ComparisonType.UncommittedChanges}),
  );

  it('provides file contents', async () => {
    const provider = new SaplingDiffContentProvider(ctx);

    const repo = mockRepoAdded();

    const content = await provider.provideTextDocumentContent(encodedFile1, mockCancelToken);

    expect(content).toEqual(FILE1_CONTENT_HEAD);
    expect(repo.cat).toHaveBeenCalledTimes(1);
    expect(repo.cat).toHaveBeenCalledWith({cwd: '/path/to/repo'}, '/path/to/repo/file1.txt', '.');
    provider.dispose();
  });

  it('caches file contents', async () => {
    const provider = new SaplingDiffContentProvider(ctx);
    const repo = mockRepoAdded();
    await provider.provideTextDocumentContent(encodedFile1, mockCancelToken);
    await provider.provideTextDocumentContent(encodedFile1, mockCancelToken);
    expect(repo.cat).toHaveBeenCalledTimes(1); // second call hits file content cache
    provider.dispose();
  });

  it('invalidates files when the repository head changes', async () => {
    const commit1 = {hash: '1'} as CommitInfo;
    const commit2 = {hash: '2'} as CommitInfo;
    const provider = new SaplingDiffContentProvider(ctx);
    const onChange = jest.fn();
    const onChangeDisposable = provider.onDidChange(onChange);
    const repo = mockRepoAdded();
    repo.mockChangeHeadCommit(commit1);
    await provider.provideTextDocumentContent(encodedFile1, mockCancelToken);
    // changing the head commit has no effect since we hadn't yet provided content
    expect(onChange).toHaveBeenCalledTimes(0);

    repo.mockChangeHeadCommit(commit2);
    await provider.provideTextDocumentContent(encodedFile1, mockCancelToken);
    // now the provider knows that encodedFile1 is an active file, which triggers onChange.
    expect(onChange).toHaveBeenCalledTimes(1);

    provider.dispose();
    onChangeDisposable.dispose();
  });

  it('invalidates file content cache when the repository head changes', async () => {
    const commit1 = {hash: '1'} as CommitInfo;
    const commit2 = {hash: '2'} as CommitInfo;
    const provider = new SaplingDiffContentProvider(ctx);
    const repo = mockRepoAdded();
    repo.mockChangeHeadCommit(commit1);

    await provider.provideTextDocumentContent(encodedFile1, mockCancelToken);
    expect(repo.cat).toHaveBeenCalledTimes(1);

    repo.mockChangeHeadCommit(commit2);
    await provider.provideTextDocumentContent(encodedFile1, mockCancelToken);
    expect(repo.cat).toHaveBeenCalledTimes(2);

    provider.dispose();
  });

  it('files opened before repo created provide content once repo is ready', async () => {
    mockNoActiveRepo();
    const provider = new SaplingDiffContentProvider(ctx);
    const onChange = jest.fn();
    const onChangeDisposable = provider.onDidChange(onChange);

    const contentBeforeRepo = await provider.provideTextDocumentContent(
      encodedFile1,
      mockCancelToken,
    );
    expect(contentBeforeRepo).toEqual(null);

    expect(onChange).not.toHaveBeenCalled();
    mockRepoAdded();
    // adding a repo triggers the content provider to tell vscode that the path changed...
    expect(onChange).toHaveBeenCalledWith(encodedFile1);

    // ...which means we re-run provideTextDocumentContent
    const contentAfterRepo = await provider.provideTextDocumentContent(
      encodedFile1,
      mockCancelToken,
    );
    expect(contentAfterRepo).toEqual(FILE1_CONTENT_HEAD);
    provider.dispose();
    onChangeDisposable.dispose();
  });

  it('closing a file disables telling vscode about file changes on checkout', async () => {
    let onCloseCallback: (e: vscode.TextDocument) => unknown = () => undefined;
    (vscode.workspace.onDidCloseTextDocument as jest.Mock).mockImplementation(cb => {
      onCloseCallback = cb;
      return {dispose: jest.fn()};
    });
    const commit1 = {hash: '1'} as CommitInfo;
    const commit2 = {hash: '2'} as CommitInfo;
    const provider = new SaplingDiffContentProvider(ctx);
    const onChange = jest.fn();
    const onChangeDisposable = provider.onDidChange(onChange);
    const repo = mockRepoAdded();
    await provider.provideTextDocumentContent(encodedFile1, mockCancelToken);
    expect(onChange).toHaveBeenCalledTimes(0);

    // normally if head changes, we detect it
    repo.mockChangeHeadCommit(commit1);
    expect(onChange).toHaveBeenCalledTimes(1);

    // closing any old file doesn't do anything, we still detect head commit changes
    onCloseCallback({
      uri: vscode.Uri.file('/some/unrelated/file'),
    } as unknown as vscode.TextDocument);
    repo.mockChangeHeadCommit(commit2);
    expect(onChange).toHaveBeenCalledTimes(2);

    // closing the encoded uri means we stop listening for changes
    onCloseCallback?.({uri: encodedFile1} as unknown as vscode.TextDocument);
    repo.mockChangeHeadCommit(commit2);
    expect(onChange).toHaveBeenCalledTimes(2); // no new call happened

    provider.dispose();
    onChangeDisposable.dispose();
  });

  it('closing a file, then updating the head commit removes the file content cache', async () => {
    let onCloseCallback: (e: vscode.TextDocument) => unknown = () => undefined;
    (vscode.workspace.onDidCloseTextDocument as jest.Mock).mockImplementation(cb => {
      onCloseCallback = cb;
      return {dispose: jest.fn()};
    });
    const commit2 = {hash: '2'} as CommitInfo;
    const provider = new SaplingDiffContentProvider(ctx);
    const repo = mockRepoAdded();
    await provider.provideTextDocumentContent(encodedFile1, mockCancelToken);
    expect(repo.cat).toHaveBeenCalledTimes(1);

    onCloseCallback?.({uri: encodedFile1} as unknown as vscode.TextDocument);

    await provider.provideTextDocumentContent(encodedFile1, mockCancelToken);
    expect(repo.cat).toHaveBeenCalledTimes(1); // file still cached

    onCloseCallback?.({uri: encodedFile1} as unknown as vscode.TextDocument);

    repo.mockChangeHeadCommit(commit2);
    await provider.provideTextDocumentContent(encodedFile1, mockCancelToken);
    expect(repo.cat).toHaveBeenCalledTimes(2); // file no longer cached

    provider.dispose();
  });
});

describe('SaplingDiffEncodedUri', () => {
  it('is reversible', () => {
    const encoded = encodeSaplingDiffUri(
      vscode.Uri.file('/path/to/myRepo'),
      currRevsetForComparison({
        type: ComparisonType.UncommittedChanges,
      }),
    );
    const decoded = decodeSaplingDiffUri(encoded);
    expect(decoded).toEqual({
      originalUri: expect.anything(),
      revset: 'wdir()',
    });
    expect(decoded.originalUri.toString()).toEqual('file:///path/to/myRepo');
  });
});
