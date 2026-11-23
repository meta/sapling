/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include "eden/fs/telemetry/EdenErrorInfoBuilder.h"
#include <gtest/gtest.h>
#include <cerrno>
#include <stdexcept>
#include <system_error>
#include <utility>

#include <folly/CPortability.h>

#include "eden/common/telemetry/DynamicEvent.h"
#include "eden/fs/config/EdenConfig.h"
#include "eden/fs/config/ReloadableConfig.h"
#include "eden/fs/telemetry/DaemonError.h"
#include "eden/fs/telemetry/EdenStats.h"
#include "eden/fs/telemetry/ErrorLogger.h"
#include "eden/fs/telemetry/test/CapturingXplatLogger.h"

#include "eden/fs/telemetry/ErrorArg.h"
#include "eden/fs/telemetry/ThrowTraceCapture.h"
#include "eden/fs/telemetry/XplatKeys.h"

#include <fb303/ServiceData.h>
#include <fmt/core.h>

using namespace facebook::eden;

namespace {
const bool kCaptureEnabled = [] {
  setThrowTraceCaptureEnabled(true);
  return true;
}();

[[noreturn]] FOLLY_NOINLINE void throwRuntimeError() {
  throw std::runtime_error("fuse read failed");
}

int64_t getCounter(EdenStats& stats, std::string_view name) {
  stats.flush();
  try {
    return facebook::fb303::ServiceData::get()->getCounter(
        fmt::format("{}.sum", name));
  } catch (...) {
    return 0;
  }
}
} // namespace

TEST(EdenErrorInfoTest, InitializeFuseEdenErrorInfoWithException) {
  try {
    throwRuntimeError();
  } catch (const std::exception& ex) {
    auto info = EdenErrorInfo::fuse(ex, 42, "/mnt/repo").create();

    EXPECT_EQ(info.component, EdenComponent::Fuse);
    EXPECT_EQ(info.errorMessage, "fuse read failed");
    EXPECT_EQ(info.inode.value(), 42);
    EXPECT_EQ(info.mountPoint.value(), "/mnt/repo");
    EXPECT_FALSE(info.errorCode.has_value());
    EXPECT_NE(
        info.exceptionType.value().find("runtime_error"), std::string::npos);
    ASSERT_TRUE(info.stackTrace.has_value());
    EXPECT_NE(info.stackTrace->find("Stack trace:"), std::string::npos)
        << "Stack trace should contain raw trace section, got: "
        << *info.stackTrace;
#ifndef _WIN32
    // Windows without PDB debug info can't resolve file paths or function names
    EXPECT_NE(
        info.stackTrace->find("EdenErrorInfoBuilderTest.cpp"),
        std::string::npos)
        << "Stack trace should contain source file, got: " << *info.stackTrace;
    EXPECT_NE(info.stackTrace->find("throwRuntimeError"), std::string::npos)
        << "Stack trace should contain the throwing function, got: "
        << *info.stackTrace;
#endif
  }
}

TEST(EdenErrorInfoTest, InitializeFuseEdenErrorInfoWithStringMessage) {
  auto info =
      EdenErrorInfo::fuse("inode failed to load", 99, "/mnt/repo").create();

  EXPECT_EQ(info.component, EdenComponent::Fuse);
  EXPECT_EQ(info.errorMessage, "inode failed to load");
  EXPECT_EQ(info.inode.value(), 99);
  EXPECT_FALSE(info.errorCode.has_value());
  EXPECT_FALSE(info.errorName.has_value());
  EXPECT_FALSE(info.exceptionType.has_value());
  ASSERT_TRUE(info.stackTrace.has_value());
  EXPECT_NE(
      info.stackTrace->find("EdenErrorInfoBuilderTest.cpp"), std::string::npos)
      << "Stack trace should contain source file, got: " << *info.stackTrace;
}

TEST(EdenErrorInfoTest, FuseErrorInfoOverridesErrorCodeAndName) {
  std::runtime_error ex("request timed out");
  auto info = EdenErrorInfo::fuse(ex, 123, "/mnt/repo")
                  .withErrorCode(ETIMEDOUT)
                  .withErrorName("ETIMEDOUT")
                  .create();

  EXPECT_EQ(info.component, EdenComponent::Fuse);
  EXPECT_EQ(info.errorMessage, "request timed out");
  EXPECT_EQ(info.inode.value(), 123);
  EXPECT_EQ(info.errorCode.value(), ETIMEDOUT);
  EXPECT_EQ(info.errorName.value(), "ETIMEDOUT");
  EXPECT_NE(
      info.exceptionType.value().find("runtime_error"), std::string::npos);
}

