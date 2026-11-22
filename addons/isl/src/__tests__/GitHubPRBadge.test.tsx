/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import type {DiffId, DiffSummary} from '../types';

import {act, fireEvent, render, screen, within} from '@testing-library/react';
import {PullRequestState} from '../../../isl-server/src/github/generated/graphql';
import App from '../App';
import platform from '../platform';
import {
  COMMIT,
  closeCommitInfoSidebar,
  expectMessageSentToServer,
  resetTestMessages,
  simulateCommits,
  simulateMessageFromServer,
  suppressReactErrorBoundaryErrorMessages,
} from '../testUtils';

describe('GitHubPRBadge', () => {
  beforeEach(() => {
    resetTestMessages();
    render(<App />);
    act(() => {
      closeCommitInfoSidebar();
      simulateCommits({
        value: [
          COMMIT('1', 'some public base', '0', {phase: 'public', diffId: '2'}),
          COMMIT('a', 'Commit A', '1', {diffId: '10'}),
          COMMIT('b', 'Commit B', 'a', {isDot: true, diffId: '11'}),
          COMMIT('c', 'Commit C', '1'),
        ],
      });
    });
  });

  it("doesn't render any github component when we don't know the remote repo provider", () => {
    expect(screen.queryAllByTestId('diff-spinner')).toHaveLength(0);
  });

  describe('in github repo', () => {
    beforeEach(() => {
      act(() => {
        simulateMessageFromServer({
          type: 'repoInfo',
          info: {
            type: 'success',
            command: 'sl',
            repoRoot: '/path/to/testrepo',
            dotdir: '/path/to/testrepo/.sl',
            codeReviewSystem: {
              type: 'github',
              repo: 'testrepo',
              owner: 'myusername',
              hostname: 'github.com',
            },
            pullRequestDomain: undefined,
            preferredSubmitCommand: 'pr',
            isEdenFs: false,
          },
        });
      });
    });

    it('requests a diff fetch on mount', () => {
      expectMessageSentToServer({
        type: 'fetchDiffSummaries',
      });
    });

    it('renders spinners for commits with github PRs', () => {
      expect(screen.queryAllByTestId('diff-spinner')).toHaveLength(2);
    });

    describe('after PRs loaded', () => {
      beforeEach(() => {
        act(() => {
          simulateMessageFromServer({
            type: 'fetchedDiffSummaries',
            summaries: {
              value: new Map<DiffId, DiffSummary>([
                [
                  '10',
                  {
                    number: '10',
                    state: PullRequestState.Open,
                    title: 'PR A',
                    type: 'github',
                    url: 'https://github.com/myusername/testrepo/pull/10',
                    anyUnresolvedComments: false,
                    commentCount: 0,
                    commitMessage: 'PR A',
                    base: '1',
                    head: 'a',
                    branchName: 'origin/sl-pr-a',
                  },
                ],
                [
                  '11',
                  {
                    number: '11',
                    state: PullRequestState.Closed,
                    title: 'PR B',
                    type: 'github',
                    url: 'https://github.com/myusername/testrepo/pull/11',
                    anyUnresolvedComments: false,
                    commentCount: 0,
                    commitMessage: 'PR B',
                    base: 'a',
                    head: 'b',
                    branchName: 'origin/sl-pr-b',
                  },
                ],
              ]),
            },
          });
        });
      });

      it("doesn't show spinners anymore", () => {
        expect(screen.queryByTestId('diff-spinner')).not.toBeInTheDocument();
      });

      it('renders PR badges', () => {
        expect(screen.queryAllByTestId('github-diff-info')).toHaveLength(2);
        expect(
          within(
            within(screen.getByTestId('commit-a')).getByTestId('github-diff-info'),
          ).queryByText('Open'),
        ).toBeInTheDocument();
        expect(
          within(
            within(screen.getByTestId('commit-b')).getByTestId('github-diff-info'),
          ).queryByText('Closed'),
        ).toBeInTheDocument();
      });

      it('does not render PR badge for public commits', () => {
        expect(
          within(screen.getByTestId('commit-1')).queryByTestId('github-diff-info'),
        ).not.toBeInTheDocument();
      });

      it('shows PR numbers if config enabled', () => {
        expect(screen.queryAllByTestId('github-diff-info')).toHaveLength(2);
        act(() => {
          simulateMessageFromServer({
            type: 'gotConfig',
            name: 'isl.show-diff-number',
            value: 'true',
          });
        });
        expect(
          within(
            within(screen.getByTestId('commit-a')).getByTestId('github-diff-info'),
          ).queryByText('#10'),
        ).toBeInTheDocument();
        expect(
          within(
            within(screen.getByTestId('commit-b')).getByTestId('github-diff-info'),
          ).queryByText('#11'),
        ).toBeInTheDocument();
      });

      describe('url opener', () => {
        beforeEach(() => {
          jest.spyOn(platform, 'openExternalLink').mockImplementation(() => undefined);
        });

        it('opens with default uri', () => {
          const prA = within(
            within(screen.getByTestId('commit-a')).getByTestId('github-diff-info'),
          ).getByText('open', {exact: false});
          fireEvent.click(prA);

          expect(platform.openExternalLink).toHaveBeenCalledWith(
            'https://github.com/myusername/testrepo/pull/10',
          );
        });

        it('uses custom opener', () => {
          act(() => {
            simulateMessageFromServer({
              type: 'repoInfo',
              info: {
                type: 'success',
                command: 'sl',
                repoRoot: '/path/to/testrepo',
                dotdir: '/path/to/testrepo/.sl',
                codeReviewSystem: {
                  type: 'github',
                  repo: 'testrepo',
                  owner: 'myusername',
                  hostname: 'github.com',
                },
                pullRequestDomain: 'https://myreviewsite.dev',
                preferredSubmitCommand: 'pr',
                isEdenFs: false,
              },
            });
          });

          const prA = within(
            within(screen.getByTestId('commit-a')).getByTestId('github-diff-info'),
          ).getByText('open', {exact: false});
          fireEvent.click(prA);

          expect(platform.openExternalLink).toHaveBeenCalledWith(
            'https://myreviewsite.dev/myusername/testrepo/pull/10',
          );
        });

        it('uses custom opener with relaxed format', () => {
          act(() => {
            simulateMessageFromServer({
              type: 'repoInfo',
              info: {
                type: 'success',
                command: 'sl',
                repoRoot: '/path/to/testrepo',
                dotdir: '/path/to/testrepo/.sl',
                codeReviewSystem: {
                  type: 'github',
                  repo: 'testrepo',
                  owner: 'myusername',
                  hostname: 'github.com',
                },
                // no leading https://, adds custom prefix
                pullRequestDomain: 'myreviewsite.dev/codereview',
                preferredSubmitCommand: 'pr',
                isEdenFs: false,
              },
            });
          });

          const prA = within(
            within(screen.getByTestId('commit-a')).getByTestId('github-diff-info'),
          ).getByText('open', {exact: false});
          fireEvent.click(prA);

          expect(platform.openExternalLink).toHaveBeenCalledWith(
            'https://myreviewsite.dev/codereview/myusername/testrepo/pull/10',
          );
        });
      });
    });

    describe('commit on its own PR branch', () => {
      beforeEach(() => {
        act(() => {
          simulateCommits({
            value: [
              COMMIT('1', 'some public base', '0', {phase: 'public'}),
              COMMIT('a', 'Commit A', '1', {
                isDot: true,
                diffId: '10',
                remoteBookmarks: ['origin/pr10', 'origin/feature'],
              }),
            ],
          });
          simulateMessageFromServer({
            type: 'fetchedDiffSummaries',
            summaries: {
              value: new Map<DiffId, DiffSummary>([
                [
                  '10',
                  {
                    number: '10',
                    state: PullRequestState.Open,
                    title: 'PR A',
                    type: 'github',
                    url: 'https://github.com/myusername/testrepo/pull/10',
                    anyUnresolvedComments: false,
                    commentCount: 0,
                    commitMessage: 'PR A',
                    base: '1',
                    head: 'a',
                    branchName: 'pr10',
                  },
                ],
                [
                  '12',
                  {
                    number: '12',
                    state: PullRequestState.Closed,
                    title: 'Feature PR',
                    type: 'github',
                    url: 'https://github.com/myusername/testrepo/pull/12',
                    anyUnresolvedComments: false,
                    commentCount: 0,
                    commitMessage: 'Feature PR',
                    base: '1',
                    head: 'a',
                    branchName: 'feature',
                  },
                ],
              ]),
            },
          });
        });
      });

      it('renders its own PR badge once', () => {
        const commitA = screen.getByTestId('commit-a');
        expect(within(commitA).getByText('origin/pr10')).toBeInTheDocument();
        expect(within(commitA).queryAllByText('Open')).toHaveLength(1);
      });

      it('still renders badges for other PR branches', () => {
        expect(within(screen.getByTestId('commit-a')).queryAllByText('Closed')).toHaveLength(1);
      });
    });

    describe('after error', () => {
      suppressReactErrorBoundaryErrorMessages();

      beforeEach(() => {
        act(() => {
          simulateMessageFromServer({
            type: 'fetchedDiffSummaries',
            summaries: {
              error: new Error('something failed'),
            },
          });
        });
      });

      it('shows error', () => {
        expect(screen.queryByText('something failed')).toBeInTheDocument();
        expect(screen.queryAllByTestId('github-error')).toHaveLength(2);
      });
    });

    it('can recover from errors', () => {
      const successful = {
        value: new Map<DiffId, DiffSummary>([
          [
            '10',
            {
              number: '10',
              state: PullRequestState.Open,
              title: 'PR A',
              type: 'github',
              url: 'https://github.com/myusername/testrepo/pull/10',
              anyUnresolvedComments: false,
              commentCount: 0,
              commitMessage: 'PR a',
              base: '1',
              head: 'a',
              branchName: 'origin/sl-pr-a',
            },
          ],
        ]),
      };

      const error = {error: new Error('something failed')};

      // start with success
      act(() => {
        simulateMessageFromServer({
          type: 'fetchedDiffSummaries',
          summaries: successful,
        });
      });
      expect(screen.queryByText('something failed')).not.toBeInTheDocument();
      expect(screen.queryAllByTestId('github-error')).toHaveLength(0);
      expect(
        within(within(screen.getByTestId('commit-a')).getByTestId('github-diff-info')).queryByText(
          'Open',
        ),
      ).toBeInTheDocument();

      // simulate error
      act(() => {
        simulateMessageFromServer({
          type: 'fetchedDiffSummaries',
          summaries: error,
        });
      });
      expect(screen.queryByText('something failed')).toBeInTheDocument();
      expect(screen.queryAllByTestId('github-error')).toHaveLength(2);

      // recover from error: get normal data again
      act(() => {
        simulateMessageFromServer({
          type: 'fetchedDiffSummaries',
          summaries: successful,
        });
      });
      expect(screen.queryByText('something failed')).not.toBeInTheDocument();
      expect(screen.queryAllByTestId('github-error')).toHaveLength(0);
      expect(
        within(screen.getByTestId('commit-a')).getByTestId('github-diff-info'),
      ).toBeInTheDocument();
    });
  });
});
