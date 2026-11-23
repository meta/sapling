/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include "eden/fs/telemetry/ThrowTraceCapture.h"

#include <stdexcept>

#include <folly/CPortability.h>
#include <gtest/gtest.h>

using namespace facebook::eden;

namespace {

const bool kCaptureEnabled = [] {
  setThrowTraceCaptureEnabled(true);
  return true;
}();

[[noreturn]] FOLLY_NOINLINE void innerThrowingFunc() {
  throw std::runtime_error("inner error");
}

[[noreturn]] FOLLY_NOINLINE void outerThrowingFunc() {
  innerThrowingFunc();
}

} // namespace

TEST(ThrowTraceCaptureTest, CapturesThrowSiteStackTrace) {
  try {
    outerThrowingFunc();
  } catch (const std::exception&) {
    auto trace = getThrowSiteStackTrace();
    ASSERT_TRUE(trace.has_value()) << "Stack trace capture is broken";
#ifndef _WIN32
    // Windows without PDB debug info can't resolve function names
    EXPECT_NE(trace->find("innerThrowingFunc"), std::string::npos)
        << "Expected throw-site function in trace, got: " << *trace;
    EXPECT_NE(trace->find("outerThrowingFunc"), std::string::npos)
        << "Expected caller function in trace, got: " << *trace;
#endif
  }
}

namespace {
struct ExpectedError : std::runtime_error {
  using std::runtime_error::runtime_error;
};
} // namespace

TEST(ThrowTraceCaptureTest, SkipsRegisteredExpectedTypes) {
  skipThrowTraceFor(typeid(ExpectedError), [](const void*) { return true; });
  try {
    outerThrowingFunc();
  } catch (const std::exception&) {
    // An earlier trace must not survive an expected throw.
  }
  try {
    throw ExpectedError("expected");
  } catch (const ExpectedError&) {
#ifndef _WIN32
    // The Windows hook sees no exception type, so nothing is skipped there.
    EXPECT_FALSE(getThrowSiteStackTrace().has_value());
#endif
  }
  try {
    outerThrowingFunc();
  } catch (const std::exception&) {
    EXPECT_TRUE(getThrowSiteStackTrace().has_value());
  }
}

TEST(ThrowTraceCaptureTest, ReturnsNulloptOutsideCatchBlock) {
  auto trace = getThrowSiteStackTrace();
  EXPECT_FALSE(trace.has_value());
}

TEST(ThrowTraceCaptureTest, CapturesNothingWhenDisabled) {
  // Drop any trace an earlier test left on this thread.
  (void)getThrowSiteStackTrace();
  setThrowTraceCaptureEnabled(false);
  try {
    outerThrowingFunc();
  } catch (const std::exception&) {
    EXPECT_FALSE(getThrowSiteStackTrace().has_value());
  }
  setThrowTraceCaptureEnabled(true);
}
