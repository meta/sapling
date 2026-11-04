/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import * as ejeca from 'shared/ejeca';
import queryGraphQL from '../github/queryGraphQL';

function mockGhExit(exitCode: number) {
  jest.spyOn(ejeca, 'ejeca').mockImplementation((() =>
    Promise.reject(
      new ejeca.EjecaError({
        exitCode,
        stdout: '',
        stderr: '',
        killed: false,
        escapedCommand: 'gh api graphql',
      }),
    )) as unknown as typeof ejeca.ejeca);
}

describe('queryGraphQL', () => {
  it('reports gh exit code 4 as NotAuthenticatedError', async () => {
    mockGhExit(4);
    await expect(queryGraphQL('query {}', {}, 'github.com')).rejects.toThrow(
      /^NotAuthenticatedError/,
    );
  });

  it('rethrows other gh failures unchanged', async () => {
    mockGhExit(1);
    await expect(queryGraphQL('query {}', {}, 'github.com')).rejects.toThrow(
      /exited with non-zero status with exit code 1/,
    );
  });
});
