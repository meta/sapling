/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

export type Level = 'info' | 'warn' | 'error';

const tzOptions: Intl.DateTimeFormatOptions & {
  // This is an ES2021 feature which is not properly reflected in the types but is available. TODO: update target to ES2021 to fix this.
  // See https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/Intl/DateTimeFormat/DateTimeFormat#fractionalseconddigits
  fractionalSecondDigits: 1 | 2 | 3;
} = {
  timeZoneName: 'short',
  // Show millisecond resolution for extra timing information in the logs
  fractionalSecondDigits: 3,
  // After setting fractionalSecondDigits, it requires setting all fields we want printed to 'numeric'.
  year: 'numeric',
  month: 'numeric',
  day: 'numeric',
  hour: 'numeric',
  minute: 'numeric',
  second: 'numeric',
};

/** Standardized logging interface for the server.
 * Format:  [YYY-MM-DD HH:MM:SS,MILLISECONDS TIMEZONE] [LEVEL] your message here
 * Example: [2025-01-14 17:54:55,092 GMT−8] [INFO] Setup analytics
 */
export abstract class Logger {
  abstract write(level: Level, timeStr: string, ...args: Parameters<typeof console.log>): void;

  private writeLog(level: Level, ...args: Parameters<typeof console.info>): void {
    const timeStr = `[${new Date().toLocaleString('sv', tzOptions)}]`;
    this.write(level, timeStr, ...args);
  }

  info(...args: Parameters<typeof console.info>): void {
    this.writeLog('info', ...args);
  }

  warn(...args: Parameters<typeof console.info>): void {
    this.writeLog('warn', ...args);
  }

  error(...args: Parameters<typeof console.info>): void {
    this.writeLog('error', ...args);
  }

  /** Get all previously logged contents, usually for filing a bug report. */
  getLogFileContents?(): Promise<string>;

  levelToString(level: Level): string {
    switch (level) {
      case 'info':
        return ' [INFO]';
      case 'warn':
        return ' [WARN]';
      case 'error':
        return '[ERROR]';
    }
  }
}

const GREY = '\x1b[38;5;8m';
const RED = '\x1b[38;5;9m';
const YELLOW = '\x1b[38;5;11m';
const CLEAR = '\x1b[0m';

/**
 * Logger that prints to stdout via `console`, with ANSI escape coloring for easy reading.
 * Typically used in dev mode.
 */
export class StdoutLogger extends Logger {
  write(level: Level, timeStr: string, ...args: Parameters<typeof console.log>): void {
    // eslint-disable-next-line no-console
    console[level]('%s%s%s%s', GREY, timeStr, this.levelToString(level), CLEAR, ...args);
  }

  levelToString(level: Level): string {
    switch (level) {
      case 'info':
        return GREY + ' [INFO]';
      case 'warn':
        return YELLOW + ' [WARN]';
      case 'error':
        return RED + '[ERROR]';
    }
  }
}
