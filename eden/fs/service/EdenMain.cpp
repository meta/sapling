/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include "eden/fs/service/EdenMain.h"
#include "eden/common/telemetry/SessionId.h"

#include <algorithm>
#include <chrono>
#include <limits>
#include <memory>
#include <optional>
#include <string_view>

#include <fb303/FollyLoggingHandler.h>
#include <fb303/TFunctionStatHandler.h>
#include <fmt/format.h>
#include <folly/Conv.h>
#include <folly/MapUtil.h>
#include <folly/Portability.h>
#include <folly/String.h>
#include <folly/init/Init.h>
#include <folly/logging/LogConfigParser.h>
#include <folly/logging/xlog.h>
#include <folly/portability/Unistd.h>
#include <folly/stop_watch.h>
#include <gflags/gflags.h>
#ifdef __APPLE__
#include <sys/sysctl.h>
#include <sys/time.h>
#endif
#ifdef __linux__
#include <sigbus_memops.h>
#endif
#include <sys/stat.h>
#if defined(__linux__) && __has_include(<systemd/sd-daemon.h>)
#include <systemd/sd-daemon.h> // @manual, autodeps cannot do linux-only dep
#define EDEN_HAVE_SYSTEMD 1
#endif
#include <thrift/lib/cpp2/Flags.h>
#include <thrift/lib/cpp2/server/ThriftServer.h>
#ifdef __APPLE__
#include <ctime>
#endif
#include <filesystem>

#include "eden/common/telemetry/SessionInfo.h"
#include "eden/common/utils/Bug.h"
#include "eden/common/utils/UserInfo.h"
#include "eden/fs/config/CheckoutConfig.h"
#include "eden/fs/config/EdenConfig.h"
#include "eden/fs/eden-config.h"
#include "eden/fs/inodes/ServerState.h"
#include "eden/fs/privhelper/PrivHelper.h"
#include "eden/fs/privhelper/PrivHelperImpl.h"
#include "eden/fs/service/EdenInit.h"
#include "eden/fs/service/EdenServer.h"
#include "eden/fs/service/EdenServiceHandler.h" // for kServiceName
#include "eden/fs/service/StartupLogger.h"
#include "eden/fs/service/StartupStatusSubscriber.h"
#include "eden/fs/store/BackingStoreLogger.h"
#include "eden/fs/store/EmptyBackingStore.h"
#include "eden/fs/store/FilteredBackingStore.h"
#include "eden/fs/store/filter/HgSparseFilter.h"
#include "eden/fs/store/sl/SaplingBackingStore.h"
#include "eden/fs/telemetry/EdenFsEventsLogger.h"
#include "eden/fs/telemetry/IScribeLogger.h"
#include "eden/fs/telemetry/LogEvent.h"
#include "eden/fs/utils/WinStackTrace.h"

#if EDEN_HAVE_GIT
#include "eden/fs/store/git/GitBackingStore.h" // @manual
#endif

#ifdef EDEN_HAVE_FBINIT
#include <common/init/light.h>
#endif

DEFINE_bool(edenfs, false, "This legacy argument is ignored.");
DEFINE_bool(allowRoot, false, "Allow running eden directly as root");
DEFINE_string(edenLogLevel, "DBG2", "Logging level to use at startup");
DEFINE_bool(
    fatal_eden_bug,
    folly::kIsDebug,
    "Abort the daemon when an EDEN_BUG report fires instead of throwing. "
    "Defaults to the build mode and is intended for correctness testing.");

THRIFT_FLAG_DECLARE_bool(server_header_reject_framed);

using folly::StringPiece;
using std::string;

