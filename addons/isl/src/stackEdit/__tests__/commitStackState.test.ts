/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import type {RepoPath} from 'shared/types/common';
import type {ExportCommit, ExportFile, ExportStack} from 'shared/types/stack';
import type {CommitRev, FileRev} from '../commitStackState';

import {Map as ImMap, Set as ImSet, List} from 'immutable';
import {nullthrows} from 'shared/utils';
import {WDIR_NODE} from '../../dag/virtualCommit';
import {
  ABSENT_FILE,
  CommitIdx,
  CommitStackState,
  CommitState,
  FileIdx,
  FileState,
} from '../commitStackState';
import {FileStackState} from '../fileStackState';
import {describeAbsorbIdChunkMap} from './absorb.test';

export const exportCommitDefault: ExportCommit = {
  requested: true,
  immutable: false,
  author: 'test <test@example.com>',
  date: [0, 0],
  node: '',
  text: '',
};

// In this test we tend to use uppercase for commits (ex. A, B, C),
// and lowercase for files (ex. x, y, z).

/**
 * Created by `drawdag --no-files`:
 *
 *       C  # C/z.txt=(removed)
 *       |
 *       B  # B/y.txt=33 (renamed from x.txt)
 *       |
 *       A  # A/x.txt=33
 *       |  # A/z.txt=22
 *      /
 *     Z  # Z/z.txt=11
 *
 * and exported via `debugexportstack -r 'desc(A)::'`.
 */
const exportStack1: ExportStack = [
  {
    ...exportCommitDefault,
    immutable: true,
    node: 'Z_NODE',
    relevantFiles: {
      'x.txt': null,
      'z.txt': {data: '11'},
    },
    requested: false,
    text: 'Z',
  },
  {
    ...exportCommitDefault,
    files: {
      'x.txt': {data: '33'},
      'z.txt': {data: '22'},
    },
    node: 'A_NODE',
    parents: ['Z_NODE'],
    relevantFiles: {'y.txt': null},
    text: 'A',
  },
  {
    ...exportCommitDefault,
    files: {
      'x.txt': null,
      'y.txt': {copyFrom: 'x.txt', data: '33'},
    },
    node: 'B_NODE',
    parents: ['A_NODE'],
    relevantFiles: {'z.txt': {data: '22'}},
    text: 'B',
  },
  {
    ...exportCommitDefault,
    date: [0.0, 0],
    files: {'z.txt': null},
    node: 'C_NODE',
    parents: ['B_NODE'],
    text: 'C',
  },
];

/** Construct `CommitStackState` from a stack of files for testing purpose. */
export function linearStackWithFiles(
  stackFiles: Array<{[path: RepoPath]: ExportFile | null}>,
): CommitStackState {
  return new CommitStackState(
    stackFiles.map((files, i) => {
      const nextFiles = stackFiles.at(i + 1) ?? {};
      return {
        ...exportCommitDefault,
        node: `NODE_${i}`,
        parents: i === 0 ? [] : [`NODE_${i - 1}`],
        text: `Commit ${i}`,
        files,
        relevantFiles: Object.fromEntries(
          Object.entries(nextFiles).filter(([path, _file]) => !Object.hasOwn(files, path)),
        ),
      } as ExportCommit;
    }),
  );
}

