/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

pub(crate) mod commands;
mod connection;
mod runtime;

pub(crate) use connection::ConnectionArgs;
pub(crate) use runtime::PlatformArgs;
pub(crate) use runtime::disable_cpp_log;
pub(crate) use runtime::long_version;
pub(crate) use runtime::prepare;
pub(crate) use runtime::short_version;
// The test-support build of the scmquery client is a separate crate
// (`scmqueryclient_rust_test_support`) so it can coexist with the prod crate in
// one build graph without a name collision; alias it back to the canonical
// `scmqueryclient_rust::` path so submodules don't care which is linked.
// Linux-only: it transitively depends on srclients, which is Linux-only.
#[cfg(target_os = "linux")]
pub(crate) use scmqueryclient_rust_test_support as scmqueryclient_rust;
