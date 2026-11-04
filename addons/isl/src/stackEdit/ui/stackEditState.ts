/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import type {RecordOf} from 'immutable';
import type {ExportStack} from 'shared/types/stack';
import type {Hash} from '../../types';
import type {CommitRev, CommitState} from '../commitStackState';

import {List, Record} from 'immutable';
import {atom, useAtom} from 'jotai';
import {nullthrows} from 'shared/utils';
import clientToServerAPI from '../../ClientToServerAPI';
import {latestCommitMessageFieldsWithEdits} from '../../CommitInfoView/CommitInfoState';
import {
  commitMessageFieldsSchema,
  commitMessageFieldsToString,
} from '../../CommitInfoView/CommitMessageFields';
import {tracker} from '../../analytics';
import {WDIR_NODE} from '../../dag/virtualCommit';
import {t} from '../../i18n';
import {readAtom, writeAtom} from '../../jotaiUtils';
import {waitForNothingRunning} from '../../operationsState';
import {uncommittedSelection} from '../../partialSelection';
import {CommitStackState} from '../../stackEdit/commitStackState';
import {assert, registerDisposable} from '../../utils';
import {prev} from '../revMath';

/**
 * The "edit stack" dialog state that works with undo/redo in the dialog.
 * Extra states that do not need undo/redo support (ex. which tab is active)
 * are not here.
 */
type StackStateWithOperationProps = {
  op: StackEditOpDescription;
  state: CommitStackState;
  // Extra states for different kinds of operations.
  /** The split range selected in the "Split" tab. */
  splitRange: SplitRangeRecord;
};

type Intention = 'general' | 'split' | 'absorb';

/** Description of a stack edit operation. Used for display purpose. */
export type StackEditOpDescription =
  | {
      name: 'move';
      offset: number;
      /** Count of dependencies excluding self. */
      depCount?: number;
      commit: CommitState;
    }
  | {
      name: 'swap';
    }
  | {
      name: 'drop';
      commit: CommitState;
    }
  | {
      name: 'fold';
      commit: CommitState;
    }
  | {name: 'import'}
  | {name: 'insertBlankCommit'}
  | {name: 'removeEmptyCommit'}
  | {name: 'fileStack'; fileDesc: string}
  | {name: 'split'; path: string}
  | {name: 'splitWithAI'}
  | {name: 'metaedit'; commit: CommitState}
  | {name: 'absorbMove'; commit: CommitState};

type SplitRangeProps = {
  startKey: string;
  endKey: string;
};
export const SplitRangeRecord = Record<SplitRangeProps>({startKey: '', endKey: ''});
export type SplitRangeRecord = RecordOf<SplitRangeProps>;

// See `StackStateWithOperationProps`.
const StackStateWithOperation = Record<StackStateWithOperationProps>({
  op: {name: 'import'},
  state: new CommitStackState([]),
  splitRange: SplitRangeRecord(),
});
type StackStateWithOperation = RecordOf<StackStateWithOperationProps>;

/** History of multiple states for undo/redo support. */
type HistoryProps = {
  history: List<StackStateWithOperation>;
  currentIndex: number;
};

const HistoryRecord = Record<HistoryProps>({
  history: List(),
  currentIndex: 0,
});
type HistoryRecord = RecordOf<HistoryProps>;

class History extends HistoryRecord {
  get current(): StackStateWithOperation {
    return nullthrows(this.history.get(this.currentIndex));
  }

  push(
    state: CommitStackState,
    op: StackEditOpDescription,
    extras?: {
      splitRange?: SplitRangeRecord;
    },
  ): History {
    const newSplitRange = extras?.splitRange ?? this.current.splitRange;
    const newHistory = this.history.slice(0, this.currentIndex + 1).push(
      StackStateWithOperation({
        op,
        state,
        splitRange: newSplitRange,
      }),
    );
    return new History({
      history: newHistory,
      currentIndex: newHistory.size - 1,
    });
  }

