# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

A merge takes a restricted directory from its second parent. The directory is
unchanged relative to p2, so it is absent from the merge's bonsai file changes,
yet the sync diffs against p1 -- which never had it -- so the restricted tree is
in the upload set and must still be filtered out of it.

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

Only C touches the restricted directory. M merges C into B and changes nothing
of its own.
  $ drawdag --parent-order=name << 'EOF'
  > M          # B/feature = random:30
  > |\         # C/restricted/secret = random:30
  > B C        # A/public/readme = random:30
  > |/
  > A
  > EOF

B is p1 and C is p2. The whole test depends on this: the restricted directory
must arrive from the parent the sync does NOT diff against.
  $ hg log -r "p1($M)" -T "{node}" | grep $B > /dev/null
  $ hg log -r "p2($M)" -T "{node}" | grep $C > /dev/null

  $ hg push -r $A --to master_bookmark -q --create
  $ hg push -r $M --to master_bookmark -q

The bookmark move A -> M expands to [B, C, M], so one run covers both the linear
case and the merge case. Both C and M are filtered, for 7 digests: A's root,
public and public/readme; B's root and feature; C's restricted/secret; and M's
restricted/secret. Neither C's nor M's restricted tree is uploaded, and they are
the same tree -- an unchanged directory reuses its parent's node. Their root
trees have "restricted" as a child, so they are not uploaded either.
  $ mononoke_cas_sync repo 0
  [INFO] [execute{repo=repo}] Initiating mononoke RE CAS sync command execution
  [INFO] [execute{repo=repo}] using repo "repo" repoid RepositoryId(0) and CAS use case "source-control-testing"
  [INFO] [execute{repo=repo}] syncing log entries [1, 2] ...
  [INFO] [execute{repo=repo}] log entry BookmarkUpdateLogEntry * is a creation of bookmark (glob)
  [INFO] [execute{repo=repo}] Found 1 restricted path roots for changeset *: [NonRootMPath("restricted")] (glob)
  [INFO] [execute{repo=repo}] Filtered out 1 of 3 entries (trees under restricted paths) for changeset * (glob)
  [INFO] [execute{repo=repo}] Skipped 1 parent trees of restriction roots for changeset * (glob)
  [INFO] [execute{repo=repo}] Found 1 restricted path roots for changeset *: [NonRootMPath("restricted")] (glob)
  [INFO] [execute{repo=repo}] Filtered out 1 of 3 entries (trees under restricted paths) for changeset * (glob)
  [INFO] [execute{repo=repo}] Skipped 1 parent trees of restriction roots for changeset * (glob)
  [INFO] [execute{repo=repo}] log entries [1, 2] synced (4 commits uploaded, upload stats: uploaded digests: 7, already present digests: 0, uploaded bytes: *, the largest uploaded blob: *), took overall * sec, derivation checks took * sec (glob)
  [INFO] [execute{repo=repo}] queue size after processing: 0
  [INFO] [execute{repo=repo}] successful sync of entries [1, 2]
  [INFO] [execute{repo=repo}] Finished mononoke RE CAS sync command execution for repo repo
