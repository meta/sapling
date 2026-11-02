/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

const {default: tsJest} = require('ts-jest');

// `isolatedModules` transpiles each file on its own instead of building a full program,
// which is much faster. `tsc --noEmit` covers type checking.
const transformer = tsJest.createTransformer({diagnostics: false, isolatedModules: true});

/**
 * Replace 'import.meta.hot' with 'undefined' to make Jest happy.
 * Replace 'import.meta.url' with a require filename to enable WebWorkers.
 * Then delegates to the ts-jest transformer.
 *
 * For simplicity, we just do a naive string replace without complex parsing.
 */
function process(sourceText, path, options) {
  const newSourceText = sourceText
    .replace(/import\.meta\.hot/g, 'undefined')
    .replace(/import\.meta\.url/g, `require('url').pathToFileURL(__filename).toString()`);
  return transformer.process(newSourceText, path, options);
}

module.exports = {process};
