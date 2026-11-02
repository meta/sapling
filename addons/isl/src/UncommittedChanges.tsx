/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import type {Comparison} from 'shared/Comparison';
import type {CommitMessageFields} from './CommitInfoView/types';
import type {UseUncommittedSelection} from './partialSelection';
import type {
  ChangedFile,
  ChangedFileMode,
  ChangedFileStatus,
  MergeConflicts,
  RepoRelativePath,
} from './types';

import {Badge} from 'isl-components/Badge';
import {Banner, BannerKind} from 'isl-components/Banner';
import {Button} from 'isl-components/Button';
import {ErrorNotice} from 'isl-components/ErrorNotice';
import {HorizontallyGrowingTextField} from 'isl-components/HorizontallyGrowingTextField';
import {Icon} from 'isl-components/Icon';
import {DOCUMENTATION_DELAY, Tooltip} from 'isl-components/Tooltip';
import {useAtom, useAtomValue} from 'jotai';
import React, {useCallback, useEffect, useMemo, useRef, useState} from 'react';
import {ComparisonType} from 'shared/Comparison';
import {useDeepMemo} from 'shared/hooks';
import {group, partition} from 'shared/utils';
import {Avatar} from './Avatar';
import {File} from './ChangedFile';
import {
  ChangedFileDisplayTypePicker,
  type ChangedFilesDisplayType,
  changedFilesDisplayType,
} from './ChangedFileDisplayTypePicker';
import {Collapsable} from './Collapsable';
import {Commit} from './Commit';
import {
  commitMessageTemplate,
  commitMode,
  editedCommitMessages,
  forceNextCommitToEditAllFields,
} from './CommitInfoView/CommitInfoState';
import {
  commitMessageFieldsSchema,
  commitMessageFieldsToString,
} from './CommitInfoView/CommitMessageFields';
import {PendingDiffStats} from './CommitInfoView/DiffStats';
import {temporaryCommitTitle} from './CommitTitle';
import {OpenComparisonViewButton} from './ComparisonView/OpenComparisonViewButton';
import {Row} from './ComponentUtils';
import {FileTree, FileTreeFolderHeader} from './FileTree';
import {useGeneratedFileStatuses} from './GeneratedFile';
import {Internal} from './Internal';
import {AbsorbButton} from './StackActions';
import {confirmSuggestedEditsForFiles} from './SuggestedEdits';
import css from './UncommittedChanges.module.css';
import {processChangedFiles} from './UncommittedChangesUtils';
import {UnsavedFilesCount, confirmUnsavedFiles} from './UnsavedFiles';
import {tracker} from './analytics';
import {latestCommitMessageFields} from './codeReview/CodeReviewInfo';
import {expandCommitInfoView} from './drawerState';
import {externalMergeToolAtom} from './externalMergeTool';
import {T, t} from './i18n';
import {DownwardArrow} from './icons/DownwardIcon';
import {localStorageBackedAtom, readAtom, useAtomGet, writeAtom} from './jotaiUtils';
import {
  AutoResolveSettingCheckbox,
  PartialAbortSettingCheckbox,
  shouldAutoResolveAllBeforeContinue,
  shouldPartialAbort,
} from './mergeConflicts/state';
import {AbortMergeOperation} from './operations/AbortMergeOperation';
import {AddRemoveOperation} from './operations/AddRemoveOperation';
import {getAmendOperation} from './operations/AmendOperation';
import {getCommitOperation} from './operations/CommitOperation';
import {ContinueOperation} from './operations/ContinueMergeOperation';
import {DiscardOperation, PartialDiscardOperation} from './operations/DiscardOperation';
import {PurgeOperation} from './operations/PurgeOperation';
import {ResolveInExternalMergeToolOperation} from './operations/ResolveInExternalMergeToolOperation';
import {RevertOperation} from './operations/RevertOperation';
import {RunMergeDriversOperation} from './operations/RunMergeDriversOperation';
import {getShelveOperation} from './operations/ShelveOperation';
import {operationList, useRunOperation} from './operationsState';
import {useUncommittedSelection} from './partialSelection';
import platform from './platform';
import {
  CommitPreview,
  dagWithPreviews,
  optimisticMergeConflicts,
  uncommittedChangesWithPreviews,
  useIsOperationRunningOrQueued,
} from './previews';
import {repoRootAtom} from './repositoryData';
import {selectedCommits} from './selection';
import {
  latestHeadCommit,
  submodulePathsByRoot,
  uncommittedChangesFetchError,
} from './serverAPIState';
import {SmartActionsDropdown} from './smartActions/SmartActionsDropdown';
import {GeneratedStatus} from './types';
import {showConfirmation, showInfoModal} from './useModal';

