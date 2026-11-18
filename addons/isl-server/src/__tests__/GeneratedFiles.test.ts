/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import type {PathLike} from 'node:fs';
import type {FileHandle} from 'node:fs/promises';
import type {Repository} from '../Repository';
import type {ServerPlatform} from '../serverPlatform';
import type {RepositoryContext} from '../serverTypes';

import {GeneratedStatus} from 'isl/src/types';
import {promises} from 'node:fs';
import {mockLogger} from 'shared/testUtils';
import {GeneratedFilesDetector} from '../GeneratedFiles';
import {Internal} from '../Internal';
import {makeServerSideTracker} from '../analytics/serverSideTracker';

/* eslint-disable require-await */

const mockTracker = makeServerSideTracker(
  mockLogger,
  {platformName: 'test'} as ServerPlatform,
  '0.1',
  jest.fn(),
);

const mockCtx: RepositoryContext = {
  agentSessionMetadata: undefined,
  cwd: 'cwd',
  cmd: 'sl',
  logger: mockLogger,
  tracker: mockTracker,
};

describe('GeneratedFiles', () => {
  describe('getGeneratedFilePathRegex', () => {
    it('can take configured custom regex', async () => {
      jest.spyOn(promises, 'open').mockImplementation(async () => {
        throw new Error('skipping in tests');
      });

      const mockRepo = {
        getConfig: async () => Promise.resolve('foobar'),
        logger: mockLogger,
      } as unknown as Repository;
      const detector = new GeneratedFilesDetector();
      const result = await detector.queryFilesGenerated(mockRepo, mockCtx, '/', [
        'src/myFile.js',
        'foobar',
      ]);
      expect(result).toEqual({
        'src/myFile.js': GeneratedStatus.Manual,
        foobar: GeneratedStatus.Generated,
      });
    });

    it('detects yarn.lock as generated', async () => {
      jest.spyOn(promises, 'open').mockImplementation(async () => {
        throw new Error('skipping in tests');
      });

      const mockRepo = {
        getConfig: async () => Promise.resolve(undefined),
        logger: mockLogger,
      } as unknown as Repository;
      const detector = new GeneratedFilesDetector();
      const result = await detector.queryFilesGenerated(mockRepo, mockCtx, '/', [
        'src/myFile.js',
        'yarn.lock',
        'subproject/yarn.lock',
      ]);
      expect(result).toEqual({
        'src/myFile.js': GeneratedStatus.Manual,
        'yarn.lock': GeneratedStatus.Generated,
        'subproject/yarn.lock': GeneratedStatus.Generated,
      });
    });

    // The dsp.zip pattern is only in the internal list.
    (Internal.generatedFilesRegex == null ? it.skip : it)(
      'detects omnigenerator dsp.zip under xplat as generated',
      async () => {
        jest.spyOn(promises, 'open').mockImplementation(async () => {
          throw new Error('skipping in tests');
        });

        const mockRepo = {
          getConfig: async () => Promise.resolve(undefined),
          logger: mockLogger,
        } as unknown as Repository;
        const detector = new GeneratedFilesDetector();
        const result = await detector.queryFilesGenerated(mockRepo, mockCtx, '/', [
          'xplat/orca/msys/feature_aggregation/dasm/msys_sps/dsp.zip',
          'xplat/bizapp_android/msys/dasm/msys_sps/dsp.zip',
          'xplat/orca/msys/feature_aggregation/dasm/unbound_sp_list.txt',
          'fbcode/foo/dasm/msys_sps/dsp.zip',
        ]);
        expect(result).toEqual({
          'xplat/orca/msys/feature_aggregation/dasm/msys_sps/dsp.zip': GeneratedStatus.Generated,
          'xplat/bizapp_android/msys/dasm/msys_sps/dsp.zip': GeneratedStatus.Generated,
          'xplat/orca/msys/feature_aggregation/dasm/unbound_sp_list.txt': GeneratedStatus.Manual,
          'fbcode/foo/dasm/msys_sps/dsp.zip': GeneratedStatus.Manual,
        });
      },
    );
  });

  describe('readFilesLookingForGeneratedTag', () => {
    it('detects generate tag in file content', async () => {
      jest.spyOn(promises, 'open').mockImplementation(async (filePath: PathLike, _flags, _mod) => {
        return {
          read: jest.fn(async () => ({
            buffer:
              filePath === '/myGeneratedFile.js'
                ? `/* this file is ${'@'}generated */`
                : filePath === '/myPartiallyGeneratedFile.js'
                  ? `/* this file is ${'@'}partially-generated */`
                  : '// Normal file content',
          })),
          close: jest.fn(),
        } as unknown as FileHandle;
      });

      const mockRepo = {
        getConfig: async () => Promise.resolve(undefined),
        logger: mockLogger,
      } as unknown as Repository;
      const detector = new GeneratedFilesDetector();
      const result = await detector.queryFilesGenerated(mockRepo, mockCtx, '/', [
        'myFile.js',
        'myPartiallyGeneratedFile.js',
        'myGeneratedFile.js',
      ]);
      expect(result).toEqual({
        'myFile.js': GeneratedStatus.Manual,
        'myPartiallyGeneratedFile.js': GeneratedStatus.PartiallyGenerated,
        'myGeneratedFile.js': GeneratedStatus.Generated,
      });
    });
  });
});
