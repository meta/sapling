/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import {nullthrows} from 'shared/utils';
import {webviewPlatform} from '../webviewPlatform';

(window.external as unknown as Record<string, unknown>).invoke = jest.fn();

describe('webview platform', () => {
  const external = () => {
    return window.external as unknown as {invoke: jest.FunctionLike};
  };

  it('can send openExternal messages', () => {
    webviewPlatform.openExternalLink('example.com');
    expect(external().invoke).toHaveBeenCalledWith(
      '{"cmd":"openExternal","url":"example.com","id":0}',
    );
  });

  it('can send request and receive response messages', async () => {
    const promise = webviewPlatform.chooseFile?.('my title', true);
    expect(external().invoke).toHaveBeenCalledWith(
      '{"cmd":"chooseFile","title":"my title","path":"","multi":true,"mediaOnly":true,"id":1}',
    );

    const msg_bytes = new Uint8Array([0x00, 0x48, 0x80, 0xff]);
    const msg_b64 = Buffer.from(msg_bytes).toString('base64');
    window.islWebviewHandleResponse({
      cmd: 'chooseFile',
      files: [{name: 'file.txt', base64Content: msg_b64}],
      id: 1,
    });

    const result = nullthrows(await promise);
    expect(result[0].name).toEqual('file.txt');
    expect(result[0].size).toEqual(msg_bytes.length);
    const contents = await new Promise<ArrayBuffer>((resolve, reject) => {
      const reader = new FileReader();
      reader.onload = () => resolve(reader.result as ArrayBuffer);
      reader.onerror = () => reject(reader.error);
      reader.readAsArrayBuffer(result[0]);
    });
    expect(new Uint8Array(contents)).toEqual(msg_bytes);
  });
});
