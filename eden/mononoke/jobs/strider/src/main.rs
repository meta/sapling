/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the CLI, in the next diff, runs the families")
)]
mod families;
pub mod runner;
#[cfg(test)]
mod test_utils;
pub mod validation;

fn main() {}
