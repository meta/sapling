/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import type {RenderGlyphResult} from './RenderDag';
import type {DagCommitInfo} from './dag/dag';
import type {ExtendedGraphRow} from './dag/render';
import type {CommitTreeWidth} from './responsive';
import type {Hash} from './types';

import {Button} from 'isl-components/Button';
import {ErrorNotice} from 'isl-components/ErrorNotice';
import {Icon} from 'isl-components/Icon';
import {DOCUMENTATION_DELAY, Tooltip} from 'isl-components/Tooltip';
import {ErrorShortMessages} from 'isl-server/src/constants';
import {atom, useAtom, useAtomValue, useSetAtom} from 'jotai';
import {useEffect, useRef} from 'react';
import {normalizeForComparison} from 'shared/utils';
import {currentWorktreeName, otherWorktreeCheckoutsByHash} from './CheckedOutElsewhere';
import {CheckedOutElsewhereBadge, Commit, InlineProgressSpan} from './Commit';
import {appliedCommitTreeSearchFilter, commitTreeSearchFilter} from './CommitTreeSearchFilter';
import {Center, LargeSpinner} from './ComponentUtils';
import {EmptyState} from './EmptyState';
import {FetchingAdditionalCommitsRow} from './FetchAdditionalCommitsButton';
import {highlightedCommits} from './HighlightedCommits';
import {RegularGlyph, RenderDag, YouAreHereGlyph} from './RenderDag';
import {StackActions} from './StackActions';
import {latestCommitTitles} from './codeReview/CodeReviewInfo';
import {
  makeCheckedOutElsewhereVirtualCommit,
  YOU_ARE_HERE_VIRTUAL_COMMIT,
} from './dag/virtualCommit';
import {T, t} from './i18n';
import {localStorageBackedAtom, useAtomHas, useAtomSelect} from './jotaiUtils';
import {CreateEmptyInitialCommitOperation} from './operations/CreateEmptyInitialCommitOperation';
import {useInlineProgress, useRunOperation} from './operationsState';
import {dagWithPreviews, treeWithPreviews, useMarkOperationsCompleted} from './previews';
import {hideIrrelevantCwdStacks, isIrrelevantToCwd, repoRelativeCwd} from './repositoryData';
import {commitTreeWidth} from './responsive';
import {
  selectedCommits,
  useArrowKeysToChangeSelection,
  useBackspaceToHideSelected,
  useCommitCallbacks,
  useShortcutToRebaseSelected,
} from './selection';
import {commitFetchError, latestUncommittedChangesData} from './serverAPIState';
import {MaybeEditStackModal} from './stackEdit/ui/EditStackModal';

import './CommitTreeList.css';
import {tracker} from './analytics';
import {focusMode} from './atoms/FocusModeState';

type DagCommitListProps = {
  width: CommitTreeWidth;
};

const YOU_ARE_HERE_ANCHOR_ID = 'isl-you-are-here-anchor';

/**
 * Where the "You are here" row is relative to the scroll viewport: visible, or
 * scrolled off the top ('above') / bottom ('below'). Drives the floating button.
 */
type YouAreHerePosition = 'visible' | 'above' | 'below';
const youAreHerePosition = atom<YouAreHerePosition>('visible');

/**
 * Scroll the commit graph so the "You are here" row is visible.
 * Returns false if the anchor is not in the DOM yet (nothing scrolled).
 */
function scrollToYouAreHere(behavior: ScrollBehavior = 'smooth'): boolean {
  // ponytail: getElementById avoids threading a ref from the DAG row up to the scroll controls.
  const anchor = document.getElementById(YOU_ARE_HERE_ANCHOR_ID);
  // The anchor is a zero-height span trailing the badge; scroll the container so the
  // badge above it isn't clipped at the top edge.
  const container = anchor?.parentElement;
  if (container == null) {
    return false;
  }
  container.scrollIntoView({behavior, block: 'start'});
  return true;
}

