/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import type {Comparison} from 'shared/Comparison';
import type {ParsedDiff} from 'shared/patch/types';
import type {Result} from '../types';
import type {Context} from './SplitDiffView/types';

import deepEqual from 'fast-deep-equal';
import {Banner, BannerKind} from 'isl-components/Banner';
import {Button} from 'isl-components/Button';
import {Checkbox} from 'isl-components/Checkbox';
import {Dropdown} from 'isl-components/Dropdown';
import {ErrorBoundary, ErrorNotice} from 'isl-components/ErrorNotice';
import {Icon} from 'isl-components/Icon';
import {RadioGroup} from 'isl-components/Radio';
import {Subtle} from 'isl-components/Subtle';
import {Tooltip} from 'isl-components/Tooltip';
import {atom, useAtom, useAtomValue, useSetAtom} from 'jotai';
import {useEffect, useMemo, useState} from 'react';
import {
  ComparisonType,
  comparisonIsAgainstHead,
  comparisonStringKey,
  labelForComparison,
} from 'shared/Comparison';
import {group, notEmpty, nullthrows} from 'shared/utils';
import serverAPI from '../ClientToServerAPI';
import {EmptyState} from '../EmptyState';
import {useGeneratedFileStatuses} from '../GeneratedFile';
import {T, t} from '../i18n';
import {atomFamilyWeak, atomLoadableWithRefresh, localStorageBackedAtom} from '../jotaiUtils';
import {AddOperation} from '../operations/AddOperation';
import {AddRemoveOperation} from '../operations/AddRemoveOperation';
import {useRunOperation} from '../operationsState';
import platform from '../platform';
import {dagWithPreviews, uncommittedChangesWithPreviews} from '../previews';
import {
  latestHeadCommit,
  latestUncommittedChanges,
  latestUncommittedChangesData,
} from '../serverAPIState';
import {copyAndShowToast} from '../toast';
import {GeneratedStatus} from '../types';
import {SplitDiffView} from './SplitDiffView';
import {currentComparisonMode} from './atoms';
import {parsePatchAndFilter, sortFilesByType} from './utils';

import './ComparisonView.css';

/**
 * Transform Result<T> to Result<U> by applying `fn` on result.value.
 * If the result is an error, just return it unchanged.
 */
function mapResult<T, U>(result: Result<T>, fn: (t: T) => U): Result<U> {
  return result.error == null ? {value: fn(result.value)} : result;
}

const comparisonIgnoreWhitespace = localStorageBackedAtom<boolean>(
  'isl.comparison-ignore-whitespace',
  true,
  {portable: true},
);

const currentComparisonData = atomFamilyWeak((comparison: Comparison) =>
  atomLoadableWithRefresh<Result<Array<ParsedDiff>>>(async get => {
    const ignoreWhitespace = get(comparisonIgnoreWhitespace);
    serverAPI.postMessage({
      type: 'requestComparison',
      comparison,
      ...(ignoreWhitespace ? {ignoreWhitespace} : {}),
    });
    const event = await serverAPI.nextMessageMatching(
      'comparison',
      event =>
        deepEqual(comparison, event.comparison) &&
        (event.ignoreWhitespace ?? false) === ignoreWhitespace,
    );
    return mapResult(event.data.diff, parsePatchAndFilter);
  }),
);

/**
 * Whether the right side of a comparison matches the current version of the
 * file on disk, so clickable line numbers can be offered.
 * Derived state (head commit + optimistic working-copy status), kept in a
 * Jotai atom family so it's reusable/testable and stays in sync with previews.
 */
