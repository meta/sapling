/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import path from 'node:path';
import {fileURLToPath} from 'node:url';
import {defineConfig} from 'rolldown';

// eslint-disable-next-line no-undef
const isProduction = process.env.NODE_ENV === 'production';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const projectRootDir = path.dirname(__dirname);

export default defineConfig({
  input: './extension/extension.ts',
  output: {
    format: 'cjs',
    dir: 'dist',
    sourcemap: true,
    sourcemapExcludeSources: isProduction,
    minify: isProduction,
  },
  external: ['ws', 'vscode'],
  platform: 'node',
  transform: {
    define: {
      'process.env.NODE_ENV': JSON.stringify(isProduction ? 'production' : 'development'),
    },
  },
  resolve: {
    alias: {
      isl: path.resolve(projectRootDir, 'isl'),
      shared: path.resolve(projectRootDir, 'shared'),
    },
  },
});
