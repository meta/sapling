---
name: acr-sev-derived-patterns
metadata:
  oncalls: ['scm_client_infra']
  strict: true
  apply_to_path: 'eden/scm/.*\.(py|rs)$'
  apply_to_content: 'PathAuditor|audit|symlink|CBOR|serde|Deserialize|cfg!\(not\(windows\)\)|deprecated_default|retry|rate_limit'
  apply_to_clients: ['code_review']
---

# SEV-Derived Dangerous Code Patterns

**Severity: HIGH**

Patterns identified from analysis of ~117 Sapling and Mononoke SEVs (2023-2026). Client-side patterns from analysis of Sapling/Mononoke SEVs (2023-2026). Server-side patterns are covered by Mononoke's own review guidelines.
Each pattern below has caused at least one SEV.

## What to Look For

- Path validation and filesystem security checks (PathAuditor, symlink handling)
- Wire protocol or serialization changes (serde CBOR, Thrift structs, compression)
- API endpoints or operations that accept unbounded input
- Retry logic modifications or removal
- Platform-specific filesystem assumptions (case sensitivity, symlink behavior)

## When to Flag

### Path Auditor Case-Sensitivity Bypass (S630253, S631940 — both RCE)
- Path validation using `cfg!(not(windows))` as a proxy for "case-sensitive filesystem"
  — macOS uses case-insensitive APFS by default, so `.SL`, `.Hg`, `.HG` bypass
  blocklist checks and enable `.sl/config` overwrite leading to remote code execution
- ASCII lowercasing alone as the case-insensitive check on macOS — APFS folds via NFD +
  Unicode case folding, so `.ſl` reaches `.sl`; `FsFeatures::APPLE_NFD` must be set
  alongside `CASE_INSENSITIVE` for `audit_invalid_components` to reject it
- Symlink creation that validates the symlink NAME but not where it POINTS — allows
  `Link -> .sl` to redirect writes into the repo config directory
- Directory creation (especially for submodules) that bypasses the PathAuditor entirely
- Missing case-fold collision detection during checkout (only checked during `sl add`)

### Unbounded Query Amplification (S548986, S428805, S501479, S617619, S610888)
- API endpoints that accept empty or degenerate input and perform work proportional
  to the entire repository size — empty ancestors set in `commit_graph_v2` caused
  O(repo-size) SQL queries
- O(n^2) algorithmic complexity on user-controlled input — a large diff stack caused
  per-diff TD jobs to rebase ALL ancestor drafts, each running `hg bundle` querying
  the filenodes DB, falling back from replicas to the DB master
- Merge drivers or hooks running unbounded `sl diff` or `sl prefetch` across arbitrary
  commit ranges during rebase — amplified by running on every Sandcastle rebase
- CI jobs that schedule unbounded fan-out without deduplication — 250k concurrent
  Sandcastle jobs from duplicate triggers overwhelmed the database

### Backwards-Incompatible Serialization (S430619, S599975, S618804)
- Removing fields from serde CBOR structs used in the Mononoke-Sapling wire protocol
  without coordinating client-side changes first — `missing field '1'` errors
- Rust Thrift enum fields using `deprecated_default_enum_min_i32` — zero-value defaults
  are interpreted as `i32::MIN`, silently corrupting data at deserialization
- Compression format changes (zstd) between client and server without graceful fallback
  — "zstd stream did not finish" errors fleet-wide

### Silent Retry Removal (S519521, S513355)
- Removing retry logic as "cleanup" without realizing it was load-bearing — a single
  bad Mononoke host caused complete IO failures for any client routed to it because
  retries had been accidentally turned off months earlier
- Environment variable inheritance exposing Sapling to aggressive jemalloc tunings from
  systemd, combined with removed retries amplifying SSL timeouts into total failures

## Do NOT Flag

- Additive protocol changes (adding new optional fields is safe)
- Path validation that explicitly handles case-insensitive filesystems at runtime
  (e.g., checking `fs::is_case_sensitive()` or equivalent)
