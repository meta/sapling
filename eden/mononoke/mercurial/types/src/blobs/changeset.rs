/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

mod revlog;
pub use revlog::Extra;
pub use revlog::RevlogChangeset;
pub use revlog::serialize_cs;
pub use revlog::serialize_extras;

mod blob;
pub use blob::ChangesetMetadata;
pub use blob::HgBlobChangeset;
pub use blob::HgChangesetContent;
pub use blob::SNAPSHOT_METADATA_KEY;
pub use blob::snapshot_state;

#[cfg(test)]
mod test;
