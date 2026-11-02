/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import type {ContextMenuItem} from 'shared/ContextMenu';
import type {RepoRelativePath} from './types';

import {Button} from 'isl-components/Button';
import {Icon} from 'isl-components/Icon';
import {Subtle} from 'isl-components/Subtle';
import {atom, useAtomValue} from 'jotai';
import {useContextMenu} from 'shared/ContextMenu';
import {minimalDisambiguousPaths} from 'shared/minimalDisambiguousPaths';
import serverAPI from './ClientToServerAPI';
import {Column, Row} from './ComponentUtils';
import {availableCwds} from './CwdSelector';
import {T, t} from './i18n';
import {localStorageBackedAtom, readAtom, writeAtom} from './jotaiUtils';
import platform from './platform';
import {showModal} from './useModal';
import {registerCleanup, registerDisposable} from './utils';

/**
 * A list of files for this repo that are unsaved in the IDE.
 * This is always `[]` for unsupported platforms like browser.
 */
export const unsavedFiles = atom<Array<{path: RepoRelativePath; uri: string}>>([]);
registerCleanup(
  availableCwds,
  serverAPI.onConnectOrReconnect(() => {
    serverAPI.postMessage({
      type: 'platform/subscribeToUnsavedFiles',
    });
  }),
  import.meta.hot,
);
registerDisposable(
  availableCwds,
  serverAPI.onMessageOfType('platform/unsavedFiles', event =>
    writeAtom(unsavedFiles, event.unsaved),
  ),
  import.meta.hot,
);

/**
 * Some users intentionally leave files unsaved to keep changes out of their commits,
 * so the warning about unsaved files can be opted out of.
 */
export const disableUnsavedFilesWarning = localStorageBackedAtom<boolean>(
  'isl.disable-unsaved-files-warning',
  false,
  {portable: true},
);

export function UnsavedFilesCount() {
  const unsaved = useAtomValue(unsavedFiles);

  const menu = useContextMenu(() => {
    const fullPaths = unsaved.map(({path}) => path);
    const disambiguated = minimalDisambiguousPaths(fullPaths);
    const options: Array<ContextMenuItem> = disambiguated.map((name, i) => ({
      label: t('Open $name', {replace: {$name: name}}),
      onClick: () => {
        platform.openFile(fullPaths[i]);
      },
    }));
    options.push({type: 'divider'});
    options.push({
      label: t('Save All'),
      onClick: () => serverAPI.postMessage({type: 'platform/saveAllUnsavedFiles'}),
    });
    return options;
  });

  if (unsaved.length === 0) {
    return null;
  }
  return (
    <Subtle>
      <Row>
        <T count={unsaved.length}>unsavedFileCount</T>
        <Button icon>
          <Icon icon="ellipsis" onClick={menu} />
        </Button>
      </Row>
    </Subtle>
  );
}

/**
 * If there are unsaved files, ask the user if they want to save them.
 * Returns true if the user wants to continue with the operation (after possibly having saved the files),
 * false if they cancelled.
 */
export async function confirmUnsavedFiles(): Promise<boolean> {
  if (readAtom(disableUnsavedFilesWarning)) {
    return true;
  }

  const unsaved = readAtom(unsavedFiles);
  if (unsaved.length === 0) {
    return true;
  }

  const buttons = [
    t('Cancel'),
    t('Continue Without Saving'),
    {label: t('Save All and Continue'), primary: true},
  ];
  const answer = await showModal({
    type: 'confirm',
    buttons,
    title: <T count={unsaved.length}>confirmUnsavedFileCount</T>,
    message: (
      <Column alignStart>
        <Column alignStart>
          {unsaved.map(({path}) => (
            <Row key={path}>{path}</Row>
          ))}
        </Column>
        <Row>
          <T count={unsaved.length}>doYouWantToSaveThem</T>
        </Row>
      </Column>
    ),
  });

  if (answer === buttons[2]) {
    serverAPI.postMessage({type: 'platform/saveAllUnsavedFiles'});
    const message = await serverAPI.nextMessageMatching(
      'platform/savedAllUnsavedFiles',
      () => true,
    );
    return message.success;
  }

  return answer === buttons[1];
}
