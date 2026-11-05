/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include "eden/fs/testharness/TestServer.h"

#include <folly/io/async/EventBase.h>

#include "eden/common/telemetry/SessionInfo.h"
#include "eden/common/testharness/TempFile.h"
#include "eden/common/utils/UserInfo.h"
#include "eden/fs/config/EdenConfig.h"
#include "eden/fs/service/EdenServer.h"
#include "eden/fs/service/StartupLogger.h"
#include "eden/fs/store/BackingStore.h"
#include "eden/fs/telemetry/IActivityRecorder.h"
#include "eden/fs/telemetry/IScribeLogger.h"
#include "eden/fs/testharness/FakePrivHelper.h"

using std::make_shared;
using std::make_unique;
using std::unique_ptr;

namespace facebook::eden {

namespace {

class EmptyBackingStoreFactory : public BackingStoreFactory {
  std::shared_ptr<BackingStore> createBackingStore(
      BackingStoreType,
      const CreateParams&) override {
    throw std::logic_error("TestServer has no BackingStores by default");
  }
};

EmptyBackingStoreFactory gEmptyBackingStoreFactory;

} // namespace

TestServer::TestServer() : TestServer(Options{}) {}

TestServer::TestServer(const Options& options) : tmpDir_(makeTempDir()) {
  auto startupSubscriberChannel = std::make_shared<StartupStatusChannel>();
  server_ = createServer(getTmpDir(), startupSubscriberChannel, options);
  prepareResult_ = server_->prepare(
      make_shared<ForegroundStartupLogger>(
          std::move(startupSubscriberChannel)));
}

TestServer::~TestServer() = default;

AbsolutePath TestServer::getTmpDir() const {
  return canonicalPath(tmpDir_.path().string());
}

void TestServer::waitUntilReady() {
  if (prepareResult_.valid()) {
    std::move(prepareResult_).getVia(server_->getMainEventBase());
  }
}

unique_ptr<EdenServer> TestServer::createServer(
    AbsolutePathPiece tmpDir,
    std::shared_ptr<StartupStatusChannel> startupSubscriberChannel,
    const Options& options) {
  auto edenDir = tmpDir + "eden"_pc;
  ensureDirectoryExists(edenDir);

  auto userInfo = UserInfo::lookup();
  userInfo.setHomeDirectory(tmpDir + "home"_pc);
  auto config = make_shared<EdenConfig>(
      getUserConfigVariables(userInfo),
      userInfo.getHomeDirectory(),
      tmpDir + "etc"_pc,
      EdenConfig::SourceVector{
          std::make_shared<NullConfigSource>(ConfigSourceType::SystemConfig),
          std::make_shared<NullConfigSource>(ConfigSourceType::Dynamic),
          std::make_shared<NullConfigSource>(ConfigSourceType::UserConfig)});
  auto privHelper = make_unique<FakePrivHelper>();
  config->edenDir.setValue(edenDir, ConfigSourceType::CommandLine);
  config->enableNfsServer.setValue(
      options.enableNfsServer, ConfigSourceType::CommandLine);
  if (options.takeoverRequestGrace) {
    config->takeoverRequestGrace.setValue(
        *options.takeoverRequestGrace, ConfigSourceType::CommandLine);
  }
  if (options.takeoverWaitsForRequests) {
    config->takeoverWaitsForRequests.setValue(
        *options.takeoverWaitsForRequests, ConfigSourceType::CommandLine);
  }
  if (options.takeoverRecentActivityWindow) {
    config->takeoverRecentActivityWindow.setValue(
        *options.takeoverRecentActivityWindow, ConfigSourceType::CommandLine);
  }
  // Unit tests must never reach production Scuba, whatever the build mode.
  config->enableScribeLogging.setValue(false, ConfigSourceType::CommandLine);
#ifdef _WIN32
  config->enableEdenMenu.setValue(false, ConfigSourceType::SystemConfig);
#endif // _WIN32

  return make_unique<EdenServer>(
      std::vector<std::string>{"edenfs_unit_test"},
      userInfo,
      makeRefPtr<EdenStats>(),
      SessionInfo{},
      std::move(privHelper),
      config,
      [](std::shared_ptr<const EdenMount>) {
        return std::make_unique<NullActivityRecorder>();
      },
      options.backingStoreFactory ? options.backingStoreFactory
                                  : &gEmptyBackingStoreFactory,
      make_shared<NullScribeLogger>(),
      std::move(startupSubscriberChannel),
      "test server");
}

} // namespace facebook::eden