describe('CommitStackState', () => {
  it('accepts an empty stack', () => {
    const stack = new CommitStackState([]);
    expect(stack.revs()).toStrictEqual([]);
  });

  it('accepts a stack without a public commit', () => {
    const stack = new CommitStackState([
      {
        ...exportCommitDefault,
        files: {'a.txt': {data: 'a'}},
        node: 'x',
        parents: [],
        text: 'A',
      },
    ]);
    expect(stack.revs()).toStrictEqual([0]);
  });

  it('rejects a stack with multiple roots', () => {
    const stack = [
      {...exportCommitDefault, node: 'Z1'},
      {...exportCommitDefault, node: 'Z2'},
    ];
    expect(() => new CommitStackState(stack)).toThrowError(
      'Multiple roots ["Z1","Z2"] is not supported',
    );
  });

  it('rejects a stack with merges', () => {
    const stack = [
      {...exportCommitDefault, node: 'A', parents: []},
      {...exportCommitDefault, node: 'B', parents: ['A']},
      {...exportCommitDefault, node: 'C', parents: ['A', 'B']},
    ];
    expect(() => new CommitStackState(stack)).toThrowError('Merge commit C is not supported');
  });

  it('rejects circular stack', () => {
    const stack = [
      {...exportCommitDefault, node: 'A', parents: ['B']},
      {...exportCommitDefault, node: 'B', parents: ['A']},
    ];
    expect(() => new CommitStackState(stack)).toThrowError();
  });

  it('provides file paths', () => {
    const stack = new CommitStackState(exportStack1);
    expect(stack.getAllPaths()).toStrictEqual(['x.txt', 'y.txt', 'z.txt']);
  });

  it('logs commit history', () => {
    const stack = new CommitStackState(exportStack1);
    expect(stack.revs()).toStrictEqual([0, 1, 2, 3]);
    expect([...stack.log(1 as CommitRev)]).toStrictEqual([1, 0]);
    expect([...stack.log(3 as CommitRev)]).toStrictEqual([3, 2, 1, 0]);
  });

  it('finds child commits via childRevs', () => {
    const stack = new CommitStackState(exportStack1);
    expect(stack.childRevs(0 as CommitRev)).toMatchInlineSnapshot(`
      [
        1,
      ]
    `);
    expect(stack.childRevs(1 as CommitRev)).toMatchInlineSnapshot(`
      [
        2,
      ]
    `);
    expect(stack.childRevs(2 as CommitRev)).toMatchInlineSnapshot(`
      [
        3,
      ]
    `);
    expect(stack.childRevs(3 as CommitRev)).toMatchInlineSnapshot(`[]`);
  });

  describe('log file history', () => {
    // [rev, path] => [rev, path, file]
    const extend = (stack: CommitStackState, revPathPairs: Array<[number, string]>) => {
      return revPathPairs.map(([rev, path]) => {
        const file =
          rev >= 0 ? stack.get(rev as CommitRev)?.files.get(path) : stack.bottomFiles.get(path);
        expect(file).toBe(stack.getFile(rev as CommitRev, path));
        return [rev, path, file];
      });
    };

    it('logs file history', () => {
      const stack = new CommitStackState(exportStack1);
      expect([...stack.logFile(3 as CommitRev, 'x.txt')]).toStrictEqual(
        extend(stack, [
          [2, 'x.txt'],
          [1, 'x.txt'],
        ]),
      );
      expect([...stack.logFile(3 as CommitRev, 'y.txt')]).toStrictEqual(
        extend(stack, [[2, 'y.txt']]),
      );
      expect([...stack.logFile(3 as CommitRev, 'z.txt')]).toStrictEqual(
        extend(stack, [
          [3, 'z.txt'],
          [1, 'z.txt'],
        ]),
      );
      // Changes in not requested commits (rev 0) are ignored.
      expect([...stack.logFile(3 as CommitRev, 'k.txt')]).toStrictEqual([]);
    });

    it('logs file history following renames', () => {
      const stack = new CommitStackState(exportStack1);
      expect([...stack.logFile(3 as CommitRev, 'y.txt', true)]).toStrictEqual(
        extend(stack, [
          [2, 'y.txt'],
          [1, 'x.txt'],
        ]),
      );
    });

    it('logs file history including the bottom', () => {
      const stack = new CommitStackState(exportStack1);
      ['x.txt', 'z.txt'].forEach(path => {
        expect([...stack.logFile(1 as CommitRev, path, true, true)]).toStrictEqual(
          extend(stack, [
            [1, path],
            // rev 0 does not change x.txt or z.txt
            [-1, path],
          ]),
        );
      });
    });

    it('parentFile follows rename to bottomFile', () => {
      const stack = new CommitStackState([
        {
          ...exportCommitDefault,
          relevantFiles: {
            'x.txt': {data: '11'},
            'z.txt': {data: '22'},
          },
          files: {
            'z.txt': {data: '33', copyFrom: 'x.txt'},
          },
          text: 'Commit Foo',
        },
      ]);
      const file = stack.getFile(0 as CommitRev, 'z.txt');
      expect(stack.getUtf8Data(file)).toBe('33');
      const [, , parentFileWithRename] = stack.parentFile(0 as CommitRev, 'z.txt', true);
      expect(stack.getUtf8Data(parentFileWithRename)).toBe('11');
      const [, , parentFile] = stack.parentFile(0 as CommitRev, 'z.txt', false);
      expect(stack.getUtf8Data(parentFile)).toBe('22');
    });
  });

  it('provides file contents at given revs', () => {
    const stack = new CommitStackState(exportStack1);
    expect(stack.getFile(0 as CommitRev, 'x.txt')).toBe(ABSENT_FILE);
    expect(stack.getFile(0 as CommitRev, 'y.txt')).toBe(ABSENT_FILE);
    expect(stack.getFile(0 as CommitRev, 'z.txt')).toMatchObject({data: '11'});
    expect(stack.getFile(1 as CommitRev, 'x.txt')).toMatchObject({data: '33'});
    expect(stack.getFile(1 as CommitRev, 'y.txt')).toBe(ABSENT_FILE);
    expect(stack.getFile(1 as CommitRev, 'z.txt')).toMatchObject({data: '22'});
    expect(stack.getFile(2 as CommitRev, 'x.txt')).toBe(ABSENT_FILE);
    expect(stack.getFile(2 as CommitRev, 'y.txt')).toMatchObject({data: '33'});
    expect(stack.getFile(2 as CommitRev, 'z.txt')).toMatchObject({data: '22'});
    expect(stack.getFile(3 as CommitRev, 'x.txt')).toBe(ABSENT_FILE);
    expect(stack.getFile(3 as CommitRev, 'y.txt')).toMatchObject({data: '33'});
    expect(stack.getFile(3 as CommitRev, 'z.txt')).toBe(ABSENT_FILE);
  });

  describe('builds FileStack', () => {
    it('for double renames', () => {
      // x.txt renamed to both y.txt and z.txt.
      const stack = new CommitStackState([
        {...exportCommitDefault, node: 'A', files: {'x.txt': {data: 'xx'}}},
        {
          ...exportCommitDefault,
          node: 'B',
          parents: ['A'],
          files: {
            'x.txt': null,
            'y.txt': {data: 'yy', copyFrom: 'x.txt'},
            'z.txt': {data: 'zz', copyFrom: 'x.txt'},
          },
        },
      ]);
      expect(stack.describeFileStacks()).toStrictEqual([
        // y.txt inherits x.txt's history.
        '0:./x.txt 1:A/x.txt(xx) 2:B/y.txt(yy)',
        // z.txt does not inherit x.txt's history (but still has a parent for diff rendering purpose).
        '0:A/x.txt(xx) 1:B/z.txt(zz)',
      ]);
    });

    it('for double copies', () => {
      // x.txt copied to both y.txt and z.txt.
      const stack = new CommitStackState([
        {...exportCommitDefault, node: 'A', files: {'x.txt': {data: 'xx'}}},
        {
          ...exportCommitDefault,
          node: 'B',
          parents: ['A'],
          files: {
            'y.txt': {data: 'yy', copyFrom: 'x.txt'},
            'z.txt': {data: 'zz', copyFrom: 'y.txt'},
          },
        },
      ]);
      expect(stack.describeFileStacks()).toStrictEqual([
        // y.txt connects to x.txt's history.
        '0:./x.txt 1:A/x.txt(xx) 2:B/y.txt(yy)',
        // z.txt does not connect to x.txt's history (but still have one parent for diff).
        '0:./y.txt 1:B/z.txt(zz)',
      ]);
    });

    it('for changes and copies', () => {
      // x.txt is changed, and copied to both y.txt and z.txt.
      const stack = new CommitStackState([
        {...exportCommitDefault, node: 'A', files: {'x.txt': {data: 'xx'}}},
        {
          ...exportCommitDefault,
          node: 'B',
          parents: ['A'],
          files: {
            'x.txt': {data: 'yy'},
            'y.txt': {data: 'xx', copyFrom: 'x.txt'},
            'z.txt': {data: 'xx', copyFrom: 'x.txt'},
          },
        },
      ]);
      expect(stack.describeFileStacks()).toStrictEqual([
        // x.txt has its own history.
        '0:./x.txt 1:A/x.txt(xx) 2:B/x.txt(yy)',
        // y.txt and z.txt do not share x.txt's history (but still have one parent for diff).
        '0:A/x.txt(xx) 1:B/y.txt(xx)',
        '0:A/x.txt(xx) 1:B/z.txt(xx)',
      ]);
    });

    it('for the the example stack', () => {
      const stack = new CommitStackState(exportStack1);
      expect(stack.describeFileStacks()).toStrictEqual([
        // x.txt: added by A, modified and renamed by B.
        '0:./x.txt 1:A/x.txt(33) 2:B/y.txt(33)',
        // z.txt: modified by A, deleted by C.
        '0:./z.txt(11) 1:A/z.txt(22) 2:C/z.txt',
      ]);
    });

    it('with rename tracking disabled', () => {
      const stack = new CommitStackState(exportStack1).buildFileStacks({followRenames: false});
      // no x.txt -> y.txt rename
      expect(stack.describeFileStacks()).toStrictEqual([
        '0:./x.txt 1:A/x.txt(33) 2:B/x.txt',
        '0:./z.txt(11) 1:A/z.txt(22) 2:C/z.txt',
        '0:./y.txt 1:B/y.txt(33)',
      ]);
    });
  });

  describe('calculates dependencies', () => {
    const e = exportCommitDefault;

    it('for content changes', () => {
      const stack = new CommitStackState([
        {...e, node: 'Z', requested: false, relevantFiles: {'x.txt': null}},
        {...e, node: 'A', parents: ['Z'], files: {'x.txt': {data: 'b\n'}}},
        {...e, node: 'B', parents: ['A'], files: {'x.txt': {data: 'a\nb\n'}}},
        {...e, node: 'C', parents: ['B'], files: {'x.txt': {data: 'a\nB\n'}}},
      ]);
      expect(stack.calculateDepMap()).toStrictEqual(
        new Map([
          [0, new Set()],
          [1, new Set()],
          [2, new Set()], // insertion at other insertion boundary is dependency-free
          [3, new Set([1])],
        ]),
      );
    });

    it('for file addition and deletion', () => {
      const stack = new CommitStackState([
        {...e, node: 'Z', requested: false, relevantFiles: {'x.txt': {data: 'a'}}},
        {...e, node: 'A', parents: ['Z'], files: {'x.txt': null}},
        {...e, node: 'B', parents: ['A'], files: {'x.txt': {data: 'a'}}},
        {...e, node: 'C', parents: ['B'], files: {'x.txt': null}},
      ]);
      expect(stack.calculateDepMap()).toStrictEqual(
        new Map([
          [0, new Set()],
          [1, new Set()],
          [2, new Set([1])], // commit B adds x.txt, depends on commit A's deletion.
          [3, new Set([2])], // commit C deletes x.txt, depends on commit B's addition.
        ]),
      );
    });

    it('for copies', () => {
      const stack = new CommitStackState([
        {...e, node: 'A', files: {'x.txt': {data: 'a'}}},
        {...e, node: 'B', parents: ['A'], files: {'y.txt': {data: 'a', copyFrom: 'x.txt'}}},
        {...e, node: 'C', parents: ['B'], files: {'z.txt': {data: 'a', copyFrom: 'x.txt'}}},
        {
          ...e,
          node: 'D',
          parents: ['C'],
          files: {'p.txt': {data: 'a', copyFrom: 'x.txt'}, 'q.txt': {data: 'a', copyFrom: 'z.txt'}},
        },
      ]);
      expect(stack.calculateDepMap()).toStrictEqual(
        new Map([
          [0, new Set()],
          [1, new Set([0])], // commit B copies commit A's x.txt to y.txt.
          [2, new Set([0])], // commit C copies commit A's x.txt to z.txt.
          [3, new Set([0, 2])], // commit D copies commit A's x.txt to p.txt, and commit C's z.txt to q.txt.
        ]),
      );
    });
  });

  describe('folding commits', () => {
    const e = exportCommitDefault;

    it('cannot be used for immutable commits', () => {
      const stack = new CommitStackState([
        {...e, node: 'A', immutable: true},
        {...e, node: 'B', parents: ['A'], immutable: false},
        {...e, node: 'C', parents: ['B'], immutable: false},
      ]);
      expect(stack.canFoldDown(1 as CommitRev)).toBeFalsy();
      expect(stack.canFoldDown(2 as CommitRev)).toBeTruthy();
    });

    it('cannot be used for out-of-range commits', () => {
      const stack = new CommitStackState([
        {...e, node: 'A'},
        {...e, node: 'B', parents: ['A']},
      ]);
      expect(stack.canFoldDown(0 as CommitRev)).toBeFalsy();
      expect(stack.canFoldDown(1 as CommitRev)).toBeTruthy();
      expect(stack.canFoldDown(2 as CommitRev)).toBeFalsy();
    });

    it('cannot be used for forks', () => {
      const stack = new CommitStackState([
        {...e, node: 'A'},
        {...e, node: 'B', parents: ['A']},
        {...e, node: 'C', parents: ['A']},
      ]);
      expect(stack.canFoldDown(1 as CommitRev)).toBeFalsy();
      expect(stack.canFoldDown(2 as CommitRev)).toBeFalsy();
    });

    it('works for simple edits', () => {
      let stack = new CommitStackState([
        {
          ...e,
          node: 'A',
          text: 'Commit A',
          parents: [],
          files: {'x.txt': {data: 'xx'}, 'y.txt': {data: 'yy'}},
        },
        {...e, node: 'B', text: 'Commit B', parents: ['A'], files: {'x.txt': {data: 'yy'}}},
        {...e, node: 'C', text: 'Commit C', parents: ['B'], files: {'x.txt': {data: 'zz'}}},
      ]);
      expect(stack.canFoldDown(1 as CommitRev)).toBeTruthy();
      stack = stack.foldDown(1 as CommitRev);
      expect(stack.stack.size).toBe(2);
      expect(stack.stack.get(0)?.toJS()).toMatchObject({
        key: 'A',
        files: {
          'x.txt': {data: 'yy'},
          'y.txt': {data: 'yy'},
        },
        originalNodes: new Set(['A', 'B']),
        text: 'Commit A, Commit B',
        parents: [],
      });
      expect(stack.stack.get(1)?.toJS()).toMatchObject({
        key: 'C',
        text: 'Commit C',
        parents: [0], // Commit C's parent is updated to Commit A.
      });
    });

    it('removes copyFrom appropriately', () => {
      let stack = new CommitStackState([
        {...e, node: 'A', parents: [], files: {'x.txt': {data: 'xx'}}},
        {...e, node: 'B', parents: ['A'], files: {'y.txt': {data: 'yy', copyFrom: 'x.txt'}}},
      ]);
      expect(stack.canFoldDown(1 as CommitRev)).toBeTruthy();
      stack = stack.foldDown(1 as CommitRev);
      expect(stack.stack.get(0)?.toJS()).toMatchObject({
        files: {
          'x.txt': {data: 'xx'},
          'y.txt': {data: 'yy'}, // no longer has "copyFrom", since 'x.txt' does not exist in commit A.
        },
      });
    });

    it('keeps copyFrom appropriately', () => {
      let stack = new CommitStackState([
        {...e, node: 'A', parents: [], files: {xt: {data: 'xx'}, yt: {data: 'yy'}}},
        {...e, node: 'B', parents: ['A'], files: {y1t: {data: 'yy', copyFrom: 'yt'}}},
        {
          ...e,
          node: 'C',
          parents: ['B'],
          files: {x1t: {data: 'x1', copyFrom: 'xt'}, y1t: {data: 'y1'}},
        },
      ]);
      // Fold B+C.
      expect(stack.canFoldDown(2 as CommitRev)).toBeTruthy();
      stack = stack.foldDown(2 as CommitRev);
      expect(stack.stack.get(1)?.toJS()).toMatchObject({
        files: {
          y1t: {data: 'y1', copyFrom: 'yt'}, // reuse copyFrom: 'yt' from commit B.
          x1t: {data: 'x1', copyFrom: 'xt'}, // reuse copyFrom: 'xt' from commit C.
        },
      });
    });

    it('chains renames', () => {
      let stack = new CommitStackState([
        {...e, node: 'A', parents: [], files: {xt: {data: 'xx'}}},
        {...e, node: 'B', parents: ['A'], files: {yt: {data: 'yy', copyFrom: 'xt'}, xt: null}},
        {...e, node: 'C', parents: ['B'], files: {zt: {data: 'zz', copyFrom: 'yt'}, yt: null}},
      ]);
      // Fold B+C.
      expect(stack.canFoldDown(2 as CommitRev)).toBeTruthy();
      stack = stack.foldDown(2 as CommitRev);
      expect(stack.stack.get(1)?.toJS()).toMatchObject({
        files: {
          xt: ABSENT_FILE.toJS(),
          // 'yt' is no longer considered changed.
          zt: {data: 'zz', copyFrom: 'xt'}, // 'xt'->'yt'->'zt' is folded to 'xt'->'zt'.
        },
      });
    });

    it('removes cancel-out changes', () => {
      let stack = new CommitStackState([
        {...e, node: 'A', parents: [], files: {xt: {data: 'xx'}}},
        {...e, node: 'B', parents: ['A'], files: {xt: {data: 'yy'}, zt: {data: 'zz'}}},
        {...e, node: 'C', parents: ['B'], files: {xt: {data: 'xx'}}},
      ]);
      // Fold B+C.
      expect(stack.canFoldDown(2 as CommitRev)).toBeTruthy();
      stack = stack.foldDown(2 as CommitRev);
      expect(stack.stack.get(1)?.toJS()).toMatchObject({
        files: {zt: {data: 'zz'}}, // changes to 'yt' is removed.
      });
    });
  });

  describe('dropping commits', () => {
    const e = exportCommitDefault;

    it('cannot be used for immutable commits', () => {
      const stack = new CommitStackState([
        {...e, node: 'A', immutable: true},
        {...e, node: 'B', parents: ['A'], immutable: true},
        {...e, node: 'C', parents: ['B'], immutable: false},
      ]);
      expect(stack.canDrop(0 as CommitRev)).toBeFalsy();
      expect(stack.canDrop(1 as CommitRev)).toBeFalsy();
      expect(stack.canDrop(2 as CommitRev)).toBeTruthy();
    });

    it('detects content dependencies', () => {
      const stack = new CommitStackState([
        {...e, node: 'A', files: {xx: {data: '0\n2\n'}}},
        {...e, node: 'B', parents: ['A'], files: {xx: {data: '0\n1\n2\n'}}},
        {...e, node: 'C', parents: ['B'], files: {xx: {data: '0\n1\n2\n3\n'}}},
        {...e, node: 'D', parents: ['C'], files: {xx: {data: '0\n1\n2\n4\n'}}},
      ]);
      expect(stack.canDrop(0 as CommitRev)).toBeFalsy();
      expect(stack.canDrop(1 as CommitRev)).toBeTruthy();
      expect(stack.canDrop(2 as CommitRev)).toBeFalsy(); // D depends on C
      expect(stack.canDrop(3 as CommitRev)).toBeTruthy();
    });

    it('detects commit graph dependencies', () => {
      const stack = new CommitStackState([
        {...e, node: 'A', files: {xx: {data: '1'}}},
        {...e, node: 'B', parents: ['A'], files: {xx: {data: '2'}}},
        {...e, node: 'C', parents: ['A'], files: {xx: {data: '3'}}},
        {...e, node: 'D', parents: ['C'], files: {xx: {data: '4'}}},
      ]);
      expect(stack.canDrop(0 as CommitRev)).toBeFalsy();
      expect(stack.canDrop(1 as CommitRev)).toBeTruthy();
      expect(stack.canDrop(2 as CommitRev)).toBeFalsy();
      expect(stack.canDrop(3 as CommitRev)).toBeTruthy();
    });

    it('for a change in the middle of a stack', () => {
      let stack = new CommitStackState([
        {...e, node: 'A', files: {xx: {data: 'p\ny\n'}}},
        {...e, node: 'B', parents: ['A'], files: {xx: {data: 'p\nx\ny\n'}}},
        {...e, node: 'C', parents: ['B'], files: {xx: {data: 'p\nx\ny\nz\n'}}},
      ]);
      expect(stack.canDrop(0 as CommitRev)).toBeFalsy();
      expect(stack.canDrop(1 as CommitRev)).toBeTruthy();
      expect(stack.canDrop(2 as CommitRev)).toBeTruthy();
      stack = stack.drop(1 as CommitRev);
      expect(stack.stack.size).toBe(2);
      expect(stack.stack.get(1)?.toJS()).toMatchObject({
        originalNodes: ['C'],
        files: {xx: {data: 'p\ny\nz\n'}},
      });
      expect(stack.stack.toArray().map(c => c.key)).toMatchObject(['A', 'C']);
    });
  });

  describe('reordering commits', () => {
    const e = exportCommitDefault;

    it('cannot be used for immutable commits', () => {
      const stack = new CommitStackState([
        {...e, node: 'A', immutable: true},
        {...e, node: 'B', parents: ['A'], immutable: true},
        {...e, node: 'C', parents: ['B'], immutable: false},
      ]);
      expect(stack.canReorder([0, 2, 1] as CommitRev[])).toBeFalsy();
      expect(stack.canReorder([1, 0, 2] as CommitRev[])).toBeFalsy();
      expect(stack.canReorder([0, 1, 2] as CommitRev[])).toBeTruthy();
    });

    it('respects content dependencies', () => {
      const stack = new CommitStackState([
        {...e, node: 'A', files: {xx: {data: '0\n2\n'}}},
        {...e, node: 'B', parents: ['A'], files: {xx: {data: '0\n1\n2\n'}}},
        {...e, node: 'C', parents: ['B'], files: {xx: {data: '0\n1\n2\n3\n'}}},
        {...e, node: 'D', parents: ['C'], files: {xx: {data: '0\n1\n2\n4\n'}}},
      ]);
      expect(stack.canReorder([0, 2, 3, 1] as CommitRev[])).toBeTruthy();
      expect(stack.canReorder([0, 2, 1, 3] as CommitRev[])).toBeTruthy();
      expect(stack.canReorder([0, 3, 2, 1] as CommitRev[])).toBeFalsy();
      expect(stack.canReorder([0, 3, 1, 2] as CommitRev[])).toBeFalsy();
    });

    it('refuses to reorder non-linear stack', () => {
      const stack = new CommitStackState([
        {...e, node: 'A', files: {xx: {data: '1'}}},
        {...e, node: 'B', parents: ['A'], files: {xx: {data: '2'}}},
        {...e, node: 'C', parents: ['A'], files: {xx: {data: '3'}}},
        {...e, node: 'D', parents: ['C'], files: {xx: {data: '4'}}},
      ]);
      expect(stack.canReorder([0, 2, 3, 1] as CommitRev[])).toBeFalsy();
      expect(stack.canReorder([0, 2, 1, 3] as CommitRev[])).toBeFalsy();
      expect(stack.canReorder([0, 1, 2, 3] as CommitRev[])).toBeFalsy();
    });

    it('can reorder a long stack', () => {
      const exportStack: ExportStack = [...Array(20).keys()].map(i => {
        return {...e, node: `A${i}`, parents: i === 0 ? [] : [`A${i - 1}`], files: {}};
      });
      const stack = new CommitStackState(exportStack);
      expect(stack.canReorder(stack.revs().reverse())).toBeTruthy();
    });

    it('reorders adjacent changes', () => {
      // Note: usually rev 0 is a public parent commit, rev 0 is not usually reordered.
      // But this test reorders rev 0 and triggers some interesting code paths.
      let stack = new CommitStackState([
        {...e, node: 'A', files: {xx: {data: '1\n'}}},
        {...e, node: 'B', parents: ['A'], files: {xx: {data: '1\n2\n'}}},
      ]);
      expect(stack.canReorder([1, 0] as CommitRev[])).toBeTruthy();
      stack = stack.reorder([1, 0] as CommitRev[]);
      expect(stack.stack.toArray().map(c => c.files.get('xx')?.data)).toMatchObject([
        '2\n',
        '1\n2\n',
      ]);
      expect(stack.stack.toArray().map(c => c.key)).toMatchObject(['B', 'A']);
      // Reorder back.
      expect(stack.canReorder([1, 0] as CommitRev[])).toBeTruthy();
      stack = stack.reorder([1, 0] as CommitRev[]);
      expect(stack.stack.toArray().map(c => c.files.get('xx')?.data)).toMatchObject([
        '1\n',
        '1\n2\n',
      ]);
      expect(stack.stack.toArray().map(c => c.key)).toMatchObject(['A', 'B']);
    });

    it('reorders content changes', () => {
      let stack = new CommitStackState([
        {...e, node: 'A', files: {xx: {data: '1\n1\n'}}},
        {...e, node: 'B', parents: ['A'], files: {xx: {data: '0\n1\n1\n'}}},
        {...e, node: 'C', parents: ['B'], files: {yy: {data: '0'}}}, // Does not change 'xx'.
        {...e, node: 'D', parents: ['C'], files: {xx: {data: '0\n1\n1\n2\n'}}},
        {...e, node: 'E', parents: ['D'], files: {xx: {data: '0\n1\n3\n1\n2\n'}}},
      ]);

      // A-B-C-D-E => A-C-E-B-D.
      let order = [0, 2, 4, 1, 3] as CommitRev[];
      expect(stack.canReorder(order)).toBeTruthy();
      stack = stack.reorder(order);
      const getNode = (r: CommitRev) => stack.stack.get(r)?.originalNodes?.first();
      const getParents = (r: CommitRev) => stack.stack.get(r)?.parents?.toJS();
      expect(stack.revs().map(getNode)).toMatchObject(['A', 'C', 'E', 'B', 'D']);
      expect(stack.revs().map(getParents)).toMatchObject([[], [0], [1], [2], [3]]);
      expect(stack.revs().map(r => stack.getFile(r, 'xx').data)).toMatchObject([
        '1\n1\n',
        '1\n1\n', // Not changed by 'C'.
        '1\n3\n1\n',
        '0\n1\n3\n1\n',
        '0\n1\n3\n1\n2\n',
      ]);
      expect(stack.revs().map(r => stack.getFile(r, 'yy').data)).toMatchObject([
        '',
        '0',
        '0',
        '0',
        '0',
      ]);

      // Reorder back. A-C-E-B-D => A-B-C-D-E.
      order = [0, 3, 1, 4, 2] as CommitRev[];
      expect(stack.canReorder(order)).toBeTruthy();
      stack = stack.reorder(order);
      expect(stack.revs().map(getNode)).toMatchObject(['A', 'B', 'C', 'D', 'E']);
      expect(stack.revs().map(getParents)).toMatchObject([[], [0], [1], [2], [3]]);
      expect(stack.revs().map(r => stack.getFile(r, 'xx').data)).toMatchObject([
        '1\n1\n',
        '0\n1\n1\n',
        '0\n1\n1\n',
        '0\n1\n1\n2\n',
        '0\n1\n3\n1\n2\n',
      ]);
    });
  });

  describe('calculating ImportStack', () => {
    it('skips all if nothing changed', () => {
      const stack = new CommitStackState(exportStack1);
      expect(stack.calculateImportStack()).toMatchObject([]);
    });

    it('skips unchanged commits', () => {
      // Edits B/y.txt, affects descendants C.
      const stack = new CommitStackState(exportStack1).updateEachFile((_rev, file, path) =>
        path === 'y.txt' ? file.set('data', '333') : file,
      );
      expect(stack.calculateImportStack()).toMatchObject([
        [
          'commit',
          {
            mark: ':r2',
            date: [0, 0],
            text: 'B',
            parents: ['A_NODE'],
            predecessors: ['B_NODE'],
            files: {
              'x.txt': null,
              'y.txt': {data: '333', copyFrom: 'x.txt', flags: ''},
            },
          },
        ],
        [
          'commit',
          {
            mark: ':r3',
            date: [0, 0],
            text: 'C',
            parents: [':r2'],
            predecessors: ['C_NODE'],
            files: {'z.txt': null},
          },
        ],
      ]);
    });

    it('follows reorder', () => {
      // Reorder B and C in the example stack.
      const stack = new CommitStackState(exportStack1).reorder([0, 1, 3, 2] as CommitRev[]);
      expect(stack.calculateImportStack({goto: 'B_NODE', preserveDirtyFiles: true})).toMatchObject([
        ['commit', {text: 'C'}],
        ['commit', {mark: ':r3', text: 'B'}],
        ['reset', {mark: ':r3'}],
      ]);
    });

    it('stays at the stack top on reorder', () => {
      // Reorder B and C in the example stack.
      const stack = new CommitStackState(exportStack1).reorder([0, 1, 3, 2] as CommitRev[]);
      expect(stack.calculateImportStack({goto: 'C_NODE'})).toMatchObject([
        ['commit', {text: 'C'}],
        ['commit', {mark: ':r3', text: 'B'}],
        ['goto', {mark: ':r3'}],
      ]);
    });

    it('hides dropped commits', () => {
      let stack = new CommitStackState(exportStack1);
      const revs = stack.revs();
      // Drop the last 2 commits: B and C.
      stack = stack.drop(revs[revs.length - 1]).drop(revs[revs.length - 2]);
      expect(stack.calculateImportStack()).toMatchObject([
        [
          'hide',
          {
            nodes: ['B_NODE', 'C_NODE'],
          },
        ],
      ]);
    });

    it('produces goto or reset command', () => {
      const stack = new CommitStackState(exportStack1).updateEachFile((_rev, file, path) =>
        path === 'y.txt' ? file.set('data', '333') : file,
      );
      expect(stack.calculateImportStack({goto: 3 as CommitRev})).toMatchObject([
        ['commit', {}],
        ['commit', {}],
        ['goto', {mark: ':r3'}],
      ]);
      expect(
        stack.calculateImportStack({goto: 3 as CommitRev, preserveDirtyFiles: true}),
      ).toMatchObject([
        ['commit', {}],
        ['commit', {}],
        ['reset', {mark: ':r3'}],
      ]);
    });

    it('optionally rewrites commit date', () => {
      // Swap the last 2 commits.
      const stack = new CommitStackState(exportStack1).reorder([0, 1, 3, 2] as CommitRev[]);
      expect(stack.calculateImportStack({rewriteDate: 40})).toMatchObject([
        ['commit', {date: [40, 0], text: 'C'}],
        ['commit', {date: [40, 0], text: 'B'}],
      ]);
    });

    it('setFile drops invalid "copyFrom"s', () => {
      // Commit A (x.txt) -> Commit B (y.txt, renamed from x.txt).
      const stack = new CommitStackState([
        {
          ...exportCommitDefault,
          files: {'x.txt': {data: '33'}},
          node: 'A_NODE',
          parents: [],
          relevantFiles: {'y.txt': null},
          text: 'A',
        },
        {
          ...exportCommitDefault,
          files: {'x.txt': null, 'y.txt': {data: '33', copyFrom: 'x.txt'}},
          node: 'B_NODE',
          parents: ['A_NODE'],
          text: 'B',
        },
      ]);

      // Invalid copyFrom is dropped.
      expect(
        stack
          .setFile(0 as CommitRev, 'x.txt', f => f.set('copyFrom', 'z.txt'))
          .getFile(0 as CommitRev, 'x.txt').copyFrom,
      ).toBeUndefined();

      // Creating "y.txt" in the parent commit (0) makes the child commit (1) drop copyFrom of "y.txt".
      expect(
        stack
          .setFile(0 as CommitRev, 'y.txt', f => f.merge({data: '33', flags: ''}))
          .getFile(1 as CommitRev, 'y.txt').copyFrom,
      ).toBeUndefined();

      // Dropping "x.txt" in the parent commit (0) makes the child commit (1) not copying from "x.txt".
      // The content of "y.txt" is not changed.
      const fileY = stack
        .setFile(0 as CommitRev, 'x.txt', _f => ABSENT_FILE)
        .getFile(1 as CommitRev, 'y.txt');
      expect(fileY.copyFrom).toBeUndefined();
      expect(fileY.data).toBe('33');
    });

    it('optionally skips wdir()', () => {
      const stack = new CommitStackState([
        {
          ...exportCommitDefault,
          files: {
            'x.txt': {data: '11'},
          },
          node: WDIR_NODE,
          parents: [],
          text: 'Temp commit',
        },
      ]).setFile(0 as CommitRev, 'x.txt', f => f.set('data', '22'));
      expect(stack.calculateImportStack()).toMatchInlineSnapshot(`
        [
          [
            "commit",
            {
              "author": "test <test@example.com>",
              "date": [
                0,
                0,
              ],
              "files": {
                "x.txt": {
                  "data": "22",
                  "flags": "",
                },
              },
              "mark": ":r0",
              "parents": [],
              "predecessors": [],
              "text": "Temp commit",
            },
          ],
        ]
      `);
      expect(stack.calculateImportStack({skipWdir: true})).toMatchInlineSnapshot(`[]`);
    });
  });

  describe('denseSubStack', () => {
    it('provides bottomFiles', () => {
      const stack = new CommitStackState(exportStack1);
      let subStack = stack.denseSubStack(List([3 as CommitRev])); // C
      // The bottom files contains z (deleted) and its content is before deletion.
      expect([...subStack.bottomFiles.keys()].sort()).toEqual(['z.txt']);
      expect(subStack.bottomFiles.get('z.txt')?.data).toBe('22');

      subStack = stack.denseSubStack(List([2, 3] as CommitRev[])); // B, C
      // The bottom files contains x (deleted), y (modified) and z (deleted).
      expect([...subStack.bottomFiles.keys()].sort()).toEqual(['x.txt', 'y.txt', 'z.txt']);
    });

    it('marks all files at every commit as changed', () => {
      const stack = new CommitStackState(exportStack1);
      const subStack = stack.denseSubStack(List([2, 3] as CommitRev[])); // B, C
      // All commits (B, C) should have 3 files (x.txt, y.txt, z.txt) marked as "changed".
      expect(subStack.stack.map(c => c.files.size).toJS()).toEqual([3, 3]);
      // All file stacks (x.txt, y.txt, z.txt) should have 3 revs (bottomFile, B, C).
      expect(subStack.fileStacks.map(f => f.revLength).toJS()).toEqual([3, 3, 3]);
    });
  });

  describe('insertEmpty', () => {
    const stack = new CommitStackState(exportStack1);
    const getRevs = (stack: CommitStackState) =>
      stack.stack.map(c => [c.rev, c.parents.toArray()]).toArray();

    it('updates revs of commits', () => {
      expect(getRevs(stack)).toEqual([
        [0, []],
        [1, [0]],
        [2, [1]],
        [3, [2]],
      ]);
      expect(getRevs(stack.insertEmpty(2 as CommitRev, 'foo'))).toEqual([
        [0, []],
        [1, [0]],
        [2, [1]],
        [3, [2]],
        [4, [3]],
      ]);
    });

    it('inserts at stack top', () => {
      expect(getRevs(stack.insertEmpty(4 as CommitRev, 'foo'))).toEqual([
        [0, []],
        [1, [0]],
        [2, [1]],
        [3, [2]],
        [4, [3]],
      ]);
    });

    it('uses the provided commit message', () => {
      const msg = 'provided message\nfoobar';
      ([0, 2, 4] as CommitRev[]).forEach(i => {
        expect(stack.insertEmpty(i, msg).stack.get(i)?.text).toBe(msg);
      });
    });

    it('provides unique keys for inserted commits', () => {
      const newStack = stack
        .insertEmpty(1 as CommitRev, '')
        .insertEmpty(1 as CommitRev, '')
        .insertEmpty(1 as CommitRev, '');
      const keys = newStack.stack.map(c => c.key);
      expect(keys.size).toBe(ImSet(keys).size);
    });

    // The "originalNodes" are useful for split to set predecessors correctly.
    it('preserves the originalNodes with splitFromRev', () => {
      ([1, 4] as CommitRev[]).forEach(i => {
        const newStack = stack.insertEmpty(i, '', 2 as CommitRev);
        expect(newStack.get(i)?.originalNodes).toBe(stack.get(2 as CommitRev)?.originalNodes);
        expect(newStack.get(i)?.originalNodes?.isEmpty()).toBeFalsy();
        const anotherStack = stack.insertEmpty(i, '');
        expect(anotherStack.get(i)?.originalNodes?.isEmpty()).toBeTruthy();
      });
    });
  });

  describe('applySubStack', () => {
    const stack = new CommitStackState(exportStack1);
    const subStack = stack.denseSubStack(List([2, 3] as CommitRev[]));
    const emptyStack = subStack.set('stack', List());

    const getChangedFiles = (state: CommitStackState, rev: number): Array<string> => {
      return [...nullthrows(state.stack.get(rev as CommitRev)).files.keys()].sort();
    };

    it('optimizes file changes by removing unmodified changes', () => {
      const newStack = stack.applySubStack(2 as CommitRev, 4 as CommitRev, subStack);
      expect(newStack.stack.size).toBe(stack.stack.size);
      // The original `stack` does not have unmodified changes.
      // To verify that `newStack` does not have unmodified changes, check it
      // against the original `stack`.
      stack.revs().forEach(i => {
        expect(getChangedFiles(newStack, i)).toEqual(getChangedFiles(stack, i));
      });
    });

    it('drops empty commits at the end of subStack', () => {
      // Change the 2nd commit in subStack to empty.
      const newSubStack = subStack.set(
        'stack',
        subStack.stack.setIn([1, 'files'], nullthrows(subStack.stack.get(0)).files),
      );
      // `applySubStack` should drop the 2nd commit in `newSubStack`.
      const newStack = stack.applySubStack(2 as CommitRev, 4 as CommitRev, newSubStack);
      newStack.assertRevOrder();
      expect(newStack.stack.size).toBe(stack.stack.size - 1);
    });

    it('rewrites revs for the remaining of the stack', () => {
      const newStack = stack.applySubStack(1 as CommitRev, 2 as CommitRev, emptyStack);
      newStack.assertRevOrder();
      [1, 2].forEach(i => {
        expect(newStack.stack.get(i)?.toJS()).toMatchObject({rev: i, parents: [i - 1]});
      });
    });

    it('rewrites revs for the inserted stack', () => {
      const newStack = stack.applySubStack(2 as CommitRev, 3 as CommitRev, subStack);
      newStack.assertRevOrder();
      [2, 3, 4].forEach(i => {
        expect(newStack.stack.get(i)?.toJS()).toMatchObject({rev: i, parents: [i - 1]});
      });
    });

    it('preserves file contents of the old stack', () => {
      // Add a file 'x.txt' deleted by the original stack.
      const newSubStack = subStack.set(
        'stack',
        List([
          CommitState({
            key: 'foo',
            files: ImMap([['x.txt', stack.getFile(1 as CommitRev, 'x.txt')]]),
          }),
        ]),
      );
      const newStack = stack.applySubStack(1 as CommitRev, 3 as CommitRev, newSubStack);

      // 'y.txt' was added by the old stack, not the new stack. So it is re-added
      // to preserve its old content.
      // 'x.txt' was added by the new stack, deleted by the old stack. So it is
      // re-deleted.
      expect(getChangedFiles(newStack, 2)).toEqual(['x.txt', 'y.txt', 'z.txt']);
      expect(newStack.getFile(2 as CommitRev, 'y.txt').data).toBe('33');
      expect(newStack.getFile(2 as CommitRev, 'x.txt')).toBe(ABSENT_FILE);
    });

    it('update keys to avoid conflict', () => {
      const oldKey = nullthrows(stack.stack.get(1)).key;
      const newSubStack = subStack.set('stack', subStack.stack.setIn([0, 'key'], oldKey));
      const newStack = stack.applySubStack(2 as CommitRev, 3 as CommitRev, newSubStack);

      // Keys are still unique.
      const keys = newStack.stack.map(c => c.key);
      const keysSet = ImSet(keys);
      expect(keys.size).toBe(keysSet.size);
    });

    it('drops ABSENT flag if content is not empty', () => {
      // x.txt was deleted by subStack rev 0 (B). We are moving it to be deleted by rev 1 (C).
      expect(subStack.getFile(0 as CommitRev, 'x.txt').flags).toBe(ABSENT_FILE.flags);
      // To break the deletion into done by 2 commits, we edit the file stack of 'x.txt'.
      const fileIdx = nullthrows(
        subStack.commitToFile.get(CommitIdx({rev: 0 as CommitRev, path: 'x.txt'})),
      ).fileIdx;
      const fileStack = nullthrows(subStack.fileStacks.get(fileIdx));
      // The file stack has 3 revs: (base, before deletion), (deleted at rev 0), (deleted at rev 1).
      expect(fileStack.convertToPlainText().toArray()).toEqual(['33', '', '']);
      const newFileStack = new FileStackState(['33', '3', '']);
      const newSubStack = subStack.setFileStack(fileIdx, newFileStack);
      expect(newSubStack.getUtf8Data(newSubStack.getFile(0 as CommitRev, 'x.txt'))).toBe('3');
      // Apply the file stack back to the main stack.
      const newStack = stack.applySubStack(2 as CommitRev, 4 as CommitRev, newSubStack);
      expect(newStack.stack.size).toBe(4);
      // Check that x.txt in rev 2 (B) is '3', not absent.
      const file = newStack.getFile(2 as CommitRev, 'x.txt');
      expect(file.data).toBe('3');
      expect(file.flags ?? '').not.toContain(ABSENT_FILE.flags);

      // Compare the old and new file stacks.
      // - x.txt deletion is now by commit 'C', not 'B'.
      // - x.txt -> y.txt rename is preserved.
      expect(stack.describeFileStacks(true)).toEqual([
        '0:./x.txt 1:A/x.txt(33) 2:B/y.txt(33)',
        '0:./z.txt(11) 1:A/z.txt(22) 2:C/z.txt',
      ]);
      expect(newStack.describeFileStacks(true)).toEqual([
        '0:./x.txt 1:A/x.txt(33) 2:B/x.txt(3) 3:C/x.txt',
        '0:./z.txt(11) 1:A/z.txt(22) 2:C/z.txt',
        '0:A/x.txt(33) 1:B/y.txt(33)',
      ]);
    });

    it('does not add ABSENT flag if content becomes empty', () => {
      // This was a herustics when `flags` are not handled properly. Now it is no longer needed.
      // y.txt was added by subStack rev 0 (B). We are moving it to be added by rev 1 (C).
      const fileIdx = nullthrows(
        subStack.commitToFile.get(CommitIdx({rev: 0 as CommitRev, path: 'y.txt'})),
      ).fileIdx;
      const fileStack = nullthrows(subStack.fileStacks.get(fileIdx));
      // The file stack has 3 revs: (base, before add), (add by rev 0), (unchanged by rev 1).
      expect(fileStack.convertToPlainText().toArray()).toEqual(['', '33', '33']);
      const newFileStack = new FileStackState(['', '', '33']);
      const newSubStack = subStack.setFileStack(fileIdx, newFileStack);
      // Apply the file stack back to the main stack.
      const newStack = stack.applySubStack(2 as CommitRev, 4 as CommitRev, newSubStack);
      // Check that y.txt in rev 2 (B) is absent, not just empty.
      const file = newStack.getFile(2 as CommitRev, 'y.txt');
      expect(file.data).toBe('');
      expect(file.flags).toBe('');
    });
  });

  describe('absorb', () => {
    const absorbStack1: ExportStack = [
      {
        ...exportCommitDefault,
        immutable: true,
        node: 'Z_NODE',
        relevantFiles: {
          'seq.txt': {data: '0\n'},
          'rename_from.txt': null,
        },
        requested: false,
        text: 'PublicCommit',
      },
      {
        ...exportCommitDefault,
        files: {
          'seq.txt': {data: '0\n1\n'},
          'rename_from.txt': {data: '1\n'},
        },
        node: 'A_NODE',
        parents: ['Z_NODE'],
        relevantFiles: {'rename_to.txt': null},
        text: 'CommitA',
      },
      {
        ...exportCommitDefault,
        files: {
          'seq.txt': {data: '0\n1\n2\n'},
          'rename_to.txt': {copyFrom: 'rename_from.txt', data: '1\n'},
          'rename_from.txt': null,
        },
        node: 'B_NODE',
        parents: ['A_NODE'],
        text: 'CommitB',
      },
      {
        ...exportCommitDefault,
        // Working copy changes. 012 => xyz.
        files: {
          'seq.txt': {data: 'x\ny\nz\n'},
          'rename_to.txt': {data: 'p\n'},
        },
        node: 'WDIR',
        parents: ['B_NODE'],
        text: 'Wdir',
      },
    ];

    it('can prepare for absorb', () => {
      const stack = new CommitStackState(absorbStack1);
      expect(stack.describeFileStacks()).toMatchInlineSnapshot(`
        [
          "0:./rename_from.txt 1:CommitA/rename_from.txt(1↵) 2:CommitB/rename_to.txt(1↵) 3:Wdir/rename_to.txt(p↵)",
          "0:./seq.txt(0↵) 1:CommitA/seq.txt(0↵1↵) 2:CommitB/seq.txt(0↵1↵2↵) 3:Wdir/seq.txt(x↵y↵z↵)",
        ]
      `);
      const stackWithAbsorb = stack.analyseAbsorb();
      expect(stackWithAbsorb.hasPendingAbsorb()).toBeTruthy();
      // The "1 => p" change in "rename_to.txt" is absorbed following file renames into rename_from.txt.
      // The "1 => y", "2 => z" changes in "seq.txt" are absorbed to CommitA and CommitB.
      // The "0 => x" change in "seq.txt" is left in the working copy as "0" is an immutable line (public commit).
      expect(stackWithAbsorb.describeFileStacks()).toMatchInlineSnapshot(`
        [
          "0:./rename_from.txt 1:CommitA/rename_from.txt(1↵;absorbed:p↵)",
          "0:./seq.txt(0↵) 1:CommitA/seq.txt(0↵1↵;absorbed:0↵y↵) 2:CommitB/seq.txt(0↵y↵2↵;absorbed:0↵y↵z↵) 3:Wdir/seq.txt(0↵y↵z↵;absorbed:x↵y↵z↵)",
        ]
      `);
      expect(describeAbsorbExtra(stackWithAbsorb)).toMatchInlineSnapshot(`
        {
          "0": [
            "0: -1↵ +p↵ Selected=1 Introduced=1",
          ],
          "1": [
            "0: -0↵ +x↵ Introduced=0",
            "1: -1↵ +y↵ Selected=1 Introduced=1",
            "2: -2↵ +z↵ Selected=2 Introduced=2",
          ],
        }
      `);
    });

    const absorbStack2: ExportStack = [
      {
        ...exportCommitDefault,
        immutable: true,
        node: 'Z_NODE',
        relevantFiles: {
          'a.txt': null,
        },
        requested: false,
        text: 'PublicCommit',
      },
      {
        ...exportCommitDefault,
        files: {
          'a.txt': {data: 'a1\na2\na3\n'},
        },
        node: 'A_NODE',
        parents: ['Z_NODE'],
        relevantFiles: {'b.txt': null},
        text: 'CommitA',
      },
      {
        ...exportCommitDefault,
        files: {
          'b.txt': {data: 'b1\nb2\nb3\n'},
        },
        relevantFiles: {
          'a.txt': {data: 'a1\na2\na3\n'},
          'c.txt': {data: 'c1\nc2\nc3\n'},
        },
        node: 'B_NODE',
        parents: ['A_NODE'],
        text: 'CommitB',
      },
      {
        ...exportCommitDefault,
        files: {
          'a.txt': {data: 'a1\na2\na3\nx1\n'},
          'b.txt': {data: 'b1\nb2\nb3\ny1\n'},
          'c.txt': {data: 'c1\nc2\nc3\nz1\n'},
        },
        node: 'C_NODE',
        parents: ['B_NODE'],
        text: 'CommitC',
      },
      {
        ...exportCommitDefault,
        files: {
          'a.txt': {data: 'A1\na2\na3\nX1\n'},
          'b.txt': {data: 'B1\nb2\nb3\nY1\n'},
          'c.txt': {data: 'C1\nC2\nc3\nz1\n'},
        },
        node: 'WDIR',
        parents: ['C_NODE'],
        text: 'Wdir',
      },
    ];

    it('provides absorb candidate revs', () => {
      const stack = new CommitStackState(absorbStack2).analyseAbsorb();
      expect(describeAbsorbExtra(stack)).toMatchInlineSnapshot(`
        {
          "0": [
            "0: -a1↵ +A1↵ Selected=1 Introduced=1",
            "1: -x1↵ +X1↵ Selected=2 Introduced=2",
          ],
          "1": [
            "0: -b1↵ +B1↵ Selected=1 Introduced=1",
            "1: -y1↵ +Y1↵ Selected=2 Introduced=2",
          ],
          "2": [
            "0: -c1↵ c2↵ +C1↵ C2↵ Introduced=0",
          ],
        }
      `);
      expect(describeAbsorbEditCommits(stack)).toEqual([
        {
          // The "a1 -> A1" change is applied to "CommitA" which introduced "a".
          // It can be applied to "CommitC" which changes "a.txt" too.
          // It cannot be applied to "CommitB" which didn't change "a.txt" (and
          // therefore not tracked by linelog).
          id: 'a.txt/0',
          diff: ['a1↵', 'A1↵'],
          selected: 'CommitA',
          candidates: ['CommitA', 'CommitC', 'Wdir'],
        },
        {
          // The "x1 -> X1" change is applied to "CommitC" which introduced "c".
          id: 'a.txt/1',
          diff: ['x1↵', 'X1↵'],
          selected: 'CommitC',
          candidates: ['CommitC', 'Wdir'],
        },
        {
          // The "b1 -> B1" change belongs to CommitB.
          id: 'b.txt/0',
          diff: ['b1↵', 'B1↵'],
          selected: 'CommitB',
          candidates: ['CommitB', 'CommitC', 'Wdir'],
        },
        {
          // The "y1 -> Y1" change belongs to CommitB.
          id: 'b.txt/1',
          diff: ['y1↵', 'Y1↵'],
          selected: 'CommitC',
          candidates: ['CommitC', 'Wdir'],
        },
        {
          // The "c1c2 -> C1C2" change is not automatically absorbed, since
          // "ccc" is public/immutable.
          id: 'c.txt/0',
          diff: ['c1↵c2↵', 'C1↵C2↵'],
          selected: undefined,
          // CommitC is a candidate because it modifies c.txt.
          candidates: ['CommitC', 'Wdir'],
        },
      ]);
    });

    it('updates absorb destination commit', () => {
      const stack = new CommitStackState(absorbStack2).analyseAbsorb();
      // Current state. Note the "-a1 +A1" has "Selected=1" where 1 is the "file stack rev".
      expect(stack.absorbExtra.get(0)?.get(0)?.selectedRev).toBe(1);
      // Move the "a1 -> A1" change from CommitA to CommitC.
      // See the above test's "describeAbsorbExtra" to confirm that "a1 -> A1"
      // has fileIdx=0 and absorbEditId=0.
      // CommitC has rev=3.
      const newStack = stack.setAbsorbEditDestination(0, 0, 3 as CommitRev);
      // "-a1 +A1" now has "Selected=2":
      expect(newStack.absorbExtra.get(0)?.get(0)?.selectedRev).toBe(2);
      expect(describeAbsorbExtra(newStack)).toMatchInlineSnapshot(`
        {
          "0": [
            "0: -a1↵ +A1↵ Selected=2 Introduced=1",
            "1: -x1↵ +X1↵ Selected=2 Introduced=2",
          ],
          "1": [
            "0: -b1↵ +B1↵ Selected=1 Introduced=1",
            "1: -y1↵ +Y1↵ Selected=2 Introduced=2",
          ],
          "2": [
            "0: -c1↵ c2↵ +C1↵ C2↵ Introduced=0",
          ],
        }
      `);
      // The A1 is now absorbed at CommitC.
      expect(newStack.describeFileStacks()).toMatchInlineSnapshot(`
        [
          "0:./a.txt 1:CommitA/a.txt(a1↵a2↵a3↵) 2:CommitC/a.txt(a1↵a2↵a3↵x1↵;absorbed:A1↵a2↵a3↵X1↵)",
          "0:./b.txt 1:CommitB/b.txt(b1↵b2↵b3↵;absorbed:B1↵b2↵b3↵) 2:CommitC/b.txt(B1↵b2↵b3↵y1↵;absorbed:B1↵b2↵b3↵Y1↵)",
          "0:./c.txt(c1↵c2↵c3↵) 1:CommitC/c.txt(c1↵c2↵c3↵z1↵) 2:Wdir/c.txt(c1↵c2↵c3↵z1↵;absorbed:C1↵C2↵c3↵z1↵)",
        ]
      `);
      // It can be moved back.
      const newStack2 = newStack.setAbsorbEditDestination(0, 0, 1 as CommitRev);
      expect(newStack2.absorbExtra.get(0)?.get(0)?.selectedRev).toBe(1);
      // It can be moved to wdir(), the top rev.
      const topRev = nullthrows(newStack2.revs().at(-1));
      const newStack3 = newStack2.setAbsorbEditDestination(0, 0, topRev);
      expect(newStack3.getAbsorbCommitRevs(0, 0).selectedRev).toBe(topRev);
    });

    it('updates getUtf8 with pending absorb edits', () => {
      const stack1 = new CommitStackState(absorbStack2).switchToFileStack();
      const get = (
        stack: CommitStackState,
        fileIdx: number,
        fileRev: number,
        considerAbsorb?: boolean,
      ) =>
        replaceNewLines(
          stack.getUtf8Data(
            FileState({data: FileIdx({fileIdx, fileRev: fileRev as FileRev})}),
            considerAbsorb,
          ),
        );
      expect(get(stack1, 0, 1)).toMatchInlineSnapshot(`"a1↵a2↵a3↵"`);
      // getUtf8Data considers the pending absorb (a1 -> A1).
      const stack2 = stack1.analyseAbsorb();
      expect(get(stack2, 0, 1)).toMatchInlineSnapshot(`"A1↵a2↵a3↵"`);
      // Can still ask for the content without absorb explicitly.
      expect(get(stack2, 0, 1, false)).toMatchInlineSnapshot(`"a1↵a2↵a3↵"`);
    });

    it('can apply absorb edits', () => {
      const beforeStack = new CommitStackState(absorbStack2).switchToFileStack().analyseAbsorb();
      expect(beforeStack.switchToFileStack().describeFileStacks()).toMatchInlineSnapshot(`
        [
          "0:./a.txt 1:CommitA/a.txt(a1↵a2↵a3↵;absorbed:A1↵a2↵a3↵) 2:CommitC/a.txt(A1↵a2↵a3↵x1↵;absorbed:A1↵a2↵a3↵X1↵)",
          "0:./b.txt 1:CommitB/b.txt(b1↵b2↵b3↵;absorbed:B1↵b2↵b3↵) 2:CommitC/b.txt(B1↵b2↵b3↵y1↵;absorbed:B1↵b2↵b3↵Y1↵)",
          "0:./c.txt(c1↵c2↵c3↵) 1:CommitC/c.txt(c1↵c2↵c3↵z1↵) 2:Wdir/c.txt(c1↵c2↵c3↵z1↵;absorbed:C1↵C2↵c3↵z1↵)",
        ]
      `);
      // After `applyAbsorbEdits`, "absorbed:" contents become real contents.
      const afterStack = beforeStack.applyAbsorbEdits();
      expect(afterStack.hasPendingAbsorb()).toBeFalsy();
      expect(describeAbsorbExtra(afterStack)).toMatchInlineSnapshot(`{}`);
      expect(afterStack.switchToFileStack().describeFileStacks()).toMatchInlineSnapshot(`
        [
          "0:./a.txt 1:CommitA/a.txt(A1↵a2↵a3↵) 2:CommitC/a.txt(A1↵a2↵a3↵X1↵) 3:Wdir/a.txt(A1↵a2↵a3↵X1↵)",
          "0:./b.txt 1:CommitB/b.txt(B1↵b2↵b3↵) 2:CommitC/b.txt(B1↵b2↵b3↵Y1↵) 3:Wdir/b.txt(B1↵b2↵b3↵Y1↵)",
          "0:./c.txt(c1↵c2↵c3↵) 1:CommitC/c.txt(c1↵c2↵c3↵z1↵) 2:Wdir/c.txt(C1↵C2↵c3↵z1↵)",
        ]
      `);
    });

    function describeAbsorbExtra(stack: CommitStackState) {
      return stack.absorbExtra.map(describeAbsorbIdChunkMap).toJS();
    }

    function replaceNewLines(text: string): string {
      return text.replaceAll('\n', '↵');
    }

    function describeAbsorbEditCommits(stack: CommitStackState) {
      const describeCommit = (rev: CommitRev) => nullthrows(stack.get(rev)).text;
      const result: object[] = [];
      stack.absorbExtra.forEach((absorbEdits, fileIdx) => {
        absorbEdits.forEach((absorbEdit, absorbEditId) => {
          const {candidateRevs, selectedRev} = stack.getAbsorbCommitRevs(fileIdx, absorbEditId);
          result.push({
            id: `${stack.getFileStackPath(fileIdx, absorbEdit.introductionRev)}/${absorbEditId}`,
            diff: [
              replaceNewLines(absorbEdit.oldLines.join('')),
              replaceNewLines(absorbEdit.newLines.join('')),
            ],
            candidates: candidateRevs.map(describeCommit),
            selected: selectedRev && describeCommit(selectedRev),
          });
        });
      });
      return result;
    }
  });
});
