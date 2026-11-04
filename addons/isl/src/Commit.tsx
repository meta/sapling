/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import type {JSX, ReactNode} from 'react';
import type {ContextMenuItem} from 'shared/ContextMenu';
import type {UICodeReviewProvider} from './codeReview/UICodeReviewProvider';
import type {DagCommitInfo} from './dag/dag';
import type {CommitInfo, MergeConflicts, SuccessorInfo, WorktreeEntry} from './types';
import {succeedableRevset, WarningCheckResult} from './types';

import {Button} from 'isl-components/Button';
import {Icon} from 'isl-components/Icon';
import {Subtle} from 'isl-components/Subtle';
import {Tag} from 'isl-components/Tag';
import {Tooltip} from 'isl-components/Tooltip';
import {atom, useAtomValue, useSetAtom} from 'jotai';
import React, {memo, useState} from 'react';
import {ComparisonType} from 'shared/Comparison';
import {useContextMenu} from 'shared/ContextMenu';
import {MS_PER_DAY} from 'shared/constants';
import {useAutofocusRef} from 'shared/hooks';
import {basename, guessPathSep, notEmpty, nullthrows} from 'shared/utils';
import {AllBookmarksTruncated, Bookmark, Bookmarks, createBookmarkAtCommit} from './Bookmark';
import {openBrowseUrlForHash, supportsBrowseUrlForHash} from './BrowseRepo';
import {worktreeMutationsSupported} from './CheckedOutElsewhere';
import css from './Commit.module.css';
import {cloudSyncStateAtom} from './CommitCloud';
import {hasUnsavedEditedCommitMessage} from './CommitInfoView/CommitInfoState';
import {showComparison} from './ComparisonView/atoms';
import {Row} from './ComponentUtils';
import {DragToRebase} from './DragToRebase';
import {EducationInfoTip} from './Education';
import {HighlightCommitsWhileHovering} from './HighlightedCommits';
import {Internal} from './Internal';
import {SubmitSelectionButton} from './SubmitSelectionButton';
import {SubmitSingleCommitButton} from './SubmitSingleCommitButton';
import {getSuggestedRebaseOperation, suggestedRebaseDestinations} from './SuggestedRebase';
import {UncommitButton} from './UncommitButton';
import {UncommittedChanges} from './UncommittedChanges';
import {changeCwd, openWorktreeInWindow, RenameWorktreeModal} from './WorktreeSection';
import {tracker} from './analytics';
import {clipboardLinkHtml} from './clipboard';
import {
  allDiffSummaries,
  branchingDiffInfos,
  codeReviewProvider,
  diffSummary,
  latestCommitTitles,
} from './codeReview/CodeReviewInfo';
import {DiffBadge, DiffFollower, DiffInfo} from './codeReview/DiffBadge';
import {submitAsDraft} from './codeReview/DraftCheckbox';
import {SyncStatus, syncStatusAtom} from './codeReview/syncStatus';
import {FoldButton, useRunFoldPreview} from './fold';
import {findPublicBaseAncestor} from './getCommitTree';
import {t, T} from './i18n';
import {IconStack} from './icons/IconStack';
import {IrrelevantCwdIcon} from './icons/IrrelevantCwdIcon';
import {
  configBackedAtom,
  localStorageBackedAtom,
  readAtom,
  useAtomGet,
  useAtomSelect,
  writeAtom,
} from './jotaiUtils';
import {CONFLICT_SIDE_LABELS} from './mergeConflicts/consts';
import {getAmendToOperation, isAmendToAllowedForCommit} from './operationUtils';
import {CommitCloudMoveCommitsOperation} from './operations/CommitCloudMoveCommitsOperation';
import {GotoOperation} from './operations/GotoOperation';
import {HideOperation} from './operations/HideOperation';
import {RebaseOperation} from './operations/RebaseOperation';
import {RemoveWorktreeOperation} from './operations/RemoveWorktreeOperation';
import {RenameWorktreeOperation} from './operations/RenameWorktreeOperation';
import {
  operationBeingPreviewed,
  runOperation,
  useInlineProgress,
  useRunOperation,
  useRunPreviewedOperation,
} from './operationsState';
import platform from './platform';
import {CommitPreview, dagWithPreviews, uncommittedChangesWithPreviews} from './previews';
import {RelativeDate, relativeDate} from './relativeDate';
import {repoRelativeCwd, useIsIrrelevantToCwd} from './repositoryData';
import {commitTreeWidth} from './responsive';
import {
  selectedCommitInfos,
  selectedCommitInfosInDagOrder,
  selectedCommits,
  selectedCommitsRangeComparison,
  useCommitCallbacks,
} from './selection';
import {applicationinfo, inMergeConflicts, mergeConflicts} from './serverAPIState';
import {SmartActionsDropdown} from './smartActions/SmartActionsDropdown';
import {useConfirmUnsavedEditsBeforeSplit} from './stackEdit/ui/ConfirmUnsavedEditsBeforeSplit';
import {SplitButton} from './stackEdit/ui/SplitButton';
import {editingStackIntentionHashes} from './stackEdit/ui/stackEditState';
import {latestSuccessorUnlessExplicitlyObsolete} from './successionUtils';
import {copyAndShowToast} from './toast';
import {showConfirmation, showModal} from './useModal';
import {short} from './utils';

export const rebaseOffWarmWarningEnabled = localStorageBackedAtom<boolean>(
  'isl.rebase-off-warm-warning-enabled',
  true,
  {portable: true},
);

export const distantRebaseWarningEnabled = localStorageBackedAtom<boolean>(
  'isl.distant-rebase-warning-enabled',
  true,
  {portable: true},
);

export const rebaseOntoMasterWarningEnabled = localStorageBackedAtom<boolean>(
  'isl.rebase-onto-master-warning-enabled',
  true,
  {portable: true},
);

export type CopyCommitHashFormat = 'long' | 'short';

export const copyCommitHashFormatAtom = configBackedAtom<CopyCommitHashFormat>(
  'isl.copy-commit-hash-format',
  'long',
);

/**
 * Some preview types should not allow actions on top of them
 * For example, you can't click goto on the preview of dragging a rebase,
 * but you can goto on the optimistic form of a running rebase.
 */
function previewPreventsActions(preview?: CommitPreview): boolean {
  switch (preview) {
    case CommitPreview.REBASE_OLD:
    case CommitPreview.REBASE_DESCENDANT:
    case CommitPreview.REBASE_ROOT:
    case CommitPreview.HIDDEN_ROOT:
    case CommitPreview.HIDDEN_DESCENDANT:
    case CommitPreview.FOLD:
    case CommitPreview.FOLD_PREVIEW:
    case CommitPreview.NON_ACTIONABLE_COMMIT:
      return true;
  }
  return false;
}

