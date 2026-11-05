/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#pragma once

#include "eden/common/telemetry/SessionInfo.h"
#include "eden/fs/config/ReloadableConfig.h"

namespace facebook::eden {

class EdenMount;

class IScribeLogger {
 public:
  IScribeLogger(
      SessionInfo sessionInfo,
      std::shared_ptr<ReloadableConfig> config)
      : sessionInfo_{std::move(sessionInfo)},
        reloadableConfig_{std::move(config)} {}
  virtual ~IScribeLogger() = default;

  virtual void log(std::string_view category, std::string&& message) = 0;

  /**
   * This allows us to create objects derived from IScribeLogger with
   * only a IScribeLogger pointer
   */
  virtual std::unique_ptr<IScribeLogger> create() = 0;

 protected:
  SessionInfo sessionInfo_;
  std::shared_ptr<ReloadableConfig> reloadableConfig_;
};

class NullScribeLogger : public IScribeLogger {
 public:
  NullScribeLogger() : IScribeLogger{SessionInfo{}, {}} {}

  std::unique_ptr<IScribeLogger> create() override {
    return std::make_unique<NullScribeLogger>();
  }

  void log(std::string_view /*category*/, std::string&& /*message*/) override {}
};

} // namespace facebook::eden
