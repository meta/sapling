/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import {act, fireEvent, render, screen, waitFor} from '@testing-library/react';
import * as utils from 'shared/utils';
import App from '../App';
import {
  COMMIT,
  closeCommitInfoSidebar,
  expectMessageSentToServer,
  getLastMessageOfTypeSentToServer,
  simulateCommits,
  simulateMessageFromServer,
  simulateRepoConnected,
} from '../testUtils';
import {CommandRunner} from '../types';

const allCommits = [
  COMMIT('1', 'some public base', '0', {phase: 'public'}),
  COMMIT('a', 'My Commit', '1'),
  COMMIT('b', 'Another Commit', 'a', {isDot: true}),
];

describe('CommitTreeList', () => {
  beforeEach(async () => {
    render(<App />);
    await act(() => {
      simulateRepoConnected();
      closeCommitInfoSidebar();
      expectMessageSentToServer({
        type: 'subscribe',
        kind: 'smartlogCommits',
        subscriptionID: expect.anything(),
      });
      simulateCommits({value: allCommits});
    });
  });

  it('load more button works', async () => {
    fireEvent.click(screen.getByText('Load more commits'));
    expectMessageSentToServer({type: 'loadMoreCommits'});
    await act(() => simulateMessageFromServer({type: 'commitsShownRange', rangeInDays: 60}));
    await act(() => simulateMessageFromServer({type: 'beganLoadingMoreCommits'}));
    await act(() => simulateCommits({value: allCommits}));
  });

  it('shows how to disable focus mode to see additional commits', () => {
    fireEvent.click(screen.getByTestId('focus-mode-toggle'));

    expect(
      screen.getByText('Focus mode is on. Disable to see additional commits'),
    ).toBeInTheDocument();
    expect(screen.queryByText('Load more commits')).not.toBeInTheDocument();

    fireEvent.click(screen.getByText('Focus mode is on. Disable to see additional commits'));

    expect(
      screen.queryByText('Focus mode is on. Disable to see additional commits'),
    ).not.toBeInTheDocument();
    expect(screen.getByText('Load more commits')).toBeInTheDocument();
    expect(screen.getByTestId('focus-mode-toggle')).toHaveAttribute('data-focus-mode', 'false');
  });

  it('does not show the focus mode indicator at the true repository root', async () => {
    await act(() =>
      simulateCommits({
        value: [
          {...COMMIT('0', 'repository root', 'unused'), parents: []},
          COMMIT('a', 'My Commit', '0'),
          COMMIT('b', 'Another Commit', 'a', {isDot: true}),
        ],
      }),
    );

    fireEvent.click(screen.getByTestId('focus-mode-toggle'));

    expect(
      screen.queryByText('Focus mode is on. Disable to see additional commits'),
    ).not.toBeInTheDocument();
  });

  it('disables while running', async () => {
    fireEvent.click(screen.getByText('Load more commits'));
    expectMessageSentToServer({type: 'loadMoreCommits'});
    await act(() => simulateMessageFromServer({type: 'commitsShownRange', rangeInDays: 60}));
    await act(() => simulateMessageFromServer({type: 'beganLoadingMoreCommits'}));
    expect(screen.getByText('Load more commits')).toBeDisabled();

    await act(() => simulateCommits({value: allCommits}));
    expect(screen.getByText('Load more commits')).not.toBeDisabled();
  });

  it('uses cloud sync after loading all commits', async () => {
    fireEvent.click(screen.getByText('Load more commits'));
    expectMessageSentToServer({type: 'loadMoreCommits'});
    await act(() => simulateMessageFromServer({type: 'commitsShownRange', rangeInDays: 60}));
    await act(() => simulateMessageFromServer({type: 'beganLoadingMoreCommits'}));
    await act(() => simulateCommits({value: allCommits}));

    fireEvent.click(screen.getByText('Load more commits'));
    expectMessageSentToServer({type: 'loadMoreCommits'});
    await act(() => simulateMessageFromServer({type: 'commitsShownRange', rangeInDays: undefined}));
    await act(() => simulateMessageFromServer({type: 'beganLoadingMoreCommits'}));
    await act(() => simulateCommits({value: allCommits}));

    expectMessageSentToServer({type: 'getConfig', name: 'extensions.commitcloud'});
    await act(() =>
      simulateMessageFromServer({type: 'gotConfig', name: 'extensions.commitcloud', value: ''}),
    );

    await waitFor(() => expect(screen.getByText('Fetch all cloud commits')));
    fireEvent.click(screen.getByText('Fetch all cloud commits'));

    const message = await waitFor(() =>
      utils.nullthrows(getLastMessageOfTypeSentToServer('runOperation')),
    );
    const id = message.operation.id;

    expectMessageSentToServer({
      type: 'runOperation',
      operation: {
        args: ['cloud', 'sync', '--full'],
        id,
        runner: CommandRunner.Sapling,
        trackEventName: 'CommitCloudSyncOperation',
      },
    });

    await act(() =>
      simulateMessageFromServer({
        type: 'operationProgress',
        id,
        kind: 'spawn',
        queue: [],
      }),
    );
    await act(() =>
      simulateMessageFromServer({
        type: 'operationProgress',
        id,
        kind: 'exit',
        exitCode: 0,
        timestamp: 1234,
      }),
    );

    // buttons are gone now that we synced from cloud
    await waitFor(() => {
      expect(screen.queryByText('Load more commits')).not.toBeInTheDocument();
      expect(screen.queryByText('Fetch all cloud commits')).not.toBeInTheDocument();
    });
  });

  it('does not show cloud sync button if commit cloud not enabled', async () => {
    fireEvent.click(screen.getByText('Load more commits'));
    expectMessageSentToServer({type: 'loadMoreCommits'});
    await act(() => simulateMessageFromServer({type: 'commitsShownRange', rangeInDays: 60}));
    await act(() => simulateMessageFromServer({type: 'beganLoadingMoreCommits'}));
    await act(() => simulateCommits({value: allCommits}));

    fireEvent.click(screen.getByText('Load more commits'));
    expectMessageSentToServer({type: 'loadMoreCommits'});
    await act(() => simulateMessageFromServer({type: 'commitsShownRange', rangeInDays: undefined}));
    await act(() => simulateMessageFromServer({type: 'beganLoadingMoreCommits'}));
    await act(() => simulateCommits({value: allCommits}));

    expectMessageSentToServer({type: 'getConfig', name: 'extensions.commitcloud'});
    await act(() =>
      simulateMessageFromServer({type: 'gotConfig', name: 'extensions.commitcloud', value: '!'}),
    );

    expect(screen.queryByText('Fetch all cloud commits')).not.toBeInTheDocument();
  });
});
