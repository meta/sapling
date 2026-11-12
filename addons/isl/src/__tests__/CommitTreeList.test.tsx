/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import {act, fireEvent, render, screen, waitFor, within} from '@testing-library/react';
import {ErrorShortMessages} from 'isl-server/src/constants';
import App from '../App';
import {scrollToYouAreHereOnOpen} from '../CommitTreeList';
import {writeAtom} from '../jotaiUtils';
import {CommitInfoTestUtils, ignoreRTL} from '../testQueries';
import {
  closeCommitInfoSidebar,
  COMMIT,
  commitInfoIsOpen,
  expectMessageSentToServer,
  openCommitInfoSidebar,
  resetTestMessages,
  simulateCommits,
  simulateRepoConnected,
  simulateUncommittedChangedFiles,
} from '../testUtils';
import {CommandRunner} from '../types';

describe('CommitTreeList', () => {
  beforeEach(() => {
    resetTestMessages();
    writeAtom(scrollToYouAreHereOnOpen, false);
    jest.mocked(HTMLElement.prototype.scrollIntoView).mockClear();
  });

  it('shows loading spinner on mount', () => {
    render(<App />);

    expect(screen.getByTestId('loading-spinner')).toBeInTheDocument();
  });

  it('shows bug button even on error', () => {
    render(<App />);

    act(() => {
      simulateCommits({
        error: new Error('invalid certificate'),
      });
    });

    expect(screen.getByTestId('bug-button')).toBeInTheDocument();
    expect(screen.getByText('invalid certificate')).toBeInTheDocument();
  });

  it('explains how to recover when there are too many commits', () => {
    render(<App />);

    act(() => {
      simulateCommits({
        error: new Error(ErrorShortMessages.TooManyCommits),
      });
    });

    expect(screen.getByText('Too many commits to render')).toBeInTheDocument();
    expect(
      screen.getByText(
        'Check your public branch configuration, or run `sl doctor` to hide unrelated commits.',
      ),
    ).toBeInTheDocument();
    expect(screen.queryByTestId('loading-spinner')).not.toBeInTheDocument();
  });

  it('renders a limited graph with a warning and clears it after a complete fetch', () => {
    render(<App />);
    const commits = [
      COMMIT('1', 'some public base', '0', {phase: 'public'}),
      COMMIT('a', 'Current commit', '1', {isDot: true}),
    ];
    act(() => {
      simulateRepoConnected();
      closeCommitInfoSidebar();
      simulateCommits({value: commits}, {numCommits: 2000});
    });

    expect(screen.getByTestId('commit-a')).toBeInTheDocument();
    expect(screen.getByTestId('commit-1')).toBeInTheDocument();
    expect(screen.getByText('Showing a limited commit graph')).toBeInTheDocument();
    expect(screen.queryByText('Too many commits to render')).not.toBeInTheDocument();
    expect(screen.queryByTestId('loading-spinner')).not.toBeInTheDocument();

    act(() => {
      simulateCommits({value: commits});
    });

    expect(screen.getByTestId('commit-a')).toBeInTheDocument();
    expect(screen.queryByText('Showing a limited commit graph')).not.toBeInTheDocument();
  });

  it('renders a truncated stack whose parent is absent instead of showing a spinner', () => {
    render(<App />);
    act(() => {
      simulateRepoConnected();
      closeCommitInfoSidebar();
      simulateCommits(
        {value: [COMMIT('a', 'Current commit', 'missing-parent', {isDot: true})]},
        {numCommits: 2000},
      );
    });

    expect(screen.getByTestId('commit-a')).toBeInTheDocument();
    expect(screen.getByText('Showing a limited commit graph')).toBeInTheDocument();
    expect(screen.queryByTestId('loading-spinner')).not.toBeInTheDocument();
  });

  it('keeps the warning when a failed refetch retains the limited graph', () => {
    render(<App />);
    act(() => {
      simulateRepoConnected();
      closeCommitInfoSidebar();
      simulateCommits(
        {value: [COMMIT('a', 'Current commit', '0', {isDot: true})]},
        {numCommits: 2000},
      );
      simulateCommits({error: new Error('invalid certificate')});
    });

    expect(screen.getByTestId('commit-a')).toBeInTheDocument();
    expect(screen.getByText('Showing a limited commit graph')).toBeInTheDocument();
    expect(screen.getByText('invalid certificate')).toBeInTheDocument();
  });

  it('auto-scrolls to the current commit when enabled', async () => {
    render(<App />);
    act(() => {
      writeAtom(scrollToYouAreHereOnOpen, true);
      simulateRepoConnected();
      closeCommitInfoSidebar();
      simulateCommits({
        value: [
          COMMIT('1', 'some public base', '0', {phase: 'public'}),
          COMMIT('a', 'My Commit', '1', {isDot: true}),
        ],
      });
    });

    await waitFor(() => {
      expect(HTMLElement.prototype.scrollIntoView).toHaveBeenCalledTimes(1);
      expect(HTMLElement.prototype.scrollIntoView).toHaveBeenCalledWith({
        behavior: 'auto',
        block: 'start',
      });
    });
  });

  it('highlights only the commit with an open context menu', () => {
    render(<App />);
    act(() => {
      closeCommitInfoSidebar();
      simulateCommits({
        value: [
          COMMIT('a', 'Commit A', '1'),
          COMMIT('b', 'Commit B', 'a', {isDot: true}),
          COMMIT('1', 'some public base', '0', {phase: 'public'}),
        ],
      });
    });

    const commitA = screen.getByTestId('commit-a');
    const commitB = screen.getByTestId('commit-b');

    fireEvent.contextMenu(commitA);
    expect(commitA.querySelector('.commit-rows')).toHaveClass('commit-row-actioning');
    expect(commitB.querySelector('.commit-rows')).not.toHaveClass('commit-row-actioning');

    fireEvent.contextMenu(commitB);
    expect(commitA.querySelector('.commit-rows')).not.toHaveClass('commit-row-actioning');
    expect(commitB.querySelector('.commit-rows')).toHaveClass('commit-row-actioning');

    fireEvent.keyUp(window, {key: 'Escape'});
    expect(commitB.querySelector('.commit-rows')).not.toHaveClass('commit-row-actioning');
  });

  it('clears the top bar height when it unmounts', () => {
    const {unmount} = render(<App />);
    act(() => {
      simulateRepoConnected();
      simulateCommits({
        value: [COMMIT('1', 'some public base', '0', {phase: 'public', isDot: true})],
      });
    });

    expect(document.body.style.getPropertyValue('--top-bar-height')).not.toBe('');
    unmount();
    expect(document.body.style.getPropertyValue('--top-bar-height')).toBe('');
  });

  describe('scroll to current commit button', () => {
    const defaultIntersectionObserver = global.IntersectionObserver;
    const defaultResizeObserver = global.ResizeObserver;

    class TestIntersectionObserver {
      static instances: Array<TestIntersectionObserver> = [];

      readonly root: Element | Document | null;
      readonly rootMargin: string;
      readonly thresholds = [0];
      readonly targets = new Set<Element>();

      constructor(
        private callback: IntersectionObserverCallback,
        options?: IntersectionObserverInit,
      ) {
        this.root = options?.root ?? null;
        this.rootMargin = options?.rootMargin ?? '0px';
        TestIntersectionObserver.instances.push(this);
      }

      observe(target: Element) {
        this.targets.add(target);
      }

      unobserve(target: Element) {
        this.targets.delete(target);
      }

      disconnect() {
        this.targets.clear();
      }

      takeRecords(): Array<IntersectionObserverEntry> {
        return [];
      }

      trigger() {
        this.callback([], this as unknown as IntersectionObserver);
      }
    }

    class TestResizeObserver {
      static instances: Array<TestResizeObserver> = [];

      readonly targets = new Set<Element>();

      constructor(private callback: ResizeObserverCallback) {
        TestResizeObserver.instances.push(this);
      }

      observe(target: Element) {
        this.targets.add(target);
      }

      unobserve(target: Element) {
        this.targets.delete(target);
      }

      disconnect() {
        this.targets.clear();
      }

      trigger() {
        this.callback([], this as unknown as ResizeObserver);
      }
    }

    type Layout = {
      clientHeight: number;
      scrollHeight: number;
      rootTop: number;
      rootHeight?: number;
      clientTop?: number;
      offsetHeight?: number;
      targetTop: number;
      targetHeight: number;
    };

    function mockLayout(root: HTMLElement, target: HTMLElement, initial: Layout) {
      const layout = {...initial};
      Object.defineProperties(root, {
        clientHeight: {configurable: true, get: () => layout.clientHeight},
        clientTop: {configurable: true, get: () => layout.clientTop ?? 0},
        offsetHeight: {
          configurable: true,
          get: () => layout.offsetHeight ?? layout.rootHeight ?? layout.clientHeight,
        },
        scrollHeight: {configurable: true, get: () => layout.scrollHeight},
      });
      jest.spyOn(root, 'getBoundingClientRect').mockImplementation(() => ({
        x: 0,
        y: layout.rootTop,
        width: 500,
        height: layout.rootHeight ?? layout.clientHeight,
        top: layout.rootTop,
        right: 500,
        bottom: layout.rootTop + (layout.rootHeight ?? layout.clientHeight),
        left: 0,
        toJSON: () => ({}),
      }));
      jest.spyOn(target, 'getBoundingClientRect').mockImplementation(() => ({
        x: 0,
        y: layout.targetTop,
        width: 200,
        height: layout.targetHeight,
        top: layout.targetTop,
        right: 200,
        bottom: layout.targetTop + layout.targetHeight,
        left: 0,
        toJSON: () => ({}),
      }));
      return (next: Partial<Layout>) => Object.assign(layout, next);
    }

    function getMarkerAndViewport() {
      const marker = document.getElementById('isl-you-are-here-anchor')?.parentElement;
      const viewport = document.querySelector<HTMLElement>('.main-content-area');
      const tree = document.querySelector<HTMLElement>('.commit-tree-root');
      if (marker == null || viewport == null || tree == null) {
        throw new Error('Expected the current commit marker and commit viewport to be rendered');
      }
      return {marker, viewport, tree};
    }

    function triggerIntersection(marker: HTMLElement) {
      const observer = TestIntersectionObserver.instances.find(instance =>
        instance.targets.has(marker),
      );
      if (observer == null) {
        throw new Error('Expected the current commit marker to be observed');
      }
      act(() => observer.trigger());
    }

    beforeEach(() => {
      TestIntersectionObserver.instances = [];
      TestResizeObserver.instances = [];
      global.IntersectionObserver =
        TestIntersectionObserver as unknown as typeof IntersectionObserver;
      global.ResizeObserver = TestResizeObserver as unknown as typeof ResizeObserver;

      render(<App />);
      act(() => {
        simulateRepoConnected();
        closeCommitInfoSidebar();
        simulateCommits({
          value: [
            COMMIT('1', 'some public base', '0', {phase: 'public'}),
            COMMIT('a', 'My Commit', '1', {isDot: true}),
          ],
        });
      });
    });

    afterAll(() => {
      global.IntersectionObserver = defaultIntersectionObserver;
      global.ResizeObserver = defaultResizeObserver;
    });

    it('tracks vertical intersection independently of horizontal clipping', () => {
      const {marker} = getMarkerAndViewport();
      const observer = TestIntersectionObserver.instances.find(instance =>
        instance.targets.has(marker),
      );

      expect(observer?.rootMargin).toBe('0px 100000px');
    });

    it('stays hidden when the commit viewport cannot scroll', () => {
      const {marker, viewport} = getMarkerAndViewport();
      mockLayout(viewport, marker, {
        clientHeight: 500,
        scrollHeight: 500,
        rootTop: 0,
        targetTop: 600,
        targetHeight: 20,
      });

      triggerIntersection(marker);

      expect(screen.queryByTestId('scroll-to-current-commit-button')).not.toBeInTheDocument();
    });

    it.each([
      {position: 'above', targetTop: -40, expectedClass: 'floating-top'},
      {position: 'below', targetTop: 120, expectedClass: 'floating-bottom'},
    ])('shows the $position button when the viewport can scroll', ({targetTop, expectedClass}) => {
      const {marker, viewport} = getMarkerAndViewport();
      mockLayout(viewport, marker, {
        clientHeight: 100,
        scrollHeight: 500,
        rootTop: 0,
        targetTop,
        targetHeight: 20,
      });

      triggerIntersection(marker);

      expect(
        screen.getByTestId('scroll-to-current-commit-button').closest('.scroll-to-current-commit'),
      ).toHaveClass(expectedClass);
    });

    it('stays hidden for non-intersection that is not vertically off-screen', () => {
      const {marker, viewport} = getMarkerAndViewport();
      mockLayout(viewport, marker, {
        clientHeight: 100,
        scrollHeight: 500,
        rootTop: 0,
        targetTop: 40,
        targetHeight: 20,
      });

      triggerIntersection(marker);

      expect(screen.queryByTestId('scroll-to-current-commit-button')).not.toBeInTheDocument();
    });

    it('uses the inner scrollport rather than the scrollbar area', () => {
      const {marker, viewport} = getMarkerAndViewport();
      mockLayout(viewport, marker, {
        clientHeight: 100,
        scrollHeight: 500,
        rootTop: 0,
        rootHeight: 115,
        targetTop: 105,
        targetHeight: 5,
      });

      triggerIntersection(marker);

      expect(
        screen.getByTestId('scroll-to-current-commit-button').closest('.scroll-to-current-commit'),
      ).toHaveClass('floating-bottom');
    });

    it('accounts for CSS zoom when measuring the inner scrollport', () => {
      const {marker, viewport} = getMarkerAndViewport();
      mockLayout(viewport, marker, {
        clientHeight: 100,
        scrollHeight: 500,
        rootTop: 0,
        rootHeight: 50,
        offsetHeight: 100,
        targetTop: 55,
        targetHeight: 5,
      });

      triggerIntersection(marker);

      expect(
        screen.getByTestId('scroll-to-current-commit-button').closest('.scroll-to-current-commit'),
      ).toHaveClass('floating-bottom');
    });

    it.each([
      {position: 'above', offscreenTop: -21, edgeTop: -20},
      {position: 'below', offscreenTop: 101, edgeTop: 100},
    ])(
      'hides the $position button when the marker reaches the viewport edge',
      ({offscreenTop, edgeTop}) => {
        const {marker, viewport} = getMarkerAndViewport();
        const updateLayout = mockLayout(viewport, marker, {
          clientHeight: 100,
          scrollHeight: 500,
          rootTop: 0,
          targetTop: offscreenTop,
          targetHeight: 20,
        });
        triggerIntersection(marker);
        expect(screen.getByTestId('scroll-to-current-commit-button')).toBeInTheDocument();

        updateLayout({targetTop: edgeTop});
        triggerIntersection(marker);

        expect(screen.queryByTestId('scroll-to-current-commit-button')).not.toBeInTheDocument();
      },
    );

    it('hides when the commit tree shrinks until the viewport cannot scroll', () => {
      const {marker, viewport, tree} = getMarkerAndViewport();
      const updateLayout = mockLayout(viewport, marker, {
        clientHeight: 100,
        scrollHeight: 500,
        rootTop: 0,
        targetTop: 120,
        targetHeight: 20,
      });
      triggerIntersection(marker);
      expect(screen.getByTestId('scroll-to-current-commit-button')).toBeInTheDocument();

      updateLayout({scrollHeight: 100});
      const observer = TestResizeObserver.instances.find(instance => instance.targets.has(tree));
      if (observer == null) {
        throw new Error('Expected the commit tree to be observed for size changes');
      }
      act(() => observer.trigger());

      expect(screen.queryByTestId('scroll-to-current-commit-button')).not.toBeInTheDocument();
    });
  });

  describe('after commits loaded', () => {
    beforeEach(() => {
      render(<App />);
      act(() => {
        simulateRepoConnected();
        closeCommitInfoSidebar();
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

    it('renders commits', () => {
      expect(screen.getByText('My Commit')).toBeInTheDocument();
      expect(screen.getByText('Another Commit')).toBeInTheDocument();
      expect(screen.queryByText('some public base')).not.toBeInTheDocument();
    });

    it('renders exactly one head', () => {
      expect(screen.getByText('You are here')).toBeInTheDocument();
    });

    describe('uncommitted changes', () => {
      beforeEach(() => {
        act(() => {
          expectMessageSentToServer({
            type: 'subscribe',
            kind: 'uncommittedChanges',
            subscriptionID: expect.anything(),
          });
          simulateUncommittedChangedFiles({
            value: [
              {path: 'src/file.js', status: 'M'},
              {path: 'src/file_add.js', status: 'A'},
              {path: 'src/file_removed.js', status: 'R'},
              {path: 'src/file_untracked.js', status: '?'},
              {path: 'src/file_missing.js', status: '!'},
            ],
          });
        });
      });

      it('renders uncommitted changes', () => {
        expect(screen.getByText(ignoreRTL('file.js'), {exact: false})).toBeInTheDocument();
        expect(screen.getByText(ignoreRTL('file_add.js'), {exact: false})).toBeInTheDocument();
        expect(screen.getByText(ignoreRTL('file_removed.js'), {exact: false})).toBeInTheDocument();
        expect(
          screen.getByText(ignoreRTL('file_untracked.js'), {exact: false}),
        ).toBeInTheDocument();
        expect(screen.getByText(ignoreRTL('file_missing.js'), {exact: false})).toBeInTheDocument();
      });

      it('shows quick commit button', () => {
        expect(screen.getByText('Commit')).toBeInTheDocument();
      });
      it('shows quick amend button only on non-public commits', () => {
        expect(screen.getByText('Amend')).toBeInTheDocument();
        // checkout a public commit
        act(() => {
          simulateCommits({
            value: [
              COMMIT('1', 'some public base', '0', {phase: 'public', isDot: true}),
              COMMIT('a', 'My Commit', '1', {successorInfo: {hash: 'a2', type: 'land'}}),
              COMMIT('b', 'Another Commit', 'a'),
            ],
          });
        });
        // no longer see quick amend
        expect(screen.queryByText('Amend')).not.toBeInTheDocument();
      });

      it('shows file actions', () => {
        const fileActions = screen.getAllByTestId('file-actions');
        expect(fileActions).toHaveLength(5); // 5 files
        const revertButtons = screen.getAllByTestId('file-revert-button');
        expect(revertButtons).toHaveLength(3); // modified, removed, missing files can be reverted
      });

      it('runs revert command when clicking revert button', async () => {
        const revertButtons = screen.getAllByTestId('file-revert-button');
        act(() => {
          fireEvent.click(revertButtons[0]);
        });
        const dialog = await screen.findByRole('dialog', {name: 'Revert File?'});
        fireEvent.click(within(dialog).getByRole('button', {name: 'Revert'}));
        await waitFor(() => {
          expectMessageSentToServer({
            type: 'runOperation',
            operation: {
              args: ['revert', {type: 'repo-relative-file-list', paths: ['src/file.js']}],
              id: expect.anything(),
              runner: CommandRunner.Sapling,
              trackEventName: 'RevertOperation',
            },
          });
        });
      });

      describe('addremove', () => {
        it('hides addremove if all files tracked', () => {
          act(() => {
            simulateUncommittedChangedFiles({
              value: [
                {path: 'src/file.js', status: 'M'},
                {path: 'src/file_add.js', status: 'A'},
                {path: 'src/file_removed.js', status: 'R'},
              ],
            });
          });
          expect(screen.queryByTestId('addremove-button')).not.toBeInTheDocument();

          act(() => {
            simulateUncommittedChangedFiles({
              value: [
                {path: 'src/file.js', status: 'M'},
                {path: 'src/file_add.js', status: 'A'},
                {path: 'src/file_removed.js', status: 'R'},
                {path: 'src/file_untracked.js', status: '?'},
                {path: 'src/file_missing.js', status: '!'},
              ],
            });
          });
          expect(screen.queryByTestId('addremove-button')).toBeInTheDocument();
        });

        it('runs addremove', async () => {
          const addremove = screen.getByTestId('addremove-button');
          act(() => {
            fireEvent.click(addremove);
          });
          await waitFor(() => {
            expectMessageSentToServer({
              type: 'runOperation',
              operation: {
                args: ['addremove'],
                id: expect.anything(),
                runner: CommandRunner.Sapling,
                trackEventName: 'AddRemoveOperation',
              },
            });
          });
        });

        it('optimistically updates file statuses while addremove is running', async () => {
          const addremove = screen.getByTestId('addremove-button');
          act(() => {
            fireEvent.click(addremove);
          });
          await waitFor(() => {
            expectMessageSentToServer({
              type: 'runOperation',
              operation: {
                args: ['addremove'],
                id: expect.anything(),
                runner: CommandRunner.Sapling,
                trackEventName: 'AddRemoveOperation',
              },
            });
          });

          expect(
            document.querySelectorAll('.changed-files .changed-file.file-ignored'),
          ).toHaveLength(0);
        });

        it('runs addremove only on selected files that are untracked', async () => {
          const ignoredFileCheckboxes = document.querySelectorAll(
            '.changed-files .changed-file.file-ignored input[type="checkbox"]',
          );
          const missingFileCheckboxes = document.querySelectorAll(
            '.changed-files .changed-file.file-missing input[type="checkbox"]',
          );
          expect(ignoredFileCheckboxes).toHaveLength(1); // file_untracked.js
          expect(missingFileCheckboxes).toHaveLength(1); // file_missing.js
          act(() => {
            fireEvent.click(missingFileCheckboxes[0]);
          });

          const addremove = screen.getByTestId('addremove-button');
          act(() => {
            fireEvent.click(addremove);
          });
          await waitFor(() => {
            expectMessageSentToServer({
              type: 'runOperation',
              operation: {
                // note: although src/file.js & others are selected, they aren't passed to addremove as they aren't untracked
                args: ['addremove', {path: 'src/file_untracked.js', type: 'repo-relative-file'}],
                id: expect.anything(),
                runner: CommandRunner.Sapling,
                trackEventName: 'AddRemoveOperation',
              },
            });
          });
        });
      });
    });

    it('shows log errors', () => {
      act(() => {
        simulateCommits({
          error: new Error('error running log'),
        });
      });
      expect(screen.getByText('Failed to fetch commits')).toBeInTheDocument();
      expect(screen.getByText('error running log')).toBeInTheDocument();

      // we should still have commits from the last successful fetch
      expect(screen.getByText('My Commit')).toBeInTheDocument();
      expect(screen.getByText('Another Commit')).toBeInTheDocument();
      expect(screen.queryByText('some public base')).not.toBeInTheDocument();
    });

    it('shows status errors', () => {
      act(() => {
        simulateUncommittedChangedFiles({
          error: new Error('error running status'),
        });
      });
      expect(screen.getByText('Failed to fetch Uncommitted Changes')).toBeInTheDocument();
      expect(screen.getByText('error running status')).toBeInTheDocument();
    });

    it('shows successor info', () => {
      act(() => {
        simulateCommits({
          value: [
            COMMIT('1', 'some public base', '0', {phase: 'public'}),
            COMMIT('a', 'My Commit', '1', {successorInfo: {hash: 'a2', type: 'land'}}),
            COMMIT('b', 'Another Commit', 'a', {isDot: true}),
          ],
        });
      });
      expect(screen.getByText('Landed as a newer commit', {exact: false})).toBeInTheDocument();
    });

    it('shows button to open sidebar', () => {
      act(() => {
        simulateCommits({
          value: [
            COMMIT('1', 'some public base', '0', {phase: 'public'}),
            COMMIT('a', 'Commit A', '1', {isDot: true}),
            COMMIT('b', 'Commit B', '1'),
          ],
        });
      });
      expect(commitInfoIsOpen()).toBeFalsy();

      // doesn't appear for public commits
      expect(
        within(screen.getByTestId('commit-1')).queryByTestId('open-commit-info-button'),
      ).not.toBeInTheDocument();

      const openButton = within(screen.getByTestId('commit-b')).getByTestId(
        'open-commit-info-button',
      );
      expect(openButton).toBeInTheDocument();
      // screen reader accessible
      expect(screen.getByLabelText('Open commit "Commit B"')).toBeInTheDocument();
      fireEvent.click(openButton);
      expect(commitInfoIsOpen()).toBeTruthy();
      expect(CommitInfoTestUtils.withinCommitInfo().getByText('Commit B')).toBeInTheDocument();
    });

    it('sets to amend mode when clicking open button', () => {
      act(() => {
        simulateCommits({
          value: [
            COMMIT('1', 'some public base', '0', {phase: 'public'}),
            COMMIT('a', 'Commit A', '1', {isDot: true}),
            COMMIT('b', 'Commit B', '1'),
          ],
        });
      });
      act(() => {
        openCommitInfoSidebar();
      });
      CommitInfoTestUtils.clickCommitMode();

      const openButton = within(screen.getByTestId('commit-b')).getByTestId(
        'open-commit-info-button',
      );
      expect(openButton).toBeInTheDocument();
      fireEvent.click(openButton);
      expect(commitInfoIsOpen()).toBeTruthy();
      expect(CommitInfoTestUtils.withinCommitInfo().getByText('Commit B')).toBeInTheDocument();
    });
  });

  describe('render dag subset', () => {
    describe('irrelevant cwd stacks', () => {
      beforeEach(() => {
        render(<App />);
        act(() => {
          // Set repo root to "/repo" and cwd to "/repo/www"
          simulateRepoConnected('/repo', '/repo/www');
          closeCommitInfoSidebar();
          expectMessageSentToServer({
            type: 'subscribe',
            kind: 'smartlogCommits',
            subscriptionID: expect.anything(),
          });
          simulateCommits({
            value: [
              COMMIT('1', 'Public base', '0', {phase: 'public'}),
              // Commits with different maxCommonPathPrefix values
              COMMIT('a', 'Commit in www', '1', {maxCommonPathPrefix: 'www/'}),
              COMMIT('b', 'Commit in addons', '1', {maxCommonPathPrefix: 'addons/'}),
              COMMIT('c', 'Commit in root', '1', {maxCommonPathPrefix: ''}),
              COMMIT('d', 'Commit in www/js', '1', {
                maxCommonPathPrefix: 'www/js/',
                isDot: true,
              }),
            ],
          });
        });
      });

      it('shows irrelevant commits by default', () => {
        // By default, hideIrrelevantCwdStacks is false, so all commits should be visible
        expect(screen.queryByText('Commit in www')).toBeInTheDocument();
        expect(screen.queryByText('Commit in addons')).toBeInTheDocument();
        expect(screen.queryByText('Commit in root')).toBeInTheDocument();
        expect(screen.queryByText('Commit in www/js')).toBeInTheDocument();

        // But the addons/ commit should be marked as irrelevant
        expect(document.body.querySelectorAll('.commit.irrelevant')).toHaveLength(1);
      });

      it('can hide irrelevant commits when enabled', () => {
        fireEvent.click(screen.getByTestId('settings-gear-button'));

        const settingsDropdown = screen.getByTestId('settings-dropdown');
        expect(settingsDropdown).toBeInTheDocument();
        const cwdIrrelevantDropdown =
          within(settingsDropdown).getByTestId('cwd-irrelevant-commits');
        expect(cwdIrrelevantDropdown).toBeInTheDocument();

        // Change the dropdown value to 'hide'
        fireEvent.change(cwdIrrelevantDropdown, {target: {value: 'hide'}});

        expect(screen.queryByText('Commit in www')).toBeInTheDocument();
        expect(screen.queryByText('Commit in root')).toBeInTheDocument();
        expect(screen.queryByText('Commit in www/js')).toBeInTheDocument();

        expect(screen.queryByText('Commit in addons')).not.toBeInTheDocument();
      });
    });

    describe('obsolete stacks', () => {
      beforeEach(() => {
        render(<App />);
        act(() => {
          simulateRepoConnected();
          closeCommitInfoSidebar();
          expectMessageSentToServer({
            type: 'subscribe',
            kind: 'smartlogCommits',
            subscriptionID: expect.anything(),
          });
          simulateCommits({
            value: [
              COMMIT('1', 'some public base', '0', {phase: 'public'}),
              COMMIT('a', 'Commit A', '1', {successorInfo: {hash: 'a2', type: 'rebase'}}),
              COMMIT('b', 'Commit B', 'a', {successorInfo: {hash: 'b2', type: 'rebase'}}),
              COMMIT('c', 'Commit C', 'b', {successorInfo: {hash: 'c2', type: 'rebase'}}),
              COMMIT('d', 'Commit D', 'c', {successorInfo: {hash: 'd2', type: 'rebase'}}),
              COMMIT('e', 'Commit E', 'd', {isDot: true}),
            ],
          });
        });
      });
      it('hides obsolete stacks by default', () => {
        expect(screen.queryByText('Commit A')).toBeInTheDocument();
        expect(screen.queryByText('Commit B')).not.toBeInTheDocument();
        expect(screen.queryByText('Commit C')).not.toBeInTheDocument();
        expect(screen.queryByText('Commit D')).toBeInTheDocument();
        expect(screen.queryByText('Commit E')).toBeInTheDocument();
      });

      it('can configure to not hide obsolete stacks', () => {
        fireEvent.click(screen.getByTestId('settings-gear-button'));
        fireEvent.click(screen.getByTestId('condense-obsolete-stacks'));
        expect(screen.queryByText('Commit A')).toBeInTheDocument();
        expect(screen.queryByText('Commit B')).toBeInTheDocument();
        expect(screen.queryByText('Commit C')).toBeInTheDocument();
        expect(screen.queryByText('Commit D')).toBeInTheDocument();
        expect(screen.queryByText('Commit E')).toBeInTheDocument();
      });
    });
  });
});
