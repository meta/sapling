/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import type {EnsureAssignedTogether} from 'shared/EnsureAssignedTogether';
import type {RepoPath} from 'shared/types/common';
import type {CommitMessageFields} from '../../CommitInfoView/types';
import type {CommitRev, CommitStackState, FileMetadata, FileStackIndex} from '../commitStackState';
import type {FileRev, FileStackState} from '../fileStackState';
import type {UseStackEditState} from './stackEditState';

import {Set as ImSet, type List, Range} from 'immutable';
import {Button} from 'isl-components/Button';
import {Icon} from 'isl-components/Icon';
import {Subtle} from 'isl-components/Subtle';
import {TextField} from 'isl-components/TextField';
import {Tooltip} from 'isl-components/Tooltip';
import {useAtomValue} from 'jotai';
import type {JSX} from 'react';
import {useEffect, useMemo, useRef, useState} from 'react';
import {useContextMenu} from 'shared/ContextMenu';
import {readableDiffBlocks as diffBlocks, type LineIdx, splitLines} from 'shared/diff';
import {useThrottledEffect} from 'shared/hooks';
import {firstLine, nullthrows} from 'shared/utils';
import {BranchIndicator} from '../../BranchIndicator';
import {commitMessageTemplate} from '../../CommitInfoView/CommitInfoState';
import {
  commitMessageFieldsSchema,
  commitMessageFieldsToString,
} from '../../CommitInfoView/CommitMessageFields';
import {FileHeader, IconType} from '../../ComparisonView/SplitDiffView/SplitDiffFileHeader';
import {useTokenizedContentsOnceVisible} from '../../ComparisonView/SplitDiffView/syntaxHighlighting';
import {Column, Row, ScrollX, ScrollY} from '../../ComponentUtils';
import {EmptyState} from '../../EmptyState';
import {useGeneratedFileStatuses} from '../../GeneratedFile';
import {tracker} from '../../analytics';
import {t, T} from '../../i18n';
import {readAtom} from '../../jotaiUtils';
import {GeneratedStatus} from '../../types';
import {isAbsent, reorderedRevs} from '../commitStackState';
import {max, next, prev} from '../revMath';
import {computeLinesForFileStackEditor} from './FileStackEditorLines';
import css from './SplitStackEditPanel.module.css';
import {
  bumpStackEditMetric,
  findStartEndRevs,
  SplitRangeRecord,
  useStackEditState,
} from './stackEditState';

import './SplitStackEditPanel.css';

