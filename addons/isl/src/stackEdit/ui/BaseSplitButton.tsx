/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import type {TrackEventName} from 'isl-server/src/analytics/eventNames';
import type {CommitInfo} from '../../types';

import {Button} from 'isl-components/Button';
import {Tooltip} from 'isl-components/Tooltip';
import {useAtomValue, useSetAtom} from 'jotai';
import {tracker} from '../../analytics';
import {t} from '../../i18n';
import {uncommittedChangesWithPreviews} from '../../previews';
import {useConfirmUnsavedEditsBeforeSplit} from './ConfirmUnsavedEditsBeforeSplit';
import {bumpStackEditMetric, editingStackIntentionHashes} from './stackEditState';

export interface BaseSplitButtonProps {
  /** The commit to split */
  commit: CommitInfo;
  /** The event name to track when the button is clicked */
  trackerEventName: TrackEventName;
  /** Function to call after the split action is initiated */
  onSplitInitiated?: () => void;
  /** Whether to bump the "splitFromSuggestion" metric */
  bumpSplitFromSuggestion?: boolean;
  /** Children to render inside the button */
  children: React.ReactNode;
}

/** Base button component for initiating split operations */
export function BaseSplitButton({
  commit,
  trackerEventName,
  onSplitInitiated,
  bumpSplitFromSuggestion = false,
  children,
  ...buttonProps
}: BaseSplitButtonProps & React.ComponentProps<typeof Button>) {
  const confirmUnsavedEditsBeforeSplit = useConfirmUnsavedEditsBeforeSplit();
  const setEditStackIntentionHashes = useSetAtom(editingStackIntentionHashes);

  const uncommittedChanges = useAtomValue(uncommittedChangesWithPreviews);
  const hasUncommittedChanges = uncommittedChanges.length > 0;

  const onClick = async (e: React.MouseEvent) => {
    if (!(await confirmUnsavedEditsBeforeSplit([commit], 'split'))) {
      return;
    }
    setEditStackIntentionHashes(['split', new Set([commit.hash])]);
    if (bumpSplitFromSuggestion) {
      bumpStackEditMetric('splitFromSuggestion');
    }
    tracker.track(trackerEventName);
    if (onSplitInitiated) {
      onSplitInitiated();
    }
    e.stopPropagation();
  };

  return (
    <Tooltip
      title={hasUncommittedChanges ? t('Cannot currently split with uncommitted changes') : ''}
      trigger={hasUncommittedChanges ? 'hover' : 'disabled'}>
      <Button onClick={onClick} disabled={hasUncommittedChanges} {...buttonProps}>
        {children}
      </Button>
    </Tooltip>
  );
}
