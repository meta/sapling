/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import type {RecordOf} from 'immutable';
import type {Hash, RepoPath} from 'shared/types/common';
import type {
  ExportFile,
  ExportStack,
  ImportAction,
  ImportCommit,
  ImportStack,
  Mark,
} from 'shared/types/stack';
import type {AbsorbEdit, AbsorbEditId} from './absorb';
import type {CommitRev, FileFlag, FileMetadata, FileRev, FileStackIndex} from './common';

import deepEqual from 'fast-deep-equal';
import {Map as ImMap, Set as ImSet, List, Record, Seq, is} from 'immutable';
import {LRU, cachedMethod} from 'shared/LRU';
import {SelfUpdate} from 'shared/immutableExt';
import {firstLine, generatorContains, nullthrows, zip} from 'shared/utils';
import {
  commitMessageFieldsSchema,
  commitMessageFieldsToString,
  mergeCommitMessageFields,
  parseCommitMessageFields,
} from '../CommitInfoView/CommitMessageFields';
import {WDIR_NODE} from '../dag/virtualCommit';
import {t} from '../i18n';
import {readAtom} from '../jotaiUtils';
import {assert} from '../utils';
import {
  calculateAbsorbEditsForFileStack,
  embedAbsorbId,
  extractRevAbsorbId,
  revWithAbsorb,
} from './absorb';
import {
  ABSENT_FILE,
  ABSENT_FLAG,
  Base85,
  CommitIdx,
  CommitState,
  DataRef,
  DateTuple,
  FileIdx,
  FileState,
  isAbsent,
  isContentSame,
  isRename,
  isUtf8,
  toMetadata,
} from './common';
import {FileStackState} from './fileStackState';
import {max, next, prev} from './revMath';

type CommitStackProps = {
  /**
   * Original stack exported by `debugexportstack`. Immutable.
   * Useful to calculate "predecessor" information.
   */
  originalStack: Readonly<ExportStack>;

  /**
   * File contents at the bottom of the stack.
   *
   * For example, when editing stack with two commits A and B:
   *
   * ```
   *    B <- draft, rev 2
   *    |
   *    A <- draft, modifies foo.txt, rev 1
   *   /
   *  P <- public, does not modify foo.txt, rev 0
   * ```
   *
   * `bottomFiles['foo.txt']` would be the `foo.txt` content in P,
   * despite P does not change `foo.txt`.
   *
   * `bottomFiles` are considered immutable - stack editing operations
   * won't change `bottomFiles` directly.
   *
   * This also assumes there are only one root of the stack.
   *
   * This implies that: every file referenced or edited by any commit
   * in the stack will be present in this map. If a file was added
   * later in the stack, it is in this map and marked as absent.
   */
  bottomFiles: Readonly<Map<RepoPath, FileState>>;

  /**
   * Mutable commit stack. Indexed by rev.
   * Only stores "modified (added, edited, deleted)" files.
   */
  stack: List<CommitState>;

  /**
   * File stack states.
   * They are constructed on demand, and provide advanced features.
   */
  fileStacks: List<FileStackState>;

  /**
   * Map from `CommitIdx` (commitRev and path) to `FileIdx` (FileStack index and rev).
   * Note the commitRev could be -1, meaning that `bottomFiles` is used.
   */
  commitToFile: ImMap<CommitIdx, FileIdx>;

  /**
   * Reverse (swapped key and value) mapping of `commitToFile` mapping.
   * Note the commitRev could be -1, meaning that `bottomFiles` is used.
   */
  fileToCommit: ImMap<FileIdx, CommitIdx>;

  /**
   * Extra information for absorb.
   *
   * The state might also be calculated from the linelog file stacks
   * (by editing the linelogs, and calculating diffs). It's also tracked here
   * for ease-of-access.
   */
  absorbExtra: AbsorbExtra;
};

// Factory function for creating instances.
// Its type is the factory function (or the "class type" in OOP sense).
const CommitStackRecord = Record<CommitStackProps>({
  originalStack: [],
  bottomFiles: new Map(),
  stack: List(),
  fileStacks: List(),
  commitToFile: ImMap(),
  fileToCommit: ImMap(),
  absorbExtra: ImMap(),
});

/**
 * For absorb use-case, each file stack (keyed by the index of fileStacks) has
 * an AbsorbEditId->AbsorbEdit mapping.
 */
type AbsorbExtra = ImMap<FileStackIndex, ImMap<AbsorbEditId, AbsorbEdit>>;

// Type of *instances* created by the `CommitStackRecord`.
// This makes `CommitStackState` work more like a common OOP `class Foo`:
// `new Foo(...)` is a constructor, and `Foo` is the type of the instances,
// not the constructor or factory.
type CommitStackRecord = RecordOf<CommitStackProps>;

/**
 * A stack of commits with stack editing features.
 *
 * Provides read write APIs for editing the stack.
 * Under the hood, continuous changes to a same file are grouped
 * to file stacks. Part of analysis and edit operations are delegated
 * to corresponding file stacks.
 */
export class CommitStackState extends SelfUpdate<CommitStackRecord> {
  // Initial setup.

  /**
   * Construct from an exported stack. For efficient operations,
   * call `.buildFileStacks()` to build up states.
   *
   * `record` initialization is for internal use only.
   */
  constructor(originalStack?: Readonly<ExportStack>, record?: CommitStackRecord) {
    super(
      originalStack !== undefined
        ? CommitStackRecord({
            originalStack,
            bottomFiles: getBottomFilesFromExportStack(originalStack),
            stack: getCommitStatesFromExportStack(originalStack),
          })
        : record !== undefined
          ? record
          : CommitStackRecord(),
    );
  }

  // Delegates to SelfUpdate.inner

  get originalStack(): Readonly<ExportStack> {
    return this.inner.originalStack;
  }

  get bottomFiles(): Readonly<Map<RepoPath, FileState>> {
    return this.inner.bottomFiles;
  }

  get stack(): List<CommitState> {
    return this.inner.stack;
  }

  get fileStacks(): List<FileStackState> {
    return this.inner.fileStacks;
  }

  get commitToFile(): ImMap<CommitIdx, FileIdx> {
    return this.inner.commitToFile;
  }

  get fileToCommit(): ImMap<FileIdx, CommitIdx> {
    return this.inner.fileToCommit;
  }

  get absorbExtra(): AbsorbExtra {
    return this.inner.absorbExtra;
  }

  merge(props: Partial<CommitStackProps>): CommitStackState {
    return new CommitStackState(undefined, this.inner.merge(props));
  }

  set<K extends keyof CommitStackProps>(key: K, value: CommitStackProps[K]): CommitStackState {
    return new CommitStackState(undefined, this.inner.set(key, value));
  }

  // Read operations.

  /** Returns all valid revs. */
  revs(): CommitRev[] {
    return [...this.stack.keys()] as CommitRev[];
  }

  /** Find the first "Rev" that satisfy the condition. */
  findRev(predicate: (commit: CommitState, rev: CommitRev) => boolean): CommitRev | undefined {
    return this.stack.findIndex(predicate as (commit: CommitState, rev: number) => boolean) as
      CommitRev | undefined;
  }

  /** Find the last "Rev" that satisfy the condition. */
  findLastRev(predicate: (commit: CommitState, rev: CommitRev) => boolean): CommitRev | undefined {
    return this.stack.findLastIndex(predicate as (commit: CommitState, rev: number) => boolean) as
      CommitRev | undefined;
  }

  /**
   * Return mutable revs.
   * This filters out public or commits outside the original stack export request.
   */
  mutableRevs(): CommitRev[] {
    return [...this.stack.filter(c => c.immutableKind !== 'hash').map(c => c.rev)];
  }

  /**
   * Get the file at the given `rev`.
   *
   * Returns `ABSENT_FILE` if the file does not exist in the commit.
   * Throws if the stack does not have information about the path.
   *
   * Note this is different from `this.stack[rev].files.get(path)`,
   * since `files` only tracks modified files, not existing files
   * created from the bottom of the stack.
   *
   * If `rev` is `-1`, check `bottomFiles`.
   */
  getFile(rev: CommitRev, path: RepoPath): FileState {
    if (rev > -1) {
      for (const logRev of this.log(rev)) {
        const commit = this.stack.get(logRev);
        if (commit == null) {
          return ABSENT_FILE;
        }
        const file = commit.files.get(path);
        if (file !== undefined) {
          // Commit modified `file`.
          return file;
        }
      }
    }
    const file = this.bottomFiles.get(path) ?? ABSENT_FILE;
    if (file === undefined) {
      throw new Error(
        `file ${path} is not tracked by stack (tracked files: ${JSON.stringify(
          this.getAllPaths(),
        )})`,
      );
    }
    return file;
  }

