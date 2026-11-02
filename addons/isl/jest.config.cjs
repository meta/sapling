/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

const IS_CI = !!process.env.SANDCASTLE || !!process.env.GITHUB_ACTIONS;

// Jest defaults to one worker per core. Beyond ~16 workers the suite stops getting faster
// (the slowest test files bound it), while each jsdom worker still costs hundreds of MB,
// which can exhaust memory on large dev servers.
const MAX_WORKERS = Math.max(1, Math.min(16, require('os').availableParallelism() - 1));

/* global module */
/** @type {import('ts-jest/dist/types').JestConfigWithTsJest} */
module.exports = {
  preset: 'ts-jest',
  testEnvironment: 'jsdom',
  collectCoverageFrom: ['src/**/*.{js,jsx,ts,tsx}', '!src/**/*.d.ts'],
  setupFilesAfterEnv: ['<rootDir>/src/setupTests.ts'],
  testTimeout: IS_CI ? 15 * 1000 : 5 * 1000,
  maxWorkers: MAX_WORKERS,
  moduleNameMapper: {
    '\\.(jpg|jpeg|png|gif|eot|otf|webp|svg|ttf|woff|woff2|mp4|webm|wav|mp3|m4a|aac|oga)$':
      '<rootDir>/src/__mocks__/fileMock.js',
    '\\.css$': '<rootDir>/src/__mocks__/styleMock.ts',
    '\\?worker&url$': '<rootDir>/src/__mocks__/fileMock.js',
  },
  transformIgnorePatterns: [
    '[/\\\\]node_modules[/\\\\].+\\.(js|jsx|mjs|cjs|ts|tsx)$',
    '^.+\\.module\\.(css|sass|scss)$',
  ],
  testMatch: [
    '<rootDir>/src/**/__tests__/**/*.{js,jsx,ts,tsx}',
    '<rootDir>/src/**/*.{spec,test}.{js,jsx,ts,tsx}',
  ],
  modulePaths: ['<rootDir>/src'],
  transform: {
    '^.+\\.tsx?$': '<rootDir>/jest-transformer-import-meta.cjs',
  },
  resetMocks: true,
};
