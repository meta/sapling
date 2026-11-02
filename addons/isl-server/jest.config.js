/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

/* global module */
/** @type {import('ts-jest/dist/types').InitialOptionsTsJest} */
module.exports = {
  preset: 'ts-jest',
  // Transpile each file on its own instead of type-checking it; `tsc --noEmit` covers types.
  // TypeScript 7 has no JavaScript API, so ts-jest uses the TypeScript 6 compatibility package.
  transform: {
    '^.+\\.tsx?$': [
      'ts-jest',
      {isolatedModules: true, diagnostics: false, compiler: '@typescript/typescript6'},
    ],
  },
  testEnvironment: 'node',
  setupFiles: ['<rootDir>/src/setupTests.ts'],
};