  /**
   * Update a single file without affecting the rest of the stack.
   * Use `getFile` to get the `FileState`.
   *
   * Does some normalization:
   * - If a file is non-empty, then "absent" flag will be ignored.
   * - If a file is absent, then "copyFrom" and other flags will be ignored.
   * - If the "copyFrom" file does not exist in parent, it'll be ignored.
   * - If a file is not newly added, "copyFrom" will be ignored.
   *
   * `rev` cannot be `-1`. `bottomFiles` cannot be modified.
   */
  setFile(rev: CommitRev, path: RepoPath, editFile: (f: FileState) => FileState): CommitStackState {
    if (rev < 0) {
      throw new Error(`invalid rev for setFile: ${rev}`);
    }
    const origFile = this.getFile(rev, path);
    const newFile = editFile(origFile);
    let file = newFile;
    // Remove 'absent' for non-empty files.
    if (isAbsent(file) && this.getUtf8Data(file) !== '') {
      const newFlags: FileFlag = file.flags === ABSENT_FLAG ? '' : (file.flags ?? '');
      file = file.set('flags', newFlags);
    }
    // Remove other flags for absent files.
    if (isAbsent(file) && file.flags !== ABSENT_FLAG) {
      file = file.set('flags', ABSENT_FLAG);
    }
    // Check "copyFrom".
    const copyFrom = file.copyFrom;
    if (copyFrom != null) {
      const p1 = this.singleParentRev(rev) ?? (-1 as CommitRev);
      if (!isAbsent(this.getFile(p1, path))) {
        file = file.remove('copyFrom');
      } else {
        const copyFromFile = this.getFile(p1, copyFrom);
        if (isAbsent(copyFromFile)) {
          file = file.remove('copyFrom');
        }
      }
    }
    let newStack: CommitStackState = this.set(
      'stack',
      this.stack.setIn([rev, 'files', path], file),
    );
    // Adjust "copyFrom" of child commits.
    // If this file is deleted, then child commits cannot copy from it.
    if (isAbsent(file) && !isAbsent(origFile)) {
      newStack.childRevs(rev).forEach(childRev => {
        newStack = newStack.dropCopyFromIf(childRev, (_p, f) => f.copyFrom === path);
      });
    }
    // If this file is added, then the same path in the child commits cannot use copyFrom.
    if (!isAbsent(file) && isAbsent(origFile)) {
      newStack.childRevs(rev).forEach(childRev => {
        newStack = newStack.dropCopyFromIf(childRev, (p, _f) => p === path);
      });
    }
    return newStack;
  }

  dropCopyFromIf(
    rev: CommitRev,
    predicate: (path: RepoPath, file: FileState) => boolean,
  ): CommitStackState {
    const commit = this.stack.get(rev);
    if (commit == null) {
      return this;
    }
    const newFiles = commit.files.mapEntries(([path, file]) => {
      const newFile = predicate(path, file) ? file.remove('copyFrom') : file;
      return [path, newFile];
    });
    const newStack = this.stack.setIn([rev, 'files'], newFiles);
    return this.set('stack', newStack);
  }

  childRevs(rev: CommitRev): Array<CommitRev> {
    const result = [];
    for (let i = rev + 1; i < this.stack.size; ++i) {
      if (this.stack.get(i)?.parents?.contains(rev)) {
        result.push(i as CommitRev);
      }
    }
    return result;
  }

  /**
   * Get a list of paths changed by a commit.
   *
   * If `text` is set to `true`, only return text (content editable) paths.
   * If `text` is set to `false`, only return non-text (not content editable) paths.
   */
  getPaths(rev: CommitRev, props?: {text?: boolean}): RepoPath[] {
    const commit = this.stack.get(rev);
    if (commit == null) {
      return [];
    }
    const text = props?.text;
    const result = [];
    for (const [path, file] of commit.files) {
      if (text != null && isUtf8(file) !== text) {
        continue;
      }
      result.push(path);
    }
    return result.sort();
  }

  /** Get all file paths ever referred (via "copy from") or changed in the stack. */
  getAllPaths(): RepoPath[] {
    return [...this.bottomFiles.keys()].sort();
  }

  /** List revs, starting from the given rev. */
  *log(startRev: CommitRev): Generator<CommitRev, void> {
    const toVisit = [startRev];
    while (true) {
      const rev = toVisit.pop();
      if (rev === undefined || rev < 0) {
        break;
      }
      yield rev;
      const commit = this.stack.get(rev);
      if (commit != null) {
        // Visit parent commits.
        commit.parents.forEach(parentRev => {
          assert(parentRev < rev, 'parent rev must < child to prevent infinite loop in log()');
          toVisit.push(parentRev);
        });
      }
    }
  }

  /**
   * List revs that change the given file, starting from the given rev.
   * Optionally follow renames.
   * Optionally return bottom (rev -1) file.
   */
  *logFile(
    startRev: CommitRev,
    startPath: RepoPath,
    followRenames = false,
    includeBottom = false,
  ): Generator<[CommitRev, RepoPath, FileState], void> {
    let path = startPath;
    let lastFile = undefined;
    let lastPath = path;
    for (const rev of this.log(startRev)) {
      const commit = this.stack.get(rev);
      if (commit == null) {
        continue;
      }
      const file = commit.files.get(path);
      if (file !== undefined) {
        yield [rev, path, file];
        lastFile = file;
        lastPath = path;
      }
      if (followRenames && file?.copyFrom) {
        path = file.copyFrom;
      }
    }
    if (includeBottom && lastFile != null) {
      const bottomFile = this.bottomFiles.get(path);
      if (bottomFile != null && (path !== lastPath || !bottomFile.equals(lastFile))) {
        yield [-1 as CommitRev, path, bottomFile];
      }
    }
  }

  // "Save changes" related.

  /**
   * Produce a `ImportStack` useful for the `debugimportstack` command
   * to save changes.
   *
   * Note this function only returns parts that are changed. If nothing is
   * changed, this function might return an empty array.
   *
   * Options:
   * - goto: specify a rev or (old commit) to goto. The rev must be changed
   *   otherwise this parameter is ignored.
   * - preserveDirtyFiles: if true, do not change files in the working copy.
   *   Under the hood, this changes the "goto" to "reset".
   * - rewriteDate: if set, the unix timestamp (in seconds) for newly
   *   created commits.
   * - skipWdir: if set, skip changes for the wdir() virtual commit.
   *   This is desirable for operations like absorb, or "amend --to",
   *   where the working copy is expected to stay unchanged regardless
   *   of the current partial/chunk selection.
   *
   * Example use-cases:
   * - Editing a stack (clean working copy): goto = origCurrentHash
   * - commit -i: create new rev, goto = maxRev, preserveDirtyFiles = true
   * - amend -i, absorb: goto = origCurrentHash, preserveDirtyFiles = true
   */
  calculateImportStack(opts?: {
    goto?: CommitRev | Hash;
    preserveDirtyFiles?: boolean;
    rewriteDate?: number;
    skipWdir?: boolean;
  }): ImportStack {
    // Resolve goto to a Rev.
    // Special case: if it's at the old stack top, use the new stack top instead.
    const gotoRev: CommitRev | undefined =
      typeof opts?.goto === 'string'
        ? this.originalStack.at(-1)?.node == opts.goto
          ? this.stack.last()?.rev
          : this.findLastRev(c => c.originalNodes.has(opts.goto as string))
        : opts?.goto;

    // Figure out the first changed rev.
    const state = this.switchToFileContent();
    const originalState = new CommitStackState(state.originalStack);
    const firstChangedRev = state.stack.findIndex((commit, i) => {
      const originalCommit = originalState.stack.get(i);
      return originalCommit == null || !is(commit, originalCommit);
    });

    // Figure out what commits are changed.
    let changedCommits: CommitState[] =
      firstChangedRev < 0 ? [] : state.stack.slice(firstChangedRev).toArray();
    if (opts?.skipWdir) {
      changedCommits = changedCommits.filter(c => !c.originalNodes.contains(WDIR_NODE));
    }
    const changedRevs: Set<CommitRev> = new Set(changedCommits.map(c => c.rev));
    const revToMark = (rev: CommitRev): Mark => `:r${rev}`;
    const revToMarkOrHash = (rev: CommitRev): Mark | Hash => {
      if (changedRevs.has(rev)) {
        return revToMark(rev);
      } else {
        const nodes = nullthrows(state.stack.get(rev)).originalNodes;
        assert(nodes.size === 1, 'unchanged commits should have exactly 1 nodes');
        return nullthrows(nodes.first());
      }
    };

    // "commit" new commits based on state.stack.
    const actions: ImportAction[] = changedCommits.map(commit => {
      assert(commit.immutableKind !== 'hash', 'immutable commits should not be changed');
      const newFiles: {[path: RepoPath]: ExportFile | null} = Object.fromEntries(
        [...commit.files.entries()].map(([path, file]) => {
          if (isAbsent(file)) {
            return [path, null];
          }
          const newFile: ExportFile = {};
          if (typeof file.data === 'string') {
            newFile.data = file.data;
          } else if (file.data instanceof Base85) {
            newFile.dataBase85 = file.data.dataBase85;
          } else if (file.data instanceof DataRef) {
            newFile.dataRef = file.data.toJS();
          }
          if (file.copyFrom != null) {
            newFile.copyFrom = file.copyFrom;
          }
          if (file.flags != null) {
            newFile.flags = file.flags;
          }
          return [path, newFile];
        }),
      );
      // Ensure the text is not empty with a filler title.
      const text =
        commit.text.trim().length === 0 ||
        // if a commit template is used, but the title is not given, then we may have non-title text.
        // sl would trim the leading whitespace, which can end up using the commit template as the commit title.
        // Instead, use the same filler title.
        commit.text[0] === '\n'
          ? t('(no title provided)') + commit.text
          : commit.text;
      const importCommit: ImportCommit = {
        mark: revToMark(commit.rev),
        author: commit.author,
        date: [opts?.rewriteDate ?? commit.date.unix, commit.date.tz],
        text,
        parents: commit.parents.toArray().map(revToMarkOrHash),
        predecessors: commit.originalNodes.toArray().filter(n => n !== WDIR_NODE),
        files: newFiles,
      };
      return ['commit', importCommit];
    });

    // "goto" or "reset" as requested.
    if (gotoRev != null && changedRevs.has(gotoRev)) {
      if (opts?.preserveDirtyFiles) {
        actions.push(['reset', {mark: revToMark(gotoRev)}]);
      } else {
        actions.push(['goto', {mark: revToMark(gotoRev)}]);
      }
    }

    // "hide" commits that disappear from state.originalStack => state.stack.
    // Only requested mutable commits are considered.
    const coveredNodes: Set<Hash> = state.stack.reduce((acc, commit) => {
      commit.originalNodes.forEach((n: Hash): Set<Hash> => acc.add(n));
      return acc;
    }, new Set<Hash>());
    const orphanedNodes: Hash[] = state.originalStack
      .filter(c => c.requested && !c.immutable && !coveredNodes.has(c.node))
      .map(c => c.node);
    if (orphanedNodes.length > 0) {
      actions.push(['hide', {nodes: orphanedNodes}]);
    }

    return actions;
  }

