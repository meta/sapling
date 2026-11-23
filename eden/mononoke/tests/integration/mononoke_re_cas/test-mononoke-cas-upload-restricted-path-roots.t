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

START is a time between A's and B's bookmark moves, where the scans into an
output folder at the end of this test start.
  $ sleep 1
  $ START="$(date -u '+%Y-%m-%d %H:%M:%SZ')"

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

The finder lists every parent tree in B. It looks them up in CAS only when
asked to, and none of them is there.
  $ mononoke_admin cas-store --repo-name repo acl-parent-trees -s now
  *:* unchecked * / (glob)
  *:* unchecked * /cfg/inner (glob)
  *:* unchecked * /open (glob)
  *:* unchecked * /tent/inner (glob)
  [INFO] Slice 1/1 from * to *: 1 commits, 4 parent trees, scanned in * (glob)
  [INFO] Scanned 1 commits of master_bookmark from * to * in 1 slices: 4 parent trees of restriction roots, counted once per slice (glob)
  $ mononoke_admin cas-store --repo-name repo acl-parent-trees -s now --check-presence
  *:* missing * / (glob)
  *:* missing * /cfg/inner (glob)
  *:* missing * /open (glob)
  *:* missing * /tent/inner (glob)
  [INFO] Slice 1/1 from * to *: 1 commits, 4 parent trees, scanned in * (glob)
  [INFO] Scanned 1 commits of master_bookmark from * to * in 1 slices: 4 parent trees of restriction roots, 0 of them in CAS, counted once per slice (glob)

With an output folder, each slice's trees go to a file named after the slice's
start and end in UTC. The scan from START covers B and its start commit A,
whose root tree is a parent too.
  $ mononoke_admin cas-store --repo-name repo acl-parent-trees -s "$START" --output-dir "$TESTTMP/out"
  [INFO] Slice 1/1 from * to *: 2 commits, 5 parent trees, scanned in * (glob)
  [INFO] Scanned 2 commits of master_bookmark from * to * in 1 slices: 5 parent trees of restriction roots, counted once per slice (glob)
  $ ls -A "$TESTTMP/out"
  .lock
  digests-*Z-*Z.txt (glob)
  $ cat "$TESTTMP"/out/digests-*.txt
  *:* unchecked * / (glob)
  *:* unchecked * / (glob)
  *:* unchecked * /cfg/inner (glob)
  *:* unchecked * /open (glob)
  *:* unchecked * /tent/inner (glob)

The same command again has nothing new to scan.
  $ mononoke_admin cas-store --repo-name repo acl-parent-trees -s "$START" --output-dir "$TESTTMP/out"
  [INFO] Resuming at *, where the slices in $TESTTMP/out from * end (glob)
  [INFO] Nothing to scan from * to * (glob)

MID is a time between B's and C's bookmark moves.
  $ MID="$(date -u '+%Y-%m-%d %H:%M:%SZ')"

After C moves the bookmark, the same command resumes where the last file ends.
It looks up its first commit a little before that, but not before START, so the
new slice starts at A again and lists A's and B's trees a second time. The
merge drops them.
  $ testtool_drawdag -R repo --no-default-files --derive-all <<EOF
  > C
  > |
  > B
  > # exists: B $B
  > # modify: C open/readme "$(head -c 20 /dev/urandom | base64)"
  > # bookmark: C master_bookmark
  > EOF
  B=* (glob)
  C=* (glob)
  $ mononoke_admin cas-store --repo-name repo acl-parent-trees -s "$START" --output-dir "$TESTTMP/out"
  [INFO] Resuming at *, where the slices in $TESTTMP/out from * end (glob)
  [INFO] Slice 1/1 from * to *: 3 commits, 7 parent trees, scanned in * (glob)
  [INFO] Scanned 3 commits of master_bookmark from * to * in 1 slices: 7 parent trees of restriction roots, counted once per slice (glob)
  $ ls "$TESTTMP/out" | wc -l
  2
  $ cat "$TESTTMP"/out/digests-*.txt | sort -u -k1,1 | wc -l
  7

Run again, it follows both files and has nothing new to scan.
  $ mononoke_admin cas-store --repo-name repo acl-parent-trees -s "$START" --output-dir "$TESTTMP/out"
  [INFO] Resuming at *, where the slices in $TESTTMP/out from * end (glob)
  [INFO] Nothing to scan from * to * (glob)

An end time before the bookmark's latest move ends the scan there: the scan from
START to MID covers B and A only, and its file is named after MID.
  $ mononoke_admin cas-store --repo-name repo acl-parent-trees -s "$START" -e "$MID" --output-dir "$TESTTMP/out2"
  [INFO] Slice 1/1 from * to *: 2 commits, 5 parent trees, scanned in * (glob)
  [INFO] Scanned 2 commits of master_bookmark from * to * in 1 slices: 5 parent trees of restriction roots, counted once per slice (glob)
  $ ls "$TESTTMP/out2" | sed "s/$(date -u -d "$MID" '+%Y%m%dT%H%M%SZ')/MID/"
  digests-*Z-MID.txt (glob)

