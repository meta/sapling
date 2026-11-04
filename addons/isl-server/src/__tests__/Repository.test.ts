/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import {
  CommandRunner,
  type AbsolutePath,
  type MergeConflicts,
  type RunnableOperation,
  type Submodule,
  type ValidatedRepoInfo,
  type WorktreeInfo,
} from 'isl/src/types';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import * as ejeca from 'shared/ejeca';
import * as fsUtils from 'shared/fs';
import {clone, mockLogger} from 'shared/testUtils';
import {nextTick} from 'shared/utils';
import {
  absolutePathForFileInRepo,
  isIllegalOperationFlag,
  Repository,
  validateMergeToolArgs,
} from '../Repository';
import {makeServerSideTracker} from '../analytics/serverSideTracker';
import {
  extractRepoInfoFromUrl,
  setConfigOverrideForTests,
  type ResolveCommandConflictOutput,
} from '../commands';
import {ErrorShortMessages} from '../constants';
import {type ServerPlatform} from '../serverPlatform';
import {type RepositoryContext} from '../serverTypes';

/* eslint-disable require-await */

jest.mock('../WatchForChanges', () => {
  class MockWatchForChanges {
    dispose = jest.fn();
    poll = jest.fn();
  }
  return {WatchForChanges: MockWatchForChanges};
});

const mockTracker = makeServerSideTracker(
  mockLogger,
  {platformName: 'test'} as ServerPlatform,
  '0.1',
  jest.fn(),
);

function mockEjeca(
  cmds: Array<
    [
      RegExp,
      (() => {stdout: string; stderr?: string} | Error) | {stdout: string; stderr?: string} | Error,
    ]
  >,
) {
  return jest.spyOn(ejeca, 'ejeca').mockImplementation(((cmd: string, args: Array<string>) => {
    const argStr = cmd + ' ' + args?.join(' ');
    const ejecaOther = {
      kill: jest.fn(),
      on: jest.fn((event, cb) => {
        // immediately call exit cb to teardown timeout
        if (event === 'exit') {
          cb();
        }
      }),
    };
    for (const [regex, output] of cmds) {
      if (regex.test(argStr)) {
        let value = output;
        if (typeof output === 'function') {
          value = output();
        }
        if (value instanceof Error) {
          throw value;
        }
        return {...ejecaOther, ...value};
      }
    }
    return {...ejecaOther, stdout: ''};
  }) as unknown as typeof ejeca.ejeca);
}

function processExitError(code: number, message: string): ejeca.EjecaError {
  const err = new Error(message) as ejeca.EjecaError;
  err.exitCode = code;
  return err;
}

function setPathsDefault(path: string) {
  setConfigOverrideForTests([['paths.default', path]], false);
}

