# Hook Implementation Guide

## Key Paths

| What | Path |
|------|------|
| OSS hook implementations | `fbcode/eden/mononoke/features/hooks/src/implementations/` |
| Facebook-only hook implementations | `fbcode/eden/mononoke/features/hooks/src/facebook/implementations/` |
| OSS hook registration | `fbcode/eden/mononoke/features/hooks/src/implementations.rs` |
| Facebook-only hook registration | `fbcode/eden/mononoke/features/hooks/src/facebook/implementations.rs` |
| BUCK | `fbcode/eden/mononoke/features/hooks/BUCK` |
| Cargo.toml | `fbcode/eden/mononoke/features/hooks/Cargo.toml` |
| Autocargo Cargo.toml | `fbcode/eden/mononoke/public_autocargo/features/hooks/Cargo.toml` |
| Integration tests | `fbcode/eden/mononoke/tests/integration/hooks/` |

## Hook Types

- **ChangesetHook** -- validates the entire commit (directory sizes, commit message, merge policy).
  Registered in `make_changeset_hook`. Most common.
- **FileHook** -- validates individual files (content patterns, filenames, size). Registered in `make_file_hook`.
- **BookmarkHook** -- validates bookmark operations (tag limits, branch creation). Registered in `make_bookmark_hook`.

## Step 1: Decide hook type and config

Read the user's request and decide:
1. Which hook type (ChangesetHook, FileHook, BookmarkHook)
2. What config fields the hook needs (deserialized from JSON via serde)
3. What derived data it needs (ContentManifest for recursive sizes, SkeletonManifest for entry counts,
   fsnodes for file info)

Read `implementations.rs` and an existing similar hook (e.g. `limit_directory_size.rs`, `block_files.rs`) for patterns.

## Step 2: Create 3 commits

Split the work into 3 commits on a stack:

**Commit 1 -- Skeleton:**
- New file `implementations/<hook_name>.rs` with:
  - Config struct with `#[derive(Deserialize, Clone, Debug)]`
  - Hook struct with `new(config: &HookConfig)` and `with_config(config: Config)`
  - No-op `run()` returning `Ok(HookExecution::accepted())`.
  - For hooks whose behavior differs by execution context, implement `run_with_purpose` and inspect
    `HookExecutionPurpose`.
  - `HookExecution` wraps a `HookResult` plus `extra_logs`; build with `accepted()` / `rejected(info)`, or
    `*_with_logs` variants to attach Scuba diagnostic lines.
- `implementations.rs`: add `mod <hook_name>;` line only (NOT the match arm)
- No tests, no BUCK/Cargo changes (skeleton has minimal imports)

**Commit 2 -- Tests:**
- `implementations/<hook_name>.rs`: add `#[cfg(test)] mod test` with ALL test cases, but rejection tests
  assert `Accepted` with `// TODO` comments (showing wrong/no-op behavior)
- Integration test `.t` file with all pushes succeeding (wrong behavior)

**Commit 3 -- Wiring + Implementation:**
- `implementations.rs`: add match arm in `make_changeset_hook`/`make_file_hook`/`make_bookmark_hook`
- `implementations/<hook_name>.rs`: real `run()` logic + updated tests asserting correct rejections
- BUCK + both Cargo.toml: add new deps (sorted order)
- Integration test: updated expected output with rejections
- Keep the `Differential Revision:` line if updating an existing diff

## Step 3: Verify

Run after each commit:
```bash
arc rust-check fbcode//eden/mononoke/features/hooks:hooks
buck test fbcode//eden/mononoke/features/hooks:hooks -- <hook_name>
arc lint -a -e extra
```

## Rejection Messages

Use `HookRejectionInfo::new_long(short_desc, long_desc)`:
- `short_desc`: terse label like `"Directory too large"`
- `long_desc`: human-readable with path, current value, limit, and what the user should do
- User-visible rejection text is declared with `hook_messages!` (example:
  `implementations/always_fail_changeset.rs`), rendered with
  `<name>().into_rejection(...)`; repos override templates via the config `messages` map.
