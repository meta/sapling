/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import type {Map as ImMap} from 'immutable';
import type {MutableRefObject, ReactNode} from 'react';
import type {Comparison} from 'shared/Comparison';
import type {ContextMenuItem} from 'shared/ContextMenu';
import type {ParsedDiff} from 'shared/patch/types';
import type {Context} from '../../ComparisonView/SplitDiffView/types';
import type {DragHandler} from '../../DragHandle';
import type {RenderGlyphResult} from '../../RenderDag';
import type {Dag} from '../../dag/dag';
import type {DagCommitInfo} from '../../dag/dagCommitInfo';
import type {HashSet} from '../../dag/set';
import type {AbsorbEdit, AbsorbEditId} from '../absorb';
import type {
  CommitRev,
  CommitStackState,
  CommitState,
  FileRev,
  FileStackIndex,
} from '../commitStackState';
import type {UseStackEditState} from './stackEditState';

import {Banner, BannerKind} from 'isl-components/Banner';
import {Button} from 'isl-components/Button';
import {Column, Row} from 'isl-components/Flex';
import {Icon} from 'isl-components/Icon';
import {Tooltip} from 'isl-components/Tooltip';
import {atom, useAtomValue} from 'jotai';
import React, {useContext, useEffect, useMemo, useRef} from 'react';
import {ComparisonType} from 'shared/Comparison';
import {useContextMenu} from 'shared/ContextMenu';
import {cn} from 'shared/cn';
import {firstLine, nullthrows} from 'shared/utils';
import {FileHeader, IconType} from '../../ComparisonView/SplitDiffView/SplitDiffFileHeader';
import {SplitDiffTable} from '../../ComparisonView/SplitDiffView/SplitDiffHunk';
import {ScrollY} from '../../ComponentUtils';
import {DragHandle} from '../../DragHandle';
import {DraggingOverlay} from '../../DraggingOverlay';
import {defaultRenderGlyph, RenderDag} from '../../RenderDag';
import {YOU_ARE_HERE_VIRTUAL_COMMIT} from '../../dag/virtualCommit';
import {t, T} from '../../i18n';
import {readAtom, writeAtom} from '../../jotaiUtils';
import {prev} from '../revMath';
import {calculateDagFromStack} from '../stackDag';
import css from './AbsorbStackEditPanel.module.css';
import {type DragDestinationRow, findDragDestinationCommitRev} from './dragDestination';
import {stackEditStack, useStackEditState} from './stackEditState';

type DragDestinationSnapshot = {
  container: HTMLElement;
  rows: ReadonlyArray<DragDestinationRow>;
};

/** The `AbsorbEdit` that is currently being dragged. */
const draggingAbsorbEdit = atom<AbsorbEdit | null>(null);
const draggingHint = atom<string | null>(null);
const AbsorbDragContext = React.createContext<MutableRefObject<DragHandler | null> | null>(null);

function useAbsorbDragRef(): MutableRefObject<DragHandler | null> {
  return nullthrows(useContext(AbsorbDragContext));
}

export function AbsorbStackEditPanel() {
  useResetCollapsedFilesOnMount();
  const isDragging = useAtomValue(draggingAbsorbEdit) != null;
  const stackEdit = useStackEditState();
  const stack = stackEdit.commitStack;
  const dag = calculateDagFromStack(stack);
  const subset = relevantSubset(stack, dag);
  const onDragRef = useRef<DragHandler | null>(null);

  return (
    <AbsorbDragContext.Provider value={onDragRef}>
      <Column className={css.container}>
        <AbsorbInstruction dag={dag} subset={subset} />
        <ScrollY maxSize="calc(100vh - 200px)" className={css.scrollYPadding}>
          <RenderDag
            className="absorb-dag"
            disableReorderAnimation={isDragging}
            dag={dag}
            renderCommit={renderCommit}
            renderCommitExtras={renderCommitExtras}
            renderGlyph={RenderGlyph}
            subset={subset}
          />
        </ScrollY>
      </Column>
      <AbsorbDraggingOverlay />
    </AbsorbDragContext.Provider>
  );
}

