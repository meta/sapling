/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import type {Tracker} from 'isl-server/src/analytics/tracker';
import type {UseUncommittedSelection} from './partialSelection';
import type {CommitInfo, Diagnostic, DiagnosticAllowlist} from './types';

import {Checkbox} from 'isl-components/Checkbox';
import {Column, Row} from 'isl-components/Flex';
import {Icon} from 'isl-components/Icon';
import {Subtle} from 'isl-components/Subtle';
import {Tooltip} from 'isl-components/Tooltip';
import {useAtom} from 'jotai';
import {basename, timeout} from 'shared/utils';
import serverAPI from './ClientToServerAPI';
import {Collapsable} from './Collapsable';
import css from './Diagnostics.module.css';
import {Internal} from './Internal';
import {tracker} from './analytics';
import {getFeatureFlag} from './featureFlags';
import {T, t} from './i18n';
import {localStorageBackedAtom, readAtom} from './jotaiUtils';
import platform from './platform';
import {uncommittedChangesWithPreviews} from './previews';
import {showModal} from './useModal';

export const shouldWarnAboutDiagnosticsAtom = localStorageBackedAtom<boolean>(
  'isl.warn-about-diagnostics',
  true,
  {portable: true},
);

const DIAGNOSTICS_TIMEOUT_MS = 10_000;

const hideNonBlockingDiagnosticsAtom = localStorageBackedAtom<boolean>(
  'isl.hide-non-blocking-diagnostics',
  true,
  {portable: true},
);

export function isBlockingDiagnostic(
  d: Diagnostic,
  /** Many diagnostics are low-quality and don't reflect what would appear on CI.
   * Start with an allowlist while we validate which signals are worthwhile. */
  allowlistedCodesBySource: undefined | DiagnosticAllowlist = Internal.allowlistedDiagnosticCodes ??
    undefined,
): boolean {
  if (allowlistedCodesBySource == null) {
    // In OSS, let's assume all errors are blocking.
    return true;
  }
  if (d.severity !== 'error' && d.severity !== 'warning') {
    return false;
  }
  if (allowlistedCodesBySource == null) {
    return true;
  }
  // source/code may be missing, but we still want to route that through the allowlist
  const source = d.source ?? 'undefined';
  const code = d.code ?? 'undefined';
  const relevantAllowlist = allowlistedCodesBySource.get(d.severity)?.get(source);
  return (
    relevantAllowlist != null &&
    (relevantAllowlist.allow
      ? relevantAllowlist.allow.has(code) === true
      : relevantAllowlist.block.has(code) === false)
  );
}

function isErrorDiagnosticToLog(d: Diagnostic): boolean {
  return d.severity === 'error';
}

/** Render diagnostic to a string, in the format `Source(Code): Snippet of error message` */
function previewDiagnostic(diagnostic: Diagnostic | undefined) {
  return diagnostic != null
    ? `${diagnostic.source}(${diagnostic.code}): ${diagnostic?.message.slice(0, 100)}`
    : undefined;
}

/**
 * Check IDE diagnostics for files that will be commit/amended/submitted,
 * to confirm if they intended the errors.
 */
export async function confirmNoBlockingDiagnostics(
  /** Check diagnostics for these selected files. */
  selection: UseUncommittedSelection,
  /** If provided, warn for changes to files in this commit. Used when checking diagnostics when amending a commit. */
  commit?: CommitInfo,
): Promise<boolean> {
  if (!readAtom(shouldWarnAboutDiagnosticsAtom)) {
    return true;
  }
  if (platform.platformName === 'vscode') {
    const allFiles = new Set<string>();
    for (const file of readAtom(uncommittedChangesWithPreviews)) {
      if (selection.isFullyOrPartiallySelected(file.path)) {
        allFiles.add(file.path);
      }
    }
    for (const filePath of commit?.filePathsSample ?? []) {
      allFiles.add(filePath);
    }

    serverAPI.postMessage({
      type: 'platform/checkForDiagnostics',
      paths: [...allFiles],
    });
    const checked = await Promise.race([
      Promise.all([
        serverAPI.nextMessageMatching('platform/gotDiagnostics', () => true),
        getFeatureFlag(
          Internal.featureFlags?.ShowPresubmitDiagnosticsWarning,
          /* enable this feature in OSS */ true,
        ),
      ]).catch(() => null),
      timeout(() => null, DIAGNOSTICS_TIMEOUT_MS),
    ]);
    if (checked == null) {
      tracker.track('DiagnosticsCheckSkipped');
      return true;
    }
    const [result, enabled] = checked;
    if (result.diagnostics.size > 0) {
      const allDiagnostics = [...result.diagnostics.values()];
      const allBlockingErrors = allDiagnostics
        .map(value => value.filter(d => isBlockingDiagnostic(d)))
        .flat();
      const totalErrors = allBlockingErrors.length;

      // It's useful to track even the diagnostics that are filtered out, to refine the allowlist in the future
      const unfilteredErrors = allDiagnostics
        .map(value => value.filter(isErrorDiagnosticToLog))
        .flat();

      const totalDiagnostics = allDiagnostics.flat().length;

      const firstError = allBlockingErrors[0];
      const firstUnfilteredError = unfilteredErrors[0];

      const childTracker = tracker.trackAsParent('DiagnosticsConfirmationOpportunity', {
        extras: {
          shown: enabled,
          unfilteredErrorCodes: [...new Set(unfilteredErrors.map(d => `${d.source}(${d.code})`))],
          filteredErrorCodes: [...new Set(allBlockingErrors.map(d => `${d.source}(${d.code})`))],
          sampleMessage: previewDiagnostic(firstError),
          unfilteredSampleMessage: previewDiagnostic(firstUnfilteredError),
          totalUnfilteredErrors: unfilteredErrors.length,
          totalErrors,
          totalDiagnostics,
        },
      });

      if (!enabled) {
        return true;
      }

      if (totalErrors > 0) {
        const buttons = [{label: 'Cancel'}, {label: 'Continue', primary: true}] as const;
        const shouldContinue =
          (await showModal({
            type: 'confirm',
            title: (
              <Row>
                <T count={totalErrors}>codeIssuesFound</T>
                <Tooltip
                  title={t(
                    'Error-severity issues are typically land blocking and should be resolved before submitting for code review.\n\n' +
                      'Errors shown here are best-effort and not necessarily comprehensive.',
                  )}>
                  <Icon icon="info" />
                </Tooltip>
              </Row>
            ),
            message: (
              <DiagnosticsList
                diagnostics={[...result.diagnostics.entries()]}
                tracker={childTracker}
              />
            ),
            buttons,
          })) === buttons[1];

        childTracker.track('DiagnosticsConfirmationAction', {
          extras: {
            action: shouldContinue ? 'continue' : 'cancel',
          },
        });

        return shouldContinue;
      }
    }
  }
  return true;
}

