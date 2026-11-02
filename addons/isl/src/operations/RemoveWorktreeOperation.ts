/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import type {CommandArg} from '../types';

import {Operation} from './Operation';

export class RemoveWorktreeOperation extends Operation {
  static opName = 'RemoveWorktree';

  constructor(private worktreePath: string) {
    super('RemoveWorktreeOperation');
  }

  getArgs(): Array<CommandArg> {
    return [
      {type: 'config', key: 'worktree.enabled', value: 'true'},
      'worktree',
      'remove',
      this.worktreePath,
      '--force',
    ];
  }
}