import './UncommittedChanges.css';

export type UIChangedFile = {
  path: RepoRelativePath;
  // Compute file mode here rather than in the isl-server,
  // as the info is not directly available from the `status` command
  mode: ChangedFileMode;
  // disambiguated path, or rename with arrow
  label: string;
  status: ChangedFileStatus;
  visualStatus: VisualChangedFileStatus;
  copiedFrom?: RepoRelativePath;
  renamedFrom?: RepoRelativePath;
  tooltip: string;
};

export type VisualChangedFileStatus = ChangedFileStatus | 'Renamed' | 'Copied';

type SectionProps = Omit<React.ComponentProps<typeof LinearFileList>, 'files'> & {
  filesByPrefix: Map<string, Array<UIChangedFile>>;
};

function SectionedFileList({filesByPrefix, ...rest}: SectionProps) {
  const [collapsedSections, setCollapsedSections] = useState(new Set<string>());
  return (
    <div className="file-tree">
      {Array.from(filesByPrefix.entries(), ([prefix, files]) => {
        const isCollapsed = collapsedSections.has(prefix);
        const isEverythingSelected = files.every(file =>
          rest.selection?.isFullySelected(file.path),
        );
        const isPartiallySelected = files.some(file =>
          rest.selection?.isFullyOrPartiallySelected(file.path),
        );
        return (
          <div className="file-tree-section" key={prefix}>
            <FileTreeFolderHeader
              checkedState={
                isEverythingSelected ? true : isPartiallySelected ? 'indeterminate' : false
              }
              toggleChecked={checked => {
                if (checked) {
                  rest.selection?.select(...files.map(file => file.path));
                } else {
                  rest.selection?.deselect(...files.map(file => file.path));
                }
              }}
              isCollapsed={isCollapsed}
              toggleCollapsed={() =>
                setCollapsedSections(previous =>
                  previous.has(prefix)
                    ? new Set(Array.from(previous).filter(e => e !== prefix))
                    : new Set(Array.from(previous).concat(prefix)),
                )
              }
              folder={prefix}
            />
            {!isCollapsed ? (
              <div className="file-tree-level">
                <LinearFileList {...rest} files={files} />
              </div>
            ) : null}
          </div>
        );
      })}
    </div>
  );
}

/**
 * Display a list of changed files.
 *
 * (Case 1) If filesSubset is too long, but filesSubset.length === totalFiles, pagination buttons
 * are shown. This happens for uncommitted changes, where we have the entire list of files.
 *
 * (Case 2) If filesSubset.length < totalFiles, no pagination buttons are shown.
 * It's expected that filesSubset is already truncated to fit.
 * This happens initially for committed lists of changes, where we don't have the entire list of files.
 * Note that we later fetch the remaining files, to end up in (Case 1) again.
 *
 * In either case, a banner is shown to warn that not all files are shown.
 */
