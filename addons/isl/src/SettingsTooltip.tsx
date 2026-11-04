/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import type {ThemePreference} from './theme';
import type {PreferredSubmitCommand} from './types';

import {Button} from 'isl-components/Button';
import {Checkbox} from 'isl-components/Checkbox';
import {Dropdown} from 'isl-components/Dropdown';
import {Icon} from 'isl-components/Icon';
import {Kbd} from 'isl-components/Kbd';
import {KeyCode, Modifier} from 'isl-components/KeyboardShortcuts';
import {Subtle} from 'isl-components/Subtle';
import {Tooltip} from 'isl-components/Tooltip';
import {useAtom, useAtomValue} from 'jotai';
import {Suspense} from 'react';
import {nullthrows, tryJsonParse} from 'shared/utils';
import {clickToOpenDiffViewAtom} from './ChangedFile';
import {showWorktreeLabels} from './CheckedOutElsewhere';
import {
  copyCommitHashFormatAtom,
  distantRebaseWarningEnabled,
  rebaseOffWarmWarningEnabled,
  rebaseOntoMasterWarningEnabled,
} from './Commit';
import {condenseObsoleteStacks, scrollToYouAreHereOnOpen} from './CommitTreeList';
import {Column, Row} from './ComponentUtils';
import {confirmShouldSubmitEnabledAtom} from './ConfirmSubmitStack';
import {DropdownField, DropdownFields} from './DropdownFields';
import {useShowKeyboardShortcutsHelp} from './ISLShortcuts';
import {Link} from './Link';
import {RestackBehaviorSetting} from './RestackBehavior';
import {Setting} from './Setting';
import {disableUnsavedFilesWarning} from './UnsavedFiles';
import {
  currentExperimentalFeaturesList,
  hasExperimentalFeatures,
} from './atoms/experimentalFeatureAtoms';
import {codeReviewProvider} from './codeReview/CodeReviewInfo';
import {showDiffNumberConfig} from './codeReview/DiffBadge';
import {SubmitAsDraftCheckbox} from './codeReview/DraftCheckbox';
import {showAiReviewingBadge} from './codeReview/aiReviewBadgeState';
import {
  branchPRsSupported,
  experimentalBranchPRsEnabled,
  overrideDisabledSubmitModes,
} from './codeReview/github/branchPrState';
import {debugToolsEnabledState} from './debug/DebugToolsState';
import {commitInfoLocationAtom, type CommitInfoLocationWithAuto} from './drawerState';
import {externalMergeToolAtom} from './externalMergeTool';
import {t, T} from './i18n';
import {configBackedAtom} from './jotaiUtils';
import {AutoResolveSettingCheckbox} from './mergeConflicts/state';
import {SetConfigOperation} from './operations/SetConfigOperation';
import {useRunOperation} from './operationsState';
import platform from './platform';
import {irrelevantCwdDisplayModeAtom} from './repositoryData';
import {renderCompactAtom, useZoomShortcut, zoomUISettingAtom} from './responsive';
import {mainCommandName, repositoryInfo} from './serverAPIState';
import {themePreferenceState, useThemeShortcut} from './theme';

import {Internal} from './Internal';
import './SettingsTooltip.css';
import {enableSaplingDebugFlag, enableSaplingVerboseFlag} from './atoms/debugToolAtoms';

export const showAuthoredDiffsConfig = configBackedAtom<boolean>('isl.show-authored-diffs', true);

export function SettingsGearButton() {
  useThemeShortcut();
  useZoomShortcut();
  const showShortcutsHelp = useShowKeyboardShortcutsHelp();
  return (
    <Tooltip
      trigger="click"
      component={dismiss => (
        <SettingsDropdown dismiss={dismiss} showShortcutsHelp={showShortcutsHelp} />
      )}
      group="topbar"
      placement="bottom">
      <Button icon data-testid="settings-gear-button">
        <Icon icon="gear" />
      </Button>
    </Tooltip>
  );
}

