/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import type {ReactNode} from 'react';

import {Button} from 'isl-components/Button';
import {tracker} from '../analytics';
import {T} from '../i18n';
import {FrecencyStore} from './FrecencyStore';

import './CommitSuggestions.css';

export const recentTags = new FrecencyStore({
  storageKey: 'ISL_RECENT_TAGS',
  maxVisible: 5,
});

export function SuggestedTags({
  existingTags,
  addTag,
}: {
  existingTags: Array<string>;
  addTag: (value: string) => unknown;
}) {
  const recent = recentTags.getRecent().filter(s => !existingTags.includes(s));

  if (recent.length === 0) {
    return null;
  }

  return (
    <div className="commit-suggestions" data-testid="suggested-tags">
      <div data-testid="recent-tags-list">
        <div className="suggestion-header">
          <T>Recent</T>
        </div>
        <div className="suggestions">
          {recent.map(s => (
            <Suggestion
              key={s}
              onClick={() => {
                addTag(s);
                tracker.track('AcceptSuggestedTag', {extras: {type: 'recent'}});
              }}>
              {s}
            </Suggestion>
          ))}
        </div>
      </div>
    </div>
  );
}

function Suggestion({children, onClick}: {children: ReactNode; onClick: () => unknown}) {
  return (
    <Button className="suggestion token" onClick={onClick}>
      {children}
    </Button>
  );
}
