/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

mod debug;
mod linelog;
mod maybe_mut;
#[allow(unused)]
mod nanodag;
mod stacks;

pub use small_set::SmallSet as SmallRevs;

pub use crate::linelog::AbstractLineLog;
pub use crate::linelog::CheckoutRev;
pub use crate::linelog::EditFlags;
pub use crate::linelog::EntryId;
pub use crate::nanodag::NanoDag;
pub use crate::stacks::FlattenLine;

/// LineLog with string line content.
pub type LineLog = AbstractLineLog<String>;

// Re-export
pub use smallvec;

#[cfg(test)]
mod tests;