  // File stack related.

  /**
   * Get the parent version of a file and its introducing rev.
   * If the returned `rev` is -1, it means the file comes from
   * "bottomFiles", aka. its introducing rev is outside the stack.
   */
  parentFile(
    rev: CommitRev,
    path: RepoPath,
    followRenames = true,
  ): [CommitRev, RepoPath, FileState] {
    let prevRev = -1 as CommitRev;
    let prevPath = path;
    let prevFile = nullthrows(this.bottomFiles.get(path));
    const includeBottom = true;
    const logFile = this.logFile(rev, path, followRenames, includeBottom);
    for (const [logRev, logPath, file] of logFile) {
      if (logRev !== rev) {
        [prevRev, prevPath] = [logRev, logPath];
        prevFile = file;
        break;
      }
    }
    return [prevRev, prevPath, prevFile];
  }

  /** Assert that the revs are in the right order. */
  assertRevOrder() {
    assert(
      this.stack.every(c => c.parents.every(p => p < c.rev)),
      'parent rev should < child rev',
    );
    assert(
      this.stack.every((c, i) => c.rev === i),
      'rev should equal to stack index',
    );
  }

  // Absorb related {{{

  /** Check if there is a pending absorb in this stack */
  hasPendingAbsorb(): boolean {
    return !this.inner.absorbExtra.isEmpty();
  }

  /**
   * Prepare for absorb use-case. Break down "wdir()" edits into the stack
   * with special revs so they can be later moved around.
   * See `calculateAbsorbEditsForFileStack` for details.
   *
   * This function assumes the stack top is "wdir()" to absorb, and the stack
   * bottom is immutable (public()).
   */
  analyseAbsorb(): CommitStackState {
    const stack = this.switchToFileStack();
    const wdirCommitRev = stack.stack.size - 1;
    assert(wdirCommitRev > 0, 'stack cannot be empty');
    let newFileStacks = stack.fileStacks;
    let absorbExtra: AbsorbExtra = ImMap();
    stack.fileStacks.forEach((fileStack, fileIdx) => {
      const topFileRev = prev(fileStack.revLength);
      if (topFileRev < 0) {
        // Empty file stack. Skip.
        return;
      }
      const rev = stack.fileToCommit.get(FileIdx({fileIdx, fileRev: topFileRev}))?.rev;
      if (rev != wdirCommitRev) {
        // wdir() did not change this file. Skip.
        return;
      }
      const [newFileStack, absorbMap] = calculateAbsorbEditsForFileStack(fileStack, {
        fileStackIndex: fileIdx,
      });
      absorbExtra = absorbExtra.set(fileIdx, absorbMap);
      newFileStacks = newFileStacks.set(fileIdx, newFileStack);
    });
    const newStackInner = stack.inner.set('fileStacks', newFileStacks);
    const newStack = new CommitStackState(undefined, newStackInner).set('absorbExtra', absorbExtra);
    return newStack;
  }

  /**
   * For an absorb edit, defined by `fileIdx`, and `absorbEditId`, return the
   * currently selected and possible "absorb into" commit revs.
   *
   * The edit can be looked up from the `absorbExtra` state.
   */
  getAbsorbCommitRevs(
    fileIdx: number,
    absorbEditId: AbsorbEditId,
  ): {candidateRevs: ReadonlyArray<CommitRev>; selectedRev?: CommitRev} {
    const fileStack = nullthrows(this.fileStacks.get(fileIdx));
    const edit = nullthrows(this.absorbExtra.get(fileIdx)?.get(absorbEditId));
    const toCommitRev = (fileRev: FileRev | null | undefined): CommitRev | undefined => {
      if (fileRev == null) {
        return undefined;
      }
      return this.fileToCommit.get(FileIdx({fileIdx, fileRev}))?.rev;
    };
    // diffChunk uses fileRev, map it to commitRev.
    const selectedRev = toCommitRev(edit.selectedRev);
    const startCandidateFileRev = Math.max(1, edit.introductionRev); // skip file rev 0 (bottomFiles)
    const endCandidateFileRev = fileStack.revLength;
    const candidateRevs: CommitRev[] = [];
    for (let fileRev = startCandidateFileRev; fileRev <= endCandidateFileRev; ++fileRev) {
      const rev = toCommitRev(fileRev as FileRev);
      // Skip immutable (public) commits.
      if (rev != null && this.get(rev)?.immutableKind !== 'hash') {
        candidateRevs.push(rev);
      }
    }
    return {selectedRev, candidateRevs};
  }

  /**
   * Filter `absorbExtra` by commit rev.
   *
   * Only returns a subset of `absorbExtra` that has the `rev` selected.
   */
  absorbExtraByCommitRev(rev: CommitRev): AbsorbExtra {
    const commit = this.get(rev);
    const isWdir = commit?.originalNodes.contains(WDIR_NODE);
    return ImMap<FileStackIndex, ImMap<AbsorbEditId, AbsorbEdit>>().withMutations(mut => {
      let result = mut;
      this.absorbExtra.forEach((edits, fileStackIndex) => {
        edits.forEach((edit, editId) => {
          assert(edit.absorbEditId === editId, 'absorbEditId should match its map key');
          const fileRev = edit.selectedRev;
          const fileIdx = edit.fileStackIndex;
          const selectedCommitRev =
            fileRev != null &&
            fileIdx != null &&
            this.fileToCommit.get(FileIdx({fileIdx, fileRev}))?.rev;
          if (selectedCommitRev === rev || (edit.selectedRev == null && isWdir)) {
            if (!result.has(fileStackIndex)) {
              result = result.set(fileStackIndex, ImMap<AbsorbEditId, AbsorbEdit>());
            }
            result = result.setIn([fileStackIndex, editId], edit);
          }
        });
      });
      return result;
    });
  }

  /**
   * Calculates the "candidateRevs" for all absorb edits.
   *
   * For example, in a 26-commit stack A..Z, only C and K changes a.txt, E and J
   * changes b.txt. When the user wants to absorb changes from a.txt and b.txt,
   * we only show 4 relevant commits: C, E, J, K.
   *
   * This function does not report public commits.
   */
  getAllAbsorbCandidateCommitRevs(): Set<CommitRev> {
    const result = new Set<CommitRev>();
    this.absorbExtra.forEach((edits, fileIdx) => {
      edits.forEach((_edit, absorbEditId) => {
        this.getAbsorbCommitRevs(fileIdx, absorbEditId)?.candidateRevs.forEach(rev => {
          if (this.get(rev)?.immutableKind !== 'hash') {
            result.add(rev);
          }
        });
      });
    });
    return result;
  }

  /**
   * Set `rev` as the "target commit" (amend --to) of an "absorb edit".
   * Happens when the user moves the absorb edit among candidate commits.
   *
   * Throws if the edit cannot be fulfilled, for example, the `commitRev` is
   * before the commit introducing the change (conflict), or if the `commitRev`
   * does not touch the file being edited (current limitation, might be lifted).
   */
  setAbsorbEditDestination(
    fileIdx: number,
    absorbEditId: AbsorbEditId,
    commitRev: CommitRev,
  ): CommitStackState {
    assert(this.hasPendingAbsorb(), 'stack is not prepared for absorb');
    const fileStack = nullthrows(this.fileStacks.get(fileIdx));
    const edit = nullthrows(this.absorbExtra.get(fileIdx)?.get(absorbEditId));
    const selectedFileRev = edit.selectedRev;
    if (selectedFileRev != null) {
      const currentCommitRev = this.fileToCommit.get(
        FileIdx({fileIdx, fileRev: selectedFileRev}),
      )?.rev;
      if (currentCommitRev === commitRev) {
        // No need to edit.
        return this;
      }
    }
    // Figure out the "file rev" from "commit rev", since we don't know the
    // "path" of the file at the "commitRev", for now, we just naively looks up
    // the fileRev one by one... for now
    for (let fileRev = max(edit.introductionRev, 1); ; fileRev = next(fileRev)) {
      const candidateCommitRev = this.fileToCommit.get(FileIdx({fileIdx, fileRev}))?.rev;
      if (candidateCommitRev == null) {
        break;
      }
      if (candidateCommitRev === commitRev) {
        // Update linelog to move the edit to "fileRev".
        const newFileRev = embedAbsorbId(fileRev, absorbEditId);
        const newFileStack = fileStack.remapRevs(rev =>
          !Number.isInteger(rev) && extractRevAbsorbId(rev)[1] === absorbEditId ? newFileRev : rev,
        );
        // Update the absorb extra too.
        const newEdit = edit.set('selectedRev', fileRev);
        const newAbsorbExtra = this.absorbExtra.setIn([fileIdx, absorbEditId], newEdit);
        // It's possible that "wdir()" is all absorbed, the new stack is
        // shorter than the original stack. So we bypass the length check.
        const newStack = this.setFileStackInternal(fileIdx, newFileStack).set(
          'absorbExtra',
          newAbsorbExtra,
        );
        return newStack;
      }
    }
    throw new Error('setAbsorbIntoRev did not find corresponding commit to absorb');
  }

