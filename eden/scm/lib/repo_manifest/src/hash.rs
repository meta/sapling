/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Git object-id recognition shared across the crate.

/// Returns true if `s` is a full 40-character hex Git SHA-1.
///
/// git-repo treats such a revision as already pinned ("only save the origin if
/// the origin is not a sha1"), so callers use this to skip re-resolving a
/// revision that is already a concrete object id.
pub fn is_git_hash(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::is_git_hash;

    #[test]
    fn test_full_hex_is_git_hash() {
        assert!(is_git_hash(&"a".repeat(40)), "40 hex chars is a git hash");
    }

    #[test]
    fn test_symbolic_ref_is_not_git_hash() {
        assert!(!is_git_hash("main"), "a branch name is not a git hash");
    }

    #[test]
    fn test_short_hex_is_not_git_hash() {
        assert!(
            !is_git_hash(&"a".repeat(39)),
            "39 hex chars is one short of a git hash"
        );
    }

    #[test]
    fn test_forty_non_hex_is_not_git_hash() {
        assert!(
            !is_git_hash(&"g".repeat(40)),
            "40 non-hex chars is not a git hash"
        );
    }

    #[test]
    fn test_sha256_length_hex_is_not_git_hash() {
        // A 64-hex string is a SHA-256-length object id. `is_git_hash` is the
        // canonical "already a pinned SHA-1" check — exactly 40 hex — so a longer
        // all-hex string must be rejected. Guards a future `>= 40` mutation of
        // the length check.
        assert!(
            !is_git_hash(&"a".repeat(64)),
            "64 hex chars (SHA-256 length) is not a 40-char SHA-1 git hash"
        );
    }
}