function SettingsDropdown({
  dismiss,
  showShortcutsHelp,
}: {
  dismiss: () => unknown;
  showShortcutsHelp: () => unknown;
}) {
  const [themePreference, setThemePreference] = useAtom(themePreferenceState);
  const [repoInfo, setRepoInfo] = useAtom(repositoryInfo);
  const runOperation = useRunOperation();
  const [showDiffNumber, setShowDiffNumber] = useAtom(showDiffNumberConfig);
  const [showAuthoredDiffs, setShowAuthoredDiffs] = useAtom(showAuthoredDiffsConfig);
  const branchPRsAreSupported = useAtomValue(branchPRsSupported);
  return (
    <DropdownFields title={<T>Settings</T>} icon="gear" data-testid="settings-dropdown">
      <Button
        style={{justifyContent: 'center', gap: 0}}
        icon
        onClick={() => {
          dismiss();
          showShortcutsHelp();
        }}>
        <T
          replace={{
            $shortcut: <Kbd keycode={KeyCode.QuestionMark} modifiers={[Modifier.SHIFT]} />,
          }}>
          View Keyboard Shortcuts - $shortcut
        </T>
      </Button>
      {platform.theme != null ? null : (
        <Setting title={<T>Theme</T>}>
          <Dropdown
            options={
              [
                {value: 'light', name: 'Light'},
                {value: 'dark', name: 'Dark'},
                {value: 'system', name: 'System'},
              ] as Array<{value: ThemePreference; name: string}>
            }
            value={themePreference}
            onChange={event => setThemePreference(event.currentTarget.value as ThemePreference)}
          />
          <div style={{marginTop: 'var(--pad)'}}>
            <Subtle>
              <T>Toggle: </T>
              <Kbd keycode={KeyCode.T} modifiers={[Modifier.ALT]} />
            </Subtle>
          </div>
        </Setting>
      )}

      <Setting title={<T>UI Scale</T>}>
        <ZoomUISetting />
      </Setting>
      <Setting title={<T>Commits</T>}>
        <Column alignStart>
          <RenderCompactSetting />
          <CondenseObsoleteSetting />
          <ScrollToYouAreHereSetting />
          <ShowWorktreeLabelsSetting />
          <RebaseOffWarmWarningSetting />
          <DistantRebaseWarningSetting />
          <RebaseOntoMasterWarningSetting />
          <DeemphasizeIrrelevantCommitsSetting />
          <CopyCommitHashFormatSetting />
        </Column>
      </Setting>
      <Setting title={<T>Conflicts</T>}>
        <AutoResolveSettingCheckbox />
        <RestackBehaviorSetting />
      </Setting>
      {/* TODO: enable this setting when there is actually a chocie to be made here. */}
      {/* <Setting
        title={<T>Language</T>}
        description={<T>Locale for translations used in the UI. Currently only en supported.</T>}>
        <Dropdown value="en" options=['en'] />
      </Setting> */}
      {repoInfo == null ? (
        <Icon icon="loading" />
      ) : repoInfo.codeReviewSystem.type === 'github' ? (
        <Setting
          title={<T>Preferred Code Review Submit Method</T>}
          description={
            <>
              <T>How to submit code for code review on GitHub.</T>{' '}
              {/* TODO: update this to document branchign PRs */}
              <Link href="https://sapling-scm.com/docs/git/intro#pull-requests">
                <T>Learn More</T>
              </Link>
            </>
          }>
          <Dropdown
            value={repoInfo.preferredSubmitCommand ?? 'not set'}
            options={(repoInfo.preferredSubmitCommand == null
              ? [{value: 'not set', name: '(not set)'}]
              : []
            ).concat([
              {value: 'ghstack', name: 'sl ghstack (stacked PRs)'},
              {value: 'pr', name: 'sl pr (stacked PRs)'},
              ...(branchPRsAreSupported ? [{value: 'push', name: 'sl push (branching PR)'}] : []),
            ])}
            onChange={event => {
              const value = (event as React.FormEvent<HTMLSelectElement>).currentTarget.value as
                PreferredSubmitCommand | 'not set';
              if (value === 'not set') {
                return;
              }

              runOperation(
                new SetConfigOperation('local', 'github.preferred_submit_command', value),
              );
              setRepoInfo(info => ({...nullthrows(info), preferredSubmitCommand: value}));
            }}
          />
        </Setting>
      ) : null}
      <Setting title={<T>Code Review</T>}>
        <div className="multiple-settings">
          <Checkbox
            checked={showDiffNumber}
            onChange={checked => {
              setShowDiffNumber(checked);
            }}>
            <T>Show copyable Diff / Pull Request numbers inline for each commit</T>
          </Checkbox>
          <ConfirmSubmitStackSetting />
          <SubmitAsDraftCheckbox forceShow />
          {Internal.showAuthoredDiffsOption && (
            <Tooltip
              title={t(
                'When enabled, ISL also fetches diff information for diffs you authored but where the latest version was submitted by someone else (e.g., via AI tools, or commandeering).',
              )}>
              <Checkbox
                checked={showAuthoredDiffs}
                onChange={checked => {
                  setShowAuthoredDiffs(checked);
                }}>
                <T>Fetch authored diffs from Phabricator</T>
              </Checkbox>
            </Tooltip>
          )}
          {Internal.showAiReviewingBadgeOption && <ShowAiReviewingBadgeSetting />}
        </div>
      </Setting>
      {platform.canCustomizeFileOpener && (
        <Setting title={<T>Environment</T>}>
          <Column alignStart>
            <OpenFilesCmdSetting />
            <ExternalMergeToolSetting />
            <DisableUnsavedFilesWarningSetting />
          </Column>
        </Setting>
      )}
      <Setting title={<T>Files & Layout</T>}>
        <Column alignStart>
          <ClickToOpenDiffViewSetting />
          <CommitInfoLocationSetting />
        </Column>
      </Setting>
      <Suspense>{platform.Settings == null ? null : <platform.Settings />}</Suspense>
      <DebugToolsField />
    </DropdownFields>
  );
}