function commitLabelForCommit(conflicts: MergeConflicts | undefined, hash: string): string | null {
  const {localShort, incomingShort} = CONFLICT_SIDE_LABELS;
  const hashes = conflicts?.hashes;
  if (hash === hashes?.other) {
    return incomingShort;
  } else if (hash === hashes?.local) {
    return localShort;
  }
  return null;
}

const hasUncommittedChangesAtom = atom(get => get(uncommittedChangesWithPreviews).length > 0);

function RepoRelativeCwd() {
  const cwd = useAtomValue(repoRelativeCwd);
  return <pre>{cwd}</pre>;
}

export const Commit = memo(
  ({
    commit,
    previewType,
    hasChildren,
  }: {
    commit: DagCommitInfo | CommitInfo;
    previewType?: CommitPreview;
    hasChildren: boolean;
  }) => {
    const isPublic = commit.phase === 'public';
    const isObsoleted = commit.successorInfo != null;
    const hasUncommittedChanges = useAtomValue(hasUncommittedChangesAtom);

    const isIrrelevantToCwd = useIsIrrelevantToCwd(commit);

    const handlePreviewedOperation = useRunPreviewedOperation();
    const runOperation = useRunOperation();
    const setEditStackIntentionHashes = useSetAtom(editingStackIntentionHashes);

    const inlineProgress = useInlineProgress(commit.hash);

    const {isSelected, onDoubleClickToShowDrawer} = useCommitCallbacks(commit);
    const actionsPrevented = previewPreventsActions(previewType);

    const [isActioning, setIsActioning] = useState(false);

    const inConflicts = useAtomValue(inMergeConflicts);

    const treeWidth = useAtomValue(commitTreeWidth);

    const title = useAtomGet(latestCommitTitles, commit.hash) ?? '';

    const commitLabel = useAtomSelect(
      mergeConflicts,
      conflicts => commitLabelForCommit(conflicts, commit.hash),
      [commit.hash],
    );

    const clipboardCopy = (text: string, url?: string) =>
      copyAndShowToast(text, url == null ? undefined : clipboardLinkHtml(text, url));

    const confirmUnsavedEditsBeforeSplit = useConfirmUnsavedEditsBeforeSplit();
    async function handleSplit() {
      if (!(await confirmUnsavedEditsBeforeSplit([commit], 'split'))) {
        return;
      }
      setEditStackIntentionHashes(['split', new Set([commit.hash])]);
      tracker.track('SplitOpenFromCommitContextMenu');
    }

    const makeContextMenuOptions = () => {
      const syncStatus = readAtom(syncStatusAtom)?.get(commit.hash);

      const copyHashFormat = readAtom(copyCommitHashFormatAtom);
      const items: Array<ContextMenuItem & {loggingLabel?: string}> = [];

      const selectedCommitHashes = readAtom(selectedCommitInfos)
        .map(info => (copyHashFormat === 'short' ? short(info.hash) : info.hash))
        .filter(notEmpty);
      if (selectedCommitHashes.length > 1) {
        items.push({
          label: (
            <T
              replace={{
                $count: selectedCommitHashes.length < 10 ? selectedCommitHashes.length : '9+',
              }}>
              Copy $count Selected Commit Hashes
            </T>
          ),
          onClick: () => {
            const text = selectedCommitHashes.join(' ');
            clipboardCopy(text);
          },
          loggingLabel: 'Copy Selected Commit Hashes',
        });
      } else {
        const hashToCopy = copyHashFormat === 'short' ? short(commit.hash) : commit.hash;
        items.push({
          label: <T replace={{$hash: short(commit?.hash)}}>Copy Commit Hash "$hash"</T>,
          onClick: () => clipboardCopy(hashToCopy),
          loggingLabel: 'Copy Commit Hash',
        });
      }

      if (isPublic && readAtom(supportsBrowseUrlForHash)) {
        items.push({
          label: (
            <Row>
              <T>Browse Repo At This Commit</T>
              <Icon icon="link-external" />
            </Row>
          ),
          onClick: () => {
            openBrowseUrlForHash(commit.hash);
          },
          loggingLabel: 'Browse Repo At This Commit',
        });
      }
      const selectedDiffIDs = readAtom(selectedCommitInfos)
        .map(info => info.diffId)
        .filter(notEmpty);
      if (selectedDiffIDs.length > 1) {
        items.push({
          label: (
            <T replace={{$count: selectedDiffIDs.length < 10 ? selectedDiffIDs.length : '9+'}}>
              Copy $count Selected Diff Numbers
            </T>
          ),
          onClick: () => {
            const text = selectedDiffIDs.join(' ');
            clipboardCopy(text);
          },
          loggingLabel: 'Copy Selected Diff Numbers',
        });
      } else if (!isPublic && commit.diffId != null) {
        items.push({
          label: <T replace={{$number: commit.diffId}}>Copy Diff Number "$number"</T>,
          onClick: () => {
            const info = readAtom(diffSummary(commit.diffId));
            const url = info?.value?.url;
            clipboardCopy(commit.diffId ?? '', url);
          },
          loggingLabel: 'Copy Diff Number',
        });
      }
      const multiDiffProvider = readAtom(codeReviewProvider);
      if (multiDiffProvider?.getMultiDiffUrl != null) {
        const selectedDiffIDsInDagOrder = readAtom(selectedCommitInfosInDagOrder)
          .map(selected => selected.diffId)
          .filter(notEmpty);
        if (selectedDiffIDsInDagOrder.length > 1) {
          const multiDiffUrl = multiDiffProvider.getMultiDiffUrl(selectedDiffIDsInDagOrder);
          items.push({
            label: <T>Copy Multi-Diff URL</T>,
            onClick: () => {
              clipboardCopy(multiDiffUrl);
            },
            loggingLabel: 'Copy Multi-Diff URL',
          });
          items.push({
            label: <T>Open Multi-Diff in Phabricator</T>,
            onClick: () => {
              platform.openExternalLink(multiDiffUrl);
            },
            loggingLabel: 'Open Multi-Diff in Phabricator',
          });
        }
      }
      if (!isPublic) {
        items.push({
          label: <T>View Changes in Commit</T>,
          onClick: () => showComparison({type: ComparisonType.Committed, hash: commit.hash}),
          loggingLabel: 'View Changes in Commit',
        });

        const commitRangeComparison = readAtom(selectedCommitsRangeComparison);

        if (commitRangeComparison != null) {
          items.push({
            label: <T>View Changes Across Selected Commits</T>,
            onClick: () => showComparison(commitRangeComparison),
            loggingLabel: 'View Changes Across Selected Commits',
          });
        }

        const provider = readAtom(codeReviewProvider);
        if (provider != null) {
          const selectedInfos = readAtom(selectedCommitInfos);
          const diffSummaries = readAtom(allDiffSummaries);

          const isMultiSelect =
            selectedInfos.length > 1 && selectedInfos.some(c => c.hash === commit.hash);
          const commits = isMultiSelect ? readAtom(selectedCommitInfosInDagOrder) : [commit];
          const submittable =
            (diffSummaries?.value != null
              ? provider.getSubmittableDiffs(commits, diffSummaries.value)
              : undefined) ?? [];

          if (submittable.length > 0) {
            items.push({
              label:
                submittable.length > 1 ? (
                  <T replace={{$count: submittable.length}}>Submit $count Commits</T>
                ) : (
                  <T>Submit Commit</T>
                ),
              onClick: () => {
                runOperation(
                  provider.submitOperation(submittable, {
                    draft: readAtom(submitAsDraft),
                  }),
                );
              },
              loggingLabel: submittable.length > 1 ? 'Submit Multiple Commits' : 'Submit Commit',
            });
          }
          // Bulk publish for multi-selected unpublished/draft diffs
          if (isMultiSelect && diffSummaries?.value != null) {
            const publishActions: Array<() => void> = [];
            for (const c of selectedInfos) {
              if (c.diffId == null) {
                continue;
              }
              const summary = diffSummaries.value.get(c.diffId);
              if (summary == null) {
                continue;
              }
              const actions = provider.getUpdateDiffActions(summary);
              const publishAction = actions.find(a => a.label === 'Publish');
              if (publishAction != null) {
                publishActions.push(publishAction.onClick);
              }
            }
            if (publishActions.length > 0) {
              items.push({
                label:
                  publishActions.length > 1 ? (
                    <T replace={{$count: publishActions.length}}>Publish $count Diffs</T>
                  ) : (
                    <T>Publish Diff</T>
                  ),
                onClick: () => {
                  for (const action of publishActions) {
                    action();
                  }
                },
                loggingLabel: publishActions.length > 1 ? 'Publish Multiple Diffs' : 'Publish Diff',
              });
            }
          }
        }
      }
      if (!isPublic && syncStatus != null && syncStatus !== SyncStatus.InSync) {
        const provider = readAtom(codeReviewProvider);
        if (provider?.supportsComparingSinceLastSubmit) {
          items.push({
            label: <T replace={{$provider: provider?.label ?? 'remote'}}>Compare with $provider</T>,
            onClick: () => {
              showComparison({type: ComparisonType.SinceLastCodeReviewSubmit, hash: commit.hash});
            },
            loggingLabel: 'Compare with Provider',
          });
        }
      }
      if (!isPublic && commit.diffId != null) {
        const provider = readAtom(codeReviewProvider);
        const summary = readAtom(diffSummary(commit.diffId));
        if (summary.value) {
          const actions = provider?.getUpdateDiffActions(summary.value);
          if (actions != null && actions.length > 0) {
            items.push({
              label: <T replace={{$number: commit.diffId}}>Update Diff $number</T>,
              type: 'submenu',
              children: actions,
            });
          }
        }
      }
      if (!isPublic && !actionsPrevented && !inConflicts) {
        const suggestedRebases = readAtom(suggestedRebaseDestinations);
        items.push({
          label: 'Rebase onto',
          type: 'submenu',
          children:
            suggestedRebases
              ?.filter(([dest]) => !commit.isDot || !dest.isDot)
              .map(([dest, name]) => ({
                label: name,
                onClick: async () => {
                  const operation = getSuggestedRebaseOperation(
                    dest,
                    latestSuccessorUnlessExplicitlyObsolete(commit),
                  );

                  const shouldProceed = await runWarningChecks([
                    () => maybeWarnAboutRebaseOntoMaster(dest),
                    () => maybeWarnAboutRebaseOffWarm(dest),
                  ]);

                  if (shouldProceed) {
                    runOperation(operation);
                  }
                },
              })) ?? [],
        });
        if (isAmendToAllowedForCommit(commit)) {
          items.push({
            label: <T>Amend changes to here</T>,
            onClick: () => runOperation(getAmendToOperation(commit)),
          });
        }
        if (!isObsoleted) {
          items.push({
            label: hasUncommittedChanges ? (
              <span className="context-menu-disabled-option">
                <T>Split... </T>
                <Subtle>
                  <T>(disabled due to uncommitted changes)</T>
                </Subtle>
              </span>
            ) : (
              <T>Split...</T>
            ),
            onClick: hasUncommittedChanges ? () => null : handleSplit,
            loggingLabel: 'Split',
          });
        }
        items.push({
          label: <T>Create Bookmark...</T>,
          onClick: () => {
            createBookmarkAtCommit(commit);
          },
          loggingLabel: 'Create Bookmark',
        });
        const hideSelectedInfos = readAtom(selectedCommitInfos);
        const isHideMultiSelect =
          hideSelectedInfos.length > 1 && hideSelectedInfos.some(c => c.hash === commit.hash);
        if (isHideMultiSelect) {
          items.push({
            label: <T replace={{$count: hideSelectedInfos.length}}>Hide $count Commits</T>,
            onClick: () =>
              writeAtom(
                operationBeingPreviewed,
                new HideOperation(
                  hideSelectedInfos.map(c => latestSuccessorUnlessExplicitlyObsolete(c)),
                ),
              ),
            loggingLabel: 'Hide Selected Commits',
          });
        } else {
          items.push({
            label: hasChildren ? <T>Hide Commit and Descendants</T> : <T>Hide Commit</T>,
            onClick: () =>
              writeAtom(
                operationBeingPreviewed,
                new HideOperation(latestSuccessorUnlessExplicitlyObsolete(commit)),
              ),
            loggingLabel: 'Hide Commit',
          });
        }
      }
      if (!isPublic && !actionsPrevented && !inConflicts) {
        // Render "Move to workspace …" only when cloud is enabled, the
        // current workspace is known, and at least one other workspace
        // exists to target. `sl cloud move` itself follows descendants
        // and bookmarks, so each per-destination submenu item dispatches
        // exactly the hashes the user has selected — no separate
        // stack-aware branching is needed beyond multi-select.
        const cloudSyncState = readAtom(cloudSyncStateAtom);
        const cloud = cloudSyncState?.value;
        const currentWorkspace = cloud?.currentWorkspace;
        const workspaceChoices = cloud?.workspaceChoices;
        if (
          cloud != null &&
          cloud.isDisabled !== true &&
          currentWorkspace != null &&
          workspaceChoices != null &&
          workspaceChoices.length >= 2
        ) {
          const destinations = workspaceChoices.filter(ws => ws !== currentWorkspace);
          if (destinations.length > 0) {
            // Multi-select: when the right-clicked commit is part of the
            // current smartlog selection (>1 commits), the operation
            // moves the whole selection in one `sl cloud move` invocation.
            // Mirrors the sibling Hide flow above (`isHideMultiSelect`)
            // so the two context-menu items behave consistently — without
            // this, the user would right-click on one of N selected
            // commits, see no visible cue that the menu only acts on
            // that single commit, and silently lose the other N-1
            // commits' moves (they'd stay in the source workspace).
            //
            // Filter to draft-phase only on the multi-select path. The
            // outer `!isPublic` gate at the submenu level already
            // protects the right-clicked commit; this filter extends
            // the same guarantee to the rest of the selection. `sl
            // cloud move` rejects public commits server-side, so
            // dispatching them in a mixed batch would either fail the
            // whole batch (no commits moved) or silently skip them
            // (depending on the sl version) — both bad UX. Filtering
            // client-side makes the behavior deterministic: only
            // drafts get moved, public commits stay put.
            const moveSelectedInfos = readAtom(selectedCommitInfos);
            const isMoveMultiSelect =
              moveSelectedInfos.length > 1 && moveSelectedInfos.some(c => c.hash === commit.hash);
            // Use `latestSuccessorUnlessExplicitlyObsolete` instead of raw
            // hashes so queued operations that mutate these commits before
            // `cloud move` runs are followed to their successor — except
            // when the user right-clicked a visibly-obsolete commit, in
            // which case we lock the action to the exact hash they saw.
            // Mirrors the sibling `HideOperation` callsite above.
            const sourcesToMove = isMoveMultiSelect
              ? moveSelectedInfos
                  .filter(c => c.phase !== 'public')
                  .map(c => latestSuccessorUnlessExplicitlyObsolete(c))
              : [latestSuccessorUnlessExplicitlyObsolete(commit)];
            items.push({
              label: isMoveMultiSelect ? (
                <T replace={{$count: sourcesToMove.length}}>Move $count Commits to workspace</T>
              ) : (
                <T>Move to workspace</T>
              ),
              type: 'submenu',
              children: destinations.map(ws => ({
                label: ws,
                onClick: () => {
                  runOperation(new CommitCloudMoveCommitsOperation(ws, sourcesToMove));
                },
              })),
            });
          }
        }
      }
      if (!actionsPrevented && !commit.isDot) {
        items.push({
          label: <T>Goto</T>,
          onClick: async () => {
            await gotoAction(runOperation, commit);
          },
          loggingLabel: 'Goto',
        });
      }
      if (commit.fullRepoBranch != null) {
        const fullRepoBranchItems = Internal.getFullRepoBranchMergeContextMenuItems?.(
          commit.fullRepoBranch,
        );
        if (fullRepoBranchItems != null && fullRepoBranchItems instanceof Array) {
          items.push(...fullRepoBranchItems);
        }
      }
      return items;
    };

    const contextMenu = useContextMenu(
      (): Array<ContextMenuItem> => {
        return makeContextMenuOptions().map((item: ContextMenuItem & {loggingLabel?: string}) => {
          if (item.type == null && notEmpty(item.loggingLabel)) {
            return {
              ...item,
              onClick: () => {
                tracker.track('CommitContextMenuItemClick', {
                  extras: {choice: item.loggingLabel},
                });
                item.onClick?.();
              },
            };
          }
          return item;
        });
      },
      {onOpen: () => setIsActioning(true), onDismiss: () => setIsActioning(false)},
    );

    const inlineCommitActions = [];
    const floatingCommitActions = [];

    if (previewType === CommitPreview.REBASE_ROOT) {
      inlineCommitActions.push(
        <React.Fragment key="rebase">
          <Button onClick={() => handlePreviewedOperation(/* cancel */ true)}>
            <T>Cancel</T>
          </Button>
          <Button
            primary
            onClick={() => {
              return handleRebaseConfirmation(commit, handlePreviewedOperation);
            }}>
            <T>Run Rebase</T>
          </Button>
        </React.Fragment>,
      );
    } else if (previewType === CommitPreview.HIDDEN_ROOT) {
      inlineCommitActions.push(
        <React.Fragment key="hide">
          <Button onClick={() => handlePreviewedOperation(/* cancel */ true)}>
            <T>Cancel</T>
          </Button>
          <ConfirmHideButton onClick={() => handlePreviewedOperation(/* cancel */ false)} />
        </React.Fragment>,
      );
    } else if (previewType === CommitPreview.FOLD_PREVIEW) {
      inlineCommitActions.push(<ConfirmCombineButtons key="fold" />);
    }

    if (!isPublic && !actionsPrevented && isSelected) {
      inlineCommitActions.push(
        <SubmitSelectionButton key="submit-selection-btn" commit={commit} />,
        <FoldButton key="fold-button" commit={commit} />,
      );
    }

    if (!actionsPrevented && !commit.isDot) {
      floatingCommitActions.push(
        <span className="goto-button" key="goto-button">
          <Tooltip
            title={t(
              'Update files in the working copy to match this commit. Mark this commit as the "current commit".',
            )}
            delayMs={250}>
            <Button
              icon
              aria-label={t('Go to commit "$title"', {replace: {$title: commit.title}})}
              className={css.gotoButton}
              onClick={async event => {
                event.stopPropagation(); // don't toggle selection by letting click propagate onto selection target.
                await gotoAction(runOperation, commit);
              }}>
              <T>Goto</T>
              <Icon icon="newline" />
            </Button>
          </Tooltip>
        </span>,
      );
    }

    if (!isPublic && !actionsPrevented && commit.isDot && !inConflicts) {
      inlineCommitActions.push(<SubmitSingleCommitButton key="submit" />);
      inlineCommitActions.push(<UncommitButton key="uncommit" />);
    }

    if (!isPublic && !actionsPrevented && commit.isDot && !isObsoleted && !inConflicts) {
      inlineCommitActions.push(
        <SplitButton icon key="split" trackerEventName="SplitOpenFromHeadCommit" commit={commit} />,
      );
    }

    if (!isPublic && !actionsPrevented && commit.isDot && !hasUncommittedChanges && !inConflicts) {
      inlineCommitActions.push(<SmartActionsDropdown key="smartActions" commit={commit} />);
    }

    if (!isPublic && !actionsPrevented) {
      floatingCommitActions.push(
        <OpenCommitInfoButton
          key="open-sidebar"
          revealCommit={onDoubleClickToShowDrawer}
          commit={commit}
        />,
      );
    }

    const commitActions = [...inlineCommitActions, ...floatingCommitActions];

    if ((commit as DagCommitInfo).isYouAreHere) {
      return (
        <div className="head-commit-info">
          <UncommittedChanges place="main" />
        </div>
      );
    }

    if ((commit as DagCommitInfo).isCheckedOutElsewhere) {
      return null;
    }

    return (
      <div
        className={
          'commit' +
          (commit.isDot ? ' head-commit' : '') +
          (commit.successorInfo != null ? ' obsolete' : '') +
          (isIrrelevantToCwd ? ' irrelevant' : '')
        }
        onContextMenu={contextMenu}
        data-testid={`commit-${commit.hash}`}>
        <div
          className={'commit-rows' + (isActioning ? ' commit-row-actioning' : '')}
          data-testid={isSelected ? 'selected-commit' : undefined}>
          <DragToRebase
            className={
              'commit-details' + (previewType != null ? ` commit-preview-${previewType}` : '')
            }
            commit={commit}
            previewType={previewType}>
            {isPublic ? null : (
              <span className="commit-title">
                {isIrrelevantToCwd && (
                  <Tooltip
                    title={
                      <T
                        replace={{
                          $prefix: <pre>{commit.maxCommonPathPrefix}</pre>,
                          $cwd: <RepoRelativeCwd />,
                        }}>
                        This commit only contains files within: $prefix These are irrelevant to your
                        current working directory: $cwd
                      </T>
                    }>
                    <IrrelevantCwdIcon />
                  </Tooltip>
                )}
                {commitLabel && <CommitLabel>{commitLabel}</CommitLabel>}
                <span>{title}</span>
                <CommitDate date={commit.date} />
              </span>
            )}
            <UnsavedEditedMessageIndicator commit={commit} />
            {!isPublic && <BranchingPrs bookmarks={commit.remoteBookmarks} />}
            <AllBookmarksTruncated
              local={commit.bookmarks}
              remote={
                isPublic
                  ? commit.remoteBookmarks
                  : /* draft commits with remote bookmarks are probably branching PRs, rendered above. */ []
              }
              stable={commit?.stableCommitMetadata ?? []}
              fullRepoBranch={commit.fullRepoBranch}
            />
            {isPublic ? <CommitDate date={commit.date} /> : null}
            {treeWidth !== 'wide' ? (
              <>
                {inlineCommitActions}
                {treeWidth === 'very-narrow' ? (
                  <div className="commit-narrow-right-actions">{floatingCommitActions}</div>
                ) : (
                  floatingCommitActions
                )}
              </>
            ) : null}
          </DragToRebase>
          <DivIfChildren className="commit-second-row">
            {commit.diffId && !isPublic ? (
              <DiffInfo commit={commit} hideActions={actionsPrevented || inlineProgress != null} />
            ) : null}
            {commit.successorInfo != null ? (
              <SuccessorInfoToDisplay successorInfo={commit.successorInfo} />
            ) : null}
            {inlineProgress && <InlineProgressSpan message={inlineProgress} />}
            {commit.isFollower ? <DiffFollower commit={commit} /> : null}
          </DivIfChildren>
          {treeWidth === 'wide' ? commitActions : null}
        </div>
      </div>
    );
  },
  (prevProps, nextProps) => {
    const prevCommit = prevProps.commit;
    const nextCommit = nextProps.commit;
    const commitEqual =
      'equals' in nextCommit ? nextCommit.equals(prevCommit) : nextCommit === prevCommit;
    return (
      commitEqual &&
      nextProps.previewType === prevProps.previewType &&
      nextProps.hasChildren === prevProps.hasChildren
    );
  },
);