const comparisonTargetIsCurrentAtom = atomFamilyWeak((comparison: Comparison) =>
  atom(get => {
    if (comparisonIsAgainstHead(comparison)) {
      return true;
    }
    // Use preview-aware dag so head hash reflects optimistic state while an
    // operation (e.g. amend/goto) is in flight, not the last fetched status.
    const headHash = get(dagWithPreviews).resolve('.')?.hash;
    if (headHash == null) {
      return false;
    }
    // Before the first uncommittedChanges subscription result arrives, files
    // is [] which would incorrectly look clean. Be conservative and don't
    // offer clickable line numbers until we've fetched at least once.
    const uncommittedData = get(latestUncommittedChangesData);
    if (uncommittedData.fetchCompletedTimestamp <= 0) {
      return false;
    }
    // Use preview-aware changes so dirtiness reflects optimistic state.
    const workingCopyDirty = get(uncommittedChangesWithPreviews).length > 0;
    if (workingCopyDirty) {
      return false;
    }
    switch (comparison.type) {
      case ComparisonType.Committed:
      case ComparisonType.SinceLastCodeReviewSubmit:
        return comparison.hash === headHash;
      case ComparisonType.CommitRange:
        return comparison.hashTo === headHash;
      default:
        return false;
    }
  }),
);

/**
 * Untracked files are never in a comparison's diff, so their focused diff view would be empty.
 * Reads the status last reported by the server rather than the optimistic one, so the notice
 * stays up until tracking the file has finished.
 */
const focusedFileIsUntrackedAtom = atomFamilyWeak((path: string) =>
  atom(
    get =>
      path !== '' &&
      get(latestUncommittedChanges).some(file => file.path === path && file.status === '?'),
  ),
);

/**
 * Untracked files in the working copy, which are left out of every comparison against head.
 * Like {@link focusedFileIsUntrackedAtom}, this ignores optimistic state.
 */
const untrackedFilesAtom = atom(get =>
  get(latestUncommittedChanges)
    .filter(file => file.status === '?')
    .map(file => file.path),
);

const MAX_UNTRACKED_FILES_IN_TOOLTIP = 20;

function UntrackedFilesBanner({
  paths,
  onTracked,
}: {
  paths: Array<string>;
  onTracked: () => unknown;
}) {
  const runOperation = useRunOperation();
  const [isTracking, setIsTracking] = useState(false);
  const shownPaths = paths.slice(0, MAX_UNTRACKED_FILES_IN_TOOLTIP);
  const hiddenCount = paths.length - shownPaths.length;
  const tooltip = [
    ...shownPaths,
    ...(hiddenCount > 0 ? [t('andMoreFiles', {count: hiddenCount})] : []),
  ].join('\n');
  return (
    <Banner
      className="comparison-view-untracked-files-banner"
      kind={BannerKind.warning}
      icon={<Icon icon="warning" />}
      alwaysShowButtons
      buttons={
        <Button
          data-testid="comparison-track-untracked-files"
          disabled={isTracking}
          onClick={async () => {
            setIsTracking(true);
            await runOperation(new AddRemoveOperation(paths));
            setIsTracking(false);
            onTracked();
          }}>
          <Icon icon={isTracking ? 'loading' : 'add'} slot="start" />
          <T count={paths.length}>trackUntrackedFiles</T>
        </Button>
      }>
      {paths.length === 1 ? (
        <span data-testid="comparison-untracked-files-banner">
          <T replace={{$path: paths[0]}}>$path is untracked and not included in this comparison</T>
        </span>
      ) : (
        <Tooltip title={tooltip} placement="bottom">
          <span data-testid="comparison-untracked-files-banner">
            <T count={paths.length}>untrackedFilesNotShownInComparison</T>
          </span>
        </Tooltip>
      )}
    </Banner>
  );
}

function UntrackedFileNotice({path, onTracked}: {path: string; onTracked: () => unknown}) {
  const runOperation = useRunOperation();
  const [isTracking, setIsTracking] = useState(false);
  return (
    <EmptyState>
      <T>This file is untracked</T>
      <Subtle>
        <T>Track it to see its changes here and include it in a commit.</T>
      </Subtle>
      <Button
        data-testid="comparison-track-untracked-file"
        disabled={isTracking}
        onClick={async () => {
          setIsTracking(true);
          await runOperation(new AddOperation(path));
          setIsTracking(false);
          onTracked();
        }}>
        <Icon icon={isTracking ? 'loading' : 'add'} slot="start" />
        <T>Track file</T>
      </Button>
    </EmptyState>
  );
}