function ConfirmSubmitStackSetting() {
  const [value, setValue] = useAtom(confirmShouldSubmitEnabledAtom);
  const provider = useAtomValue(codeReviewProvider);
  if (provider == null || !provider.supportSubmittingAsDraft) {
    return null;
  }
  return (
    <Tooltip
      title={t(
        'This lets you choose to submit as draft and provide an update message. ' +
          'If false, no confirmation is shown and it will submit as draft if you previously ' +
          'checked the submit as draft checkbox.',
      )}>
      <Checkbox
        checked={value}
        onChange={checked => {
          setValue(checked);
        }}>
        <T>Show confirmation when submitting a stack</T>
      </Checkbox>
    </Tooltip>
  );
}

function ClickToOpenDiffViewSetting() {
  const [value, setValue] = useAtom(clickToOpenDiffViewAtom);
  return (
    <Tooltip
      title={t(
        'Controls what clicking a file name in the changed files list does. ' +
          'By default, clicking a file name opens the file, and the hover-only action button opens the diff view. ' +
          'When enabled, this is flipped: clicking a file name opens the diff view instead, and the hover button opens the file (like the VS Code git panel). ' +
          'Conflicted files always open the file regardless of this setting.',
      )}>
      <Checkbox
        data-testid="click-to-open-diff-view"
        checked={value}
        onChange={checked => {
          setValue(checked);
        }}>
        <T>Click filename to open diff view</T>
      </Checkbox>
    </Tooltip>
  );
}

function RenderCompactSetting() {
  const [value, setValue] = useAtom(renderCompactAtom);
  return (
    <Tooltip
      title={t(
        'Render commits in the tree more compactly, by reducing spacing and not wrapping Diff info to multiple lines. ' +
          'May require more horizontal scrolling.',
      )}>
      <Checkbox
        checked={value}
        onChange={checked => {
          setValue(checked);
        }}>
        <T>Compact Mode</T>
      </Checkbox>
    </Tooltip>
  );
}

function CommitInfoLocationSetting() {
  const [location, setLocation] = useAtom(commitInfoLocationAtom);
  return (
    <Tooltip
      title={t(
        'Position of the Commit Info panel relative to the commit graph. ' +
          '"Auto" uses bottom in narrow mode (e.g. sidebar) and right otherwise.',
      )}>
      <div className="dropdown-container setting-inline-dropdown">
        <T>Commit Info Panel</T>
        <Dropdown<{value: CommitInfoLocationWithAuto; name: string}>
          data-testid="commit-info-location-setting"
          value={location}
          options={[
            {value: 'auto', name: t('Auto')},
            {value: 'right', name: t('Right')},
            {value: 'bottom', name: t('Bottom')},
            {value: 'left', name: t('Left')},
            {value: 'top', name: t('Top')},
          ]}
          onChange={event => setLocation(event.currentTarget.value as CommitInfoLocationWithAuto)}
        />
      </div>
    </Tooltip>
  );
}