  /**
   * Apply pending absorb edits.
   *
   * After this, absorb edits can no longer be edited by `setAbsorbEditDestination`,
   * `hasPendingAbsorb()` returns `false`, and `calculateImportStack()` can be used.
   */
  applyAbsorbEdits(): CommitStackState {
    if (!this.hasPendingAbsorb()) {
      return this;
    }
    return this.switchToFileContent().set('absorbExtra', ImMap());
  }

  // }}} (absorb related)

  /**
   * (Re-)build file stacks and mappings.
   *
   * If `followRenames` is true, then attempt to follow renames
   * when building linelogs (default: true).
   */
  buildFileStacks(opts?: BuildFileStackOptions): CommitStackState {
    const fileStacks: FileStackState[] = [];
    let commitToFile = ImMap<CommitIdx, FileIdx>();
    let fileToCommit = ImMap<FileIdx, CommitIdx>();

    const followRenames = opts?.followRenames ?? true;

    this.assertRevOrder();

    const processFile = (
      state: CommitStackState,
      rev: CommitRev,
      file: FileState,
      path: RepoPath,
    ) => {
      const [prevRev, prevPath, prevFile] = state.parentFile(rev, path, followRenames);
      if (isUtf8(file)) {
        // File was added or modified and has utf-8 content.
        let fileAppended = false;
        if (prevRev >= 0) {
          // Try to reuse an existing file stack.
          const prev = commitToFile.get(CommitIdx({rev: prevRev, path: prevPath}));
          if (prev) {
            const prevFileStack = fileStacks[prev.fileIdx];
            // File stack history is linear. Only reuse it if its last
            // rev matches `prevFileRev`
            if (prevFileStack.source.revLength === prev.fileRev + 1) {
              const fileRev = next(prev.fileRev);
              fileStacks[prev.fileIdx] = prevFileStack.editText(
                fileRev,
                state.getUtf8Data(file),
                false,
              );
              const cIdx = CommitIdx({rev, path});
              const fIdx = FileIdx({fileIdx: prev.fileIdx, fileRev});
              commitToFile = commitToFile.set(cIdx, fIdx);
              fileToCommit = fileToCommit.set(fIdx, cIdx);
              fileAppended = true;
            }
          }
        }
        if (!fileAppended) {
          // Cannot reuse an existing file stack. Create a new file stack.
          const fileIdx = fileStacks.length;
          let fileTextList = [state.getUtf8Data(file)];
          let fileRev = 0 as FileRev;
          if (isUtf8(prevFile)) {
            // Use "prevFile" as rev 0 (immutable public).
            fileTextList = [state.getUtf8Data(prevFile), ...fileTextList];
            const cIdx = CommitIdx({rev: prevRev, path: prevPath});
            const fIdx = FileIdx({fileIdx, fileRev});
            commitToFile = commitToFile.set(cIdx, fIdx);
            fileToCommit = fileToCommit.set(fIdx, cIdx);
            fileRev = 1 as FileRev;
          }
          const fileStack = new FileStackState(fileTextList);
          fileStacks.push(fileStack);
          const cIdx = CommitIdx({rev, path});
          const fIdx = FileIdx({fileIdx, fileRev});
          commitToFile = commitToFile.set(cIdx, fIdx);
          fileToCommit = fileToCommit.set(fIdx, cIdx);
        }
      }
    };

    // Migrate off 'fileStack' type, since we are going to replace the file stacks.
    const state = this.switchToFileContent();

    state.stack.forEach((commit, revNumber) => {
      const rev = revNumber as CommitRev;
      const files = commit.files;
      // Process order: renames, non-copy, copies.
      const priorityFiles: [number, RepoPath, FileState][] = [...files.entries()].map(
        ([path, file]) => {
          const priority =
            followRenames && isRename(commit, path) ? 0 : file.copyFrom == null ? 1 : 2;
          return [priority, path, file];
        },
      );
      const renamed = new Set<RepoPath>();
      priorityFiles
        .sort(([aPri, aPath, _aFile], [bPri, bPath, _bFile]) =>
          aPri < bPri || (aPri === bPri && aPath < bPath) ? -1 : 1,
        )
        .forEach(([priority, path, file]) => {
          // Skip already "renamed" absent files.
          let skip = false;
          if (priority === 0 && file.copyFrom != null) {
            renamed.add(file.copyFrom);
          } else {
            skip = isAbsent(file) && renamed.has(path);
          }
          if (!skip) {
            processFile(state, rev, file, path);
          }
        });
    });

    return state.merge({
      fileStacks: List(fileStacks),
      commitToFile,
      fileToCommit,
    });
  }

  /**
   * Build file stacks if it's not present.
   * This is part of the `switchToFileStack` implementation detail.
   * It does not ensure the file stack references are actually used for `getFile`.
   * For public API, use `switchToFileStack` instead.
   */
  private maybeBuildFileStacks(opts?: BuildFileStackOptions): CommitStackState {
    return this.fileStacks.size === 0 ? this.buildFileStacks(opts) : this;
  }

  /**
   * Switch file contents to use FileStack as source of truth.
   * Useful when using FileStack to edit files.
   */
  switchToFileStack(): CommitStackState {
    const state = this.maybeBuildFileStacks();
    return state.updateEachFile((rev, file, path) => {
      if (typeof file.data === 'string') {
        const index = state.commitToFile.get(CommitIdx({rev, path}));
        if (index != null) {
          return file.set('data', index);
        }
      }
      return file;
    });
  }

  /**
   * Switch file contents to use string as source of truth.
   * Useful when rebuilding FileStack.
   */
  switchToFileContent(): CommitStackState {
    return this.updateEachFile((_rev, file) => {
      if (typeof file.data !== 'string' && isUtf8(file)) {
        const data = this.getUtf8Data(file);
        return file.set('data', data);
      }
      return file;
    }).merge({
      fileStacks: List(),
      commitToFile: ImMap(),
      fileToCommit: ImMap(),
    });
  }

  /**
   * Iterate through all changed files via the given function.
   */
  updateEachFile(
    func: (commitRev: CommitRev, file: FileState, path: RepoPath) => FileState,
  ): CommitStackState {
    const newStack = this.stack.map(commit => {
      const newFiles = commit.files.map((file, path) => {
        return func(commit.rev, file, path);
      });
      return commit.set('files', newFiles);
    });
    return this.set('stack', newStack);
  }

  /**
   * Describe all file stacks for testing purpose.
   * Each returned string represents a file stack.
   *
   * Output in `rev:commit/path(content)` format.
   * If `(content)` is left out it means the file at the rev is absent.
   * If `commit` is `.` then it comes from `bottomFiles` meaning that
   * the commit last modifies the path might be outside the stack.
   *
   * Rev 0 is usually the "public" version that is not editable.
   *
   * For example, `0:./x.txt 1:A/x.txt(33) 2:B/y.txt(33)` means:
   * commit A added `x.txt` with the content `33`, and commit B renamed it to
   * `y.txt`.
   *
   * `0:./z.txt(11) 1:A/z.txt(22) 2:C/z.txt` means: `z.txt` existed at
   * the bottom of the stack with the content `11`. Commit A modified
   * its content to `22` and commit C deleted `z.txt`.
   */
  describeFileStacks(showContent = true): string[] {
    const state = this.switchToFileStack();
    const fileToCommit = state.fileToCommit;
    const stack = state.stack;
    const hasAbsorb = state.hasPendingAbsorb();
    return state.fileStacks
      .map((fileStack, fileIdx) => {
        return fileStack
          .revs()
          .map(fileRev => {
            const value = fileToCommit.get(FileIdx({fileIdx, fileRev}));
            const spans = [`${fileRev}:`];
            assert(
              value != null,
              `fileToCommit should have all file stack revs (missing: fileIdx=${fileIdx} fileRev=${fileRev})`,
            );
            const {rev, path} = value;
            const [commitTitle, absent] =
              rev < 0
                ? ['.', isAbsent(state.bottomFiles.get(path))]
                : ((c: CommitState): [string, boolean] => [
                    c.text.split('\n').at(0) || [...c.originalNodes].at(0) || '?',
                    isAbsent(c.files.get(path)),
                  ])(nullthrows(stack.get(rev)));
            spans.push(`${commitTitle}/${path}`);
            if (showContent && !absent) {
              let content = fileStack.getRev(fileRev).replaceAll('\n', '↵');
              if (hasAbsorb) {
                const absorbedContent = fileStack
                  .getRev(revWithAbsorb(fileRev))
                  .replaceAll('\n', '↵');
                if (absorbedContent !== content) {
                  content += `;absorbed:${absorbedContent}`;
                }
              }
              spans.push(`(${content})`);
            }
            return spans.join('');
          })
          .join(' ');
      })
      .toArray();
  }

