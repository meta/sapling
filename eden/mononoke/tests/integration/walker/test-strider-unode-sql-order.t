# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"

Commit-graph SQL IDs out of topological order must not change the unodes
Strider reports for each commit, where chunked Walker loses some. See
test-strider-unode-history.t for the Walker arguments and
test-strider-chunked-sql-order.t for the ID swap. Unodes are named
`path@linknode`, as in test-strider-unode-history.t.

  $ setup_common_config "blob_files"
  $ cd "$TESTTMP"
  $ testtool_drawdag -R repo --derive-all --no-default-files <<'EOF'
  > A-B-C-D
  > # modify: A "dir1/file1" "one"
  > # modify: A "dir1/sub/file3" "three"
  > # modify: A "dir2/file2" "two"
  > # modify: A "live" "live"
  > # modify: B "dir1/sub/file3" "three-b"
  > # modify: C "live" "live-c"
  > # modify: D "dir2/file2" "two-d"
  > # bookmark: D master
  > EOF
  A=* (glob)
  B=* (glob)
  C=* (glob)
  D=* (glob)

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
  >     { i = index($0, ":"); id = substr($0, i + 1); if (i && id in name) $0 = substr($0, 1, i) name[id]; print }'
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
  >     --include-output-node-type Changeset \
  >     --include-output-node-type UnodeMapping \
  >     --include-output-node-type UnodeManifest \
  >     --include-output-node-type UnodeFile \
  >     --output-format Debug "$@" 2>&1
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
  >     --family core --family deleted-manifest --family unode \
  >     "$@" 2> strider.err
  > }
  $ strider_nodes() {
  >   grep -v ' status=' "$1" | cut -d' ' -f3 \
  >     | sed -e 's/^Mapping:unodes:/UnodeMapping:/' | identities
  > }
  $ completed() {
  >   grep 'Strider completed' strider.err | grep -cE 'status="?complete"?'
  > }

One chunk covering every commit is the reference. It names the unodes and
reaches every stored one.

  $ walker_scrub_all --chunk-size 100 > reference.out
  $ unode_names < reference.out > unode.names
  $ identities < reference.out > reference.nodes
  $ cat reference.nodes
  Changeset:A
  Changeset:B
  Changeset:C
  Changeset:D
  UnodeFile:dir1/file1@A
  UnodeFile:dir1/sub/file3@A
  UnodeFile:dir1/sub/file3@B
  UnodeFile:dir2/file2@A
  UnodeFile:dir2/file2@D
  UnodeFile:live@A
  UnodeFile:live@C
  UnodeManifest:(root)@A
  UnodeManifest:(root)@B
  UnodeManifest:(root)@C
  UnodeManifest:(root)@D
  UnodeManifest:dir1/sub@A
  UnodeManifest:dir1/sub@B
  UnodeManifest:dir1@A
  UnodeManifest:dir1@B
  UnodeManifest:dir2@A
  UnodeManifest:dir2@D
  UnodeMapping:A
  UnodeMapping:B
  UnodeMapping:C
  UnodeMapping:D
  $ ls "$TESTTMP/blobstore/blobs" | sed -nE \
  >     -e 's/^blob-repo0000\.manifestunode\.blake2\.([0-9a-f]+)$/UnodeManifest:\1/p' \
  >     -e 's/^blob-repo0000\.fileunode\.blake2\.([0-9a-f]+)$/UnodeFile:\1/p' \
  >   | unode_rename | LC_ALL=C sort > stored.nodes
  $ grep '^Unode\(Manifest\|File\):' reference.nodes | diff -u stored.nodes -

In SQL-ID order, chunks of two lose nothing. With scrub-all's families, every
commit and family is validated, and the union of each commit's additions is
exactly the reference.

  $ walker_scrub_all --chunk-size 2 | identities | diff -u reference.nodes -
  $ strider --chunk-size 2 > ordered.out
  $ grep -c ' status=validated$' ordered.out
  12
  $ strider_nodes ordered.out | diff -u reference.nodes -
  $ completed
  1
  $ LC_ALL=C sort ordered.out > ordered.sorted

Out of order: swap the SQL IDs of B and C, as a batched insert can, so child
C is enumerated before its parent B. Pointer columns are swapped too, so the
stored graph stays consistent.

  $ sqlite3 "$TESTTMP/monsql/sqlite_dbs" "SELECT count(*) FROM commit_graph_merge_parents; SELECT count(*) FROM commit_graph_subtree_sources"
  0
  0
  $ swap() {
  >   echo "$1 = CASE $1 WHEN 2 THEN 3 WHEN 3 THEN 2 ELSE $1 END"
  > }
  $ sqlite3 "$TESTTMP/monsql/sqlite_dbs" "
  >   UPDATE commit_graph_edges SET id = -1 WHERE id = 2;
  >   UPDATE commit_graph_edges SET id = 2 WHERE id = 3;
  >   UPDATE commit_graph_edges SET id = 3 WHERE id = -1;
  >   UPDATE commit_graph_edges SET $(swap p1_parent), $(swap merge_ancestor),
  >     $(swap skip_tree_parent), $(swap skip_tree_skew_ancestor),
  >     $(swap p1_linear_skew_ancestor), $(swap subtree_or_merge_ancestor),
  >     $(swap subtree_source_parent), $(swap subtree_source_skew_ancestor);"
  $ sqlite3 "$TESTTMP/monsql/sqlite_dbs" "SELECT e.id, lower(hex(e.cs_id)), lower(hex(p.cs_id)) FROM commit_graph_edges e LEFT JOIN commit_graph_edges p ON p.id = e.p1_parent ORDER BY e.id" \
  >   | names
  1|A|
  2|C|B
  3|B|A
  4|D|C

The one-chunk walk is unchanged. Chunked Walker misses B's dir1/sub and file3
unodes, as it misses a deleted-manifest node in
test-strider-chunked-sql-order.t: C's chunk runs first and defers B's dir1
manifest unode, which B's chunk then does not expand. Strider reports exactly
what it reported before the swap.

  $ walker_scrub_all --chunk-size 100 | identities | diff -u reference.nodes -
  $ walker_scrub_all --chunk-size 2 | identities | LC_ALL=C comm -23 reference.nodes -
  UnodeFile:dir1/sub/file3@B
  UnodeManifest:dir1/sub@B

  $ strider --chunk-size 2 > swapped.out
  $ LC_ALL=C sort swapped.out | diff -u ordered.sorted -
  $ completed
  1