function CondenseObsoleteSetting() {
  const [value, setValue] = useAtom(condenseObsoleteStacks);
  return (
    <Tooltip
      title={t(
        'Visually condense a continuous stack of obsolete commits into just the top and bottom commits.',
      )}>
      <Checkbox
        data-testid="condense-obsolete-stacks"
        checked={value !== false}
        onChange={checked => {
          setValue(checked);
        }}>
        <T>Condense Obsolete Stacks</T>
      </Checkbox>
    </Tooltip>
  );
}

function ScrollToYouAreHereSetting() {
  const [value, setValue] = useAtom(scrollToYouAreHereOnOpen);
  return (
    <Tooltip
      title={t('When enabled, ISL automatically scrolls to your current commit when it opens.')}>
      <Checkbox
        data-testid="scroll-to-you-are-here-on-open"
        checked={value}
        onChange={checked => {
          setValue(checked);
        }}>
        <T>Auto scroll to "You are here" on open</T>
      </Checkbox>
    </Tooltip>
  );
}

function ShowWorktreeLabelsSetting() {
  const [value, setValue] = useAtom(showWorktreeLabels);
  return (
    <Tooltip
      title={t(
        'Show worktree name labels next to commits, such as "You are here" and badges for commits checked out in other worktrees.',
      )}>
      <Checkbox
        data-testid="show-worktree-labels"
        checked={value}
        onChange={checked => {
          setValue(checked);
        }}>
        <T>Show Worktree Labels</T>
      </Checkbox>
    </Tooltip>
  );
}

function ShowAiReviewingBadgeSetting() {
  const [value, setValue] = useAtom(showAiReviewingBadge);
  return (
    <Tooltip
      title={t('Show an "AI Reviewing" badge while an AI code review is running on a diff.')}>
      <Checkbox
        data-testid="show-ai-reviewing-badge"
        checked={value}
        onChange={checked => {
          setValue(checked);
        }}>
        <T>Show AI Reviewing badge</T>
      </Checkbox>
    </Tooltip>
  );
}

function CopyCommitHashFormatSetting() {
  const [value, setValue] = useAtom(copyCommitHashFormatAtom);
  return (
    <Tooltip
      title={t(
        'Choose whether "Copy Commit Hash" in the context menu copies the full hash or short (12-character) hash.',
      )}>
      <div className="dropdown-container setting-inline-dropdown">
        <T>Copy Commit Hash Format</T>
        <Dropdown<{value: typeof value; name: string}>
          data-testid="copy-commit-hash-format"
          options={[
            {value: 'long', name: t('Full Hash')},
            {value: 'short', name: t('Short Hash (12 chars)')},
          ]}
          value={value}
          onChange={event => {
            setValue(event.currentTarget.value as typeof value);
          }}
        />
      </div>
    </Tooltip>
  );
}

function DeemphasizeIrrelevantCommitsSetting() {
  const [value, setValue] = useAtom(irrelevantCwdDisplayModeAtom);
  return (
    <Tooltip
      title={t(
        'How to display commits which only change files in an unrelated directory to your current working directory.\n',
      )}>
      <div className="dropdown-container setting-inline-dropdown">
        <T>Cwd-Irrelevant Commits</T>
        <Dropdown<{value: typeof value; name: string}>
          data-testid="cwd-irrelevant-commits"
          options={[
            {value: 'show', name: t('Show')},
            {value: 'deemphasize', name: t('Deemphasize')},
            {value: 'hide', name: t('Hide')},
          ]}
          value={value}
          onChange={event => {
            setValue(event.currentTarget.value as typeof value);
          }}
        />
      </div>
    </Tooltip>
  );
}

function RebaseOffWarmWarningSetting() {
  const [value, setValue] = useAtom(rebaseOffWarmWarningEnabled);
  return (
    <Tooltip
      title={t(
        'Show a warning when rebasing off a commit that is not warm (i.e. not in the current stack).',
      )}>
      <Checkbox
        data-testid="rebase-off-warm-warning-enabled"
        checked={value}
        onChange={checked => {
          setValue(checked);
        }}>
        <T>Show Warning on Rebase Off Warm</T>
      </Checkbox>
    </Tooltip>
  );
}