function BranchingPrs({bookmarks}: {bookmarks: ReadonlyArray<string>}) {
  const provider = useAtomValue(codeReviewProvider);
  if (provider == null || !provider.supportBranchingPrs) {
    // If we don't have a provider, just render them as bookmarks so they don't get hidden.
    return <Bookmarks bookmarks={bookmarks} kind="remote" />;
  }
  return bookmarks.map(bookmark => (
    <BranchingPr key={bookmark} provider={provider} bookmark={bookmark} />
  ));
}

function BranchingPr({bookmark, provider}: {bookmark: string; provider: UICodeReviewProvider}) {
  const branchName = nullthrows(provider.branchNameForRemoteBookmark)(bookmark);
  const info = useAtomValue(branchingDiffInfos(branchName));
  return (
    <>
      <Bookmark kind="remote">{bookmark}</Bookmark>
      {info.value == null ? null : (
        <DiffBadge diff={info.value} provider={provider} url={info.value.url} />
      )}
    </>
  );
}

function CommitLabel({children}: {children?: ReactNode}) {
  return <div className={css.commitLabel}>{children}</div>;
}

/**
 * A label naming a sibling worktree currently checked out (`.`) at this commit,
 * followed by buttons to open and (for linked worktrees) rename and remove it.
 * Rename and remove are only shown where the server reports worktree mutations are supported.
 */