TEST(EdenErrorInfoTest, InitializeThriftEdenErrorInfoWithSystemError) {
  std::system_error ex(
      std::make_error_code(std::errc::permission_denied), "access denied");
  auto info = EdenErrorInfo::thrift(ex, "hg status").create();

  EXPECT_EQ(info.component, EdenComponent::Thrift);
  EXPECT_EQ(info.clientCommandName.value(), "hg status");
  EXPECT_TRUE(info.errorCode.has_value());
  EXPECT_TRUE(info.errorName.has_value());
  EXPECT_NE(info.exceptionType.value().find("system_error"), std::string::npos);
}

TEST(EdenErrorInfoTest, SparseFieldsSerializedIntoExtrasJsonColumn) {
  std::runtime_error ex("test error");

  // Verify backingStore fields are populated in EdenErrorInfo
  auto info = EdenErrorInfo::backingStore(ex)
                  .withRepoName("fbsource")
                  .withFetchType(FetchType::Blob)
                  .withIsDogfoodingHost(true)
                  .create();
  EXPECT_EQ(info.repoName.value(), "fbsource");
  EXPECT_EQ(info.fetchType.value(), "blob");
  EXPECT_TRUE(info.isDogfoodingHost.value());

  // The FetchType enum and std::string overloads of withFetchType are
  // equivalent: both populate the same fetchType field.
  EXPECT_EQ(fetchTypeToString(FetchType::TreeAux), "tree_aux");
  auto stringOverload = EdenErrorInfo::backingStore(ex)
                            .withFetchType(std::string{"tree"})
                            .create();
  EXPECT_EQ(stringOverload.fetchType.value(), "tree");

  // Verify all sparse fields are serialized into extras JSON column
  auto event = EdenErrorInfo::backingStore(ex)
                   .withRepoName("fbsource")
                   .withFetchType(FetchType::Blob)
                   .withIsDogfoodingHost(true)
                   .createEvent();
  DynamicEvent de;
  event.populate(de);
  const auto& strings = de.getStringMap();
  auto it = strings.find("extras");
  ASSERT_NE(it, strings.end()) << "Should have extras column";
  EXPECT_NE(it->second.find("repo_name"), std::string::npos)
      << "Extras should contain repo_name, got: " << it->second;
  EXPECT_NE(it->second.find("fbsource"), std::string::npos)
      << "Extras should contain fbsource, got: " << it->second;
  EXPECT_NE(it->second.find("fetch_type"), std::string::npos)
      << "Extras should contain fetch_type, got: " << it->second;
  EXPECT_NE(it->second.find("blob"), std::string::npos)
      << "Extras should contain blob, got: " << it->second;
  EXPECT_NE(it->second.find("is_dogfooding_host"), std::string::npos)
      << "Extras should contain is_dogfooding_host, got: " << it->second;

  // Verify clientCommandName is serialized into extras JSON column
  auto thriftEvent = EdenErrorInfo::thrift(ex, "mount").createEvent();
  DynamicEvent thriftDe;
  thriftEvent.populate(thriftDe);
  auto thriftIt = thriftDe.getStringMap().find("extras");
  ASSERT_NE(thriftIt, thriftDe.getStringMap().end());
  EXPECT_NE(thriftIt->second.find("client_command_name"), std::string::npos);
  EXPECT_NE(thriftIt->second.find("mount"), std::string::npos);
}

TEST(EdenErrorInfoTest, ThriftErrorLoggedThroughErrorLogger) {
  CapturingXplatLogger xplatLogger;
  auto config = EdenConfig::createTestEdenConfig();
  config->enableErrorLogging.setValue(true, ConfigSourceType::UserConfig);
  auto reloadableConfig = std::make_shared<ReloadableConfig>(config);
  ErrorLogger errorLogger{reloadableConfig, &xplatLogger};

  std::runtime_error ex("mount not found");
  errorLogger.log(EdenErrorInfo::thrift(ex, "unmount"));

  ASSERT_EQ(xplatLogger.events().size(), 1);
  const auto& strings = xplatLogger.events()[0].event.getStringMap();
  EXPECT_EQ(strings.at("component"), "thrift");
  EXPECT_NE(strings.at("extras").find("unmount"), std::string::npos);
}

TEST(EdenErrorInfoTest, SymbolizationIsDeferredUntilCreate) {
  try {
    throw std::runtime_error("deferred test");
  } catch (const std::exception& ex) {
    // ErrorArg should NOT consume the trace — just record that one exists.
    ErrorArg error(ex);
    EXPECT_TRUE(error.hasCapturedTrace);

    // Trace should still be available — ErrorArg only sets a flag,
    // it doesn't call getThrowSiteStackTrace().
    auto trace = getThrowSiteStackTrace();
    ASSERT_TRUE(trace.has_value())
        << "Trace should still be in thread-local storage after ErrorArg";

    // After getThrowSiteStackTrace() consumed the trace, create()
    // should still work but without a throw-site trace section.
  }
}

