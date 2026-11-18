/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import type {RepoInfo, RepositoryError, ValidatedRepoInfo} from 'isl/src/types';
import type {Repository} from '../Repository';
import type {Logger} from '../logger';
import type {ServerPlatform} from '../serverPlatform';
import type {RepositoryContext} from '../serverTypes';

import fs from 'node:fs';
import {ensureTrailingPathSep} from 'shared/pathUtils';
import {mockLogger} from 'shared/testUtils';
import {defer} from 'shared/utils';
import {__TEST__} from '../RepositoryCache';
import {makeServerSideTracker} from '../analytics/serverSideTracker';

const {RepositoryCache, RepoMap, RefCounted} = __TEST__;

const mockTracker = makeServerSideTracker(
  mockLogger,
  {platformName: 'test'} as ServerPlatform,
  '0.1',
  jest.fn(),
);

class SimpleMockRepositoryImpl {
  static getRepoInfo(ctx: RepositoryContext): Promise<RepoInfo> {
    const {cwd, cmd} = ctx;
    let data;
    if (cwd.includes('/path/to/repo')) {
      data = {
        repoRoot: '/path/to/repo',
        dotdir: '/path/to/repo/.sl',
      };
    } else if (cwd.includes('/path/to/anotherrepo')) {
      data = {
        repoRoot: '/path/to/anotherrepo',
        dotdir: '/path/to/anotherrepo/.sl',
      };
    } else if (cwd.includes('/path/to/submodule')) {
      data = {
        repoRoot: cwd.endsWith('/cwd') ? cwd.slice(0, -4) : cwd,
        dotdir: ensureTrailingPathSep(cwd) + '.sl',
      };
    } else {
      return Promise.resolve({type: 'cwdNotARepository', cwd} as RepositoryError);
    }
    return Promise.resolve({
      type: 'success',
      command: cmd,
      pullRequestDomain: undefined,
      preferredSubmitCommand: 'pr',
      codeReviewSystem: {type: 'unknown'},
      isEdenFs: false,
      ...data,
    });
  }
  constructor(
    public info: RepoInfo,
    public logger: Logger,
  ) {}

  dispose = jest.fn();
}
const SimpleMockRepository = SimpleMockRepositoryImpl as unknown as typeof Repository;

function mockRealpath(impl: (p: string) => string) {
  return jest
    .spyOn(fs, 'realpath')
    .mockImplementation(((p: string, cb: (err: null, resolved: string) => void) =>
      cb(null, impl(p))) as unknown as typeof fs.realpath);
}

const ctx: RepositoryContext = {
  agentSessionMetadata: undefined,
  cmd: 'sl',
  logger: mockLogger,
  tracker: mockTracker,
  cwd: '/path/to/repo/cwd',
};