namespace facebook::eden {

namespace {

#ifdef __linux__
// TODO: Remove these fallback functions once all privhelper daemons include
// the getNamespaceInfo method.
std::optional<uint64_t> getNamespaceInode(const char* path) {
  struct stat st = {};
  if (::stat(path, &st) == 0) {
    return st.st_ino;
  }
  XLOGF(DBG5, "Failed to stat {}: {}", path, folly::errnoStr(errno));
  return std::nullopt;
}

std::optional<bool> checkIsRootMountNamespace() {
  auto self = getNamespaceInode("/proc/self/ns/mnt");
  auto root = getNamespaceInode("/proc/1/ns/mnt");
  if (self.has_value() && root.has_value()) {
    return *self == *root;
  }
  return std::nullopt;
}

// Helper macro to convert optional to string for logging
#define OPT_TO_STRING(opt) \
  ((opt).has_value() ? folly::to<std::string>(*(opt)) : "nullopt")

std::optional<std::string> readCgroup() {
  std::string contents;
  if (folly::readFile("/proc/self/cgroup", contents)) {
    // Trim trailing newline(s) for cleaner logging.
    while (!contents.empty() && contents.back() == '\n') {
      contents.pop_back();
    }
    return contents;
  }
  return std::nullopt;
}
#endif

#ifdef EDEN_HAVE_SYSTEMD
class SystemdStartupTimeoutExtender {
 public:
  explicit SystemdStartupTimeoutExtender(std::chrono::nanoseconds extension)
      : extension_{extension} {}

  void reportProgress(std::string_view status) {
    auto now = std::chrono::steady_clock::now();
    if (lastNotifyTime_.has_value() &&
        now - *lastNotifyTime_ < kMinNotifyInterval) {
      return;
    }
    lastNotifyTime_ = now;

    auto extensionUsec =
        std::chrono::duration_cast<std::chrono::microseconds>(extension_)
            .count();
    auto message =
        fmt::format("EXTEND_TIMEOUT_USEC={}\nSTATUS={}", extensionUsec, status);
    auto result = sd_notify(0, message.c_str());
    if (result > 0) {
      XLOGF(
          DBG2,
          "sent systemd startup timeout extension of {}us: {}",
          extensionUsec,
          status);
    } else if (result < 0) {
      XLOGF(
          ERR,
          "sd_notify EXTEND_TIMEOUT_USEC failed: {}",
          folly::errnoStr(-result));
    }
  }

 private:
  static constexpr auto kMinNotifyInterval = std::chrono::seconds(5);