  /** File name for `fileStacks[index]`. If the file is renamed, return  */
  getFileStackDescription(fileIdx: number): string {
    const fileStack = nullthrows(this.fileStacks.get(fileIdx));
    const revLength = prev(fileStack.revLength);
    const nameAtFirstRev = this.getFileStackPath(fileIdx, 0 as FileRev);
    const nameAtLastRev = this.getFileStackPath(fileIdx, prev(revLength));
    const words = [];
    if (nameAtFirstRev) {
      words.push(nameAtFirstRev);
    }
    if (nameAtLastRev && nameAtLastRev !== nameAtFirstRev) {
      // U+2192. Rightwards Arrow (Unicode 1.1).
      words.push('→');
      words.push(nameAtLastRev);
    }
    if (revLength > 1) {
      words.push(t('(edited by $n commits)', {replace: {$n: revLength.toString()}}));
    }
    return words.join(' ');
  }

  /** Get the path name for a specific revision in the given file stack. */
  getFileStackPath(fileIdx: number, fileRev: FileRev): string | undefined {
    return this.fileToCommit.get(FileIdx({fileIdx, fileRev}))?.path;
  }

  /**
   * Get the commit from a file stack revision.
   * Returns undefined when rev is out of range, or the commit is "public" (ex. fileRev is 0).
   */
  getCommitFromFileStackRev(fileIdx: number, fileRev: FileRev): CommitState | undefined {
    const commitRev = this.fileToCommit.get(FileIdx({fileIdx, fileRev}))?.rev;
    if (commitRev == null || commitRev < 0) {
      return undefined;
    }
    return nullthrows(this.stack.get(commitRev));
  }

  /**
   * Test if a file rev is "absent". An absent file is different from an empty file.
   */
  isAbsentFromFileStackRev(fileIdx: number, fileRev: FileRev): boolean {
    const commitIdx = this.fileToCommit.get(FileIdx({fileIdx, fileRev}));
    if (commitIdx == null) {
      return true;
    }
    const {rev, path} = commitIdx;
    const file = rev < 0 ? this.bottomFiles.get(path) : this.getFile(rev, path);
    return file == null || isAbsent(file);
  }

  /**
   * Extract utf-8 data from a file.
   * Pending absorb is applied if considerPendingAbsorb is true.
   */
  getUtf8Data(file: FileState, considerPendingAbsorb = true): string {
    if (typeof file.data === 'string') {
      return file.data;
    }
    if (file.data instanceof FileIdx) {
      let fileRev = file.data.fileRev;
      if (considerPendingAbsorb && this.hasPendingAbsorb()) {
        fileRev = revWithAbsorb(fileRev);
      }
      return nullthrows(this.fileStacks.get(file.data.fileIdx)).getRev(fileRev);
    } else {
      throw new Error('getUtf8Data called on non-utf8 file.');
    }
  }

  /** Similar to `getUtf8Data`, but returns `null` if not utf-8 */
  getUtf8DataOptional(file: FileState, considerPendingAbsorb = true): string | null {
    return isUtf8(file) ? this.getUtf8Data(file, considerPendingAbsorb) : null;
  }

  /** Test if two files have the same data. */
  isEqualFile(a: FileState, b: FileState): boolean {
    if ((a.flags ?? '') !== (b.flags ?? '')) {
      return false;
    }
    if (isUtf8(a) && isUtf8(b)) {
      return this.getUtf8Data(a) === this.getUtf8Data(b);
    }
    // We assume base85 data is immutable, non-utf8 so they won't match utf8 data.
    if (a.data instanceof Base85 && b.data instanceof Base85) {
      return a.data.dataBase85 === b.data.dataBase85;
    }
    if (a.data instanceof DataRef && b.data instanceof DataRef) {
      return is(a.data, b.data);
    }
    return false;
  }

  /** Test if the stack is linear. */
  isStackLinear(): boolean {
    return this.stack.every(
      (commit, rev) =>
        rev === 0 || (commit.parents.size === 1 && commit.parents.first() === rev - 1),
    );
  }

  /** Find a commit by key. */
  findCommitByKey(key: string): CommitState | undefined {
    return this.stack.find(c => c.key === key);
  }

  /** Get a specified commit. */
  get(rev: CommitRev): CommitState | undefined {
    return this.stack.get(rev);
  }

  /** Get the stack size. */
  get size(): number {
    return this.stack.size;
  }

  // Histedit-related operations.

  /**
   * Calculate the dependencies of revisions.
   * For example, `{5: [3, 1]}` means rev 5 depends on rev 3 and rev 1.
   *
   * This is used to detect what's reasonable when reordering and dropping
   * commits. For example, if rev 3 depends on rev 2, then rev 3 cannot be
   * moved to be an ancestor of rev 2, and rev 2 cannot be dropped alone.
   */
  calculateDepMap = cachedMethod(this.calculateDepMapImpl, {cache: calculateDepMapCache});
  private calculateDepMapImpl(): Readonly<Map<CommitRev, Set<CommitRev>>> {
    const state = this.switchToFileStack();
    const depMap = new Map<CommitRev, Set<CommitRev>>(state.stack.map(c => [c.rev, new Set()]));

    const fileIdxRevToCommitRev = (fileIdx: FileStackIndex, fileRev: FileRev): CommitRev =>
      nullthrows(state.fileToCommit.get(FileIdx({fileIdx, fileRev}))).rev;

    // Ask FileStack for dependencies about content edits.
    state.fileStacks.forEach((fileStack, fileIdx) => {
      const fileDepMap = fileStack.calculateDepMap();
      const toCommitRev = (rev: FileRev) => fileIdxRevToCommitRev(fileIdx, rev);
      // Convert file revs to commit revs.
      fileDepMap.forEach((valueFileRevs, keyFileRev) => {
        const keyCommitRev = toCommitRev(keyFileRev);
        if (keyCommitRev >= 0) {
          const set = nullthrows(depMap.get(keyCommitRev));
          valueFileRevs.forEach(fileRev => {
            const rev = toCommitRev(fileRev);
            if (rev >= 0) {
              set.add(rev);
            }
          });
        }
      });
    });

    // Besides, file deletion / addition / renames also introduce dependencies.
    state.stack.forEach(commit => {
      const set = nullthrows(depMap.get(commit.rev));
      commit.files.forEach((file, path) => {
        const [prevRev, prevPath, prevFile] = state.parentFile(commit.rev, path, true);
        if (prevRev >= 0 && (isAbsent(prevFile) !== isAbsent(file) || prevPath !== path)) {
          set.add(prevRev);
        }
      });
    });

    return depMap;
  }

  /** Return the single parent rev, or null. */
  singleParentRev(rev: CommitRev): CommitRev | null {
    const commit = this.stack.get(rev);
    const parents = commit?.parents;
    if (parents != null) {
      const parentRev = parents?.first();
      if (parentRev != null && parents.size === 1) {
        return parentRev;
      }
    }
    return null;
  }

  /**
   * Test if the commit can be folded with its parent.
   */
  canFoldDown = cachedMethod(this.canFoldDownImpl, {cache: canFoldDownCache});
  private canFoldDownImpl(rev: CommitRev): boolean {
    if (rev <= 0) {
      return false;
    }
    const commit = this.stack.get(rev);
    if (commit == null) {
      return false;
    }
    const parentRev = this.singleParentRev(rev);
    if (parentRev == null) {
      return false;
    }
    const parent = nullthrows(this.stack.get(parentRev));
    if (commit.immutableKind !== 'none' || parent.immutableKind !== 'none') {
      return false;
    }
    // This is a bit conservative. But we're not doing complex content check for now.
    const childCount = this.stack.count(c => c.parents.includes(parentRev));
    if (childCount > 1) {
      return false;
    }
    return true;
  }

  /**
   * Drop the given `rev`.
   * The callsite should take care of `files` updates.
   */
  rewriteStackDroppingRev(rev: CommitRev): CommitStackState {
    const revMapFunc = (r: CommitRev) => (r < rev ? r : prev(r));
    const newStack = this.stack
      .filter(c => c.rev !== rev)
      .map(c => rewriteCommitRevs(c, revMapFunc));
    // Recalculate file stacks.
    return this.set('stack', newStack).buildFileStacks();
  }

