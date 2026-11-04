/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import {ErrorNotice} from 'isl-components/ErrorNotice';
import {Icon} from 'isl-components/Icon';
import {useAtom, useAtomValue} from 'jotai';
import {lazy, Suspense, useEffect} from 'react';
import {cn} from 'shared/cn';
import {Center, Row} from '../../ComponentUtils';
import {Modal} from '../../Modal';
import {t} from '../../i18n';
import css from './EditStackModal.module.css';
import {editingStackIntentionHashes, loadingStackState} from './stackEditState';

// The stack editing UI is only needed once a stack is being edited,
// so keep it out of the initial bundle.
const importLoadedEditStackModal = () => import('./LoadedEditStackModals');
const LoadedEditStackModal = lazy(importLoadedEditStackModal);

/// Show a <Modal /> when editing a stack.
export function MaybeEditStackModal() {
  const loadingState = useAtomValue(loadingStackState);
  const [[stackIntention, stackHashes], setStackIntention] = useAtom(editingStackIntentionHashes);

  const isEditing = stackHashes.size > 0;
  const isLoaded = isEditing && loadingState.state === 'hasValue';

  useEffect(() => {
    if (isEditing) {
      // Start fetching the UI while the stack is still loading.
      importLoadedEditStackModal();
    }
  }, [isEditing]);

  const loadingModal = (
    <Modal
      dataTestId="edit-stack-loading"
      dismiss={() => {
        // allow dismissing in loading state in case it gets stuck
        setStackIntention(['general', new Set()]);
      }}>
      <Center
        className={cn(
          (stackIntention === 'general' || stackIntention === 'absorb') && css.container,
          stackIntention === 'split' && 'interactive-split',
          css.loading,
        )}>
        {loadingState.state === 'hasError' ? (
          <ErrorNotice error={new Error(loadingState.error)} title={t('Loading stack failed')} />
        ) : (
          <Row>
            <Icon icon="loading" size="M" />
            {(loadingState.state === 'loading' && loadingState.message) ?? null}
          </Row>
        )}
      </Center>
    </Modal>
  );

  return isLoaded ? (
    <Suspense fallback={loadingModal}>
      <LoadedEditStackModal intention={stackIntention} />
    </Suspense>
  ) : isEditing ? (
    loadingModal
  ) : null;
}