describe('RepositoryCache', () => {
  it('Provides repository references that resolve', async () => {
    const cache = new RepositoryCache(SimpleMockRepository);
    const ref = cache.getOrCreate(ctx);

    const repo = await ref.promise;
    expect(repo).toEqual(
      expect.objectContaining({
        info: expect.objectContaining({
          repoRoot: '/path/to/repo',
          dotdir: '/path/to/repo/.sl',
        }),
      }),
    );

    ref.unref();
  });

  it('canonicalizes cwd via realpath when binding a context to a repo', async () => {
    // OnDemand: a symlinked cwd resolves to a different canonical path; canonicalizing it avoids the
    // downstream "... is not under root" abort.
    const logicalCwd = '/path/to/symlink/cwd';
    const canonicalCwd = '/path/to/repo/cwd';
    const realpathSpy = mockRealpath(p => (p === logicalCwd ? canonicalCwd : p));

    const cache = new RepositoryCache(SimpleMockRepository);
    const symlinkedCtx: RepositoryContext = {...ctx, cwd: logicalCwd};
    const ref = cache.getOrCreate(symlinkedCtx);

    const repo = await ref.promise;
    // getRepoInfo (mock) only recognizes the canonical path as a repo, so this proves cwd was
    // canonicalized before getRepoInfo ran.
    expect(repo).toEqual(
      expect.objectContaining({info: expect.objectContaining({repoRoot: '/path/to/repo'})}),
    );
    expect(symlinkedCtx.cwd).toBe(canonicalCwd);

    ref.unref();
    realpathSpy.mockRestore();
  });

  it('reuses the fast path for a symlinked cwd after caching its canonical path', async () => {
    const logicalCwd = '/path/to/symlink-root';
    const canonicalRoot = '/path/to/repo';
    const realpathSpy = mockRealpath(p => (p === logicalCwd ? canonicalRoot : p));

    const cache = new RepositoryCache(SimpleMockRepository);
    const ref1 = cache.getOrCreate({...ctx, cwd: logicalCwd});
    await ref1.promise;
    expect(realpathSpy).toHaveBeenCalledTimes(1);

    realpathSpy.mockClear();
    const ref2 = cache.getOrCreate({...ctx, cwd: logicalCwd});
    const repo2 = await ref2.promise;
    expect(realpathSpy).not.toHaveBeenCalled();
    expect(repo2).toEqual(
      expect.objectContaining({info: expect.objectContaining({repoRoot: canonicalRoot})}),
    );

    ref1.unref();
    ref2.unref();
    realpathSpy.mockRestore();
  });

  it('does not canonicalize cwd with the native realpath', async () => {
    // Native realpath uppercases Windows drive letters, which VS Code's `Uri.fsPath` does not.
    const nativeSpy = jest
      .spyOn(fs.promises, 'realpath')
      .mockImplementation((async (p: string) =>
        p.toUpperCase()) as unknown as typeof fs.promises.realpath);
    const realpathSpy = mockRealpath(p => p);

    const cache = new RepositoryCache(SimpleMockRepository);
    const cwdCtx: RepositoryContext = {...ctx, cwd: '/path/to/repo/cwd'};
    const ref = cache.getOrCreate(cwdCtx);
    await ref.promise;
    expect(cwdCtx.cwd).toBe('/path/to/repo/cwd');
    expect(nativeSpy).not.toHaveBeenCalled();

    ref.unref();
    nativeSpy.mockRestore();
    realpathSpy.mockRestore();
  });

  it('Gives error for paths without repos', async () => {
    const cache = new RepositoryCache(SimpleMockRepository);
    const ref = cache.getOrCreate({...ctx, cwd: '/some/invalid/repo'});

    const repo = await ref.promise;
    expect(repo).toEqual({type: 'cwdNotARepository', cwd: '/some/invalid/repo'});

    ref.unref();
  });

  it('Disposes repositories', async () => {
    const cache = new RepositoryCache(SimpleMockRepository);
    const ref = cache.getOrCreate(ctx);

    const repo = await ref.promise;
    const disposeFunc = (repo as Repository).dispose;

    expect(cache.numberOfActiveServers()).toBe(1);

    ref.unref();
    expect(disposeFunc).toHaveBeenCalledTimes(1);
    expect(cache.numberOfActiveServers()).toBe(0);
  });

  it('Can dispose references before the repo promise has resolved', async () => {
    const cache = new RepositoryCache(SimpleMockRepository);
    const ref = cache.getOrCreate(ctx);

    ref.unref();

    const repo = await ref.promise;
    // even though this would be a valid repo, by disposing the ref before it is created,
    // we prevent creating a repo.
    expect(repo).toEqual({type: 'unknownError', error: expect.anything()});
  });

  it('shares repositories under the same cwd', async () => {
    const cache = new RepositoryCache(SimpleMockRepository);
    const ref1 = cache.getOrCreate({...ctx, cwd: '/path/to/repo'});
    const repo1 = await ref1.promise;

    const ref2 = cache.getOrCreate({...ctx, cwd: '/path/to/repo'});
    const repo2 = await ref2.promise;

    expect(cache.numberOfActiveServers()).toBe(2);

    expect(repo1).toBe(repo2);

    ref1.unref();
    ref2.unref();
  });

  it('shares repositories under the same repo', async () => {
    const cache = new RepositoryCache(SimpleMockRepository);
    const ref1 = cache.getOrCreate({...ctx, cwd: '/path/to/repo/cwd1'});
    const repo1 = await ref1.promise;

    const ref2 = cache.getOrCreate({...ctx, cwd: '/path/to/repo/cwd2'});
    const repo2 = await ref2.promise;

    expect(cache.numberOfActiveServers()).toBe(2);

    expect(repo1).toBe(repo2);

    ref1.unref();
    ref2.unref();
  });

  it('does not share repositories under different cwds', async () => {
    const cache = new RepositoryCache(SimpleMockRepository);
    const ref1 = cache.getOrCreate({...ctx, cwd: '/path/to/repo/cwd'});
    const ref2 = cache.getOrCreate({...ctx, cwd: '/path/to/anotherrepo/cwd'});

    const repo1 = await ref1.promise;
    const repo2 = await ref2.promise;

    expect(repo1).not.toBe(repo2);

    ref1.unref();
    ref2.unref();
  });

  it('reference counts and disposes after 0 refs', async () => {
    const cache = new RepositoryCache(SimpleMockRepository);
    const ref1 = cache.getOrCreate({...ctx, cwd: '/path/to/repo/cwd1'});
    const ref2 = cache.getOrCreate({...ctx, cwd: '/path/to/repo/cwd2'});

    const repo = await ref1.promise;
    await ref2.promise;
    expect(cache.numberOfActiveServers()).toBe(2);

    const disposeFunc = (repo as Repository).dispose;

    ref1.unref();
    expect(disposeFunc).not.toHaveBeenCalled();
    ref2.unref();
    expect(disposeFunc).toHaveBeenCalledTimes(1);

    expect(cache.numberOfActiveServers()).toBe(0);
  });

  it('does not reuse disposed repos', async () => {
    const cache = new RepositoryCache(SimpleMockRepository);
    const ref1 = cache.getOrCreate({...ctx, cwd: '/path/to/repo/cwd1'});

    const repo1 = await ref1.promise;
    ref1.unref();

    const ref2 = cache.getOrCreate({...ctx, cwd: '/path/to/repo/cwd2'});
    const repo2 = await ref2.promise;

    expect(cache.numberOfActiveServers()).toBe(1);

    expect(repo1).not.toBe(repo2);
    expect((repo1 as Repository).dispose).toHaveBeenCalledTimes(1);
    expect((repo2 as Repository).dispose).not.toHaveBeenCalled();

    ref2.unref();
  });

  it('prefix matching repos are treated separately', async () => {
    const cache = new RepositoryCache(SimpleMockRepository);
    const ref1 = cache.getOrCreate({...ctx, cwd: '/path/to/repo'});
    await ref1.promise;

    expect(cache.cachedRepositoryForPath('/path/to/repo')).not.toEqual(undefined);
    expect(cache.cachedRepositoryForPath('/path/to/repo/')).not.toEqual(undefined);
    expect(cache.cachedRepositoryForPath('/path/to/repo/foo')).not.toEqual(undefined);
    // this is actually different repo
    expect(cache.cachedRepositoryForPath('/path/to/repo-1')).toEqual(undefined);

    ref1.unref();
    expect(cache.cachedRepositoryForPath('/path/to/repo')).toEqual(undefined);

    // Test longest prefix match for nested repos/submodules
    const refSubmodule = cache.getOrCreate({...ctx, cwd: '/path/to/submodule'});
    await refSubmodule.promise;
    const refSubmoduleNested = cache.getOrCreate({...ctx, cwd: '/path/to/submodule/nested'});
    await refSubmoduleNested.promise;

    const repoSubmodule = cache.cachedRepositoryForPath('/path/to/submodule');
    const repoSubmoduleFoo = cache.cachedRepositoryForPath('/path/to/submodule/foo');
    const repoNested = cache.cachedRepositoryForPath('/path/to/submodule/nested');
    const repoNestedBar = cache.cachedRepositoryForPath('/path/to/submodule/nested/bar');

    expect(repoSubmodule?.info.repoRoot).toEqual('/path/to/submodule');
    expect(repoSubmoduleFoo).toEqual(repoSubmodule);
    expect(repoNested?.info.repoRoot).toEqual('/path/to/submodule/nested');
    expect(repoNestedBar).toEqual(repoNested);

    refSubmoduleNested.unref();
    expect(cache.cachedRepositoryForPath('/path/to/submodule/nested')).toEqual(undefined);
    expect(cache.cachedRepositoryForPath('/path/to/submodule')).not.toEqual(undefined);

    refSubmodule.unref();
    expect(cache.cachedRepositoryForPath('/path/to/submodule')).toEqual(undefined);
  });

  it('only creates one Repository even when racing lookups', async () => {
    const repoInfo = {
      type: 'success',
      command: 'sl',
      pullRequestDomain: undefined,
      codeReviewSystem: {type: 'unknown'},
      repoRoot: '/path/to/repo',
      dotdir: '/path/to/repo/.sl',
      isEdenFs: false,
    } as RepoInfo;

    // two different fake async fetches for RepoInfo
    const p1 = defer<RepoInfo>();
    const p2 = defer<RepoInfo>();

    class BlockingMockRepository {
      static getRepoInfo(ctx: RepositoryContext): Promise<RepoInfo> {
        const {cwd} = ctx;
        if (cwd === '/path/to/repo/cwd1') {
          return p1.promise;
        } else if (cwd === '/path/to/repo/cwd2') {
          return p2.promise;
        }
        throw new Error('unknown repo');
      }
      constructor(
        public info: RepoInfo,
        public logger: Logger,
      ) {}

      dispose = jest.fn();
    }

    const cache = new RepositoryCache(BlockingMockRepository as unknown as typeof Repository);
    // start looking up repoInfos at the same time
    const ref1 = cache.getOrCreate({...ctx, cwd: '/path/to/repo/cwd1'});
    const ref2 = cache.getOrCreate({...ctx, cwd: '/path/to/repo/cwd2'});

    p2.resolve({...repoInfo});
    const repo2 = await ref2.promise;

    p1.resolve({...repoInfo});
    const repo1 = await ref1.promise;

    // we end up with the same repo
    expect(repo1).toBe(repo2);

    ref1.unref();
    ref2.unref();
  });
});

