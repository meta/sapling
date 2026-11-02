/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum FetchCause {
    // The fetch originated from EdenFS inode garbage collection
    EdenInodeGc,
    // Unknown orginination from EdenFS
    EdenUnknown,
    // The fetch originated from a Eden Thrift prefetch endpoint
    EdenPrefetch,
    // The fetch originated from an Eden Thrift glob endpoint
    EdenGlob,
    // The fetch originated from a Eden Thrift endpoint
    EdenThrift,
    // The fetch originated from FUSE/NFS/PrjFS
    EdenFs,
    // The fetch originated from a mixed EdenFS causes
    EdenMixed,
    // The fetch originated from prefetch based on apparent EdenFS walk
    EdenWalkPrefetch,
    // The fetch originated from a Sapling prefetch
    SaplingPrefetch,
    // Unknown orginination from Sapling
    SaplingUnknown,
    // The fetch originated from a Sapling checkout operation
    SaplingCheckout,
    // The fetch originated from a Sapling copytrace operation
    SaplingCopytrace,
    // The fetch originated from a Sapling status operation
    SaplingStatus,
    // The fetch originated from a Sapling sparse profile fetch
    SaplingSparse,
    // The fetch originated from a Sapling log/pathhistory operation
    SaplingLog,
    // Unknown originiation, usually from Sapling (the default)
    Unspecified,
}

impl FetchCause {
    pub fn to_str(&self) -> &str {
        match self {
            FetchCause::EdenInodeGc => "edenfs-inode-gc",
            FetchCause::EdenUnknown => "edenfs-unknown",
            FetchCause::EdenPrefetch => "edenfs-prefetch",
            FetchCause::EdenGlob => "edenfs-glob",
            FetchCause::EdenThrift => "edenfs-thrift",
            FetchCause::EdenFs => "edenfs-fs",
            FetchCause::EdenMixed => "edenfs-mixed",
            FetchCause::EdenWalkPrefetch => "eden-walk-prefetch",
            FetchCause::SaplingPrefetch => "sl-prefetch",
            FetchCause::SaplingUnknown => "sl-unknown",
            FetchCause::SaplingCheckout => "sl-checkout",
            FetchCause::SaplingCopytrace => "sl-copytrace",
            FetchCause::SaplingStatus => "sl-status",
            FetchCause::SaplingSparse => "sl-sparse",
            FetchCause::SaplingLog => "sl-log",
            FetchCause::Unspecified => "unspecified",
        }
    }

    pub fn is_prefetch(&self) -> bool {
        match self {
            FetchCause::EdenPrefetch => true,
            FetchCause::SaplingPrefetch => true,
            FetchCause::EdenWalkPrefetch => true,

            FetchCause::EdenInodeGc => false,
            FetchCause::EdenUnknown => false,
            FetchCause::EdenGlob => false,
            FetchCause::EdenThrift => false,
            FetchCause::EdenFs => false,
            FetchCause::EdenMixed => false,
            FetchCause::SaplingUnknown => false,
            FetchCause::SaplingCheckout => false,
            FetchCause::SaplingCopytrace => false,
            FetchCause::SaplingStatus => false,
            FetchCause::SaplingSparse => false,
            FetchCause::SaplingLog => false,
            FetchCause::Unspecified => false,
        }
    }
}

impl std::str::FromStr for FetchCause {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "edenfs-inode-gc" => Ok(FetchCause::EdenInodeGc),
            "edenfs-unknown" => Ok(FetchCause::EdenUnknown),
            "edenfs-prefetch" => Ok(FetchCause::EdenPrefetch),
            "edenfs-glob" => Ok(FetchCause::EdenGlob),
            "edenfs-thrift" => Ok(FetchCause::EdenThrift),
            "edenfs-fs" => Ok(FetchCause::EdenFs),
            "edenfs-mixed" => Ok(FetchCause::EdenMixed),
            "eden-walk-prefetch" => Ok(FetchCause::EdenWalkPrefetch),
            "sl-prefetch" => Ok(FetchCause::SaplingPrefetch),
            "sl-unknown" => Ok(FetchCause::SaplingUnknown),
            "sl-checkout" => Ok(FetchCause::SaplingCheckout),
            "sl-copytrace" => Ok(FetchCause::SaplingCopytrace),
            "sl-status" => Ok(FetchCause::SaplingStatus),
            "sl-sparse" => Ok(FetchCause::SaplingSparse),
            "sl-log" => Ok(FetchCause::SaplingLog),
            "unspecified" => Ok(FetchCause::Unspecified),
            _ => Err(anyhow::anyhow!("Invalid FetchCause string")),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;
    #[test]
    fn test_fetch_cause_serialisation_deserialisation() {
        let variants = [
            FetchCause::EdenInodeGc,
            FetchCause::EdenUnknown,
            FetchCause::EdenPrefetch,
            FetchCause::EdenGlob,
            FetchCause::EdenThrift,
            FetchCause::EdenFs,
            FetchCause::EdenMixed,
            FetchCause::EdenWalkPrefetch,
            FetchCause::SaplingPrefetch,
            FetchCause::SaplingUnknown,
            FetchCause::SaplingCheckout,
            FetchCause::SaplingCopytrace,
            FetchCause::SaplingStatus,
            FetchCause::SaplingSparse,
            FetchCause::SaplingLog,
            FetchCause::Unspecified,
        ];

        for variant in variants {
            let string = variant.to_str();
            let parsed = FetchCause::from_str(string).unwrap();

            assert_eq!(variant, parsed);
        }
    }
}
