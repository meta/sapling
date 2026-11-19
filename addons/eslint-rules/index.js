/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

const jotaiMaybeUseFamily = require('./jotai-maybe-use-family');
const internalPromiseCallbackTypes = require('./internal-promise-callback-types');
const noFacebookImports = require('./no-facebook-imports');
const noSyncIo = require('./no-sync-io');

module.exports = {
  rules: {
    'jotai-maybe-use-family': jotaiMaybeUseFamily,
    'internal-promise-callback-types': internalPromiseCallbackTypes,
    'no-facebook-imports': noFacebookImports,
    'no-sync-io': noSyncIo,
  },
};