function DistantRebaseWarningSetting() {
  const [value, setValue] = useAtom(distantRebaseWarningEnabled);
  return (
    <Tooltip
      title={t(
        'Show a warning when rebasing onto a commit that is significantly older than the current commit.',
      )}>
      <Checkbox
        data-testid="distant-rebase-warning-enabled"
        checked={value}
        onChange={checked => {
          setValue(checked);
        }}>
        <T>Show Warning on Distant Rebase</T>
      </Checkbox>
    </Tooltip>
  );
}

function RebaseOntoMasterWarningSetting() {
  const [value, setValue] = useAtom(rebaseOntoMasterWarningEnabled);
  return (
    <Tooltip
      title={t(
        'Show a warning when rebasing directly onto master branch, which can cause unexpected failures and slower builds.',
      )}>
      <Checkbox
        data-testid="rebase-master-warning-enabled"
        checked={value}
        onChange={checked => {
          setValue(checked);
        }}>
        <T>Show Warning on Rebase onto Master</T>
      </Checkbox>
    </Tooltip>
  );
}

export const openFileCmdAtom = configBackedAtom<string | null>(
  'isl.open-file-cmd',
  null,
  /* readonly */ true,
  /* use raw value */ true,
);

function OpenFilesCmdSetting() {
  const cmdRaw = useAtomValue(openFileCmdAtom);
  const cmd = cmdRaw == null ? null : ((tryJsonParse(cmdRaw) as string | Array<string>) ?? cmdRaw);
  const cmdEl =
    cmd == null ? (
      <T>OS Default Program</T>
    ) : (
      <code>{Array.isArray(cmd) ? cmd.join(' ') : cmd}</code>
    );
  return (
    <Tooltip
      component={() => (
        <div>
          <div>
            <T>You can configure how to open files from ISL via</T>
          </div>
          <pre>sl config --user isl.open-file-cmd "/path/to/command"</pre>
          <div>
            <T>or</T>
          </div>
          <pre>sl config --user isl.open-file-cmd '["cmd", "with", "args"]'</pre>
        </div>
      )}>
      <Row>
        <T replace={{$cmd: cmdEl}}>Open files in: $cmd</T>
        <Subtle>
          <T>How to configure?</T>
        </Subtle>
        <Icon icon="question" />
      </Row>
    </Tooltip>
  );
}

function ExternalMergeToolSetting() {
  const mergeTool = useAtomValue(externalMergeToolAtom);
  const cmdEl = mergeTool == null ? <T>None</T> : <code>{mergeTool}</code>;
  return (
    <Tooltip
      component={() => (
        <div>
          <div style={{alignItems: 'flex-start', maxWidth: 400}}>
            <T
              replace={{
                $help: <code>sl help config.merge-tools</code>,
                $configedit: <code>sl config --edit</code>,
                $mymergetool: <code>merge-tools.mymergetool</code>,
                $uimerge: <code>ui.merge = mymergetool</code>,
                $gui: <code>merge-tools.mymergetool.gui</code>,
                $local: <code>--local</code>,
                $br: (
                  <>
                    <br />
                    <br />
                  </>
                ),
              }}>
              You can configure Sapling and ISL to use a custom external merge tool, which is used
              when a merge conflict is detected.$br Define your tool with $configedit (or with
              $local to configure only for the current repository), by setting $mymergetool and
              $uimerge$brCLI merge tools like vimdiff won't be used from ISL. Ensure $gui is set to
              True.$br For more information, see: $help
            </T>
          </div>
        </div>
      )}>
      <Row>
        <T replace={{$cmd: cmdEl}}>External Merge Tool: $cmd</T>
        <Subtle>
          <T>How to configure?</T>
        </Subtle>
        <Icon icon="question" />
      </Row>
    </Tooltip>
  );
}

function DisableUnsavedFilesWarningSetting() {
  const [value, setValue] = useAtom(disableUnsavedFilesWarning);
  return (
    <Tooltip
      title={t(
        'By default, committing or amending while files are unsaved in your editor shows a confirmation, ' +
          'which lets you save them first. Enable this to skip that confirmation, ' +
          'for example if you intentionally leave files unsaved to keep them out of your commits.',
      )}>
      <Checkbox
        checked={value}
        onChange={checked => {
          setValue(checked);
        }}>
        <T>Don't warn about unsaved files when committing or amending</T>
      </Checkbox>
    </Tooltip>
  );
}

