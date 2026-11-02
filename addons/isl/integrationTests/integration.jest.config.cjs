/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

/* global module */
/** @type {import('@jest/types').Config.InitialOptions} */
module.exports = {
  verbose: true,
  testRegex: '/integrationTests/.*\\.test\\.tsx?',

  // shutting down watchman is async, so our `dispose` does not synchronously tear down everything,
  // thus jest thinks we have leaked handles. Force exits quiets this warning.
  forceExit: true,

  // Even though the repos are separate, somehow running multiple integration tests in parallel
  // causes failures. For now, just limit to one test at a time.
  maxWorkers: 1,

  // use typescript in tests
  preset: 'ts-jest',
  testEnvironment: 'jsdom',
  rootDir: '..',
  setupFilesAfterEnv: ['<rootDir>/integrationTests/setupTests.ts'],
  testTimeout: 120_000,

  modulePaths: ['<rootDir>/src'],
  moduleNameMapper: {
    '\\.(jpg|jpeg|png|gif|eot|otf|webp|svg|ttf|woff|woff2|mp4|webm|wav|mp3|m4a|aac|oga)$':
      '<rootDir>/src/__mocks__/fileMock.js',
    '\\.css$': '<rootDir>/src/__mocks__/styleMock.ts',
    '\\?worker&url$': '<rootDir>/src/__mocks__/fileMock.js',
  },
  transform: {
    '^.+\\.tsx?$': '<rootDir>/jest-transformer-import-meta.cjs',
  },
  transformIgnorePatterns: [
    '[/\\\\]node_modules[/\\\\].+\\.(js|jsx|mjs|cjs|ts|tsx)$',
    '^.+\\.module\\.(css|sass|scss)$',
  ],
};
