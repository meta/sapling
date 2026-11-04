/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

/* The code in this test is a little hard to follow.
 * Here's a quick primer!
 *
 * The StartupLogger class encapsulates a channel between a parent
 * and child process pair that is used in the main project to allow
 * the parent to daemonize its child, but for the parent to linger
 * long enough to report the status of the child initialization.
 *
 * It works by spawning a new copy of itself and passing some command
 * line arguments to allow the child to realize that it is should
 * report back to its parent.
 *
 * This test verifies the behavior of the channel between the two
 * processes and thus needs to be able to spawn a copy of itself.
 *
 * Because we want the behavior of the spawned child to vary based
 * on the test we have a custom `main()` function that will look
 * at any command line arguments that spill over from the
 * gflags-registered arguments; the convention is that the first
 * argument names a function that will be called in a child process,
 * and that a second optional argument can be passed down to it.
 */

#include "eden/fs/service/StartupLogger.h"
#include "eden/common/telemetry/SessionId.h"

#include <folly/File.h>
#include <folly/FileUtil.h>
#include <folly/init/Init.h>
#include <folly/logging/xlog.h>
#include <folly/testing/TestUtil.h>
#include <gmock/gmock.h>
#include <gtest/gtest.h>
#include <sysexits.h>
#include <cerrno>
#include <chrono>
#include <csignal>
#include <cstdio>
#include <cstdlib>
#include <optional>
#include <string>
#include <thread>
#include "eden/common/testharness/ScopedEnvVar.h"
#include "eden/common/utils/FileUtils.h"
#include "eden/common/utils/SpawnedProcess.h"
#include "eden/fs/privhelper/PrivHelper.h"
#include "eden/fs/service/StartupStatusSubscriber.h"

using namespace facebook::eden;
using namespace std::chrono_literals;
using folly::StringPiece;
using folly::test::TemporaryFile;
using std::string;
using testing::ContainsRegex;
using testing::HasSubstr;
using testing::Not;
using namespace folly::string_piece_literals;

namespace facebook::eden {

namespace {

// Copy the original command line arguments for use with
// the StartupLogger::spawn method
std::vector<std::string> originalCommandLine;

struct FunctionResult {
  std::string standardOutput;
  std::string standardError;
  ProcessStatus returnCode;
};

FunctionResult runFunctionInSeparateProcess(folly::StringPiece functionName);
FunctionResult runFunctionInSeparateProcess(
    folly::StringPiece functionName,
    std::vector<std::string> arguments);
bool isReadablePipeBroken(FileDescriptor& fd);
bool isWritablePipeBroken(FileDescriptor& fd);
bool fileExists(folly::fs::path);

class StartupLoggerTestBase : public ::testing::Test {
 protected:
  AbsolutePath logPath() {
    return canonicalPath(logFile_.path().string());
  }
  string readLogContents() {
    return readFile(logPath()).value();
  }

  TemporaryFile logFile_{"eden_test_log"};
};
} // namespace

class DaemonStartupLoggerTest : public StartupLoggerTestBase {
 protected:
  // Wrappers simply to allow our tests to access private DaemonStartupLogger
  // methods
  FileDescriptor createPipe(DaemonStartupLogger& logger) {
    Pipe pipe;
    logger.pipe_ = std::move(pipe.write);
    return std::move(pipe.read);
  }
  void closePipe(DaemonStartupLogger& logger) {
    logger.pipe_.close();
  }
  DaemonStartupLogger::ParentResult waitForChildStatus(
      DaemonStartupLogger& logger,
      FileDescriptor& readPipe,
      SpawnedProcess& childProc,
      StringPiece logPath,
      std::optional<std::chrono::milliseconds> timeout = std::nullopt) {
    return logger.waitForChildStatus(readPipe, childProc, logPath, timeout);
  }

  std::optional<std::chrono::milliseconds> startupTimeout() {
    return DaemonStartupLogger::startupTimeout();
  }

