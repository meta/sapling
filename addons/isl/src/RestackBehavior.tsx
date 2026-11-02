/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import {Dropdown} from 'isl-components/Dropdown';
import {Tooltip} from 'isl-components/Tooltip';
import {useAtom} from 'jotai';
import {t, T} from './i18n';
import {localStorageBackedAtom} from './jotaiUtils';

export enum AmendRestackBehavior {
  ALWAYS = 'always',
  NEVER = 'never',
  NO_CONFLICT = 'no-conflict',
}
const DEFAULT_AMEND_RESTACK_BEHAVIOR = AmendRestackBehavior.ALWAYS;

/** This is controlled by the underlying Sapling config for this feature.
 * This way, we don't pass any additional data to sl to run amend,
 * and this one setting controls this behavior everywhere.
 * We merely give the setting a UI since it's common to customize.
 */
export const restackBehaviorAtom = localStorageBackedAtom<AmendRestackBehavior>(
  'isl.amend-autorestack',
  DEFAULT_AMEND_RESTACK_BEHAVIOR,
  {
    portable: true,
    validate: value => Object.values(AmendRestackBehavior).includes(value as AmendRestackBehavior),
  },
);

export function RestackBehaviorSetting() {
  const [value, setValue] = useAtom(restackBehaviorAtom);
  return (
    <Tooltip
      title={t(
        'Whether to restack (rebase) child commits when amending a commit in a stack. ' +
          'By default, commits are always restacked, even if it introduces merge conflicts. ',
      )}>
      <div className="dropdown-container setting-inline-dropdown">
        <label htmlFor="restack-setting">
          <T>Restack on Amend</T>
        </label>
        <Dropdown
          value={value}
          onChange={event => setValue(event.currentTarget.value as AmendRestackBehavior)}
          options={
            [
              {value: AmendRestackBehavior.NO_CONFLICT, name: t('No Conflict')},
              {value: AmendRestackBehavior.ALWAYS, name: t('Always')},
              {value: AmendRestackBehavior.NEVER, name: t('Never')},
            ] as Array<{value: AmendRestackBehavior; name: string}>
          }
        />
      </div>
    </Tooltip>
  );
}
