/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

 fn main() {
    cc::Build::new()
        .files(&[
            "ffi/bridge.c",
            "original_sources/diff-delta.c",
        ])
        .std("c99")
        .flag("-DCARGO_BUILD=1")
        .flag("-DNO_OPENSSL=1")
        .flag_if_supported("-Wno-sign-compare")
        .compile("git_delta_bridge");
}