TEST(EdenErrorInfoTest, RoutesToXplatLoggerWithoutLegacyGate) {
  auto config = EdenConfig::createTestEdenConfig();
  config->enableErrorLogging.setValue(true, ConfigSourceType::UserConfig);
  auto reloadableConfig = std::make_shared<ReloadableConfig>(config);
  CapturingXplatLogger xplatLogger;
  auto stats = makeRefPtr<EdenStats>();
  ErrorLogger errorLogger{reloadableConfig, &xplatLogger, stats.copy()};

  auto before = getCounter(*stats, "telemetry.errors_via_xplat_logger");
  std::runtime_error ex("mount not found");
  errorLogger.log(EdenErrorInfo::thrift(ex, "unmount"));

  ASSERT_EQ(xplatLogger.events().size(), 1);
  EXPECT_EQ(
      xplatLogger.events()[0].category,
      std::string{xplat_keys::kErrorsCategory});
  EXPECT_EQ(
      xplatLogger.events()[0].event.getStringMap().at("component"), "thrift");
  EXPECT_EQ(
      getCounter(*stats, "telemetry.errors_via_xplat_logger") - before, 1);
}

TEST(EdenErrorInfoTest, RateLimitsPerErrorType) {
  CapturingXplatLogger xplatLogger;
  auto config = EdenConfig::createTestEdenConfig();
  config->enableErrorLogging.setValue(true, ConfigSourceType::UserConfig);
  config->errorLogMaxPerMinute.setValue(1, ConfigSourceType::UserConfig);
  config->errorLogBurst.setValue(1, ConfigSourceType::UserConfig);
  auto reloadableConfig = std::make_shared<ReloadableConfig>(config);
  auto stats = makeRefPtr<EdenStats>();
  ErrorLogger errorLogger{reloadableConfig, &xplatLogger, stats.copy()};

  std::runtime_error ex("mount not found");
  auto logAs = [&](std::string errorType) {
    return errorLogger.log(
        EdenErrorInfo::thrift(ex, "unmount")
            .withErrorType(std::move(errorType)));
  };

  EXPECT_EQ(logAs("first"), ErrorLogOutcome::Logged);
  EXPECT_EQ(logAs("first"), ErrorLogOutcome::RateLimited);
  EXPECT_EQ(logAs("first"), ErrorLogOutcome::RateLimited);
  EXPECT_EQ(logAs("second"), ErrorLogOutcome::Logged);
  EXPECT_EQ(xplatLogger.events().size(), 2);
  EXPECT_EQ(getCounter(*stats, "telemetry.errors_rate_limited"), 2);

  // Turning the limit off admits the next event, which carries the number of
  // events dropped before it.
  config->errorLogMaxPerMinute.setValue(0, ConfigSourceType::UserConfig);
  EXPECT_EQ(logAs("first"), ErrorLogOutcome::Logged);
  ASSERT_EQ(xplatLogger.events().size(), 3);
  const auto& extras =
      xplatLogger.events()[2].event.getStringMap().at("extras");
  EXPECT_NE(extras.find("\"suppressed_count\":2"), std::string::npos);
  EXPECT_EQ(
      xplatLogger.events()[0].event.getStringMap().at("extras").find(
          "suppressed_count"),
      std::string::npos);
}

TEST(EdenErrorInfoTest, RateLimitAppliesWhenErrorLoggingDisabled) {
  CapturingXplatLogger xplatLogger;
  auto config = EdenConfig::createTestEdenConfig();
  config->errorLogMaxPerMinute.setValue(1, ConfigSourceType::UserConfig);
  config->errorLogBurst.setValue(1, ConfigSourceType::UserConfig);
  auto reloadableConfig = std::make_shared<ReloadableConfig>(config);
  ErrorLogger errorLogger{reloadableConfig, &xplatLogger};

  std::runtime_error ex("mount not found");
  EXPECT_EQ(
      errorLogger.log(EdenErrorInfo::thrift(ex, "unmount")),
      ErrorLogOutcome::Disabled);
  EXPECT_EQ(
      errorLogger.log(EdenErrorInfo::thrift(ex, "unmount")),
      ErrorLogOutcome::RateLimited);
  EXPECT_TRUE(xplatLogger.events().empty());
}

TEST(EdenErrorInfoTest, DoesNotLogWithoutXplatLogger) {
  auto config = EdenConfig::createTestEdenConfig();
  config->enableErrorLogging.setValue(true, ConfigSourceType::UserConfig);
  auto reloadableConfig = std::make_shared<ReloadableConfig>(config);
  auto stats = makeRefPtr<EdenStats>();
  ErrorLogger errorLogger{reloadableConfig, nullptr, stats.copy()};

  std::runtime_error ex("mount not found");
  errorLogger.log(EdenErrorInfo::thrift(ex, "unmount"));

  EXPECT_FALSE(errorLogger.isEnabled());
  EXPECT_EQ(getCounter(*stats, "telemetry.errors_via_xplat_logger"), 0);
}
