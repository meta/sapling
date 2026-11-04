/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import type {Operation} from '../operations/Operation';
import type {CommitInfo, DiffId} from '../types';
import type {CommitInfoMode, EditedMessage} from './CommitInfoState';
import type {CommitMessageFields, FieldConfig, FieldsBeingEdited} from './types';

import deepEqual from 'fast-deep-equal';
import {Badge} from 'isl-components/Badge';
import {Banner, BannerKind, BannerTooltip} from 'isl-components/Banner';
import {Button} from 'isl-components/Button';
import {Divider} from 'isl-components/Divider';
import {ErrorNotice} from 'isl-components/ErrorNotice';
import {Column} from 'isl-components/Flex';
import {Icon} from 'isl-components/Icon';
import {RadioGroup} from 'isl-components/Radio';
import {Subtle} from 'isl-components/Subtle';
import {Tooltip} from 'isl-components/Tooltip';
import {atom, useAtom, useAtomValue} from 'jotai';
import {useAtomCallback} from 'jotai/utils';
import {useCallback, useEffect, useLayoutEffect, useMemo, useRef} from 'react';
import {ComparisonType} from 'shared/Comparison';
import {useContextMenu} from 'shared/ContextMenu';
import {usePrevious} from 'shared/hooks';
import {firstLine, notEmpty, nullthrows} from 'shared/utils';
import {tracker} from '../analytics';
import {ChangedFilesWithFetching} from '../ChangedFilesWithFetching';
import serverAPI from '../ClientToServerAPI';
import {
  allDiffSummaries,
  codeReviewProvider,
  effectiveSchemaForCommit,
  latestCommitMessageFields,
} from '../codeReview/CodeReviewInfo';
import {submitAsDraft, SubmitAsDraftCheckbox} from '../codeReview/DraftCheckbox';
import {showBranchingPrModal} from '../codeReview/github/BranchingPrModal';
import {overrideDisabledSubmitModes} from '../codeReview/github/branchPrState';
import {publishWhenReady, PublishWhenReadyCheckbox} from '../codeReview/PublishWhenReadyCheckbox';
import {Commit} from '../Commit';
import {OpenComparisonViewButton} from '../ComparisonView/OpenComparisonViewButton';
import {Center} from '../ComponentUtils';
import {confirmNoBlockingDiagnostics} from '../Diagnostics';
import {FoldButton, useRunFoldPreview} from '../fold';
import {getCachedGeneratedFileStatuses, useGeneratedFileStatuses} from '../GeneratedFile';
import {t, T} from '../i18n';
import {IrrelevantCwdIcon} from '../icons/IrrelevantCwdIcon';
import {numPendingImageUploads} from '../ImageUpload';
import {readAtom, writeAtom} from '../jotaiUtils';
import {Link} from '../Link';
import {
  messageSyncingEnabledState,
  messageSyncingOverrideState,
  updateRemoteMessage,
} from '../messageSyncing';
import {OperationDisabledButton} from '../OperationDisabledButton';
import {AmendMessageOperation} from '../operations/AmendMessageOperation';
import {getAmendOperation} from '../operations/AmendOperation';
import {getCommitOperation} from '../operations/CommitOperation';
import {FOLD_COMMIT_PREVIEW_HASH_PREFIX} from '../operations/FoldOperation';
import {GhStackSubmitOperation} from '../operations/GhStackSubmitOperation';
import {PrSubmitOperation} from '../operations/PrSubmitOperation';
import {SetConfigOperation} from '../operations/SetConfigOperation';
import {useRunOperation} from '../operationsState';
import {useUncommittedSelection} from '../partialSelection';
import platform from '../platform';
import {CommitPreview, dagWithPreviews, uncommittedChangesWithPreviews} from '../previews';
import {repoRelativeCwd, useIsIrrelevantToCwd} from '../repositoryData';
import {selectedCommits, selectedCommitsRangeComparison} from '../selection';
import {authorString, commitByHash, latestHeadCommit, repositoryInfo} from '../serverAPIState';
import {SplitButton} from '../stackEdit/ui/SplitButton';
import {SubmitSelectionButton} from '../SubmitSelectionButton';
import {SubmitUpdateMessageInput} from '../SubmitUpdateMessageInput';
import {latestSuccessorUnlessExplicitlyObsolete} from '../successionUtils';
import {SuggestedRebaseButton} from '../SuggestedRebase';
import {showToast} from '../toast';
import {GeneratedStatus, succeedableRevset} from '../types';
import {UncommittedChanges} from '../UncommittedChanges';
import {confirmUnsavedFiles} from '../UnsavedFiles';
import {showConfirmation, useModal} from '../useModal';
import {firstOfIterable} from '../utils';
import {CommitInfoField} from './CommitInfoField';
import {
  commitInfoViewCurrentCommits,
  commitMode,
  diffUpdateMessagesState,
  editedCommitMessages,
  forceNextCommitToEditAllFields,
  hasUnsavedEditedCommitMessage,
  unsavedFieldsBeingEdited,
} from './CommitInfoState';
import {
  applyEditedFields,
  commitMessageFieldsToString,
  editedMessageSubset,
  findEditedDiffNumber,
  findFieldsBeingEdited,
  parseCommitMessageFields,
  removeNoopEdits,
} from './CommitMessageFields';
import {DiffStats, PendingDiffStats} from './DiffStats';
import {FillCommitMessage} from './FillCommitMessage';
import {CommitTitleByline, getFieldToAutofocus, Section, SmallCapsTitle} from './utils';