export function CheckedOutElsewhereBadge({wt}: {wt: WorktreeEntry}) {
  const canMutate = useAtomValue(worktreeMutationsSupported);
  const name =
    wt.label != null && wt.label !== '' ? wt.label : basename(wt.path, guessPathSep(wt.path));
  const hoverTitle = t('Checked out in worktree $name', {replace: {$name: name}});

  return (
    <span className={css.checkedOutElsewhereGroup}>
      <Tooltip title={hoverTitle}>
        <Tag
          className={css.checkedOutElsewhereTag}
          data-testid="checked-out-elsewhere-badge"
          aria-label={hoverTitle}>
          <Icon icon="worktree" size="XS" aria-hidden="true" />
          {name}
        </Tag>
      </Tooltip>
      <OpenWorktreeButton wt={wt} name={name} />
      {canMutate && <RenameWorktreeButton wt={wt} name={name} />}
      {canMutate && wt.role !== 'main' && <RemoveWorktreeButton wt={wt} name={name} />}
    </span>
  );
}

/**
 * On VS Code, opens a menu to choose current vs. new window; elsewhere (no
 * concept of "windows") it switches cwd directly.
 */
function OpenWorktreeButton({wt, name}: {wt: WorktreeEntry; name: string}) {
  const appInfo = useAtomValue(applicationinfo);
  const isVSCode = platform.platformName === 'vscode';
  const openMenu = useContextMenu<HTMLButtonElement>((): Array<ContextMenuItem> => [
    ...(appInfo?.isBasecamp
      ? []
      : [
          {
            label: t('Open in Current Window'),
            onClick: () => openWorktreeInWindow(wt.path, false),
          } as ContextMenuItem,
        ]),
    {
      label: appInfo?.isBasecamp ? t('Open in New Tile') : t('Open in New Window'),
      onClick: () => openWorktreeInWindow(wt.path, true),
    },
  ]);
  const title = isVSCode
    ? t('Open worktree $name', {replace: {$name: name}})
    : t('Switch to worktree $name', {replace: {$name: name}});
  return (
    <Tooltip title={title} delayMs={250}>
      <Button
        icon
        aria-label={title}
        aria-haspopup={isVSCode ? 'menu' : undefined}
        data-testid="checked-out-elsewhere-open-button"
        onClick={e => {
          if (isVSCode) {
            // useContextMenu already stops propagation and prevents default.
            openMenu(e);
            return;
          }
          e.stopPropagation();
          e.preventDefault();
          changeCwd(wt.path);
        }}>
        <Icon icon="arrow-swap" />
      </Button>
    </Tooltip>
  );
}