  /**
   * Like `pop` then `push`, used to update the most recent operation as an optimization.
   */
  replaceTop(
    state: CommitStackState,
    op: StackEditOpDescription,
    extras?: {
      splitRange?: SplitRangeRecord;
    },
  ): History {
    const newSplitRange = extras?.splitRange ?? this.current.splitRange;
    const newHistory = this.history.slice(0, this.currentIndex).push(
      StackStateWithOperation({
        op,
        state,
        splitRange: newSplitRange,
      }),
    );
    return new History({
      history: newHistory,
      currentIndex: newHistory.size - 1,
    });
  }

  setSplitRange(range: SplitRangeRecord): History {
    const newHistory = this.history.set(this.currentIndex, this.current.set('splitRange', range));
    return new History({
      history: newHistory,
      currentIndex: newHistory.size - 1,
    });
  }

  canUndo(): boolean {
    return this.currentIndex > 0;
  }

  canRedo(): boolean {
    return this.currentIndex + 1 < this.history.size;
  }

  undoOperationDescription(): StackEditOpDescription | undefined {
    return this.canUndo() ? this.history.get(this.currentIndex)?.op : undefined;
  }

  redoOperationDescription(): StackEditOpDescription | undefined {
    return this.canRedo() ? this.history.get(this.currentIndex + 1)?.op : undefined;
  }

  undo(): History {
    return this.canUndo() ? this.set('currentIndex', this.currentIndex - 1) : this;
  }

  redo(): History {
    return this.canRedo() ? this.set('currentIndex', this.currentIndex + 1) : this;
  }
}

/** State related to stack editing UI. */
type StackEditState = {
  /**
   * Commit hashes being edited.
   * Empty means no editing is requested.
   *
   * Changing this to a non-empty value triggers `exportStack`
   * message to the server.
   */
  hashes: Set<Hash>;

  /** Intention of the stack editing. */
  intention: Intention;

  /**
   * The (mutable) main history of stack states.
   */
  history: Loading<History>;
};

/** Like Jotai's `loadable`, but not coupled with Promise. */
export type Loading<T> =
  | {
      state: 'loading';
      exportedStack:
        | ExportStack /* Got the exported stack. Analyzing. */
        | undefined /* Haven't got the exported stack. */;
      message?: string;
    }
  | {state: 'hasValue'; value: T}
  | {state: 'hasError'; error: string};

/**
 * Meant to be private. Exported for debugging purpose.
 *
 * You probably want to use `useStackEditState` and other atoms instead,
 * which ensures consistency (ex. stack and requested hashes cannot be
 * out of sync).
 */
export const stackEditState = (() => {
  const inner = atom<StackEditState>({
    hashes: new Set<Hash>(),
    intention: 'general',
    history: {state: 'loading', exportedStack: undefined},
  });
  return atom<StackEditState, [StackEditState | ((s: StackEditState) => StackEditState)], void>(
    get => get(inner),
    // Kick off stack analysis on receiving an exported stack.
    (get, set, newValue) => {
      const {hashes, intention, history} =
        typeof newValue === 'function' ? newValue(get(inner)) : newValue;
      if (hashes.size > 0 && history.state === 'loading' && history.exportedStack !== undefined) {
        try {
          let stack = new CommitStackState(history.exportedStack).buildFileStacks();
          if (intention === 'absorb') {
            // Perform absorb analysis. Note: the absorb use-case has an extra
            // "wdir()" at the stack top for absorb purpose. When the intention
            // is "general" or "split", there is no "wdir()" in the stack.
            stack = stack.analyseAbsorb();
          }
          const historyValue = new History({
            history: List([StackStateWithOperation({state: stack})]),
            currentIndex: 0,
          });
          currentMetrics = {
            commits: hashes.size,
            fileStacks: stack.fileStacks.size,
            fileStackRevs: stack.fileStacks.reduce((acc, f) => acc + f.source.revLength, 0),
            splitFromSuggestion: currentMetrics.splitFromSuggestion,
          };
          currentMetricsStartTime = Date.now();
          // Cannot write to self (`stackEditState`) here.
          set(inner, {
            hashes,
            intention,
            history: {state: 'hasValue', value: historyValue},
          });
        } catch (err) {
          const msg = `Cannot construct stack ${err}`;
          set(inner, {hashes, intention, history: {state: 'hasError', error: msg}});
        }
      } else {
        set(inner, newValue);
      }
    },
  );
})();