export function SplitStackEditPanel() {
  const stackEdit = useStackEditState();

  const {commitStack} = stackEdit;

  const messageTemplate = useAtomValue(commitMessageTemplate);
  const schema = useAtomValue(commitMessageFieldsSchema);

  // Find the commits being split.
  const [startRev, endRev] = findStartEndRevs(stackEdit);

  // Nothing to split? Show a dropdown.
  if (startRev == null || endRev == null || startRev > endRev) {
    return (
      <div>
        <EmptyState small>
          <T>Select a commit to split its changes.</T>
          <Subtle>
            <T>Or, select a range of commits to move contents among them.</T>
          </Subtle>
        </EmptyState>
      </div>
    );
  }

  // Prepare a "dense" subStack with an extra empty commit to move right.
  const emptyTitle = getEmptyCommitTitle(commitStack.get(endRev)?.text ?? '');
  const fields: CommitMessageFields = {...messageTemplate, Title: emptyTitle};
  const message = commitMessageFieldsToString(schema, fields);
  const subStack = commitStack
    .insertEmpty(next(endRev), message, endRev)
    .denseSubStack(Range(startRev, endRev + 2).toList() as List<CommitRev>);

  const insertBlankCommit = (rev: CommitRev) => {
    const fields: CommitMessageFields = {...messageTemplate, Title: t('New Commit')};
    const message = commitMessageFieldsToString(schema, fields);

    const newStack = stackEdit.commitStack.insertEmpty((startRev + rev) as CommitRev, message);

    bumpStackEditMetric('splitInsertBlank');

    let {splitRange} = stackEdit;
    if (rev === 0) {
      const newStart = newStack.get(startRev);
      if (newStart != null) {
        splitRange = splitRange.set('startKey', newStart.key);
      }
    }

    stackEdit.push(newStack, {name: 'insertBlankCommit'}, splitRange);
  };

  const removeEmptyCommit = (rev: CommitRev) => {
    const mainRev = (startRev + rev) as CommitRev;
    const newStack = stackEdit.commitStack.drop(mainRev);

    bumpStackEditMetric('splitRemoveEmpty');

    let {splitRange} = stackEdit;
    const removedKey = stackEdit.commitStack.get(mainRev)?.key;
    if (removedKey === splitRange.startKey) {
      const newStart = newStack.get(mainRev);
      if (newStart != null) {
        splitRange = splitRange.set('startKey', newStart.key);
      }
    } else if (removedKey === splitRange.endKey) {
      const newEnd = newStack.get(prev(mainRev));
      if (newEnd != null) {
        splitRange = splitRange.set('endKey', newEnd.key);
      }
    }

    stackEdit.push(newStack, {name: 'removeEmptyCommit'}, splitRange);
  };

  // One commit per column.
  const columns: JSX.Element[] = subStack
    .revs()
    .map(rev => (
      <SplitColumn
        stackEdit={stackEdit}
        key={rev}
        rev={rev}
        subStack={subStack}
        insertBlankCommit={insertBlankCommit}
        removeEmptyCommit={removeEmptyCommit}
      />
    ));

  return (
    <div className="interactive-split">
      <ScrollX maxSize="calc((100vw / var(--zoom)) - 30px)">
        <Row style={{padding: '0 var(--pad)', alignItems: 'flex-start'}}>{columns}</Row>
      </ScrollX>
    </div>
  );
}

type SplitColumnProps = {
  stackEdit: UseStackEditState;
  subStack: CommitStackState;
  rev: CommitRev;
  insertBlankCommit: (rev: CommitRev) => unknown;
  removeEmptyCommit: (rev: CommitRev) => unknown;
};

function InsertBlankCommitButton({
  beforeRev,
  onClick,
}: {
  beforeRev: CommitRev | undefined;
  onClick: () => unknown;
}) {
  return (
    <div className="split-insert-blank-commit-container" role="button" onClick={onClick}>
      <Tooltip
        placement="top"
        title={
          beforeRev == 0
            ? t('Insert a new blank commit before the next commit')
            : t('Insert a new blank commit between these commits')
        }>
        <div className="split-insert-blank-commit">
          <Icon icon="add" />
        </div>
      </Tooltip>
    </div>
  );
}

function SwapCommitsButton({
  stackEdit,
  beforeRev,
}: {
  stackEdit: UseStackEditState;
  beforeRev: CommitRev | undefined;
}) {
  if (beforeRev == null || beforeRev === 0) {
    return null;
  }
  const state = stackEdit.commitStack;
  const beforeRevCommit = state.get(beforeRev);
  if (beforeRevCommit == null) {
    return null;
  }
  const newOrder = reorderedRevs(state, beforeRev);
  const canSwap = state.canReorder(newOrder);
  if (!canSwap) {
    return null;
  }
  return (
    <div
      className="split-insert-blank-commit-container"
      role="button"
      onClick={() => {
        stackEdit.push(state.reorder(newOrder), {
          name: 'swap',
        });
        bumpStackEditMetric('swapLeftRight');
      }}>
      <Tooltip placement="top" title={t('Swap the order of two commits.')}>
        <div className="split-insert-blank-commit">
          <Icon icon="arrow-swap" />
        </div>
      </Tooltip>
    </div>
  );
}

