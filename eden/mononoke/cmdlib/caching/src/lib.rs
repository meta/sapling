/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

mod args;
#[cfg(fbcode_build)]
pub mod facebook;
mod settings;

use environment::Caching;
#[cfg(fbcode_build)]
use environment::LocalCacheConfig;
#[cfg(fbcode_build)]
use environment::LocalCacheEncoding;
use fbinit::FacebookInit;

#[cfg(fbcode_build)]
use crate::args::CacheEncoding;
pub use crate::args::CacheMode;
pub use crate::args::CachelibArgs;
pub use crate::settings::CachelibSettings;

/// Initializes cachelib with settings that are patched beforehand with args
pub fn init_cachelib(
    fb: FacebookInit,
    settings: &CachelibSettings,
    args: &CachelibArgs,
) -> Caching {
    if args.cache_mode == CacheMode::Disabled {
        return Caching::Disabled;
    }

    let mut settings = settings.clone();
    settings.update_from_args(args);

    #[cfg(not(fbcode_build))]
    {
        let _ = fb;
        unimplemented!("Initialization of cachelib works only for fbcode builds")
    }
    #[cfg(fbcode_build)]
    {
        let enable_cacheadmin = !args.cachelib_disable_cacheadmin;
        facebook::init_cachelib_from_settings(fb, settings, enable_cacheadmin)
            .expect("cachelib initialize should always succeed");

        let encoding = match args.cache_encoding {
            CacheEncoding::Bincode => LocalCacheEncoding::Bincode,
        };

        match args.cache_mode {
            CacheMode::Enabled => Caching::Enabled(LocalCacheConfig {
                blobstore_cache_shards: args.cachelib_shards,
                encoding,
            }),
            CacheMode::LocalOnly => Caching::LocalOnly(LocalCacheConfig {
                blobstore_cache_shards: args.cachelib_shards,
                encoding,
            }),
            CacheMode::Disabled => unreachable!(),
        }
    }
}