type LineRangeKey = string;
export function keyForLineRange(param: {path: string; comparison: Comparison}): LineRangeKey {
  return `${param.path}:${comparisonStringKey(param.comparison)}`;
}

type ComparisonDisplayMode = 'unified' | 'split';
const comparisonDisplayMode = localStorageBackedAtom<ComparisonDisplayMode | 'responsive'>(
  'isl.comparison-display-mode',
  'responsive',
  {
    portable: true,
    validate: value => ['responsive', 'unified', 'split'].includes(value as string),
  },
);

export default function ComparisonView({
  comparison,
  dismiss,
  focusedFile,
}: {
  comparison: Comparison;
  dismiss?: () => void;
  focusedFile?: string;
}) {
  const [compared, reloadComparison] = useAtom(currentComparisonData(comparison));
  const focusedFileIsUntracked = useAtomValue(
    focusedFileIsUntrackedAtom(
      focusedFile != null && comparisonIsAgainstHead(comparison) ? focusedFile : '',
    ),
  );
  const untrackedFiles = useAtomValue(untrackedFilesAtom);
  const showUntrackedFilesBanner =
    focusedFile == null && comparisonIsAgainstHead(comparison) && untrackedFiles.length > 0;

  const displayMode = useComparisonDisplayMode();

  const data = compared.state === 'hasData' ? compared.data : null;

  const paths = useMemo(
    () => data?.value?.map(file => file.newFileName).filter(notEmpty) ?? [],
    [data?.value],
  );
  const generatedStatuses = useGeneratedFileStatuses(paths);
  const [collapsedFiles, setCollapsedFile] = useCollapsedFilesState({
    isLoading: compared.state === 'loading',
    data,
  });

  let content;
  if (data == null) {
    content = <Icon icon="loading" />;
  } else if (compared.state === 'hasError') {
    const error = compared.error instanceof Error ? compared.error : new Error(`${compared.error}`);
    content = <ErrorNotice error={error} title={t('Unable to load comparison')} />;
  } else if (focusedFileIsUntracked) {
    content = <UntrackedFileNotice path={nullthrows(focusedFile)} onTracked={reloadComparison} />;
  } else if (data?.value && data.value.length === 0) {
    content =
      comparison.type === ComparisonType.SinceLastCodeReviewSubmit ? (
        <EmptyState>
          <T>No Content Changes</T>
          <Subtle>
            <T>This commit might have been rebased</T>
          </Subtle>
        </EmptyState>
      ) : (
        <EmptyState>
          <T>No Changes</T>
        </EmptyState>
      );
  } else {
    let files = [...(data.value ?? [])];
    if (focusedFile != null) {
      files = files.filter(f => f.newFileName === focusedFile || f.oldFileName === focusedFile);
    }
    sortFilesByType(files);
    const fileGroups = group(files, file => generatedStatuses[file.newFileName ?? '']);
    content = (
      <>
        {fileGroups[GeneratedStatus.Manual]?.map((parsed, i) => (
          <ComparisonViewFile
            diff={parsed}
            comparison={comparison}
            key={i}
            collapsed={collapsedFiles.get(parsed.newFileName ?? '') ?? false}
            setCollapsed={(collapsed: boolean) =>
              setCollapsedFile(parsed.newFileName ?? '', collapsed)
            }
            generatedStatus={GeneratedStatus.Manual}
            displayMode={displayMode}
          />
        ))}
        {fileGroups[GeneratedStatus.PartiallyGenerated]?.map((parsed, i) => (
          <ComparisonViewFile
            diff={parsed}
            comparison={comparison}
            key={i}
            collapsed={collapsedFiles.get(parsed.newFileName ?? '') ?? false}
            setCollapsed={(collapsed: boolean) =>
              setCollapsedFile(parsed.newFileName ?? '', collapsed)
            }
            generatedStatus={GeneratedStatus.PartiallyGenerated}
            displayMode={displayMode}
          />
        ))}
        {fileGroups[GeneratedStatus.Generated]?.map((parsed, i) => (
          <ComparisonViewFile
            diff={parsed}
            comparison={comparison}
            key={i}
            collapsed={collapsedFiles.get(parsed.newFileName ?? '') ?? false}
            setCollapsed={(collapsed: boolean) =>
              setCollapsedFile(parsed.newFileName ?? '', collapsed)
            }
            generatedStatus={GeneratedStatus.Generated}
            displayMode={displayMode}
          />
        ))}
      </>
    );
  }

  return (
    <div data-testid="comparison-view" className="comparison-view">
      <ComparisonViewHeader
        comparison={comparison}
        collapsedFiles={collapsedFiles}
        setCollapsedFile={setCollapsedFile}
        dismiss={dismiss}
        focusedFile={focusedFile}
      />
      <div className="comparison-view-details">
        {showUntrackedFilesBanner && (
          <UntrackedFilesBanner paths={untrackedFiles} onTracked={reloadComparison} />
        )}
        {content}
      </div>
    </div>
  );
}