import {AICodeReviewUpsell} from '../firstPassCodeReview/AICodeReviewUpsell';
import {Internal} from '../Internal';
import {confirmSuggestedEditsForFiles} from '../SuggestedEdits';
import './CommitInfoView.css';

export function CommitInfoSidebar() {
  const commitsToShow = useAtomValue(commitInfoViewCurrentCommits);

  if (commitsToShow == null) {
    return (
      <div className="commit-info-view" data-testid="commit-info-view-loading">
        <Center>
          <Icon icon="loading" />
        </Center>
      </div>
    );
  } else {
    if (commitsToShow.length > 1) {
      return <MultiCommitInfo selectedCommits={commitsToShow} />;
    }

    // only one commit selected
    return <CommitInfoDetails commit={commitsToShow[0]} />;
  }
}

export function MultiCommitInfo({selectedCommits}: {selectedCommits: Array<CommitInfo>}) {
  const commitsWithDiffs = selectedCommits.filter(commit => commit.diffId != null);
  const commitRangeComparison = useAtomValue(selectedCommitsRangeComparison);

  return (
    <div className="commit-info-view-multi-commit" data-testid="commit-info-view">
      <strong className="commit-list-header">
        <Icon icon="layers" size="M" />
        <T replace={{$num: selectedCommits.length}}>$num Commits Selected</T>
      </strong>
      <Divider />
      <div className="commit-list">
        {selectedCommits.map(commit => (
          <Commit
            key={commit.hash}
            commit={commit}
            hasChildren={false}
            previewType={CommitPreview.NON_ACTIONABLE_COMMIT}
          />
        ))}
      </div>
      <div className="commit-info-actions-bar">
        <div className="commit-info-actions-bar-right">
          {commitRangeComparison != null && (
            <OpenComparisonViewButton comparison={commitRangeComparison} />
          )}
          <SuggestedRebaseButton
            sources={selectedCommits.map(commit => succeedableRevset(commit.hash))}
          />
          <FoldButton />
        </div>
        {commitsWithDiffs.length === 0 ? null : (
          <SubmitUpdateMessageInput commits={selectedCommits} />
        )}
        <div className="commit-info-actions-bar-left">
          <SubmitAsDraftCheckbox commitsToBeSubmit={selectedCommits} />
          <PublishWhenReadyCheckbox />
        </div>
        <div className="commit-info-actions-bar-right">
          <SubmitSelectionButton />
        </div>
      </div>
    </div>
  );
}

/**
 * CommitInfoDetails can be unmounted and remounted while ISL stays open (e.g. on
 * VS Code tab switch / window focus). That remount resets the scroll
 * position of the main content container. Persist the last scroll offset at module
 * scope so it survives remounts. We only remember the most recently viewed commit's
 * offset (tracked via `savedScrollKey`); selecting a different commit resets to top.
 */
let savedScrollTop = 0;
let savedScrollKey: string | undefined;

function useFetchActiveDiffDetails(diffId?: string) {
  useEffect(() => {
    if (diffId != null) {
      serverAPI.postMessage({
        type: 'fetchDiffSummaries',
        diffIds: [diffId],
        partial: true,
      });
      tracker.track('DiffFetchSource', {extras: {source: 'active_diff_details'}});
    }
  }, [diffId]);
}