describe('Repository', () => {
  let ctx: RepositoryContext;
  beforeEach(() => {
    ctx = {
      cmd: 'sl',
      cwd: '/path/to/cwd',
      logger: mockLogger,
      tracker: mockTracker,
    };
  });

  it('setting command name', async () => {
    const ejecaSpy = mockEjeca([]);
    await Repository.getRepoInfo({...ctx, cmd: 'slb'});
    expect(ejecaSpy).toHaveBeenCalledWith(
      'slb',
      expect.arrayContaining(['root']),
      expect.anything(),
    );
  });

  describe('extracting github repo info', () => {
    beforeEach(() => {
      setConfigOverrideForTests([['github.pull_request_domain', 'github.com']]);
      mockEjeca([
        [/^sl root --dotdir/, {stdout: '/path/to/myRepo/.sl'}],
        [/^sl root/, {stdout: '/path/to/myRepo'}],
        [/^sl debugroots/, {stdout: '/path/to/myRepo'}],
        [
          /^gh auth status --hostname gitlab.myCompany.com/,
          new Error('not authenticated on this hostname'),
        ],
        [/^gh auth status --hostname ghe.myCompany.com/, {stdout: ''}],
      ]);
    });

    it('extracting github repo info', async () => {
      setPathsDefault('https://github.com/myUsername/myRepo.git');
      const info = (await Repository.getRepoInfo(ctx)) as ValidatedRepoInfo;
      const repo = new Repository(info, ctx);
      expect(repo.info).toEqual({
        type: 'success',
        command: 'sl',
        repoRoot: '/path/to/myRepo',
        repoRoots: ['/path/to/myRepo'],
        dotdir: '/path/to/myRepo/.sl',
        codeReviewSystem: {
          type: 'github',
          owner: 'myUsername',
          repo: 'myRepo',
          hostname: 'github.com',
        },
        pullRequestDomain: 'github.com',
        preferredSubmitCommand: undefined,
        isEdenFs: false,
      });
    });

    it('extracting github enterprise repo info', async () => {
      setPathsDefault('https://ghe.myCompany.com/myUsername/myRepo.git');
      const info = (await Repository.getRepoInfo(ctx)) as ValidatedRepoInfo;
      const repo = new Repository(info, ctx);
      expect(repo.info).toEqual({
        type: 'success',
        command: 'sl',
        repoRoot: '/path/to/myRepo',
        repoRoots: ['/path/to/myRepo'],
        dotdir: '/path/to/myRepo/.sl',
        codeReviewSystem: {
          type: 'github',
          owner: 'myUsername',
          repo: 'myRepo',
          hostname: 'ghe.myCompany.com',
        },
        pullRequestDomain: 'github.com',
        preferredSubmitCommand: undefined,
        isEdenFs: false,
      });
    });

    it('prefers the upstream repo over the fork it was cloned from', async () => {
      setConfigOverrideForTests(
        [
          ['paths.default', 'https://github.com/myUsername/myRepo.git'],
          ['paths.upstream', 'https://github.com/upstreamOrg/myRepo.git'],
        ],
        false,
      );
      const info = (await Repository.getRepoInfo(ctx)) as ValidatedRepoInfo;
      expect(info.codeReviewSystem).toEqual({
        type: 'github',
        owner: 'upstreamOrg',
        repo: 'myRepo',
        hostname: 'github.com',
      });
    });

    it('falls back to the default path when upstream is not a github repo', async () => {
      setConfigOverrideForTests(
        [
          ['paths.default', 'https://github.com/myUsername/myRepo.git'],
          ['paths.upstream', 'https://gitlab.myCompany.com/myUsername/myRepo.git'],
        ],
        false,
      );
      const info = (await Repository.getRepoInfo(ctx)) as ValidatedRepoInfo;
      expect(info.codeReviewSystem).toEqual({
        type: 'github',
        owner: 'myUsername',
        repo: 'myRepo',
        hostname: 'github.com',
      });
    });

    it('handles non-github-enterprise unknown code review providers', async () => {
      setPathsDefault('https://gitlab.myCompany.com/myUsername/myRepo.git');
      const info = (await Repository.getRepoInfo(ctx)) as ValidatedRepoInfo;
      const repo = new Repository(info, ctx);
      expect(repo.info).toEqual({
        type: 'success',
        command: 'sl',
        repoRoot: '/path/to/myRepo',
        repoRoots: ['/path/to/myRepo'],
        dotdir: '/path/to/myRepo/.sl',
        codeReviewSystem: {
          type: 'unknown',
          path: 'https://gitlab.myCompany.com/myUsername/myRepo.git',
        },
        pullRequestDomain: 'github.com',
        preferredSubmitCommand: undefined,
        isEdenFs: false,
      });
    });
  });

  it('applies isl.hold-off-refresh-ms config', async () => {
    setConfigOverrideForTests([['isl.hold-off-refresh-ms', '12345']], false);
    const info = (await Repository.getRepoInfo(ctx)) as ValidatedRepoInfo;
    const repo = new Repository(info, ctx);
    await new Promise(process.nextTick);
    expect(repo.configHoldOffRefreshMs).toBe(12345);
  });

  it('extracting repo info', async () => {
    setConfigOverrideForTests([]);
    setPathsDefault('mononoke://0.0.0.0/fbsource');
    mockEjeca([
      [/^sl root --dotdir/, {stdout: '/path/to/myRepo/.sl'}],
      [/^sl root/, {stdout: '/path/to/myRepo'}],
      [/^sl debugroots/, {stdout: '/path/to/myRepo/submodule\n/path/to/myRepo'}],
      [
        /^sl debuggitmodules/,
        {
          stdout: JSON.stringify([
            {
              name: 'submodule',
              url: 'https://example.com/submodule',
              path: 'submodule',
              active: true,
            },
          ]),
        },
      ],
    ]);
    const info = (await Repository.getRepoInfo(ctx)) as ValidatedRepoInfo;
    const repo = new Repository(info, ctx);
    expect(repo.info).toEqual({
      type: 'success',
      command: 'sl',
      repoRoot: '/path/to/myRepo',
      repoRoots: ['/path/to/myRepo', '/path/to/myRepo/submodule'],
      dotdir: '/path/to/myRepo/.sl',
      codeReviewSystem: expect.anything(),
      pullRequestDomain: undefined,
      preferredSubmitCommand: undefined,
      isEdenFs: false,
    });
  });

  it('drops ancestor repos that are not really submodules', async () => {
    setConfigOverrideForTests([]);
    setPathsDefault('mononoke://0.0.0.0/fbsource');
    // A checkout cloned inside another repo's working copy. `debugroots` reports both, but the
    // outer repo does not declare it as a submodule, so only the most local repo survives.
    mockEjeca([
      [/^sl root --dotdir/, {stdout: '/workspace/project/.sl'}],
      [/^sl root/, {stdout: '/workspace/project'}],
      [/^sl debugroots/, {stdout: '/workspace/project\n/workspace'}],
      [/^sl debuggitmodules/, {stdout: '[]'}],
    ]);
    const info = (await Repository.getRepoInfo(ctx)) as ValidatedRepoInfo;
    expect(info.repoRoots).toEqual(['/workspace/project']);
  });

  it('drops ancestor repos when submodules cannot be listed', async () => {
    setConfigOverrideForTests([]);
    setPathsDefault('mononoke://0.0.0.0/fbsource');
    mockEjeca([
      [/^sl root --dotdir/, {stdout: '/workspace/project/.sl'}],
      [/^sl root/, {stdout: '/workspace/project'}],
      [/^sl debugroots/, {stdout: '/workspace/project\n/workspace'}],
      [/^sl debuggitmodules/, new Error('unknown command debuggitmodules')],
    ]);
    const info = (await Repository.getRepoInfo(ctx)) as ValidatedRepoInfo;
    expect(info.repoRoots).toEqual(['/workspace/project']);
  });

  it('handles cwd not exists', async () => {
    const err = new Error('cwd does not exist') as Error & {code: string};
    err.code = 'ENOENT';
    mockEjeca([[/^sl root/, err]]);
    const info = (await Repository.getRepoInfo(ctx)) as ValidatedRepoInfo;
    expect(info).toEqual({
      type: 'cwdDoesNotExist',
      cwd: '/path/to/cwd',
    });
  });

  it('handles missing executables on windows', async () => {
    const osSpy = jest.spyOn(os, 'platform').mockImplementation(() => 'win32');
    mockEjeca([
      [
        /^sl root/,
        processExitError(
          /* code */ 1,
          `'sl' is not recognized as an internal or external command, operable program or batch file.`,
        ),
      ],
    ]);
    jest.spyOn(fsUtils, 'exists').mockImplementation(async () => true);
    const info = (await Repository.getRepoInfo(ctx)) as ValidatedRepoInfo;
    expect(info).toEqual({
      type: 'invalidCommand',
      command: 'sl',
      path: expect.anything(),
    });
    osSpy.mockRestore();
  });

  it('prevents setting configs not in the allowlist', async () => {
    setConfigOverrideForTests([]);
    setPathsDefault('mononoke://0.0.0.0/fbsource');
    mockEjeca([
      [/^sl root --dotdir/, {stdout: '/path/to/myRepo/.sl'}],
      [/^sl root/, {stdout: '/path/to/myRepo'}],
    ]);
    const info = (await Repository.getRepoInfo(ctx)) as ValidatedRepoInfo;
    const repo = new Repository(info, ctx);
    // @ts-expect-error We expect a type error in addition to runtime validation
    await expect(repo.setConfig(ctx, 'user', 'some-random-config', 'hi')).rejects.toEqual(
      new Error('config some-random-config not in allowlist for settable configs'),
    );
  });

  describe('running operations', () => {
    const repoInfo: ValidatedRepoInfo = {
      type: 'success',
      command: 'sl',
      dotdir: '/path/to/repo/.sl',
      repoRoot: '/path/to/repo',
      codeReviewSystem: {type: 'unknown'},
      pullRequestDomain: undefined,
      isEdenFs: false,
    };

    let ejecaSpy: ReturnType<typeof mockEjeca>;
    beforeEach(() => {
      ejecaSpy = mockEjeca([]);
    });

    async function runOperation(op: Partial<RunnableOperation>) {
      const repo = new Repository(repoInfo, ctx);
      const progressSpy = jest.fn();

      await repo.runOrQueueOperation(
        ctx,
        {
          id: '1',
          trackEventName: 'CommitOperation',
          args: [],
          runner: CommandRunner.Sapling,
          ...op,
        },
        progressSpy,
      );
    }

    it('runs operations', async () => {
      await runOperation({
        args: ['commit', '--message', 'hi'],
      });

      expect(ejecaSpy).toHaveBeenCalledWith(
        'sl',
        ['commit', '--message', 'hi', '--noninteractive'],
        expect.anything(),
      );
    });

    it('handles succeedable revsets', async () => {
      await runOperation({
        args: ['rebase', '--rev', {type: 'succeedable-revset', revset: 'aaa'}],
      });

      expect(ejecaSpy).toHaveBeenCalledWith(
        'sl',
        ['rebase', '--rev', 'max(successors(aaa))', '--noninteractive'],
        expect.anything(),
      );
    });

    it('handles exact revsets', async () => {
      await runOperation({
        args: ['rebase', '--rev', {type: 'exact-revset', revset: 'aaa'}],
      });

      expect(ejecaSpy).toHaveBeenCalledWith(
        'sl',
        ['rebase', '--rev', 'aaa', '--noninteractive'],
        expect.anything(),
      );
    });

    it('handles repo-relative files', async () => {
      await runOperation({
        args: ['add', {type: 'repo-relative-file', path: 'path/to/file.txt'}],
      });

      expect(ejecaSpy).toHaveBeenCalledWith(
        'sl',
        ['add', '../repo/path/to/file.txt', '--noninteractive'],
        expect.anything(),
      );
    });

    it('produces repo-root-relative file args when cwd is a deeper subdir of the root', async () => {
      // Regression for "cwd relative path ... is not under root": once cwd is canonicalized to the
      // root's namespace, the relativized file arg must not climb above the root with `../`.
      const canonicalRoot = '/data/repos/fbsource';
      const canonicalCwd = '/data/repos/fbsource/www';

      const symlinkedRepoInfo: ValidatedRepoInfo = {
        ...repoInfo,
        dotdir: `${canonicalRoot}/.sl`,
        repoRoot: canonicalRoot,
        isEdenFs: true,
      };
      const symlinkedCtx: RepositoryContext = {...ctx, cwd: canonicalCwd};
      const repo = new Repository(symlinkedRepoInfo, symlinkedCtx);
      const relativeFile =
        'flib/intern/aurora/lint_rules/ast_linters/graphql/__tests__/GraphQLBuildSandboxCacheTopologyLintRuleTest.php';

      await repo.runOrQueueOperation(
        symlinkedCtx,
        {
          id: '1',
          trackEventName: 'AmendFileSubsetOperation',
          runner: CommandRunner.Sapling,
          args: ['amend', {type: 'repo-relative-file', path: `www/${relativeFile}`}],
        },
        jest.fn(),
      );

      expect(ejecaSpy).toHaveBeenCalledWith(
        'sl',
        ['amend', relativeFile, '--noninteractive'],
        expect.anything(),
      );
      const passedArgs = ejecaSpy.mock.calls[0][1] as Array<string>;
      expect(passedArgs.some(arg => arg.startsWith('..'))).toBe(false);
    });

    it('handles allowed configs', async () => {
      await runOperation({
        args: ['commit', {type: 'config', key: 'ui.allowemptycommit', value: 'True'}],
      });

      expect(ejecaSpy).toHaveBeenCalledWith(
        'sl',
        ['commit', '--config', 'ui.allowemptycommit=True', '--noninteractive'],
        expect.anything(),
      );
    });

    it('disallows some commands', async () => {
      await runOperation({
        args: ['debugsh'],
      });

      expect(ejecaSpy).not.toHaveBeenCalledWith(
        'sl',
        ['debugsh', '--noninteractive'],
        expect.anything(),
      );
    });

    it('disallows unknown configs', async () => {
      await runOperation({
        args: ['commit', {type: 'config', key: 'foo.bar', value: '1'}],
      });

      expect(ejecaSpy).not.toHaveBeenCalledWith(
        'sl',
        expect.arrayContaining(['commit', '--config', 'foo.bar=1']),
        expect.anything(),
      );
    });

    it('disallows unstructured --config flag', async () => {
      await runOperation({
        args: ['commit', '--config', 'foo.bar=1'],
      });

      expect(ejecaSpy).not.toHaveBeenCalledWith(
        'sl',
        expect.arrayContaining(['commit', '--config', 'foo.bar=1']),
        expect.anything(),
      );
    });

    it.each([
      ['--config=hooks.pre-status=true'],
      ['--configfile', '/tmp/x.rc'],
      ['--configfile=/tmp/x.rc'],
      ['--cwd=/tmp'],
      ['--repo=/tmp'],
      ['--repository=/tmp'],
      ['-R/tmp'],
    ])('disallows global flag %s in any form', async (...flag) => {
      await runOperation({
        args: ['status', ...flag],
      });

      expect(ejecaSpy).not.toHaveBeenCalledWith(
        'sl',
        expect.arrayContaining(flag),
        expect.anything(),
      );
    });

    it('allows free-text values that look like flags', async () => {
      await runOperation({
        args: ['commit', '--message', '-Refactor --config handling'],
      });

      expect(ejecaSpy).toHaveBeenCalledWith(
        'sl',
        ['commit', '--message', '-Refactor --config handling', '--noninteractive'],
        expect.anything(),
      );
    });

    it('disallows debugpython', async () => {
      await runOperation({
        args: ['debugpython', '-c', 'print(1)'],
      });

      expect(ejecaSpy).not.toHaveBeenCalledWith(
        'sl',
        expect.arrayContaining(['debugpython']),
        expect.anything(),
      );
    });

    it('allows built-in merge tools', async () => {
      await runOperation({
        args: ['resolve', '--tool', 'internal:union', '--all'],
      });

      expect(ejecaSpy).toHaveBeenCalledWith(
        'sl',
        ['resolve', '--tool', 'internal:union', '--all', '--noninteractive'],
        expect.anything(),
      );
    });

    it.each([[['--tool', 'sh']], [['--tool=sh']], [['-t', 'sh']]])(
      'disallows arbitrary merge tool %j',
      async toolArgs => {
        await runOperation({
          args: ['rebase', '-d', 'aaa', ...toolArgs],
        });

        expect(ejecaSpy).not.toHaveBeenCalledWith(
          'sl',
          expect.arrayContaining(toolArgs),
          expect.anything(),
        );
      },
    );
  });

  describe('validateMergeToolArgs', () => {
    it('allows the configured ui.merge tool', () => {
      expect(() => validateMergeToolArgs(['resolve', '--tool', 'mytool'], 'mytool')).not.toThrow();
      expect(() => validateMergeToolArgs(['resolve', '--tool', 'other'], 'mytool')).toThrow();
    });

    it('ignores free-text values', () => {
      expect(() => validateMergeToolArgs(['commit', '--message', '-tidy up'])).not.toThrow();
    });
  });

  describe('isIllegalOperationFlag', () => {
    it.each(['--rev', '--continue', '--interactive', '-r', '-T', '--', 'commit'])(
      'allows %s',
      arg => {
        expect(isIllegalOperationFlag(arg)).toBe(false);
      },
    );
  });

  describe('fetchSloc', () => {
    const repoInfo: ValidatedRepoInfo = {
      type: 'success',
      command: 'sl',
      dotdir: '/path/to/repo/.sl',
      repoRoot: '/path/to/repo',
      codeReviewSystem: {type: 'unknown'},
      pullRequestDomain: undefined,
      isEdenFs: false,
    };

    const EXAMPLE_DIFFSTAT = `
|  34 ++++++++++
www/flib/intern/entity/diff/EntPhabricatorDiffSchema.php                                                            |  11 +++
2 files changed, 45 insertions(+), 0 deletions(-)\n`;

    it('parses sloc', async () => {
      const repo = new Repository(repoInfo, ctx);

      const ejecaSpy = mockEjeca([[/^sl diff/, () => ({stdout: EXAMPLE_DIFFSTAT})]]);
      const results = repo.fetchSignificantLinesOfCode(ctx, 'abcdef', ['generated.file']);
      await expect(results).resolves.toEqual({insertions: 45, deletions: 0});
      expect(ejecaSpy).toHaveBeenCalledWith(
        'sl',
        expect.arrayContaining([
          'diff',
          '-B',
          '-X',
          '**__generated__**',
          '-X',
          '/path/to/repo/generated.file',
          '-c',
          'abcdef',
        ]),
        expect.anything(),
      );
    });

    it('handles empty generated list', async () => {
      const repo = new Repository(repoInfo, ctx);
      const ejecaSpy = mockEjeca([[/^sl diff/, () => ({stdout: EXAMPLE_DIFFSTAT})]]);
      repo.fetchSignificantLinesOfCode(ctx, 'abcdef', []);
      expect(ejecaSpy).toHaveBeenCalledWith(
        'sl',
        expect.arrayContaining(['diff', '-B', '-X', '**__generated__**', '-c', 'abcdef']),
        expect.anything(),
      );
    });

    it('handles multiple generated files', async () => {
      const repo = new Repository(repoInfo, ctx);
      const ejecaSpy = mockEjeca([[/^sl diff/, () => ({stdout: EXAMPLE_DIFFSTAT})]]);
      const generatedFiles = ['generated1.file', 'generated2.file'];
      repo.fetchSignificantLinesOfCode(ctx, 'abcdef', generatedFiles);
      await nextTick();
      expect(ejecaSpy).toHaveBeenCalledWith(
        'sl',
        expect.arrayContaining([
          'diff',
          '-B',
          '-X',
          '**__generated__**',
          '-X',
          '/path/to/repo/generated1.file',
          '-X',
          '/path/to/repo/generated2.file',
          '-c',
          'abcdef',
        ]),
        expect.anything(),
      );
    });
  });

  describe('fetchSmartlogCommits', () => {
    const repoInfo: ValidatedRepoInfo = {
      type: 'success',
      command: 'sl',
      dotdir: '/path/to/repo/.sl',
      repoRoot: '/path/to/repo',
      codeReviewSystem: {type: 'unknown'},
      pullRequestDomain: undefined,
      isEdenFs: false,
    };

    const expectCalledWithRevset = (spy: jest.SpyInstance<unknown>, revset: string) => {
      expect(spy).toHaveBeenCalledWith(
        'sl',
        expect.arrayContaining(['log', '--rev', revset]),
        expect.anything(),
      );
    };

    it('uses correct revset in normal case', async () => {
      const repo = new Repository(repoInfo, ctx);

      const ejecaSpy = mockEjeca([]);

      await repo.fetchSmartlogCommits();
      expectCalledWithRevset(
        ejecaSpy,
        'smartlog(((interestingbookmarks() + heads(draft())) & date(-14)) + .)',
      );
    });

    it('reports when smartlog finds too many commits to render', async () => {
      const repo = new Repository(repoInfo, ctx);
      const onChange = jest.fn();
      const trackSpy = jest.spyOn(ctx.tracker, 'track').mockClear();
      repo.subscribeToSmartlogCommitsChanges(onChange);
      mockEjeca([
        [
          /^sl log/,
          {
            stdout: '',
            stderr:
              'smartlog: too many (160811) commits, not rendering all of them\n' +
              "(consider running 'sl doctor' to hide unrelated commits)\n",
          },
        ],
      ]);

      await repo.fetchSmartlogCommits();

      expect(onChange).toHaveBeenCalledWith(
        expect.objectContaining({
          commits: {error: expect.objectContaining({message: ErrorShortMessages.TooManyCommits})},
        }),
      );
      expect(trackSpy).toHaveBeenCalledWith('SmartlogTooManyCommits', {
        extras: {numCommits: 160811, visibleCommitDayRange: 14},
      });
    });

    it('tracks too many commits once until a fetch no longer has too many commits', async () => {
      const repo = new Repository(repoInfo, ctx);
      const trackSpy = jest.spyOn(ctx.tracker, 'track').mockClear();
      const tooManyCommits: [RegExp, {stdout: string; stderr: string}] = [
        /^sl log/,
        {stdout: '', stderr: 'smartlog: too many (2000) commits, not rendering all of them\n'},
      ];
      const countTracked = () =>
        trackSpy.mock.calls.filter(([name]) => name === 'SmartlogTooManyCommits').length;

      mockEjeca([tooManyCommits]);
      await repo.fetchSmartlogCommits();
      await repo.fetchSmartlogCommits();
      expect(countTracked()).toEqual(1);

      mockEjeca([[/^sl log/, {stdout: ''}]]);
      await repo.fetchSmartlogCommits();

      mockEjeca([tooManyCommits]);
      await repo.fetchSmartlogCommits();
      expect(countTracked()).toEqual(2);
    });

    it('updates revset when changing date range', async () => {
      const ejecaSpy = mockEjeca([]);
      const repo = new Repository(repoInfo, ctx);

      repo.nextVisibleCommitRangeInDays();
      await repo.fetchSmartlogCommits();
      expectCalledWithRevset(
        ejecaSpy,
        'smartlog(((interestingbookmarks() + heads(draft())) & date(-60)) + .)',
      );

      repo.nextVisibleCommitRangeInDays();
      await repo.fetchSmartlogCommits();
      expectCalledWithRevset(ejecaSpy, 'smartlog((interestingbookmarks() + heads(draft())) + .)');
    });

    it('fetches additional revsets', async () => {
      const ejecaSpy = mockEjeca([]);
      const repo = new Repository(repoInfo, ctx);

      repo.stableLocations = [
        {name: 'mystable', hash: 'aaa', info: 'this is the stable for aaa', date: new Date(0)},
      ];
      await repo.fetchSmartlogCommits();
      expectCalledWithRevset(
        ejecaSpy,
        'smartlog(((interestingbookmarks() + heads(draft())) & date(-14)) + . + present(aaa))',
      );

      repo.stableLocations = [
        {name: 'mystable', hash: 'aaa', info: 'this is the stable for aaa', date: new Date(0)},
        {name: '2', hash: 'bbb', info: '2', date: new Date(0)},
      ];
      await repo.fetchSmartlogCommits();
      expectCalledWithRevset(
        ejecaSpy,
        'smartlog(((interestingbookmarks() + heads(draft())) & date(-14)) + . + present(aaa) + present(bbb))',
      );

      repo.nextVisibleCommitRangeInDays();
      repo.nextVisibleCommitRangeInDays();
      await repo.fetchSmartlogCommits();
      expectCalledWithRevset(
        ejecaSpy,
        'smartlog((interestingbookmarks() + heads(draft())) + . + present(aaa) + present(bbb))',
      );
    });

    it('includes other worktrees checked-out hashes in revset, excluding own worktree', async () => {
      const repo = new Repository(repoInfo, ctx);
      (repo as unknown as {worktreeInfo: WorktreeInfo}).worktreeInfo = {
        sharedRoot: repoInfo.repoRoot,
        worktrees: [
          {path: repoInfo.repoRoot, role: 'main', node: 'abc123'},
          {path: '/path/to/other', role: 'linked', node: 'def456'},
          {path: '/path/to/no-hash', role: 'linked'},
        ],
        supportsMutation: false,
      };

      const ejecaSpy = mockEjeca([]);
      await repo.fetchSmartlogCommits();
      expectCalledWithRevset(
        ejecaSpy,
        'smartlog(((interestingbookmarks() + heads(draft())) & date(-14)) + . + present(def456))',
      );
    });

    it('getOtherWorktreeDotHashes dedupes and excludes own worktree', () => {
      const repo = new Repository(repoInfo, ctx);
      (repo as unknown as {worktreeInfo: WorktreeInfo}).worktreeInfo = {
        sharedRoot: repoInfo.repoRoot,
        worktrees: [
          {path: repoInfo.repoRoot, role: 'main', node: 'abc123'},
          {path: '/path/to/other', role: 'linked', node: 'def456'},
          {path: '/path/to/other2', role: 'linked', node: 'def456'},
        ],
        supportsMutation: false,
      };

      expect(repo.getOtherWorktreeDotHashes()).toEqual(['def456']);
    });
  });

  describe('merge conflicts', () => {
    jest.spyOn(fs.promises, 'realpath').mockImplementation(async (path, _opts) => {
      return path as string;
    });
    const repoInfo: ValidatedRepoInfo = {
      type: 'success',
      command: 'sl',
      dotdir: '/path/to/repo/.sl',
      repoRoot: '/path/to/repo',
      codeReviewSystem: {type: 'unknown'},
      pullRequestDomain: undefined,
      isEdenFs: false,
    };
    const NOT_IN_CONFLICT: ResolveCommandConflictOutput = [
      {
        command: null,
        conflicts: [],
        pathconflicts: [],
      },
    ];

    const conflictFileData = (contents: string) => ({
      contents,
      exists: true,
      isexec: false,
      issymlink: false,
    });
    const MARK_IN = '<'.repeat(7) + ` dest:   aaaaaaaaaaaa - unixname: Commit A`;
    const MARK_OUT = '>'.repeat(7) + ` source: bbbbbbbbbbbb - unixname: Commit B`;
    const MARK_BASE_START = `||||||| base`;
    const MARK_BASE_END = `=======`;

    const MOCK_CONFLICT: ResolveCommandConflictOutput = [
      {
        command: 'rebase',
        command_details: {
          cmd: 'rebase',
          to_abort: 'rebase --abort',
          to_continue: 'rebase --continue',
        },
        conflicts: [
          {
            base: conflictFileData('hello\nworld\n'),
            local: conflictFileData('hello\nworld - modified 1\n'),
            other: conflictFileData('hello\nworld - modified 2\n'),
            output: conflictFileData(
              `\
hello
${MARK_IN}
world - modified 1
${MARK_BASE_START}
world
${MARK_BASE_END}
modified 2
${MARK_OUT}
`,
            ),
            path: 'file1.txt',
          },
          {
            base: conflictFileData('hello\nworld\n'),
            local: conflictFileData('hello\nworld - modified 1\n'),
            other: conflictFileData('hello\nworld - modified 2\n'),
            output: conflictFileData(
              `\
hello
${MARK_IN}
world - modified 1
${MARK_BASE_START}
world
${MARK_BASE_END}
modified 2
${MARK_OUT}
`,
            ),
            path: 'file2.txt',
          },
        ],
        pathconflicts: [],
      },
    ];

    // same as MOCK_CONFLICT, but without any data for file1.txt
    const MOCK_CONFLICT_WITH_FILE1_RESOLVED: ResolveCommandConflictOutput = clone(MOCK_CONFLICT);
    MOCK_CONFLICT_WITH_FILE1_RESOLVED[0].conflicts.splice(0, 1);

    // these mock values are returned by ejeca / fs mocks
    // default: start in a not-in-conflict state
    let slMergeDirExists = false;
    let conflictData: ResolveCommandConflictOutput = NOT_IN_CONFLICT;

    /**
     * the next time repo.checkForMergeConflicts is called, this new conflict data will be used
     */
    function enterMergeConflict(conflict: ResolveCommandConflictOutput) {
      slMergeDirExists = true;
      conflictData = conflict;
    }

    beforeEach(() => {
      slMergeDirExists = false;
      conflictData = NOT_IN_CONFLICT;

      jest.spyOn(fsUtils, 'exists').mockImplementation(() => Promise.resolve(slMergeDirExists));

      mockEjeca([
        [
          /^sl resolve --tool internal:dumpjson --all/,
          () => ({stdout: JSON.stringify(conflictData)}),
        ],
      ]);
    });

    it('checks for merge conflicts', async () => {
      const repo = new Repository(repoInfo, ctx);

      const onChange = jest.fn();
      repo.onChangeConflictState(onChange);

      await repo.checkForMergeConflicts();
      expect(onChange).toHaveBeenCalledTimes(0);

      enterMergeConflict(MOCK_CONFLICT);

      await repo.checkForMergeConflicts();

      expect(onChange).toHaveBeenCalledWith({state: 'loading'});
      expect(onChange).toHaveBeenCalledWith({
        state: 'loaded',
        command: 'rebase',
        toContinue: 'rebase --continue',
        toAbort: 'rebase --abort',
        files: [
          {path: 'file1.txt', status: 'U', conflictType: 'both_changed'},
          {path: 'file2.txt', status: 'U', conflictType: 'both_changed'},
        ],
        fetchStartTimestamp: expect.anything(),
        fetchCompletedTimestamp: expect.anything(),
      } as MergeConflicts);
    });

    it('shows deleted file conflicts', async () => {
      const repo = new Repository(repoInfo, ctx);

      const onChange = jest.fn();
      repo.onChangeConflictState(onChange);

      await repo.checkForMergeConflicts();
      expect(onChange).toHaveBeenCalledTimes(0);

      const MOCK_DELETED_CONFLICT: ResolveCommandConflictOutput = [
        {
          command: 'rebase',
          command_details: {
            cmd: 'rebase',
            to_abort: 'rebase --abort',
            to_continue: 'rebase --continue',
          },
          conflicts: [
            {
              base: conflictFileData('hello\nworld\n'),
              local: conflictFileData('hello\nworld - modified 1\n'),
              other: {
                contents: null,
                exists: false,
                isexec: false,
                issymlink: false,
              },
              output: {
                contents: null,
                exists: false,
                isexec: false,
                issymlink: false,
              },
              path: 'file_del1.txt',
            },
            {
              base: conflictFileData('hello\nworld\n'),
              local: {
                contents: null,
                exists: false,
                isexec: false,
                issymlink: false,
              },
              other: conflictFileData('hello\nworld - modified 2\n'),
              output: conflictFileData('hello\nworld - modified 2\n'),
              path: 'file_del2.txt',
            },
          ],
          pathconflicts: [],
        },
      ];

      enterMergeConflict(MOCK_DELETED_CONFLICT);

      await repo.checkForMergeConflicts();

      expect(onChange).toHaveBeenCalledWith({state: 'loading'});
      expect(onChange).toHaveBeenCalledWith({
        state: 'loaded',
        command: 'rebase',
        toContinue: 'rebase --continue',
        toAbort: 'rebase --abort',
        files: [
          {path: 'file_del1.txt', status: 'U', conflictType: 'source_deleted'},
          {path: 'file_del2.txt', status: 'U', conflictType: 'dest_deleted'},
        ],
        fetchStartTimestamp: expect.anything(),
        fetchCompletedTimestamp: expect.anything(),
      } as MergeConflicts);
    });

    it('disposes conflict change subscriptions', async () => {
      const repo = new Repository(repoInfo, ctx);

      const onChange = jest.fn();
      const subscription = repo.onChangeConflictState(onChange);
      subscription.dispose();

      enterMergeConflict(MOCK_CONFLICT);
      await repo.checkForMergeConflicts();
      expect(onChange).toHaveBeenCalledTimes(0);
    });

    it('sends conflicts right away on subscription if already in conflicts', async () => {
      enterMergeConflict(MOCK_CONFLICT);

      const repo = new Repository(repoInfo, ctx);

      const onChange = jest.fn();
      repo.onChangeConflictState(onChange);
      await nextTick(); // allow message to get sent

      expect(onChange).toHaveBeenCalledWith({state: 'loading'});
      expect(onChange).toHaveBeenCalledWith({
        state: 'loaded',
        command: 'rebase',
        toContinue: 'rebase --continue',
        toAbort: 'rebase --abort',
        files: [
          {path: 'file1.txt', status: 'U', conflictType: 'both_changed'},
          {path: 'file2.txt', status: 'U', conflictType: 'both_changed'},
        ],
        fetchStartTimestamp: expect.anything(),
        fetchCompletedTimestamp: expect.anything(),
      });
    });

    it('preserves previous conflicts as resolved', async () => {
      const repo = new Repository(repoInfo, ctx);
      const onChange = jest.fn();
      repo.onChangeConflictState(onChange);

      enterMergeConflict(MOCK_CONFLICT);
      await repo.checkForMergeConflicts();
      expect(onChange).toHaveBeenCalledWith({
        state: 'loaded',
        command: 'rebase',
        toContinue: 'rebase --continue',
        toAbort: 'rebase --abort',
        files: [
          {path: 'file1.txt', status: 'U', conflictType: 'both_changed'},
          {path: 'file2.txt', status: 'U', conflictType: 'both_changed'},
        ],
        fetchStartTimestamp: expect.anything(),
        fetchCompletedTimestamp: expect.anything(),
      });

      enterMergeConflict(MOCK_CONFLICT_WITH_FILE1_RESOLVED);
      await repo.checkForMergeConflicts();
      expect(onChange).toHaveBeenCalledWith({
        state: 'loaded',
        command: 'rebase',
        toContinue: 'rebase --continue',
        toAbort: 'rebase --abort',
        files: [
          // even though file1 is no longer in the output, we remember it from before.
          {path: 'file1.txt', status: 'Resolved', conflictType: 'both_changed'},
          {path: 'file2.txt', status: 'U', conflictType: 'both_changed'},
        ],
        fetchStartTimestamp: expect.anything(),
        fetchCompletedTimestamp: expect.anything(),
      });
    });

    it('handles errors from `sl resolve`', async () => {
      mockEjeca([
        [/^sl resolve --tool internal:dumpjson --all/, new Error('failed to do the thing')],
      ]);

      const repo = new Repository(repoInfo, ctx);
      const onChange = jest.fn();
      repo.onChangeConflictState(onChange);

      enterMergeConflict(MOCK_CONFLICT);
      await expect(repo.checkForMergeConflicts()).resolves.toEqual(undefined);

      expect(onChange).toHaveBeenCalledWith({state: 'loading'});
      expect(onChange).toHaveBeenCalledWith(undefined);
    });
  });
});