export function ChangedFiles(props: {
  filesSubset: ReadonlyArray<ChangedFile>;
  totalFiles: number;
  comparison: Comparison;
  selection?: UseUncommittedSelection;
  place?: Place;
}) {
  const displayType = useAtomValue(changedFilesDisplayType);
  const {filesSubset, totalFiles, ...rest} = props;
  const PAGE_SIZE = 500;
  const PAGE_FETCH_COUNT = 2;
  const [pageNum, setPageNum] = useState(0);
  const isLastPage = pageNum >= Math.floor((totalFiles - 1) / PAGE_SIZE);
  const rangeStart = pageNum * PAGE_SIZE;
  const rangeEnd = Math.min(totalFiles, (pageNum + 1) * PAGE_SIZE);
  const hasAdditionalPages = filesSubset.length > PAGE_SIZE;

  // We paginate files, but also paginate our fetches for generated statuses
  // at a larger granularity. This allows all manual files within that window
  // to be sorted to the front. This wider pagination is neceessary so we can control
  // how many files we query for generated statuses.
  const fetchPage = Math.floor(pageNum - (pageNum % PAGE_FETCH_COUNT));
  const fetchRangeStart = fetchPage * PAGE_SIZE;
  const fetchRangeEnd = (fetchPage + PAGE_FETCH_COUNT) * PAGE_SIZE;
  const filesToQueryGeneratedStatus = useMemo(
    () => filesSubset.slice(fetchRangeStart, fetchRangeEnd).map(f => f.path),
    [filesSubset, fetchRangeStart, fetchRangeEnd],
  );

  const filesMissingDueToFetchLimit =
    filesToQueryGeneratedStatus.length === 0 && totalFiles > 0 && filesSubset.length > 0;

  const generatedStatuses = useGeneratedFileStatuses(filesToQueryGeneratedStatus);
  const filesToSort = filesSubset.slice(fetchRangeStart, fetchRangeEnd);
  filesToSort.sort((a, b) => {
    const genStatA = generatedStatuses[a.path] ?? 0;
    const genStatB = generatedStatuses[b.path] ?? 0;
    return genStatA - genStatB;
  });
  const filesToShow = filesToSort.slice(rangeStart - fetchRangeStart, rangeEnd - fetchRangeStart);
  const root = useAtomValue(repoRootAtom);
  const currSubmodulePaths = useAtomValue(submodulePathsByRoot(root));
  const processedFiles = useDeepMemo(
    () => processChangedFiles(filesToShow, currSubmodulePaths),
    [filesToShow, currSubmodulePaths],
  );

  const prefixes: {key: string; prefix: string}[] = useMemo(
    () => Internal.repoPrefixes ?? [{key: 'default', prefix: ''}],
    [],
  );
  const firstNonDefaultPrefix = prefixes.find(
    p => p.prefix.length > 0 && filesToSort.some(f => f.path.indexOf(p.prefix) === 0),
  );
  const shouldShowRepoHeaders =
    prefixes.length > 1 &&
    firstNonDefaultPrefix != null &&
    filesToSort.find(f => f.path.indexOf(firstNonDefaultPrefix?.prefix) === -1) != null;

  const filesByPrefix = new Map<string, Array<UIChangedFile>>();
  for (const file of processedFiles) {
    for (const {key, prefix} of prefixes) {
      if (file.path.indexOf(prefix) === 0) {
        if (!filesByPrefix.has(key)) {
          filesByPrefix.set(key, []);
        }
        filesByPrefix.get(key)?.push(file);
        break;
      }
    }
  }

  useEffect(() => {
    // If the list of files is updated to have fewer files, we need to reset
    // the pageNum state to be in the proper range again.
    const lastPageIndex = Math.floor((totalFiles - 1) / PAGE_SIZE);
    if (pageNum > lastPageIndex) {
      setPageNum(Math.max(0, lastPageIndex));
    }
  }, [totalFiles, pageNum]);

  return (
    <div className="changed-files" data-testid="changed-files">
      {totalFiles > filesToShow.length ? (
        <Banner
          key={'alert'}
          icon={<Icon icon="info" />}
          buttons={
            hasAdditionalPages ? (
              <div className="changed-files-pages-buttons">
                <Tooltip title={t('See previous page of files')}>
                  <Button
                    data-testid="changed-files-previous-page"
                    icon
                    disabled={pageNum === 0}
                    onClick={() => {
                      setPageNum(old => old - 1);
                    }}>
                    <Icon icon="arrow-left" />
                  </Button>
                </Tooltip>
                <Tooltip title={t('See next page of files')}>
                  <Button
                    data-testid="changed-files-next-page"
                    icon
                    disabled={isLastPage}
                    onClick={() => {
                      setPageNum(old => old + 1);
                    }}>
                    <Icon icon="arrow-right" />
                  </Button>
                </Tooltip>
              </div>
            ) : null
          }>
          {pageNum === 0 ? (
            <T replace={{$numShown: filesToShow.length, $total: totalFiles}}>
              Showing first $numShown files out of $total total
            </T>
          ) : (
            <T replace={{$rangeStart: rangeStart + 1, $rangeEnd: rangeEnd, $total: totalFiles}}>
              Showing files $rangeStart – $rangeEnd out of $total total
            </T>
          )}
        </Banner>
      ) : null}
      {filesMissingDueToFetchLimit ? (
        <Banner
          key="not-everything-fetched"
          icon={<Icon icon="warning" />}
          kind={BannerKind.warning}>
          <T replace={{$maxFiles: PAGE_SIZE * PAGE_FETCH_COUNT}}>
            There are more than $maxFiles files, not all files have been fetched
          </T>
        </Banner>
      ) : totalFiles > PAGE_SIZE * PAGE_FETCH_COUNT ? (
        <Banner key="too-many-files" icon={<Icon icon="warning" />} kind={BannerKind.warning}>
          <T replace={{$maxFiles: PAGE_SIZE * PAGE_FETCH_COUNT}}>
            There are more than $maxFiles files, some files may appear out of order
          </T>
        </Banner>
      ) : null}
      {displayType === 'tree' ? (
        <FileTree {...rest} files={processedFiles} displayType={displayType} />
      ) : shouldShowRepoHeaders ? (
        <SectionedFileList
          {...rest}
          filesByPrefix={filesByPrefix}
          displayType={displayType}
          generatedStatuses={generatedStatuses}
        />
      ) : (
        <LinearFileList
          {...rest}
          files={processedFiles}
          displayType={displayType}
          generatedStatuses={generatedStatuses}
        />
      )}
    </div>
  );
}