/**
 * Read-only access to the stack being edited.
 * This can be useful without going through `UseStackEditState`.
 * This is an atom so it can be used as a dependency of other atoms.
 */
export const stackEditStack = atom<CommitStackState | undefined>(get => {
  const state = get(stackEditState);
  return state.history.state === 'hasValue' ? state.history.value.current.state : undefined;
});

// Subscribe to server exportedStack events.
registerDisposable(
  stackEditState,
  clientToServerAPI.onMessageOfType('exportedStack', event => {
    writeAtom(stackEditState, (prev): StackEditState => {
      const {hashes, intention} = prev;
      const revs = joinRevs(hashes);
      if (revs !== event.revs) {
        // Wrong stack. Ignore it.
        return prev;
      }
      if (event.error != null) {
        return {hashes, intention, history: {state: 'hasError', error: event.error}};
      } else {
        return {
          hashes,
          intention,
          history: {
            state: 'loading',
            exportedStack: rewriteWdirContent(rewriteCommitMessagesInStack(event.stack)),
          },
        };
      }
    });
  }),
  import.meta.hot,
);

/**
 * Update commits messages in an exported stack to include:
 * 1. Any local edits the user has pending (these have already been confirmed by a modal at this point)
 * 2. Any remote message changes from the server (which allows the titles in the edit stack UI to be up to date)
 */
function rewriteCommitMessagesInStack(stack: ExportStack): ExportStack {
  const schema = readAtom(commitMessageFieldsSchema);
  return stack.map(c => {
    let text = c.text;
    if (schema) {
      const editedMessage = readAtom(latestCommitMessageFieldsWithEdits(c.node));
      if (editedMessage != null) {
        text = commitMessageFieldsToString(schema, editedMessage);
      }
    }
    return {...c, text};
  });
}

/**
 * Update the file content of "wdir()" to match the current partial selection.
 * `sl` does not know the current partial selection state tracked exclusively in ISL.
 * So let's patch the `wdir()` commit (if exists) with the right content.
 */
function rewriteWdirContent(stack: ExportStack): ExportStack {
  // Run `sl debugexportstack -r "wdir()" | python3 -m json.tool` to get a sense of the `ExportStack` format.
  return stack.map(c => {
    // 'f' * 40 means the wdir() commit.
    if (c.node === WDIR_NODE) {
      const selection = readAtom(uncommittedSelection);
      if (c.files != null) {
        for (const path in c.files) {
          const selected = selection.getSimplifiedSelection(path);
          if (selected === false) {
            // Not selected. Drop the path.
            delete c.files[path];
          } else if (typeof selected === 'string') {
            // Chunk-selected. Rewrite the content.
            c.files[path] = {
              ...c.files[path],
              data: selected,
            };
          }
        }
      }
    }
    return c;
  });
}

/**
 * Commit hashes being stack edited for general purpose.
 * Setting to a non-empty value (which can be using the revsetlang)
 * triggers server-side loading.
 *
 * For advance use-cases, the "hashes" could be revset expressions.
 */
export const editingStackIntentionHashes = atom<
  [Intention, Set<Hash | string>],
  [[Intention, Set<Hash | string>]],
  void
>(
  get => {
    const state = get(stackEditState);
    return [state.intention, state.hashes];
  },
  async (_get, set, newValue) => {
    const [intention, hashes] = newValue;
    const waiter = waitForNothingRunning();
    if (waiter != null) {
      set(stackEditState, {
        hashes,
        intention,
        history: {
          state: 'loading',
          exportedStack: undefined,
          message: t('Waiting for other commands to finish'),
        },
      });
      await waiter;
    }
    if (hashes.size > 0) {
      const revs = joinRevs(hashes);
      // Search for 'exportedStack' below for code handling the response.
      // For absorb's use-case, there could be untracked ('?') files that are selected.
      // Those would not be reported by `exportStack -r "wdir()""`. However, absorb
      // currently only works for edited files. So it's okay to ignore '?' selected
      // files by not passing `--assume-tracked FILE` to request content of these files.
      // In the future, we might want to make absorb support newly added files.
      clientToServerAPI.postMessage({type: 'exportStack', revs});
    }
    set(stackEditState, {
      hashes,
      intention,
      history: {state: 'loading', exportedStack: undefined},
    });
  },
);