  std::chrono::nanoseconds extension_;
  std::optional<std::chrono::steady_clock::time_point> lastNotifyTime_;
};
#endif

EdenStatsPtr getGlobalEdenStats() {
  // A running EdenFS daemon only needs a single EdenStats instance. Avoid
  // atomic reference counts with RefPtr::singleton. We could use
  // folly::Singleton but that makes unit testing harder.
  static EdenStats* gEdenStats = new EdenStats;
  return EdenStatsPtr::singleton(*gEdenStats);
}

constexpr int kExitCodeSuccess = 0;
constexpr int kExitCodeError = 1;
constexpr int kExitCodeUsage = 2;

} // namespace

std::shared_ptr<BackingStore> DefaultBackingStoreFactory::createBackingStore(
    BackingStoreType type,
    const CreateParams& params) {
  if (auto* fn = folly::get_ptr(registered_, type)) {
    return (*fn)(params);
  }

  throw std::domain_error(
      folly::to<std::string>("unsupported backing store type: ", type));
}

void DefaultBackingStoreFactory::registerFactory(
    BackingStoreType type,
    DefaultBackingStoreFactory::Factory factory) {
  auto [it, inserted] = registered_.emplace(type, std::move(factory));
  if (!inserted) {
    EDEN_BUG() << "attempted to register BackingStore "
               << toBackingStoreString(type) << " twice";
  }
}

void EdenMain::runServer(const EdenServer& server) {
  // ThriftServer::serve() will drive the current thread's EventBase.
  // Verify that we are being called from the expected thread, and will end up
  // driving the EventBase returned by EdenServer::getMainEventBase().
  XCHECK_EQ(
      server.getMainEventBase(),
      folly::EventBaseManager::get()->getEventBase());

  fb303::fbData->setExportedValue("build_package_name", EDEN_PACKAGE_NAME);
  fb303::fbData->setExportedValue("build_package_version", EDEN_VERSION);
  fb303::fbData->setExportedValue("build_package_release", EDEN_RELEASE);
  fb303::fbData->setExportedValue("build_revision", EDEN_BUILD_REVISION);
  fb303::fbData->setExportedValue(
      "build_time_unix", folly::to<std::string>(EDEN_BUILD_TIME_UNIX));

  fb303::withThriftFunctionStats(kServiceName, [&] { server.serve(); });
}

namespace {
std::shared_ptr<SaplingBackingStore> createSaplingBackingStore(
    const BackingStoreFactory::CreateParams& params,
    const AbsolutePath& repoPath,
    const CheckoutConfig& config,
    std::shared_ptr<ReloadableConfig> reloadableConfig,
    std::unique_ptr<SaplingBackingStoreOptions> runtimeOptions) {
  return std::make_shared<SaplingBackingStore>(
      repoPath,
      config.getCaseSensitive(),
      params.sharedStats.copy(),
      params.serverState->getThreadPool().get(),
      reloadableConfig,
      std::move(runtimeOptions),
      params.serverState->getEdenFsEventsLogger(),
      params.serverState->getErrorLogger(),
      std::make_unique<BackingStoreLogger>(
          params.serverState->getEdenFsEventsLogger(),
          params.serverState->getProcessInfoCache()),
      &params.serverState->getFaultInjector());
}
} // namespace

void EdenMain::registerStandardBackingStores() {
  using CreateParams = BackingStoreFactory::CreateParams;

  registerBackingStore(BackingStoreType::EMPTY, [](const CreateParams&) {
    return std::make_shared<EmptyBackingStore>();
  });

  registerBackingStore(BackingStoreType::HG, [](const CreateParams& params) {
    const auto repoPath = realpath(params.name);
    auto reloadableConfig = params.serverState->getReloadableConfig();

    auto runtimeOptions = std::make_unique<SaplingBackingStoreOptions>();
    return createSaplingBackingStore(
        params,
        repoPath,
        params.config,
        reloadableConfig,
        std::move(runtimeOptions));
  });

  registerBackingStore(
      BackingStoreType::FILTEREDHG,
      [](const BackingStoreFactory::CreateParams& params) {
        const auto repoPath = realpath(params.name);
        auto reloadableConfig = params.serverState->getReloadableConfig();
        auto hgSparseFilter = std::make_unique<HgSparseFilter>(repoPath);

        auto options = std::make_unique<SaplingBackingStoreOptions>();
        auto saplingBackingStore = createSaplingBackingStore(
            params,
            repoPath,
            params.config,
            reloadableConfig,
            std::move(options));
        return std::make_shared<FilteredBackingStore>(
            std::move(saplingBackingStore),
            std::move(hgSparseFilter),
            reloadableConfig);
      });

  registerBackingStore(
      BackingStoreType::GIT,
      [](const CreateParams& params) -> std::shared_ptr<BackingStore> {
#if EDEN_HAVE_GIT
        const auto repoPath = realpath(params.name);
        return std::make_shared<GitBackingStore>(repoPath);
#else // EDEN_HAVE_GIT
        (void)params;
        throw std::domain_error(
            "support for Git was not enabled in this EdenFS build");
#endif // EDEN_HAVE_GIT
      });
}

std::string DefaultEdenMain::getEdenfsBuildName() {
  StringPiece version(EDEN_VERSION);
  StringPiece release(EDEN_RELEASE);

  if (!version.empty()) {
    return folly::to<string>("edenfs ", version, "-", release);
  }

  // Assume this is a development build if EDEN_VERSION is unset.
  return "edenfs (dev build)";
}

std::string DefaultEdenMain::getEdenfsVersion() {
  StringPiece version(EDEN_VERSION);

  if (!version.empty()) {
    return folly::to<string>(version);
  }

  return "(dev build)";
}

std::string DefaultEdenMain::getLocalHostname() {
  return getHostname();
}

void DefaultEdenMain::init(int* argc, char*** argv) {
  // OSS buck build should/can also call fbinit. fbinit is required at least for
  // ODS counters to be incremented from Rust (which EdenFS integration tests
  // may want to test).
#ifdef EDEN_HAVE_FBINIT
  facebook::init::initFacebookLight(argc, argv);
#else
  folly::unsafe_unscoped_init(argc, argv);
#endif
}

void DefaultEdenMain::prepare(const EdenServer& /*server*/) {
  fb303::registerFollyLoggingOptionHandlers();

  registerStandardBackingStores();
}

ActivityRecorderFactory DefaultEdenMain::getActivityRecorderFactory() {
  return [](std::shared_ptr<const EdenMount>) {
    return std::make_unique<NullActivityRecorder>();
  };
}

std::shared_ptr<IScribeLogger> DefaultEdenMain::getScribeLogger(
    SessionInfo /*sessionInfo*/,
    std::shared_ptr<EdenConfig> /*edenConfig*/) {
  return std::make_shared<NullScribeLogger>();
}

#ifdef __APPLE__
time_t getBootTimeSysctl() {
  struct timeval boottime;
  size_t size = sizeof(boottime);
  int mib[2] = {CTL_KERN, KERN_BOOTTIME};
  if (sysctl(mib, 2, &boottime, &size, nullptr, 0) < 0) {
    // It cannot find the last system boot time, just return -1
    return -1;
  }
  return boottime.tv_sec;
}
#endif

int runEdenMain(EdenMain&& main, int argc, char** argv) {
  ////////////////////////////////////////////////////////////////////
  // There are two options for running test instances or development builds of
  // EdenFS:
  //
  // 1. EdenFS uses a system (or pre-installed) privhelper so that `sudo` is not
  // required to run the privhelper as root. When installed, the privhelper is
  // setuid-root and thus the EdenFS daemon never runs as root.
  //
  // 2. EdenFS is started with sudo in order to execute a dev instance of
  // privhelper as root.
  //
  // #1 is the default behavior, but #2 can be achieved through the use of
  // environment variables. See prepare_edenfs_privileges() in fs/cli/daemon.py
  // for more information on how this works.
  //
  // Since this code can be started with root privileges, we should be very
  // careful about anything EdenFS does here before it drops privileges.  In
  // general do not add any new code here at the start of main: new
  // initialization logic should only go after the "Root privileges dropped"
  // comment below.
  ////////////////////////////////////////////////////////////////////

  // Start the privhelper process, then drop privileges in the main process.
  // This should be done as early as possible, so that everything else EdenFS
  // does runs only with normal user privileges. Note: as mentioned above, this
  // is not an issue in the default case since EdenFS will not be run as root,
  // and only the privhelper daemon will be run as a setuid-root binary.
  //
  // EdenFS does this even before calling folly::init().  The privhelper server
  // process will call folly::init() on its own.
  //
  // If the privileged parent edenfs process has already started a privhelper
  // process, then the --privhelper_fd flag is given and this child process will
  // use it to connect to the existing privhelper.
  auto identity = UserInfo::lookup();
  auto privHelper = startOrConnectToPrivHelper(identity, argc, argv);
  identity.dropPrivileges();

  ////////////////////////////////////////////////////////////////////
  //// Root privileges dropped
  ////////////////////////////////////////////////////////////////////

#ifdef _WIN32
  installWindowsExceptionFilter();
#endif

  folly::stop_watch<> daemonStart;

  // A non-zero restart count in the environment is how a daemon learns that it
  // was relaunched by the privhelper rather than started by the user. It is
  // clamped to the uint32_t range the privhelper keeps its restart budget in.
  std::optional<uint64_t> numRestarts;
#ifdef __APPLE__
  if (const auto restartCount = std::min<uint64_t>(
          readEdenFsRestartCounterEnv(kEdenFsRestartCountEnv),
          std::numeric_limits<uint32_t>::max());
      restartCount > 0) {
    numRestarts = restartCount;
  }
#endif

#ifdef __linux__
  auto cgroupInfo = readCgroup();

  std::optional<uint64_t> daemonMountNamespace;
  std::optional<uint64_t> daemonPidNamespace;
  std::optional<bool> isDaemonInRootMountNamespace;
  std::optional<bool> isPrivhelperInRootMountNamespace;
  std::optional<uint64_t> rootMountNsInode;
  std::optional<uint64_t> privhelperMountNamespace;
  std::optional<uint64_t> privhelperPidNamespace;
  try {
    auto nsInfo = privHelper->getNamespaceInfoBlocking(getpid());
    rootMountNsInode = nsInfo.rootMountNsInode;
    privhelperMountNamespace = nsInfo.privhelperMountNsInode;
    privhelperPidNamespace = nsInfo.privhelperPidNsInode;
    daemonMountNamespace = nsInfo.daemonMountNsInode;
    daemonPidNamespace = nsInfo.daemonPidNsInode;
    isDaemonInRootMountNamespace =
        (nsInfo.daemonMountNsInode == nsInfo.rootMountNsInode);
    isPrivhelperInRootMountNamespace =
        (nsInfo.privhelperMountNsInode == nsInfo.rootMountNsInode);
  } catch (const std::exception& ex) {
    // TODO: Remove this fallback once all privhelper daemons include the
    // getNamespaceInfo method.
    XLOGF(
        WARN,
        "Failed to get namespace info from privhelper (falling back to "
        "local detection): {}",
        ex.what());
    daemonMountNamespace = getNamespaceInode("/proc/self/ns/mnt");
    daemonPidNamespace = getNamespaceInode("/proc/self/ns/pid");
    isDaemonInRootMountNamespace = checkIsRootMountNamespace();
  }
#else
  std::optional<uint64_t> daemonMountNamespace;
  std::optional<uint64_t> daemonPidNamespace;
  std::optional<std::string> cgroupInfo;
  std::optional<bool> isDaemonInRootMountNamespace;
  std::optional<bool> isPrivhelperInRootMountNamespace;
  std::optional<uint64_t> privhelperMountNamespace;
  std::optional<uint64_t> privhelperPidNamespace;
#endif

  std::vector<std::string> originalCommandLine{argv, argv + argc};

  // Make sure to run this before any flag values are read.
  main.init(&argc, &argv);

  setEdenBugFatal(FLAGS_fatal_eden_bug);

  if (argc != 1) {
    fprintf(stderr, "error: unexpected trailing command line arguments\n");
    return kExitCodeUsage;
  }

  if (identity.getUid() == 0 && !FLAGS_allowRoot) {
    fprintf(
        stderr,
        "error: you appear to be running eden as root, "
        "rather than using\n"
        "sudo or a setuid binary.  This is normally undesirable.\n"
        "Pass in the --allowRoot flag if you really mean to run "
        "eden as root.\n");
    return kExitCodeUsage;
  }

  // Set the log level for the daemon.
  try {
    (void)folly::stringToLogLevel(FLAGS_edenLogLevel);
  } catch (const std::range_error& e) {
    fprintf(stderr, "error: selected log level is invalid: %s\n", e.what());
    return kExitCodeUsage;
  }
  auto loggingConfig = folly::parseLogConfig(
      fmt::format("eden={}; default:async=true", FLAGS_edenLogLevel));
  folly::LoggerDB::get().updateConfig(loggingConfig);

  // Temporary hack until client is migrated to supported channel
  THRIFT_FLAG_SET_MOCK(server_header_reject_framed, false);

  std::shared_ptr<EdenConfig> edenConfig;
  try {
    edenConfig = getEdenConfig(identity);
  } catch (const ArgumentError& ex) {
    fprintf(stderr, "%s\n", ex.what());
    return kExitCodeError;
  }

  main.prepareConfig(*edenConfig);

  auto startupStatusChannel = std::make_shared<StartupStatusChannel>();
  auto logPath = getLogPath(edenConfig->edenDir.getValue());
  bool disclaimTcc = edenConfig->disclaimTccResponsibility.getValue();
#ifdef __APPLE__
  const std::string signingTeam = selfCodeSigningTeamId();
  disclaimTcc =
      disclaimTcc && signingTeam == edenConfig->disclaimTccTeamId.getValue();
#endif
  auto startupLogger = daemonizeIfRequested(
      logPath,
      privHelper.get(),
      originalCommandLine,
      startupStatusChannel,
      disclaimTcc);
#ifdef __APPLE__
  // Logged after daemonizing rather than before: the daemonizing parent never
  // returns from daemonizeIfRequested(), and the daemon's stderr is redirected
  // into edenfs.log inside it, which is where this needs to be visible. A
  // user-run --foreground daemon has an empty logPath and spawned nothing, so
  // there is no skipped disclaim to report.
  const bool daemonDisclaimSkipped = !disclaimTcc &&
      edenConfig->disclaimTccResponsibility.getValue() && !logPath.empty();
  if (daemonDisclaimSkipped) {
    // A real certificate whose team differs (development cert, or a rotated
    // release team) silently loses the disclaim, so that is a WARN; "none" is
    // an ad-hoc buck build, which is expected.
    const auto message = fmt::format(
        "not disclaiming TCC responsibility for the daemon: code signature "
        "team {}, not the fleet team {}",
        signingTeam,
        edenConfig->disclaimTccTeamId.getValue());
    if (signingTeam == "none") {
      XLOG(INFO) << message;
    } else {
      XLOG(WARN) << message;
    }
  }
  // Scuba events for skips caused by a real but mismatched team, logged once
  // the server exists. The process that spawned the privhelper (process #1
  // when daemonizing) has no event logger, so the daemon reports that skip
  // too: it runs the same binary, so csops(2) gives the same team. Ad-hoc
  // builds ("none") are expected and not reported.
  std::vector<TccDisclaimSkipped> tccDisclaimSkips;
  if (signingTeam != "none") {
    if (daemonDisclaimSkipped) {
      tccDisclaimSkips.emplace_back(
          "daemon", signingTeam, edenConfig->disclaimTccTeamId.getValue());
    }
    if (!tccDisclaimKillswitchPresent() && signingTeam != kTccDisclaimTeamId) {
      tccDisclaimSkips.emplace_back(
          "privhelper", signingTeam, std::string{kTccDisclaimTeamId});
    }
  }
#endif
  std::optional<EdenServer> server;
  auto prepareFuture = folly::Future<folly::Unit>::makeEmpty();
  try {
    // If stderr was redirected to a log file, inform the privhelper
    // to make sure it logs to our current stderr.
    if (!logPath.empty()) {
      privHelper->setLogFileBlocking(
          folly::File(STDERR_FILENO, /*ownsFd=*/false));
    }

    // Since we are a daemon, and we don't ever want to be in a situation
    // where we hold any open descriptors through a fuse mount that points
    // to ourselves (which can happen during takeover), we chdir to `/`
    // to avoid having our cwd reference ourselves if the user runs
    // `eden daemon --takeover` from within an eden mount
    folly::checkPosixError(chdir("/"), "failed to chdir(/)");

    // Set some default glog settings, to be applied unless overridden on the
    // command line
    gflags::SetCommandLineOptionWithMode(
        "logtostderr", "1", gflags::SET_FLAGS_DEFAULT);
    gflags::SetCommandLineOptionWithMode(
        "minloglevel", "1", gflags::SET_FLAGS_DEFAULT);

    auto daemonPid = getpid();
    startupLogger->log(
        "Starting ",
        main.getEdenfsBuildName(),
        ", pid ",
        daemonPid,
        ", session_id ",
        getSessionId());

#ifdef __linux__
    const auto sigbusRetryBudget = edenConfig->sigbusRetryBudget.getValue();
    if (sigbusRetryBudget != 0) {
      if (const auto error = sigbus_install_handler()) {
        startupLogger->warn(
            "Failed to install SIGBUS retry handler: ", folly::errnoStr(error));
      } else {
        sigbus_set_retry_budget(sigbusRetryBudget);
        startupLogger->log(
            "Installed SIGBUS retry handler with a process-wide budget of ",
            sigbusRetryBudget);
      }
    }
#endif
#ifdef __APPLE__
    // Exported even when zero, so that a normal start is distinguishable from
    // a daemon too old to export the key. Platforms without privhelper-driven
    // restarts never reach here and leave the key absent.
    fb303::fbData->setCounter(
        "privhelper_restart_count",
        static_cast<int64_t>(numRestarts.value_or(0)));
    if (numRestarts.has_value()) {
      // States the evidence rather than the conclusion: edenfsctl forwards
      // every EDEN-prefixed variable, so a manual start can inherit this one.
      XLOGF(
          INFO,
          "started with a restart budget of {} already spent, which the privhelper sets when it relaunches edenfs",
          *numRestarts);
    }
#endif

    auto sessionInfo = makeSessionInfo(
        identity, main.getLocalHostname(), main.getEdenfsVersion());

    auto scribeLogger = main.getScribeLogger(sessionInfo, edenConfig);

    // Set the memory priority for the EdenFS daemon process. On macOS, this
    // modifies the process' Jetsam priority. On Linux, this modifies the value
    // of /proc/<pid>/oom_score_adj.
    auto daemonMemoryPriority =
        edenConfig->daemonTargetMemoryPriority.getValue();
    if (daemonMemoryPriority.has_value()) {
      privHelper->setMemoryPriorityForProcessBlocking(
          daemonPid, daemonMemoryPriority.value());
    }

    // Also protect the privhelper process when explicitly configured.
    const auto privhelperMemoryPriority =
        edenConfig->privHelperTargetMemoryPriority.getValue();
    if (privhelperMemoryPriority.has_value()) {
      auto privhelperPid = privHelper->getPid();
      if (privhelperPid > 0) {
        try {
          privHelper->setMemoryPriorityForProcessBlocking(
              privhelperPid, privhelperMemoryPriority.value());
        } catch (const std::exception& ex) {
          XLOGF(
              WARN,
              "Cannot set privhelper memory priority to {}: {}",
              privhelperMemoryPriority.value(),
              ex.what());
        }
      } else {
        XLOG(
            WARN,
            "Cannot set privhelper memory priority: privhelper pid unknown");
      }
    }

#ifdef __linux__
    XLOGF(
        DBG2,
        "Namespace detection: daemon(mnt={}, pid={}), privhelper(mnt={}, pid={}), "
        "rootMountNsInode={}, isDaemonInRootMountNamespace={}, "
        "isPrivhelperInRootMountNamespace={}",
        OPT_TO_STRING(daemonMountNamespace),
        OPT_TO_STRING(daemonPidNamespace),
        OPT_TO_STRING(privhelperMountNamespace),
        OPT_TO_STRING(privhelperPidNamespace),
        OPT_TO_STRING(rootMountNsInode),
        OPT_TO_STRING(isDaemonInRootMountNamespace),
        OPT_TO_STRING(isPrivhelperInRootMountNamespace));

    if (edenConfig->requireRootMountNamespace.getValue()) {
      // To avoid disruption, we assume that the daemon and privhelper are in
      // the root mount namespace unless we can prove otherwise.
      bool daemonInRootNs = isDaemonInRootMountNamespace.value_or(true);
      bool privhelperInRootNs = isPrivhelperInRootMountNamespace.value_or(true);

      if (!daemonInRootNs || !privhelperInRootNs) {
        auto errorMsg = fmt::format(
            "EdenFS cannot start: daemon in root mount namespace = {}, "
            "privhelper in root mount namespace = {}.\n To allow this "
            "behavior, set core.require-root-mount-namespace config to false",
            daemonInRootNs,
            privhelperInRootNs);
        XLOG(ERR, errorMsg);
        startupLogger->exitUnsuccessfully(kExitCodeError, errorMsg);
      }
    }
#endif // __linux__

#ifdef EDEN_HAVE_SYSTEMD
    auto systemdStartupTimeoutExtension =
        edenConfig->systemdStartupTimeoutExtension.getValue();
#endif

    server.emplace(
        std::move(originalCommandLine),
        std::move(identity),
        getGlobalEdenStats(),
        std::move(sessionInfo),
        std::move(privHelper),
        std::move(edenConfig),
        main.getActivityRecorderFactory(),
        main.getBackingStoreFactory(),
        std::move(scribeLogger),
        std::move(startupStatusChannel),
        main.getEdenfsVersion());

#ifdef __APPLE__
    for (const auto& skip : tccDisclaimSkips) {
      server->getServerState()->getEdenFsEventsLogger()->logEvent(skip);
    }
#endif

#ifdef EDEN_HAVE_SYSTEMD
    if (systemdStartupTimeoutExtension > std::chrono::nanoseconds::zero()) {
      auto startupTimeoutExtender =
          std::make_shared<SystemdStartupTimeoutExtender>(
              systemdStartupTimeoutExtension);
      server->setStartupProgressCallback(
          [startupTimeoutExtender](std::string_view status) {
            startupTimeoutExtender->reportProgress(status);
          });
    }
#endif

    main.prepare(server.value());

    prepareFuture = server->prepare(startupLogger);
  } catch (const std::exception& ex) {
    auto startTimeInSeconds =
        std::chrono::duration<double>{daemonStart.elapsed()}.count();
    if (server) {
      server->getServerState()->getEdenFsEventsLogger()->logEvent(
          DaemonStart{
              startTimeInSeconds,
              FLAGS_takeover,
              false /*success*/,
              daemonMountNamespace,
              daemonPidNamespace,
              privhelperMountNamespace,
              privhelperPidNamespace,
              isDaemonInRootMountNamespace,
              isPrivhelperInRootMountNamespace,
              cgroupInfo,
              numRestarts});
    }
    startupLogger->exitUnsuccessfully(
        kExitCodeError, "error starting EdenFS: ", folly::exceptionStr(ex));
  }

  std::move(prepareFuture)
      .thenTry([startupLogger, daemonStart, &server](
                   folly::Try<folly::Unit>&& result) {
        // If an error occurred this means that we failed to mount all of
        // the mount points.
        //
        // Mount errors are fine. We have still started and will
        // continue running, so we can report successful startup.
        if (result.hasException()) {
          // Log an overall error message here.
          // We will have already logged more detailed messages for each
          // mount failure when it occurred.
          startupLogger->warn(
              "did not successfully remount all repositories: ",
              result.exception().what());
        }
        auto startTimeInSeconds =
            std::chrono::duration<double>{daemonStart.elapsed()}.count();
#ifdef EDEN_HAVE_SYSTEMD
        auto sdNotifyMsg =
            fmt::format("MAINPID={}\nREADY=1\nSTATUS=", getpid());
        auto sdNotifyResult = sd_notify(0, sdNotifyMsg.c_str());
        if (sdNotifyResult > 0) {
          XLOGF(INFO, "notified systemd: MAINPID={}", getpid());
        } else if (sdNotifyResult == 0) {
          XLOG(INFO, "sd_notify: NOTIFY_SOCKET not set");
        } else {
          XLOGF(ERR, "sd_notify failed: {}", folly::errnoStr(-sdNotifyResult));
        }
#endif
        startupLogger->success(startTimeInSeconds);

        // Deliberately after startup has succeeded. A daemon that dies before
        // this point leaves the privhelper with no restart information at all,
        // which is what makes a boot-crash loop structurally impossible.
        server->armPrivHelperRestart();
      })
      .ensure([daemonStart,
               edenFsEventsLogger =
                   server->getServerState()->getEdenFsEventsLogger(),
               takeover = FLAGS_takeover,
               daemonMountNamespace,
               daemonPidNamespace,
               privhelperMountNamespace,
               privhelperPidNamespace,
               isDaemonInRootMountNamespace,
               isPrivhelperInRootMountNamespace,
               cgroupInfo,
               numRestarts,
               &server] {
        // This value is slightly different from `startTimeInSeconds`
        // we pass into `startupLogger->success()`, but should be
        // identical.
        auto startTimeInSeconds =
            std::chrono::duration<double>{daemonStart.elapsed()}.count();
        // Here we log a success even if we did not successfully remount
        // all repositories (if prepareFuture had an exception). In the
        // future it would be helpful to log number of successful vs
        // unsuccessful remounts
        edenFsEventsLogger->logEvent(
            DaemonStart{
                startTimeInSeconds,
                takeover,
                true /*success*/,
                daemonMountNamespace,
                daemonPidNamespace,
                privhelperMountNamespace,
                privhelperPidNamespace,
                isDaemonInRootMountNamespace,
                isPrivhelperInRootMountNamespace,
                cgroupInfo,
                numRestarts});

#ifndef _WIN32
        // Check for previous heartbeat files and handle crash detection
        server->checkForPreviousHeartbeat(takeover);

        // Create a new heartbeat file
        server->createOrUpdateEdenHeartbeatFile();
#else
        // On Windows, EdenFS does not create a heartbeat file.
        (void)server;
#endif
      });

  while (true) {
    main.runServer(server.value());
    if (server->performCleanup()) {
      break;
    }
    // performCleanup() returns false if a takeover shutdown attempt
    // failed.  Continue and re-run the server in this case.
  }

  main.cleanup();

#ifndef _WIN32
  // Remove the heartbeat file for a clean shutdown
  server->removeEdenHeartbeatFile();
#endif

  XLOG(INFO, "EdenFS exiting successfully");
  return kExitCodeSuccess;
}

} // namespace facebook::eden
