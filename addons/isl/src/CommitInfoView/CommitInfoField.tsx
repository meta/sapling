/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import type {JSX, ReactNode} from 'react';
import type {FieldConfig} from './types';

import {Icon} from 'isl-components/Icon';
import {extractTokens, TokensList} from 'isl-components/Tokens';
import {DOCUMENTATION_DELAY, Tooltip} from 'isl-components/Tooltip';
import {useAtomValue} from 'jotai';
import {Fragment, Suspense} from 'react';
import {InternalFieldName} from 'shared/constants';
import {tracker} from '../analytics';
import {Copyable} from '../Copyable';
import {T} from '../i18n';
import {Internal} from '../Internal';
import {copyFromParentCommit, parentCommitContextAtom} from './CommitInfoState';
import {isFieldNonEmpty} from './CommitMessageFields';
import {RenderMarkup} from './RenderMarkup';
import {SeeMoreContainer} from './SeeMoreContainer';
import {CommitInfoTextArea} from './TextArea';
import {CommitInfoTextField} from './TextField';
import {convertFieldNameToKey, getOnClickToken, Section, SmallCapsTitle} from './utils';

export function CommitInfoField({
  field,
  isBeingEdited,
  readonly,
  content,
  editedField,
  startEditingField,
  setEditedField,
  extra,
  autofocus,
}: {
  field: FieldConfig;
  isBeingEdited: boolean;
  readonly: boolean;
  startEditingField: () => void;
  content?: string | Array<string>;
  editedField: string | Array<string> | undefined;
  setEditedField: (value: string) => unknown;
  extra?: JSX.Element;
  autofocus?: boolean;
}): JSX.Element | null {
  const parentFields = useAtomValue(parentCommitContextAtom)?.parentFields;
  const showCopyFromParent =
    !readonly && parentFields != null && isFieldNonEmpty(parentFields[field.key]);
  const editedFieldContent =
    editedField == null ? '' : Array.isArray(editedField) ? editedField.join(', ') : editedField;
  if (field.type === 'title') {
    return (
      <>
        {isBeingEdited ? (
          <Section className="commit-info-title-field-section">
            <SmallCapsTitle>
              <Icon icon="milestone" />
              <T>{field.key}</T>
            </SmallCapsTitle>
            <CommitInfoTextArea
              kind={field.type}
              name={field.key}
              autoFocus={autofocus ?? false}
              editedMessage={editedFieldContent}
              setEditedField={setEditedField}
            />
          </Section>
        ) : (
          <div className="commit-info-title-wrapper">
            <ClickToEditField
              startEditingField={readonly ? undefined : startEditingField}
              kind={field.type}
              fieldKey={field.key}>
              <span>{content}</span>
            </ClickToEditField>
            <div className="commit-info-field-buttons">
              {readonly ? null : <EditFieldButton onClick={startEditingField} />}
              {showCopyFromParent ? (
                <CopyFromParentButton onClick={() => copyFromParentCommit(field.key)} />
              ) : null}
            </div>
          </div>
        )}
        {extra}
      </>
    );
  } else {
    const Wrapper =
      field.type === 'textarea' || field.type === 'custom' ? SeeMoreContainer : Fragment;
    if (field.type === 'read-only' && !content) {
      // don't render empty read-only fields, since you can't "click to edit"
      return null;
    }

    if (isBeingEdited) {
      if (field.type === 'custom') {
        const CustomEditorComponent = field.renderEditor;
        return (
          <Section className="commit-info-field-section">
            <SmallCapsTitle>
              <Icon icon={field.icon} />
              {field.key}
            </SmallCapsTitle>
            <Suspense fallback={null}>
              <CustomEditorComponent
                field={field}
                content={editedFieldContent}
                setEditedField={setEditedField}
                autoFocus={autofocus ?? false}
              />
            </Suspense>
            {extra}
          </Section>
        );
      } else if (field.type !== 'read-only') {
        return (
          <Section className="commit-info-field-section">
            <SmallCapsTitle>
              <Icon icon={field.icon} />
              {field.key}
            </SmallCapsTitle>
            {field.type === 'field' ? (
              <CommitInfoTextField
                field={field}
                autoFocus={autofocus ?? false}
                editedMessage={editedFieldContent}
                setEditedCommitMessage={setEditedField}
              />
            ) : (
              <CommitInfoTextArea
                kind={field.type}
                name={field.key}
                autoFocus={autofocus ?? false}
                editedMessage={editedFieldContent}
                setEditedField={setEditedField}
              />
            )}
            {extra}
          </Section>
        );
      }
    }

    let renderedContent;
    if (content) {
      if (field.type === 'custom') {
        const CustomDisplayComponent = field.renderDisplay;
        const fieldContent =
          content == null ? '' : Array.isArray(content) ? content.join(', ') : content;
        renderedContent = (
          <Suspense fallback={null}>
            <CustomDisplayComponent content={fieldContent} />
          </Suspense>
        );
      } else if (field.type === 'field') {
        const tokens = Array.isArray(content) ? content : extractTokens(content)[0];
        renderedContent = (
          <div className="commit-info-tokenized-field">
            <TokensList tokens={tokens} onClickToken={getOnClickToken(field)} />
            {field.maxTokens === 1 && tokens.length > 0 && (
              <Copyable iconOnly>{tokens[0]}</Copyable>
            )}
          </div>
        );
      } else {
        if (Array.isArray(content) || !field.isRenderableMarkup) {
          renderedContent = content;
        } else {
          renderedContent = <RenderMarkup>{content}</RenderMarkup>;
        }
      }
    } else {
      renderedContent = (
        <span className="empty-description subtle">
          {readonly ? (
            <>
              <T replace={{$name: field.key}}> No $name</T>
            </>
          ) : (
            <>
              <Icon icon="add" />
              <T replace={{$name: field.key}}> Click to add $name</T>
            </>
          )}
        </span>
      );
    }

    return (
      <Section>
        <Wrapper>
          <SmallCapsTitle>
            <Icon icon={field.icon} />
            <T>{field.key}</T>
            <div className="commit-info-field-buttons">
              {readonly ? null : <EditFieldButton onClick={startEditingField} />}
              {showCopyFromParent ? (
                <CopyFromParentButton onClick={() => copyFromParentCommit(field.key)} />
              ) : null}
              {!readonly &&
              field.key === InternalFieldName.TestPlan &&
              Internal.RecommendTestPlanButton ? (
                <Internal.RecommendTestPlanButton className="hover-edit-button" />
              ) : null}
              {!readonly &&
              field.key === InternalFieldName.Summary &&
              Internal.GenerateSummaryButton ? (
                <Internal.GenerateSummaryButton className="hover-edit-button" />
              ) : null}
            </div>
          </SmallCapsTitle>
          <ClickToEditField
            startEditingField={readonly ? undefined : startEditingField}
            kind={field.type}
            fieldKey={field.key}>
            {renderedContent}
          </ClickToEditField>
          {extra}
        </Wrapper>
      </Section>
    );
  }
}