function AbsorbInstruction(props: {subset: HashSet; dag: Dag}) {
  const {dag, subset} = props;
  const hasOmittedCommits = subset.size < dag.all().size;
  const hasDndDestinations = subset.intersect(dag.draft()).size > 1;
  let bannerKind = BannerKind.default;
  const tips: ReactNode[] = [];
  if (hasDndDestinations) {
    tips.push(
      <T>Changes have been automatically distributed through your stack.</T>,
      <T replace={{$grabber: <Icon icon="grabber" size="S" className={css.inlineIcon} />}}>
        Drag $grabber to move changes to different commits
      </T>,
    );
    if (hasOmittedCommits) {
      tips.push(<T>Only commits that modify related files/areas are shown.</T>);
    }
  } else {
    bannerKind = BannerKind.warning;
    tips.push(<T>Nothing to absorb. The commit stack did not modify relevant files.</T>);
  }

  return (
    <Row className={css.instruction}>
      <Banner className={css.instruction} kind={bannerKind}>
        <Icon icon="info" />
        <div>
          {tips.map((tip, idx) => (
            <React.Fragment key={idx}>
              {idx > 0 && <br />}
              {tip}
            </React.Fragment>
          ))}
        </div>
      </Banner>
    </Row>
  );
}

const candidateDropTargetRevs = atom<readonly CommitRev[] | undefined>(get => {
  const edit = get(draggingAbsorbEdit);
  const stack = get(stackEditStack);
  if (edit == null || stack == null) {
    return undefined;
  }
  return stack.getAbsorbCommitRevs(nullthrows(edit.fileStackIndex), edit.absorbEditId)
    .candidateRevs;
});

function RenderGlyph(info: DagCommitInfo): RenderGlyphResult {
  const revs = useAtomValue(candidateDropTargetRevs);
  const rev = info.stackRev;
  const [kind, inner] = defaultRenderGlyph(info);
  let newInner = inner;
  if (kind === 'inside-tile' && rev != null && revs?.includes(rev)) {
    // This is a candidate drop target. Wrap in a SVG circle.
    const circle = (
      <circle cx={0} cy={0} r={8} fill="transparent" stroke="var(--focus-border)" strokeWidth={4} />
    );
    newInner = (
      <>
        {circle}
        {inner}
      </>
    );
  }
  return [kind, newInner];
}

function AbsorbDraggingOverlay() {
  const onDragRef = useAbsorbDragRef();
  const absorbEdit = useAtomValue(draggingAbsorbEdit);
  const hint = useAtomValue(draggingHint);
  const stack = useAtomValue(stackEditStack);

  const fileStackIndex = absorbEdit?.fileStackIndex;
  // Path extension is used by the syntax highlighter
  let path = '';
  if (stack && fileStackIndex) {
    const fileRev = absorbEdit.selectedRev ?? (0 as FileRev);
    path = nullthrows(stack.getFileStackPath(fileStackIndex, fileRev));
  }

  return (
    <DraggingOverlay onDragRef={onDragRef} hint={hint}>
      {absorbEdit && <SingleAbsorbEdit path={path} edit={absorbEdit} inDraggingOverlay={true} />}
    </DraggingOverlay>
  );
}

/**
 * Subset of `dag` to render in an absorb UI. It skips draft commits that are
 * not absorb destinations. For example, A01..A50, a 50-commit stack, absorbing
 * `x.txt` change. There are only 3 commits that touch `x.txt`, so the absorb
 * destination only includes those 3 commits.
 */
function relevantSubset(stack: CommitStackState, dag: Dag) {
  const revs = stack.getAllAbsorbCandidateCommitRevs();
  const keys = [...revs].map(rev => nullthrows(stack.get(rev)?.key));
  // Also include the (base) public commit and the `wdir()` virtual commit.
  keys.push(YOU_ARE_HERE_VIRTUAL_COMMIT.hash);
  return dag.present(keys).union(dag.public_());
}

// NOTE: To avoid re-render, the "renderCommit" and "renderCommitExtras" functions
// need to be "static" instead of anonymous functions.
// Note this is a regular function. To use React hooks, return a React component.
function renderCommit(info: DagCommitInfo) {
  return <RenderCommit info={info} />;
}

function RenderCommit(props: {info: DagCommitInfo}) {
  const {info} = props;
  const revs = useAtomValue(candidateDropTargetRevs);
  const rev = info.stackRev;
  const fadeout = revs != null && rev != null && revs.includes(rev) === false;

  if (info.phase === 'public') {
    return <div />;
  }
  // Just show the commit title for now.
  return (
    <div className={cn(css.commitTitle, fadeout && css.deemphasizeCommitTitle)}>{info.title}</div>
  );
}

function renderCommitExtras(info: DagCommitInfo) {
  return <AbsorbDagCommitExtras info={info} />;
}