const dagWithVirtualCommits = atom(get => {
  let dag = get(dagWithPreviews);
  const virtualCommits = [];
  // Insert a virtual "You are here" as a child of ".".
  const dot = dag.resolve('.');
  if (dot != null) {
    virtualCommits.push(YOU_ARE_HERE_VIRTUAL_COMMIT.set('parents', [dot.hash]));
  }
  // Insert a virtual "checked out elsewhere" commit as a child of each hash
  // that has sibling worktree checkouts, skipping hashes not present in the dag.
  for (const hash of get(otherWorktreeCheckoutsByHash).keys()) {
    if (dag.has(hash)) {
      virtualCommits.push(makeCheckedOutElsewhereVirtualCommit(hash));
    }
  }
  if (virtualCommits.length > 0) {
    dag = dag.add(virtualCommits);
  }
  return dag;
});

export const condenseObsoleteStacks = localStorageBackedAtom<boolean | null>(
  'isl.condense-obsolete-stacks',
  true,
  {portable: true},
);

/** Opt-in because auto-scrolling on open can be intrusive. */
export const scrollToYouAreHereOnOpen = localStorageBackedAtom<boolean>(
  'isl.scroll-to-you-are-here-on-open',
  false,
  {portable: true},
);

const renderSubsetUnionSelection = atom(get => {
  const dag = get(dagWithVirtualCommits);
  const condense = get(condenseObsoleteStacks);
  let subset = dag.subsetForRendering(undefined, /* condenseObsoleteStacks */ condense !== false);
  // If selectedCommits includes commits unknown to dag (ex. in tests), ignore them to avoid errors.
  const selection = dag.present(get(selectedCommits));

  const hideIrrelevant = get(hideIrrelevantCwdStacks);
  if (hideIrrelevant) {
    const cwd = get(repoRelativeCwd);
    subset = dag.filter(commit => commit.isDot || !isIrrelevantToCwd(commit, cwd), subset);
  }

  // Deliberately the debounced atom, not `commitTreeSearchFilter`: recomputing this walks
  // the whole dag, so reading the per-keystroke one costs a full pass per character typed.
  const searchFilter = get(appliedCommitTreeSearchFilter).trim().toLowerCase();
  if (searchFilter.length > 0) {
    const matchesSearch = (commit: DagCommitInfo) => {
      if (commit.isYouAreHere || commit.isCheckedOutElsewhere) {
        return true;
      }
      const renderedTitle = get(latestCommitTitles).get(commit.hash) ?? '';
      const searchable = [
        renderedTitle,
        commit.diffId ?? '',
        ...commit.bookmarks,
        ...commit.remoteBookmarks,
      ];
      return searchable.some(s => s.toLowerCase().includes(searchFilter));
    };
    return dag.filter(matchesSearch, subset.union(selection));
  }

  return subset.union(selection);
});

function DagCommitList(props: DagCommitListProps) {
  const {width} = props;

  const dag = useAtomValue(dagWithVirtualCommits);
  const subset = useAtomValue(renderSubsetUnionSelection);
  // `hasNoResults` below describes `subset`, so it reads the same filter `subset` was built
  // from rather than whatever has been typed since. Clearing, though, writes the atom the
  // text box is bound to.
  const appliedSearchFilter = useAtomValue(appliedCommitTreeSearchFilter);
  const setSearchFilter = useSetAtom(commitTreeSearchFilter);

  // Check if filter is active and no commits (excluding "You are here") match
  const filter = appliedSearchFilter.trim().toLowerCase();
  let hasNoResults = false;
  if (filter.length > 0) {
    let hasMatchingCommit = false;
    for (const hash of subset) {
      const commit = dag.get(hash);
      if (commit && !commit.isYouAreHere && !commit.isCheckedOutElsewhere) {
        hasMatchingCommit = true;
        break;
      }
    }
    hasNoResults = !hasMatchingCommit;
  }

  if (hasNoResults) {
    return (
      <EmptyState>
        <T>No commits match your filter</T>
        <Button onClick={() => setSearchFilter('')}>
          <T>Clear filter</T>
        </Button>
      </EmptyState>
    );
  }

  return (
    <RenderDag
      dag={dag}
      subset={subset}
      className={
        'commit-tree-root ' +
        (width !== 'wide' ? ' commit-tree-narrow' : '') +
        (width === 'very-narrow' ? ' commit-tree-very-narrow' : '')
      }
      data-testid="commit-tree-root"
      renderCommit={renderCommit}
      renderCommitExtras={renderCommitExtras}
      renderGlyph={renderGlyph}
      useExtraCommitRowProps={useExtraCommitRowProps}
    />
  );
}