function SplitColumn(props: SplitColumnProps) {
  const {stackEdit, subStack, rev, insertBlankCommit, removeEmptyCommit} = props;

  const [collapsedFiles, setCollapsedFiles] = useState(new Set());

  const toggleCollapsed = (path: RepoPath) => {
    const updated = new Set(collapsedFiles);
    updated.has(path) ? updated.delete(path) : updated.add(path);
    setCollapsedFiles(updated);
  };

  const commit = subStack.get(rev);
  const commitMessage = commit?.text ?? '';

  // File stacks contain text (content-editable) files.
  // Note: subStack might contain files that are not editable
  // (ex. currently binary, but previously absent). Filter them out.
  const editablePaths = subStack.getPaths(rev, {text: true});
  const editablePathsSet = new Set(editablePaths);
  const generatedStatuses = useGeneratedFileStatuses(editablePaths);
  const sortedFileStacks = subStack.fileStacks
    .flatMap((fileStack, fileIdx): Array<[RepoPath, FileStackState, FileStackIndex]> => {
      const path = subStack.getFileStackPath(fileIdx, 0 as FileRev) ?? '';
      return editablePathsSet.has(path) ? [[path, fileStack, fileIdx]] : [];
    })
    .sort((a, b) => {
      const [pathA] = a;
      const [pathB] = b;

      const statusA = generatedStatuses[pathA] ?? GeneratedStatus.Manual;
      const statusB = generatedStatuses[pathB] ?? GeneratedStatus.Manual;

      return statusA === statusB ? pathA.localeCompare(pathB) : statusA - statusB;
    });

  // There might be non-text (ex. binary, or too large) files.
  const nonEditablePaths = subStack.getPaths(rev, {text: false}).sort();

  const editables = sortedFileStacks.flatMap(([path, fileStack, fileIdx]) => {
    // subStack is a "dense" stack. fileRev is commitRev + 1.
    const fileRev = (rev + 1) as FileRev;
    const isModified =
      (fileRev > 0 && fileStack.getRev(prev(fileRev)) !== fileStack.getRev(fileRev)) ||
      subStack.changedFileMetadata(rev, path) != null;
    const editor = (
      <SplitEditorWithTitle
        key={path}
        subStack={subStack}
        rev={rev}
        path={path}
        fileStack={fileStack}
        fileIdx={fileIdx}
        fileRev={fileRev}
        collapsed={collapsedFiles.has(path)}
        toggleCollapsed={() => toggleCollapsed(path)}
        generatedStatus={generatedStatuses[path]}
      />
    );
    const result = isModified ? [editor] : [];
    return result;
  });

  const nonEditables = nonEditablePaths.flatMap(path => {
    const file = subStack.getFile(rev, path);
    const prevFile = subStack.getFile(prev(rev), path);
    const isModified = !file.equals(prevFile);
    if (!isModified) {
      return [];
    }
    const editor = (
      <SplitEditorWithTitle
        key={path}
        subStack={subStack}
        rev={rev}
        path={path}
        collapsed={collapsedFiles.has(path)}
        toggleCollapsed={() => toggleCollapsed(path)}
      />
    );
    return [editor];
  });

  const editors = editables.concat(nonEditables);

  const canRemove = editors.isEmpty() && rev < subStack.size - 1 && subStack.size > 2;

  const body = editors.isEmpty() ? (
    <EmptyState small>
      <T>This commit is empty</T>
      <Subtle>
        <T>Use the left/right arrows to move files and lines of code and create new commits.</T>
      </Subtle>
      {canRemove ? (
        <Button onClick={() => removeEmptyCommit(rev)}>
          <Icon icon="trash" slot="start" />
          <T>Remove empty commit</T>
        </Button>
      ) : null}
    </EmptyState>
  ) : (
    <ScrollY maxSize="calc((100vh / var(--zoom)) - var(--split-vertical-overhead))" hideBar={true}>
      {editors}
    </ScrollY>
  );

  const showExtraCommitActionsContextMenu = useContextMenu(() => {
    const options = [];
    const allFiles = new Set(sortedFileStacks.map(([path]) => path));
    if (collapsedFiles.size < allFiles.size && allFiles.size > 0) {
      options.push({
        label: t('Collapse all files'),
        onClick() {
          setCollapsedFiles(allFiles);
        },
      });
    }
    if (collapsedFiles.size > 0) {
      options.push({
        label: t('Expand all files'),
        onClick() {
          setCollapsedFiles(new Set());
        },
      });
    }
    return options;
  });

  return (
    <>
      <Column>
        <InsertBlankCommitButton beforeRev={rev} onClick={() => insertBlankCommit(rev)} />
        <SwapCommitsButton stackEdit={stackEdit} beforeRev={rev} />
      </Column>
      <div className="split-commit-column">
        <div className="split-commit-header">
          <span className="split-commit-header-stack-number">
            {rev + 1} / {subStack.size}
          </span>
          <EditableCommitTitle commitMessage={commitMessage} commitKey={commit?.key} />
          <Button icon onClick={e => showExtraCommitActionsContextMenu(e)}>
            <Icon icon="ellipsis" />
          </Button>
        </div>
        {body}
      </div>
    </>
  );
}