export function CommitInfoDetails({commit}: {commit: CommitInfo}) {
  const [mode, setMode] = useAtom(commitMode);
  const isCommitMode = mode === 'commit';
  const hashOrHead = isCommitMode ? 'head' : commit.hash;
  const [editedMessage, setEditedCommitMessage] = useAtom(editedCommitMessages(hashOrHead));
  const uncommittedChanges = useAtomValue(uncommittedChangesWithPreviews);
  const selection = useUncommittedSelection();
  const schema = useAtomValue(effectiveSchemaForCommit(hashOrHead));

  const isFoldPreview = commit.hash.startsWith(FOLD_COMMIT_PREVIEW_HASH_PREFIX);
  const isOptimistic =
    useAtomValue(commitByHash(commit.hash)) == null && !isCommitMode && !isFoldPreview;

  const cwd = useAtomValue(repoRelativeCwd);
  const isIrrelevantToCwd = useIsIrrelevantToCwd(commit);

  const isPublic = commit.phase === 'public';
  const isObsoleted = commit.successorInfo != null;
  const isAmendDisabled = mode === 'amend' && (isPublic || isObsoleted);

  const fieldsBeingEdited = useAtomValue(unsavedFieldsBeingEdited(hashOrHead));
  const previousFieldsBeingEdited = usePrevious(fieldsBeingEdited, deepEqual);

  useFetchActiveDiffDetails(commit.diffId);

  const [forceEditAll, setForceEditAll] = useAtom(forceNextCommitToEditAllFields);

  useEffect(() => {
    if (isCommitMode && commit.isDot) {
      // no use resetting edited state for commit mode, where it's always being edited.
      return;
    }

    if (!forceEditAll) {
      // If the selected commit is changed, the fields being edited should slim down to only fields
      // that are meaningfully edited on the new commit.
      if (Object.keys(editedMessage).length > 0) {
        const trimmedEdits = removeNoopEdits(schema, parsedFields, editedMessage);
        if (Object.keys(trimmedEdits).length !== Object.keys(editedMessage).length) {
          setEditedCommitMessage(trimmedEdits);
        }
      }
    }
    setForceEditAll(false);

    // We only want to recompute this when the commit/mode changes.
    // we expect the edited message to change constantly.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [commit.hash, isCommitMode]);

  const parsedFields = useAtomValue(latestCommitMessageFields(hashOrHead));

  const provider = useAtomValue(codeReviewProvider);
  const startEditingField = (field: string) => {
    const original = parsedFields[field];
    // If you start editing a tokenized field, add a blank token so you can write a new token instead of
    // modifying the last existing token.
    const fieldValue = Array.isArray(original) && original.at(-1) ? [...original, ''] : original;

    setEditedCommitMessage(last => ({
      ...last,
      [field]: fieldValue,
    }));
  };

  const fieldToAutofocus = getFieldToAutofocus(
    schema,
    fieldsBeingEdited,
    previousFieldsBeingEdited,
  );

  const diffSummaries = useAtomValue(allDiffSummaries);
  const remoteTrackingBranch = provider?.getRemoteTrackingBranch(
    diffSummaries?.value,
    commit.diffId,
  );

  const selectedFiles = uncommittedChanges.filter(f =>
    selection.isFullyOrPartiallySelected(f.path),
  );
  const selectedFilesLength = selectedFiles.length;

  // Persist/restore the main content scroll position across remounts. A remount
  // for the same commit restores the offset; selecting a different commit resets
  // to the top.
  const mainContentRef = useRef<HTMLDivElement>(null);

  const handleMainContentScroll = useCallback(() => {
    const el = mainContentRef.current;
    if (el != null) {
      savedScrollTop = el.scrollTop;
      savedScrollKey = hashOrHead;
    }
  }, [hashOrHead]);

  useLayoutEffect(() => {
    const el = mainContentRef.current;
    if (el == null) {
      return;
    }
    if (savedScrollKey === hashOrHead) {
      // Same commit remounting (e.g. tab switch): restore the saved offset.
      el.scrollTop = savedScrollTop;
    } else {
      // Different commit selected: start at the top.
      savedScrollKey = hashOrHead;
      savedScrollTop = 0;
      el.scrollTop = 0;
    }
  }, [hashOrHead]);

  return (
    <div className="commit-info-view" data-testid="commit-info-view">
      {!commit.isDot ? null : (
        <div className="commit-info-view-toolbar-top" data-testid="commit-info-toolbar-top">
          <Tooltip
            title={t(
              'In Commit mode, you can edit the blank commit message for a new commit. \n\n' +
                'In Amend mode, you can view and edit the commit message for the current head commit.',
            )}>
            <RadioGroup
              horizontal
              choices={[
                {title: t('Commit'), value: 'commit'},
                {title: t('Amend'), value: 'amend'},
              ]}
              current={mode}
              onChange={setMode}
            />
          </Tooltip>
        </div>
      )}
      {isCommitMode && <FillCommitMessage commit={commit} mode={mode} />}
      <div
        className="commit-info-view-main-content"
        ref={mainContentRef}
        onScroll={handleMainContentScroll}
        // remount this if we change to commit mode
        key={mode}>
        {schema
          .filter(field => !isCommitMode || field.type !== 'read-only')
          .map(field => {
            let editedFieldValue = editedMessage?.[field.key];
            if (editedFieldValue == null && isCommitMode) {
              // If the field is supposed to edited but not in the editedMessage,
              // it means we're loading from a blank slate. This is when we can load from the commit template.
              editedFieldValue = parsedFields[field.key];
            }

            const setField = (newVal: string) =>
              setEditedCommitMessage(val => ({
                ...val,
                [field.key]: field.type === 'field' ? newVal.split(',') : newVal,
              }));

            return (
              <CommitInfoField
                key={field.key}
                field={field}
                content={parsedFields[field.key as keyof CommitMessageFields]}
                autofocus={fieldToAutofocus === field.key}
                readonly={isOptimistic || isAmendDisabled || isObsoleted}
                isBeingEdited={fieldsBeingEdited[field.key]}
                startEditingField={() => startEditingField(field.key)}
                editedField={editedFieldValue}
                setEditedField={setField}
                extra={
                  field.key === 'Title' ? (
                    <>
                      {Internal.aiCodeReview?.enabled && commit.isDot && !isPublic && (
                        <AICodeReviewUpsell
                          isCommitMode={isCommitMode}
                          hasUncommittedChanges={uncommittedChanges.length > 0}
                        />
                      )}
                      {!isCommitMode ? (
                        <>
                          <CommitTitleByline commit={commit} />
                          {isFoldPreview && <FoldPreviewBanner />}
                          <ShowingRemoteMessageBanner
                            commit={commit}
                            latestFields={parsedFields}
                            editedCommitMessageKey={isCommitMode ? 'head' : commit.hash}
                          />
                          {!isPublic && isIrrelevantToCwd ? (
                            <Tooltip
                              title={
                                <T
                                  replace={{
                                    $prefix: <pre>{commit.maxCommonPathPrefix}</pre>,
                                    $cwd: <pre>{cwd}</pre>,
                                  }}>
                                  This commit only contains files within: $prefix These are
                                  irrelevant to your current working directory: $cwd
                                </T>
                              }>
                              <Banner kind={BannerKind.default}>
                                <IrrelevantCwdIcon />
                                <div style={{paddingLeft: 'var(--halfpad)'}}>
                                  <T replace={{$cwd: <code>{cwd}</code>}}>
                                    All files in this commit are outside $cwd
                                  </T>
                                </div>
                              </Banner>
                            </Tooltip>
                          ) : null}
                        </>
                      ) : undefined}
                    </>
                  ) : undefined
                }
              />
            );
          })}
        {remoteTrackingBranch == null ? null : (
          <Section>
            <SmallCapsTitle>
              <Icon icon="source-control"></Icon>
              <T>Remote Tracking Branch</T>
            </SmallCapsTitle>
            <div className="commit-info-tokenized-field">
              <span className="token">{remoteTrackingBranch}</span>
            </div>
          </Section>
        )}
        <Divider />
        {commit.isDot && !isAmendDisabled ? (
          <Section data-testid="changes-to-amend">
            <SmallCapsTitle>
              {isCommitMode ? <T>Changes to Commit</T> : <T>Changes to Amend</T>}
              <Badge>
                {selectedFilesLength === uncommittedChanges.length
                  ? null
                  : selectedFilesLength + '/'}
                {uncommittedChanges.length}
              </Badge>
            </SmallCapsTitle>
            {uncommittedChanges.length > 0 ? <PendingDiffStats /> : null}
            {uncommittedChanges.length === 0 ? (
              <Subtle>
                {isCommitMode ? <T>No changes to commit</T> : <T>No changes to amend</T>}
              </Subtle>
            ) : (
              <UncommittedChanges place={isCommitMode ? 'commit sidebar' : 'amend sidebar'} />
            )}
          </Section>
        ) : null}
        {isCommitMode ? null : (
          <Section data-testid="committed-changes">
            <SmallCapsTitle>
              <T>Files Changed</T>
              <Badge>{commit.totalFileCount}</Badge>
            </SmallCapsTitle>
            {commit.phase !== 'public' ? <DiffStats commit={commit} /> : null}
            <div className="changed-file-list">
              <div className="button-row">
                <OpenComparisonViewButton
                  comparison={
                    commit.isDot && !isOptimistic && uncommittedChanges.length === 0
                      ? {type: ComparisonType.HeadChanges}
                      : {type: ComparisonType.Committed, hash: commit.hash}
                  }
                />
                <OpenAllFilesButton commit={commit} />
                <SplitButton trackerEventName="SplitOpenFromSplitSuggestion" commit={commit} />
              </div>
              <ChangedFilesWithFetching commit={commit} />
            </div>
          </Section>
        )}
      </div>
      {!isAmendDisabled && (
        <div className="commit-info-view-toolbar-bottom">
          {isFoldPreview ? (
            <FoldPreviewActions />
          ) : (
            <>
              <ActionsBar
                commit={commit}
                latestMessage={parsedFields}
                editedMessage={editedMessage}
                fieldsBeingEdited={fieldsBeingEdited}
                isCommitMode={isCommitMode}
                setMode={setMode}
              />
            </>
          )}
        </div>
      )}
    </div>
  );
}

/**
 * No files are generated -> "Open all" button
 * All files are generated -> "Open all" button, with warning that they're all generated
 * Some files are generated -> "Open non-generated files" button
 */
function OpenAllFilesButton({commit}: {commit: CommitInfo}) {
  const paths = useMemo(() => commit.filePathsSample, [commit]);
  const statuses = useGeneratedFileStatuses(paths);
  const allAreGenerated = paths.every(file => statuses[file] === GeneratedStatus.Generated);
  const someAreGenerated = paths.some(file => statuses[file] === GeneratedStatus.Generated);
  return (
    <Tooltip
      title={
        someAreGenerated
          ? allAreGenerated
            ? t('Opens all files for editing.\nNote: All files are generated.')
            : t('Open all non-generated files for editing')
          : t('Open all files for editing')
      }>
      <Button
        icon
        onClick={() => {
          tracker.track('OpenAllFiles');
          const statuses = getCachedGeneratedFileStatuses(commit.filePathsSample);
          const toOpen = allAreGenerated
            ? commit.filePathsSample
            : commit.filePathsSample.filter(
                file => statuses[file] == null || statuses[file] !== GeneratedStatus.Generated,
              );
          platform.openFiles(toOpen);
        }}>
        <Icon icon="go-to-file" slot="start" />
        {someAreGenerated && !allAreGenerated ? (
          <T>Open Non-Generated Files</T>
        ) : (
          <T>Open All Files</T>
        )}
      </Button>
    </Tooltip>
  );
}

/**
 * Two parsed commit messages are considered unchanged if all the textareas (summary, test plan) are unchanged.
 * This avoids marking tiny changes like adding a reviewer as substatively changing the message.
 */
function areTextFieldsUnchanged(
  schema: Array<FieldConfig>,
  a: CommitMessageFields,
  b: CommitMessageFields,
) {
  for (const field of schema) {
    if (field.type === 'textarea') {
      if (a[field.key] !== b[field.key]) {
        return false;
      }
    }
  }
  return true;
}

function FoldPreviewBanner() {
  return (
    <BannerTooltip
      tooltip={t(
        'This is the commit message after combining these commits with the fold command. ' +
          'You can edit this message before confirming and running fold.',
      )}>
      <Banner kind={BannerKind.green} icon={<Icon icon="info" />}>
        <T>Previewing result of combined commits</T>
      </Banner>
    </BannerTooltip>
  );
}

function ShowingRemoteMessageBanner({
  commit,
  latestFields,
  editedCommitMessageKey,
}: {
  commit: CommitInfo;
  latestFields: CommitMessageFields;
  editedCommitMessageKey: string;
}) {
  const provider = useAtomValue(codeReviewProvider);
  const schema = useAtomValue(effectiveSchemaForCommit(commit.hash));
  const runOperation = useRunOperation();
  const syncingEnabled = useAtomValue(messageSyncingEnabledState);
  const syncingOverride = useAtomValue(messageSyncingOverrideState);

  const loadLocalMessage = useCallback(() => {
    const originalFields = parseCommitMessageFields(schema, commit.title, commit.description);
    const beingEdited = findFieldsBeingEdited(schema, originalFields, latestFields);

    writeAtom(editedCommitMessages(editedCommitMessageKey), () =>
      editedMessageSubset(originalFields, beingEdited),
    );
  }, [commit, editedCommitMessageKey, latestFields, schema]);

  const contextMenu = useContextMenu(() => {
    return [
      {
        label: <T>Load local commit message instead</T>,
        onClick: loadLocalMessage,
      },
      {
        label: <T>Sync local commit to match remote</T>,
        onClick: () => {
          runOperation(
            new AmendMessageOperation(
              latestSuccessorUnlessExplicitlyObsolete(commit),
              commitMessageFieldsToString(schema, latestFields),
            ),
          );
        },
      },
    ];
  });

  if (!provider || (syncingOverride == null && !syncingEnabled)) {
    return null;
  }

  if (syncingOverride === false) {
    return (
      <BannerTooltip
        tooltip={t(
          'Message syncing with $provider has been temporarily disabled due to a failed sync.\n\n' +
            'Your local commit message is shown instead.\n' +
            "Changes you make won't be automatically synced.\n\n" +
            'Make sure to manually sync your message with $provider, then re-enable or restart ISL to start syncing again.',
          {replace: {$provider: provider.label}},
        )}>
        <Banner
          icon={<Icon icon="warn" />}
          alwaysShowButtons
          kind={BannerKind.warning}
          buttons={
            <Button
              icon
              onClick={() => {
                writeAtom(messageSyncingOverrideState, null);
              }}>
              <T>Show Remote Messages Instead</T>
            </Button>
          }>
          <T replace={{$provider: provider.label}}>Not syncing messages with $provider</T>
        </Banner>
      </BannerTooltip>
    );
  }

  const originalFields = parseCommitMessageFields(schema, commit.title, commit.description);

  if (areTextFieldsUnchanged(schema, originalFields, latestFields)) {
    return null;
  }

  return (
    <BannerTooltip
      tooltip={t(
        'Viewing the newer commit message from $provider. This message will be used when your code is landed. You can also load the local message instead.',
        {replace: {$provider: provider.label}},
      )}>
      <Banner
        icon={<Icon icon="info" />}
        alwaysShowButtons
        buttons={
          <Button
            icon
            data-testid="message-sync-banner-context-menu"
            onClick={e => {
              contextMenu(e);
            }}>
            <Icon icon="ellipsis" />
          </Button>
        }>
        <T replace={{$provider: provider.label}}>Showing latest commit message from $provider</T>
      </Banner>
    </BannerTooltip>
  );
}

function FoldPreviewActions() {
  const [cancel, run] = useRunFoldPreview();
  return (
    <div className="commit-info-actions-bar" data-testid="commit-info-actions-bar">
      <div className="commit-info-actions-bar-right">
        <Button onClick={cancel}>
          <T>Cancel</T>
        </Button>
        <Button primary onClick={run}>
          <T>Run Combine</T>
        </Button>
      </div>
    </div>
  );
}

const imageUploadsPendingAtom = atom(get => {
  return get(numPendingImageUploads(undefined)) > 0;
});

function ActionsBar({
  commit,
  latestMessage,
  editedMessage,
  fieldsBeingEdited,
  isCommitMode,
  setMode,
}: {
  commit: CommitInfo;
  latestMessage: CommitMessageFields;
  editedMessage: EditedMessage;
  fieldsBeingEdited: FieldsBeingEdited;
  isCommitMode: boolean;
  setMode: (mode: CommitInfoMode) => unknown;
}) {
  const isAnythingBeingEdited = Object.values(fieldsBeingEdited).some(Boolean);
  const uncommittedChanges = useAtomValue(uncommittedChangesWithPreviews);
  const selection = useUncommittedSelection();
  const anythingToCommit =
    !selection.isNothingSelected() &&
    ((!isCommitMode && isAnythingBeingEdited) || uncommittedChanges.length > 0);

  const provider = useAtomValue(codeReviewProvider);
  const schema = useAtomValue(effectiveSchemaForCommit(isCommitMode ? 'head' : commit.hash));
  const headCommit = useAtomValue(latestHeadCommit);

  const messageSyncEnabled = useAtomValue(messageSyncingEnabledState);

  // after committing/amending, if you've previously selected the head commit,
  // we should show you the newly amended/committed commit instead of the old one.
  const deselectIfHeadIsSelected = useAtomCallback((get, set) => {
    if (!commit.isDot) {
      return;
    }
    const selected = get(selectedCommits);
    // only reset if selection exactly matches our expectation
    if (selected && selected.size === 1 && firstOfIterable(selected.values()) === commit.hash) {
      set(selectedCommits, new Set());
    }
  });

  const clearEditedCommitMessage = useCallback(
    async (skipConfirmation?: boolean) => {
      if (!skipConfirmation) {
        const hasUnsavedEdits = readAtom(
          hasUnsavedEditedCommitMessage(isCommitMode ? 'head' : commit.hash),
        );
        if (hasUnsavedEdits) {
          const confirmed = await showConfirmation({
            title: t('Are you sure you want to discard your edited message?'),
            confirmLabel: t('Discard'),
          });
          if (!confirmed) {
            return;
          }
        }
      }

      // Delete the edited message atom (and delete from persisted storage)
      writeAtom(editedCommitMessages(isCommitMode ? 'head' : commit.hash), undefined);
    },
    [commit.hash, isCommitMode],
  );
  const doAmendOrCommit = () => {
    const updatedMessage = applyEditedFields(latestMessage, editedMessage);
    const message = commitMessageFieldsToString(schema, updatedMessage);
    const allFiles = uncommittedChanges.map(file => file.path);

    const operation = isCommitMode
      ? getCommitOperation(message, headCommit, selection.selection, allFiles)
      : getAmendOperation(message, headCommit, selection.selection, allFiles);

    selection.discardPartialSelections();

    clearEditedCommitMessage(/* skip confirmation */ true);
    // reset to amend mode now that the commit has been made
    setMode('amend');
    deselectIfHeadIsSelected();

    return operation;
  };

  const showOptionModal = useModal();

  const codeReviewProviderName = provider?.label;

  const areImageUploadsOngoing = useAtomValue(imageUploadsPendingAtom);

  // Generally "Amend"/"Commit" for head commit, but if there's no changes while amending, just use "Amend message"
  const showCommitOrAmend =
    commit.isDot && (isCommitMode || anythingToCommit || !isAnythingBeingEdited);

  return (
    <div className="commit-info-actions-bar" data-testid="commit-info-actions-bar">
      {isCommitMode || commit.diffId == null ? null : (
        <SubmitUpdateMessageInput commits={[commit]} />
      )}
      <div className="commit-info-actions-bar-left">
        <SubmitAsDraftCheckbox commitsToBeSubmit={isCommitMode ? [] : [commit]} />
        <PublishWhenReadyCheckbox />
      </div>
      <div className="commit-info-actions-bar-right">
        {isAnythingBeingEdited && !isCommitMode ? (
          <Button onClick={() => clearEditedCommitMessage()}>
            <T>Cancel</T>
          </Button>
        ) : null}

        {showCommitOrAmend ? (
          <Tooltip
            title={
              areImageUploadsOngoing
                ? t('Image uploads are still pending')
                : isCommitMode
                  ? selection.isEverythingSelected()
                    ? t('No changes to commit')
                    : t('No selected changes to commit')
                  : selection.isEverythingSelected()
                    ? t('No changes to amend')
                    : t('No selected changes to amend')
            }
            trigger={areImageUploadsOngoing || !anythingToCommit ? 'hover' : 'disabled'}>
            <OperationDisabledButton
              contextKey={isCommitMode ? 'commit' : 'amend'}
              disabled={!anythingToCommit || editedMessage == null || areImageUploadsOngoing}
              runOperation={async () => {
                if (!isCommitMode) {
                  const updatedMessage = applyEditedFields(latestMessage, editedMessage);
                  const stringifiedMessage = commitMessageFieldsToString(schema, updatedMessage);
                  const diffId = findEditedDiffNumber(updatedMessage) ?? commit.diffId;
                  // if there's a diff attached, we should also update the remote message
                  if (messageSyncEnabled && diffId) {
                    const shouldAbort = await tryToUpdateRemoteMessage(
                      commit,
                      diffId,
                      stringifiedMessage,
                      showOptionModal,
                      'amend',
                    );
                    if (shouldAbort) {
                      return;
                    }
                  }
                }

                {
                  const shouldContinue = await confirmUnsavedFiles();
                  if (!shouldContinue) {
                    return;
                  }
                }

                {
                  const shouldContinue = await confirmSuggestedEditsForFiles(
                    isCommitMode ? 'commit' : 'amend',
                    'accept',
                    selection.selection,
                  );
                  if (!shouldContinue) {
                    return;
                  }
                }

                return doAmendOrCommit();
              }}>
              {isCommitMode ? <T>Commit</T> : <T>Amend</T>}
            </OperationDisabledButton>
          </Tooltip>
        ) : (
          <Tooltip
            title={
              areImageUploadsOngoing
                ? t('Image uploads are still pending')
                : !isAnythingBeingEdited
                  ? t('No message edits to amend')
                  : messageSyncEnabled && commit.diffId != null
                    ? t(
                        'Amend the commit message with the newly entered message, then sync that message up to $provider.',
                        {replace: {$provider: codeReviewProviderName ?? 'remote'}},
                      )
                    : t('Amend the commit message with the newly entered message.')
            }>
            <OperationDisabledButton
              contextKey={`amend-message-${commit.hash}`}
              data-testid="amend-message-button"
              disabled={!isAnythingBeingEdited || editedMessage == null || areImageUploadsOngoing}
              runOperation={async () => {
                const updatedMessage = applyEditedFields(latestMessage, editedMessage);
                const stringifiedMessage = commitMessageFieldsToString(schema, updatedMessage);
                const diffId = findEditedDiffNumber(updatedMessage) ?? commit.diffId;
                // if there's a diff attached, we should also update the remote message
                if (messageSyncEnabled && diffId) {
                  const shouldAbort = await tryToUpdateRemoteMessage(
                    commit,
                    diffId,
                    stringifiedMessage,
                    showOptionModal,
                    'amendMessage',
                  );
                  if (shouldAbort) {
                    return;
                  }
                }

                const intendedAuthor = readAtom(authorString);
                const authorArg =
                  intendedAuthor != null && commit.author !== intendedAuthor
                    ? intendedAuthor
                    : undefined;

                const operation = new AmendMessageOperation(
                  latestSuccessorUnlessExplicitlyObsolete(commit),
                  stringifiedMessage,
                  authorArg,
                );
                clearEditedCommitMessage(/* skip confirmation */ true);
                return operation;
              }}>
              <T>Amend Message</T>
            </OperationDisabledButton>
          </Tooltip>
        )}
        <SubmitButton
          commit={commit}
          getAmendOrCommitOperation={doAmendOrCommit}
          anythingToCommit={anythingToCommit}
          isAnythingBeingEdited={isAnythingBeingEdited}
          isCommitMode={isCommitMode}
        />
      </div>
    </div>
  );
}

function SubmitButtonLabel({
  showCommitOrAmend,
  isCommitMode,
  shouldSubmitAsDraft,
}: {
  showCommitOrAmend: boolean;
  isCommitMode: boolean;
  shouldSubmitAsDraft: boolean;
}) {
  if (showCommitOrAmend) {
    if (isCommitMode) {
      return shouldSubmitAsDraft ? <T>Commit and Submit Draft</T> : <T>Commit and Submit</T>;
    }
    return shouldSubmitAsDraft ? <T>Amend and Submit Draft</T> : <T>Amend and Submit</T>;
  }
  return shouldSubmitAsDraft ? <T>Submit Draft</T> : <T>Submit</T>;
}

function SubmitButton({
  commit,
  getAmendOrCommitOperation,
  anythingToCommit,
  isAnythingBeingEdited,
  isCommitMode,
}: {
  commit: CommitInfo;
  getAmendOrCommitOperation: () => Operation;
  anythingToCommit: boolean;
  isAnythingBeingEdited: boolean;
  isCommitMode: boolean;
}) {
  const [repoInfo, setRepoInfo] = useAtom(repositoryInfo);
  const diffSummaries = useAtomValue(allDiffSummaries);
  const shouldSubmitAsDraft = useAtomValue(submitAsDraft);
  const shouldPublishWhenReady = useAtomValue(publishWhenReady);
  const [updateMessage, setUpdateMessage] = useAtom(diffUpdateMessagesState(commit.hash));
  const provider = useAtomValue(codeReviewProvider);

  const codeReviewProviderType = repoInfo?.codeReviewSystem.type ?? 'unknown';
  const canSubmitWithCodeReviewProvider =
    codeReviewProviderType !== 'none' && codeReviewProviderType !== 'unknown';
  const submittable =
    diffSummaries.value && provider?.getSubmittableDiffs([commit], diffSummaries.value);
  const canSubmitIndividualDiffs = submittable && submittable.length > 0;

  const showOptionModal = useModal();
  const forceEnableSubmit = useAtomValue(overrideDisabledSubmitModes);
  const submitDisabledReason = forceEnableSubmit ? undefined : provider?.submitDisabledReason?.();
  const messageSyncEnabled = useAtomValue(messageSyncingEnabledState);
  const areImageUploadsOngoing = useAtomValue(imageUploadsPendingAtom);

  const runOperation = useRunOperation();

  const selection = useUncommittedSelection();

  const isBranchingPREnabled =
    codeReviewProviderType === 'github' && repoInfo?.preferredSubmitCommand === 'push';

  const disabledReason = areImageUploadsOngoing
    ? t('Image uploads are still pending')
    : submitDisabledReason
      ? submitDisabledReason
      : !canSubmitWithCodeReviewProvider
        ? t('No code review system found for this repository')
        : null;

  const getApplicableOperations = async (): Promise<Array<Operation> | undefined> => {
    const shouldContinue = await confirmUnsavedFiles();
    if (!shouldContinue) {
      return;
    }

    if (!(await confirmNoBlockingDiagnostics(selection, isCommitMode ? undefined : commit))) {
      return;
    }

    let amendOrCommitOp;
    if (commit.isDot && anythingToCommit) {
      // TODO: we should also amend if there are pending commit message changes, and change the button
      // to amend message & submit.
      // Or just remove the submit button if you start editing since we'll update the remote message anyway...
      amendOrCommitOp = getAmendOrCommitOperation();
    }

    if (
      repoInfo?.type === 'success' &&
      repoInfo.codeReviewSystem.type === 'github' &&
      repoInfo.preferredSubmitCommand == null
    ) {
      const buttons = [t('Cancel') as 'Cancel', 'ghstack', 'pr'] as const;
      const cancel = buttons[0];
      const answer = await showOptionModal({
        type: 'confirm',
        icon: 'warning',
        title: t('Preferred Code Review command not yet configured'),
        message: (
          <div className="commit-info-confirm-modal-paragraphs">
            <div>
              <T replace={{$pr: <code>sl pr</code>, $ghstack: <code>sl ghstack</code>}}>
                You can configure Sapling to use either $pr or $ghstack to submit for code review on
                GitHub.
              </T>
            </div>
            <div>
              <T
                replace={{
                  $config: <code>github.preferred_submit_command</code>,
                }}>
                Each submit command has tradeoffs, due to how GitHub creates Pull Requests. This can
                be controlled by the $config config.
              </T>
            </div>
            <div>
              <T>To continue, select a command to use to submit.</T>
            </div>
            <Link href="https://sapling-scm.com/docs/git/intro#pull-requests">
              <T>Learn More</T>
            </Link>
          </div>
        ),
        buttons,
      });
      if (answer === cancel || answer == null) {
        return;
      }
      const rememberConfigOp = new SetConfigOperation(
        'local',
        'github.preferred_submit_command',
        answer,
      );
      setRepoInfo(info => ({
        ...nullthrows(info),
        preferredSubmitCommand: answer,
      }));
      // setRepoInfo updates `provider`, but we still have a stale reference in this callback.
      // So this one time, we need to manually run the new submit command.
      // Future submit calls can delegate to provider.submitOperation();
      const submitOp =
        answer === 'ghstack'
          ? new GhStackSubmitOperation({
              draft: shouldSubmitAsDraft,
            })
          : answer === 'pr'
            ? new PrSubmitOperation({
                draft: shouldSubmitAsDraft,
              })
            : null;

      // TODO: account for branching PR

      return [amendOrCommitOp, rememberConfigOp, submitOp].filter(notEmpty);
    }

    // Only do message sync if we're amending the local commit in some way.
    // If we're just doing a submit, we expect the message to have been synced previously
    // during another amend or amend message.
    const shouldUpdateMessage = !isCommitMode && messageSyncEnabled && anythingToCommit;

    const submitOp = isBranchingPREnabled
      ? null // branching PRs will show a follow-up modal which controls submitting
      : nullthrows(provider).submitOperation(
          commit.isDot ? [] : [commit], // [] means to submit the head commit
          {
            draft: shouldSubmitAsDraft,
            updateFields: shouldUpdateMessage,
            updateMessage: updateMessage || undefined,
            publishWhenReady: shouldPublishWhenReady,
          },
        );

    // clear out the update message now that we've used it to submit
    if (updateMessage) {
      setUpdateMessage('');
    }

    return [amendOrCommitOp, submitOp].filter(notEmpty);
  };

  return (commit.isDot && (anythingToCommit || !isAnythingBeingEdited)) ||
    (!commit.isDot &&
      canSubmitIndividualDiffs &&
      // For non-head commits, "submit" doesn't update the message, which is confusing.
      // Just hide the submit button so you're encouraged to "amend message" first.
      !isAnythingBeingEdited) ? (
    <Tooltip
      title={
        disabledReason ??
        t('Submit for code review with $provider', {
          replace: {$provider: provider?.label ?? 'remote'},
        })
      }
      placement="top">
      {isBranchingPREnabled ? (
        <Button
          primary
          disabled={disabledReason != null}
          onClick={async () => {
            try {
              const operations = await getApplicableOperations();
              if (operations == null || operations.length === 0) {
                return;
              }

              for (const operation of operations) {
                runOperation(operation);
              }
              const dag = readAtom(dagWithPreviews);
              const topOfStack = commit.isDot && isCommitMode ? dag.resolve('.') : commit;
              if (topOfStack == null) {
                throw new Error('could not find commit to push');
              }
              const pushOps = await showBranchingPrModal(topOfStack);
              if (pushOps == null) {
                return;
              }
              for (const pushOp of pushOps) {
                runOperation(pushOp);
              }
            } catch (err) {
              const error = err as Error;
              showToast(<ErrorNotice error={error} title={<T>Failed to push commits</T>} />, {
                durationMs: 10000,
              });
            }
          }}>
          {commit.isDot && anythingToCommit ? (
            isCommitMode ? (
              <T>Commit and Push...</T>
            ) : (
              <T>Amend and Push...</T>
            )
          ) : (
            <T>Push...</T>
          )}
        </Button>
      ) : (
        <OperationDisabledButton
          kind="primary"
          contextKey={`submit-${commit.isDot ? 'head' : commit.hash}`}
          disabled={disabledReason != null}
          runOperation={getApplicableOperations}>
          <SubmitButtonLabel
            showCommitOrAmend={commit.isDot && anythingToCommit}
            isCommitMode={isCommitMode}
            shouldSubmitAsDraft={shouldSubmitAsDraft}
          />
        </OperationDisabledButton>
      )}
    </Tooltip>
  ) : null;
}

async function tryToUpdateRemoteMessage(
  commit: CommitInfo,
  diffId: DiffId,
  latestMessageString: string,
  showOptionModal: ReturnType<typeof useModal>,
  reason: 'amend' | 'amendMessage',
): Promise<boolean> {
  // TODO: we could skip the update if the new message matches the old one,
  // which is possible when amending changes without changing the commit message

  let optedOutOfSync = false;
  if (diffId !== commit.diffId) {
    const buttons = [
      t('Cancel') as 'Cancel',
      t('Use Remote Message'),
      t('Sync New Message'),
    ] as const;
    const cancel = buttons[0];
    const syncButton = buttons[2];
    const answer = await showOptionModal({
      type: 'confirm',
      icon: 'warning',
      title: t('Sync message for newly attached Diff?'),
      message: (
        <T>
          You're changing the attached Diff for this commit. Would you like you sync your new local
          message up to the remote Diff, or just use the existing remote message for this Diff?
        </T>
      ),
      buttons,
    });
    tracker.track('ConfirmSyncNewDiffNumber', {
      extras: {
        choice: answer,
      },
    });
    if (answer === cancel || answer == null) {
      return true; // abort
    }
    optedOutOfSync = answer !== syncButton;
  }
  if (!optedOutOfSync) {
    const title = firstLine(latestMessageString);
    const description = latestMessageString.slice(title.length);
    // don't wait for the update mutation to go through, just let it happen in parallel with the metaedit
    tracker
      .operation('SyncDiffMessageMutation', 'SyncMessageError', {extras: {reason}}, () =>
        updateRemoteMessage(diffId, title, description),
      )
      .catch(err => {
        // Uh oh we failed to sync. Let's override all syncing so you can see your local changes
        // and we don't get you stuck in a syncing loop.

        writeAtom(messageSyncingOverrideState, false);

        showToast(
          <Banner kind={BannerKind.error}>
            <Column alignStart>
              <div>
                <T>Failed to sync message to remote. Further syncing has been disabled.</T>
              </div>
              <div>
                <T>Try manually syncing and restarting ISL.</T>
              </div>
              <div>{firstLine(err.message || err.toString())}</div>
            </Column>
          </Banner>,
          {
            durationMs: 20_000,
          },
        );
      });
  }
  return false;
}