function snapshotDragDestinationRows(stack: CommitStackState): DragDestinationSnapshot | undefined {
  const container = document.querySelector<HTMLElement>('.absorb-dag');
  if (container == null) {
    return undefined;
  }
  // Reorder FLIP animations translate these rows; cancel them so measurements
  // reflect settled positions rather than mid-animation offsets.
  container.getAnimations?.({subtree: true})?.forEach(animation => animation.cancel());
  const containerTop = container.getBoundingClientRect().top;
  const keyToRev = new Map<string, CommitRev>();
  for (const rev of stack.revs()) {
    const commit = stack.get(rev);
    if (commit != null) {
      keyToRev.set(commit.key, rev);
    }
  }
  const rows: DragDestinationRow[] = [];
  container.querySelectorAll<HTMLElement>('.render-dag-row-group').forEach(element => {
    const key = element.getAttribute('data-reorder-id');
    const rev = key == null ? undefined : keyToRev.get(key);
    if (rev != null) {
      const rect = element.getBoundingClientRect();
      const top = rect.top - containerTop;
      rows.push({midpoint: top + rect.height / 2, rev, top});
    }
  });
  return {container, rows: rows.sort((a, b) => a.top - b.top)};
}

class AbsorbDragController {
  private geometryRefreshFrame: number | null = null;
  private lastTargetRev: CommitRev | null | undefined;
  private operationStackEdit: UseStackEditState | null = null;
  private snapshot: DragDestinationSnapshot | undefined;
  private started = false;

  hint: string | null = null;

  prepare(stack: CommitStackState, stackEdit: UseStackEditState) {
    if (!this.started) {
      this.started = true;
      this.operationStackEdit = stackEdit;
      this.snapshot = snapshotDragDestinationRows(stack);
    } else if (this.geometryRefreshFrame != null) {
      document.defaultView?.cancelAnimationFrame(this.geometryRefreshFrame);
      this.geometryRefreshFrame = null;
      this.snapshot = snapshotDragDestinationRows(stack);
    }
    return this.snapshot;
  }

  isNewTarget(rev: CommitRev | null) {
    if (rev === this.lastTargetRev) {
      return false;
    }
    this.lastTargetRev = rev;
    return true;
  }

  applyDestination(stack: CommitStackState, commit: CommitState) {
    nullthrows(this.operationStackEdit).push(stack, {name: 'absorbMove', commit});
    this.scheduleGeometryRefresh();
  }

  finish() {
    const view = document.defaultView;
    if (view != null && this.geometryRefreshFrame != null) {
      view.cancelAnimationFrame(this.geometryRefreshFrame);
    }
    this.geometryRefreshFrame = null;
    this.hint = null;
    this.lastTargetRev = undefined;
    this.operationStackEdit = null;
    this.snapshot = undefined;
    this.started = false;
  }

  private scheduleGeometryRefresh() {
    const view = document.defaultView;
    if (view == null) {
      this.geometryRefreshFrame = null;
      const stack = readAtom(stackEditStack);
      this.snapshot = stack == null ? undefined : snapshotDragDestinationRows(stack);
      return;
    }
    if (this.geometryRefreshFrame != null) {
      view.cancelAnimationFrame(this.geometryRefreshFrame);
    }
    this.geometryRefreshFrame = view.requestAnimationFrame(() => {
      this.geometryRefreshFrame = null;
      const stack = readAtom(stackEditStack);
      this.snapshot = stack == null ? undefined : snapshotDragDestinationRows(stack);
    });
  }
}

/** Show file paths and diff chunks. */
function AbsorbDagCommitExtras(props: {info: DagCommitInfo}) {
  const {info} = props;
  const stackEdit = useStackEditState();
  const stack = stackEdit.commitStack;
  const rev = info.stackRev;
  if (rev == null) {
    return null;
  }

  const fileIdxToEdits = stack.absorbExtraByCommitRev(rev);
  if (fileIdxToEdits.isEmpty()) {
    return null;
  }

  const isWdir = info.hash === YOU_ARE_HERE_VIRTUAL_COMMIT.hash;

  return (
    <div className={css.commitExtras}>
      {isWdir && (
        <div className={css.uncommittedChanges}>
          <T>Uncommitted Changes</T>
        </div>
      )}
      {fileIdxToEdits
        .map((edits, fileIdx) => (
          <AbsorbEditsForFile
            isWdir={isWdir}
            fileStackIndex={fileIdx}
            absorbEdits={edits}
            key={fileIdx}
          />
        ))
        .valueSeq()}
    </div>
  );
}

const absorbCollapsedFiles = atom<Map<string, boolean>>(new Map());