type SplitEditorWithTitleProps = {
  subStack: CommitStackState;
  rev: CommitRev;
  path: RepoPath;
  fileStack?: FileStackState;
  fileIdx?: number;
  fileRev?: FileRev;
  collapsed: boolean;
  toggleCollapsed: () => unknown;
  generatedStatus?: GeneratedStatus;
};

function SplitEditorWithTitle(props: SplitEditorWithTitleProps) {
  const stackEdit = useStackEditState();

  const {commitStack} = stackEdit;
  const {
    subStack,
    path,
    fileStack,
    fileIdx,
    fileRev,
    collapsed,
    toggleCollapsed,
    rev,
    generatedStatus,
  } = props;
  const file = subStack.getFile(rev, path);
  const [showGeneratedFileAnyway, setShowGeneratedFileAnyway] = useState(false);

  const setSubStack = (newSubStack: CommitStackState) => {
    const [startRev, endRev] = findStartEndRevs(stackEdit);
    if (startRev != null && endRev != null) {
      const newCommitStack = commitStack.applySubStack(startRev, next(endRev), newSubStack);
      // Find the new split range.
      const endOffset = newCommitStack.size - commitStack.size;
      const startKey = newCommitStack.get(startRev)?.key ?? '';
      const endKey = newCommitStack.get(next(endRev, endOffset))?.key ?? '';
      const splitRange = SplitRangeRecord({startKey, endKey});
      // Update the main stack state.
      stackEdit.push(newCommitStack, {name: 'split', path}, splitRange);
    }
  };

  const setStack = (newFileStack: FileStackState) => {
    if (fileIdx == null || fileRev == null) {
      return;
    }
    const newSubStack = subStack.setFileStack(fileIdx, newFileStack);
    setSubStack(newSubStack);
  };

  const moveEntireFile = (dir: 'left' | 'right') => {
    // Suppose the file has 5 versions, and current version is 'v3':
    //             v1--v2--v3--v4--v5
    // Move left:
    //             v1--v3--v3--v4--v5 (replace v2 with v3)
    //             If v3 has 'copyFrom', drop 'copyFrom' on the second 'v3'.
    //             If v2 had 'copyFrom', preserve it on the first 'v3'.
    // Move right:
    //             v1--v2--v2--v4--v5 (replace v3 with v2)
    //             If v3 has 'copyFrom', update 'copyFrom' on 'v4'.
    //             v4 should not have 'copyFrom'.
    const [fromRev, toRev] = dir === 'left' ? [rev, prev(rev)] : [prev(rev), rev];
    const fromFile = subStack.getFile(fromRev, path);
    let newStack = subStack.setFile(toRev, path, oldFile => {
      if (dir === 'left' && oldFile.copyFrom != null) {
        return fromFile.set('copyFrom', oldFile.copyFrom);
      }
      return fromFile;
    });
    if (file.copyFrom != null) {
      if (dir === 'right') {
        newStack = newStack.setFile(next(rev), path, f => f.set('copyFrom', file.copyFrom));
      } else {
        newStack = newStack.setFile(rev, path, f => f.remove('copyFrom'));
      }
    }
    bumpStackEditMetric('splitMoveFile');
    setSubStack(newStack);
  };

  const changedMeta = subStack.changedFileMetadata(rev, path, false);
  let iconType = IconType.Modified;
  if (changedMeta != null) {
    const [oldMeta, newMeta] = changedMeta;
    if (isAbsent(oldMeta) && !isAbsent(newMeta)) {
      iconType = IconType.Added;
    } else if (!isAbsent(oldMeta) && isAbsent(newMeta)) {
      iconType = IconType.Removed;
    }
  }
  const canMoveLeft =
    rev > 0 && (file.copyFrom == null || isAbsent(subStack.getFile(prev(rev), path)));
  let copyFromText = undefined;
  if (file.copyFrom != null) {
    const copyFromFile = subStack.getFile(prev(rev), file.copyFrom);
    try {
      // This will throw if copyFromFile is non-text (binary, or too large).
      copyFromText = subStack.getUtf8Data(copyFromFile);
    } catch {}
  }

  return (
    <div className="split-commit-file">
      <FileHeader
        path={path}
        copyFrom={file.copyFrom}
        iconType={iconType}
        open={!collapsed}
        onChangeOpen={toggleCollapsed}
        fileActions={
          <div className="split-commit-file-arrows">
            {canMoveLeft ? (
              <Button icon onClick={() => moveEntireFile('left')}>
                ⬅
              </Button>
            ) : null}
            <Button icon onClick={() => moveEntireFile('right')}>
              ⮕
            </Button>
          </div>
        }
      />
      {!collapsed && (
        <>
          <ModeChangeHints changedMeta={changedMeta} />
          {fileRev != null && fileStack != null ? (
            !showGeneratedFileAnyway && generatedStatus !== GeneratedStatus.Manual ? (
              <Generated onShowAnyway={setShowGeneratedFileAnyway} />
            ) : (
              <SplitFile
                key={fileIdx}
                rev={fileRev}
                stack={fileStack}
                setStack={setStack}
                path={path}
                copyFromText={copyFromText}
              />
            )
          ) : (
            <NonEditable />
          )}
        </>
      )}
    </div>
  );
}