  /**
   * Fold the commit with its parent.
   * This should only be called when `canFoldDown(rev)` returned `true`.
   */
  foldDown(rev: CommitRev) {
    const commit = nullthrows(this.stack.get(rev));
    const parentRev = nullthrows(this.singleParentRev(rev));
    const parent = nullthrows(this.stack.get(parentRev));
    let newParentFiles = parent.files;
    const newFiles = commit.files.map((origFile, path) => {
      // Fold copyFrom. `-` means "no change".
      //
      // | grand  | direct |      |                   |
      // | parent | parent | rev  | folded (copyFrom) |
      // +--------------------------------------------+
      // | A      | A->B   | B->C | A->C   (parent)   |
      // | A      | A->B   | B    | A->B   (parent)   |
      // | A      | A->B   | -    | A->B   (parent)   |
      // | A      | A      | A->C | A->C   (rev)      |
      // | A      | -      | A->C | A->C   (rev)      |
      // | -      | B      | B->C | C      (drop)     |
      let file = origFile;
      const optionalParentFile = newParentFiles.get(file.copyFrom ?? path);
      const copyFrom = optionalParentFile?.copyFrom ?? file.copyFrom;
      if (copyFrom != null && isAbsent(this.parentFile(parentRev, file.copyFrom ?? path)[2])) {
        // "copyFrom" is no longer valid (not existed in grand parent). Drop it.
        file = file.set('copyFrom', undefined);
      } else {
        file = file.set('copyFrom', copyFrom);
      }
      if (this.isEqualFile(this.parentFile(parentRev, path, false /* [1] */)[2], file)) {
        // The file changes cancel out. Remove it.
        // [1]: we need to disable following renames when comparing files for cancel-out check.
        newParentFiles = newParentFiles.delete(path);
      } else {
        // Fold the change of this file.
        newParentFiles = newParentFiles.set(path, file);
      }
      return file;
    });

    // Fold other properties to parent.
    let newParentText = parent.text;
    if (isMeaningfulText(commit.text)) {
      const schema = readAtom(commitMessageFieldsSchema);
      const parentTitle = firstLine(parent.text);
      const parentFields = parseCommitMessageFields(
        schema,
        parentTitle,
        parent.text.slice(parentTitle.length),
      );
      const commitTitle = firstLine(commit.text);
      const commitFields = parseCommitMessageFields(
        schema,
        commitTitle,
        commit.text.slice(commitTitle.length),
      );
      const merged = mergeCommitMessageFields(schema, parentFields, commitFields);
      newParentText = commitMessageFieldsToString(schema, merged);
    }

    const newParent = parent.merge({
      text: newParentText,
      date: commit.date,
      originalNodes: parent.originalNodes.merge(commit.originalNodes),
      files: newParentFiles,
    });
    const newCommit = commit.set('files', newFiles);
    const newStack = this.stack.withMutations(mutStack => {
      mutStack.set(parentRev, newParent).set(rev, newCommit);
    });

    return this.set('stack', newStack).rewriteStackDroppingRev(rev);
  }

  /**
   * Test if the commit can be dropped. That is, none of its descendants depend on it.
   */
  canDrop = cachedMethod(this.canDropImpl, {cache: canDropCache});
  private canDropImpl(rev: CommitRev): boolean {
    if (rev < 0 || this.stack.get(rev)?.immutableKind !== 'none') {
      return false;
    }
    const depMap = this.calculateDepMap();
    for (const [currentRev, dependentRevs] of depMap.entries()) {
      if (dependentRevs.has(rev) && generatorContains(this.log(currentRev), rev)) {
        return false;
      }
    }
    return true;
  }

  /**
   * Drop a commit. Changes made by the commit will be removed in its
   * descendants.
   *
   * This should only be called when `canDrop(rev)` returned `true`.
   */
  drop(rev: CommitRev): CommitStackState {
    let state = this.switchToFileStack().inner;
    const commit = nullthrows(state.stack.get(rev));
    commit.files.forEach((file, path) => {
      const fileIdxRev: FileIdx | undefined = state.commitToFile.get(CommitIdx({rev, path}));
      if (fileIdxRev != null) {
        const {fileIdx, fileRev} = fileIdxRev;
        const fileStack = nullthrows(state.fileStacks.get(fileIdx));
        // Drop the rev by remapping it to an unused rev.
        const unusedFileRev = fileStack.source.revLength;
        const newFileStack = fileStack.remapRevs(new Map([[fileRev, unusedFileRev]]));
        state = state.setIn(['fileStacks', fileIdx], newFileStack);
      }
    });

    return new CommitStackState(undefined, state).rewriteStackDroppingRev(rev);
  }

  /**
   * Insert an empty commit at `rev`.
   * Cannot insert to an empty stack.
   */
  insertEmpty(rev: CommitRev, message: string, splitFromRev?: CommitRev): CommitStackState {
    assert(rev <= this.stack.size && rev >= 0, 'rev out of range');
    const state = this.switchToFileContent();
    let newStack;
    const newKey = this.nextKey('insert');
    const originalNodes = splitFromRev == null ? undefined : state.get(splitFromRev)?.originalNodes;
    if (rev === this.stack.size) {
      const top = this.stack.last();
      assert(top != null, 'stack cannot be empty');
      newStack = this.stack.push(
        CommitState({
          rev,
          parents: List(rev === 0 ? [] : [prev(rev)]),
          text: message,
          key: newKey,
          author: top.author,
          date: top.date,
          originalNodes,
        }),
      );
    } else {
      const revMapFunc = (r: CommitRev) => (r >= rev ? next(r) : r);
      const origParents = nullthrows(state.stack.get(rev)).parents;
      newStack = state.stack
        .map(c => rewriteCommitRevs(c, revMapFunc))
        .flatMap(c => {
          if (c.rev == rev + 1) {
            return Seq([
              CommitState({
                rev,
                parents: origParents,
                text: message,
                key: newKey,
                author: c.author,
                date: c.date,
                originalNodes,
              }),
              c.set('parents', List([rev])),
            ]);
          } else {
            return Seq([c]);
          }
        });
    }
    return this.set('stack', newStack).buildFileStacks();
  }

  /**
   * Update commit message.
   */
  editCommitMessage(rev: CommitRev, message: string): CommitStackState {
    assert(rev <= this.stack.size && rev >= 0, 'rev out of range');
    const newStack = this.stack.setIn([rev, 'text'], message);
    return this.set('stack', newStack);
  }

  /**
   * Find a unique "key" not yet used by the commit stack.
   */
  nextKey(prefix: string): string {
    const usedKeys = ImSet(this.stack.map(c => c.key));
    for (let i = 0; ; i++) {
      const key = `${prefix}-${i}`;
      if (usedKeys.has(key)) {
        continue;
      }
      return key;
    }
  }

  /**
   * Check if reorder is conflict-free.
   *
   * `order` defines the new order as a "from rev" list.
   * For example, when `this.revs()` is `[0, 1, 2, 3]` and `order` is
   * `[0, 2, 3, 1]`, it means moving the second (rev 1) commit to the
   * stack top.
   *
   * Reordering in a non-linear stack is not supported and will return
   * `false`. This is because it's tricky to describe the desired
   * new parent relationships with just `order`.
   *
   * If `order` is `this.revs()` then no reorder is done.
   */
  canReorder(order: CommitRev[]): boolean {
    const state = this.switchToFileStack();
    if (!state.isStackLinear()) {
      return false;
    }
    if (
      !deepEqual(
        [...order].sort((a, b) => a - b),
        state.revs(),
      )
    ) {
      return false;
    }

    // "hash" immutable commits cannot be moved.
    if (state.stack.some((commit, rev) => commit.immutableKind === 'hash' && order[rev] !== rev)) {
      return false;
    }

    const map = new Map<CommitRev, CommitRev>(
      order.map((fromRev, toRev) => [fromRev as CommitRev, toRev as CommitRev]),
    );
    // Check dependencies.
    const depMap = state.calculateDepMap();
    for (const [rev, depRevs] of depMap) {
      const newRev = map.get(rev);
      if (newRev == null) {
        return false;
      }
      for (const depRev of depRevs) {
        const newDepRev = map.get(depRev);
        if (newDepRev == null) {
          return false;
        }
        if (!generatorContains(state.log(newRev), newDepRev)) {
          return false;
        }
      }
    }
    // Passed checks.
    return true;
  }

  canMoveDown = cachedMethod(this.canMoveDownImpl, {cache: canMoveDownCache});
  private canMoveDownImpl(rev: CommitRev): boolean {
    return rev > 0 && this.canMoveUp(prev(rev));
  }

  canMoveUp = cachedMethod(this.canMoveUpImpl, {cache: canMoveUpCache});
  private canMoveUpImpl(rev: CommitRev): boolean {
    return this.canReorder(reorderedRevs(this, rev));
  }