function FocusModeIndicator() {
  const [focused, setFocused] = useAtom(focusMode);
  if (!focused) {
    return null;
  }
  return (
    <Button
      onClick={() => {
        tracker.track('SetFocusMode', {extras: {focus: false}});
        setFocused(false);
      }}
      icon>
      <Icon icon="screen-normal" />
      <T>Focus mode is on. Disable to see additional commits</T>
    </Button>
  );
}

function renderCommit(info: DagCommitInfo): React.JSX.Element | null {
  if (info.isCheckedOutElsewhere) {
    return null;
  }
  return <DagCommitBody info={info} />;
}

function renderCommitExtras(info: DagCommitInfo, row: ExtendedGraphRow) {
  return <CommitExtras info={info} row={row} />;
}

function CommitExtras({info, row}: {info: DagCommitInfo; row: ExtendedGraphRow}) {
  const focused = useAtomValue(focusMode);
  if (row.termLine != null && (info.parents.length > 0 || (info.ancestors?.size ?? 0) > 0)) {
    // Root (no parents) in the displayed DAG, but not root in the full DAG.
    return focused ? (
      <FocusModeIndicator />
    ) : (
      <MaybeFetchingAdditionalCommitsRow hash={info.hash} />
    );
  } else if (info.phase === 'draft') {
    // Draft but parents are not drafts. Likely a stack root. Show stack buttons.
    return <MaybeStackActions hash={info.hash} />;
  }
  return null;
}

function renderGlyph(info: DagCommitInfo): RenderGlyphResult {
  if (info.isYouAreHere) {
    return ['replace-tile', <YouAreHereGlyphWithProgress key="glyph" info={info} />];
  } else if (info.isCheckedOutElsewhere) {
    return ['replace-tile', <CheckedOutElsewhereGlyph key="glyph" info={info} />];
  } else {
    return ['inside-tile', <HighlightedGlyph key="glyph" info={info} />];
  }
}

function useExtraCommitRowProps(info: DagCommitInfo): React.HTMLAttributes<HTMLDivElement> | void {
  const {isSelected, onClickToSelect, onDoubleClickToShowDrawer} = useCommitCallbacks(info);

  return {
    onClick: onClickToSelect,
    onDoubleClick: onDoubleClickToShowDrawer,
    className: isSelected ? 'commit-row-selected' : '',
  };
}

