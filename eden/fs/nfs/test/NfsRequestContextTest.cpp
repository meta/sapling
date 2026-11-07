/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#ifndef _WIN32

#include "eden/fs/nfs/NfsRequestContext.h"

#include <gtest/gtest.h>

#include "eden/common/utils/ProcessInfoCache.h"
#include "eden/fs/nfs/rpc/Rpc.h"
#include "eden/fs/telemetry/EdenFsEventsLogger.h"
#include "eden/fs/utils/ProcessAccessLog.h"

namespace {

using namespace facebook::eden;

/**
 * Verifies the threading Nfsd3::dispatchRpc relies on: an NfsRequestContext
 * built from a request's parsed AUTH_SYS credential must expose the client
 * uid/gid through its ObjectFetchContext accessors.
 */
struct NfsRequestContextTest : ::testing::Test {
  std::shared_ptr<ProcessAccessLog> processAccessLog{
      std::make_shared<ProcessAccessLog>(std::make_shared<ProcessInfoCache>())};

  std::unique_ptr<NfsRequestContext> makeContext(
      const std::optional<authsys_parms>& authSysCreds) {
    return std::make_unique<NfsRequestContext>(
        /*xid=*/1,
        nfsv3Procs::getattr,
        processAccessLog,
        std::make_shared<EdenFsEventsLogger>(nullptr),
        /*longRunningFsRequestThreshold=*/std::chrono::nanoseconds{0},
        authSysCreds);
  }
};

TEST_F(NfsRequestContextTest, root_creds_reach_the_fetch_context) {
  auto context =
      makeContext(authsys_parms{/*stamp=*/1, "mac", /*uid=*/0, /*gid=*/0, {0}});
  EXPECT_EQ(context->getObjectFetchContext()->getClientUid(), 0u);
  EXPECT_EQ(context->getObjectFetchContext()->getClientGid(), 0u);
}

TEST_F(NfsRequestContextTest, user_creds_reach_the_fetch_context) {
  auto context = makeContext(
      authsys_parms{/*stamp=*/1, "mac", /*uid=*/501, /*gid=*/20, {20}});
  EXPECT_EQ(context->getObjectFetchContext()->getClientUid(), 501u);
  EXPECT_EQ(context->getObjectFetchContext()->getClientGid(), 20u);
}

TEST_F(NfsRequestContextTest, missing_creds_yield_no_client_identity) {
  auto context = makeContext(std::nullopt);
  EXPECT_EQ(context->getObjectFetchContext()->getClientUid(), std::nullopt);
  EXPECT_EQ(context->getObjectFetchContext()->getClientGid(), std::nullopt);
}

TEST_F(NfsRequestContextTest, procedure_name_is_the_fetch_cause_detail) {
  auto context = makeContext(std::nullopt);
  EXPECT_EQ(context->getObjectFetchContext()->getCauseDetail(), "GETATTR");
}

} // namespace

#endif