A scan from another start time refuses the folder.
  $ mononoke_admin cas-store --repo-name repo acl-parent-trees -s now --output-dir "$TESTTMP/out"
  Error: $TESTTMP/out has slices that do not continue a scan from *, the earliest being digests-*. Use a new folder for a new scan (glob)
  [1]

`delete-digests` deletes the digests in its files, one per line. None of the
parent trees is in CAS, so the tree of "public" stands in for one uploaded
before uploads skipped them.
  $ cut -d' ' -f1 "$TESTTMP"/out/digests-*.txt | sort -u > "$TESTTMP/digests.txt"
  $ ROOT="$(grep -h ' /$' "$TESTTMP"/out/digests-*.txt | head -1 | cut -d' ' -f3)"
  $ mononoke_admin cas-store --repo-name repo tree-info -i "$ROOT" -c | grep '^public -> ' | cut -d' ' -f6 > "$TESTTMP/public.txt"

A dry run counts each digest once, finds only "public" in CAS, and records the
state of each digest.
  $ mononoke_admin cas-store --repo-name repo delete-digests --digests-per-second 1000 --dry-run --output-file "$TESTTMP/dry.txt" "$TESTTMP/digests.txt" "$TESTTMP/public.txt" "$TESTTMP/public.txt"
  [INFO] Read 8 digests from 3 files
  [INFO] source-control-testing: looked up 8 of 8 digests, 1 present
  $ wc -l < "$TESTTMP/dry.txt"
  8
  $ grep -v ' missing source-control-testing$' "$TESTTMP/dry.txt" | sed "s/$(cat "$TESTTMP/public.txt")/PUBLIC/"
  PUBLIC present source-control-testing

The rate limits the digests sent to CAS: at 1 per second, looking up 8
digests takes at least 7 seconds after the first.
  $ START_SECS=$(date +%s)
  $ mononoke_admin cas-store --repo-name repo delete-digests --digests-per-second 1 --dry-run --output-file "$TESTTMP/throttled.txt" "$TESTTMP/digests.txt" "$TESTTMP/public.txt"
  [INFO] Read 8 digests from 2 files
  [INFO] source-control-testing: looked up 8 of 8 digests, 1 present
  $ [ $(( $(date +%s) - START_SECS )) -ge 7 ] && echo throttled
  throttled

Delete it. Running again finds nothing to delete.
  $ mononoke_admin cas-store --repo-name repo delete-digests --digests-per-second 1000 --output-file "$TESTTMP/deleted.txt" "$TESTTMP/digests.txt" "$TESTTMP/public.txt"
  [INFO] Read 8 digests from 2 files
  [INFO] source-control-testing: looked up 8 of 8 digests, 1 present and deleted
  $ grep -v ' missing source-control-testing$' "$TESTTMP/deleted.txt" | sed "s/$(cat "$TESTTMP/public.txt")/PUBLIC/"
  PUBLIC deleted source-control-testing
  $ mononoke_admin cas-store --repo-name repo delete-digests --digests-per-second 1000 --output-file "$TESTTMP/again.txt" "$TESTTMP/digests.txt" "$TESTTMP/public.txt"
  [INFO] Read 8 digests from 2 files
  [INFO] source-control-testing: looked up 8 of 8 digests, 0 present and deleted
  $ grep -c ' missing source-control-testing$' "$TESTTMP/again.txt"
  8

The output file of an earlier run is not overwritten.
  $ mononoke_admin cas-store --repo-name repo delete-digests --digests-per-second 1000 --output-file "$TESTTMP/deleted.txt" "$TESTTMP/public.txt"
  [INFO] Read 1 digests from 1 files
  Error: Failed to create $TESTTMP/deleted.txt
  
  Caused by:
      File exists (os error 17)
  [1]

A full walk of A uploads "public" again, and nothing else was deleted.
  $ mononoke_admin cas-store --repo-name repo upload --full -i $A
  [INFO] Skipped 4 restriction root trees for changeset * (glob)
  [INFO] Skipped 3 parent trees of restriction roots for changeset * (glob)
  [INFO] Upload completed. Upload stats: uploaded digests: 1, already present digests: 7, uploaded bytes: *, the largest uploaded blob: * (glob)

A line that is not just a digest fails the whole run, so the lines of
`acl-parent-trees` need converting first.
  $ mononoke_admin cas-store --repo-name repo delete-digests --digests-per-second 1000 --output-file "$TESTTMP/bad.txt" "$TESTTMP/public.txt" "$TESTTMP"/out/digests-*.txt
  Error: $TESTTMP/out/digests-*.txt has a line that is not <blake3>:<size>: "*:* unchecked * /" (glob)
  [1]
  $ [ -e "$TESTTMP/bad.txt" ] || echo "no output file"
  no output file
