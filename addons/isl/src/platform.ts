/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import type {JSX, LazyExoticComponent} from 'react';
import type {Comparison} from 'shared/Comparison';
import type {Json} from 'shared/typeUtils';
import type {MessageBus} from './MessageBus';
import type {ThemeColor} from './theme';
import type {
  AbsolutePath,
  AgentPrompt,
  Disposable,
  OneIndexedLineNumber,
  PageVisibility,
  PlatformName,
  RepoRelativePath,
  ServerToClientMessage,
} from './types';

import {makeBrowserLikePlatformImpl} from './platform/browserPlatformImpl';

export type InitialParamKeys = 'token' | string;

/** What `localStorageBackedAtom` tells a platform about a setting it stores. */
export type PersistedStateOptions = {
  /** A preference that means the same on every machine, which a platform may keep elsewhere too. */
  portable: true;
  defaultValue: Json;
  /** Whether a value kept elsewhere is acceptable here, beyond having the default's JSON type. */
  validate?: (value: Json) => boolean;
};

export interface PlatformVisibility {
  getVisibility(): PageVisibility;
  onDidChangeVisibility(callback: (visibility: PageVisibility) => unknown): Disposable;
}

/**
 * Platform-specific API for each target: vscode extension, electron standalone, browser, ...
 */
export interface Platform {
  platformName: PlatformName;
  /** Whether the embedding host supports investigating a failed operation. */
  supportsFailureInvestigation?: boolean;
  confirm(message: string, details?: string): Promise<boolean>;
  openFile(path: RepoRelativePath, options?: {line?: OneIndexedLineNumber}): void;
  openFiles(paths: ReadonlyArray<RepoRelativePath>, options?: {line?: OneIndexedLineNumber}): void;
  canCustomizeFileOpener: boolean;
  openContainingFolder?(path: RepoRelativePath): void;
  revealInFileExplorer?(path: RepoRelativePath): void;
  revealInExplorerView?(path: RepoRelativePath): void;
  openDiff?(path: RepoRelativePath, comparison: Comparison): void;
  openFileAtRevset?(path: RepoRelativePath, revset: string): void;
  openPreview?(path: RepoRelativePath): void;
  openExternalLink(url: string): void;
  clipboardCopy(text: string, html?: string): void | Promise<void>;
  chooseFile?(title: string, multi: boolean): Promise<Array<File>>;
  /** Whether to ask to configure an external merge tool. Useful for standalone platforms, but not embedded ones like vscode. */
  upsellExternalMergeTool: boolean;
  /**
   * Get stored data from local persistent cache (usually browser local storage).
   * Note: Some platforms may not support this (e.g. browser with localStorage disabled),
   * or it may not be persisted indefinitely---usual localStorage caveats apply.
   */
  getPersistedState<T extends Json>(key: string, options?: PersistedStateOptions): T | null;
  /** see getPersistedState  */
  setPersistedState<T extends Json>(
    key: string,
    value: T | undefined,
    options?: PersistedStateOptions,
  ): void;
  /** see getPersistedState  */
  clearPersistedState(): void;
  /** see getPersistedState  */
  getAllPersistedState(): Json | undefined;

  handleServerMessage?: (message: ServerToClientMessage) => void;

  openDedicatedComparison?: (comparison: Comparison) => Promise<boolean>;

  /**
   * Component representing additional buttons/info in the cwd menu,
   * used to show a button or hint about how to add more cwds.
   * Note: This should be lazy-loaded via `React.lazy()` so that implementations
   * may import any files without worrying about the platform being set up yet or not.
   */
  AddMoreCwdsHint?: LazyExoticComponent<() => JSX.Element>;

  /** Platform-specific settings, such as how ISL panels work */
  Settings?: LazyExoticComponent<() => JSX.Element>;

  theme?: {
    getTheme(): ThemeColor;
    getThemeName?(): string | undefined;
    onDidChangeTheme(callback: (theme: ThemeColor) => unknown): Disposable;
    resetCSS?: string;
  };

  /** Optional embedding-host activity. Effective visibility is the least-active
   * value reported by this source and the browser document. */
  visibility?: PlatformVisibility;

  /** If the platform has a notion of pending edits (typically from an AI), methods for listening and resolving them. */
  suggestedEdits?: {
    /** listen for changes to edits so ISL can confirm edits before taking actions. */
    onDidChangeSuggestedEdits(callback: (suggestedEdits: Array<AbsolutePath>) => void): Disposable;
    /** Accepts/Rejects edits */
    resolveSuggestedEdits(action: 'accept' | 'reject', files?: Array<AbsolutePath>): void;
  };

  messageBus: MessageBus;
  /** In browser-like platforms, some ISL parameters are passed via URL query params */
  initialUrlParams?: Map<InitialParamKeys, string>;

  /**
   * Whether `promptAgent` is available. This may change whenever the connection to the host opens.
   * When true, AI smart actions send their prompts to the host's agent instead of Devmate.
   */
  supportsAgentPrompts?(): boolean;
  /**
   * Send a prompt to the embedding host's AI agent, which decides how to run it.
   * With `awaitResponse`, resolves with the agent's final response; otherwise once it is sent.
   */
  promptAgent?(prompt: AgentPrompt): Promise<string | undefined>;
}

declare global {
  interface Window {
    islPlatform?: Platform;
  }
}

// [!] NOTE: On some platforms (vscode), this file is replaced at bundle time with a platform-specific implementation
// of the Platform interface.
// This file should have no other side effects than exporting the platform.

// However, non-vscode but non-browser platforms are defined by setting window.islPlatform
// before the main ISL script loads.

/** The ISL client Platform. This may be the browser platform, VSCodeWebviewPlatform, or another platforms, determined at runtime.  */
const platform = window.islPlatform ?? makeBrowserLikePlatformImpl('browser');
window.islPlatform = platform;

export default platform;
