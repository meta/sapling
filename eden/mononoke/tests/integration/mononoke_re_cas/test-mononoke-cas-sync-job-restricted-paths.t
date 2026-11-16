# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

Verify that CAS sync skips uploading trees under restricted paths, and trees
with a restriction root as a direct child, while still uploading the file content
beneath them.

  $ . "${TEST_FIXTURES}/library.sh"
  $ setup_common_config
  $ export CAS_STORE_PATH="$TESTTMP"
  $ setconfig drawdag.defaultfiles=false

Configure restricted paths: "restricted" directory is restricted.
  $ cd "$TESTTMP/mononoke-config"
  $ cat >> repos/repo/server.toml <<EOF
  > [restricted_paths_config]
  > path_restriction_metadata = { "restricted" = { repo_region_acl = "SERVICE_IDENTITY:restricted_acl" } }
  > [restricted_paths_config.manifest_id_store_config]
  > use_manifest_id_cache = false
  > cache_update_interval_ms = 1000
  > EOF

  $ start_and_wait_for_mononoke_server
  $ hg clone -q mono:repo repo
  $ cd repo

Create commits with files under both restricted and unrestricted paths. C only
changes the unrestricted sibling of "restricted".
  $ drawdag << EOS
  > C # C/public/readme = random:30
  > |
  > B # B/restricted/secret = random:30
  > |
  > A # A/public/readme = random:30
  > EOS

  $ hg goto $A -q
  $ hg push -r . --to master_bookmark -q --create

  $ hg goto $B -q
  $ hg push -r . --to master_bookmark -q

Sync all bookmark moves. The sync should detect restricted path roots and
skip the tree entries under "restricted/", while still uploading the file
content. The root tree of B has "restricted" as a child, so it is skipped too.
Without filtering, the sync would upload 6 digests (3 per commit). With
filtering, both trees are skipped from commit B, resulting in 4.
  $ mononoke_cas_sync repo 0
  [INFO] [execute{repo=repo}] Initiating mononoke RE CAS sync command execution
  [INFO] [execute{repo=repo}] using repo "repo" repoid RepositoryId(0) and CAS use case "source-control-testing"
  [INFO] [execute{repo=repo}] syncing log entries [1, 2] ...
  [INFO] [execute{repo=repo}] log entry BookmarkUpdateLogEntry * is a creation of bookmark (glob)
  [INFO] [execute{repo=repo}] Found 1 restricted path roots for changeset *: [NonRootMPath("restricted")] (glob)
  [INFO] [execute{repo=repo}] Filtered out 1 of 3 entries (trees under restricted paths) for changeset * (glob)
  [INFO] [execute{repo=repo}] Skipped 1 parent trees of restriction roots for changeset * (glob)
  [INFO] [execute{repo=repo}] log entries [1, 2] synced (2 commits uploaded, upload stats: uploaded digests: 4, already present digests: 0, uploaded bytes: *, the largest uploaded blob: *), took overall * sec, derivation checks took * sec (glob)
  [INFO] [execute{repo=repo}] queue size after processing: 0
  [INFO] [execute{repo=repo}] successful sync of entries [1, 2]
  [INFO] [execute{repo=repo}] Finished mononoke RE CAS sync command execution for repo repo

C changes no tree under "restricted/", yet its root tree still has "restricted"
as a child, so only the "public" tree and public/readme are uploaded.
  $ hg goto $C -q
  $ hg push -r . --to master_bookmark -q
  $ mononoke_cas_sync repo 2
  [INFO] [execute{repo=repo}] Initiating mononoke RE CAS sync command execution
  [INFO] [execute{repo=repo}] using repo "repo" repoid RepositoryId(0) and CAS use case "source-control-testing"
  [INFO] [execute{repo=repo}] syncing log entries [3] ...
  [INFO] [execute{repo=repo}] Skipped 1 parent trees of restriction roots for changeset * (glob)
  [INFO] [execute{repo=repo}] log entries [3] synced (1 commits uploaded, upload stats: uploaded digests: 2, already present digests: 0, uploaded bytes: *, the largest uploaded blob: *), took overall * sec, derivation checks took * sec (glob)
  [INFO] [execute{repo=repo}] queue size after processing: 0
  [INFO] [execute{repo=repo}] successful sync of entries [3]
  [INFO] [execute{repo=repo}] Finished mononoke RE CAS sync command execution for repo repo

Validate unrestricted content for commit A is fully present in CAS.
  $ mononoke_admin cas-store --repo-name repo upload --full -i $A
  [INFO] Upload completed. Upload stats: uploaded digests: 0, already present digests: 3, uploaded bytes: 0 B, the largest uploaded blob: 0 B

Validate that the file content under "restricted/" WAS uploaded by the sync:
a blobs-only walk of the restricted subtree finds the file already present.
We use random:30 content because the test CAS backend is shared --
deterministic content may already exist from prior runs.
  $ mononoke_admin cas-store --repo-name repo upload --full --blobs-only -i $B -p restricted
  [INFO] Upload completed. Upload stats: uploaded digests: 0, already present digests: 1, uploaded bytes: 0 B, the largest uploaded blob: 0 B

Validate that the admin tool applies the same policy: a trees-only walk of the
restricted subtree skips the "restricted" tree instead of uploading it.
  $ mononoke_admin cas-store --repo-name repo upload --full --trees-only -i $B -p restricted
  [INFO] Skipped 1 restriction root trees for changeset * (glob)
  [INFO] Upload completed. Upload stats: uploaded digests: 0, already present digests: 0, uploaded bytes: 0 B, the largest uploaded blob: 0 B

A full walk of commit B finds everything the sync uploaded already present
("public" tree, public/readme, restricted/secret) and skips the "restricted"
tree and the root tree, so they stay absent from CAS.
  $ mononoke_admin cas-store --repo-name repo upload --full -i $B
  [INFO] Skipped 1 restriction root trees for changeset * (glob)
  [INFO] Skipped 1 parent trees of restriction roots for changeset * (glob)
  [INFO] Upload completed. Upload stats: uploaded digests: 0, already present digests: 3, uploaded bytes: 0 B, the largest uploaded blob: 0 B

The same holds for commit C.
  $ mononoke_admin cas-store --repo-name repo upload --full -i $C
  [INFO] Skipped 1 restriction root trees for changeset * (glob)
  [INFO] Skipped 1 parent trees of restriction roots for changeset * (glob)
  [INFO] Upload completed. Upload stats: uploaded digests: 0, already present digests: 3, uploaded bytes: 0 B, the largest uploaded blob: 0 B

Uploading the restricted file by path is allowed (content is not ACL'd).
  $ mononoke_admin cas-store --repo-name repo upload --full -i $B -p restricted/secret
  [INFO] Upload completed. Upload stats: uploaded digests: 0, already present digests: 1, uploaded bytes: 0 B, the largest uploaded blob: 0 B
