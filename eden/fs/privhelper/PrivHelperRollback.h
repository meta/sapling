/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#pragma once

namespace facebook::eden {

constexpr const char* kDisablePrivHelperHardeningPath{
    "/etc/eden/disable_privhelper_hardening"};

/**
 * Check the root-controlled rollback marker and minimum supported kernel.
 * The marker restores legacy mount operations, cleanup, mount flags, request
 * validation, and startup owner selection. It intentionally disables security
 * checks; ordinary syscall errors never enable rollback.
 * Stop EdenFS and wait for its helper to exit before changing the marker, then
 * start EdenFS again. Owner IDs and existing mount flags cannot change live.
 */
bool disablePrivHelperHardening();

} // namespace facebook::eden
