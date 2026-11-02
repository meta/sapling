#!/usr/bin/env node
/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

// @ts-check

/* eslint-disable no-console */

const {spawn} = require('child_process');
const path = require('path');
const os = require('os');
const process = require('process');

const addons = __dirname;
const description = `Verifies the contents of this folder by running tests and linters.

Requirements:
- \`node\` and \`yarn\` are on the \`$PATH\`
- \`yarn install\` has already been run in the addons/ folder
- \`sl\` required to be on the PATH for isl integration tests`;

const TASK_LABEL_WIDTH = 14;
const COMMAND_LABEL_WIDTH = 34;

/**
 * `isl/` has by far the largest unit test suite, so it is the long pole of this script.
 * Give it more Jest workers than the other packages when the host has the cores for it,
 * but stay bounded so concurrent Jest pools do not exhaust a CI container's memory.
 */
const ISL_TEST_WORKERS = Math.max(
  2,
  Math.min(6, Math.floor((os.availableParallelism?.() ?? os.cpus().length) / 4)),
);

/**
 * @typedef {{
 *   useVendoredGrammars: boolean;
 *   skipIntegrationTests: boolean;
 * }} Args
 */

class CommandError extends Error {
  /** @type {string} */
  taskLabel;

  /** @type {string} */
  commandLabel;

  /** @type {string} */
  output;

  /**
   * @param {string} cwd
   * @param {string[]} command
   * @param {number | null} exitCode
   * @param {string} output
   */
  constructor(cwd, command, exitCode, output) {
    super(`command failed (${exitCode ?? 'unknown'}): ${command.join(' ')}`);
    this.name = 'CommandError';
    this.taskLabel = deriveTaskLabel(cwd);
    this.commandLabel = deriveCommandLabel(command);
    this.output = output;
  }
}

class Timer {
  /** @type {number} */
  #start;

  constructor() {
    this.#start = Date.now();
  }

