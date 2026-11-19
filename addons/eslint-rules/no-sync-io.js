/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

/** Every synchronous function exported by `node:fs`. */
const SYNC_FS_METHODS = [
  'accessSync',
  'appendFileSync',
  'chmodSync',
  'chownSync',
  'closeSync',
  'copyFileSync',
  'cpSync',
  'existsSync',
  'fchmodSync',
  'fchownSync',
  'fdatasyncSync',
  'fstatSync',
  'fsyncSync',
  'ftruncateSync',
  'futimesSync',
  'globSync',
  'lchmodSync',
  'lchownSync',
  'linkSync',
  'lstatSync',
  'lutimesSync',
  'mkdirSync',
  'mkdtempSync',
  'opendirSync',
  'openSync',
  'readdirSync',
  'readFileSync',
  'readlinkSync',
  'readSync',
  'readvSync',
  'realpathSync',
  'renameSync',
  'rmdirSync',
  'rmSync',
  'statfsSync',
  'statSync',
  'symlinkSync',
  'truncateSync',
  'unlinkSync',
  'utimesSync',
  'writeFileSync',
  'writeSync',
  'writevSync',
];

const SYNC_PROCESS_METHODS = ['execSync', 'execFileSync', 'spawnSync'];

const SYNC_METHODS = new Set([...SYNC_FS_METHODS, ...SYNC_PROCESS_METHODS]);

/** Long-running Node processes, where a blocked event loop stalls every client. */
const SERVER_PATH = /[\\/](isl-server[\\/]src|vscode[\\/]extension)[\\/]/;
const TEST_PATH = /[\\/]__tests__[\\/]|\.test\.tsx?$/;

module.exports = {
  meta: {
    type: 'problem',
    docs: {
      description:
        'disallow synchronous filesystem and child process calls in isl-server/src and vscode/extension',
    },
    fixable: null, // Not automatically fixable
    messages: {
      noSyncIo:
        '`{{name}}` blocks the whole Node process until it returns. In the VS Code extension host that stalls every other extension too. Use the async API instead (`fs.promises`, `ejeca`) and await it.',
    },
    schema: [], // no options
  },
  create(context) {
    const filename = context.filename ?? context.getFilename();
    if (!SERVER_PATH.test(filename) || TEST_PATH.test(filename)) {
      return {};
    }
    return {
      CallExpression(node) {
        const callee = node.callee;
        let name;
        if (callee.type === 'Identifier') {
          name = callee.name;
        } else if (
          callee.type === 'MemberExpression' &&
          !callee.computed &&
          callee.property.type === 'Identifier'
        ) {
          name = callee.property.name;
        }
        if (name == null || !SYNC_METHODS.has(name)) {
          return;
        }
        context.report({
          node: callee,
          messageId: 'noSyncIo',
          data: {name},
        });
      },
    };
  },
};