function RenameWorktreeButton({wt, name}: {wt: WorktreeEntry; name: string}) {
  const runOperation = useRunOperation();
  if (wt.role === 'main') {
    return null;
  }
  const title = t('Rename worktree $name', {replace: {$name: name}});
  return (
    <Tooltip title={title} delayMs={250}>
      <Button
        icon
        aria-label={title}
        data-testid="checked-out-elsewhere-rename-button"
        onClick={async e => {
          e.stopPropagation();
          e.preventDefault();
          const result = await showModal<string | undefined>({
            type: 'custom',
            title: <T>Rename Worktree</T>,
            icon: 'worktree',
            component: ({returnResultAndDismiss}) => (
              <RenameWorktreeModal
                returnResultAndDismiss={returnResultAndDismiss}
                currentLabel={wt.label ?? ''}
                wtBasename={name}
              />
            ),
          });
          if (result !== undefined) {
            await runOperation(new RenameWorktreeOperation(wt.path, result || undefined), true);
          }
        }}>
        <Icon icon="edit" />
      </Button>
    </Tooltip>
  );
}

function RemoveWorktreeButton({wt, name}: {wt: WorktreeEntry; name: string}) {
  const runOperation = useRunOperation();
  const title = t('Remove worktree $name', {replace: {$name: name}});
  return (
    <Tooltip title={title} delayMs={250}>
      <Button
        icon
        aria-label={title}
        data-testid="checked-out-elsewhere-remove-button"
        onClick={async e => {
          e.stopPropagation();
          e.preventDefault();
          const confirmed = await showModal({
            type: 'confirm',
            title: <T>Remove Worktree</T>,
            icon: 'worktree',
            message: (
              <span>
                <Row>
                  <T replace={{$path: <code>{name}</code>}}>
                    Are you sure you want to remove the worktree $path?
                  </T>
                </Row>
                <Row style={{marginTop: 'var(--pad)'}}>
                  <Subtle>
                    <T>
                      Any uncommitted changes and shelves in this worktree will be lost forever.
                    </T>
                  </Subtle>
                </Row>
              </span>
            ),
            buttons: [{label: t('Cancel')}, {label: t('Remove'), primary: true}],
          });
          if (confirmed?.label === t('Remove')) {
            await runOperation(new RemoveWorktreeOperation(wt.path), true);
          }
        }}>
        <Icon icon="trash" />
      </Button>
    </Tooltip>
  );
}

