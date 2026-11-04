/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import type {Platform} from '../platform';

import {makeBrowserLikePlatformImpl} from './browserPlatformImpl';

// important: this file should not try to import other code from 'isl',
// since it will end up getting duplicated when bundling.

/**
 * This platform is used when spawned as a standalone webview from `sl web`.
 * We pass messages to the rust side via `external.invoke`,
 * with JSON serialized requests. Rust will respond back with JSON serialized responses.
 * This lets us handle features like alerts, file dialogs, and opening external links
 * which are not implemented in the webview itself.
 */
export const webviewPlatform: Platform = {
  // just act like the browser platform by default, since the app use case is similar
  ...makeBrowserLikePlatformImpl('webview'),

  openExternalLink(url: string) {
    invoke({cmd: 'openExternal', url});
  },
  confirm(message: string, details?: string): Promise<boolean> {
    return request({cmd: 'confirm', message, details}).then(({ok}) => ok);
  },
  async chooseFile(title: string, multi: boolean): Promise<Array<File>> {
    const response = await request({cmd: 'chooseFile', title, path: '', multi, mediaOnly: true});
    const {files} = response;
    if (!files) {
      return [];
    }
    const result = files.map(value => b64toFile(value.base64Content, value.name));
    return result;
  },
};

function b64toFile(b64Data: string, filename: string): File {
  const byteCharacters = atob(b64Data);
  const bytes = new Uint8Array(byteCharacters.length);
  for (let i = 0; i < byteCharacters.length; i++) {
    bytes[i] = byteCharacters.charCodeAt(i);
  }
  return new File([bytes], filename);
}

window.islPlatform = webviewPlatform;

/**
 * Typed commands to communicate from the frontend with the Rust app hosting the webview.
 * This should match the rust types used in webview-app.
 */
type ExternalWebviewCommandsInvoke =
  | {cmd: 'openExternal'; url: string}
  | {cmd: 'confirm'; message: string; details?: string}
  | {cmd: 'chooseFile'; title: string; path: string; multi: boolean; mediaOnly: boolean};
type ExternalWebviewCommandsResponse = (
  | {cmd: 'confirm'; ok: boolean}
  | {
      cmd: 'chooseFile';
      files: Array<{
        name: string;
        base64Content: string;
      }>;
    }
) & {id: number};

declare global {
  interface Window {
    islWebviewHandleResponse: (response: ExternalWebviewCommandsResponse) => void;
  }
}

let nextId = 0;
const callbacks = new Map<number, (response: ExternalWebviewCommandsResponse) => void>();
window.islWebviewHandleResponse = (response: ExternalWebviewCommandsResponse) => {
  const cb = callbacks.get(response.id);
  if (cb) {
    cb(response);
    callbacks.delete(response.id);
  }
};

declare const external: {
  invoke(arg: string): Promise<void>;
};

function invoke(json: ExternalWebviewCommandsInvoke) {
  external.invoke(JSON.stringify({...json, id: nextId++}));
}

function request<K extends ExternalWebviewCommandsInvoke['cmd']>(
  json: ExternalWebviewCommandsInvoke & {cmd: K},
): Promise<ExternalWebviewCommandsResponse & {cmd: K}> {
  const id = nextId++;
  let resolve: (value: ExternalWebviewCommandsResponse & {cmd: K}) => void;
  const callback = (response: ExternalWebviewCommandsResponse) => {
    resolve(response as ExternalWebviewCommandsResponse & {cmd: K});
  };
  const promise = new Promise<ExternalWebviewCommandsResponse & {cmd: K}>(res => {
    resolve = res;
  });
  external.invoke(JSON.stringify({...json, id}));
  callbacks.set(id, callback);

  return promise;
}
