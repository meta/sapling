/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import type {LabelFragment} from './generated/graphql';
import type {PullsPullRequest} from './github/pullsTypes';
import type {PaginationParams} from './github/types';

import ActorAvatar from './ActorAvatar';
import CenteredSpinner from './CenteredSpinner';
import CommentCount from './CommentCount';
import Pagination from './Pagination';
import PullRequestLink from './PullRequestLink';
import PullRequestReviewDecisionLabel from './PullRequestReviewDecisionLabel';
import RepoLabelsInput from './RepoLabelsInput';
import {CURSOR_POINTER} from './constants';
import {PullRequestState} from './generated/graphql';
import {gitHubPullRequestsAtom} from './jotai/atoms';
import {formatISODate} from './utils';
import {ActionMenu, Box, IssueLabelToken, PageLayout, SubNav, Text} from '@primer/react';
import {useAtomValue} from 'jotai';
import {Fragment, Suspense, useCallback, useMemo, useState} from 'react';
import {notEmpty} from 'shared/utils';

const PAGE_SIZE = 25;
const DEFAULT_PAGINATION: PaginationParams = {first: PAGE_SIZE};
const OPEN_STATES = [PullRequestState.Open];
const CLOSED_STATES = [PullRequestState.Closed, PullRequestState.Merged];

type Tab = 'open' | 'closed';

export default function Pulls() {
  const [pagination, setPagination] = useState<PaginationParams>(DEFAULT_PAGINATION);
  const [tab, setTab] = useState<Tab>('open');
  const [labels, setLabels] = useState<LabelFragment[]>([]);

  const labelIDs = useMemo(() => new Set(labels.map(({id}) => id)), [labels]);

  const onClickOpenTab = useCallback(() => {
    setPagination(DEFAULT_PAGINATION);
    setTab('open');
  }, [setPagination, setTab]);

  const onClickClosedTab = useCallback(() => {
    setPagination(DEFAULT_PAGINATION);
    setTab('closed');
  }, [setPagination, setTab]);

  const onSelectLabel = useCallback(
    ({id, name, color}: LabelFragment, isExisting: boolean) => {
      if (!isExisting) {
        setLabels(labels => [...labels, {id, name, color}]);
      } else {
        setLabels(labels => labels.filter(label => label.id !== id));
      }
    },
    [setLabels],
  );

  const onClickLabelToken = useCallback((label: LabelFragment) => setLabels([label]), [setLabels]);

  return (
    <PageLayout>
      <PageLayout.Content>
        <SubNav>
          <SubNav.Links>
            <SubNav.Link onClick={onClickOpenTab} selected={tab === 'open'} sx={CURSOR_POINTER}>
              Open
            </SubNav.Link>
            <SubNav.Link onClick={onClickClosedTab} selected={tab === 'closed'} sx={CURSOR_POINTER}>
              Closed
            </SubNav.Link>
          </SubNav.Links>
          <ActionMenu>
            <ActionMenu.Button>Labels</ActionMenu.Button>
            <ActionMenu.Overlay width="medium">
              <RepoLabelsInput existingLabelIDs={labelIDs} onSelect={onSelectLabel} />
            </ActionMenu.Overlay>
          </ActionMenu>
        </SubNav>
        <Suspense fallback={<CenteredSpinner />}>
          <PullsBootstrap
            labels={labels}
            onClickLabelToken={onClickLabelToken}
            pagination={pagination}
            setPagination={setPagination}
            tab={tab}
          />
        </Suspense>
      </PageLayout.Content>
    </PageLayout>
  );
}

type Props = {
  labels: LabelFragment[];
  onClickLabelToken: (label: LabelFragment) => void;
  pagination: PaginationParams;
  setPagination: (pagination: PaginationParams) => void;
  tab: Tab;
};

function PullsBootstrap({labels, onClickLabelToken, pagination, setPagination, tab}: Props) {
  const states = tab === 'open' ? OPEN_STATES : CLOSED_STATES;
  const pullsWithPageInfo = useAtomValue(
    gitHubPullRequestsAtom({
      ...pagination,
      labels: labels.map(({name}) => name),
      states,
    }),
  );

  const pullRequests = useMemo(() => {
    const seen = new Set();
    return (pullsWithPageInfo?.pullRequests ?? []).filter(({number}) => {
      if (seen.has(number)) {
        return false;
      }
      seen.add(number);
      return true;
    });
  }, [pullsWithPageInfo?.pullRequests]);

  if (pullsWithPageInfo == null || pullRequests.length === 0) {
    return <div>No pull requests found.</div>;
  }

  const {pageInfo, totalCount} = pullsWithPageInfo;

  let paginationEl = null;
  if (pageInfo.hasNextPage || pageInfo.hasPreviousPage) {
    paginationEl = (
      <Pagination
        id={tab}
        pageInfo={pageInfo}
        pageSize={PAGE_SIZE}
        setPagination={setPagination}
        totalCount={totalCount}
      />
    );
  }

  return (
    <>
      <Box marginY={3}>
        <PullsList onClickLabelToken={onClickLabelToken} pullRequests={pullRequests} />
      </Box>
      {paginationEl}
    </>
  );
}

type PullsListProps = {
  onClickLabelToken: (label: LabelFragment) => void;
  pullRequests: PullsPullRequest[];
};

function PullsList({onClickLabelToken, pullRequests}: PullsListProps): React.ReactElement {
  return (
    <Box display="grid" gridTemplateColumns="35px 60px 1fr 100px 60px 145px" fontSize={1}>
      {pullRequests.map(({author, comments, labels, number, reviewDecision, title, updatedAt}) => (
        <Fragment key={number}>
          <Cell>
            <ActorAvatar login={author?.login} url={author?.avatarUrl} size={24} />
          </Cell>
          <Cell>
            <PullRequestLink number={number}>
              <Text>#{number}</Text>
            </PullRequestLink>
          </Cell>
          <Cell>
            <Box display="flex" flexWrap="wrap" alignItems="center" gridGap={1}>
              <PullRequestLink number={number}>
                <Text>{title}</Text>
              </PullRequestLink>
              {(labels?.nodes ?? []).filter(notEmpty).map(({id, name, color}) => (
                <IssueLabelToken
                  key={id}
                  text={name}
                  fillColor={`#${color}`}
                  onClick={() => onClickLabelToken({id, name, color})}
                />
              ))}
            </Box>
          </Cell>
          <Cell>{formatISODate(updatedAt, false)}</Cell>
          <Cell>
            <CommentCount count={comments.totalCount} />
          </Cell>
          <Cell>
            <PullRequestReviewDecisionLabel reviewDecision={reviewDecision} />
          </Cell>
        </Fragment>
      ))}
    </Box>
  );
}

function Cell({children}: {children: React.ReactNode}): React.ReactElement {
  return <Box padding={1}>{children}</Box>;
}
