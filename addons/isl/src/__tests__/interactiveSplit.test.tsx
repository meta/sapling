/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import type {DateTuple} from 'shared/types/common';
import type {CodeReviewSystem} from '../types';

import {act, fireEvent, render, screen, waitFor, within} from '@testing-library/react';
import * as utils from 'shared/utils';
import App from '../App';
import {
  closeCommitInfoSidebar,
  expectMessageNOTSentToServer,
  expectMessageSentToServer,
  getLastMessageOfTypeSentToServer,
  resetTestMessages,
  simulateCommits,
  simulateMessageFromServer,
  TEST_COMMIT_HISTORY,
} from '../testUtils';
import {CommandRunner} from '../types';

const EXPORT_STACK_DATA = [
  {
    requested: false,
    node: 'd',
    author: 'username',
    date: [1715719789, 25200] as DateTuple,
    text: 'Commit D',
    immutable: false,
    relevantFiles: {
      'myFile.js': {
        data: 'hello\nworld!\n',
      },
    },
  },
  {
    requested: true,
    node: 'e',
    author: 'username',
    date: [1715719789, 25200] as DateTuple,
    text: 'Commit E',
    immutable: false,
    parents: ['d'],
    files: {
      'myFile.js': {
        data: 'hello (changed)\nworld!\n',
      },
    },
  },
];

describe('Interactive Split', () => {
  beforeEach(() => {
    resetTestMessages();
    render(<App />);
    act(() => {
      closeCommitInfoSidebar();
      simulateMessageFromServer({
        type: 'repoInfo',
        info: {
          type: 'success',
          repoRoot: '/path/to/repo',
          dotdir: '/path/to/repo/.sl',
          command: 'sl',
          pullRequestDomain: undefined,
          codeReviewSystem: {type: 'github'} as CodeReviewSystem,
          isEdenFs: false,
        },
      });
      expectMessageSentToServer({
        type: 'subscribe',
        kind: 'smartlogCommits',
        subscriptionID: expect.anything(),
      });
      simulateCommits({
        value: TEST_COMMIT_HISTORY,
      });
    });
  });

  it('shows split button on dot commit', () => {
    expect(screen.getByText('Split')).toBeInTheDocument();
  });

  it('show split modal with spinner on click', async () => {
    fireEvent.click(screen.getByText('Split'));
    await waitFor(() => expect(screen.getByTestId('edit-stack-loading')).toBeInTheDocument());
  });

  it('requests debugexportstack data', async () => {
    fireEvent.click(screen.getByText('Split'));

    await waitFor(() => expectMessageSentToServer({type: 'exportStack', revs: 'e'}));
  });

  it('shows errors', async () => {
    fireEvent.click(screen.getByText('Split'));
    await waitFor(() => expectMessageSentToServer({type: 'exportStack', revs: 'e'}));
    act(() => {
      simulateMessageFromServer({
        type: 'exportedStack',
        revs: 'e',
        assumeTracked: [],
        error: 'test error',
        stack: [],
      });
    });
    await waitFor(() => expect(screen.getByText('test error')).toBeInTheDocument());
  });

  it('waits for existing commands to finish running before loading stack', async () => {
    fireEvent.click(screen.getByText('Pull', {selector: 'button'}));
    const message = await waitFor(() =>
      utils.nullthrows(getLastMessageOfTypeSentToServer('runOperation')),
    );
    const id = message.operation.id;

    fireEvent.click(screen.getByText('Split'));

    expectMessageNOTSentToServer({type: 'exportStack', revs: 'e'});

    act(() =>
      simulateMessageFromServer({
        type: 'operationProgress',
        id,
        kind: 'exit',
        exitCode: 0,
        timestamp: 0,
      }),
    );

    await waitFor(() => expectMessageSentToServer({type: 'exportStack', revs: 'e'}));
  });

  it('opens split modal when the server requests it', async () => {
    act(() => {
      simulateMessageFromServer({type: 'openSplitViewForCommit', commitHash: 'e'});
    });
    await waitFor(() => expectMessageSentToServer({type: 'exportStack', revs: 'e'}));
    act(() => {
      simulateMessageFromServer({
        type: 'exportedStack',
        revs: 'e',
        assumeTracked: [],
        error: undefined,
        stack: EXPORT_STACK_DATA,
      });
    });
    await waitFor(() => expect(screen.getByTestId('interactive-split-modal')).toBeInTheDocument());
  });

  describe('with loaded stack data', () => {
    beforeEach(async () => {
      fireEvent.click(screen.getByText('Split'));
      await waitFor(() => expectMessageSentToServer({type: 'exportStack', revs: 'e'}));
      act(() => {
        simulateMessageFromServer({
          type: 'exportedStack',
          revs: 'e',
          assumeTracked: [],
          error: undefined,
          stack: EXPORT_STACK_DATA,
        });
      });
      await waitFor(() =>
        expect(screen.getByTestId('interactive-split-modal')).toBeInTheDocument(),
      );
    });

    it('loads exported stack into UI', () => {
      expect(
        within(screen.getByTestId('interactive-split-modal')).getByText('myFile.js'),
      ).toBeInTheDocument();

      expect(
        within(screen.getByTestId('interactive-split-modal')).getByText('hello'),
      ).toBeInTheDocument();

      expect(
        within(screen.getByTestId('interactive-split-modal')).getByText('hello (changed)'),
      ).toBeInTheDocument();
    });

    it('moves lines and requests importing', async () => {
      jest.useFakeTimers().setSystemTime(new Date('2020-01-01'));

      const arrows = screen.getAllByTitle('Move this line change right');
      fireEvent.click(arrows[1]);
      fireEvent.click(screen.getByTestId('confirm-edit-stack-button'));

      const message = await waitFor(() =>
        utils.nullthrows(getLastMessageOfTypeSentToServer('runOperation')),
      );
      const id = message.operation.id;

      expectMessageSentToServer({
        type: 'runOperation',
        operation: {
          id,
          trackEventName: 'ImportStackOperation',
          args: ['debugimportstack'],
          stdin: JSON.stringify([
            [
              'commit',
              {
                mark: ':r1',
                author: 'username',
                date: [1577836800, 25200],
                text: 'Commit E',
                parents: ['d'],
                predecessors: ['e'],
                files: {'myFile.js': {data: 'world!\n', flags: ''}},
              },
            ],
            [
              'commit',
              {
                mark: ':r2',
                author: 'username',
                date: [1577836800, 25200],
                text: 'Split of "Commit E"',
                parents: [':r1'],
                predecessors: ['e'],
                files: {'myFile.js': {data: 'hello (changed)\nworld!\n', flags: ''}},
              },
            ],
            ['goto', {mark: ':r2'}],
          ]),
          runner: CommandRunner.Sapling,
        },
      });
    });
  });
});
