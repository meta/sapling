/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

mononoke_app::subcommands! {
    mod apply;
    mod check_key;
    mod import_keys;
    mod plan;
    mod recipes;
    mod scan;
    mod verify;
}
