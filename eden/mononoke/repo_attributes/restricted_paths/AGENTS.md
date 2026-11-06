# Path ACLs (restricted paths)

Path ACLs (also called restricted paths, or code tenting) restrict access to
directories in a Mononoke repo. A restricted directory is a
**tent**. Each tent is guarded by one REPO_REGION ACL, and the server decides
for every access whether the caller may read it. This crate is the core of the
feature: restriction lookup, authorization, enforcement, and access logging.
Every other Path ACL crate either feeds it or calls it.

Paths below are relative to `fbcode/eden/mononoke/`. Meta-internal technical
details, where available, are in
`repo_attributes/restricted_paths/facebook/AGENTS.md`.

## Keep these files current

Each Path ACL directory with an `AGENTS.md` documents its own design. When you
change a behaviour, invariant, config field or module that one of them
describes, update it in the same diff. Record new design decisions, with the
reason, in [Design decisions](#design-decisions) or in the narrower file they
belong to. `CLAUDE.md` is a symlink to `AGENTS.md`: edit `AGENTS.md`.

## Glossary

| Term | Meaning |
|---|---|
| Tent / restriction | A restricted directory tree guarded by one REPO_REGION ACL. One access can match several restrictions (nested or multiple tents). |
| Restriction root | The directory that carries the restriction: the directory holding the `.slacl` file, or the configured path. |
| Restriction source | Where a restriction is declared: the repo config (`path_restriction_metadata`), or `.slacl` files read through the AclManifest derived data. |
| Authorization | Whether the caller may read a restriction. |
| Permission request group | The group a denied caller is told to request. |
| Allowlist groups | Configured groups that authorize reads in addition to the tent's ACL. |

## Where the code lives

| Area | Path | What it owns |
|---|---|---|
| Core facet | `repo_attributes/restricted_paths/` (this crate) | `RestrictedPaths` facet, lookups, authorization, enforcement, access logging, `.slacl` change validation |
| Config-backed source | `repo_attributes/restricted_paths_common/` | `RestrictedPathsConfigBased`, manifest-id store and cache, `ManifestType` |
| `.slacl` parser | `repo_attributes/restricted_paths_acl_file/` | `parse_acl_file` |
| AclManifest derived data | `derived_data/acl_manifest/` | The sparse tree of restriction roots derived from `.slacl` files |
| Config types | `metaconfig/types/src/lib.rs` | `RestrictedPathsConfig` and its field types, `RestrictedPathsAclFile`, `parse_bare_group_name` |
| Config parsing | `metaconfig/parser/src/convert/repo.rs` | Raw repo config to the types above |
| Manifest-id store writes | derivation crates | `ManifestIdStoreWriteCallsite` |
| ACL data in HgAugmented manifests | `derived_data/mercurial_derivation/src/upload_augmented_manifest.rs` | Builds ACL nodes with `acl_manifest::acl_node_for_directory` |
| Enforcement callers | `mononoke_api/`, `mononoke_api_hg/` | Call the enforcement entry points |
| API-layer types | `mononoke_api/src/restricted_paths.rs`, `mononoke_api/src/errors.rs` | `PathAccessInfo`, `RestrictedPathsPolicy`, `MononokeError::RestrictedPathsAuthorizationError` |
| Changeset creation | changeset creation paths | Call `validate_acl_file_changes_in_changesets` |
| SCS | `servers/scs/scs_methods/src/methods/commit_restricted_paths.rs`, `servers/scs/scs_errors/` | Restricted-paths metadata methods, denial error mapping |
| Admin CLI | `tools/admin/src/commands/restricted_paths.rs` | Manifest-id store administration |
| `scsc` CLI | `clients/scsc/src/commands/restricted_paths.rs` | Client commands for the metadata methods |

## Architecture

This crate is a repo attribute: it must not depend on `features/`, `tools/` or
server business logic (it uses the SLAPI `context` crate only for
`CoreContext`). Servers call `mononoke_api`, which calls this crate.

```
server handlers
        |
mononoke_api, mononoke_api_hg   (RestrictedPathsPolicy)
        |
restricted_paths (this crate)   lib.rs: entry points and enforcement orchestration
   |-- restriction_check.rs     authorization and enforcement outcomes
   |-- restriction_info.rs      source lookups (config, AclManifest), source dispatch
   `-- access_log.rs            access-log rows
        |                        |
restricted_paths_common     acl_manifest derived data -> restricted_paths_acl_file
```

- `lib.rs`: the facet, public lookup methods, the `.slacl` validator, and the
  enforcement entry points `spawn_enforce_restricted_path_access` and
  `spawn_enforce_restricted_manifest_access`. Both delegate to
  `spawn_enforce_restricted_access`, the single enforcement flow. Put new
  enforcement logic there, not in the entry points.
- `restriction_check.rs`: per-restriction authorization, enforcement outcomes
  and their aggregation across sources, `SharedFetchHandle`, and the logging
  aggregate `SourceRestrictionSummary`.
- `restriction_info.rs`: restriction lookups from each source, and the
  source dispatch.
- `access_log.rs`: builds access-log rows (raw Scuba, plus the schematized
  logger in fbcode builds).

## Enforcement

- Every access check goes through `spawn_enforce_restricted_access`, which
  reads each restriction source once through shared handles (logging reuses
  the same handles), and returns `RestrictedPathsError::AuthorizationError` on
  a denial.
- Fail closed: a failed source read or ACL check returns an error, never an
  allow.
- When several restrictions deny, the reported `DenyingRestriction` is chosen
  deterministically, so error messages are stable.
- Logging receives the computed outcome; it never recomputes it.

### Config validation

Repo config validation lives in the repo config parser
(`metaconfig/parser/src/convert/repo.rs`) and the constructors in
`metaconfig_types`. Add each new rule there, with a parser unit test.

## Restriction sources

- `RestrictedPaths::new` checks that the derived data types the restriction
  sources need are enabled.
- An access is denied if any source denies, and metadata lookups return the
  union (for the same root, the AclManifest entry wins).
- Use `may_have_restricted_paths()`, not config emptiness, to decide whether a
  repo can have restrictions: it accounts for `.slacl` files.

## Authorization

Each matched restriction is authorized on its own
(`check_restriction_authorization_with_acl`). The caller is authorized for a
restriction through `read` on its REPO_REGION ACL or a configured allowlist
group.

- An access is denied if **any** of its restrictions denies. Nested and
  multiple tents are conjunctive: access to one tent never authorizes another.
- An absent group never grants access.
- ACL-provider and group-membership errors propagate (fail closed).
- `SourceRestrictionSummary` aggregates checks for logging only; enforcement
  uses the per-restriction results. Keep the two separate.

## Metadata APIs

Restriction metadata and check APIs are implemented in `restriction_info.rs`
and `restriction_check.rs`. They report restrictions and authorization, not
enforcement. A client that asks whether a fetch would be served (SLAPI
`check_manifest_permission`) goes through the manifest enforcement entry
point instead, so its answer is the fetch's decision and logs as one.

## `.slacl` change validation

`validate_acl_file_changes_in_changesets` authorizes `.slacl` changes; do not
re-implement its checks at call sites.

## Access logging

Rows are built in `access_log.rs` and written to `ACCESS_LOG_SCUBA_TABLE` and
the schematized logger. A new or renamed column must be added to both.
Logged enum values come from `strum::AsRefStr` derives; renaming a variant
changes logged data.

## Design decisions

Record new decisions here with their reason.

1. **Server-side enforcement.** The server decides every access; enforcement
   does not depend on client behaviour. Client-side checks are UX only.
2. **Manifest enforcement.** Manifest accesses are matched to restrictions
   server-side (`restriction_info.rs`).
3. **Fail closed.** A failed source read or ACL check is an error, never an
   allow.
4. **Authorization is recorded.** The access log records the caller's
   authorization.
5. **One enforcement flow, evaluated once.** Path and manifest enforcement
   share `spawn_enforce_restricted_access`.
6. **Two restriction sources.** Config and `.slacl`; an access is denied if
   either denies.
7. **Cheap guards before I/O.** Avoid per-tree store I/O that cannot match a
   restriction.

## Changing this code safely

- This is the hot read path of every surface. A bug either serves restricted
  content or denies unrestricted content to everyone. Prefer small, reversible
  diffs.
- Never default an ACL check, source lookup or config read to allow.
- When you change authorization, change the per-restriction checks and the
  logging summary deliberately, and test both.
- A behaviour change updates the unit tests, `test/` and the integration
  tests.
- Split refactors from behaviour changes; add tests that pin the existing
  behaviour in a diff below the behaviour change.

## Testing

```bash
arc rust-check fbcode//eden/mononoke/repo_attributes/restricted_paths:restricted_paths
buck2 test @fbcode//mode/opt fbcode//eden/mononoke/repo_attributes/restricted_paths/...
```

- Unit tests: inline in `lib.rs`, and `src/<module>/tests.rs` for the other
  modules, with helpers in `src/test_utils.rs`.
- `test/`: the scenario test target `restricted_paths_test` (see
  `test/AGENTS.md`).
- A change to enforcement or restriction config parsing needs unit tests in
  `src/restriction_check/tests.rs`, a parser unit test per validation rule,
  and updated integration goldens.
- New tests start with a `What it tests:` / `Expected:` comment.
