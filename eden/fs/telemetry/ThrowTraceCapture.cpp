/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include "eden/fs/telemetry/ThrowTraceCapture.h"

#include <atomic>
#include <cstdlib>
#include <mutex>

#include <folly/futures/Future.h>
#include <folly/logging/xlog.h>

#ifdef __APPLE__
#include "folly/debugging/exception_tracer/ExceptionTracerLib.h"
#endif
#ifdef _WIN32
#include <windows.h>
#endif

#include "eden/fs/rust/backtrace_ffi/src/lib.rs.h"

namespace {
struct SkippedType {
  const std::type_info* type{nullptr};
  bool (*isExpected)(const void*){nullptr};
};

// Registered during startup, read on every throw; small enough to scan.
// Slots are written under the mutex and published through the count, so
// the throw path reads them without a lock.
constexpr size_t kMaxSkippedTypes = 4;
struct SkipRegistry {
  SkippedType types[kMaxSkippedTypes];
  std::atomic<size_t> count{0};
  std::mutex registerMutex;
};

SkipRegistry& skipRegistry() {
  static SkipRegistry registry;
  return registry;
}

std::atomic<bool> captureEnabled{false};

bool isExpectedThrow(const void* exception, const std::type_info* type) {
  if (!exception || !type) {
    return false;
  }
  auto& registry = skipRegistry();
  size_t n = registry.count.load(std::memory_order_acquire);
  for (size_t i = 0; i < n; ++i) {
    if (*registry.types[i].type == *type) {
      return registry.types[i].isExpected(exception);
    }
  }
  return false;
}

void onThrow(const void* exception, const std::type_info* type) {
  if (!captureEnabled.load(std::memory_order_relaxed)) {
    return;
  }
  if (isExpectedThrow(exception, type)) {
    // Leave no trace behind: a later error must not report this throw's
    // predecessor as its own throw site.
    facebook::eden::clear_captured_trace();
    return;
  }
  constexpr size_t kMaxStackDepth = 64;
  facebook::eden::capture_backtrace(kMaxStackDepth);
}
} // namespace

namespace facebook::eden {
void skipThrowTraceFor(
    const std::type_info& type,
    bool (*isExpected)(const void* exception)) {
  auto& registry = skipRegistry();
  std::lock_guard<std::mutex> guard{registry.registerMutex};
  size_t n = registry.count.load(std::memory_order_relaxed);
  if (n >= kMaxSkippedTypes) {
    XLOGF(
        DFATAL,
        "cannot skip throw traces for {}: all {} registry slots are in use",
        type.name(),
        kMaxSkippedTypes);
    return;
  }
  registry.types[n] = SkippedType{&type, isExpected};
  registry.count.store(n + 1, std::memory_order_release);
}

void setThrowTraceCaptureEnabled(bool enabled) {
  captureEnabled.store(enabled, std::memory_order_relaxed);
}
} // namespace facebook::eden

namespace {
// Timed waits, such as Scribe's config fetcher, throw FutureTimeout in normal
// operation, and nothing reports its throw-site trace.
const bool kFutureTimeoutTracesSkipped = [] {
  facebook::eden::skipThrowTraceFor(
      typeid(folly::FutureTimeout), [](const void*) { return true; });
  return true;
}();
} // namespace

#if defined(__linux__)
// Linux throw hook using the GNU linker's --wrap mechanism.
//
// When -Wl,--wrap=__cxa_throw is passed (via exported_linker_flags in BUCK),
// the linker redirects all calls to __cxa_throw to __wrap___cxa_throw, and
// makes the original available as __real___cxa_throw. This lets us intercept
// every C++ throw to capture a backtrace before the stack unwinds.
//
// Flow: throw expr → __wrap___cxa_throw → onThrow() → capture_backtrace()
//       → __real___cxa_throw (original, performs the actual throw)

extern "C" {
void __real___cxa_throw(void*, void*, void (*)(void*))
    __attribute__((__noreturn__));

__attribute__((__noreturn__)) void __wrap___cxa_throw(
    void* thrownException,
    void* type,
    void (*destructor)(void*)) {
  onThrow(thrownException, static_cast<const std::type_info*>(type));
  __real___cxa_throw(thrownException, type, destructor);
  __builtin_unreachable();
}
} // extern "C"

#elif defined(__APPLE__)
// macOS: Register with folly's exception tracer callback system

static void throwCallback(
    void* thrownException,
    std::type_info* type,
    void (**)(void*)) noexcept {
  onThrow(thrownException, type);
}

static struct RegisterThrowCallback {
  RegisterThrowCallback() {
    folly::exception_tracer::registerCxaThrowCallback(throwCallback);
  }
} registrar;

#elif defined(_WIN32)
// Windows: Vectored Exception Handler catches C++ exceptions (SEH 0xE06D7363).

constexpr DWORD kCppExceptionCode = 0xE06D7363;

LONG WINAPI cppExceptionHandler(PEXCEPTION_POINTERS info) {
  if (info && info->ExceptionRecord &&
      info->ExceptionRecord->ExceptionCode == kCppExceptionCode) {
    onThrow(nullptr, nullptr);
  }
  return EXCEPTION_CONTINUE_SEARCH;
}

// AddVectoredExceptionHandler first parameter: CALL_FIRST (1) registers our
// handler at the front of the VEH list so it runs before other handlers,
// ensuring we capture the stack trace before the exception is modified.
// See:
// https://learn.microsoft.com/en-us/windows/win32/api/errhandlingapi/nf-errhandlingapi-addvectoredexceptionhandler
#define CALL_FIRST 1

struct VehRegistrar {
  PVOID handle_ = nullptr;
  VehRegistrar(const VehRegistrar&) = delete;
  VehRegistrar& operator=(const VehRegistrar&) = delete;
  VehRegistrar(VehRegistrar&&) = delete;
  VehRegistrar& operator=(VehRegistrar&&) = delete;

  VehRegistrar() {
    handle_ = AddVectoredExceptionHandler(CALL_FIRST, cppExceptionHandler);
    if (!handle_) {
      fprintf(
          stderr,
          "Failed to register VEH for throw-site stack trace capture\n");
    }
  }
  ~VehRegistrar() {
    if (handle_) {
      RemoveVectoredExceptionHandler(handle_);
    }
  }
};

static VehRegistrar vehRegistrar;

#endif // platform hooks

namespace facebook::eden {

// Lazy symbolization via Rust FFI — resolves captured frames on demand.
std::optional<std::string> getThrowSiteStackTrace() {
  auto trace = symbolize_captured_trace();
  if (trace.empty()) {
    return std::nullopt;
  }
  return std::string(trace.data(), trace.size());
}

} // namespace facebook::eden