function useCollapsedFile(
  path: string | undefined,
  fileHasAnyDestinations: boolean,
): [boolean, (value: boolean) => void] | [undefined, undefined] {
  const collapsedFiles = useAtomValue(absorbCollapsedFiles);
  if (path == null) {
    return [undefined, undefined];
  }
  const isCollapsed = collapsedFiles.get(path) ?? (fileHasAnyDestinations ? false : true);
  const setCollapsed = (collapsed: boolean) => {
    const newMap = new Map(collapsedFiles);
    newMap.set(path, collapsed);
    writeAtom(absorbCollapsedFiles, newMap);
  };
  return [isCollapsed, setCollapsed];
}
function useResetCollapsedFilesOnMount() {
  useEffect(() => {
    writeAtom(absorbCollapsedFiles, new Map());
  }, []);
}

function AbsorbEditsForFile(props: {
  isWdir: boolean;
  fileStackIndex: FileStackIndex;
  absorbEdits: ImMap<AbsorbEditId, AbsorbEdit>;
}) {
  const {fileStackIndex, absorbEdits} = props;
  const stack = nullthrows(useAtomValue(stackEditStack));
  const fileStack = nullthrows(stack.fileStacks.get(fileStackIndex));
  // In case the file is renamed, show "path1 -> path2" where path1 is the file
  // name in the commit, and path2 is the file name in the working copy.
  // Note: the line numbers we show are based on the working copy, not the commit.
  // So it seems showing the file name in the working copy is relevant.
  const fileRev = absorbEdits.first()?.selectedRev ?? (0 as FileRev);
  const pathInCommit = stack.getFileStackPath(fileStackIndex, fileRev);
  const wdirRev = prev(fileStack.revLength);
  const pathInWorkingCopy = stack.getFileStackPath(fileStackIndex, wdirRev);
  const path = pathInWorkingCopy ?? pathInCommit;

  const fileHasAnyDestinations = !props.isWdir
    ? true
    : absorbEdits.some(edit => {
        const absorbEditId = edit.absorbEditId;
        const dests = stack?.getAbsorbCommitRevs(fileStackIndex, absorbEditId);
        return dests != null && dests.candidateRevs.length > 1;
      });

  const [isCollapsed, setCollapsed] = useCollapsedFile(path, fileHasAnyDestinations);

  return (
    <div>
      {path && (
        <FileHeader
          {...(isCollapsed == null
            ? {open: undefined, onChangeOpen: undefined}
            : {
                open: isCollapsed === false,
                onChangeOpen: open => setCollapsed(!open),
              })}
          copyFrom={pathInCommit}
          path={path}
          iconType={IconType.Modified}
        />
      )}
      {fileHasAnyDestinations === false && !isCollapsed ? (
        <div className={css.fileHint}>
          <Icon icon="warning" />
          <T>This file was not changed in this stack and can't be absorbed</T>
        </div>
      ) : null}
      {
        // Edits are rendered even when collapsed, so the reordering id animation doesn't trigger when collapsing.
        props.absorbEdits
          .map((edit, i) => (
            <SingleAbsorbEdit
              collapsed={isCollapsed}
              path={path}
              edit={edit}
              key={i}
              unmovable={fileHasAnyDestinations === false}
            />
          ))
          .valueSeq()
      }
    </div>
  );
}

