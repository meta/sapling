/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import {Checkbox} from 'isl-components/Checkbox';
import {Subtle} from 'isl-components/Subtle';
import {Tooltip} from 'isl-components/Tooltip';
import {useAtom} from 'jotai';
import {Internal} from '../Internal';
import {T, t} from '../i18n';
import {localStorageBackedAtom} from '../jotaiUtils';

export const shouldAutoResolveAllBeforeContinue = localStorageBackedAtom<boolean>(
  'isl.auto-resolve-before-continue',
  // OSS doesn't typically use merge drivers, so `sl resolve --all` would be added overhead for little gain.
  // You can still configure this in settings if you want.
  Internal.autoRunMergeDriversByDefault === true,
  {portable: true},
);

export function AutoResolveSettingCheckbox({subtle}: {subtle?: boolean}) {
  const [shouldAutoResolve, setShouldAutoResolve] = useAtom(shouldAutoResolveAllBeforeContinue);

  const label = <T>Auto-run Merge Drivers</T>;
  return (
    <Tooltip
      title={t(
        'Whether to run `sl resolve --all` before `sl continue`. ' +
          'This runs automated merge drivers to regenerate generated files.\n' +
          'This is usually needed to finish a merge, but merge drivers can be slow.',
      )}>
      <Checkbox checked={shouldAutoResolve} onChange={setShouldAutoResolve}>
        {subtle ? <Subtle>{label}</Subtle> : label}
      </Checkbox>
    </Tooltip>
  );
}

export const shouldPartialAbort = localStorageBackedAtom<boolean>('isl.partial-abort', false, {
  portable: true,
});

export function PartialAbortSettingCheckbox({subtle}: {subtle?: boolean}) {
  const [isPartialAbort, setShouldPartialAbort] = useAtom(shouldPartialAbort);

  const label = <T>Keep Rebased Commits on Abort</T>;
  return (
    <Tooltip title={t('Keep already rebased commits when aborting a rebase operation.')}>
      <Checkbox checked={isPartialAbort} onChange={setShouldPartialAbort}>
        {subtle ? <Subtle>{label}</Subtle> : label}
      </Checkbox>
    </Tooltip>
  );
}