function ClickToEditField({
  children,
  startEditingField,
  fieldKey,
  kind,
}: {
  children: ReactNode;
  /** function to run when you click to edit. If null, the entire field will be non-editable. */
  startEditingField?: () => void;
  fieldKey: string;
  kind: 'title' | 'field' | 'textarea' | 'custom' | 'read-only';
}) {
  const editable = startEditingField != null && kind !== 'read-only';
  const renderKey = convertFieldNameToKey(fieldKey);
  return (
    <div
      className={`commit-info-rendered-${kind}${editable ? '' : ' non-editable'}`}
      data-testid={`commit-info-rendered-${renderKey}`}
      onClick={() => {
        if (startEditingField != null && kind !== 'read-only') {
          startEditingField();

          tracker.track('CommitInfoFieldEditFieldClick', {
            extras: {
              fieldKey,
              kind,
            },
          });
        }
      }}
      onKeyPress={
        startEditingField != null && kind !== 'read-only'
          ? e => {
              if (e.key === 'Enter' || e.key === ' ') {
                startEditingField();
                e.preventDefault();
              }
            }
          : undefined
      }
      tabIndex={0}>
      {children}
    </div>
  );
}

function EditFieldButton({onClick}: {onClick: () => void}) {
  return (
    <Tooltip title="Edit field" delayMs={DOCUMENTATION_DELAY}>
      <button className="hover-edit-button" onClick={onClick}>
        <Icon icon="edit" />
      </button>
    </Tooltip>
  );
}

function CopyFromParentButton({onClick}: {onClick: () => void}) {
  return (
    <Tooltip title="Copy from previous commit" delayMs={DOCUMENTATION_DELAY}>
      <button className="hover-edit-button" onClick={onClick}>
        <Icon icon="clippy" />
      </button>
    </Tooltip>
  );
}