const generatedFilesInitiallyExpanded = localStorageBackedAtom<boolean>(
  'isl.expand-generated-files',
  false,
  {portable: true},
);

export const __TEST__ = {
  generatedFilesInitiallyExpanded,
};

function LinearFileList(props: {
  files: Array<UIChangedFile>;
  displayType: ChangedFilesDisplayType;
  generatedStatuses: Record<RepoRelativePath, GeneratedStatus>;
  comparison: Comparison;
  selection?: UseUncommittedSelection;
  place?: Place;
}) {
  const {files, generatedStatuses, ...rest} = props;

  const groupedByGenerated = group(files, file => generatedStatuses[file.path]);
  const [initiallyExpanded, setInitiallyExpanded] = useAtom(generatedFilesInitiallyExpanded);

  function GeneratedFilesCollapsableSection(status: GeneratedStatus) {
    const group = groupedByGenerated[status] ?? [];
    if (group.length === 0) {
      return null;
    }
    return (
      <Collapsable
        title={
          <T
            replace={{
              $count: <Badge>{group.length}</Badge>,
            }}>
            {status === GeneratedStatus.PartiallyGenerated
              ? 'Partially Generated Files $count'
              : 'Generated Files $count'}
          </T>
        }
        startExpanded={status === GeneratedStatus.PartiallyGenerated || initiallyExpanded}
        onToggle={expanded => setInitiallyExpanded(expanded)}>
        {group.map(file => (
          <File key={file.path} {...rest} file={file} generatedStatus={status} />
        ))}
      </Collapsable>
    );
  }

  return (
    <div className="changed-files-list-container">
      <div className="changed-files-list">
        {groupedByGenerated[GeneratedStatus.Manual]?.map(file => (
          <File
            key={file.path}
            {...rest}
            file={file}
            generatedStatus={generatedStatuses[file.path] ?? GeneratedStatus.Manual}
          />
        ))}
        {GeneratedFilesCollapsableSection(GeneratedStatus.PartiallyGenerated)}
        {GeneratedFilesCollapsableSection(GeneratedStatus.Generated)}
      </div>
    </div>
  );
}

export type Place = 'main' | 'amend sidebar' | 'commit sidebar';

