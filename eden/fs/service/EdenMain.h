/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#pragma once

#include <map>
#include <string>

#include "eden/fs/service/EdenServer.h"
#include "eden/fs/store/BackingStore.h"
#include "eden/fs/telemetry/IActivityRecorder.h"

namespace facebook::eden {

class EdenConfig;
struct SessionInfo;

/**
 * Allows EdenMain subclasses to register BackingStores.
 */
class DefaultBackingStoreFactory : public BackingStoreFactory {
 public:
  using Factory =
      std::function<std::shared_ptr<BackingStore>(const CreateParams&)>;

  std::shared_ptr<BackingStore> createBackingStore(
      BackingStoreType type,
      const CreateParams& params) override;

  void registerFactory(BackingStoreType type, Factory factory);

 private:
  std::map<BackingStoreType, Factory> registered_;
};

/**
 * Hooks to customize the flavor of the edenfs daemon build.
 */
class EdenMain {
 public:
  virtual ~EdenMain() = default;

  virtual std::string getEdenfsBuildName() = 0;
  virtual std::string getEdenfsVersion() = 0;
  virtual std::string getLocalHostname() = 0;
  virtual void init(int* argc, char*** argv) = 0;
  virtual void prepare(const EdenServer& server) = 0;
  virtual void prepareConfig(EdenConfig&) {}
  virtual void cleanup() = 0;
  virtual ActivityRecorderFactory getActivityRecorderFactory() = 0;

  void runServer(const EdenServer& server);

  BackingStoreFactory* getBackingStoreFactory() {
    return &backingStoreFactory_;
  }

 protected:
  void registerStandardBackingStores();

  void registerBackingStore(
      BackingStoreType type,
      DefaultBackingStoreFactory::Factory factory) {
    backingStoreFactory_.registerFactory(type, std::move(factory));
  }

 private:
  DefaultBackingStoreFactory backingStoreFactory_;
};

/**
 * A default, open-source implementation of EdenMain.
 */
class DefaultEdenMain : public EdenMain {
 public:
  std::string getEdenfsBuildName() override;
  std::string getEdenfsVersion() override;
  std::string getLocalHostname() override;
  void init(int* argc, char*** argv) override;
  void prepare(const EdenServer& server) override;
  void cleanup() override {}
  ActivityRecorderFactory getActivityRecorderFactory() override;
};

int runEdenMain(EdenMain&& main, int argc, char** argv);

} // namespace facebook::eden