  /**
   * @returns {string}
   */
  elapsed() {
    const durationSeconds = (Date.now() - this.#start) / 1000;
    return `${durationSeconds.toFixed(2)}s`;
  }
}

/**
 * @returns {Args}
 */
function parseArgs() {
  const args = process.argv.slice(2);

  /** @type {Args} */
  const parsed = {
    useVendoredGrammars: false,
    skipIntegrationTests: false,
  };

  for (const arg of args) {
    switch (arg) {
      case '--use-vendored-grammars':
        parsed.useVendoredGrammars = true;
        break;
      case '--skip-integration-tests':
        parsed.skipIntegrationTests = true;
        break;
      case '-h':
      case '--help':
        printHelp(0);
        break;
      default:
        printHelp(1, `Unknown argument: ${arg}`);
    }
  }

  return parsed;
}

/**
 * @param {number} exitCode
 * @param {string=} errorMessage
 * @returns {never}
 */
function printHelp(exitCode, errorMessage) {
  const lines = [
    'usage: verify-addons-folder.js [--use-vendored-grammars] [--skip-integration-tests]',
    '',
    description,
    '',
    'options:',
    '  --use-vendored-grammars   No-op. Provided for compatibility.',
    "  --skip-integration-tests  Don't run isl integrations tests",
    '  -h, --help                show this help message and exit',
  ];

  const output = lines.join('\n');
  if (errorMessage != null) {
    console.error(errorMessage);
    console.error(output);
  } else {
    console.log(output);
  }
  process.exit(exitCode);
}

/**
 * @param {Args} args
 * @returns {Promise<void>}
 */
async function verify(args) {
  await Promise.all([
    verifyPrettier(),
    verifyShared(),
    verifyComponents(),
    verifyTextmate(),
    verifyIsl(args),
    verifyInternal(),
  ]);
}

/** @returns {Promise<void>} */
async function verifyPrettier() {
  await runTask(['yarn', 'run', 'prettier-check'], addons);
}

/** @returns {Promise<void>} */
async function verifyShared() {
  await lintAndTest(path.join(addons, 'shared'));
}

/** @returns {Promise<void>} */
async function verifyComponents() {
  await lintAndTest(path.join(addons, 'components'));
}

/** @returns {Promise<void>} */
async function verifyTextmate() {
  const textmate = path.join(addons, 'textmate');
  await Promise.all([
    runTask(['yarn', 'run', 'tsc', '--noEmit'], textmate),
    runTask(['yarn', 'run', 'lint'], textmate),
  ]);
}

/** @returns {Promise<void>} */
async function verifyInternal() {
  await runTask(['yarn', 'run', 'verify-internal'], addons);
}

/**
 * Verifies `isl/` and `isl-server/` and `vscode/` as the builds are interdependent.
 * @param {Args} args
 * @returns {Promise<void>}
 */
async function verifyIsl(args) {
  const isl = path.join(addons, 'isl');
  const islServer = path.join(addons, 'isl-server');
  const vscode = path.join(addons, 'vscode');

  await runTask(['yarn', 'codegen'], islServer);
  await Promise.all([
    (async () => {
      await Promise.all([runTask(['yarn', 'build'], islServer), runTask(['yarn', 'build'], isl)]);
      await Promise.all([
        runTask(['yarn', 'build-extension'], vscode),
        runTask(['yarn', 'build-webview'], vscode),
      ]);
      await Promise.all([
        lintAndTest(isl, ISL_TEST_WORKERS),
        lintAndTest(islServer),
        lintAndTest(vscode),
      ]);
    })(),
    // Integration tests run serially (one Jest worker) and import sources, not build output,
    // so run them alongside the builds and unit tests instead of after them.
    args.skipIntegrationTests
      ? Promise.resolve()
      : runTask(['yarn', 'integration', '--watchAll=false'], isl),
  ]);
}

/**
 * @param {string} cwd
 * @param {number} testWorkers
 * @returns {Promise<void>}
 */
async function lintAndTest(cwd, testWorkers = 2) {
  await Promise.all([
    runTask(['yarn', 'run', 'lint'], cwd),
    runTask(['yarn', 'run', 'tsc', '--noEmit'], cwd),
    // Packages run concurrently. Bound each Jest pool so they do not each consume
    // the host's CPU count in workers and exhaust the CI container's memory.
    runTask(['yarn', 'test', '--watchAll=false', `--maxWorkers=${testWorkers}`], cwd),
  ]);
}

/**
 * @param {string[]} command
 * @param {string} cwd
 * @returns {Promise<void>}
 */
async function runTask(command, cwd) {
  const timer = new Timer();
  const taskLabel = formatTask(deriveTaskLabel(cwd));
  const commandLabel = formatCommand(deriveCommandLabel(command));
  await run(command, cwd);
  console.log(`${ok(taskLabel, commandLabel)} in ${timer.elapsed()}`);
}

/**
 * @param {string[]} command
 * @param {string} cwd
 * @returns {Promise<void>}
 */
function run(command, cwd) {
  return new Promise((resolve, reject) => {
    const child = spawn(command[0], command.slice(1), {
      cwd,
      env: process.env,
      stdio: ['ignore', 'pipe', 'pipe'],
    });

    /** @type {Buffer[]} */
    const stdoutChunks = [];
    /** @type {Buffer[]} */
    const stderrChunks = [];

    child.stdout.on('data', chunk => {
      stdoutChunks.push(Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk));
    });
    child.stderr.on('data', chunk => {
      stderrChunks.push(Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk));
    });
    child.on('error', error => {
      reject(new CommandError(cwd, command, null, String(error)));
    });
    child.on('close', code => {
      if (code === 0) {
        resolve();
        return;
      }

      const stdout = Buffer.concat(stdoutChunks).toString('utf8').trimEnd();
      const stderr = Buffer.concat(stderrChunks).toString('utf8').trimEnd();
      const output = [stdout, stderr].filter(part => part !== '').join('\n');
      reject(new CommandError(cwd, command, code, output));
    });
  });
}

/**
 * @param {string} cwd
 * @returns {string}
 */
function deriveTaskLabel(cwd) {
  if (cwd === addons) {
    return '';
  }

  return `${path.basename(cwd)}/`;
}

/**
 * @param {string[]} command
 * @returns {string}
 */
function deriveCommandLabel(command) {
  return command.join(' ');
}

/**
 * @param {string} value
 * @returns {string}
 */
function formatTask(value) {
  return value === '' ? ''.padEnd(TASK_LABEL_WIDTH) : `[${value}]`.padEnd(TASK_LABEL_WIDTH);
}

/**
 * @param {string} value
 * @returns {string}
 */
function formatCommand(value) {
  return value.padEnd(COMMAND_LABEL_WIDTH);
}

/**
 * @param {unknown} error
 */
function reportFailure(error) {
  if (error instanceof CommandError) {
    console.error(`\n${fail(formatTask(error.taskLabel), formatCommand(error.commandLabel))}:`);
    if (error.output !== '') {
      console.error(error.output);
    } else {
      console.error(error.message);
    }
    return;
  }

  console.error('\nFAIL unexpected error:');
  console.error(error);
}

const RED = '\u001b[0;31m';
const GREEN = '\u001b[0;32m';
const RESET = '\u001b[0m';

/**
 * @param {string} taskLabel
 * @param {string} commandLabel
 * @returns {string}
 */
function ok(taskLabel, commandLabel) {
  return `${GREEN}OK${RESET} ${taskLabel} ${commandLabel}`;
}

/**
 * @param {string} taskLabel
 * @param {string} commandLabel
 * @returns {string}
 */
function fail(taskLabel, commandLabel) {
  return `${RED}FAIL${RESET} ${taskLabel} ${commandLabel}`;
}

verify(parseArgs()).catch(error => {
  reportFailure(error);
  process.exit(1);
});