/**
 * State for check whether the stack is loaded or not.
 * Use `useStackEditState` if you want to read or edit the stack.
 *
 * This is not `Loading<CommitStackState>` so `hasValue`
 * states do not trigger re-render.
 */
export const loadingStackState = atom<Loading<null>>(get => {
  const history = get(stackEditState).history;
  if (history.state === 'hasValue') {
    return hasValueState;
  } else {
    return history;
  }
});

const hasValueState: Loading<null> = {state: 'hasValue', value: null};

export const shouldAutoSplitState = atom<boolean>(false);

/** APIs exposed via useStackEditState() */
class UseStackEditState {
  state: StackEditState;
  setState: (_state: StackEditState) => void;

  // derived properties.
  private history: History;

  constructor(state: StackEditState, setState: (_state: StackEditState) => void) {
    this.state = state;
    this.setState = setState;
    assert(
      state.history.state === 'hasValue',
      'useStackEditState only works when the stack is loaded',
    );
    this.history = state.history.value;
  }

  get commitStack(): CommitStackState {
    return this.history.current.state;
  }

  get splitRange(): SplitRangeRecord {
    return this.history.current.splitRange;
  }

  get intention(): Intention {
    return this.state.intention;
  }

  setSplitRange(range: SplitRangeRecord | string) {
    const splitRange =
      typeof range === 'string'
        ? SplitRangeRecord({
            startKey: range,
            endKey: range,
          })
        : range;
    const newHistory = this.history.setSplitRange(splitRange);
    this.setHistory(newHistory);
  }

  push(commitStack: CommitStackState, op: StackEditOpDescription, splitRange?: SplitRangeRecord) {
    if (commitStack.originalStack !== this.commitStack.originalStack) {
      // Wrong stack. Discard.
      return;
    }
    const newHistory = this.history.push(commitStack, op, {splitRange});
    this.setHistory(newHistory);
  }

  /**
   * Like `pop` then `push`, used to update the most recent operation as an optimization
   * to avoid lots of tiny state changes in the history.
   */
  replaceTopOperation(
    commitStack: CommitStackState,
    op: StackEditOpDescription,
    extras?: {
      splitRange?: SplitRangeRecord;
    },
  ) {
    if (commitStack.originalStack !== this.commitStack.originalStack) {
      // Wrong stack. Discard.
      return;
    }
    const newHistory = this.history.replaceTop(commitStack, op, extras);
    this.setHistory(newHistory);
  }

  canUndo(): boolean {
    return this.history.canUndo();
  }

  canRedo(): boolean {
    return this.history.canRedo();
  }

  undo() {
    this.setHistory(this.history.undo());
  }

  undoOperationDescription(): StackEditOpDescription | undefined {
    return this.history.undoOperationDescription();
  }

  redoOperationDescription(): StackEditOpDescription | undefined {
    return this.history.redoOperationDescription();
  }

  redo() {
    this.setHistory(this.history.redo());
  }

  numHistoryEditsOfType(name: StackEditOpDescription['name']): number {
    return this.history.history
      .slice(0, this.history.currentIndex + 1)
      .filter(s => s.op.name === name).size;
  }

