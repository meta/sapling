/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import type {Writable} from 'shared/typeUtils';
import type {TestingEventBus} from './TestingMessageBus';
import type {
  ClientToServerMessage,
  CommitInfo,
  Hash,
  Result,
  ServerToClientMessage,
  SmartlogCommits,
  UncommittedChanges,
  ValidatedRepoInfo,
  WorktreeEntry,
} from './types';

import {act, screen, waitFor, within} from '@testing-library/react';
import {nextTick} from 'shared/utils';
import platform from './platform';
import {deserializeFromString, serializeToString} from './serialize';
import {mostRecentSubscriptionIds} from './serverAPIState';

const testMessageBus = platform.messageBus as TestingEventBus;

export function simulateMessageFromServer(message: ServerToClientMessage): void {
  testMessageBus.simulateMessage(serializeToString(message));
}

/** Filter out binary messages, and filter by wanted type. */
function filterMessages<K extends string>(wantedType?: K) {
  let messages = testMessageBus.sent
    .filter((msg: unknown): msg is string => !(msg instanceof ArrayBuffer))
    .map(deserializeFromString) as Array<ClientToServerMessage>;
  if (wantedType != null) {
    messages = messages.filter(msg => msg.type == null || msg.type === wantedType);
  }
  return messages;
}

export function expectMessageSentToServer(message: Partial<ClientToServerMessage>): void {
  expect(filterMessages(message.type)).toContainEqual(message);
}

export function getMessagesOfTypeSentToServer<K extends string>(
  wantedType: K,
): Array<ClientToServerMessage & {type: K}> {
  return filterMessages(wantedType) as Array<ClientToServerMessage & {type: K}>;
}

export function getLastMessageOfTypeSentToServer<K extends string>(
  wantedType: K,
): (ClientToServerMessage & {type: K}) | undefined {
  const values = getMessagesOfTypeSentToServer(wantedType);
  return values[values.length - 1];
}

export function expectMessageNOTSentToServer(message: Partial<ClientToServerMessage>): void {
  expect(filterMessages(message.type)).not.toContainEqual(message);
}

/**
 * Return last `num` stringified JSON messages sent to the server.
 */
export function getLastMessagesSentToServer(num: number): Array<string> {
  return testMessageBus.sent.slice(-num);
}

export function simulateServerDisconnected(): void {
  testMessageBus.simulateServerStatusChange({type: 'error', error: 'server disconnected'});
}

export function simulateWorktreeInfo(
  sharedRoot: string,
  worktrees: ReadonlyArray<{
    path: string;
    role: 'main' | 'linked';
    label?: string;
    node?: string;
  }>,
  supportsMutation = true,
) {
  act(() => {
    simulateMessageFromServer({
      type: 'subscriptionResult',
      kind: 'worktreeInfo',
      subscriptionID: mostRecentSubscriptionIds.worktreeInfo,
      data: {sharedRoot, worktrees: worktrees as WorktreeEntry[], supportsMutation},
    });
  });
}

export function simulateCommits(
  commits: Result<SmartlogCommits>,
  tooManyCommits?: {numCommits: number},
) {
  simulateMessageFromServer({
    type: 'subscriptionResult',
    kind: 'smartlogCommits',
    subscriptionID: mostRecentSubscriptionIds.smartlogCommits,
    data: {
      fetchStartTimestamp: 1,
      fetchCompletedTimestamp: 2,
      commits,
      tooManyCommits,
    },
  });
}
export function simulateUncommittedChangedFiles(files: Result<UncommittedChanges>) {
  simulateMessageFromServer({
    type: 'subscriptionResult',
    kind: 'uncommittedChanges',
    subscriptionID: mostRecentSubscriptionIds.uncommittedChanges,
    data: {
      fetchStartTimestamp: 1,
      fetchCompletedTimestamp: 2,
      files,
    },
  });
}
export function simulateRepoConnected(
  repoRoot?: string,
  cwd?: string,
  repoInfoOverrides?: Partial<ValidatedRepoInfo>,
) {
  const root = repoRoot ?? '/path/to/repo';
  simulateMessageFromServer({
    type: 'repoInfo',
    info: {
      type: 'success',
      repoRoot: root,
      dotdir: `${root}/.sl`,
      command: 'sl',
      pullRequestDomain: undefined,
      codeReviewSystem: {type: 'github', owner: 'owner', repo: 'repo', hostname: 'github.com'},
      isEdenFs: false,
      ...repoInfoOverrides,
    },
    cwd,
  });
  testMessageBus.simulateServerStatusChange({type: 'open'});
}

export function resetTestMessages() {
  act(() => {
    testMessageBus.resetTestMessages();
  });
}

export function commitInfoIsOpen(): boolean {
  return (
    screen.queryByTestId('commit-info-view') != null ||
    screen.queryByTestId('commit-info-view-loading') != null
  );
}

export function closeCommitInfoSidebar() {
  if (!commitInfoIsOpen()) {
    return;
  }
  screen.queryAllByTestId('drawer-label').forEach(el => {
    const commitInfoTab = within(el).queryByText('Commit Info');
    commitInfoTab?.click();
  });
}