const FLAG_TO_MESSAGE = new Map<string, string>([
  ['', t('regular')],
  ['l', t('symlink')],
  ['x', t('executable')],
  ['m', t('Git submodule')],
]);

function ModeChangeHints(props: {changedMeta?: [FileMetadata, FileMetadata]}) {
  const {changedMeta} = props;
  if (changedMeta == null) {
    return null;
  }

  const [oldMeta, newMeta] = changedMeta;
  const oldFlag = oldMeta.flags ?? '';
  const newFlag = newMeta.flags ?? '';
  let message = null;

  if (!isAbsent(newMeta)) {
    const newDesc = FLAG_TO_MESSAGE.get(newFlag);
    // Show hint for newly added non-regular files.
    if (newFlag !== '' && isAbsent(oldMeta)) {
      if (newDesc != null) {
        message = t('File type: $new', {replace: {$new: newDesc}});
      }
    } else {
      // Show hint when the flag (mode) has changed.
      if (newFlag !== oldFlag) {
        const oldDesc = FLAG_TO_MESSAGE.get(oldFlag);
        if (oldDesc != null && newDesc != null && oldDesc !== newDesc) {
          message = t('File type change: $old → $new', {replace: {$old: oldDesc, $new: newDesc}});
        }
      }
    }
  }

  return message == null ? null : <div className="split-header-hint">{message}</div>;
}

function NonEditable() {
  return (
    <div className="split-header-hint">
      <T>Binary or large file content is not editable.</T>
    </div>
  );
}

function Generated({onShowAnyway}: {onShowAnyway: (show: boolean) => void}) {
  return (
    <div className="split-header-hint">
      <Column>
        <T>This file is generated</T>
        <Button icon onClick={() => onShowAnyway(true)}>
          <T>Show anyway</T>
        </Button>
      </Column>
    </div>
  );
}

