/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import type {FileRev, FileStackState} from '../fileStackState';
import type {Mode} from './FileStackEditorLines';

import {Checkbox} from 'isl-components/Checkbox';
import {Dropdown} from 'isl-components/Dropdown';
import {RadioGroup} from 'isl-components/Radio';
import {atom, useAtom} from 'jotai';
import {useState} from 'react';
import {nullthrows} from 'shared/utils';
import {Row} from '../../ComponentUtils';
import {EmptyState} from '../../EmptyState';
import {t, T} from '../../i18n';
import {FileStackEditorRow} from './FileStackEditor';
import {bumpStackEditMetric, useStackEditState} from './stackEditState';

const editModeAtom = atom<Mode>('unified-diff');

export default function FileStackEditPanel() {
  const [fileIdx, setFileIdx] = useState<null | number>(null);
  const [mode, setMode] = useAtom(editModeAtom);
  const [textEdit, setTextEdit] = useState(false);
  const stackEdit = useStackEditState();

  // File list dropdown.
  const commitStack = stackEdit.commitStack.switchToFileStack();
  const pathFileIdxList: Array<[string, number]> = commitStack.fileStacks
    .map((_f, i): [string, number] => {
      const label = commitStack.getFileStackDescription(i);
      return [label, i];
    })
    .toArray()
    .sort();
  const fileSelector = (
    <div
      className="dropdown-container"
      style={{
        marginBottom: 'var(--pad)',
        width: '100%',
        minWidth: '500px',
        zIndex: 3,
      }}>
      <label htmlFor="stack-file-dropdown">File to edit</label>
      <Dropdown
        id="stack-file-dropdown"
        value={fileIdx == null ? 'none' : String(fileIdx)}
        style={{width: '100%', zIndex: 3}}
        onChange={e => {
          const idx = e.currentTarget.value;
          setFileIdx(idx === 'none' ? null : parseInt(idx));
        }}
        options={
          [
            {value: 'none', name: t('Select a file to edit')},
            ...pathFileIdxList.map(([path, idx]) => ({value: String(idx), name: path})),
          ] as Array<{value: string; name: string}>
        }
      />
    </div>
  );

  if (fileIdx == null) {
    return (
      <div>
        {fileSelector}
        <EmptyState small>
          <T>Select a file to see all changes in a row.</T>
        </EmptyState>
      </div>
    );
  }

  // Properties for file stack editing.
  const stack = nullthrows(commitStack.fileStacks.get(fileIdx));
  const getTitle = (rev: FileRev) =>
    commitStack.getCommitFromFileStackRev(fileIdx, rev)?.text ??
    t(
      '(Base version)\n\n' +
        'Not part of the stack being edited. ' +
        'Cannot be edited here.\n\n' +
        'Provided to show diff against changes in the stack.',
    );
  const skip = (rev: FileRev) => commitStack.isAbsentFromFileStackRev(fileIdx, rev);
  const setStack = (newStack: FileStackState) => {
    const fileDesc = commitStack.getFileStackDescription(fileIdx);
    const newCommitStack = commitStack.setFileStack(fileIdx, newStack);
    stackEdit.push(newCommitStack, {name: 'fileStack', fileDesc});
    bumpStackEditMetric('fileStackEdit');
  };

  const editorRow = (
    <FileStackEditorRow
      stack={stack}
      setStack={setStack}
      getTitle={getTitle}
      skip={skip}
      mode={mode}
      textEdit={textEdit || mode === 'side-by-side-diff'}
    />
  );

  return (
    <div>
      {fileSelector}
      <div
        style={{
          marginLeft: 'calc(0px - var(--pad))',
          marginRight: 'calc(0px - var(--pad))',
          minWidth: 'calc((100vw / var(--zoom)) - 81px)',
          minHeight: 'calc((100vh / var(--zoom)) - 265px)',
        }}>
        {editorRow}
      </div>
      <Row>
        <RadioGroup
          choices={[
            {value: 'unified-diff', title: t('Unified diff')},
            {value: 'side-by-side-diff', title: t('Side by side diff')},
            {value: 'unified-stack', title: t('Unified stack (advanced)')},
          ]}
          current={mode}
          onChange={setMode}
        />
        <Checkbox
          accessKey="t"
          checked={textEdit || mode === 'side-by-side-diff'}
          disabled={mode === 'side-by-side-diff'}
          onChange={() => {
            setTextEdit(c => !c);
          }}>
          <T>Edit text</T>
        </Checkbox>
      </Row>
    </div>
  );
}