function YouAreHereGlyphWithProgress({info}: {info: DagCommitInfo}) {
  const inlineProgress = useInlineProgress(info.hash);
  const setPosition = useSetAtom(youAreHerePosition);
  const anchorRef = useRef<HTMLSpanElement>(null);
  useEffect(() => {
    // Observe the badge container (the anchor's parent has real height) to drive the
    // floating "scroll to here" button, which shows when the row is off-screen and
    // points toward wherever it scrolled off.
    const el = anchorRef.current?.parentElement;
    if (el == null) {
      return;
    }
    const scrollRoot = el.closest<HTMLElement>('.main-content-area');
    if (scrollRoot == null) {
      return;
    }

    const recalculatePosition = () => {
      if (scrollRoot.scrollHeight <= scrollRoot.clientHeight) {
        setPosition('visible');
        return;
      }

      const bounds = scrollRoot.getBoundingClientRect();
      const scaleY = scrollRoot.offsetHeight === 0 ? 1 : bounds.height / scrollRoot.offsetHeight;
      const viewportTop = bounds.top + scrollRoot.clientTop * scaleY;
      const viewportBottom = viewportTop + scrollRoot.clientHeight * scaleY;
      const targetBounds = el.getBoundingClientRect();
      if (targetBounds.bottom < viewportTop) {
        setPosition('above');
      } else if (targetBounds.top > viewportBottom) {
        setPosition('below');
      } else {
        // IntersectionObserver also reports horizontal clipping, which cannot be fixed by scrolling.
        setPosition('visible');
      }
    };

    const intersectionObserver = new IntersectionObserver(recalculatePosition, {
      root: scrollRoot,
      // Track vertical visibility even when a narrow commit graph is horizontally clipped.
      rootMargin: '0px 100000px',
      threshold: 0,
    });
    intersectionObserver.observe(el);

    const resizeObserver = new ResizeObserver(recalculatePosition);
    resizeObserver.observe(scrollRoot);
    const commitTree = el.closest<HTMLElement>('.commit-tree-root');
    if (commitTree != null) {
      resizeObserver.observe(commitTree);
    }

    return () => {
      intersectionObserver.disconnect();
      resizeObserver.disconnect();
      // No "You are here" row rendered -> nothing to scroll to, so hide the button.
      setPosition('visible');
    };
  }, [setPosition]);
  const worktreeName = useAtomValue(currentWorktreeName);
  return (
    <YouAreHereGlyph
      info={info}
      badgeChildren={
        worktreeName != null && (
          <Tooltip title={t('Current worktree')}>
            <span className="current-worktree-label">
              <Icon icon="worktree" aria-hidden="true" />
              {worktreeName}
            </span>
          </Tooltip>
        )
      }>
      <span id={YOU_ARE_HERE_ANCHOR_ID} ref={anchorRef} />
      {inlineProgress && <InlineProgressSpan message={inlineProgress} />}
    </YouAreHereGlyph>
  );
}

// Sticky (not fixed) so it centers over the commit-tree column (`.main-content-area`)
// rather than the whole panel — the commit-info sidebar is a separate drawer outside it.
// Rendered in the 'above' slot (first child, sticks to top) or 'below' slot (last child,
// sticks to bottom) so it points toward wherever the current commit scrolled off.
function ScrollToCurrentCommitButton({slot}: {slot: 'above' | 'below'}) {
  const position = useAtomValue(youAreHerePosition);
  if (position !== slot) {
    return null;
  }
  const atTop = slot === 'above';
  return (
    <div className={'scroll-to-current-commit ' + (atTop ? 'floating-top' : 'floating-bottom')}>
      <Tooltip
        delayMs={DOCUMENTATION_DELAY}
        placement={atTop ? 'bottom' : 'top'}
        title={<T>Scroll to your current commit ("You are here")</T>}>
        <Button
          primary
          className="scroll-to-current-commit-pill"
          onClick={() => scrollToYouAreHere('smooth')}
          data-testid="scroll-to-current-commit-button">
          <Icon icon={atTop ? 'arrow-up' : 'arrow-down'} />
          <T>Scroll to current commit</T>
        </Button>
      </Tooltip>
    </div>
  );
}

function CheckedOutElsewhereGlyph({info}: {info: DagCommitInfo}) {
  const map = useAtomValue(otherWorktreeCheckoutsByHash);
  const worktrees = info.parents.flatMap(p => map.get(p) ?? []);
  if (worktrees.length === 0) {
    return null;
  }
  const dedupeKey = (p: string) => {
    // Case-fold Windows absolute paths so drive-letter case differences dedupe,
    // matching pathsAreIdentical's comparison semantics.
    const normalized = normalizeForComparison(p);
    return /^[A-Za-z]:\//.test(normalized) ? normalized.toLowerCase() : normalized;
  };
  const seen = new Set<string>();
  const deduped = worktrees.filter(wt => {
    const k = dedupeKey(wt.path);
    return seen.has(k) ? false : (seen.add(k), true);
  });
  return (
    <div className="checked-out-elsewhere-container">
      {deduped.map(wt => (
        <CheckedOutElsewhereBadge key={wt.path} wt={wt} />
      ))}
    </div>
  );
}