describe('extractRepoInfoFromUrl', () => {
  describe('github.com', () => {
    it('handles http', () => {
      expect(extractRepoInfoFromUrl('https://github.com/myUsername/myRepo.git')).toEqual({
        owner: 'myUsername',
        repo: 'myRepo',
        hostname: 'github.com',
      });
    });
    it('handles plain github.com', () => {
      expect(extractRepoInfoFromUrl('github.com/myUsername/myRepo.git')).toEqual({
        owner: 'myUsername',
        repo: 'myRepo',
        hostname: 'github.com',
      });
    });
    it('handles git@github', () => {
      expect(extractRepoInfoFromUrl('git@github.com:myUsername/myRepo.git')).toEqual({
        owner: 'myUsername',
        repo: 'myRepo',
        hostname: 'github.com',
      });
    });
    it('handles ssh with slashes', () => {
      expect(extractRepoInfoFromUrl('ssh://git@github.com/myUsername/my-repo.git')).toEqual({
        owner: 'myUsername',
        repo: 'my-repo',
        hostname: 'github.com',
      });
    });
    it('handles git+ssh', () => {
      expect(extractRepoInfoFromUrl('git+ssh://git@github.com:myUsername/myRepo.git')).toEqual({
        owner: 'myUsername',
        repo: 'myRepo',
        hostname: 'github.com',
      });
    });
    it('handles dotted http', () => {
      expect(extractRepoInfoFromUrl('https://github.com/myUsername/my.dotted.repo.git')).toEqual({
        owner: 'myUsername',
        repo: 'my.dotted.repo',
        hostname: 'github.com',
      });
    });
    it('handles dotted ssh', () => {
      expect(extractRepoInfoFromUrl('git@github.com:myUsername/my.dotted.repo.git')).toEqual({
        owner: 'myUsername',
        repo: 'my.dotted.repo',
        hostname: 'github.com',
      });
    });
  });

  describe('github enterprise', () => {
    it('handles http', () => {
      expect(extractRepoInfoFromUrl('https://ghe.company.com/myUsername/myRepo.git')).toEqual({
        owner: 'myUsername',
        repo: 'myRepo',
        hostname: 'ghe.company.com',
      });
    });
    it('handles plain github.com', () => {
      expect(extractRepoInfoFromUrl('ghe.company.com/myUsername/myRepo.git')).toEqual({
        owner: 'myUsername',
        repo: 'myRepo',
        hostname: 'ghe.company.com',
      });
    });
    it('handles git@github', () => {
      expect(extractRepoInfoFromUrl('git@ghe.company.com:myUsername/myRepo.git')).toEqual({
        owner: 'myUsername',
        repo: 'myRepo',
        hostname: 'ghe.company.com',
      });
    });
    it('handles ssh with slashes', () => {
      expect(extractRepoInfoFromUrl('ssh://git@ghe.company.com/myUsername/my-repo.git')).toEqual({
        owner: 'myUsername',
        repo: 'my-repo',
        hostname: 'ghe.company.com',
      });
    });
    it('handles git+ssh', () => {
      expect(extractRepoInfoFromUrl('git+ssh://git@ghe.company.com:myUsername/myRepo.git')).toEqual(
        {
          owner: 'myUsername',
          repo: 'myRepo',
          hostname: 'ghe.company.com',
        },
      );
    });
    it('handles dotted http', () => {
      expect(
        extractRepoInfoFromUrl('https://ghe.company.com/myUsername/my.dotted.repo.git'),
      ).toEqual({
        owner: 'myUsername',
        repo: 'my.dotted.repo',
        hostname: 'ghe.company.com',
      });
    });
    it('handles dotted ssh', () => {
      expect(extractRepoInfoFromUrl('git@ghe.company.com:myUsername/my.dotted.repo.git')).toEqual({
        owner: 'myUsername',
        repo: 'my.dotted.repo',
        hostname: 'ghe.company.com',
      });
    });
  });
});