/** Open dialog to select a commit range to split. */
function StackRangeSelectorButton() {
  const stackEdit = useStackEditState();

  const [startRev, endRev] = findStartEndRevs(stackEdit);
  const {commitStack} = stackEdit;
  const startCommit = startRev == null ? null : commitStack.get(startRev);

  const label =
    startRev == null ? null : endRev == null || startRev === endRev ? (
      <T replace={{$commit: firstLine(startCommit?.text ?? '')}}>Splitting $commit</T>
    ) : (
      <T replace={{$numCommits: endRev - startRev + 1}}>Splitting $numCommits commits</T>
    );
  return (
    <div className="split-range-selector-button">
      <Tooltip trigger="click" component={() => <StackRangeSelector />}>
        <Button>
          <Icon icon="layers" slot="start" />
          <T>Change split range</T>
        </Button>
      </Tooltip>
      {label}
    </div>
  );
}

type DragSelection = {
  start: number;
  startKey: string;
  isDragging: boolean;
} & EnsureAssignedTogether<{
  end: number;
  endKey: string;
}>;

/** Split range should be ordered with start at the bottom of the stack, and end at the top. */
function orderRevsInDrag(drag: DragSelection): DragSelection {
  if (drag.end == null) {
    return drag;
  }
  if (drag.start > drag.end) {
    return {
      ...drag,
      start: drag.end,
      startKey: drag.endKey,
      end: drag.start,
      endKey: drag.startKey,
    };
  }
  return drag;
}

function StackRangeSelector() {
  const stackEdit = useStackEditState();

  useThrottledEffect(
    () => {
      tracker.track('SplitOpenRangeSelector');
    },
    100,
    [],
  );

  const {commitStack} = stackEdit;
  let {splitRange} = stackEdit;
  const [startRev, endRev] = findStartEndRevs(stackEdit);
  const endKey = (endRev != null && commitStack.get(endRev)?.key) || '';
  splitRange = splitRange.set('endKey', endKey);
  const mutableRevs = commitStack.mutableRevs().reverse();

  const startCommitKey = startRev == null ? '' : (commitStack.get(startRev)?.key ?? '');
  const [dragSelection, setDragSelection] = useState<DragSelection>({
    start: startRev ?? 0,
    startKey: startCommitKey,
    isDragging: false,
  });

  const orderedDrag = orderRevsInDrag(dragSelection);
  const selectStart = orderedDrag.start;
  const selectEnd = orderedDrag.end ?? selectStart;

  const commits = mutableRevs.map(rev => {
    const commit = nullthrows(commitStack.get(rev));
    return (
      <div
        onPointerDown={() => {
          setDragSelection({start: rev, startKey: commit.key, isDragging: true});
        }}
        onPointerEnter={() => {
          if (dragSelection?.isDragging === true) {
            setDragSelection(old => ({...nullthrows(old), end: rev, endKey: commit.key}));
          }
        }}
        key={rev}
        className={
          'split-range-commit' +
          (commit.rev === selectStart ? ' selection-start' : '') +
          (commit.rev === selectEnd ? ' selection-end' : '') +
          (selectStart != null &&
          selectEnd != null &&
          commit.rev > selectStart &&
          commit.rev < selectEnd
            ? ' selection-middle'
            : '')
        }>
        <div className="commit-selection-avatar" />
        <div className="commit-avatar" />
        <div className="commit-title">{firstLine(commit.text)}</div>
      </div>
    );
  });

  return (
    <div className="split-range-selector">
      <div className="split-range-selector-info">
        <Icon icon="info" />
        <div>
          <b>
            <T>Click to select a commit to split.</T>
          </b>
          <br />
          <T>Click and drag to select a range of commits.</T>
        </div>
      </div>
      <div
        className="commit-tree-root commit-group with-vertical-line"
        onPointerUp={() => {
          // update drag preview
          setDragSelection(old => ({...old, isDragging: false}));

          const {startKey, endKey} = orderRevsInDrag(dragSelection);

          // actually change range
          let newRange = splitRange;
          newRange = newRange.set('startKey', startKey);
          newRange = newRange.set('endKey', endKey ?? startKey);
          stackEdit.setSplitRange(newRange);

          bumpStackEditMetric('splitChangeRange');
        }}>
        <div className="commit-group inner-commit-group">{commits}</div>
        <BranchIndicator />
      </div>
    </div>
  );
}