const defaultComparisons = [
  ComparisonType.UncommittedChanges as const,
  ComparisonType.HeadChanges as const,
  ComparisonType.StackChanges as const,
];
function ComparisonViewHeader({
  comparison,
  collapsedFiles,
  setCollapsedFile,
  dismiss,
  focusedFile,
}: {
  comparison: Comparison;
  collapsedFiles: Map<string, boolean>;
  setCollapsedFile: (path: string, collapsed: boolean) => unknown;
  dismiss?: () => void;
  focusedFile?: string;
}) {
  const setComparisonMode = useSetAtom(currentComparisonMode);
  const [compared, reloadComparison] = useAtom(currentComparisonData(comparison));

  const data = compared.state === 'hasData' ? compared.data : null;

  const allFilesExpanded =
    data?.value?.every(
      file => file.newFileName && collapsedFiles.get(file.newFileName) === false,
    ) === true;
  const noFilesExpanded =
    data?.value?.every(
      file => file.newFileName && collapsedFiles.get(file.newFileName) === true,
    ) === true;
  const isLoading = compared.state === 'loading';

  return (
    <>
      <div className="comparison-view-header">
        <span className="comparison-view-header-group">
          <Dropdown
            data-testid="comparison-view-picker"
            value={comparison.type}
            onChange={event => {
              const newComparison = {
                type: (event as React.FormEvent<HTMLSelectElement>).currentTarget
                  .value as (typeof defaultComparisons)[0],
              };
              setComparisonMode(previous => ({
                ...previous,
                comparison: newComparison,
              }));
              // When viewed in a dedicated viewer, change the title as the comparison changes
              if (window.islAppMode != null && window.islAppMode.mode != 'isl') {
                serverAPI.postMessage({
                  type: 'platform/changeTitle',
                  title: labelForComparison(newComparison),
                });
              }
            }}
            options={[
              ...defaultComparisons.map(comparison => ({
                value: comparison,
                name: labelForComparison({type: comparison}),
              })),

              !defaultComparisons.includes(comparison.type as (typeof defaultComparisons)[0])
                ? {value: comparison.type, name: labelForComparison(comparison)}
                : undefined,
            ].filter(notEmpty)}
          />
          <Tooltip
            delayMs={1000}
            title={t('Reload this comparison. Comparisons do not refresh automatically.')}>
            <Button onClick={reloadComparison}>
              <Icon icon="refresh" data-testid="comparison-refresh-button" />
            </Button>
          </Tooltip>
          {focusedFile == null && (
            <Button
              onClick={() => {
                for (const file of data?.value ?? []) {
                  if (file.newFileName) {
                    setCollapsedFile(file.newFileName, false);
                  }
                }
              }}
              disabled={isLoading || allFilesExpanded}
              icon>
              <Icon icon="unfold" slot="start" />
              <T>Expand all files</T>
            </Button>
          )}
          {focusedFile == null && (
            <Button
              onClick={() => {
                for (const file of data?.value ?? []) {
                  if (file.newFileName) {
                    setCollapsedFile(file.newFileName, true);
                  }
                }
              }}
              icon
              disabled={isLoading || noFilesExpanded}>
              <Icon icon="fold" slot="start" />
              <T>Collapse all files</T>
            </Button>
          )}
          <Tooltip trigger="click" component={() => <ComparisonSettingsDropdown />}>
            <Button icon>
              <Icon icon="ellipsis" />
            </Button>
          </Tooltip>
          {isLoading ? <Icon icon="loading" data-testid="comparison-loading" /> : null}
        </span>
        {dismiss == null ? null : (
          <Button data-testid="close-comparison-view-button" icon onClick={dismiss}>
            <Icon icon="x" />
          </Button>
        )}
      </div>
    </>
  );
}