describe('absolutePathForFileInRepo', () => {
  let ctx: RepositoryContext;
  beforeEach(() => {
    ctx = {
      cmd: 'sl',
      cwd: '/path/to/cwd',
      logger: mockLogger,
      tracker: mockTracker,
    };
  });

  it('rejects .. in paths that escape the repo', () => {
    const repoInfo: ValidatedRepoInfo = {
      type: 'success',
      command: 'sl',
      dotdir: '/path/to/repo/.sl',
      repoRoot: '/path/to/repo',
      codeReviewSystem: {type: 'unknown'},
      pullRequestDomain: undefined,
      isEdenFs: false,
    };
    const repo = new Repository(repoInfo, ctx);

    expect(absolutePathForFileInRepo('foo/bar/file.txt', repo)).toEqual(
      '/path/to/repo/foo/bar/file.txt',
    );
    expect(absolutePathForFileInRepo('foo/../bar/file.txt', repo)).toEqual(
      '/path/to/repo/bar/file.txt',
    );
    expect(absolutePathForFileInRepo('file.txt', repo)).toEqual('/path/to/repo/file.txt');

    expect(absolutePathForFileInRepo('/file.txt', repo)).toEqual(null);
    expect(absolutePathForFileInRepo('', repo)).toEqual(null);
    expect(absolutePathForFileInRepo('foo/../../file.txt', repo)).toEqual(null);
    expect(absolutePathForFileInRepo('../file.txt', repo)).toEqual(null);
    expect(absolutePathForFileInRepo('/../file.txt', repo)).toEqual(null);
  });

  it('works on windows', () => {
    const repoInfo: ValidatedRepoInfo = {
      type: 'success',
      command: 'sl',
      dotdir: 'C:\\path\\to\\repo\\.sl',
      repoRoot: 'C:\\path\\to\\repo',
      codeReviewSystem: {type: 'unknown'},
      pullRequestDomain: undefined,
      isEdenFs: false,
    };
    const repo = new Repository(repoInfo, ctx);

    expect(absolutePathForFileInRepo('foo\\bar\\file.txt', repo, path.win32)).toEqual(
      'C:\\path\\to\\repo\\foo\\bar\\file.txt',
    );

    expect(absolutePathForFileInRepo('foo\\..\\..\\file.txt', repo, path.win32)).toEqual(null);
  });
});