export function openCommitInfoSidebar() {
  if (commitInfoIsOpen()) {
    return;
  }
  screen.queryAllByTestId('drawer-label').forEach(el => {
    const commitInfoTab = within(el).queryByText('Commit Info');
    commitInfoTab?.click();
  });
}

export const emptyCommit: CommitInfo = {
  title: 'title',
  hash: '0',
  parents: [],
  grandparents: [],
  phase: 'draft',
  isDot: false,
  author: 'author',
  date: new Date(),
  description: '',
  bookmarks: [],
  remoteBookmarks: [],
  filePathsSample: [],
  totalFileCount: 0,
  maxCommonPathPrefix: '',
};

export function COMMIT(
  hash: string,
  title: string,
  parent: Hash,
  info?: Partial<CommitInfo>,
): CommitInfo {
  return {
    ...emptyCommit,
    ...info,
    hash,
    title,
    parents: [parent],
  };
}

/**
 ```
    | z - Commit Z
    | |
    | y - Commit Y
    | |
    | x - Commit X
    |/
    2 - another public branch (remote/master)
    |
    | e - Commit E  (You are here)
    | |
    | d - Commit D
    | |
    | c - Commit C
    | |
    | b - Commit B
    | |
    | a - Commit A
    |/
    1 - some public base
    ```
*/
export const TEST_COMMIT_HISTORY = [
  COMMIT('z', 'Commit Z', 'y'),
  COMMIT('y', 'Commit Y', 'x'),
  COMMIT('x', 'Commit X', '2'),
  COMMIT('2', 'another public branch', '0', {phase: 'public', remoteBookmarks: ['remote/master']}),
  COMMIT('e', 'Commit E', 'd', {isDot: true}),
  COMMIT('d', 'Commit D', 'c'),
  COMMIT('c', 'Commit C', 'b'),
  COMMIT('b', 'Commit B', 'a'),
  COMMIT('a', 'Commit A', '1'),
  COMMIT('1', 'some public base', '0', {phase: 'public'}),
];

export const fireMouseEvent = function (
  type: string,
  elem: EventTarget,
  centerX: number,
  centerY: number,
  additionalProperties?: Partial<MouseEvent | InputEvent>,
) {
  const evt = document.createEvent('MouseEvents') as Writable<MouseEvent & InputEvent>;
  evt.initMouseEvent(
    type,
    true,
    true,
    window,
    1,
    1,
    1,
    centerX,
    centerY,
    false,
    false,
    false,
    false,
    0,
    elem,
  );
  evt.dataTransfer = {} as DataTransfer;
  if (additionalProperties != null) {
    for (const [key, value] of Object.entries(additionalProperties)) {
      (evt as Record<string, unknown>)[key] = value;
    }
  }
  return elem.dispatchEvent(evt);
};

export const drag = (elemDrag: HTMLElement, elemDrop: HTMLElement) => {
  act(() => {
    // calculate positions
    let pos = elemDrag.getBoundingClientRect();
    const center1X = Math.floor((pos.left + pos.right) / 2);
    const center1Y = Math.floor((pos.top + pos.bottom) / 2);

    pos = elemDrop.getBoundingClientRect();
    const center2X = Math.floor((pos.left + pos.right) / 2);
    const center2Y = Math.floor((pos.top + pos.bottom) / 2);

    // mouse over dragged element and mousedown
    fireMouseEvent('mousemove', elemDrag, center1X, center1Y);
    fireMouseEvent('mouseenter', elemDrag, center1X, center1Y);
    fireMouseEvent('mouseover', elemDrag, center1X, center1Y);
    fireMouseEvent('mousedown', elemDrag, center1X, center1Y);

    if (!elemDrag.draggable) {
      return;
    }

    // start dragging process over to drop target
    const dragStarted = fireMouseEvent('dragstart', elemDrag, center1X, center1Y);
    if (!dragStarted) {
      return;
    }

    fireMouseEvent('drag', elemDrag, center1X, center1Y);
    fireMouseEvent('mousemove', elemDrag, center1X, center1Y);
    fireMouseEvent('drag', elemDrag, center2X, center2Y);
    fireMouseEvent('mousemove', elemDrop, center2X, center2Y);

    // trigger dragging process on top of drop target
    fireMouseEvent('mouseenter', elemDrop, center2X, center2Y);
    fireMouseEvent('dragenter', elemDrop, center2X, center2Y);
    fireMouseEvent('mouseover', elemDrop, center2X, center2Y);
    fireMouseEvent('dragover', elemDrop, center2X, center2Y);
  });
};
export const drop = (elemDrag: HTMLElement, elemDrop: HTMLElement) => {
  act(() => {
    // calculate positions
    let pos = elemDrag.getBoundingClientRect();
    pos = elemDrop.getBoundingClientRect();
    const center2X = Math.floor((pos.left + pos.right) / 2);
    const center2Y = Math.floor((pos.top + pos.bottom) / 2);

    // release dragged element on top of drop target
    fireMouseEvent('drop', elemDrop, center2X, center2Y);
    fireMouseEvent('dragend', elemDrag, center2X, center2Y);
    fireMouseEvent('mouseup', elemDrag, center2X, center2Y);
  });
};

