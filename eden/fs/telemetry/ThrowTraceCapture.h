/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#pragma once

#include <optional>
#include <string>

#include <typeinfo>

namespace facebook::eden {

/**
 * Do not capture a throw-site trace for exceptions of exactly `type` when
 * `isExpected` returns true for the thrown object. Expected errors such as a
 * negative lookup throw on request paths, and the capture is the largest
 * cost of such a throw. Call during startup; at most four types.
 */
void skipThrowTraceFor(
    const std::type_info& type,
    bool (*isExpected)(const void* exception));

/**
 * Turn throw-site trace capture on or off for the whole process. It is off by
 * default: the capture unwinds the stack on every throw, and only structured
 * error logging reads the result.
 */
void setThrowTraceCaptureEnabled(bool enabled);

/**
 * Returns the throw-site stack trace for the current exception.
 * Must be called inside a catch block.
 *
 * Uses backtrace-rs via Rust FFI for cross-platform stack trace capture.
 * Raw IP addresses are captured at throw time; symbolization is deferred
 * until this function is called (lazy symbolization).
 *
 * Platform-specific hooks:
 *   Linux:   __wrap___cxa_throw (via --wrap linker flag)
 *   macOS:   __cxa_throw override (via dlsym RTLD_NEXT)
 *   Windows: Vectored Exception Handler (SEH code 0xE06D7363)
 */
std::optional<std::string> getThrowSiteStackTrace();

} // namespace facebook::eden
