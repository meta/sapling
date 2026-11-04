/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import type {ParsedDiff} from 'shared/patch/types';
import type {DiffComment, DiffCommentReaction, DiffId} from '../types';

import {ErrorNotice} from 'isl-components/ErrorNotice';
import {Icon} from 'isl-components/Icon';
import {Subtle} from 'isl-components/Subtle';
import {Tooltip} from 'isl-components/Tooltip';
import {useAtom} from 'jotai';
import {useEffect} from 'react';
import {ComparisonType} from 'shared/Comparison';
import {cn} from 'shared/cn';
import {group} from 'shared/utils';
import {layout} from '../../../components/theme/layout';
import {spacing} from '../../../components/theme/tokens';
import {AvatarImg} from '../Avatar';
import {SplitDiffTable} from '../ComparisonView/SplitDiffView/SplitDiffHunk';
import {Column, Row} from '../ComponentUtils';
import {Link} from '../Link';
import {T, t} from '../i18n';
import platform from '../platform';
import {RelativeDate} from '../relativeDate';
import css from './DiffComments.module.css';
import {diffCommentData} from './codeReviewAtoms';

function Comment({comment, isTopLevel}: {comment: DiffComment; isTopLevel?: boolean}) {
  return (
    <Row className={css.comment}>
      <Column className={css.left}>
        <AvatarImg username={comment.author} url={comment.authorAvatarUri} className={css.avatar} />
      </Column>
      <Column className={css.commentInfo}>
        <b className={css.author}>{comment.author}</b>
        <div>
          {isTopLevel && comment.filename && (
            <Link
              className={css.inlineCommentFilename}
              onClick={() =>
                comment.filename && platform.openFile(comment.filename, {line: comment.line})
              }>
              {comment.filename}
              {comment.line == null ? '' : ':' + comment.line}
            </Link>
          )}
          <div className={css.commentContent}>
            <div className="rendered-markup" dangerouslySetInnerHTML={{__html: comment.html}} />
          </div>
          {comment.suggestedChange != null && comment.suggestedChange.patch != null && (
            <InlineDiff patch={comment.suggestedChange.patch} />
          )}
        </div>
        <Subtle className={css.byline}>
          <RelativeDate date={comment.created} />
          <Reactions reactions={comment.reactions} />
          {comment.isResolved === true ? (
            <span>
              <T>Resolved</T>
            </span>
          ) : comment.isResolved === false ? (
            <span>
              <T>Unresolved</T>
            </span>
          ) : null}
        </Subtle>
        {comment.replies.map((reply, i) => (
          <Comment key={i} comment={reply} />
        ))}
      </Column>
    </Row>
  );
}

function InlineDiff({patch}: {patch: ParsedDiff}) {
  const path = patch.newFileName ?? '';
  return (
    <div className={css.diffView}>
      <div className="split-diff-view">
        <SplitDiffTable
          patch={patch}
          path={path}
          ctx={{
            collapsed: false,
            id: {
              comparison: {type: ComparisonType.HeadChanges},
              path,
            },
            setCollapsed: () => null,
            display: 'unified',
          }}
        />
      </div>
    </div>
  );
}

const emoji: Record<DiffCommentReaction['reaction'], string> = {
  LIKE: '👍',
  WOW: '😮',
  SORRY: '🤗',
  LOVE: '❤️',
  HAHA: '😆',
  ANGER: '😡',
  SAD: '😢',
  // GitHub reactions
  CONFUSED: '😕',
  EYES: '👀',
  HEART: '❤️',
  HOORAY: '🎉',
  LAUGH: '😄',
  ROCKET: '🚀',
  THUMBS_DOWN: '👎',
  THUMBS_UP: '👍',
};

function Reactions({reactions}: {reactions: Array<DiffCommentReaction>}) {
  if (reactions.length === 0) {
    return null;
  }
  const groups = Object.entries(group(reactions, r => r.reaction)).filter(
    (group): group is [DiffCommentReaction['reaction'], DiffCommentReaction[]] =>
      (group[1]?.length ?? 0) > 0,
  );
  groups.sort((a, b) => b[1].length - a[1].length);
  const total = groups.reduce((last, g) => last + g[1].length, 0);
  // Show only the 3 most used reactions as emoji, even if more are used
  const icons = groups.slice(0, 2).map((g, i) => <span key={i}>{emoji[g[0]]}</span>);
  const names = reactions.map(r => r.name);
  return (
    <Tooltip title={names.join(', ')}>
      <Row style={{gap: spacing.half}}>
        <span style={{letterSpacing: '-2px'}}>{icons}</span>
        <span>{total}</span>
      </Row>
    </Tooltip>
  );
}

export default function DiffCommentsDetails({diffId}: {diffId: DiffId}) {
  const [comments, refresh] = useAtom(diffCommentData(diffId));
  useEffect(() => {
    // make sure we fetch whenever loading the UI again
    refresh();
  }, [refresh]);

  if (comments.state === 'loading') {
    return (
      <div>
        <Icon icon="loading" />
      </div>
    );
  }
  if (comments.state === 'hasError') {
    return (
      <div>
        <ErrorNotice title={t('Failed to fetch comments')} error={comments.error as Error} />
      </div>
    );
  }
  return (
    <div className={cn(layout.flexCol, css.list)}>
      {comments.data.map((comment, i) => (
        <Comment key={i} comment={comment} isTopLevel />
      ))}
    </div>
  );
}