// See https://github.com/testing-library/user-event/issues/440
export const dragAndDrop = (elemDrag: HTMLElement, elemDrop: HTMLElement) => {
  drag(elemDrag, elemDrop);
  drop(elemDrag, elemDrop);
};

export function dragAndDropCommits(
  draggedCommit: Hash | HTMLElement,
  onto: Hash,
  op: typeof dragAndDrop | typeof drag | typeof drop = dragAndDrop,
) {
  const draggableCommit =
    typeof draggedCommit !== 'string'
      ? draggedCommit
      : within(screen.getByTestId(`commit-${draggedCommit}`)).queryByTestId('draggable-commit');
  expect(draggableCommit).toBeDefined();
  const dragTargetCommit = screen.queryByTestId(`commit-${onto}`)?.querySelector('.commit-details');
  expect(dragTargetCommit).toBeDefined();

  act(() => {
    op(draggableCommit as HTMLElement, dragTargetCommit as HTMLElement);
    jest.advanceTimersByTime(2);
  });
}

export function dragCommits(draggedCommit: Hash | HTMLElement, onto: Hash) {
  dragAndDropCommits(draggedCommit, onto, drag);
}

export function dropCommits(draggedCommit: Hash | HTMLElement, onto: Hash) {
  dragAndDropCommits(draggedCommit, onto, drop);
}

/** Check that YouAreHere points to the given commit. */
export function expectYouAreHerePointAt(hash: string) {
  // The previous row of "hash" should be "YouAreHere".
  //   YouAreHere
  //  /
  // hash
  const row = screen.getByTestId(`dag-row-group-${hash}`);
  const previousRow = row.previousElementSibling;
  expect(previousRow).toHaveTextContent('You are here');
}

/** Check that a "checked out elsewhere" badge row points to the given commit. */
export function expectCheckedOutElsewherePointAt(hash: string, worktreeName: string) {
  const row = screen.getByTestId(`dag-row-group-${hash}`);
  const previousRow = row.previousElementSibling;
  expect(previousRow).toHaveTextContent(worktreeName);
}

/**
 * Despite catching the error in our error boundary, react + jsdom still
 * print big scary messages to console.warn when we throw an error.
 * We can ignore these during the select tests that test error boundaries.
 * This should be done only when needed, to prevent filtering out useful
 * console.error statements.
 * See also: https://github.com/facebook/react/issues/11098#issuecomment-412682721
 */
export function suppressReactErrorBoundaryErrorMessages() {
  beforeAll(() => {
    jest.spyOn(console, 'error').mockImplementation(() => undefined);
  });
  afterAll(() => {
    jest.restoreAllMocks();
  });
}

/**
 * Print test name beforeEach. This can be useful to figure out which test prints
 * React or testing-library warnings, if the stack trace does not include the test
 * code.
 */
export function beforeEachPrintTestName() {
  beforeEach(() => {
    // eslint-disable-next-line no-console
    console.log(`Starting test: ${expect.getState().currentTestName}`);
  });
}

/**
 * Drop-in replacement of `waitFor` that includes a `nextTick` to workaround
 * some `act()` warnings.
 */
export function waitForWithTick<T>(callback: () => Promise<T> | T): Promise<T> {
  return waitFor(async () => {
    await nextTick();
    return callback();
  });
}

/**
 * Check that the "commit" is in the forked branch from "base".
 *
 *   o  <- does not return commit hashes here (not right-side)
 *   :
 *   |  ┌─────┐
 *   |  │  o  │
 *   |  │  :  │
 *   |  │  o  │ <- return commit hashes here (right-side branch)
 *   |  │  |  │
 *   |  │  o  │
 *   |  │ /   │
 *   |  └─────┘
 *   |  /
 *   base
 *
 * This is a naive implementation that does not consider merges.
 */
export function scanForkedBranchHashes(base: string): string[] {
  const baseRow = screen.getByTestId(`dag-row-group-${base}`);
  const getAttr = (e: Element, attr: string) => e.querySelector(`[${attr}]`)?.getAttribute(attr);
  const getNodeColumn = (row: Element) => parseInt(getAttr(row, 'data-nodecolumn') ?? '-1');
  const baseIndent = getNodeColumn(baseRow);
  // Scan rows above baseRow.
  let prevRow = baseRow.previousElementSibling;
  const branchHashes = [];
  while (prevRow) {
    const prevIndent = getNodeColumn(prevRow);
    if (prevIndent <= baseIndent) {
      // No longer right-side branch from 'base'.
      break;
    }
    const hash = getAttr(prevRow, 'data-commit-hash');
    if (hash) {
      branchHashes.push(hash);
    }
    prevRow = prevRow.previousElementSibling;
  }
  return branchHashes;
}