  /**
   * Reorder stack. Similar to running `histedit`, followed by reordering
   * commits.
   *
   * See `canReorder` for the meaning of `order`.
   * This should only be called when `canReorder(order)` returned `true`.
   */
  reorder(order: CommitRev[]): CommitStackState {
    const commitRevMap = new Map<CommitRev, CommitRev>(
      order.map((fromRev, toRev) => [fromRev, toRev as CommitRev]),
    );

    // Reorder file contents. This is somewhat tricky involving multiple
    // mappings. Here is an example:
    //
    //   Stack: A-B-C-D. Original file contents: [11, 112, 0112, 01312].
    //   Reorder to: A-D-B-C. Expected result: [11, 131, 1312, 01312].
    //
    // First, we figure out the file stack, and reorder it. The file stack
    // now has the content [11 (A), 131 (B), 1312 (C), 01312 (D)], but the
    // commit stack is still in the A-B-C-D order and refers to the file stack
    // using **fileRev**s. If we blindly reorder the commit stack to A-D-B-C,
    // the resulting files would be [11 (A), 01312 (D), 131 (B), 1312 (C)].
    //
    // To make it work properly, we apply a reverse mapping (A-D-B-C =>
    // A-B-C-D) to the file stack before reordering commits, changing
    // [11 (A), 131 (D), 1312 (B), 01312 (C)] to [11 (A), 1312 (B), 01312 (C),
    // 131 (D)]. So after the commit remapping it produces the desired
    // output.
    let state = this.switchToFileStack();
    const newFileStacks = state.fileStacks.map((origFileStack, fileIdx) => {
      let fileStack: FileStackState = origFileStack;

      // file revs => commit revs => mapped commit revs => mapped file revs
      const fileRevs = fileStack.revs();
      const commitRevPaths: CommitIdx[] = fileRevs.map(fileRev =>
        nullthrows(state.fileToCommit.get(FileIdx({fileIdx, fileRev}))),
      );
      const commitRevs: CommitRev[] = commitRevPaths.map(({rev}) => rev);
      const mappedCommitRevs: CommitRev[] = commitRevs.map(rev => commitRevMap.get(rev) ?? rev);
      // commitRevs and mappedCommitRevs might not overlap, although they
      // have the same length (fileRevs.length). Turn them into compact
      // sequence to reason about.
      const fromRevs: FileRev[] = compactSequence(commitRevs);
      const toRevs: FileRev[] = compactSequence(mappedCommitRevs);
      if (deepEqual(fromRevs, toRevs)) {
        return fileStack;
      }
      // Mapping: zip(original revs, mapped file revs)
      const fileRevMap = new Map<FileRev, FileRev>(zip(fromRevs, toRevs));
      fileStack = fileStack.remapRevs(fileRevMap);
      // Apply the reverse mapping. See the above comment for why this is necessary.
      return new FileStackState(fileRevs.map(fileRev => fileStack.getRev(toRevs[fileRev])));
    });
    state = state.set('fileStacks', newFileStacks);

    // Update state.stack.
    const newStack = state.stack.map((_commit, rev) => {
      const commit = nullthrows(state.stack.get(order[rev]));
      return commit.merge({
        parents: List(rev > 0 ? [prev(rev as CommitRev)] : []),
        rev: rev as CommitRev,
      });
    });
    state = state.set('stack', newStack);

    return state.buildFileStacks();
  }

  /** Replace a file stack. Throws if the new stack has a different length. */
  setFileStack(fileIdx: number, stack: FileStackState): CommitStackState {
    return this.setFileStackInternal(fileIdx, stack, (oldStack, newStack) => {
      assert(oldStack.revLength === newStack.revLength, 'fileStack length mismatch');
    });
  }

  /** Internal use: replace a file stack. */
  private setFileStackInternal(
    fileIdx: number,
    stack: FileStackState,
    check?: (oldStack: FileStackState, newStack: FileStackState) => void,
  ): CommitStackState {
    const oldStack = this.fileStacks.get(fileIdx);
    assert(oldStack != null, 'fileIdx out of range');
    check?.(oldStack, stack);
    const newInner = this.inner.setIn(['fileStacks', fileIdx], stack);
    return new CommitStackState(undefined, newInner);
  }

  /**
   * Extract part of the commit stack as a new linear stack.
   *
   * The new stack is "dense" in a way that each commit's "files"
   * include all files every referred by the stack, even if the
   * file is not modified.
   *
   * The new stack:
   * - Does not have "originalStack".
   * - "Dense". Therefore file revs (in fileStacks) map to all
   *   commits.
   * - Preserves the rename information, but does not follow renames
   *   when building the file stacks.
   * - Preserves non-utf8 files, but does not build into the file
   *   stacks, which means their content cannot be edited, but might
   *   still be moved around.
   *
   * It is for the interactive split use-case.
   */
  denseSubStack(revs: List<CommitRev>): CommitStackState {
    const commits = revs.map(rev => this.stack.get(rev)).filter(Boolean) as List<CommitState>;
    const bottomFiles = new Map<RepoPath, FileState>();
    const followRenames = false;

    // Use this.parentFile to populate bottomFiles.
    commits.forEach(commit => {
      const startRev = commit.rev;
      commit.files.forEach((file, startPath) => {
        ([startPath].filter(Boolean) as [string]).forEach(path => {
          if (!bottomFiles.has(path)) {
            const [, , file] = this.parentFile(startRev, path, false);
            bottomFiles.set(path, file);
          }
          if (file.copyFrom != null) {
            const [, fromPath, fromFile] = this.parentFile(startRev, path, true);
            bottomFiles.set(fromPath, fromFile);
          }
        });
      });
    });

    // Modify stack:
    // - Re-assign "rev"s (including "parents").
    // - Assign file contents so files are considered changed in every commit.
    const currentFiles = new Map(bottomFiles);
    const stack: List<CommitState> = commits.map((commit, i) => {
      const newFiles = commit.files.withMutations(mut => {
        let files = mut;
        // Add unchanged files to force treating files as "modified".
        currentFiles.forEach((file, path) => {
          const inCommitFile = files.get(path);
          if (inCommitFile == undefined) {
            // Update files so all files are considered changed and got a file rev assigned.
            files = files.set(path, file ?? ABSENT_FILE);
          } else {
            // Update currentFiles so it can be used by the next commit.
            // Avoid repeating "copyFrom".
            currentFiles.set(path, inCommitFile.remove('copyFrom'));
          }
        });
        return files;
      });
      const parents = i === 0 ? List<CommitRev>() : List([prev(i as CommitRev)]);
      return commit.merge({rev: i as CommitRev, files: newFiles, parents});
    });

    const record = CommitStackRecord({
      stack,
      bottomFiles,
    });
    const newStack = new CommitStackState(undefined, record);
    return newStack.buildFileStacks({followRenames}).switchToFileStack();
  }

  /**
   * Replace the `startRev` (inclusive) to `endRev` (exclusive) sub stack
   * with commits from the `subStack`.
   *
   * Unmodified changes will be dropped. Top commits with empty changes are
   * dropped. This turns a "dense" back to a non-"dense" one.
   *
   * Intended for interactive split use-case.
   */
  applySubStack(
    startRev: CommitRev,
    endRev: CommitRev,
    subStack: CommitStackState,
  ): CommitStackState {
    assert(
      startRev >= 0 && endRev <= this.stack.size && startRev < endRev,
      'startRev or endRev out of range',
    );

    const contentSubStack = subStack.switchToFileContent();
    const state = this.switchToFileContent();

    // Used to detect "unchanged" files in subStack.
    const afterFileMap = new Map(
      [...state.bottomFiles.entries()].map(([path, file]) => [path, file]),
    );

    // Used to check the original "final" content of files.
    const beforeFileMap = new Map(afterFileMap);

    const updateFileMap = (commit: CommitState, map: Map<string, FileState>) =>
      commit.files.forEach((file, path) => map.set(path, file));

    // Pick an unused key.
    const usedKeys = new Set(
      state.stack
        .filter(c => c.rev < startRev || c.rev >= endRev)
        .map(c => c.key)
        .toArray(),
    );
    const pickKey = (c: CommitState): CommitState => {
      if (usedKeys.has(c.key)) {
        for (let i = 0; ; ++i) {
          const key = `${c.key}-${i}`;
          if (!usedKeys.has(key)) {
            usedKeys.add(c.key);
            return c.set('key', key);
          }
        }
      } else {
        usedKeys.add(c.key);
        return c;
      }
    };

    // Process commits in a "dense" stack.
    // - Update afterFileMap.
    // - Drop unchanged files.
    // - Drop the "absent" flag from files if they are not empty.
    // - Pick a unique key.
    // - Add "parent" for the first commit.
    // - Adjust "revs".
    const processDenseCommit = (c: CommitState): CommitState => {
      const newFiles = c.files.flatMap<RepoPath, FileState>((currentFile, path) => {
        let file: FileState = currentFile;
        const oldFile = afterFileMap.get(path);
        // Drop "absent" flag (and reuse the old flag).
        if (
          file.flags?.includes(ABSENT_FLAG) &&
          typeof file.data === 'string' &&
          file.data.length > 0
        ) {
          let oldFlag = oldFile?.flags;
          if (oldFlag === ABSENT_FLAG) {
            oldFlag = undefined;
          }
          if (oldFlag == null) {
            file = file.remove('flags');
          } else {
            file = file.set('flags', oldFlag);
          }
        }
        // Drop unchanged files.
        const keep = oldFile == null || !isContentSame(oldFile, file);
        // Update afterFileMap.
        if (keep) {
          afterFileMap.set(path, file);
        }
        return Seq(keep ? [[path, file]] : []);
      });
      const isFirst = c.rev === 0;
      let commit = rewriteCommitRevs(pickKey(c), r => (r + startRev) as CommitRev).set(
        'files',
        newFiles,
      );
      if (isFirst && startRev > 0) {
        commit = commit.set('parents', List([prev(startRev)]));
      }
      return commit;
    };

    //             |<--- to delete --->|
    // Before: ... |startRev ... endRev| ...
    // New:    ... |filter(substack)   | ...
    //             filter: remove empty commits
    let newSubStackSize = 0;
    const newStack = state.stack.flatMap(c => {
      updateFileMap(c, beforeFileMap);
      if (c.rev < startRev) {
        updateFileMap(c, afterFileMap);
        return Seq([c]);
      } else if (c.rev === startRev) {
        // dropUnchangedFiles updates afterFileMap.
        let commits = contentSubStack.stack.map(c => processDenseCommit(c));
        // Drop empty commits at the end. Adjust offset.
        while (commits.last()?.files?.isEmpty()) {
          commits = commits.pop();
        }
        newSubStackSize = commits.size;
        return commits;
      } else if (c.rev > startRev && c.rev < endRev) {
        return Seq([]);
      } else {
        let commit = c;
        assert(c.rev >= endRev, 'bug: c.rev < endRev should be handled above');
        if (c.rev === endRev) {
          // This commit should have the same exact content as before, not just the
          // modified files, but also the unmodified ones.
          // We check all files ever changed by the stack between "before" and "after",
          // and bring their content back to "before" in this commit.
          beforeFileMap.forEach((beforeFile, path) => {
            if (commit.files.has(path)) {
              return;
            }
            const afterFile = afterFileMap.get(path);
            if (afterFile == null || !isContentSame(beforeFile, afterFile)) {
              commit = commit.setIn(['files', path], beforeFile);
            }
          });
          // Delete file added by the subStack that do not exist before.
          afterFileMap.forEach((_, path) => {
            if (!beforeFileMap.has(path)) {
              commit = commit.setIn(['files', path], ABSENT_FILE);
            }
          });
        }
        const offset = newSubStackSize - (endRev - startRev);
        return Seq([
          rewriteCommitRevs(
            commit,
            r => ((r >= startRev && r < endRev ? endRev - 1 : r) + offset) as CommitRev,
          ),
        ]);
      }
    });

    // This function might be frequnetly called during interacitve split.
    // Do not build file stacks (potentially slow) now.
    return state.set('stack', newStack);
  }