  DaemonStartupLogger::ParentResult spawnInChild(folly::StringPiece name) {
    auto startupStatusChannel = std::make_shared<StartupStatusChannel>();
    DaemonStartupLogger logger{startupStatusChannel};
    auto args = originalCommandLine;
    args.push_back(name.str());
    args.push_back(logPath().asString());
    auto child = logger.spawnImpl(
        logPath().view(), nullptr, args, /*disclaimTccResponsibility=*/true);
    auto result = logger.waitForChildStatus(
        child.exitStatusPipe, child.process, logPath().view());
    child.process.kill();
    child.process.wait();
    return result;
  }

  std::pair<DaemonStartupLogger::ParentResult, ProcessStatus>
  timeOutChildDuringStartup() {
    auto startupStatusChannel = std::make_shared<StartupStatusChannel>();
    DaemonStartupLogger logger{startupStatusChannel};
    auto args = originalCommandLine;
    args.emplace_back("hangDuringStartup");
    args.push_back(logPath().asString());
    auto child = logger.spawnImpl(
        logPath().view(), nullptr, args, /*disclaimTccResponsibility=*/true);
    auto result = logger.waitForChildStatus(
        child.exitStatusPipe, child.process, logPath().view(), 0ms);
    return {std::move(result), child.process.wait()};
  }
};

namespace {
void crashWithNoResult(const std::string& logPath) {
  auto startupStatusChannel = std::make_shared<StartupStatusChannel>();
  DaemonStartupLogger logger{startupStatusChannel};
  logger.initClient(
      logPath,
      FileDescriptor(FLAGS_startupLoggerFd, FileDescriptor::FDType::Pipe));
  fprintf(stderr, "this message should go to the log\n");
  fflush(stderr);
  kill(getpid(), SIGKILL);
  // Wait until we get killed.
  while (true) {
    /* sleep override */ std::this_thread::sleep_for(30s);
  }
}

TEST_F(DaemonStartupLoggerTest, crashWithNoResult) {
  auto result = spawnInChild("crashWithNoResult");

  EXPECT_EQ(EX_SOFTWARE, result.exitCode);
  EXPECT_EQ(
      fmt::format(
          "error: EdenFS crashed with status killed by signal {} "
          "before it finished initializing\n"
          "Check the EdenFS log file at {} for more details",
          SIGKILL,
          logPath()),
      result.errorMessage);

  // Verify that the log message from the child went to the log file
  EXPECT_EQ("this message should go to the log\n", readLogContents());
}

TEST_F(DaemonStartupLoggerTest, successWritesStartedMessageToStandardError) {
  auto result = runFunctionInSeparateProcess(
      "successWritesStartedMessageToStandardErrorDaemonChild");
  EXPECT_THAT(
      result.standardError,
      ContainsRegex("Started EdenFS \\(pid [0-9]+, session_id [0-9]+\\)"));
  EXPECT_THAT(result.standardError, HasSubstr("Logs available at "));
}

void successWritesStartedMessageToStandardErrorDaemonChild() {
  auto logFile = TemporaryFile{"eden_test_log"};
  auto startupStatusChannel = std::make_shared<StartupStatusChannel>();
  auto logger = daemonizeIfRequested(
      logFile.path().string(),
      nullptr,
      originalCommandLine,
      startupStatusChannel,
      /*disclaimTccResponsibility=*/true);
  logger->success(17);
  exit(0);
}

TEST_F(
    DaemonStartupLoggerTest,
    programExitsUnsuccessfullyIfLogFileIsInaccessible) {
  auto result = runFunctionInSeparateProcess(
      "programExitsUnsuccessfullyIfLogFileIsInaccessibleChild");
  EXPECT_THAT(
      result.standardError,
      ContainsRegex("error opening log file .*/file\\.txt"));
  EXPECT_THAT(result.standardError, HasSubstr("Not a directory"));
  EXPECT_EQ(
      folly::to<std::string>("exited with status ", EX_IOERR),
      result.returnCode.str());
}

void programExitsUnsuccessfullyIfLogFileIsInaccessibleChild() {
  auto logFile = TemporaryFile{"eden_test_log"};
  auto badLogFilePath = logFile.path() / "file.txt";
  auto startupStatusChannel = std::make_shared<StartupStatusChannel>();
  auto logger = daemonizeIfRequested(
      badLogFilePath.string(),
      nullptr,
      originalCommandLine,
      startupStatusChannel,
      /*disclaimTccResponsibility=*/true);
  logger->success(19);
  exit(0);
}

TEST_F(DaemonStartupLoggerTest, sigHupReportsReopenFailure) {
  auto tempDir = folly::test::TemporaryDirectory();
  auto logPath = (tempDir.path() / "logs" / "edenfs.log").string();
  auto result = runFunctionInSeparateProcess(
      "sigHupReportsReopenFailureChild", std::vector<std::string>{logPath});
  // FIXME(T291170716): stderr should get the whole line, "Failed to reopen
  // <logPath>: No such file or directory\n". Only "Failed to reopen " arrives
  // because the rest is written to the failed descriptor -1. The next diff
  // fixes the handler and expects the whole line here.
  EXPECT_THAT(result.standardError, HasSubstr("Failed to reopen "));
  EXPECT_THAT(result.standardError, Not(HasSubstr(logPath)));
}

[[noreturn]] void sigHupReportsReopenFailureChild(const std::string& logPath) {
  auto logDir = folly::fs::path(logPath).parent_path();
  folly::fs::create_directories(logDir);

  // A running daemon's stderr is the log file itself; the test points it back
  // at the real stderr so the handler's report reaches the parent.
  folly::File savedStderr{::dup(STDERR_FILENO), /*ownsFd=*/true};

  Pipe statusPipe;
  auto startupStatusChannel = std::make_shared<StartupStatusChannel>();
  DaemonStartupLogger logger{startupStatusChannel};
  // Redirects stdout and stderr to logPath and installs the SIGHUP handler.
  logger.initClient(logPath, std::move(statusPipe.write));

  ::dup2(savedStderr.fd(), STDERR_FILENO);
  ::dup2(savedStderr.fd(), STDOUT_FILENO);

  // Make reopening the log impossible: the file and its directory are gone.
  folly::fs::remove(folly::fs::path(logPath));
  folly::fs::remove(logDir);

  ::raise(SIGHUP);
  std::exit(0);
}

void exitWithNoResult(const std::string& logPath) {
  auto startupPublishers = std::make_shared<StartupStatusChannel>();
  DaemonStartupLogger logger{startupPublishers};
  logger.initClient(
      logPath,
      FileDescriptor(FLAGS_startupLoggerFd, FileDescriptor::FDType::Pipe));
  _exit(19);
}

TEST_F(DaemonStartupLoggerTest, exitWithNoResult) {
  // Fork a child that exits unsuccessfully
  auto result = spawnInChild("exitWithNoResult");

  EXPECT_EQ(19, result.exitCode);
  EXPECT_EQ(
      fmt::format(
          "error: EdenFS exited with status 19 before it finished initializing\n"
          "Check the EdenFS log file at {} for more details",
          logPath()),
      result.errorMessage);
}

void exitSuccessfullyWithNoResult(const std::string& logPath) {
  auto startupStatusChannel = std::make_shared<StartupStatusChannel>();
  DaemonStartupLogger logger{startupStatusChannel};
  logger.initClient(
      logPath,
      FileDescriptor(FLAGS_startupLoggerFd, FileDescriptor::FDType::Pipe));
  _exit(0);
}

TEST_F(DaemonStartupLoggerTest, exitSuccessfullyWithNoResult) {
  // Fork a child that exits successfully
  auto result = spawnInChild("exitSuccessfullyWithNoResult");

  // The parent process should be EX_SOFTWARE in this case
  EXPECT_EQ(EX_SOFTWARE, result.exitCode);
  EXPECT_EQ(
      fmt::format(
          "error: EdenFS exited with status 0 before it finished initializing\n"
          "Check the EdenFS log file at {} for more details",
          logPath()),
      result.errorMessage);
}

void destroyLoggerWhileDaemonIsStillRunning(const std::string& logPath) {
  auto startupStatusChannel = std::make_shared<StartupStatusChannel>();
  DaemonStartupLogger logger{startupStatusChannel};
  logger.initClient(
      logPath,
      FileDescriptor(FLAGS_startupLoggerFd, FileDescriptor::FDType::Pipe));

  // Destroy the DaemonStartupLogger object to force it to close its pipes
  // without sending a result.
  std::optional<DaemonStartupLogger> optLogger(std::move(logger));
  optLogger.reset();

  /* sleep override */ std::this_thread::sleep_for(30s);
}

void hangDuringStartup(const std::string&) {
  while (true) {
    pause();
  }
}

TEST_F(DaemonStartupLoggerTest, aUserStartedDaemonWaitsIndefinitely) {
  ScopedEnvVar restartCount{kEdenFsRestartCountEnv};
  restartCount.unset();

  EXPECT_EQ(std::nullopt, startupTimeout());
}

TEST_F(DaemonStartupLoggerTest, aRelaunchedDaemonWaitsForABoundedTime) {
  ScopedEnvVar restartCount{kEdenFsRestartCountEnv};
  restartCount.set("1");

  EXPECT_EQ(
      std::chrono::milliseconds{kRelaunchStartupTimeout}, startupTimeout());
}

TEST_F(DaemonStartupLoggerTest, startupTimeoutTerminatesAndReapsChild) {
  const auto [result, childStatus] = timeOutChildDuringStartup();

  EXPECT_EQ(EX_SOFTWARE, result.exitCode);
  EXPECT_THAT(
      result.errorMessage,
      HasSubstr("EdenFS did not finish initializing within 0 seconds"));
  EXPECT_NE(0, childStatus.killSignal());
}

TEST_F(DaemonStartupLoggerTest, destroyLoggerWhileDaemonIsStillRunning) {
  auto result = spawnInChild("destroyLoggerWhileDaemonIsStillRunning");

  EXPECT_EQ(EX_SOFTWARE, result.exitCode);
  EXPECT_EQ(
      fmt::format(
          "error: EdenFS is still running but "
          "did not report its initialization status\n"
          "Check the EdenFS log file at {} for more details",
          logPath()),
      result.errorMessage);
}

TEST_F(DaemonStartupLoggerTest, closePipeWithWaitError) {
  // Call waitForChildStatus() with our own pid.
  // wait() will return an error trying to wait on ourself.
  auto startupStatusChannel = std::make_shared<StartupStatusChannel>();
  DaemonStartupLogger logger{startupStatusChannel};
  auto readPipe = createPipe(logger);
  closePipe(logger);
  auto selfProc = SpawnedProcess::fromExistingProcess(getpid());
  auto result =
      waitForChildStatus(logger, readPipe, selfProc, "/var/log/edenfs.log");

  EXPECT_EQ(EX_SOFTWARE, result.exitCode);
  EXPECT_EQ(
      "error: EdenFS exited with status 0 before it finished initializing\n"
      "Check the EdenFS log file at /var/log/edenfs.log for more details",
      result.errorMessage);
}

void success(const std::string& logPath) {
  auto startupStatusChannel = std::make_shared<StartupStatusChannel>();
  DaemonStartupLogger logger{startupStatusChannel};
  logger.initClient(
      logPath,
      FileDescriptor(FLAGS_startupLoggerFd, FileDescriptor::FDType::Pipe));
  logger.success(23);
}

TEST_F(DaemonStartupLoggerTest, success) {
  auto result = spawnInChild("success");
  EXPECT_EQ(0, result.exitCode);
  EXPECT_EQ("", result.errorMessage);
}

void failure(const std::string& logPath) {
  auto startupStatusChannel = std::make_shared<StartupStatusChannel>();
  DaemonStartupLogger logger{startupStatusChannel};
  logger.initClient(
      logPath,
      FileDescriptor(FLAGS_startupLoggerFd, FileDescriptor::FDType::Pipe));
  logger.exitUnsuccessfully(3, "example failure for tests");
}

TEST_F(DaemonStartupLoggerTest, failure) {
  auto result = spawnInChild("failure");
  EXPECT_EQ(3, result.exitCode);
  EXPECT_EQ("", result.errorMessage);
  EXPECT_THAT(readLogContents(), HasSubstr("example failure for tests"));
}

TEST_F(DaemonStartupLoggerTest, daemonClosesStandardFileDescriptors) {
  SpawnedProcess::Options opts;
  opts.pipeStdin();
  opts.pipeStdout();
  opts.pipeStderr();
  auto process = SpawnedProcess{
      {{
          executablePath().asString(),
          "daemonClosesStandardFileDescriptorsChild",
      }},
      std::move(opts)};

  auto stdinFd = process.stdinFd();
  auto stdoutFd = process.stdoutFd();
  auto stderrFd = process.stderrFd();
  SCOPE_EXIT {
    process.wait();
  };
  stdinFd.setNonBlock();
  stdoutFd.setNonBlock();
  stderrFd.setNonBlock();

  // FIXME(strager): wait() could technically deadlock if the child is blocked
  // on writing to stdout or stderr.
  auto returnCode = process.waitTimeout(40s);
  EXPECT_EQ("exited with status 0", returnCode.str());

  auto expectReadablePipeIsBroken = [](FileDescriptor& fd,
                                       folly::StringPiece name) {
    EXPECT_TRUE(isReadablePipeBroken(fd))
        << "Daemon should have closed its " << name
        << " file descriptor (parent fd " << fd.systemHandle()
        << "), but it did not.";
  };
  auto expectWritablePipeIsBroken = [](FileDescriptor& fd,
                                       folly::StringPiece name) {
    EXPECT_TRUE(isWritablePipeBroken(fd))
        << "Daemon should have closed its " << name
        << " file descriptor (parent fd " << fd.systemHandle()
        << "), but it did not.";
  };

  expectWritablePipeIsBroken(stdinFd, "stdin");
  expectReadablePipeIsBroken(stdoutFd, "stdout");
  expectReadablePipeIsBroken(stderrFd, "stderr");

  // NOTE(strager): The daemon process should eventually exit automatically, so
  // we don't need to explicitly kill it.
}

void daemonClosesStandardFileDescriptorsChild() {
  auto logFile = TemporaryFile{"eden_test_log"};
  auto startupStatusChannel = std::make_shared<StartupStatusChannel>();
  auto logger = daemonizeIfRequested(
      logFile.path().string(),
      nullptr,
      originalCommandLine,
      startupStatusChannel,
      /*disclaimTccResponsibility=*/true);
  logger->success(29);
  std::this_thread::sleep_for(30s);
  exit(1);
}

TEST(ForegroundStartupLoggerTest, loggedMessagesAreWrittenToStandardError) {
  auto result = runFunctionInSeparateProcess(
      "loggedMessagesAreWrittenToStandardErrorChild");
  EXPECT_THAT(result.standardOutput, Not(HasSubstr("warn message")));
  EXPECT_THAT(result.standardError, HasSubstr("warn message"));
}

void loggedMessagesAreWrittenToStandardErrorChild() {
  auto startupStatusChannel = std::make_shared<StartupStatusChannel>();
  auto logger = ForegroundStartupLogger{startupStatusChannel};
  logger.warn("warn message");
}

TEST(ForegroundStartupLoggerTest, exitUnsuccessfullyMakesProcessExitWithCode) {
  auto result = runFunctionInSeparateProcess(
      "exitUnsuccessfullyMakesProcessExitWithCodeChild");
  EXPECT_EQ("exited with status 42", result.returnCode.str());
}

void exitUnsuccessfullyMakesProcessExitWithCodeChild() {
  auto startupStatusChannel = std::make_shared<StartupStatusChannel>();
  auto logger = ForegroundStartupLogger{startupStatusChannel};
  logger.exitUnsuccessfully(42, "intentionally exiting");
}

TEST(ForegroundStartupLoggerTest, xlogsAfterSuccessAreWrittenToStandardError) {
  auto result = runFunctionInSeparateProcess(
      "xlogsAfterSuccessAreWrittenToStandardErrorChild");
  EXPECT_THAT(result.standardError, HasSubstr("test error message with xlog"));
}

void xlogsAfterSuccessAreWrittenToStandardErrorChild() {
  auto startupStatusChannel = std::make_shared<StartupStatusChannel>();
  auto logger = ForegroundStartupLogger{startupStatusChannel};
  logger.success(31);
  XLOG(ERR, "test error message with xlog");
}

TEST(ForegroundStartupLoggerTest, successWritesStartedMessageToStandardError) {
  auto result = runFunctionInSeparateProcess(
      "successWritesStartedMessageToStandardErrorForegroundChild");
  EXPECT_THAT(
      result.standardError,
      ContainsRegex(
          "Started EdenFS \\(pid [0-9]+, session_id [0-9]+\\) in [0-9]+s$\n"));
}

void successWritesStartedMessageToStandardErrorForegroundChild() {
  auto startupStatusChannel = std::make_shared<StartupStatusChannel>();
  auto logger = ForegroundStartupLogger{startupStatusChannel};
  logger.success(37);
}

class FileStartupLoggerTest : public StartupLoggerTestBase {};

TEST_F(FileStartupLoggerTest, loggerCreatesFileIfMissing) {
  auto tempDir = folly::test::TemporaryDirectory();
  auto logPath = tempDir.path() / "startup.log";
  ASSERT_FALSE(fileExists(logPath));
  auto startupStatusChannel = std::make_shared<StartupStatusChannel>();
  auto logger = FileStartupLogger{logPath.string(), startupStatusChannel};
  EXPECT_TRUE(fileExists(logPath));
}

TEST_F(FileStartupLoggerTest, loggingWritesMessagesToFile) {
  auto startupStatusChannel = std::make_shared<StartupStatusChannel>();
  auto logger = FileStartupLogger{logPath().view(), startupStatusChannel};
  logger.log("hello world");
  logger.warn("warning message");
  EXPECT_EQ("hello world\nwarning message\n", readLogContents());
}

TEST_F(FileStartupLoggerTest, loggingAppendsToFileIfItAlreadyExists) {
  writeFile(logPath(), "existing line\n"_sp).throwUnlessValue();
  auto startupStatusChannel = std::make_shared<StartupStatusChannel>();
  auto logger = FileStartupLogger{logPath().view(), startupStatusChannel};
  logger.log("new line");
  EXPECT_EQ("existing line\nnew line\n", readLogContents());
}

TEST_F(FileStartupLoggerTest, successWritesMessageToFile) {
  auto startupStatusChannel = std::make_shared<StartupStatusChannel>();
  auto logger = FileStartupLogger{logPath().view(), startupStatusChannel};
  logger.success(41);
  EXPECT_EQ(
      folly::to<std::string>(
          "Started EdenFS (pid ",
          getpid(),
          ", session_id ",
          getSessionId(),
          ") in 41s\n"),
      readLogContents());
}

TEST_F(FileStartupLoggerTest, exitUnsuccessfullyWritesMessageAndKillsProcess) {
  auto result = runFunctionInSeparateProcess(
      "exitUnsuccessfullyWritesMessageAndKillsProcessChild",
      std::vector<std::string>{logPath().value()});
  EXPECT_EQ("exited with status 3", result.returnCode.str());
  EXPECT_EQ("error message\n", readLogContents());
}

void exitUnsuccessfullyWritesMessageAndKillsProcessChild(std::string logPath) {
  auto startupStatusChannel = std::make_shared<StartupStatusChannel>();
  auto logger = FileStartupLogger{logPath, startupStatusChannel};
  logger.exitUnsuccessfully(3, "error message");
}

FunctionResult runFunctionInSeparateProcess(folly::StringPiece functionName) {
  return runFunctionInSeparateProcess(functionName, std::vector<std::string>{});
}

FunctionResult runFunctionInSeparateProcess(
    folly::StringPiece functionName,
    std::vector<std::string> arguments) {
  auto execPath = executablePath();
  auto command = std::vector<std::string>{
      execPath.asString(),
      functionName.str(),
  };
  command.insert(command.end(), arguments.begin(), arguments.end());

  SpawnedProcess::Options opts;
  opts.pipeStdout();
  opts.pipeStderr();
  auto process = SpawnedProcess{command, std::move(opts)};
  SCOPE_FAIL {
    process.stdinFd();
    process.wait();
  };
  auto [out, err] = process.communicate();
  auto returnCode = process.wait();
  return FunctionResult{out, err, returnCode};
}

// This function implements a basic lookup "table" that can call
// a function defined in this file.
[[noreturn]] void runFunctionInCurrentProcess(
    folly::StringPiece functionName,
    std::vector<std::string> arguments) {
  auto checkFunction = [&](folly::StringPiece name, auto function) {
    if (functionName == name) {
      if constexpr (std::is_invocable_v<decltype(function)>) {
        function();
      } else if constexpr (std::is_invocable_v<
                               decltype(function),
                               std::string>) {
        auto argument = std::string{arguments.at(0)};
        function(std::move(argument));
      } else {
        XLOG(FATAL, "Unsupported function type");
      }
      std::exit(0);
    }
  };
  // XCHECK_FUNCTION defines a lookup table entry
#define XCHECK_FUNCTION(name) checkFunction(#name, name)
  XCHECK_FUNCTION(daemonClosesStandardFileDescriptorsChild);
  XCHECK_FUNCTION(exitUnsuccessfullyMakesProcessExitWithCodeChild);
  XCHECK_FUNCTION(exitUnsuccessfullyWritesMessageAndKillsProcessChild);
  XCHECK_FUNCTION(loggedMessagesAreWrittenToStandardErrorChild);
  XCHECK_FUNCTION(programExitsUnsuccessfullyIfLogFileIsInaccessibleChild);
  XCHECK_FUNCTION(sigHupReportsReopenFailureChild);
  XCHECK_FUNCTION(successWritesStartedMessageToStandardErrorDaemonChild);
  XCHECK_FUNCTION(successWritesStartedMessageToStandardErrorForegroundChild);
  XCHECK_FUNCTION(xlogsAfterSuccessAreWrittenToStandardErrorChild);
  XCHECK_FUNCTION(crashWithNoResult);
  XCHECK_FUNCTION(exitWithNoResult);
  XCHECK_FUNCTION(exitSuccessfullyWithNoResult);
  XCHECK_FUNCTION(destroyLoggerWhileDaemonIsStillRunning);
  XCHECK_FUNCTION(hangDuringStartup);
  XCHECK_FUNCTION(success);
  XCHECK_FUNCTION(failure);
#undef XCHECK_FUNCTION
  std::fprintf(
      stderr,
      "error: unknown function: %s\n",
      std::string{functionName}.c_str());
  std::exit(2);
}

bool isReadablePipeBroken(FileDescriptor& fd) {
  while (true) {
    char buffer[PIPE_BUF];
    auto result = fd.readNoInt(buffer, sizeof(buffer));
    result.throwUnlessValue();
    if (result.value() == 0) {
      return true;
    }
  }
}

bool isWritablePipeBroken(FileDescriptor& fd) {
  const char buffer[1] = {0};
  auto result = fd.writeNoInt(buffer, sizeof(buffer));
  if (auto exc = result.tryGetExceptionObject<std::system_error>()) {
    return exc->code() == std::error_code(EPIPE, std::generic_category());
  }
  result.throwUnlessValue();
  return false;
}

bool fileExists(folly::fs::path path) {
  auto status = folly::fs::status(path);
  return folly::fs::exists(status) && folly::fs::is_regular_file(status);
}
} // namespace
} // namespace facebook::eden

int main(int argc, char** argv) {
  originalCommandLine = {argv, argv + argc};
  ::testing::InitGoogleTest(&argc, argv);
  auto removeFlags = true;
  auto initGuard = folly::Init(&argc, &argv, removeFlags);
  // If we arguments left over then they are (probably) generated by
  // calls to DaemonStartupLoggerTest::spawnInChild that need to
  // be mapped back to functions defined in this module.
  if (argc >= 2) {
    auto functionName = folly::StringPiece{argv[1]};
    auto arguments = std::vector<std::string>{&argv[2], &argv[argc]};
    runFunctionInCurrentProcess(functionName, std::move(arguments));
  }
  return RUN_ALL_TESTS();
}