export function InlineProgressSpan(props: {message: string}) {
  return (
    <span className="commit-inline-operation-progress">
      <Icon icon="loading" /> <T>{props.message}</T>
    </span>
  );
}

function OpenCommitInfoButton({
  commit,
  revealCommit,
}: {
  commit: CommitInfo;
  revealCommit: () => unknown;
}) {
  return (
    <Tooltip title={t("Open commit's details in sidebar")} delayMs={250}>
      <Button
        icon
        onClick={e => {
          revealCommit();
          e.stopPropagation();
          e.preventDefault();
        }}
        className="open-commit-info-button"
        aria-label={t('Open commit "$title"', {replace: {$title: commit.title}})}
        data-testid="open-commit-info-button">
        <Icon icon="chevron-right" />
      </Button>
    </Tooltip>
  );
}

function ConfirmHideButton({onClick}: {onClick: () => unknown}) {
  const ref = useAutofocusRef() as React.MutableRefObject<null>;
  return (
    <Button ref={ref} primary onClick={onClick}>
      <T>Hide</T>
    </Button>
  );
}

function ConfirmCombineButtons() {
  const ref = useAutofocusRef() as React.MutableRefObject<null>;
  const [cancel, run] = useRunFoldPreview();

  return (
    <>
      <Button onClick={cancel}>
        <T>Cancel</T>
      </Button>
      <Button ref={ref} primary onClick={run}>
        <T>Run Combine</T>
      </Button>
    </>
  );
}

function CommitDate({date}: {date: Date}) {
  return (
    <span className="commit-date" title={date.toLocaleString()}>
      <RelativeDate date={date} useShortVariant />
    </span>
  );
}

function DivIfChildren({
  children,
  ...props
}: React.DetailedHTMLProps<React.HTMLAttributes<HTMLDivElement>, HTMLDivElement>) {
  if (!children || (Array.isArray(children) && children.filter(notEmpty).length === 0)) {
    return null;
  }
  return <div {...props}>{children}</div>;
}

function UnsavedEditedMessageIndicator({commit}: {commit: CommitInfo}) {
  const isEdted = useAtomValue(hasUnsavedEditedCommitMessage(commit.hash));
  if (!isEdted) {
    return null;
  }
  return (
    <div className="unsaved-message-indicator" data-testid="unsaved-message-indicator">
      <Tooltip title={t('This commit has unsaved changes to its message')}>
        <IconStack>
          <Icon icon="output" />
          <Icon icon="circle-large-filled" />
        </IconStack>
      </Tooltip>
    </div>
  );
}

export function SuccessorInfoToDisplay({successorInfo}: {successorInfo: SuccessorInfo}) {
  const successorType = successorInfo.type;
  const inner: JSX.Element = {
    pushrebase: <T>Landed as a newer commit</T>,
    land: <T>Landed as a newer commit</T>,
    amend: <T>Amended as a newer commit</T>,
    rebase: <T>Rebased as a newer commit</T>,
    split: <T>Split as a newer commit</T>,
    fold: <T>Folded as a newer commit</T>,
    histedit: <T>Histedited as a newer commit</T>,
  }[successorType] ?? <T>Rewritten as a newer commit</T>;
  const isSuccessorPublic = successorType === 'land' || successorType === 'pushrebase';
  return (
    <Row style={{gap: 'var(--halfpad)'}}>
      <HighlightCommitsWhileHovering toHighlight={[successorInfo.hash]}>
        {inner}
      </HighlightCommitsWhileHovering>
      <EducationInfoTip>
        <ObsoleteTip isSuccessorPublic={isSuccessorPublic} />
      </EducationInfoTip>
    </Row>
  );
}

function ObsoleteTipInner(props: {isSuccessorPublic?: boolean}) {
  const tips: string[] = props.isSuccessorPublic
    ? [
        t('Avoid editing (e.g., amend, rebase) this obsoleted commit. It cannot be landed again.'),
        t(
          'The new commit was landed in a public branch and became immutable. It cannot be edited or hidden.',
        ),
        t('If you want to make changes, create a new commit.'),
      ]
    : [
        t(
          'Avoid editing (e.g., amend, rebase) this obsoleted commit. You should use the new commit instead.',
        ),
        t(
          'If you do edit, there will be multiple new versions. They look like duplications and there is no easy way to de-duplicate (e.g. merge all edits back into one commit).',
        ),
        t(
          'To revert to this obsoleted commit, simply hide the new one. It will remove the "obsoleted" status.',
        ),
      ];

  return (
    <div style={{maxWidth: '60vw'}}>
      <T>This commit is "obsoleted" because a newer version exists.</T>
      <ul>
        {tips.map((tip, i) => (
          <li key={i}>{tip}</li>
        ))}
      </ul>
    </div>
  );
}

