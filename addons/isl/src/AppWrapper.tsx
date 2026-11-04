/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import type {JSX, ReactNode} from 'react';

import {ErrorBoundary} from 'isl-components/ErrorNotice';
import {ThemedComponentsRoot} from 'isl-components/ThemedComponentsRoot';
import {ViewportOverlayRoot} from 'isl-components/ViewportOverlay';
import {Provider, useAtomValue, useStore} from 'jotai';
import React, {lazy} from 'react';
import {ContextMenus} from 'shared/ContextMenu';
import {ISLCommandContext} from './ISLShortcuts';
import {SuspenseBoundary} from './SuspenseBoundary';
import {TopLevelToast} from './TopLevelToast';
import {enableReactTools, enableReduxTools} from './atoms/debugToolAtoms';
import {I18nSupport} from './i18n';
import {setJotaiStore} from './jotaiUtils';
import platform from './platform';
import {DEFAULT_RESET_CSS} from './resetStyle';
import {zoomUISettingAtom} from './responsive';
import {themeState} from './theme';
import {ModalContainer} from './useModal';
import {isDev, isTest} from './utils';

export function AllProviders({children}: {children: ReactNode}) {
  return (
    <React.StrictMode>
      <ResetStyle />
      <I18nSupport>
        <MaybeWithJotaiRoot>
          <ISLRoot>
            <ISLCommandContext>
              <ErrorBoundary>
                {children}
                <ViewportOverlayRoot />
                <ModalContainer />
                <ContextMenus />
                <TopLevelToast />
              </ErrorBoundary>
            </ISLCommandContext>
          </ISLRoot>
        </MaybeWithJotaiRoot>
      </I18nSupport>
    </React.StrictMode>
  );
}

function ResetStyle() {
  const resetCSS = platform.theme?.resetCSS ?? DEFAULT_RESET_CSS;
  return resetCSS.length > 0 ? <style>{resetCSS}</style> : null;
}

function ISLRoot({children}: {children: ReactNode}) {
  const theme = useAtomValue(themeState);
  useAtomValue(zoomUISettingAtom);
  return (
    <div onDragEnter={handleDragAndDrop} onDragOver={handleDragAndDrop}>
      <ThemedComponentsRoot className="isl-root" theme={theme}>
        {children}
      </ThemedComponentsRoot>
    </div>
  );
}

function handleDragAndDrop(e: React.DragEvent<HTMLDivElement>) {
  // VS Code tries to capture drag & drop events to open files. But if you're dragging
  // on ISL, you probably want to do an ImageUpload. Prevent this event from propagating to vscode.
  if (e.dataTransfer?.types?.some(t => t === 'Files')) {
    e.stopPropagation();
    e.preventDefault();
    e.dataTransfer.dropEffect = 'copy';
  }
}

function MaybeWithJotaiRoot({children}: {children: JSX.Element}) {
  if (isTest) {
    // Use a new store when re-mounting so each test (that calls `render(<App />)`)
    // starts with a clean state.
    return (
      <Provider>
        <AccessJotaiRoot />
        {children}
      </Provider>
    );
  } else if (isDev) {
    return <MaybeJotaiDebugTools>{children}</MaybeJotaiDebugTools>;
  } else {
    // Such scoped Provider or store complexity is not needed outside tests or dev.
    return children;
  }
}
const AtomsDevtools = lazy(() => import('./JotaiDevtools').then(m => ({default: m.AtomsDevtools})));
const DebugAtoms = lazy(() => import('./JotaiDevtools').then(m => ({default: m.DebugAtoms})));

function MaybeJotaiDebugTools({children}: {children: JSX.Element}) {
  const enabledRedux = useAtomValue(enableReduxTools);
  const enabledReact = useAtomValue(enableReactTools);
  return enabledRedux || enabledReact ? (
    <SuspenseBoundary>
      {enabledRedux ? <AtomsDevtools>{children}</AtomsDevtools> : children}
      {enabledReact && <DebugAtoms />}
    </SuspenseBoundary>
  ) : (
    children
  );
}

function AccessJotaiRoot() {
  const store = useStore();
  setJotaiStore(store);
  return null;
}
