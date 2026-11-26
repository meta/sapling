/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use metaconfig_types::MetadataCacheConfig;
use metaconfig_types::MetadataCacheUpdateMode;
use mononoke_macros::mononoke;
use repos::RawMetadataCacheConfig;
use repos::RawMetadataCacheUpdateMode;
use repos::RawPolling;
use repos::RawTailing;

use crate::convert::Convert;

#[mononoke::test]
fn converts_update_modes() {
    let parsed = RawMetadataCacheConfig {
        wbc_update_mode: Some(RawMetadataCacheUpdateMode::tailing(RawTailing {
            category: "legacy_bookmarks".to_string(),
            ..Default::default()
        })),
        wbc_event_router_update_mode: Some(RawMetadataCacheUpdateMode::tailing(RawTailing {
            category: "event_router_bookmarks".to_string(),
            ..Default::default()
        })),
        tags_update_mode: Some(RawMetadataCacheUpdateMode::polling(RawPolling::default())),
        content_refs_update_mode: Some(RawMetadataCacheUpdateMode::tailing(RawTailing {
            category: "content_refs".to_string(),
            ..Default::default()
        })),
        ..Default::default()
    }
    .convert()
    .unwrap();
    assert_eq!(
        parsed,
        MetadataCacheConfig {
            wbc_update_mode: Some(MetadataCacheUpdateMode::Tailing {
                category: "legacy_bookmarks".to_string(),
            }),
            wbc_event_router_update_mode: Some(MetadataCacheUpdateMode::Tailing {
                category: "event_router_bookmarks".to_string(),
            }),
            tags_update_mode: Some(MetadataCacheUpdateMode::Polling),
            content_refs_update_mode: Some(MetadataCacheUpdateMode::Tailing {
                category: "content_refs".to_string(),
            }),
        }
    );
}