describe('RepoMap', () => {
  it('iteration with values() and forEach()', async () => {
    const repoNum = 10;
    const repoRoots = [];
    const promises = [];
    const createRefCountedRepo = async (ctx: RepositoryContext) => {
      const repoInfo = await SimpleMockRepository.getRepoInfo(ctx);
      const repo = new SimpleMockRepository(repoInfo as ValidatedRepoInfo, ctx);
      return new RefCounted(repo);
    };
    for (let i = 0; i < repoNum; i++) {
      repoRoots.push(`/path/to/submodule${i}`);
      promises.push(createRefCountedRepo({...ctx, cwd: `/path/to/submodule${i}`}));
    }
    const repos = await Promise.all(promises);

    const repoMap = new RepoMap();
    for (let i = 0; i < repoNum; i++) {
      repos[i].ref();
      repoMap.set(repoRoots[i], repos[i]);
    }

    const values = [...repoMap.values()];
    expect(values.length).toBe(repoNum);
    for (let i = 0; i < repoNum; i++) {
      expect(values[i].value.info.repoRoot).toBe(repoRoots[i]);
    }

    repoMap.forEach(repo => expect(repo.getNumberOfReferences()).toBe(1));
    repoMap.forEach(repo => repo.dispose());
    repoMap.forEach(repo => expect(repo.getNumberOfReferences()).toBe(0));
  });

  it('longest prefix match', async () => {
    const createRefCountedRepo = async (ctx: RepositoryContext) => {
      const repoInfo = await SimpleMockRepository.getRepoInfo(ctx);
      const repo = new SimpleMockRepository(repoInfo as ValidatedRepoInfo, ctx);
      return new RefCounted(repo);
    };
    const a = await createRefCountedRepo({...ctx, cwd: '/path/to/submoduleA/cwd'});
    const b = await createRefCountedRepo({...ctx, cwd: '/path/to/submoduleB/cwd'});
    const nested = await createRefCountedRepo({
      ...ctx,
      cwd: '/path/to/submoduleA/submoduleNested/cwd',
    });

    const repoMap = new RepoMap();
    // A raw map would iterate in insertion order, so we
    // call set in reverse order to test longest prefix match
    repoMap.set('/path/to/submoduleA/submoduleNested', nested);
    repoMap.set('/path/to/submoduleB', b);
    repoMap.set('/path/to/submoduleA', a);

    expect(repoMap.get('/path/to/submoduleA')).toBe(a);
    expect(repoMap.get('/path/to/submoduleA/some/dir')).toBeUndefined();
    expect(repoMap.getLongestPrefixMatch('/path/to/submoduleA/some/dir')).toBe(a);
    expect(repoMap.getLongestPrefixMatch('/path/to/submoduleB/some/dir')).toBe(b);
    expect(repoMap.getLongestPrefixMatch('/path/to/submoduleA/submoduleNested/dir')).toBe(nested);
  });
});
