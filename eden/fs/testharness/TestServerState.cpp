/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include "eden/fs/testharness/TestServerState.h"

#include <memory>

#include "eden/common/telemetry/NullStructuredLogger.h"
#include "eden/common/utils/ProcessInfoCache.h"
#include "eden/common/utils/UnboundedQueueExecutor.h"
#include "eden/common/utils/UserInfo.h"
#include "eden/fs/config/EdenConfig.h"
#include "eden/fs/config/ReloadableConfig.h"
#include "eden/fs/inodes/ServerState.h"
#include "eden/fs/notifications/CommandNotifier.h"
#include "eden/fs/telemetry/EdenStats.h"
#include "eden/fs/telemetry/ErrorLogger.h"
#include "eden/fs/testharness/FakeClock.h"
#include "eden/fs/testharness/FakePrivHelper.h"

namespace facebook::eden {

std::shared_ptr<ServerState> createTestServerState(
    std::shared_ptr<UnboundedQueueExecutor> fsChannelThreadPool) {
  // Use a real thread pool rather than a ManualExecutor so that work scheduled
  // on the ServerState executor (e.g. ThriftGlobImpl::glob) actually runs;
  // callers drive the resulting future with a blocking get().
  auto executor =
      std::make_shared<UnboundedQueueExecutor>(1, "TestServerState");
  if (!fsChannelThreadPool) {
    fsChannelThreadPool = executor;
  }
  auto edenConfig = EdenConfig::createTestEdenConfig();
  // This test helper does not provide the EventBase required by NFS servers.
  edenConfig->enableNfsServer.setValue(false, ConfigSourceType::Default, true);
  auto reloadableConfig = std::make_shared<ReloadableConfig>(edenConfig);

  return std::make_shared<ServerState>(
      UserInfo::lookup(),
      makeRefPtr<EdenStats>(),
      SessionInfo{},
      std::make_shared<FakePrivHelper>(),
      executor,
      fsChannelThreadPool,
      std::make_shared<FakeClock>(),
      std::make_shared<ProcessInfoCache>(),
      std::make_shared<NullStructuredLogger>(),
      std::make_shared<NullStructuredLogger>(),
      std::make_shared<ErrorLogger>(),
      std::make_shared<ReloadableConfig>(edenConfig),
      *edenConfig,
      nullptr,
      std::make_shared<CommandNotifier>(reloadableConfig),
      /*enableFaultInjection=*/true);
};

} // namespace facebook::eden