async function maybeWarnAboutOldDestination(dest: CommitInfo): Promise<WarningCheckResult> {
  const provider = readAtom(codeReviewProvider);
  // Cutoff age is determined by the code review provider since internal repos have different requirements than GitHub-backed repos.
  const MAX_AGE_CUTOFF_MS = provider?.gotoDistanceWarningAgeCutoff ?? 30 * MS_PER_DAY;

  const dag = readAtom(dagWithPreviews);
  const currentBase = findPublicBaseAncestor(dag);
  const destBase = findPublicBaseAncestor(dag, dest.hash);
  if (!currentBase || !destBase) {
    // can't determine if we can show warning
    return WarningCheckResult.PASS;
  }

  const ageDiff = currentBase.date.valueOf() - destBase.date.valueOf();
  if (ageDiff < MAX_AGE_CUTOFF_MS) {
    // Either destination base is within time limit or destination base is newer than the current base.
    // No need to warn.
    return WarningCheckResult.PASS;
  }

  const confirmed = await showConfirmation({
    title: t('Go to Older Commit?'),
    message: t(
      Internal.warnAboutOldGotoReason ??
        'The destination commit is $age older than the current commit. ' +
          "Going here may be slow. It's often faster to rebase the commit to a newer base before going. " +
          'Do you want to `goto` anyway?',
      {
        replace: {
          $age: relativeDate(destBase.date, {reference: currentBase.date, useRelativeForm: true}),
        },
      },
    ),
    confirmLabel: t('Goto Anyway'),
  });
  return confirmed ? WarningCheckResult.BYPASS : WarningCheckResult.FAIL;
}

function WarnAboutRebaseOffWarmModalContent({
  message,
  cancelLabel,
  continueLabel,
  optOutLabel,
  rebaseOntoWarmLabel,
  showRebaseOntoWarm,
  returnResultAndDismiss,
}: {
  message: string;
  cancelLabel: string;
  continueLabel: string;
  optOutLabel: string;
  rebaseOntoWarmLabel: string;
  showRebaseOntoWarm: boolean;
  returnResultAndDismiss: (data: {action: string; optOut: boolean}) => void;
}) {
  const result = (action: string, optOut = false) => returnResultAndDismiss({action, optOut});
  return (
    <>
      <div id="use-modal-message">{message}</div>
      <div className="use-modal-buttons-with-opt-out">
        <Button onClick={() => result(optOutLabel, true)}>{optOutLabel}</Button>
        <div className="use-modal-buttons-actions">
          <Button onClick={() => result(cancelLabel)}>{cancelLabel}</Button>
          <Button onClick={() => result(continueLabel)} className="warn-button-caution">
            <Icon icon="warning" />
            {continueLabel}
          </Button>
          {showRebaseOntoWarm && (
            <Button kind="primary" onClick={() => result(rebaseOntoWarmLabel)}>
              {rebaseOntoWarmLabel}
            </Button>
          )}
        </div>
      </div>
    </>
  );
}

async function maybeWarnAboutRebaseOffWarm(
  dest: CommitInfo,
  options?: {showRebaseOntoWarm?: boolean},
): Promise<WarningCheckResult> {
  const isRebaseOffWarmWarningEnabled = readAtom(rebaseOffWarmWarningEnabled);
  if (!isRebaseOffWarmWarningEnabled || dest.stableCommitMetadata == null) {
    return WarningCheckResult.PASS;
  }

  const dag = readAtom(dagWithPreviews);
  const src = findPublicBaseAncestor(dag);
  const destBase = findPublicBaseAncestor(dag, dest.hash);

  if (!src || !destBase) {
    return WarningCheckResult.PASS;
  }

  if (Internal.maybeWarnAboutRebaseOffWarm?.(src, destBase)) {
    const showRebaseOntoWarm = options?.showRebaseOntoWarm === true && dest.phase !== 'public';
    const stackRootHash = showRebaseOntoWarm
      ? dag
          ?.roots(dag.ancestors(dest.hash, {within: dag.draft()}))
          .toArray()
          .at(0)
      : undefined;
    const isStack =
      stackRootHash != null &&
      (dag?.descendants(stackRootHash, {within: dag.draft()}).size ?? 0) > 1;
    const continueLabel = t('Continue Anyway');
    const cancelLabel = t('Cancel');
    const optOutLabel = t('Opt Out of Future Warnings');
    const rebaseOntoWarmLabel = isStack ? t('Rebase Stack onto Warm') : t('Rebase onto Warm');
    const shortHash = dest.hash.slice(0, 12);
    const message = showRebaseOntoWarm
      ? t(
          "You're moving off of a warmed up commit which will degrade OD performance.\n" +
            (isStack
              ? `Would you like to rebase the destination stack ${shortHash} onto the warm commit before moving instead?`
              : `Would you like to rebase the destination commit ${shortHash} onto the warm commit before moving instead?`),
        )
      : t(
          "You're moving off of a warmed up commit which will degrade OD performance.\n" +
            'Rebase your changes onto the warm commit instead with `sl rebase -s <rev> -d .`\n' +
            'Do you want to continue anyway?',
        );
    const answer = await showModal<{action: string; optOut: boolean}>({
      type: 'custom',
      title: <T>Move off Warm Commit</T>,
      component: ({returnResultAndDismiss}) => (
        <WarnAboutRebaseOffWarmModalContent
          message={message}
          showRebaseOntoWarm={showRebaseOntoWarm}
          cancelLabel={cancelLabel}
          continueLabel={continueLabel}
          optOutLabel={optOutLabel}
          rebaseOntoWarmLabel={rebaseOntoWarmLabel}
          returnResultAndDismiss={returnResultAndDismiss}
        />
      ),
    });
    const userEnv = (await Internal.getDevEnvType?.()) ?? 'NotImplemented';
    const cwd = readAtom(repoRelativeCwd);
    tracker.track('WarnAboutRebaseOffWarm', {
      extras: {
        userAction: answer?.action,
        envType: userEnv,
        cwd,
      },
    });
    if (answer?.optOut) {
      writeAtom(rebaseOffWarmWarningEnabled, false);
      // If opting out, let the operation proceed regardless of which button was pressed
      return WarningCheckResult.PASS;
    }
    if (answer?.action === continueLabel) {
      return WarningCheckResult.BYPASS;
    }
    if (showRebaseOntoWarm && answer?.action === rebaseOntoWarmLabel) {
      // Awaited so the follow-up goto resolves against the post-rebase dag.
      await runOperation(
        new RebaseOperation(
          succeedableRevset(stackRootHash ?? dest.hash),
          succeedableRevset(src.hash),
        ),
      );
      return WarningCheckResult.PASS;
    }
    return WarningCheckResult.FAIL;
  }

  return WarningCheckResult.PASS;
}