describe('getCwdInfo', () => {
  it('computes cwd path and labels', async () => {
    mockEjeca([[/^sl root/, {stdout: '/path/to/myRepo'}]]);
    jest
      .spyOn(fs, 'realpath')
      .mockImplementation(((p: string, cb: (err: null, resolved: string) => void) =>
        cb(null, p)) as unknown as typeof fs.realpath);
    await expect(
      Repository.getCwdInfo({
        cmd: 'sl',
        cwd: '/path/to/myRepo/some/subdir',
        logger: mockLogger,
        tracker: mockTracker,
      }),
    ).resolves.toEqual({
      cwd: '/path/to/myRepo/some/subdir',
      repoRoot: '/path/to/myRepo',
      repoRelativeCwdLabel: 'myRepo/some/subdir',
    });
  });

  it('uses realpath', async () => {
    mockEjeca([[/^sl root/, {stdout: '/data/users/name/myRepo'}]]);
    const toRealPath = (p: string) => p.replace(/^\/home\/name\//, '/data/users/name/');
    jest
      .spyOn(fs, 'realpath')
      .mockImplementation(((p: string, cb: (err: null, resolved: string) => void) =>
        cb(null, toRealPath(p))) as unknown as typeof fs.realpath);
    await expect(
      Repository.getCwdInfo({
        cmd: 'sl',
        cwd: '/home/name/myRepo/some/subdir',
        logger: mockLogger,
        tracker: mockTracker,
      }),
    ).resolves.toEqual({
      cwd: '/data/users/name/myRepo/some/subdir', // cwd is realpath'd to match the server's repoInfo cwd
      repoRoot: '/data/users/name/myRepo', // repo root is realpath'd
      repoRelativeCwdLabel: 'myRepo/some/subdir',
    });
  });

  it('falls back to raw paths when realpath fails', async () => {
    mockEjeca([[/^sl root/, {stdout: '/path/to/myRepo'}]]);
    jest
      .spyOn(fs, 'realpath')
      .mockImplementation(((_p: string, cb: (err: Error) => void) =>
        cb(new Error('ENOENT'))) as unknown as typeof fs.realpath);
    await expect(
      Repository.getCwdInfo({
        cmd: 'sl',
        cwd: '/path/to/myRepo/some/subdir',
        logger: mockLogger,
        tracker: mockTracker,
      }),
    ).resolves.toEqual({
      cwd: '/path/to/myRepo/some/subdir',
      repoRoot: '/path/to/myRepo',
      repoRelativeCwdLabel: 'myRepo/some/subdir',
    });
  });

  it('returns null for non-repos', async () => {
    mockEjeca([[/^sl root/, new Error('not a repository')]]);
    await expect(
      Repository.getCwdInfo({
        cmd: 'sl',
        cwd: '/path/ro/myRepo/some/subdir',
        logger: mockLogger,
        tracker: mockTracker,
      }),
    ).resolves.toEqual({
      cwd: '/path/ro/myRepo/some/subdir',
    });
  });
});

describe('fetchSubmoduleMap', () => {
  let myRepoRoot: AbsolutePath;
  let ctx: RepositoryContext;
  beforeEach(() => {
    myRepoRoot = '/data/users/name/myRepo';
    ctx = {
      cmd: 'sl',
      cwd: myRepoRoot,
      logger: mockLogger,
      tracker: mockTracker,
    };
  });

  it('simple', async () => {
    const submodules: Submodule[] = [
      {
        name: 'submoduleA',
        url: 'https://ghe.myCompany.com/myUsername/myRepo/submoduleA',
        path: 'submoduleA',
        active: true,
      },
      {
        name: 'submoduleB',
        url: 'https://ghe.myCompany.com/myUsername/myRepo/submoduleB',
        path: 'submoduleB',
        active: false,
      },
    ];
    const submodulesJson = JSON.stringify(submodules);
    mockEjeca([
      [/^sl root/, {stdout: myRepoRoot}],
      [/^sl debugroots/, {stdout: myRepoRoot}],
      [/^sl debuggitmodules/, {stdout: submodulesJson}],
    ]);
    const info = (await Repository.getRepoInfo(ctx)) as ValidatedRepoInfo;
    const repo = new Repository(info, ctx);
    await repo.fetchSubmoduleMap();
    const fetchedSubmoduleMap = repo.getSubmoduleMap();
    expect(fetchedSubmoduleMap).not.toBeUndefined();
    expect(fetchedSubmoduleMap?.get(myRepoRoot)?.value).toEqual(submodules);
    repo.dispose();
  });

  it('no submodules', async () => {
    const submodules: Submodule[] = [];
    const submodulesJson = JSON.stringify(submodules);
    mockEjeca([
      [/^sl root/, {stdout: myRepoRoot}],
      [/^sl debugroots/, {stdout: myRepoRoot}],
      [/^sl debuggitmodules/, {stdout: submodulesJson}],
    ]);
    const info = (await Repository.getRepoInfo(ctx)) as ValidatedRepoInfo;
    const repo = new Repository(info, ctx);
    await repo.fetchSubmoduleMap();
    const fetchedSubmoduleMap = repo.getSubmoduleMap();
    expect(fetchedSubmoduleMap).not.toBeUndefined();
    expect(fetchedSubmoduleMap?.get(myRepoRoot)?.value).toBeUndefined();
    repo.dispose();
  });

  it('nested', async () => {
    const submodulesOfMyRepo: Submodule[] = [
      {
        name: 'submoduleA',
        url: 'https://ghe.myCompany.com/myUsername/myRepo/submoduleA',
        path: 'submoduleA',
        active: true,
      },
    ];
    const submoduleARoot = myRepoRoot + '/submoduleA';
    const submodulesOfA: Submodule[] = [
      {
        name: 'submoduleB',
        url: 'https://ghe.myCompany.com/myUsername/myRepo/submoduleA/submoduleB',
        path: 'submoduleB',
        active: true,
      },
    ];
    const submoduleBRoot = submoduleARoot + '/submoduleB';
    mockEjeca([
      [
        new RegExp(`^sl debuggitmodules --json --repo ${submoduleARoot}`),
        {stdout: JSON.stringify(submodulesOfA)},
      ],
      [
        new RegExp(`^sl debuggitmodules --json --repo ${myRepoRoot}`),
        {stdout: JSON.stringify(submodulesOfMyRepo)},
      ],
      [/^sl root/, {stdout: submoduleBRoot}],
      [/^sl debugroots/, {stdout: submoduleBRoot + '\n' + submoduleARoot + '\n' + myRepoRoot}],
    ]);
    const updatedCtx = {...ctx, cwd: submoduleBRoot};
    const info = (await Repository.getRepoInfo(updatedCtx)) as ValidatedRepoInfo;
    const repo = new Repository(info, updatedCtx);
    await repo.fetchSubmoduleMap();
    const fetchedSubmoduleMap = repo.getSubmoduleMap();

    expect(fetchedSubmoduleMap).not.toBeUndefined();
    expect(fetchedSubmoduleMap?.get(myRepoRoot)?.value).toEqual(submodulesOfMyRepo);
    expect(fetchedSubmoduleMap?.get(submoduleARoot)?.value).toEqual(submodulesOfA);
    repo.dispose();
  });

  it('error', async () => {
    const msg = 'mock sapling error';
    mockEjeca([
      [/^sl root/, {stdout: myRepoRoot}],
      [/^sl debugroots/, {stdout: myRepoRoot}],
      [/^sl debuggitmodules/, new Error(msg)],
    ]);
    const info = (await Repository.getRepoInfo(ctx)) as ValidatedRepoInfo;
    const repo = new Repository(info, ctx);
    await repo.fetchSubmoduleMap();
    const fetchedSubmoduleMap = repo.getSubmoduleMap();

    expect(fetchedSubmoduleMap).not.toBeUndefined();
    expect(fetchedSubmoduleMap?.get(myRepoRoot)?.value).toBeUndefined();
    expect(fetchedSubmoduleMap?.get(myRepoRoot)?.error?.message).toMatch(msg);
    repo.dispose();
  });

  describe('worktree discovery', () => {
    beforeEach(() => {
      setConfigOverrideForTests([['paths.default', 'https://github.com/owner/repo.git']]);
    });

    it('populates worktreeInfo when multiple worktrees exist', async () => {
      mockEjeca([
        [/^sl root --dotdir/, {stdout: '/repo/main/.sl'}],
        [/^sl root --shared/, {stdout: '/repo/main'}],
        [/^sl root/, {stdout: '/repo/main'}],
        [/^sl debugroots/, {stdout: '/repo/main'}],
        [
          /^sl --config worktree\.enabled=true worktree list/,
          {
            stdout: JSON.stringify([
              {path: '/repo/main', role: 'main'},
              {path: '/repo/feature', label: 'feature-x', role: 'linked'},
            ]),
          },
        ],
        // checked-out hash for the *other* worktree, fetched via `sl whereami -R <path>`
        [/^sl whereami/, {stdout: 'bbb'}],
      ]);

      const info = (await Repository.getRepoInfo(ctx)) as ValidatedRepoInfo;
      const repo = new Repository(info, ctx);
      await repo.refreshWorktreeInfo();
      expect(repo.getWorktreeInfo()).toBeDefined();
      expect(repo.getWorktreeInfo()).toEqual({
        sharedRoot: '/repo/main',
        worktrees: [
          {path: '/repo/main', role: 'main'},
          {path: '/repo/feature', label: 'feature-x', role: 'linked', node: 'bbb'},
        ],
        supportsMutation: false,
      });
    });

    it('populates worktreeInfo for solo repos with sharedRoot', async () => {
      mockEjeca([
        [/^sl root --dotdir/, {stdout: '/repo/main/.sl'}],
        [/^sl root --shared/, {stdout: '/repo/main'}],
        [/^sl root/, {stdout: '/repo/main'}],
        [/^sl debugroots/, {stdout: '/repo/main'}],
        [
          /^sl --config worktree\.enabled=true worktree list/,
          {
            stdout: JSON.stringify([{path: '/repo/main', role: 'main'}]),
          },
        ],
      ]);

      const info = (await Repository.getRepoInfo(ctx)) as ValidatedRepoInfo;
      const repo = new Repository(info, ctx);
      await repo.refreshWorktreeInfo();
      expect(repo.getWorktreeInfo()).toEqual({
        sharedRoot: '/repo/main',
        worktrees: [{path: '/repo/main', role: 'main'}],
        supportsMutation: false,
      });
    });

    it('falls back to synthetic current worktree when worktree command fails', async () => {
      mockEjeca([
        [/^sl root --dotdir/, {stdout: '/repo/main/.sl'}],
        [/^sl root --shared/, {stdout: '/repo/main'}],
        [/^sl root/, {stdout: '/repo/main'}],
        [/^sl debugroots/, {stdout: '/repo/main'}],
        [
          /^sl --config worktree\.enabled=true worktree list/,
          new Error('worktree feature not enabled'),
        ],
      ]);

      const info = (await Repository.getRepoInfo(ctx)) as ValidatedRepoInfo;
      const repo = new Repository(info, ctx);
      await repo.refreshWorktreeInfo();
      expect(repo.getWorktreeInfo()).toEqual({
        sharedRoot: '/repo/main',
        worktrees: [{path: '/repo/main', role: 'main'}],
        supportsMutation: false,
      });
    });

    it('correctly identifies non-main worktree', async () => {
      mockEjeca([
        [/^sl root --dotdir/, {stdout: '/repo/feature/.sl'}],
        [/^sl root --shared/, {stdout: '/repo/main'}],
        [/^sl root/, {stdout: '/repo/feature'}],
        [/^sl debugroots/, {stdout: '/repo/feature'}],
        [
          /^sl --config worktree\.enabled=true worktree list/,
          {
            stdout: JSON.stringify([
              {path: '/repo/main', role: 'main'},
              {path: '/repo/feature', label: 'feature-x', role: 'linked'},
            ]),
          },
        ],
        // this repo is /repo/feature, so /repo/main is the "other" worktree queried
        [/^sl whereami/, {stdout: 'aaa'}],
      ]);

      const info = (await Repository.getRepoInfo(ctx)) as ValidatedRepoInfo;
      const repo = new Repository(info, ctx);
      await repo.refreshWorktreeInfo();
      const worktreeInfo = repo.getWorktreeInfo();
      expect(worktreeInfo).toBeDefined();
      expect(worktreeInfo!.sharedRoot).toBe('/repo/main');
      expect(worktreeInfo!.worktrees).toEqual([
        {path: '/repo/main', role: 'main', node: 'aaa'},
        {path: '/repo/feature', label: 'feature-x', role: 'linked'},
      ]);
    });

    it('leaves node unset when `sl whereami` fails for a sibling worktree', async () => {
      mockEjeca([
        [/^sl root --dotdir/, {stdout: '/repo/main/.sl'}],
        [/^sl root --shared/, {stdout: '/repo/main'}],
        [/^sl root/, {stdout: '/repo/main'}],
        [/^sl debugroots/, {stdout: '/repo/main'}],
        [
          /^sl --config worktree\.enabled=true worktree list/,
          {
            stdout: JSON.stringify([
              {path: '/repo/main', role: 'main'},
              {path: '/repo/feature', label: 'feature-x', role: 'linked'},
            ]),
          },
        ],
        [/^sl whereami/, new Error('worktree mid-checkout')],
      ]);

      const info = (await Repository.getRepoInfo(ctx)) as ValidatedRepoInfo;
      const repo = new Repository(info, ctx);
      await repo.refreshWorktreeInfo();
      expect(repo.getWorktreeInfo()).toEqual({
        sharedRoot: '/repo/main',
        worktrees: [
          {path: '/repo/main', role: 'main'},
          {path: '/repo/feature', label: 'feature-x', role: 'linked'},
        ],
        supportsMutation: false,
      });
    });

    it('reports worktree mutations as supported only for EdenFS repos', async () => {
      mockEjeca([
        [/^sl root --dotdir/, {stdout: '/repo/main/.sl'}],
        [/^sl root --shared/, {stdout: '/repo/main'}],
        [/^sl root/, {stdout: '/repo/main'}],
        [/^sl debugroots/, {stdout: '/repo/main'}],
        [
          /^sl --config worktree\.enabled=true worktree list/,
          {stdout: JSON.stringify([{path: '/repo/main', role: 'main'}])},
        ],
      ]);

      const info = (await Repository.getRepoInfo(ctx)) as ValidatedRepoInfo;
      const edenRepo = new Repository({...info, isEdenFs: true}, ctx);
      await edenRepo.refreshWorktreeInfo();
      expect(edenRepo.getWorktreeInfo()?.supportsMutation).toBe(true);

      const gitRepo = new Repository({...info, isEdenFs: false}, ctx);
      await gitRepo.refreshWorktreeInfo();
      expect(gitRepo.getWorktreeInfo()?.supportsMutation).toBe(false);
    });

    it('leaves worktreeInfo undefined when sharedRoot is unavailable', async () => {
      mockEjeca([
        [/^sl root --dotdir/, {stdout: '/repo/main/.sl'}],
        [/^sl root --shared/, new Error('not supported')],
        [/^sl root/, {stdout: '/repo/main'}],
        [/^sl debugroots/, {stdout: '/repo/main'}],
        [
          /^sl --config worktree\.enabled=true worktree list/,
          {
            stdout: JSON.stringify([
              {path: '/repo/main', role: 'main'},
              {path: '/repo/feature', label: 'feature-x', role: 'linked'},
            ]),
          },
        ],
      ]);

      const info = (await Repository.getRepoInfo(ctx)) as ValidatedRepoInfo;
      const repo = new Repository(info, ctx);
      await repo.refreshWorktreeInfo();
      // Without sharedRoot, worktreeInfo should not be populated
      expect(repo.getWorktreeInfo()).toBeUndefined();
    });
  });
});
