/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

/* global module */
/** @type {import('ts-jest/dist/types').JestConfigWithTsJest} */
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
  testEnvironment: 'jsdom',
  collectCoverageFrom: ['**/*.{js,jsx,ts,tsx}', '!**/*.d.ts'],
  setupFilesAfterEnv: ['<rootDir>/__tests__/setupTests.ts'],
  moduleNameMapper: {
    '\\.css$': '<rootDir>/__tests__/styleMock.ts',
  },
  transformIgnorePatterns: [
    '[/\\\\]node_modules[/\\\\].+\\.(js|jsx|mjs|cjs|ts|tsx)$',
    '^.+\\.module\\.(css|sass|scss)$',
  ],
  testMatch: ['<rootDir>/**/__tests__/**/*.test.{js,jsx,ts,tsx}'],
  modulePaths: ['<rootDir>'],
  resetMocks: true,
};
