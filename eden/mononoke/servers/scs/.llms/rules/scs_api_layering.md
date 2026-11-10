---
name: scs-api-layering
metadata:
  oncalls: ['source_control']
  strict: true
  apply_to_path: 'eden/mononoke/servers/scs/.*\.rs$'
  apply_to_content: 'use |facet|restricted_paths|commit_graph|blobstore|repo_derived_data|repo_blobstore|repo_identity|phases|bookmarks|pushrebase'
---

# SCS Must Use mononoke_api, Not Facets Directly

**Severity: HIGH**

## What to Look For

- SCS server code (`scs_methods`, `scs_server`) importing and using Mononoke facet crates directly instead of going through `mononoke_api` primitives (`RepoContext`, `ChangesetContext`, etc.)
- Direct access to repo facets like `repo.repo_blobstore()`, `repo.commit_graph()`, `repo.repo_derived_data()` from within SCS handler code
- SCS code that re-implements logic already available (or that should be available) as a `mononoke_api` method
- New SCS code that builds on or extends an existing direct-facet call path instead of adding the `mononoke_api` primitive first (e.g. threading a facet-derived value through new helpers, or adding callers of facet-consuming functions)

## When to Flag

- `use` statements in `scs_methods` or `scs_server` importing facet crates (e.g., `restricted_paths`, `repo_derived_data`, `commit_graph`, `repo_blobstore`, `phases`, `bookmarks`, `pushrebase`, `ephemeral_blobstore`)
- SCS handler code calling facet methods directly on the repo object instead of using `RepoContext`/`ChangesetContext` methods
- SCS code that duplicates logic from `mononoke_api` -- e.g., manually querying derived data when `ChangesetContext` already exposes a method for it
- New SCS endpoints that bypass `mononoke_api` for operations that other consumers (Git server, SLAPI server) would also need
- New functions, structs, or callers that thread facet-derived values (e.g. `RepoEphemeralStore`, `BubbleId`) or add calls to helpers that consume facets directly, entrenching the bypass instead of adding the `mononoke_api` primitive first

## Do NOT Flag

- `mononoke_api` code itself importing facets -- that's the correct layer to do so
- SCS code using `RepoContext`, `ChangesetContext`, `ChangesetPathContext` and their methods -- that IS the correct API
- Test code or test fixtures
- SCS Thrift type definitions and conversion code (these are SCS-specific by nature)
- Imports of `mononoke_api` types like `MononokeError`, `RepoContext`, `ChangesetContext`

## Why This Matters

Mononoke's architecture has a clear layering:

```
SCS Server / Git Server / SLAPI Server  (consumers)
         |
    mononoke_api  (shared primitives: RepoContext, ChangesetContext, etc.)
         |
    facets  (repo_blobstore, commit_graph, derived_data, etc.)
```

When SCS bypasses `mononoke_api` and talks to facets directly:
1. Other servers (Git, SLAPI) can't reuse the logic -- it gets duplicated
2. Cross-cutting concerns (access control, logging, rate limiting) applied in `mononoke_api` get skipped
3. Refactoring facets becomes harder because SCS is coupled to their internal APIs

## Examples

**BAD (SCS handler using facet directly):**
```rust
// In scs_methods/src/commit.rs
use repo_derived_data::RepoDerivedData;

async fn commit_info(repo: &impl RepoDerivedData, cs_id: ChangesetId) -> Result<Info> {
    let derived = repo.repo_derived_data().derive::<SomeType>(ctx, cs_id).await?;
    // ...
}
```

**GOOD (SCS handler using mononoke_api):**
```rust
// In scs_methods/src/commit.rs
use mononoke_api::ChangesetContext;

async fn commit_info(changeset: &ChangesetContext) -> Result<Info> {
    let info = changeset.some_method().await?;
    // The ChangesetContext internally uses the facets, applying
    // access control and logging as needed.
}
```

**BAD (SCS re-implementing mononoke_api logic):**
```rust
// In scs_methods -- manually checking restricted paths via facet
let restricted = repo.restricted_paths_facet().check_paths(ctx, &paths).await?;
```

**GOOD (using mononoke_api primitive):**
```rust
// mononoke_api exposes this as a ChangesetContext method
let restricted = changeset.paths_restriction_info(paths).await?;
```

**BAD (new code entrenched on a facet bypass -- matches D125420313):**
```rust
// In scs_methods/src/source_control_impl.rs -- new open/resolve split
// still resolves the bubble through the RepoEphemeralStore facet:
let bubble_fetcher = self.bubble_fetcher_for_changeset(ctx.clone(), specifier.clone());
let repo_ctx = self.repo_impl(ctx, &commit.repo, authz, bubble_fetcher).await?;
// repo_impl takes FnOnce(RepoEphemeralStore) -> ... -- every new caller
// entrenches the bypass instead of fixing it.
```

**GOOD (bubble lookup moved into mononoke_api -- D130521669):**
```rust
// In mononoke_api/src/repo.rs -- the new shared primitive:
pub async fn with_bubble_of(mut self, specifier: &ChangesetSpecifier) -> Result<Self, MononokeError> {
    self.bubble_id = specifier.bubble_id(&self.ctx, self.repo.repo_ephemeral_store().clone()).await?;
    Ok(self)
}
// In scs_methods -- the ephemeral_blobstore dependency is gone:
let repo_ctx = self.repo_for_changeset(ctx, &commit.repo, authz, &changeset_specifier).await?;
```

## Recommendation

When adding new SCS functionality, first check if `mononoke_api` already exposes a suitable method on `RepoContext` or `ChangesetContext`. If not, add the primitive to `mononoke_api` first, then call it from SCS. This keeps the shared layer complete and ensures all servers benefit from the same logic. The `scs_methods` crate should ideally only depend on `mononoke_api`, not on individual facet crates. Do not build new structure on an existing bypass: if new SCS code needs a facet-consuming helper, add the `mononoke_api` primitive first -- it can land as the base of the stack, as D130521669 did under D125420313 -- then build on it.

## Evidence

- D93601164: Review comment -- "We don't want SCS (or Mononoke SLAPI server) to interface directly with the facets if we can avoid. That's one of the points of mononoke_api, is to abstract higher level operations into reusable primitives."
- Mononoke design guidelines document the layered architecture with mononoke_api as the shared primitive layer.
- D125420313: a new `OpenedCommit` open/resolve split built on `repo_impl`'s facet-based bubble lookup; missed at review, so D130521669 (`RepoContextBuilder::with_bubble_of`, dropping the `ephemeral_blobstore` dependency from `scs_methods`) had to be stacked underneath afterwards.