  /** Test if a path at the given rev is a renamed (not copy). */
  isRename(rev: CommitRev, path: RepoPath): boolean {
    const commit = this.get(rev);
    if (commit == null) {
      return false;
    }
    return isRename(commit, path);
  }

  /**
   * If the given file has a metadata change, return the old and new metadata.
   * Otherwise, return undefined.
   */
  changedFileMetadata(
    rev: CommitRev,
    path: RepoPath,
    followRenames = false,
  ): [FileMetadata, FileMetadata] | undefined {
    const file = this.getFile(rev, path);
    const parentFile = this.parentFile(rev, path, followRenames)[2];
    const fileMeta = toMetadata(file);
    // Only report "changed" if copyFrom is newly set.
    const parentMeta = toMetadata(parentFile).remove('copyFrom');
    return fileMeta.equals(parentMeta) ? undefined : [parentMeta, fileMeta];
  }
}

const canDropCache = new LRU(1000);
const calculateDepMapCache = new LRU(1000);
const canFoldDownCache = new LRU(1000);
const canMoveUpCache = new LRU(1000);
const canMoveDownCache = new LRU(1000);

function getBottomFilesFromExportStack(stack: Readonly<ExportStack>): Map<RepoPath, FileState> {
  // bottomFiles requires that the stack only has one root.
  checkStackSingleRoot(stack);

  // Calculate bottomFiles.
  const bottomFiles: Map<RepoPath, FileState> = new Map();
  stack.forEach(commit => {
    for (const [path, file] of Object.entries(commit.relevantFiles ?? {})) {
      if (!bottomFiles.has(path)) {
        bottomFiles.set(path, convertExportFileToFileState(file));
      }
    }

    // Files not yet existed in `bottomFiles` means they are added (in root commits)
    // mark them as "missing" in the stack bottom.
    for (const path of Object.keys(commit.files ?? {})) {
      if (!bottomFiles.has(path)) {
        bottomFiles.set(path, ABSENT_FILE);
      }
    }
  });

  return bottomFiles;
}

function convertExportFileToFileState(file: ExportFile | null): FileState {
  if (file == null) {
    return ABSENT_FILE;
  }
  return FileState({
    data:
      file.data != null
        ? file.data
        : file.dataBase85
          ? Base85({dataBase85: file.dataBase85})
          : DataRef(nullthrows(file.dataRef)),
    copyFrom: file.copyFrom,
    flags: file.flags,
  });
}

function getCommitStatesFromExportStack(stack: Readonly<ExportStack>): List<CommitState> {
  checkStackParents(stack);

  // Prepare nodeToRev conversion.
  const revs: CommitRev[] = [...stack.keys()] as CommitRev[];
  const nodeToRevMap: Map<Hash, CommitRev> = new Map(revs.map(rev => [stack[rev].node, rev]));
  const nodeToRev = (node: Hash): CommitRev => {
    const rev = nodeToRevMap.get(node);
    if (rev == null) {
      throw new Error(
        `Rev ${rev} should be known ${JSON.stringify(nodeToRevMap)} (bug in debugexportstack?)`,
      );
    }
    return rev;
  };

  // Calculate requested stack.
  const commitStates = stack.map(commit =>
    CommitState({
      originalNodes: ImSet([commit.node]),
      rev: nodeToRev(commit.node),
      key: commit.node,
      author: commit.author,
      date: DateTuple({unix: commit.date[0], tz: commit.date[1]}),
      text: commit.text,
      // Treat commits that are not requested explicitly as immutable too.
      immutableKind: commit.immutable || !commit.requested ? 'hash' : 'none',
      parents: List((commit.parents ?? []).map(p => nodeToRev(p))),
      files: ImMap<RepoPath, FileState>(
        Object.entries(commit.files ?? {}).map(([path, file]) => [
          path,
          convertExportFileToFileState(file),
        ]),
      ),
    }),
  );

  return List(commitStates);
}

/** Check that there is only one root in the stack. */
function checkStackSingleRoot(stack: Readonly<ExportStack>) {
  const rootNodes = stack.filter(commit => (commit.parents ?? []).length === 0);
  if (rootNodes.length > 1) {
    throw new Error(
      `Multiple roots ${JSON.stringify(rootNodes.map(c => c.node))} is not supported`,
    );
  }
}

/**
 * Check the exported stack and throws if it breaks assumptions.
 * - No duplicated commits.
 * - "parents" refer to other commits in the stack.
 */
function checkStackParents(stack: Readonly<ExportStack>) {
  const knownNodes = new Set();
  stack.forEach(commit => {
    const parents = commit.parents ?? [];
    if (parents.length > 0) {
      if (!commit.requested) {
        throw new Error(
          `Requested commit ${commit.node} should not have parents ${JSON.stringify(
            parents,
          )} (bug in debugexportstack?)`,
        );
      }
      parents.forEach(parentNode => {
        if (!knownNodes.has(parentNode)) {
          throw new Error(`Parent commit ${parentNode} is not exported (bug in debugexportstack?)`);
        }
      });
    }
    if (parents.length > 1) {
      throw new Error(`Merge commit ${commit.node} is not supported`);
    }
    knownNodes.add(commit.node);
  });
  if (knownNodes.size != stack.length) {
    throw new Error('Commit stack has duplicated nodes (bug in debugexportstack?)');
  }
}

/** Rewrite fields that contains `rev` based on the mapping function. */
function rewriteCommitRevs(
  commit: CommitState,
  revMapFunc: (rev: CommitRev) => CommitRev,
): CommitState {
  return commit.merge({
    rev: revMapFunc(commit.rev),
    parents: commit.parents.map(revMapFunc),
  });
}

/** Guess if commit message is meaningful. Messages like "wip" or "fixup" are meaningless. */
function isMeaningfulText(text: string): boolean {
  const trimmed = text.trim();
  return trimmed.includes(' ') || trimmed.includes('\n') || trimmed.length > 20;
}

/**
 * Turn distinct numbers to a 0..n sequence preserving the order.
 * For example, turn [0, 100, 50] into [0, 2, 1].
 * This could convert CommitRevs to FileRevs, assuming the file
 * stack is a sub-sequence of the commit sequence.
 */
function compactSequence(revs: CommitRev[]): FileRev[] {
  const sortedRevs = [...revs].sort((aRev, bRev) => aRev - bRev);
  return revs.map(rev => sortedRevs.indexOf(rev) as FileRev);
}

/** Reorder rev and rev + 1. Return [] if rev is out of range */
export function reorderedRevs(state: CommitStackState, rev: number): CommitRev[] {
  // Basically, `toSpliced`, but it's not available everywhere.
  const order = state.revs();
  if (rev < 0 || rev >= order.length - 1) {
    return [];
  }
  const rev1 = order[rev];
  const rev2 = order[rev + 1];
  order.splice(rev, 2, rev2, rev1);
  return order;
}

type BuildFileStackOptions = {followRenames?: boolean};

// Re-export for compatibility
export {
  ABSENT_FILE,
  ABSENT_FLAG,
  Base85,
  CommitIdx,
  CommitState,
  DataRef,
  DateTuple,
  FileIdx,
  FileState,
  isAbsent,
  isContentSame,
  isRename,
  isUtf8,
  toMetadata,
};
export type {CommitRev, FileMetadata, FileRev, FileStackIndex};