type MaybeEditableCommitTitleProps = {
  commitMessage: string;
  commitKey?: string;
};

function EditableCommitTitle(props: MaybeEditableCommitTitleProps) {
  const stackEdit = useStackEditState();

  const {commitMessage, commitKey} = props;

  const existingTitle = firstLine(commitMessage);
  const existingDescription = commitMessage.slice(existingTitle.length + 1);

  // Only allow changing the commit title, not the rest of the commit message.
  const handleEdit = (newTitle?: string) => {
    if (newTitle != null && commitKey != null) {
      const {commitStack} = stackEdit;
      const commit = commitStack.findCommitByKey(commitKey);
      if (commit != null) {
        const newFullText = newTitle + '\n' + existingDescription;
        const newStack = commitStack.stack.setIn([commit.rev, 'text'], newFullText);
        const newCommitStack = commitStack.set('stack', newStack);

        const previous = stackEdit.undoOperationDescription();
        if (previous != null && previous.name == 'metaedit' && previous.commit.rev === commit.rev) {
          // the last operation was also editing this same message, let's reuse the history instead of growing it
          stackEdit.replaceTopOperation(newCommitStack, {name: 'metaedit', commit});
        } else {
          stackEdit.push(newCommitStack, {name: 'metaedit', commit});
        }
      } else {
        // If we don't have a real commit for this editor, it's the "fake" blank commit added to the top of the dense stack.
        // We need a real commit to associate the newly edited title to, so it can be persisted/is part of the undo stack.
        // So we make the fake blank commit into a real blank commit by inserting at the end.
        // Note that this will create another fake blank commit AFTER the new real blank commit.

        const [, endRev] = findStartEndRevs(stackEdit);

        const messageTemplate = readAtom(commitMessageTemplate);
        const schema = readAtom(commitMessageFieldsSchema);
        const fields: CommitMessageFields = {...messageTemplate, Title: newTitle};
        const message = commitMessageFieldsToString(schema, fields);
        if (endRev != null) {
          const newStack = commitStack.insertEmpty(next(endRev), message);

          const newEnd = newStack.get(next(endRev));
          if (newEnd != null) {
            let {splitRange} = stackEdit;
            splitRange = splitRange.set('endKey', newEnd.key);
            stackEdit.push(newStack, {name: 'insertBlankCommit'}, splitRange);
          }
        }
      }
    }
  };
  return (
    <TextField
      containerClassName={css.full}
      value={existingTitle}
      title={t('Edit commit title')}
      style={{width: 'calc(100% - var(--pad))'}}
      onInput={e => handleEdit(e.currentTarget?.value)}
    />
  );
}

const splitMessagePrefix = t('Split of "');

function getEmptyCommitTitle(commitMessage: string): string {
  let title = '';
  if (!commitMessage.startsWith(splitMessagePrefix)) {
    // foo bar -> Split of "foo bar"
    title = commitMessage.split('\n', 1)[0];
    title = t('Split of "$title"', {replace: {$title: title}});
  } else {
    title = commitMessage.split('\n', 1)[0];
    const sep = t(' #');
    const last = title.split(sep).at(-1) ?? '';
    const number = parseInt(last);
    if (number > 0) {
      // Split of "foo" #2 -> Split of "foo" #3
      title = title.slice(0, -last.length) + (number + 1).toString();
    } else {
      // Split of "foo" -> Split of "foo" #2
      title = title + sep + '2';
    }
  }
  return title;
}