function SingleAbsorbEdit(props: {
  collapsed?: boolean;
  edit: AbsorbEdit;
  inDraggingOverlay?: boolean;
  path?: string;
  unmovable?: boolean;
}) {
  const {edit, inDraggingOverlay, path, unmovable} = props;
  const isDragging = useAtomValue(draggingAbsorbEdit);
  const stackEdit = useStackEditState();
  const reorderId = `absorb-${edit.fileStackIndex}-${edit.absorbEditId}`;
  const ref = useRef<HTMLDivElement | null>(null);
  const onDragRef = useAbsorbDragRef();
  const dragControllerRef = useRef<AbsorbDragController | null>(null);
  if (dragControllerRef.current == null) {
    dragControllerRef.current = new AbsorbDragController();
  }
  const dragController = dragControllerRef.current;

  const handleDrag = (x: number, y: number, isDragging: boolean) => {
    onDragRef.current?.(x, y, isDragging);
    let newDraggingHint = dragController.hint;
    if (isDragging) {
      const stack = readAtom(stackEditStack);
      if (stack == null) {
        return;
      }
      const snapshot = dragController.prepare(stack, stackEdit);
      const rev =
        snapshot == null
          ? undefined
          : findDragDestinationCommitRev(
              y - snapshot.container.getBoundingClientRect().top,
              snapshot.rows,
            );
      const fileStackIndex = nullthrows(edit.fileStackIndex);
      const absorbEditId = edit.absorbEditId;
      const targetRev = rev ?? null;
      if (dragController.isNewTarget(targetRev)) {
        newDraggingHint = null;
        if (rev != null) {
          const selectedRev = stack.getAbsorbCommitRevs(fileStackIndex, absorbEditId).selectedRev;
          if (rev !== selectedRev) {
            const commit = nullthrows(stack.get(rev));
            try {
              const newStack = stack.setAbsorbEditDestination(fileStackIndex, absorbEditId, rev);
              dragController.applyDestination(newStack, commit);
            } catch {
              // This should be unreachable.
              newDraggingHint = t(
                'Diff chunk can only be applied to a commit that modifies the file and has matching context lines.',
              );
            }
          }
        }
      }
    } else {
      dragController.finish();
      newDraggingHint = null;
    }
    // Ensure the hint is cleared when:
    // 1) not dragging. (important because the hint div interferes user interaction even if it's invisible)
    // 2) dragging back from an invalid rev to the current (valid) rev.
    if (readAtom(draggingHint) !== newDraggingHint) {
      writeAtom(draggingHint, newDraggingHint);
    }
    dragController.hint = newDraggingHint;
    const newDraggingEdit = isDragging ? edit : null;
    if (readAtom(draggingAbsorbEdit) !== newDraggingEdit) {
      writeAtom(draggingAbsorbEdit, newDraggingEdit);
    }
  };

  const ctx: Context = {
    id: {comparison: {type: ComparisonType.UncommittedChanges} as Comparison, path: path ?? ''},
    collapsed: false,
    setCollapsed: () => null,
    t,
    display: 'unified' as const,
  };

  const patch = useMemo(() => {
    const lines = [
      ...edit.oldLines.toArray().map(l => `-${l}`),
      ...edit.newLines.toArray().map(l => `+${l}`),
    ];
    return {
      oldFileName: path,
      newFileName: path,
      hunks: [
        {
          oldStart: edit.oldStart,
          oldLines: edit.oldEnd - edit.oldStart,
          newStart: edit.newStart,
          newLines: edit.newEnd - edit.newStart,
          lines,
          linedelimiters: new Array(lines.length).fill('\n'),
        },
      ],
    } as ParsedDiff;
  }, [edit, path]);

  return (
    <div
      ref={ref}
      className={cn(
        css.absorbEditSingleChunk,
        inDraggingOverlay && css.inDraggingOverlay,
        !inDraggingOverlay && isDragging === edit && css.beingDragged,
      )}
      data-reorder-id={reorderId}>
      {props.collapsed ? null : (
        <>
          <div className={css.dragHandlerWrapper}>
            <DragHandle
              onDrag={unmovable ? undefined : handleDrag}
              className={cn(css.dragHandle, unmovable ? css.unmoveable : undefined)}>
              <Icon icon="grabber" />
            </DragHandle>
            {!inDraggingOverlay && !unmovable && <SendToCommitButton edit={edit} />}
          </div>
          <SplitDiffTable ctx={ctx} path={path ?? ''} patch={patch} />
        </>
      )}
    </div>
  );
}

function SendToCommitButton({edit}: {edit: AbsorbEdit}) {
  const stackEdit = useStackEditState();
  const menu = useContextMenu(() => {
    const stack = readAtom(stackEditStack);

    const {fileStackIndex, absorbEditId} = edit;
    if (stack == null || fileStackIndex == null || absorbEditId == null) {
      return [];
    }

    const items: Array<ContextMenuItem> = [];

    const absorbRevs = stack.getAbsorbCommitRevs(fileStackIndex, absorbEditId);
    for (const rev of absorbRevs.candidateRevs.toReversed()) {
      const info = nullthrows(stack.get(rev));

      if (
        rev === absorbRevs.selectedRev ||
        (absorbRevs.selectedRev == null && info.key === YOU_ARE_HERE_VIRTUAL_COMMIT.hash)
      ) {
        // skip rev this edit is already in
        continue;
      }

      items.push({
        label: (
          <div>
            {info.key === YOU_ARE_HERE_VIRTUAL_COMMIT.hash
              ? 'Uncommitted Changes'
              : firstLine(info.text)}
          </div>
        ),
        onClick: () => {
          const newStack = stack.setAbsorbEditDestination(fileStackIndex, absorbEditId, rev);
          stackEdit.push(newStack, {name: 'absorbMove', commit: info});
        },
      });
    }
    return items;
  });
  return (
    <div className={cn(css.sendToCommitButton, 'send-to-commit')}>
      <Tooltip title={t('Move to a specific commit')}>
        <Button icon onClick={menu}>
          <Icon icon="insert" />
        </Button>
      </Tooltip>
    </div>
  );
}
