/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import {act, render, screen} from '@testing-library/react';
import App from '../App';
import platform from '../platform';
import {CommitInfoTestUtils} from '../testQueries';
import {
  COMMIT,
  expectMessageSentToServer,
  simulateCommits,
  simulateRepoConnected,
} from '../testUtils';

describe('persistAtomToLocalStorageEffect', () => {
  const getTemporary = jest.fn();
  const setTemporary = jest.fn();

  beforeEach(() => {
    platform.getPersistedState = getTemporary;
    platform.setPersistedState = setTemporary;
    getTemporary.mockReset();
    setTemporary.mockReset();

    getTemporary.mockImplementation(() => null);
    setTemporary.mockImplementation(() => null);

    render(<App />);

    act(() => {
      simulateRepoConnected();
      expectMessageSentToServer({
        type: 'subscribe',
        kind: 'smartlogCommits',
        subscriptionID: expect.anything(),
      });
      simulateCommits({
        value: [
          COMMIT('1', 'some public base', '0', {phase: 'public'}),
          COMMIT('a', 'My Commit', '1'),
          COMMIT('b', 'Another Commit', 'a', {isDot: true}),
        ],
      });
    });
  });

  it('saves state to local storage', () => {
    expect(screen.getByTestId('commit-info-view')).toBeInTheDocument();

    act(() => {
      CommitInfoTestUtils.openCommitInfoSidebar(); // toggle
    });

    expect(screen.queryByTestId('commit-info-view')).not.toBeInTheDocument();

    expect(platform.setPersistedState).toHaveBeenCalledWith(
      'isl.drawer-state',
      expect.objectContaining({
        right: {collapsed: true, size: 500},
      }),
    );

    act(() => {
      CommitInfoTestUtils.openCommitInfoSidebar(); // toggle
    });

    expect(platform.setPersistedState).toHaveBeenCalledWith(
      'isl.drawer-state',
      expect.objectContaining({
        right: {collapsed: false, size: 500},
      }),
    );
  });
});