type SplitFileProps = {
  /**
   * File stack to edit.
   *
   * Note: the editor for rev 1 might want to diff against rev 0 and rev 2,
   * and might have buttons to move lines to other revs. So it needs to
   * know the entire stack.
   */
  stack: FileStackState;

  /**
   * Override the "left side" text (diff against).
   *
   * This is useful to provide the text from the "copyFrom" file.
   * Once set, move left buttons will be disabled.
   */
  copyFromText?: string;

  /** Function to update the stack. */
  setStack: (stack: FileStackState) => void;

  /** Function to get the "title" of a rev. */
  getTitle?: (rev: FileRev) => string;

  /**
   * Skip editing (or showing) given revs.
   * This is usually to skip rev 0 (public, empty) if it is absent.
   * In the side-by-side mode, rev 0 is shown it it is an existing empty file
   * (introduced by a previous public commit). rev 0 is not shown if it is
   * absent, aka. rev 1 added the file.
   */
  skip?: (rev: FileRev) => boolean;

  /** The rev in the stack to edit. */
  rev: FileRev;

  /** The filepath */
  path: string;
};

export function SplitFile(props: SplitFileProps) {
  const mainContentRef = useRef<HTMLTableElement | null>(null);
  const [expandedLines, setExpandedLines] = useState<ImSet<LineIdx>>(ImSet);
  const [selectedLineIds, setSelectedLineIds] = useState<ImSet<string>>(ImSet);
  const {stack, rev, setStack, copyFromText} = props;

  // Selection change is a document event, not a <pre> event.
  useEffect(() => {
    const handleSelect = () => {
      const selection = window.getSelection();
      if (
        selection == null ||
        mainContentRef.current == null ||
        !mainContentRef.current.contains(selection.anchorNode)
      ) {
        setSelectedLineIds(ids => (ids.isEmpty() ? ids : ImSet()));
        return;
      }
      const divs = mainContentRef.current.querySelectorAll<HTMLDivElement>('div[data-sel-id]');
      const selIds: Array<string> = [];
      for (const div of divs) {
        if (
          (div.lastChild && selection.containsNode(div.lastChild, true)) ||
          (div.firstChild && selection.containsNode(div.firstChild, true))
        ) {
          selIds.push(nullthrows(div.dataset.selId));
        }
      }

      setSelectedLineIds(ImSet(selIds));
    };
    document.addEventListener('selectionchange', handleSelect);
    return () => {
      document.removeEventListener('selectionchange', handleSelect);
    };
  }, []);

  // Diff with the left side.
  const bText = stack.getRev(rev);
  const aText = copyFromText ?? stack.getRev(max(prev(rev), 0));
  // memo to avoid syntax highlighting repeatedly even when the text hasn't changed
  const bLines = useMemo(() => splitLines(bText), [bText]);
  const aLines = useMemo(() => splitLines(aText), [aText]);
  const abBlocks = diffBlocks(aLines, bLines);

  const highlights = useTokenizedContentsOnceVisible(props.path, aLines, bLines, mainContentRef);
  const hasCopyFrom = copyFromText != null;

  const {leftGutter, leftButtons, mainContent, rightGutter, rightButtons, lineKind} =
    computeLinesForFileStackEditor(
      stack,
      setStack,
      rev,
      'unified-diff',
      aLines,
      bLines,
      highlights?.[0],
      highlights?.[1],
      abBlocks,
      [],
      abBlocks,
      expandedLines,
      setExpandedLines,
      selectedLineIds,
      [],
      false,
      false,
      hasCopyFrom,
    );

  const rows = mainContent.map((line, i) => (
    <tr key={i} className={lineKind[i]}>
      <td className="split-left-button">{leftButtons[i]}</td>
      <td className="split-left-lineno">{leftGutter[i]}</td>
      <td className="split-line-content">{line}</td>
      <td className="split-right-lineno">{rightGutter[i]}</td>
      <td className="split-right-button">{rightButtons[i]}</td>
    </tr>
  ));

  return (
    <div className="split-file">
      <table ref={mainContentRef}>
        <colgroup>
          <col width={50} />
          {/* left arrows */}
          <col width={50} />
          {/* before line numbers */}
          <col width={'100%'} />
          {/* diff content */}
          <col width={50} />
          {/* after line numbers */}
          <col width={50} />
          {/* rightarrow  */}
        </colgroup>
        <tbody>{rows}</tbody>
      </table>
    </div>
  );
}

export function SplitStackToolbar() {
  return <StackRangeSelectorButton />;
}