function ZoomUISetting() {
  const [zoom, setZoom] = useAtom(zoomUISettingAtom);
  function roundToPercent(n: number): number {
    return Math.round(n * 100) / 100;
  }
  return (
    <div className="zoom-setting">
      <Tooltip title={t('Decrease UI Zoom')}>
        <Button
          icon
          onClick={() => {
            setZoom(roundToPercent(zoom - 0.1));
          }}>
          <Icon icon="zoom-out" />
        </Button>
      </Tooltip>
      <span>{`${Math.round(100 * zoom)}%`}</span>
      <Tooltip title={t('Increase UI Zoom')}>
        <Button
          icon
          onClick={() => {
            setZoom(roundToPercent(zoom + 0.1));
          }}>
          <Icon icon="zoom-in" />
        </Button>
      </Tooltip>
      <div style={{width: '20px'}} />
      <label>
        <T>Presets:</T>
      </label>
      <Button
        style={{fontSize: '80%'}}
        icon
        onClick={() => {
          setZoom(0.8);
        }}>
        <T>Small</T>
      </Button>
      <Button
        icon
        onClick={() => {
          setZoom(1.0);
        }}>
        <T>Normal</T>
      </Button>
      <Button
        style={{fontSize: '120%'}}
        icon
        onClick={() => {
          setZoom(1.2);
        }}>
        <T>Large</T>
      </Button>
    </div>
  );
}

function DebugToolsField() {
  const [isDebug, setIsDebug] = useAtom(debugToolsEnabledState);
  const [overrideDisabledSubmit, setOverrideDisabledSubmit] = useAtom(overrideDisabledSubmitModes);
  const [debugFlag, setDebugFlag] = useAtom(enableSaplingDebugFlag);
  const [verboseFlag, setVerboseFlag] = useAtom(enableSaplingVerboseFlag);
  const provider = useAtomValue(codeReviewProvider);
  const commandName = useAtomValue(mainCommandName);

  const [branchPrsEnabled, setBranchPrsEnabled] = useAtom(experimentalBranchPRsEnabled);

  return (
    <DropdownField title={t('Debug Tools & Experimental')}>
      <Column alignStart>
        <Checkbox
          checked={isDebug}
          onChange={checked => {
            setIsDebug(checked);
          }}>
          <T>Enable Debug Tools</T>
        </Checkbox>
        <ExperimentalFeaturesCheckbox />
        {provider?.submitDisabledReason?.() != null && (
          <Checkbox
            checked={overrideDisabledSubmit}
            onChange={setOverrideDisabledSubmit}
            data-testid="force-enable-github-submit">
            <T>Force enable `sl pr submit` and `sl ghstack submit`</T>
          </Checkbox>
        )}
        {provider?.supportBranchingPrs === true && (
          <Checkbox
            checked={branchPrsEnabled}
            onChange={checked => {
              setBranchPrsEnabled(checked);
            }}>
            <T>Enable Experimental Branching PRs for GitHub</T>
          </Checkbox>
        )}
        <Row>
          <T
            replace={{
              $sl: <code>{commandName}</code>,
              $verbose: (
                <Checkbox checked={verboseFlag} onChange={setVerboseFlag}>
                  <code>--verbose</code>
                </Checkbox>
              ),
              $debug: (
                <Checkbox checked={debugFlag} onChange={setDebugFlag}>
                  <code>--debug</code>
                </Checkbox>
              ),
            }}>
            Pass extra flags to $sl: $verbose $debug
          </T>
        </Row>
      </Column>
    </DropdownField>
  );
}

function ExperimentalFeaturesCheckbox() {
  const [experimentalFeaturesEnabled, setExperimentalFeaturesEnabled] =
    useAtom(hasExperimentalFeatures);

  if (currentExperimentalFeaturesList.length === 0) {
    return null;
  }

  return (
    <Tooltip
      title={t(
        `Enable experimental features that are still being developed and may not work as expected.

Current experimental features: ${currentExperimentalFeaturesList.join(', ')}`,
      )}>
      <Checkbox
        checked={experimentalFeaturesEnabled}
        onChange={checked => {
          setExperimentalFeaturesEnabled(checked);
        }}>
        <T>Enable Experimental Features</T>
      </Checkbox>
    </Tooltip>
  );
}
