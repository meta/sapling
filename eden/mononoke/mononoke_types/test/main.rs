/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Tests of the audit implementations in `mononoke_types` that need a test
//! repository. They are a crate of their own because `test_repo_factory`
//! depends on `mononoke_types`.

mod changeset;
mod file_content;
mod test_utils;
