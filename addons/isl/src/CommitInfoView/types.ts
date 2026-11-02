/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import type {ComponentType} from 'react';

/**
 * Either a commit hash, or 'commit/foo' when making a new commit on top of commit hash 'foo'.
 */
export type HashKey = `commit/${string}` | string;

/**
 * Field name and hash key that are separated by +, used for indexing the cached suggestions and map of funnel trackers.
 */
export type FieldNameAndHashKey = `${string}+${HashKey}`;

/**
 * Values for each field key in a commit message.
 */
export type CommitMessageFields = Record<string, string | Array<string>>;

export type TypeaheadKind =
  | 'meta-user'
  | 'meta-task'
  | 'meta-tag'
  | 'meta-diff'
  | 'meta-privacy-context'
  | 'meta-gk'
  | 'meta-jk'
  | 'meta-qe'
  | 'meta-abprop'
  | 'meta-oncall';

/**
 * Which fields of the message should display as editors instead of rendered values.
 * This is derived from edits to the commit message.
 *
 * ```
 * {
 *   title: boolean,
 *   description: boolean,
 *   ...
 * }
 * ```
 */
export type FieldsBeingEdited = Record<string, boolean>;

/**
 * Dynamic configuration for a single field in a commit message
 */
export type FieldConfig = {
  /**
   * Label for this field, and the value used to parse this key from the string.
   * For example, "Summary" corresponds to 'Summary:' in the commit message.
   * There are some specially handled values:
   *   'Title' -> we don't look for "title: foo", we assume first line is the title always.
   *   'Description' -> we don't look for "description: foo", description is handled as the entire message
   */
  key: 'Title' | string;
  /** Codicon to show next to this field */
  icon: string;
  /** Whether this field may be rendered from markup into html */
  isRenderableMarkup?: boolean;
} & (
  | {
      /**
       * Type of the field to show in the UI.
       * textarea => long form content, with extra buttons for image uploading, etc. Supports vertical resize.
       * field => single-line, tokenized field
       * title => non-resizeable textarea for the title, which has special rendering.
       * read-only => this field should be parsed from the commit message but you don't need to edit it. Usually, it's something added by automation.
       */
      type: 'title' | 'textarea' | 'read-only';
    }
  | {
      type: 'field';
      typeaheadKind: TypeaheadKind;
      maxTokens?: number;
      /** pre-process value in commit message to extract token values */
      extractValues?: (text: string) => Array<string>;
      /** post-process token values before placing it in the actual commit message */
      formatValues?: (tokens: Array<string>) => string | undefined;
      getUrl?: (token: string) => string;
    }
  | {
      type: 'custom';
      renderEditor: ComponentType<{
        field: FieldConfig;
        content: string;
        setEditedField: (fieldValue: string) => unknown;
        autoFocus?: boolean;
        extraProps?: Record<string, unknown>;
      }>;
      renderDisplay: ComponentType<{content: string}>;
    }
);