function ComparisonSettingsDropdown() {
  const [mode, setMode] = useAtom(comparisonDisplayMode);
  const [ignoreWhitespace, setIgnoreWhitespace] = useAtom(comparisonIgnoreWhitespace);
  return (
    <div className="dropdown-field">
      <RadioGroup
        title={t('Comparison Display Mode')}
        choices={[
          {value: 'responsive', title: <T>Responsive</T>},
          {value: 'split', title: <T>Split</T>},
          {value: 'unified', title: <T>Unified</T>},
        ]}
        current={mode}
        onChange={setMode}
      />
      <Checkbox checked={ignoreWhitespace} onChange={setIgnoreWhitespace}>
        <T>Ignore Whitespace</T>
      </Checkbox>
    </div>
  );
}

/**
 * Derive from the parsed diff state which files should be expanded or collapsed by default.
 * This state is the source of truth of which files are expanded/collapsed.
 * This is a hook instead of a recoil selector since it depends on the comparison
 * which is a prop.
 */
function useCollapsedFilesState(data: {
  isLoading: boolean;
  data: Result<Array<ParsedDiff>> | null;
}): [Map<string, boolean>, (path: string, collapsed: boolean) => void] {
  const [collapsedFiles, setCollapsedFiles] = useState(new Map());

  useEffect(() => {
    if (data.isLoading || data.data?.value == null) {
      return;
    }

    const newCollapsedFiles = new Map(collapsedFiles);

    // Allocate a number of changed lines we're willing to show expanded by default,
    // add files until we just cross that threshold.
    // This means a single very large file will start expanded already.
    const TOTAL_DEFAULT_EXPANDED_SIZE = 4000;
    let accumulatedSize = 0;
    let indexToStartCollapsing = Infinity;
    for (const [i, diff] of data.data.value.entries()) {
      const sizeThisFile = diff.hunks.reduce((last, hunk) => last + hunk.lines.length, 0);
      accumulatedSize += sizeThisFile;
      if (accumulatedSize > TOTAL_DEFAULT_EXPANDED_SIZE) {
        indexToStartCollapsing = i;
        break;
      }
    }

    let anyChanged = false;
    for (const [i, diff] of data.data.value.entries()) {
      if (!newCollapsedFiles.has(diff.newFileName)) {
        newCollapsedFiles.set(diff.newFileName, i > 0 && i >= indexToStartCollapsing);
        anyChanged = true;
      }
      // Leave existing files alone in case the user changed their expanded state.
    }
    if (anyChanged) {
      setCollapsedFiles(newCollapsedFiles);
      // We don't bother removing files that no longer appear in the list of files.
      // That's not a big deal, this state is local to this instance of the comparison view anyway.
    }
  }, [data, collapsedFiles]);

  const setCollapsed = (path: string, collapsed: boolean) => {
    setCollapsedFiles(prev => {
      const map = new Map(prev);
      map.set(path, collapsed);
      return map;
    });
  };

  return [collapsedFiles, setCollapsed];
}