export function UncommittedChanges({place}: {place: Place}) {
  const uncommittedChanges = useAtomValue(uncommittedChangesWithPreviews);
  const error = useAtomValue(uncommittedChangesFetchError);
  // TODO: use dagWithPreviews instead, and update CommitOperation
  const headCommit = useAtomValue(latestHeadCommit);
  const schema = useAtomValue(commitMessageFieldsSchema);
  const template = useAtomValue(commitMessageTemplate);

  const conflicts = useAtomValue(optimisticMergeConflicts);

  const selection = useUncommittedSelection();
  const commitTitleRef = useRef<HTMLInputElement>(null);

  const runOperation = useRunOperation();

  const openCommitForm = useCallback(
    (which: 'commit' | 'amend') => {
      // make sure view is expanded
      expandCommitInfoView();

      // show head commit & set to correct mode
      writeAtom(selectedCommits, new Set());
      writeAtom(commitMode, which);

      // Start editing fields when amending so you can go right into typing.
      if (which === 'amend') {
        writeAtom(forceNextCommitToEditAllFields, true);
        if (headCommit != null) {
          const latestMessage = readAtom(latestCommitMessageFields(headCommit.hash));
          if (latestMessage) {
            writeAtom(editedCommitMessages(headCommit.hash), {
              ...latestMessage,
            });
          }
        }
      }

      const quickCommitTyped = commitTitleRef.current?.value;
      if (which === 'commit' && quickCommitTyped != null && quickCommitTyped != '') {
        writeAtom(editedCommitMessages('head'), value => ({
          ...value,
          Title: quickCommitTyped,
        }));
        // delete what was written in the quick commit form
        commitTitleRef.current != null && (commitTitleRef.current.value = '');
      }
    },
    [headCommit],
  );

  const onConfirmQuickCommit = async () => {
    const shouldContinue = await confirmUnsavedFiles();
    if (!shouldContinue) {
      return;
    }

    if (!(await confirmSuggestedEditsForFiles('quick-commit', 'accept', selection.selection))) {
      return;
    }

    const titleEl = commitTitleRef.current;
    const title = titleEl?.value || template?.Title || temporaryCommitTitle();
    // use the template, unless a specific quick title is given
    const fields: CommitMessageFields = {...template, Title: title};
    const message = commitMessageFieldsToString(schema, fields);
    const allFiles = uncommittedChanges.map(file => file.path);
    const operation = getCommitOperation(message, headCommit, selection.selection, allFiles);
    selection.discardPartialSelections();
    runOperation(operation);
    if (titleEl) {
      // clear out message now that we've used it
      titleEl.value = '';
    }
  };

  if (error) {
    return <ErrorNotice title={t('Failed to fetch Uncommitted Changes')} error={error} />;
  }
  if (uncommittedChanges.length === 0 && conflicts == null) {
    return null;
  }
  const allFilesSelected = selection.isEverythingSelected();
  const noFilesSelected = selection.isNothingSelected();
  const hasChunkSelection = selection.hasChunkSelection();

  const allConflictsResolved =
    conflicts?.files?.every(conflict => conflict.status === 'Resolved') ?? false;

  // only show addremove button if some files are untracked/missing
  const UNTRACKED_OR_MISSING = ['?', '!'];
  const addremoveButton = uncommittedChanges.some(file =>
    UNTRACKED_OR_MISSING.includes(file.status),
  ) ? (
    <Tooltip
      delayMs={DOCUMENTATION_DELAY}
      title={t('Add all untracked files and remove all missing files.')}>
      <Button
        icon
        key="addremove"
        data-testid="addremove-button"
        onClick={() => {
          // If all files are selected, no need to pass specific files to addremove.
          const filesToAddRemove = allFilesSelected
            ? []
            : uncommittedChanges
                .filter(file => UNTRACKED_OR_MISSING.includes(file.status))
                .filter(file => selection.isFullyOrPartiallySelected(file.path))
                .map(file => file.path);
          runOperation(new AddRemoveOperation(filesToAddRemove));
        }}>
        <Icon slot="start" icon="expand-all" />
        <T>Add/Remove</T>
      </Button>
    </Tooltip>
  ) : null;

  const onShelve = async () => {
    if (!(await confirmSuggestedEditsForFiles('shelve', 'accept', selection.selection))) {
      return;
    }

    const title = commitTitleRef.current?.value || undefined;
    const allFiles = uncommittedChanges.map(file => file.path);
    const operation = getShelveOperation(title, selection.selection, allFiles);
    runOperation(operation);
  };

  const canAmend = headCommit && headCommit.phase !== 'public' && headCommit.successorInfo == null;

  return (
    <div className="uncommitted-changes">
      {conflicts != null ? (
        <div className="conflicts-header">
          <strong>
            {allConflictsResolved ? (
              <T>All Merge Conflicts Resolved</T>
            ) : (
              <T>Unresolved Merge Conflicts</T>
            )}
          </strong>
          {conflicts.state === 'loading' ? (
            <div data-testid="merge-conflicts-spinner">
              <Icon icon="loading" />
            </div>
          ) : null}
          {allConflictsResolved ? null : (
            <T replace={{$cmd: conflicts.command}}>Resolve conflicts to continue $cmd</T>
          )}
        </div>
      ) : null}
      <div className="button-row">
        {conflicts != null ? (
          <MergeConflictButtons allConflictsResolved={allConflictsResolved} conflicts={conflicts} />
        ) : (
          <>
            <ChangedFileDisplayTypePicker />
            <OpenComparisonViewButton
              comparison={{
                type:
                  place === 'amend sidebar'
                    ? ComparisonType.HeadChanges
                    : ComparisonType.UncommittedChanges,
              }}
            />
            <Button
              icon
              key="select-all"
              disabled={allFilesSelected}
              onClick={() => {
                selection.selectAll();
              }}>
              <Icon slot="start" icon="check-all" />
              <T>Select All</T>
            </Button>
            <Button
              icon
              key="deselect-all"
              data-testid="deselect-all-button"
              disabled={noFilesSelected}
              onClick={() => {
                selection.deselectAll();
              }}>
              <Icon slot="start" icon="close-all" />
              <T>Deselect All</T>
            </Button>
            {addremoveButton}
            <Tooltip
              delayMs={DOCUMENTATION_DELAY}
              title={t(
                'Discard selected uncommitted changes, including untracked files.\n\nNote: Changes will be irreversibly lost.',
              )}>
              <Button
                icon
                disabled={noFilesSelected}
                data-testid={'discard-all-selected-button'}
                onClick={async () => {
                  if (
                    !(await confirmSuggestedEditsForFiles('discard', 'reject', selection.selection))
                  ) {
                    return;
                  }
                  if (
                    !(await showConfirmation({
                      title: t('Discard Changes?'),
                      message: t('confirmDiscardChanges'),
                      confirmLabel: t('Discard'),
                    }))
                  ) {
                    return;
                  }
                  if (allFilesSelected) {
                    // all changes selected -> use clean goto rather than reverting each file. This is generally faster.

                    // to "discard", we need to both remove uncommitted changes
                    runOperation(new DiscardOperation());
                    // ...and delete untracked files.
                    // Technically we only need to do the purge when we have untracked files, though there's a chance there's files we don't know about yet while status is running.
                    runOperation(new PurgeOperation());
                  } else if (selection.hasChunkSelection()) {
                    // TODO(quark): Make PartialDiscardOperation replace the above and below cases.
                    const allFiles = uncommittedChanges.map(file => file.path);
                    const operation = new PartialDiscardOperation(selection.selection, allFiles);
                    selection.discardPartialSelections();
                    runOperation(operation);
                  } else {
                    const selectedFiles = uncommittedChanges.filter(file =>
                      selection.isFullyOrPartiallySelected(file.path),
                    );
                    const [selectedTrackedFiles, selectedUntrackedFiles] = partition(
                      selectedFiles,
                      file => file.status !== '?', // only untracked, not missing
                    );
                    // Added files should be first reverted, then purged, so they are not tracked and also deleted.
                    // This way, the partial selection discard matches the non-partial discard.
                    const addedFilesToAlsoPurge = selectedFiles.filter(file => file.status === 'A');
                    if (selectedTrackedFiles.length > 0) {
                      // only a subset of files selected -> we need to revert selected tracked files individually
                      runOperation(new RevertOperation(selectedTrackedFiles.map(f => f.path)));
                    }
                    if (selectedUntrackedFiles.length > 0 || addedFilesToAlsoPurge.length > 0) {
                      // untracked files must be purged separately to delete from disk.
                      runOperation(
                        new PurgeOperation(
                          [...selectedUntrackedFiles, ...addedFilesToAlsoPurge].map(f => f.path),
                        ),
                      );
                    }
                  }
                }}>
                <Icon slot="start" icon="trashcan" />
                <T>Discard</T>
              </Button>
            </Tooltip>
            <AbsorbButton />
            <SmartActionsDropdown key="smartActions" />
          </>
        )}
      </div>
      {conflicts != null ? (
        <ChangedFiles
          filesSubset={conflicts.files ?? []}
          totalFiles={conflicts.files?.length ?? 0}
          place={place}
          comparison={{
            type: ComparisonType.UncommittedChanges,
          }}
        />
      ) : (
        <ChangedFiles
          filesSubset={uncommittedChanges}
          totalFiles={uncommittedChanges.length}
          place={place}
          selection={selection}
          comparison={{
            type: ComparisonType.UncommittedChanges,
          }}
        />
      )}
      <UnsavedFilesCount />
      {conflicts != null || place !== 'main' ? null : (
        <div className="button-rows">
          <div className="button-row">
            <PendingDiffStats />
          </div>
          <div className="button-row">
            <span className="quick-commit-inputs">
              <Button
                icon
                disabled={noFilesSelected}
                data-testid="quick-commit-button"
                onClick={onConfirmQuickCommit}>
                <Icon slot="start" icon="plus" />
                <T>Commit</T>
              </Button>
              <HorizontallyGrowingTextField
                data-testid="quick-commit-title"
                placeholder="Title"
                ref={commitTitleRef}
                onKeyPress={e => {
                  if (e.key === 'Enter' && !(e.ctrlKey || e.metaKey || e.altKey || e.shiftKey)) {
                    onConfirmQuickCommit();
                  }
                }}
              />
            </span>
            <Button
              icon
              className="show-on-hover"
              onClick={() => {
                openCommitForm('commit');
              }}>
              <Icon slot="start" icon="edit" />
              <T>Commit as...</T>
            </Button>
            <Tooltip
              title={t(
                'Save selected uncommitted changes for later unshelving. Removes these changes from the working copy.',
              )}>
              <Button
                disabled={noFilesSelected || hasChunkSelection}
                icon
                className="show-on-hover"
                onClick={onShelve}>
                <Icon slot="start" icon="archive" />
                <T>Shelve</T>
              </Button>
            </Tooltip>
          </div>
          {canAmend && (
            <div className="button-row">
              <Button
                icon
                disabled={noFilesSelected || !headCommit}
                data-testid="uncommitted-changes-quick-amend-button"
                onClick={async () => {
                  const shouldContinue = await confirmUnsavedFiles();
                  if (!shouldContinue) {
                    return;
                  }

                  if (
                    !(await confirmSuggestedEditsForFiles(
                      'quick-amend',
                      'accept',
                      selection.selection,
                    ))
                  ) {
                    return;
                  }

                  const allFiles = uncommittedChanges.map(file => file.path);
                  const operation = getAmendOperation(
                    undefined,
                    headCommit,
                    selection.selection,
                    allFiles,
                  );
                  selection.discardPartialSelections();
                  runOperation(operation);
                }}>
                <Icon slot="start" icon="debug-step-into" />
                <T>Amend</T>
              </Button>
              <Button
                icon
                className="show-on-hover"
                onClick={() => {
                  openCommitForm('amend');
                }}>
                <Icon slot="start" icon="edit" />
                <T>Amend as...</T>
              </Button>
            </div>
          )}
        </div>
      )}
      {place === 'main' && <ConflictingIncomingCommit />}
    </div>
  );
}