function DiagnosticsList({
  diagnostics,
  tracker,
}: {
  diagnostics: Array<[string, Array<Diagnostic>]>;
  tracker: Tracker<{parentId: string}>;
}) {
  const [hideNonBlocking, setHideNonBlocking] = useAtom(hideNonBlockingDiagnosticsAtom);
  const [shouldWarn, setShouldWarn] = useAtom(shouldWarnAboutDiagnosticsAtom);
  return (
    <>
      <Column alignStart className={css.allDiagnostics}>
        {diagnostics.map(([filepath, diagnostics]) => {
          const sortedDiagnostics = [...diagnostics].filter(d =>
            hideNonBlocking ? isBlockingDiagnostic(d) : true,
          );
          sortedDiagnostics.sort((a, b) => {
            return severityComparator(a) - severityComparator(b);
          });
          if (sortedDiagnostics.length === 0) {
            return null;
          }
          return (
            <Column key={filepath} alignStart>
              <Collapsable
                startExpanded
                title={
                  <Row>
                    <span>{basename(filepath)}</span>
                    <Subtle>{filepath}</Subtle>
                  </Row>
                }>
                <Column alignStart className={css.diagnosticList}>
                  {sortedDiagnostics.map((d, i) => (
                    <Row
                      role="button"
                      tabIndex={0}
                      key={i}
                      className={css.diagnosticRow}
                      onClick={() => {
                        platform.openFile(filepath, {line: d.range.startLine + 1});
                        tracker.track('DiagnosticsConfirmationAction', {
                          extras: {action: 'openFile'},
                        });
                      }}>
                      {iconForDiagnostic(d)}
                      <span>{d.message}</span>
                      {d.source && (
                        <Subtle className={css.nowrap}>
                          {d.source}
                          {d.code ? `(${d.code})` : null}
                        </Subtle>
                      )}{' '}
                      <Subtle className={css.nowrap}>
                        [Ln {d.range.startLine}, Col {d.range.startCol}]
                      </Subtle>
                    </Row>
                  ))}
                </Column>
              </Collapsable>
            </Column>
          );
        })}
      </Column>
      <Row className={css.confirmCheckbox}>
        <Checkbox
          checked={!shouldWarn}
          onChange={checked => {
            setShouldWarn(!checked);
            if (checked) {
              tracker.track('DiagnosticsConfirmationAction', {extras: {action: 'dontAskAgain'}});
            }
          }}>
          <T>Don't ask again</T>
        </Checkbox>
        <Tooltip
          title={t(
            "Only 'error' severity issues known to cause problems will cause this dialog to appear, but less severe issues can still be shown here. This option hides these non-blocking issues.",
          )}>
          <Checkbox checked={hideNonBlocking} onChange={setHideNonBlocking}>
            <T>Hide non-blocking issues</T>
          </Checkbox>
        </Tooltip>
      </Row>
    </>
  );
}

function severityComparator(a: Diagnostic) {
  switch (a.severity) {
    case 'error':
      return 0;
    case 'warning':
      return 1;
    case 'info':
      return 2;
    case 'hint':
      return 3;
  }
}

function iconForDiagnostic(d: Diagnostic): React.ReactNode {
  switch (d.severity) {
    case 'error':
      return <Icon icon="error" color="red" />;
    case 'warning':
      return <Icon icon="warning" color="yellow" />;
    case 'info':
      return <Icon icon="info" />;
    case 'hint':
      return <Icon icon="info" />;
  }
}