- Retry logic changes that replace one mechanism with another (not removal)
- Test code in `tests/` or code using `testutil` fixtures

## Examples

**BAD (path auditor case-sensitivity bypass — S630253):**
```rust
fn audit_invalid_components(path: &RepoPath) -> Result<()> {
    for component in path.components() {
        // WRONG: cfg!(not(windows)) treats macOS as case-sensitive
        // but macOS APFS is case-insensitive by default
        let dominated = if cfg!(not(windows)) {
            component.as_str()  // ".SL" won't match ".sl" blocklist
        } else {
            &component.as_str().to_lowercase()
        };
        if BLOCKLIST.contains(dominated) {
            return Err(/* ... */);
        }
    }
    Ok(())
}
```

**GOOD (fold the way the filesystem does — see `lib/vfs/src/pathauditor.rs`):**
```rust
// macOS gets HFS_STRIP | APPLE_NFD; CASE_INSENSITIVE is added when the FS is
// case-insensitive. APPLE_NFD upgrades the case check from ASCII lowercase to
// NFD + Unicode case folding, so `.ſl`-style aliases are caught too.
audit_invalid_components(path.as_str(), FsFeatures::current_platform())?;
```

**BAD (symlink target not validated — S631940):**
```rust
fn write_symlink(path: &RepoPath, target: &[u8]) -> Result<()> {
    audit_invalid_components(path)?;  // Only checks the symlink NAME
    // WRONG: does not validate where the symlink POINTS
    // attacker can create: Link -> .sl (redirects writes into repo config)
    std::os::unix::fs::symlink(target, path)?;
    Ok(())
}
```

**GOOD (validate both name and target):**
```rust
fn write_symlink(path: &RepoPath, target: &[u8]) -> Result<()> {
    audit_invalid_components(path)?;
    let target_path = RepoPath::from_utf8(target)?;
    audit_invalid_components(target_path)?;  // Also audit the target
    std::os::unix::fs::symlink(target, path)?;
    Ok(())
}
```

**BAD (unbounded query from degenerate input — S428805):**
```rust
fn location_to_hash(ancestors: &[HgId]) -> Result<Vec<HgId>> {
    // WRONG: empty ancestors means "traverse entire commit graph"
    let query = format!(
        "SELECT ... FROM commit_graph_edges WHERE repo_id={} ...",
        repo_id
    );
    // O(repo-size) query with no bound
    db.query(query)
}
```

**GOOD (validate input, bound the query):**
```rust
fn location_to_hash(ancestors: &[HgId]) -> Result<Vec<HgId>> {
    if ancestors.is_empty() {
        return Err(anyhow!("ancestors set must not be empty"));
    }
    // Bounded query with LIMIT
    let query = format!(
        "SELECT ... FROM commit_graph_edges WHERE repo_id={} ... LIMIT {}",
        repo_id, MAX_RESULTS
    );
    db.query(query)
}
```

**BAD (Rust Thrift deprecated default — S599975):**
```rust
// deprecated_default_enum_min_i32 causes i32::MIN default, NOT 0
#[derive(Deserialize)]
struct BookmarkInfo {
    pub freshness: i32,  // Deserialized as -2147483648 when missing
}
```

**GOOD (explicit serde default):**
```rust
#[derive(Deserialize)]
struct BookmarkInfo {
    #[serde(default)]  // Defaults to 0, not i32::MIN
    pub freshness: i32,
}
```

## Evidence

These patterns are derived from ~117 Sapling/Mononoke SEVs spanning 2023-2026.

| Pattern | SEVs | Combined Impact |
|---------|------|-----------------|
| Path auditor bypass (RCE) | S630253, S631940 | Remote code execution on macOS/Windows |
| Unbounded query amplification | S548986, S428805, S501479, S617619, S610888 | SEV-1: all lands blocked 5 hours |
| Serialization incompatibility | S430619, S599975, S618804 | buck2/hh failures, clone failures |
| Silent retry removal | S519521, S513355 | Fleet-wide IO failures |