async function maybeWarnAboutRebaseOntoMaster(commit: CommitInfo): Promise<WarningCheckResult> {
  const isRebaseOntoMasterWarningEnabled = readAtom(rebaseOntoMasterWarningEnabled);
  if (!isRebaseOntoMasterWarningEnabled) {
    return WarningCheckResult.PASS;
  }

  const dag = readAtom(dagWithPreviews);
  const src = findPublicBaseAncestor(dag);
  const destBase = findPublicBaseAncestor(dag, commit.hash);

  if (!src || !destBase) {
    // can't determine if we can show warning
    return WarningCheckResult.PASS;
  }

  if (Internal.maybeWarnAboutRebaseOntoMaster?.(src, destBase)) {
    const buttons = [
      t('Opt Out of Future Warnings'),
      {label: t('Cancel'), primary: true},
      t('Continue Anyway'),
    ];
    const answer = await showModal({
      type: 'confirm',
      buttons,
      title: <T>Rebase onto Master Warning</T>,
      message: t(
        Internal.warnAboutRebaseOntoMasterReason ??
          'You are about to rebase directly onto master/main. ' +
            'This is generally not recommended as it can cause unexpected failures and slower builds. ' +
            'Consider rebasing onto a stable or warm branch instead. ' +
            'Do you want to continue anyway?',
      ),
    });
    const userEnv = (await Internal.getDevEnvType?.()) ?? 'NotImplemented';
    const cwd = readAtom(repoRelativeCwd);
    tracker.track('WarnAboutRebaseOntoMaster', {
      extras: {
        userAction: answer,
        envType: userEnv,
        cwd,
      },
    });
    if (answer === buttons[0]) {
      writeAtom(rebaseOntoMasterWarningEnabled, false);
      return WarningCheckResult.PASS;
    }
    return answer === buttons[2] ? WarningCheckResult.BYPASS : WarningCheckResult.FAIL;
  }

  return WarningCheckResult.PASS;
}

async function gotoAction(runOperation: ReturnType<typeof useRunOperation>, commit: CommitInfo) {
  const shouldProceed = await runWarningChecks([
    () => maybeWarnAboutRebaseOntoMaster(commit),
    () => maybeWarnAboutOldDestination(commit),
    () => maybeWarnAboutRebaseOffWarm(commit, {showRebaseOntoWarm: true}),
  ]);

  if (!shouldProceed) {
    return;
  }

  const dest =
    // If the commit has a remote bookmark, use that instead of the hash. This is easier to read in the command history
    // and works better with optimistic state
    commit.remoteBookmarks.length > 0
      ? succeedableRevset(commit.remoteBookmarks[0])
      : latestSuccessorUnlessExplicitlyObsolete(commit);
  runOperation(new GotoOperation(dest));
  // Instead of propagating, ensure we remove the selection, so we view the new head commit by default
  // (since the head commit is the default thing shown in the sidebar)
  writeAtom(selectedCommits, new Set());
}

const ObsoleteTip = React.memo(ObsoleteTipInner);

/**
 * Runs a series of validation checks sequentially. Returns true if all checks pass
 * or the user manually bypassed a warning, otherwise returns false if any check fails.
 */
async function runWarningChecks(
  checks: Array<() => Promise<WarningCheckResult>>,
): Promise<boolean> {
  for (const check of checks) {
    // eslint-disable-next-line no-await-in-loop
    const result = await check();
    if (result !== WarningCheckResult.PASS) {
      return result === WarningCheckResult.BYPASS;
    }
  }
  return true;
}

async function handleRebaseConfirmation(
  commit: CommitInfo,
  handlePreviewedOperation: (cancel: boolean) => void,
): Promise<void> {
  const shouldProceed = await runWarningChecks([
    () => maybeWarnAboutRebaseOntoMaster(commit),
    () => maybeWarnAboutDistantRebase(commit),
    () => maybeWarnAboutRebaseOffWarm(commit),
  ]);

  if (!shouldProceed) {
    return;
  }

  handlePreviewedOperation(/* cancel */ false);

  const dag = readAtom(dagWithPreviews);
  const onto = dag.get(commit.parents[0]);
  if (onto) {
    tracker.track('ConfirmDragAndDropRebase', {
      extras: {
        remoteBookmarks: onto.remoteBookmarks,
        locations: onto.stableCommitMetadata?.map(s => s.value),
      },
    });
  }
}

async function maybeWarnAboutDistantRebase(commit: CommitInfo): Promise<WarningCheckResult> {
  const isDistantRebaseWarningEnabled = readAtom(distantRebaseWarningEnabled);
  if (!isDistantRebaseWarningEnabled) {
    return WarningCheckResult.PASS;
  }
  const dag = readAtom(dagWithPreviews);
  const onto = dag.get(commit.parents[0]);
  if (!onto) {
    return WarningCheckResult.PASS; // If there's no target commit, proceed without warning
  }
  const currentBase = findPublicBaseAncestor(dag);
  const destBase = findPublicBaseAncestor(dag, onto.hash);
  if (!currentBase || !destBase) {
    // can't determine if we can show warning
    return WarningCheckResult.PASS;
  }

  if (Internal.maybeWarnAboutDistantRebase?.(currentBase, destBase)) {
    const buttons = [
      t('Opt Out of Future Warnings'),
      {label: t('Cancel'), primary: true},
      t('Rebase Anyway'),
    ];
    const answer = await showModal({
      type: 'confirm',
      buttons,
      title: <T>Distant Rebase Warning</T>,
      message: t(
        Internal.warnAboutDistantRebaseReason ??
          'The target commit is $age away from your current commit. ' +
            'Rebasing across a large time gap may cause slower builds and performance. ' +
            "It's recommended to rebase the destination commit(s) to the nearest stable or warm commit first and then attempt this rebase. " +
            'Do you want to `rebase` anyway?',
        {
          replace: {
            $age: relativeDate(destBase.date, {reference: currentBase.date, useRelativeForm: true}),
          },
        },
      ),
    });
    const userEnv = (await Internal.getDevEnvType?.()) ?? 'NotImplemented';
    const cwd = readAtom(repoRelativeCwd);
    tracker.track('WarnAboutDistantRebase', {
      extras: {
        userAction: answer,
        envType: userEnv,
        cwd,
      },
    });
    if (answer === buttons[0]) {
      writeAtom(distantRebaseWarningEnabled, false);
      return WarningCheckResult.PASS;
    }
    return answer === buttons[2] ? WarningCheckResult.BYPASS : WarningCheckResult.FAIL;
  }

  return WarningCheckResult.PASS;
}
