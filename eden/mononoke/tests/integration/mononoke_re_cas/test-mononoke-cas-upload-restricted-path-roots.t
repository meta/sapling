# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

Verify that a full CAS upload skips the trees of restriction roots and of their
parents, from both restriction sources, including a restriction root nested
inside another one, and uploads every other tree and all files.

  $ . "${TEST_FIXTURES}/library.sh"
  $ setup_common_config
  $ export CAS_STORE_PATH="$TESTTMP"

"cfg" and "cfg/inner/cfg2" are config-backed restriction roots. Both mode also
reads `.slacl` files through the AclManifest.
  $ cat >> "$TESTTMP/mononoke-config/repos/repo/server.toml" <<EOF
  > [restricted_paths_config]
  > path_restriction_metadata = { "cfg" = { repo_region_acl = "REPO_REGION:cfg_acl" }, "cfg/inner/cfg2" = { repo_region_acl = "REPO_REGION:cfg2_acl" } }
  > acl_manifest_mode = "Both"
  > [restricted_paths_config.manifest_id_store_config]
  > use_manifest_id_cache = false
  > cache_update_interval_ms = 1000
  > EOF

"tent" and "tent/inner/tent2" carry a `.slacl`, so both are restriction roots;
"tent/inner" and "tent/inner/tent2/inner2" are not.
Contents are random because the test CAS backend is shared.
  $ testtool_drawdag -R repo --no-default-files --derive-all <<EOF
  > A
  > # modify: A public/readme "$(head -c 20 /dev/urandom | base64)"
  > # modify: A tent/.slacl "repo_region_acl = \"REPO_REGION:tent_acl\"\n"
  > # modify: A tent/inner/tent2/.slacl "repo_region_acl = \"REPO_REGION:tent2_acl\"\n"
  > # modify: A tent/inner/tent2/inner2/secret "$(head -c 20 /dev/urandom | base64)"
  > # modify: A cfg/inner/cfg2/inner2/secret "$(head -c 20 /dev/urandom | base64)"
  > # bookmark: A master_bookmark
  > EOF
  A=* (glob)

A full walk skips the four restriction root trees and their three parents: the
root tree, "tent/inner" and "cfg/inner". The `.slacl` contents are fixed, so the
shared backend may already hold them: the last walk pins the totals.
  $ mononoke_admin cas-store --repo-name repo upload --full -i $A
  [INFO] Skipped 4 restriction root trees for changeset * (glob)
  [INFO] Skipped 3 parent trees of restriction roots for changeset * (glob)
  [INFO] Upload completed. Upload stats: uploaded digests: *, already present digests: *, uploaded bytes: *, the largest uploaded blob: * (glob)

A trees-only walk starting at the outer `.slacl` root skips it, the nested root
and the nested root's parent, and covers the tree below them.
  $ mononoke_admin cas-store --repo-name repo upload --full --trees-only -i $A -p tent
  [INFO] Skipped 2 restriction root trees for changeset * (glob)
  [INFO] Skipped 1 parent trees of restriction roots for changeset * (glob)
  [INFO] Upload completed. Upload stats: uploaded digests: 0, already present digests: 1, uploaded bytes: 0 B, the largest uploaded blob: 0 B

The same for the config-backed roots.
  $ mononoke_admin cas-store --repo-name repo upload --full --trees-only -i $A -p cfg
  [INFO] Skipped 2 restriction root trees for changeset * (glob)
  [INFO] Skipped 1 parent trees of restriction roots for changeset * (glob)
  [INFO] Upload completed. Upload stats: uploaded digests: 0, already present digests: 1, uploaded bytes: 0 B, the largest uploaded blob: 0 B

A second full walk finds the three uploadable trees and the five files already
present, and skips the restriction roots and their parents again, so they are
still absent from CAS.
  $ mononoke_admin cas-store --repo-name repo upload --full -i $A
  [INFO] Skipped 4 restriction root trees for changeset * (glob)
  [INFO] Skipped 3 parent trees of restriction roots for changeset * (glob)
  [INFO] Upload completed. Upload stats: uploaded digests: 0, already present digests: 8, uploaded bytes: 0 B, the largest uploaded blob: 0 B

B adds a `.slacl` root under the unrestricted "open", so "open" is a parent tree
that is neither the root tree nor under a restriction root. Uploads of the
changes in B skip it and the root tree. A random comment makes the `.slacl` new
to the shared CAS backend, so the upload totals are exact.
  $ testtool_drawdag -R repo --no-default-files --derive-all <<EOF
  > B
  > |
  > A
  > # exists: A $A
  > # modify: B open/readme "$(head -c 20 /dev/urandom | base64)"
  > # modify: B open/tent3/.slacl "repo_region_acl = \"REPO_REGION:tent3_acl\"\n# $(head -c 20 /dev/urandom | base64)\n"
  > # modify: B open/tent3/secret "$(head -c 20 /dev/urandom | base64)"
  > # bookmark: B master_bookmark
  > EOF
  A=* (glob)
  B=* (glob)
  $ mononoke_admin cas-store --repo-name repo upload -i $B
  [INFO] Found 1 restricted path roots for changeset *: [NonRootMPath("open/tent3")] (glob)
  [INFO] Filtered out 1 of 6 entries (trees under restricted paths) for changeset * (glob)
  [INFO] Skipped 2 parent trees of restriction roots for changeset * (glob)
  [INFO] Upload completed. Upload stats: uploaded digests: 3, already present digests: 0, uploaded bytes: 129 B, the largest uploaded blob: 73 B
  $ mononoke_admin cas-store --repo-name repo upload --trees-only -i $B
  [INFO] Found 1 restricted path roots for changeset *: [NonRootMPath("open/tent3")] (glob)
  [INFO] Filtered out 1 of 6 entries (trees under restricted paths) for changeset * (glob)
  [INFO] Skipped 2 parent trees of restriction roots for changeset * (glob)
  [INFO] Upload completed. Upload stats: uploaded digests: 0, already present digests: 0, uploaded bytes: 0 B, the largest uploaded blob: 0 B