  /**
   * Count edits made after an AI split operation.
   * This helps measure the edit rate - how often users modify AI suggestions.
   * Returns the count of non-AI-split operations that occur after any splitWithAI operation.
   */
  countEditsAfterAiSplit(): number {
    const historySlice = this.history.history.slice(0, this.history.currentIndex + 1);
    let foundAiSplit = false;
    let editsAfterAiSplit = 0;

    for (const entry of historySlice) {
      if (entry.op.name === 'splitWithAI') {
        foundAiSplit = true;
      } else if (foundAiSplit && entry.op.name !== 'import') {
        // Count any non-import operations after an AI split
        // Exclude 'import' as it's the initial state operation
        editsAfterAiSplit++;
      }
    }

    return editsAfterAiSplit;
  }

  private setHistory(newHistory: History) {
    const {hashes, intention} = this.state;
    this.setState({
      hashes,
      intention,
      history: {state: 'hasValue', value: newHistory},
    });
  }
}

// Only export the type, not the constructor.
export type {UseStackEditState};

/**
 * Get the stack edit state. The stack must be loaded already, that is,
 * `loadingStackState.state` is `hasValue`. This is the main state for
 * reading and updating the `CommitStackState`.
 */
export function useStackEditState() {
  const [state, setState] = useAtom(stackEditState);
  return new UseStackEditState(state, setState);
}

/** Get revset expression for requested hashes. */
function joinRevs(hashes: Set<Hash>): string {
  return [...hashes].join('|');
}

type StackEditMetrics = {
  // Managed by this file.
  commits: number;
  fileStacks: number;
  fileStackRevs: number;
  acceptedAiSplits?: number;
  // Maintained by UI, via 'bumpStackEditMetric'.
  undo?: number;
  redo?: number;
  fold?: number;
  drop?: number;
  moveUpDown?: number;
  swapLeftRight?: number;
  moveDnD?: number;
  fileStackEdit?: number;
  splitMoveFile?: number;
  splitMoveLine?: number;
  splitInsertBlank?: number;
  splitRemoveEmpty?: number;
  splitChangeRange?: number;
  splitFromSuggestion?: number;
  clickedAiSplit?: number;
  // Devmate split specific metrics for acceptance rate tracking
  clickedDevmateSplit?: number;
  // Track edits made after an AI split was applied (to measure edit rate)
  editsAfterAiSplit?: number;
};

// Not atoms. They do not trigger re-render.
let currentMetrics: StackEditMetrics = {commits: 0, fileStackRevs: 0, fileStacks: 0};
let currentMetricsStartTime = 0;

export function bumpStackEditMetric(key: keyof StackEditMetrics, count = 1) {
  currentMetrics[key] = (currentMetrics[key] ?? 0) + count;
}

export function sendStackEditMetrics(stackEdit: UseStackEditState, save = true) {
  const duration = Date.now() - currentMetricsStartTime;
  const intention = readAtom(stackEditState).intention;

  // # accepted AI splits is how many AI split operations are remaining at the end
  const numAiSplits = stackEdit.numHistoryEditsOfType('splitWithAI');
  if (numAiSplits) {
    bumpStackEditMetric('acceptedAiSplits', numAiSplits);
  }

  // Count edits made after AI splits (to measure edit rate)
  // This counts any non-AI-split operations that occurred after a splitWithAI
  const editsAfterAiSplit = stackEdit.countEditsAfterAiSplit();
  if (editsAfterAiSplit > 0) {
    bumpStackEditMetric('editsAfterAiSplit', editsAfterAiSplit);
  }

  tracker.track('StackEditMetrics', {
    duration,
    extras: {...currentMetrics, save, intention},
  });
  currentMetrics.splitFromSuggestion = 0; // Reset for next time.
}

export {WDIR_NODE};

export function findStartEndRevs(
  stackEdit: UseStackEditState,
): [CommitRev | undefined, CommitRev | undefined] {
  const {splitRange, intention, commitStack} = stackEdit;
  if (intention === 'split') {
    return [1 as CommitRev, prev(commitStack.size as CommitRev)];
  }
  const startRev = commitStack.findCommitByKey(splitRange.startKey)?.rev;
  let endRev = commitStack.findCommitByKey(splitRange.endKey)?.rev;
  if (startRev == null || startRev > (endRev ?? -1)) {
    endRev = undefined;
  }
  return [startRev, endRev];
}