function DagCommitBody({info}: {info: DagCommitInfo}) {
  const hasChildren = useAtomSelect(dagWithPreviews, dag => dag.children(info.hash).size > 0, [
    info.hash,
  ]);
  return (
    <Commit
      commit={info}
      key={info.hash}
      previewType={info.previewType}
      hasChildren={hasChildren}
    />
  );
}

function MaybeFetchingAdditionalCommitsRow({hash}: {hash: Hash}) {
  const hasParents = useAtomSelect(dagWithPreviews, dag => dag.parents(hash).size > 0, [hash]);
  return hasParents ? null : <FetchingAdditionalCommitsRow />;
}

function MaybeStackActions({hash}: {hash: Hash}) {
  const isDraftStackRoot = useAtomSelect(
    dagWithPreviews,
    dag => dag.draft(dag.parents(hash)).size === 0,
    [hash],
  );
  return isDraftStackRoot ? <StackActions hash={hash} /> : null;
}

function HighlightedGlyph({info}: {info: DagCommitInfo}) {
  const highlighted = useAtomHas(highlightedCommits, info.hash);

  const highlightCircle = highlighted ? (
    <circle cx={0} cy={0} r={8} fill="transparent" stroke="var(--focus-border)" strokeWidth={4} />
  ) : null;

  return (
    <>
      {highlightCircle}
      <RegularGlyph info={info} />
    </>
  );
}

export function CommitTreeList() {
  // Make sure we trigger subscription to changes to uncommitted changes *before* we have a tree to render,
  // so we don't miss the first returned uncommitted changes message.
  // TODO: This is a little ugly, is there a better way to tell recoil to start the subscription immediately?
  // Or should we queue/cache messages?
  useAtomValue(latestUncommittedChangesData);
  useMarkOperationsCompleted();

  useArrowKeysToChangeSelection();
  useBackspaceToHideSelected();
  useShortcutToRebaseSelected();

  const treeWidth = useAtomValue(commitTreeWidth);

  const {trees} = useAtomValue(treeWithPreviews);
  const fetchError = useAtomValue(commitFetchError);
  const shouldScrollToYouAreHereOnOpen = useAtomValue(scrollToYouAreHereOnOpen);

  const hasAutoScrolled = useRef(false);
  useEffect(() => {
    if (!shouldScrollToYouAreHereOnOpen || hasAutoScrolled.current || trees.length === 0) {
      return;
    }
    hasAutoScrolled.current = scrollToYouAreHere('auto');
  }, [trees, shouldScrollToYouAreHereOnOpen]);

  return fetchError == null && trees.length === 0 ? (
    <Center>
      <LargeSpinner />
    </Center>
  ) : (
    <>
      {fetchError ? <CommitFetchError error={fetchError} /> : null}
      <ScrollToCurrentCommitButton slot="above" />
      <DagCommitList width={treeWidth} />
      <ScrollToCurrentCommitButton slot="below" />
      <MaybeEditStackModal />
    </>
  );
}

function CommitFetchError({error}: {error: Error}) {
  const runOperation = useRunOperation();
  if (error.message === ErrorShortMessages.NoCommitsFetched) {
    return (
      <ErrorNotice
        title={t('No commits found')}
        description={t('If this is a new repository, try adding an initial commit first.')}
        error={error}
        buttons={[
          <Button
            key="create-initial-commit"
            onClick={() => {
              runOperation(new CreateEmptyInitialCommitOperation());
            }}>
            <T>Create empty initial commit</T>
          </Button>,
        ]}
      />
    );
  }
  if (error.message === ErrorShortMessages.TooManyCommits) {
    return (
      <ErrorNotice
        title={t('Too many commits to render')}
        description={t(
          'Check your public branch configuration, or run `sl doctor` to hide unrelated commits.',
        )}
        error={error}
      />
    );
  }
  return <ErrorNotice title={t('Failed to fetch commits')} error={error} />;
}
