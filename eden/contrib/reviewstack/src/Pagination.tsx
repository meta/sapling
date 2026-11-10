/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

import type {PageInfo} from './generated/graphql';
import type {PaginationParams} from './github/types';

import {ArrowLeftIcon, ArrowRightIcon} from '@primer/octicons-react';
import {Box, IconButton, Text} from '@primer/react';
import {useCallback, useEffect, useState} from 'react';

type Props = {
  id: string;
  pageInfo: PageInfo;
  pageSize: number;
  setPagination: (pagination: PaginationParams) => void;
  totalCount: number;
};

export default function Pagination({
  id,
  pageInfo,
  pageSize,
  setPagination,
  totalCount,
}: Props): React.ReactElement {
  const [page, setPage] = useState(1);
  const totalPages = Math.ceil(totalCount / pageSize);

  // Reset to the first page when there is a new search.
  useEffect(() => {
    setPage(1);
  }, [id, setPage]);

  const {startCursor, endCursor, hasPreviousPage, hasNextPage} = pageInfo;

  const onPrev = useCallback(() => {
    setPagination({last: pageSize, before: startCursor});
    setPage(page => Math.max(1, page - 1));
  }, [pageSize, startCursor, setPage, setPagination]);

  const onNext = useCallback(() => {
    setPagination({first: pageSize, after: endCursor});
    setPage(page => Math.min(totalPages, page + 1));
  }, [pageSize, endCursor, setPage, setPagination, totalPages]);

  return (
    <Box display="flex" alignItems="center" gridGap={2}>
      <Box display="flex" gridGap={1}>
        <IconButton disabled={!hasPreviousPage} icon={ArrowLeftIcon} onClick={onPrev} aria-label="Prev">
          Prev
        </IconButton>
        <IconButton disabled={!hasNextPage} icon={ArrowRightIcon} onClick={onNext} aria-label="Next">
          Next
        </IconButton>
      </Box>
      <Text fontSize={1}>
        Page {page} of {totalPages}
      </Text>
    </Box>
  );
}