function splitOrUnifiedBasedOnWidth() {
  return window.innerWidth > 600 ? 'split' : 'unified';
}
function useComparisonDisplayMode(): ComparisonDisplayMode {
  const underlyingMode = useAtomValue(comparisonDisplayMode);
  const [mode, setMode] = useState(
    underlyingMode === 'responsive' ? splitOrUnifiedBasedOnWidth() : underlyingMode,
  );
  useEffect(() => {
    if (underlyingMode !== 'responsive') {
      setMode(underlyingMode);
      return;
    }
    const update = () => {
      setMode(splitOrUnifiedBasedOnWidth());
    };
    update();
    window.addEventListener('resize', update);
    return () => window.removeEventListener('resize', update);
  }, [underlyingMode, setMode]);

  return mode;
}

const latestHeadCommitHash = atom(get => get(latestHeadCommit)?.hash ?? '');

function ComparisonViewFile({
  diff,
  comparison,
  collapsed,
  setCollapsed,
  generatedStatus,
  displayMode,
}: {
  diff: ParsedDiff;
  comparison: Comparison;
  collapsed: boolean;
  setCollapsed: (isCollapsed: boolean) => void;
  generatedStatus: GeneratedStatus;
  displayMode: ComparisonDisplayMode;
}) {
  const path = diff.newFileName ?? diff.oldFileName ?? '';
  // Offer clickable line numbers whenever the right side of the comparison matches
  // the current version of the file on disk and the working copy is clean.
  const comparisonTargetIsCurrent = useAtomValue(comparisonTargetIsCurrentAtom(comparison));
  const context: Context = {
    id: {path, comparison},
    copy: copyAndShowToast,
    openFile: () => platform.openFile(path),
    // only offer clickable line numbers when the right side line numbers match the file on disk
    openFileToLine: comparisonTargetIsCurrent
      ? (line: number) => platform.openFile(path, {line})
      : undefined,

    async fetchAdditionalLines(id, start, numLines) {
      serverAPI.postMessage({
        type: 'requestComparisonContextLines',
        numLines,
        start,
        id,
      });

      const result = await serverAPI.nextMessageMatching(
        'comparisonContextLines',
        msg =>
          msg.path === id.path &&
          // Older servers don't say which lines they sent.
          (msg.start == null || (msg.start === start && msg.numLines === numLines)),
      );

      return result.lines;
    },
    // We must ensure the lineRange gets invalidated when the underlying file's context lines
    // have changed.
    // This depends on the comparison:
    // for Committed: the commit hash is included in the Comparison, thus the cached data will always be accurate.
    // for Uncommitted, Head, and Stack:
    // by referencing the latest head commit's hash, we ensure this selector reloads when the head commit changes.
    // These comparisons are all against the working copy (not exactly head),
    // but there's no change that could be made that would affect the context lines without
    // also changing the head commit's hash.
    // Note: we use latestHeadCommit WITHOUT previews, so we don't accidentally cache the file content
    // AGAIN on the same data while waiting for some new operation to finish.
    comparisonInvalidationKey: latestHeadCommitHash,
    t,
    collapsed,
    setCollapsed,
    display: displayMode,
  };
  return (
    <div className="comparison-view-file" key={path}>
      <ErrorBoundary>
        <SplitDiffView ctx={context} patch={diff} path={path} generatedStatus={generatedStatus} />
      </ErrorBoundary>
    </div>
  );
}
