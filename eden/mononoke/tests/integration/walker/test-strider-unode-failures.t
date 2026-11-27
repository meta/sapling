# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"

Absent, missing or unreadable unode data. Walker shows where
each damaged node belongs. Strider audits the unode family alone and must
report the failure against the commit that added the node, keep the other
commits validated, and exit nonzero. See test-strider-unode-history.t for the
Walker arguments. Unodes are named `path@linknode`, as in
test-strider-unode-history.t.

  $ setup_common_config "blob_files"
  $ cd "$TESTTMP"
  $ testtool_drawdag -R repo --derive-all --no-default-files <<'EOF'
  > A-B-C-D
  > # modify: A "dir1/file1" "one"
  > # modify: A "dir1/sub/file3" "three"
  > # modify: A "dir2/file2" "two"
  > # modify: A "live" "live"
  > # modify: B "dir2/file2" "two-b"
  > # modify: C "live" "live-c"
  > # modify: D "live" "live-d"
  > # bookmark: D master
  > EOF
  A=* (glob)
  B=* (glob)
  C=* (glob)
  D=* (glob)

  $ BLOBS="$TESTTMP/blobstore/blobs"
  $ names() {
  >   sed -e "s/$A/A/g" -e "s/$B/B/g" -e "s/$C/C/g" -e "s/$D/D/g"
  > }
  $ unode_names() {
  >   names | awk '
  >     function id_in(s, re,   v) {
  >       if (!match(s, re)) return ""
  >       v = substr(s, RSTART, RLENGTH); sub(/.*\(/, "", v); return v
  >     }
  >     function assign(id, path,   i) {
  >       if (id in path_of) return
  >       path_of[id] = path
  >       for (i = 1; i <= nchild[id]; i++)
  >         assign(cid[id, i], path == "(root)" ? cname[id, i] : path "/" cname[id, i])
  >     }
  >     /^Node UnodeMapping\(/ {
  >       r = id_in($0, "ManifestUnodeId\\(Blake2\\([0-9a-f]+")
  >       if (r != "") root[r] = 1
  >     }
  >     /^Node Unode(Manifest|File)\(/ && /linknode: / {
  >       id = id_in($0, "inner: [A-Za-z]+UnodeId\\(Blake2\\([0-9a-f]+")
  >       link[id] = id_in($0, "linknode: ChangesetId\\(Blake2\\([A-Za-z0-9]+")
  >       rest = $0
  >       while (match(rest, /MPathElement\("[^"]*"\): (Directory|File)\([A-Za-z]+UnodeId\(Blake2\([0-9a-f]+/)) {
  >         e = substr(rest, RSTART, RLENGTH); rest = substr(rest, RSTART + RLENGTH)
  >         n = e; sub(/^MPathElement\("/, "", n); sub(/".*/, "", n)
  >         c = e; sub(/.*\(/, "", c)
  >         nchild[id]++; cname[id, nchild[id]] = n; cid[id, nchild[id]] = c
  >       }
  >     }
  >     END {
  >       for (r in root) assign(r, "(root)")
  >       for (id in link) {
  >         if (!(id in path_of)) { print "unnamed unode " id > "/dev/stderr"; continue }
  >         nm = path_of[id] "@" link[id]
  >         if (nm in seen) print "duplicate unode name " nm > "/dev/stderr"
  >         seen[nm] = 1
  >         print id, nm
  >       }
  >     }'
  > }
  $ unode_rename() {
  >   awk -v f="$TESTTMP/unode.names" '
  >     BEGIN { while ((getline line < f) > 0) { split(line, a, " "); name[a[1]] = a[2] } }
  >     { for (id in name) gsub(id, name[id]); print }'
  > }
  $ identities() {
  >   sed -nE \
  >     -e 's/^Node Changeset\(ChangesetKey \{ inner: ChangesetId\(Blake2\(([0-9a-f]+)\)\).*/Changeset:\1/p' \
  >     -e 's/^Node UnodeMapping\(ChangesetId\(Blake2\(([0-9a-f]+)\)\)\).*/UnodeMapping:\1/p' \
  >     -e 's/^Node UnodeManifest\(UnodeKey \{ inner: ManifestUnodeId\(Blake2\(([0-9a-f]+)\)\).*/UnodeManifest:\1/p' \
  >     -e 's/^Node UnodeFile\(UnodeKey \{ inner: FileUnodeId\(Blake2\(([0-9a-f]+)\)\).*/UnodeFile:\1/p' \
  >     -e 's/^(Changeset|UnodeMapping|UnodeManifest|UnodeFile):([0-9a-f]+)(:.*)?$/\1:\2/p' \
  >   | names | unode_rename | LC_ALL=C sort -u
  > }
  $ walker_scrub_all() {
  >   mononoke_walker scrub -q \
  >     -I deep -i bonsai -i all -I marker -x Blame \
  >     --chunk-direction OldestFirst --chunk-by-public Changeset \
  >     --chunk-size "${WALKER_CHUNK_SIZE:-2}" \
  >     --include-output-node-type Changeset \
  >     --include-output-node-type UnodeMapping \
  >     --include-output-node-type UnodeManifest \
  >     --include-output-node-type UnodeFile \
  >     --output-format Debug "$@" > walker.out 2>&1
  > }
  $ walker_failure() {
  >   local class=unknown
  >   grep -q ' is missing$' walker.out && class=missing_target
  >   grep -q 'error while deserializing' walker.out && class=unreadable_target
  >   grep -m1 'Could not step to' walker.out | sed -nE \
  >     -e "s/.*target: UnodeManifest\(UnodeKey \{ inner: ManifestUnodeId\(Blake2\(([0-9a-f]+)\)\).*/$class UnodeManifest:\1/p" \
  >     -e "s/.*target: UnodeFile\(UnodeKey \{ inner: FileUnodeId\(Blake2\(([0-9a-f]+)\)\).*/$class UnodeFile:\1/p" \
  >   | names | unode_rename
  > }
  $ strider() {
  >   render_mononoke_config
  >   GLOG_minloglevel=5 "$MONONOKE_STRIDER" \
  >     "${CACHE_ARGS[@]}" \
  >     "${COMMON_ARGS[@]}" \
  >     --repo-id "$REPOID" \
  >     --config-tier "$MONONOKE_CONFIG_TIER" \
  >     --tracing-test-format \
  >     --max-concurrency 4 \
  >     --max-loads-per-commit 4 \
  >     --print-visited-nodes \
  >     --family unode \
  >     --chunk-size 2 \
  >     "$@" > strider.out 2> strider.err
  > }
  $ unode_id() {
  >   awk -v n="$1" '$2 == n { print $1 }' "$TESTTMP/unode.names"
  > }
  $ failures() {
  >   grep 'Strider family incomplete' strider.err | sed -nE \
  >     's/.*changeset_id="?([0-9a-f]+)"? family="?([a-z-]+)"? failure_stage="?([a-z]+)"? failure_class="?([a-z_]+)"? object="?([^" ]+)"? storage_key="?([^" ]+)"?.*/\1 \2 \3 \4 \5 \6/p' \
  >   | names | unode_rename | LC_ALL=C sort
  > }

The number of commit and family results, those not validated, and each
failure as commit, family, stage, class, object and storage key.

  $ outcomes() {
  >   grep -c ' status=' strider.out
  >   grep ' status=' strider.out | grep -v ' status=validated$' | names | LC_ALL=C sort
  >   failures
  > }

Name unodes from one unchunked walk, then walk in chunks of two.

  $ WALKER_CHUNK_SIZE=100 walker_scrub_all
  $ unode_names < walker.out > unode.names
  $ walker_scrub_all
  $ identities < walker.out > walker.nodes
  $ cat walker.nodes
  Changeset:A
  Changeset:B
  Changeset:C
  Changeset:D
  UnodeFile:dir1/file1@A
  UnodeFile:dir1/sub/file3@A
  UnodeFile:dir2/file2@A
  UnodeFile:dir2/file2@B
  UnodeFile:live@A
  UnodeFile:live@C
  UnodeFile:live@D
  UnodeManifest:(root)@A
  UnodeManifest:(root)@B
  UnodeManifest:(root)@C
  UnodeManifest:(root)@D
  UnodeManifest:dir1/sub@A
  UnodeManifest:dir1@A
  UnodeManifest:dir2@A
  UnodeManifest:dir2@B
  UnodeMapping:A
  UnodeMapping:B
  UnodeMapping:C
  UnodeMapping:D

The dir1/sub manifest unode and its file3 unode, created in A, are shared by
every commit's tree. B's dir2/file2 unode has A's as its only parent.

  $ SHARED=$(unode_id dir1/sub@A)
  $ FILE3=$(unode_id dir1/sub/file3@A)
  $ PARENT=$(unode_id dir2/file2@A)

An absent mapping in mid-history ends that branch normally, and the walk
completes. B's dir2/file2 unode stays reachable through the dir2 manifest unode
that C and D share with B. Chunked Walker defers that manifest outside B's
chunk and never expands it, so it misses B's file unode. Strider counts B's own
absent mapping and otherwise ignores it, as Walker does, but C was derived from
B's mapping, so C reports it missing.

  $ mv "$BLOBS/blob-repo0000.derived_root_unode_v2.$B" "$TESTTMP/mapping-B"
  $ WALKER_CHUNK_SIZE=100 walker_scrub_all
  $ identities < walker.out > reference-absent.nodes
  $ diff -u walker.nodes reference-absent.nodes
  $ walker_scrub_all
  $ identities < walker.out | LC_ALL=C comm -23 reference-absent.nodes -
  UnodeFile:dir2/file2@B
  $ strider
  [1]
  $ outcomes
  4
  B unode status=not_derived
  C unode status=incomplete
  C unode enumerate missing Mapping:unodes:B derived_root_unode_v2.B
  $ mv "$TESTTMP/mapping-B" "$BLOBS/blob-repo0000.derived_root_unode_v2.$B"

A missing shared manifest unode is fatal. Only A, which added it, reads it:
later commits compare dir1 by id. Strider reports it by key as `missing` at
`load` against A.

  $ mv "$BLOBS/blob-repo0000.manifestunode.blake2.$SHARED" "$TESTTMP/shared-manifest"
  $ walker_scrub_all
  [1]
  $ walker_failure
  missing_target UnodeManifest:dir1/sub@A
  $ strider
  [1]
  $ outcomes
  4
  A unode status=incomplete
  A unode load missing UnodeManifest:dir1/sub@A manifestunode.blake2.dir1/sub@A
  $ mv "$TESTTMP/shared-manifest" "$BLOBS/blob-repo0000.manifestunode.blake2.$SHARED"

A shared file unode that is present but undecodable is an unreadable target.
Strider reports it by key against A, which added it.

  $ mv "$BLOBS/blob-repo0000.fileunode.blake2.$FILE3" "$TESTTMP/file3"
  $ printf 'garbage' > "$BLOBS/blob-repo0000.fileunode.blake2.$FILE3"
  $ walker_scrub_all
  [1]
  $ walker_failure
  unreadable_target UnodeFile:dir1/sub/file3@A
  $ strider
  [1]
  $ outcomes
  4
  A unode status=incomplete
  A unode load unreadable UnodeFile:dir1/sub/file3@A fileunode.blake2.dir1/sub/file3@A
  $ mv "$TESTTMP/file3" "$BLOBS/blob-repo0000.fileunode.blake2.$FILE3"

Resuming after A, Walker reaches A's dir2/file2 unode only through the parent
edge of B's. It loads that unode although A is below the range, so a missing
parent is fatal.

  $ LOWER=$(( $(sqlite3 "$TESTTMP/monsql/sqlite_dbs" "SELECT id FROM commit_graph_edges WHERE cs_id = x'$A'") + 1 ))
  $ mv "$BLOBS/blob-repo0000.fileunode.blake2.$PARENT" "$TESTTMP/parent"
  $ walker_scrub_all --repo-lower-bound "$LOWER"
  [1]
  $ walker_failure
  missing_target UnodeFile:dir2/file2@A
Strider audits only B to D. B's diff compares A's dir2/file2 entry by id
without loading it, and damage below the range is not a parity target, so
Strider completes.

  $ strider --repo-lower-bound "$LOWER"
  $ outcomes
  3
  $ grep 'Strider completed' strider.err | grep -cE 'status="?complete"?'
  1
  $ mv "$TESTTMP/parent" "$BLOBS/blob-repo0000.fileunode.blake2.$PARENT"