function ConflictingIncomingCommit() {
  const conflicts = useAtomValue(optimisticMergeConflicts);
  // "other" is the incoming / source / your commit
  const commit = useAtomGet(dagWithPreviews, conflicts?.hashes?.other);
  if (commit == null) {
    return null;
  }
  return (
    <Row className={css.conflictingIncomingContainer}>
      <DownwardArrow className={css.downwardArrow} />
      <Avatar username={commit.author} />
      <Commit
        commit={commit}
        hasChildren={false}
        previewType={CommitPreview.NON_ACTIONABLE_COMMIT}
      />
    </Row>
  );
}

function MergeConflictButtons({
  conflicts,
  allConflictsResolved,
}: {
  conflicts: MergeConflicts;
  allConflictsResolved: boolean;
}) {
  const runOperation = useRunOperation();
  // usually we only care if the operation is queued or actively running,
  // but since we don't use optimistic state for continue/abort,
  // we also need to consider recently run commands to disable the buttons.
  // But only if the abort/continue command succeeded.
  // TODO: is this reliable? Is it possible to get stuck with buttons disabled because
  // we think it's still running?
  const lastRunOperation = useAtomValue(operationList).currentOperation;
  const justFinishedContinue =
    lastRunOperation?.operation instanceof ContinueOperation && lastRunOperation.exitCode === 0;
  const justFinishedAbort =
    lastRunOperation?.operation instanceof AbortMergeOperation && lastRunOperation.exitCode === 0;
  const isRunningContinue = !!useIsOperationRunningOrQueued(ContinueOperation);
  const isRunningAbort = !!useIsOperationRunningOrQueued(AbortMergeOperation);
  const isRunningResolveExternal = !!useIsOperationRunningOrQueued(
    ResolveInExternalMergeToolOperation,
  );
  const shouldDisableButtons =
    isRunningContinue ||
    isRunningAbort ||
    isRunningResolveExternal ||
    justFinishedContinue ||
    justFinishedAbort;

  const externalMergeTool = useAtomValue(externalMergeToolAtom);

  const branchMerge = Internal.getSubtreeContinueOperation?.(dagWithPreviews, conflicts);

  return (
    <Row style={{flexWrap: 'wrap', marginBottom: 'var(--pad)'}}>
      <Button
        primary
        key="continue"
        disabled={!allConflictsResolved || shouldDisableButtons}
        data-testid="conflict-continue-button"
        onClick={async () => {
          const conflictFiles =
            conflicts.state === 'loaded' ? conflicts.files.map(f => f.path) : [];
          if (!(await confirmSuggestedEditsForFiles('merge-continue', 'accept', conflictFiles))) {
            return;
          }

          if (readAtom(shouldAutoResolveAllBeforeContinue)) {
            runOperation(new RunMergeDriversOperation());
          }
          if (branchMerge) {
            runOperation(branchMerge);
          } else {
            runOperation(new ContinueOperation());
          }
        }}>
        <Icon slot="start" icon={isRunningContinue ? 'loading' : 'debug-continue'} />
        <T>Continue</T>
      </Button>
      <Button
        key="abort"
        disabled={shouldDisableButtons}
        onClick={() => {
          const partialAbortAvailable = conflicts?.command === 'rebase';
          const isPartialAbort = partialAbortAvailable && readAtom(shouldPartialAbort);
          runOperation(new AbortMergeOperation(conflicts, isPartialAbort));
        }}>
        <Icon slot="start" icon={isRunningAbort ? 'loading' : 'circle-slash'} />
        <T>Abort</T>
      </Button>
      <SmartActionsDropdown />
      {externalMergeTool == null ? (
        platform.upsellExternalMergeTool ? (
          <Tooltip
            title={
              <div>
                <T replace={{$tool: <code>{externalMergeTool}</code>, $br: <br />}}>
                  You can configure an external merge tool to use for resolving conflicts.$br
                </T>
              </div>
            }>
            <Button
              icon
              disabled={allConflictsResolved || shouldDisableButtons}
              onClick={() => {
                tracker.track('ClickedConfigureExternalMergeTool');
                const link = Internal.externalMergeToolDocsLink;
                if (link) {
                  platform.openExternalLink(link);
                  return;
                }
                void showInfoModal({
                  title: t('Configuring External Merge Tools'),
                  message: t(
                    'You can configure ISL to use an external merge tool for resolving conflicts.\n' +
                      'Set both `ui.merge = mymergetool` and `merge-tool.mymergetool`.\n' +
                      'See `sl help config.merge-tools` for more information about setting up merge tools.\n',
                  ),
                });
              }}>
              <Icon icon="gear" />
              <T>Configure External Merge Tool</T>
            </Button>
          </Tooltip>
        ) : null
      ) : (
        <Tooltip
          title={
            <div>
              <T replace={{$tool: <code>{externalMergeTool}</code>, $br: <br />}}>
                Open your configured external merge tool $tool to resolve all the conflicts.$br
                Waits for the merge tool to exit before continuing.
              </T>
              {allConflictsResolved ? (
                <>
                  <br />
                  <T>Disabled since all conflicts have been resolved.</T>
                </>
              ) : null}
            </div>
          }>
          <Button
            icon
            disabled={allConflictsResolved || shouldDisableButtons}
            onClick={() => {
              runOperation(new ResolveInExternalMergeToolOperation(externalMergeTool));
            }}>
            <Icon icon="link-external" />
            <T>Open External Merge Tool</T>
          </Button>
        </Tooltip>
      )}
      {Internal.showInlineAutoRunMergeDriversOption === true && (
        <AutoResolveSettingCheckbox subtle />
      )}
      {conflicts?.command === 'rebase' && <PartialAbortSettingCheckbox subtle />}
    </Row>
  );
}
