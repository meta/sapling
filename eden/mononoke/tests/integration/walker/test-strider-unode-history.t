# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"

Over full history, the union of what each commit added must be exactly the
Changeset, UnodeMapping, UnodeManifest and UnodeFile nodes that Walker visits
in the deployed scrub-all and scrub-unode-all jobs (deep, chunked, oldest
first), and must contain those it visits in the validate-all job (shallow,
from master). Walker runs with those jobs' graph and chunking arguments;
checkpoint, tailing, pack logging, replica and QPS arguments are omitted.

Unodes are named `path@linknode`, with `(root)` for root manifests. Walker
qualifies unode identities with UnodeFlags for fastlog and blame. Strider's
unode scope excludes both, so unodes are compared by id alone.

Each Walker job maps to the Strider families it covers: scrub-all to `core`,
`deleted-manifest` and `unode`; scrub-unode-all to `core` and `unode`; and
validate-all to scrub-all's families with `--operation validate`. Strider's
commit source is an SQL-ID range given by `--chunk-size` with an optional
`--repo-lower-bound`.

  $ setup_common_config "blob_files"
  $ cd "$TESTTMP"
  $ testtool_drawdag -R repo --derive-all --no-default-files <<'EOF'
  >     H
  >    / \
  >   F   G
  >   |   |
  >   D   E
  >    \ /
  >     C
  >     |
  >     B
  >     |
  >     A
  > # modify: A "dir1/file1" "one"
  > # modify: A "dir1/sub/file3" "three"
  > # modify: A "dir2/file2" "two"
  > # modify: A "live" "live"
  > # modify: B "dir1/file1" "one-b"
  > # modify: C "dir1/sub/file3" "three-c"
  > # modify: D "live" "live-d"
  > # modify: E "live" "live-e"
  > # modify: F "dir2/file2" "two-f"
  > # delete: G "dir1/file1"
  > # modify: H "live" "live-h"
  > # modify: H "dir1/file1" "one-h"
  > # modify: H "dir2/file2" "two-f"
  > # bookmark: H master
  > EOF
  A=* (glob)
  B=* (glob)
  C=* (glob)
  D=* (glob)
  E=* (glob)
  F=* (glob)
  G=* (glob)
  H=* (glob)

  $ BLOBS="$TESTTMP/blobstore/blobs"
  $ names() {
  >   sed -e "s/$A/A/g" -e "s/$B/B/g" -e "s/$C/C/g" -e "s/$D/D/g" \
  >     -e "s/$E/E/g" -e "s/$F/F/g" -e "s/$G/G/g" -e "s/$H/H/g"
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
  $ unode_parents() {
  >   names | awk -v f="$TESTTMP/unode.names" '
  >     BEGIN { while ((getline line < f) > 0) { split(line, a, " "); name[a[1]] = a[2] } }
  >     /^Node Unode(Manifest|File)\(/ && match($0, /parents: \[[^]]*\]/) {
  >       ps = substr($0, RSTART, RLENGTH); out = ""
  >       while (match(ps, /Blake2\([0-9a-f]+/)) {
  >         out = out " " name[substr(ps, RSTART + 7, RLENGTH - 7)]; ps = substr(ps, RSTART + RLENGTH)
  >       }
  >       if (out == "") next
  >       match($0, /inner: [A-Za-z]+UnodeId\(Blake2\([0-9a-f]+/)
  >       id = substr($0, RSTART, RLENGTH); sub(/.*\(/, "", id)
  >       t = ($0 ~ /^Node UnodeManifest/) ? "UnodeManifest" : "UnodeFile"
  >       print t ":" name[id] " <-" out
  >     }' | LC_ALL=C sort -u
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
  $ unode_output_types=(
  >   --include-output-node-type Changeset
  >   --include-output-node-type UnodeMapping
  >   --include-output-node-type UnodeManifest
  >   --include-output-node-type UnodeFile
  > )
  $ walker_scrub_all() {
  >   mononoke_walker scrub -q \
  >     -I deep -i bonsai -i all -I marker -x Blame \
  >     --chunk-direction OldestFirst --chunk-by-public Changeset \
  >     "${unode_output_types[@]}" \
  >     --output-format Debug "$@" 2>&1
  > }
  $ strider_scrub_unode_all=(--operation walk --family core --family unode)
  $ strider_scrub_all=("${strider_scrub_unode_all[@]}" --family deleted-manifest)
  $ strider_validate_all=(--operation validate --family core --family deleted-manifest
  >   --family unode)
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
  >     "$@" 2> strider.err
  > }
  $ strider_nodes() {
  >   grep -v ' status=' "$1" | cut -d' ' -f3 \
  >     | sed -e 's/^Mapping:unodes:/UnodeMapping:/' | identities
  > }
  $ completed() {
  >   grep 'Strider completed' strider.err | grep -cE 'status="?complete"?'
  > }

Name unodes from one unchunked walk. Every stored unode gets a name.

  $ walker_scrub_all --chunk-size 100 > reference.out
  $ unode_names < reference.out > unode.names
  $ ls "$BLOBS" | sed -nE \
  >     -e 's/^blob-repo0000\.manifestunode\.blake2\.([0-9a-f]+)$/UnodeManifest:\1/p' \
  >     -e 's/^blob-repo0000\.fileunode\.blake2\.([0-9a-f]+)$/UnodeFile:\1/p' \
  >   | unode_rename | LC_ALL=C sort > stored.nodes
  $ grep -c . stored.nodes
  28

Unode history. H merges divergent changes to the root, dir1 and live; its
dir2/file2 matches F's, so H reuses F's unodes for dir2.

  $ unode_parents < reference.out
  UnodeFile:dir1/file1@B <- dir1/file1@A
  UnodeFile:dir1/file1@H <- dir1/file1@B
  UnodeFile:dir1/sub/file3@C <- dir1/sub/file3@A
  UnodeFile:dir2/file2@F <- dir2/file2@A
  UnodeFile:live@D <- live@A
  UnodeFile:live@E <- live@A
  UnodeFile:live@H <- live@D live@E
  UnodeManifest:(root)@B <- (root)@A
  UnodeManifest:(root)@C <- (root)@B
  UnodeManifest:(root)@D <- (root)@C
  UnodeManifest:(root)@E <- (root)@C
  UnodeManifest:(root)@F <- (root)@D
  UnodeManifest:(root)@G <- (root)@E
  UnodeManifest:(root)@H <- (root)@F (root)@G
  UnodeManifest:dir1/sub@C <- dir1/sub@A
  UnodeManifest:dir1@B <- dir1@A
  UnodeManifest:dir1@C <- dir1@B
  UnodeManifest:dir1@G <- dir1@C
  UnodeManifest:dir1@H <- dir1@C dir1@G
  UnodeManifest:dir2@F <- dir2@A

Walker's one-chunk walk is the reference. It reaches every commit, every
mapping, and every stored unode.

  $ identities < reference.out > walker.nodes
  $ cat walker.nodes
  Changeset:A
  Changeset:B
  Changeset:C
  Changeset:D
  Changeset:E
  Changeset:F
  Changeset:G
  Changeset:H
  UnodeFile:dir1/file1@A
  UnodeFile:dir1/file1@B
  UnodeFile:dir1/file1@H
  UnodeFile:dir1/sub/file3@A
  UnodeFile:dir1/sub/file3@C
  UnodeFile:dir2/file2@A
  UnodeFile:dir2/file2@F
  UnodeFile:live@A
  UnodeFile:live@D
  UnodeFile:live@E
  UnodeFile:live@H
  UnodeManifest:(root)@A
  UnodeManifest:(root)@B
  UnodeManifest:(root)@C
  UnodeManifest:(root)@D
  UnodeManifest:(root)@E
  UnodeManifest:(root)@F
  UnodeManifest:(root)@G
  UnodeManifest:(root)@H
  UnodeManifest:dir1/sub@A
  UnodeManifest:dir1/sub@C
  UnodeManifest:dir1@A
  UnodeManifest:dir1@B
  UnodeManifest:dir1@C
  UnodeManifest:dir1@G
  UnodeManifest:dir1@H
  UnodeManifest:dir2@A
  UnodeManifest:dir2@F
  UnodeMapping:A
  UnodeMapping:B
  UnodeMapping:C
  UnodeMapping:D
  UnodeMapping:E
  UnodeMapping:F
  UnodeMapping:G
  UnodeMapping:H
  $ grep '^Unode\(Manifest\|File\):' walker.nodes | diff -u stored.nodes -

Chunks of three lose nothing here. With one commit per chunk every unode
parent edge leaves its chunk, and Walker's coverage still does not change.

  $ walker_scrub_all --chunk-size 3 | identities | diff -u walker.nodes -
  $ walker_scrub_all --chunk-size 1 | identities | diff -u walker.nodes -

With scrub-all's families, every commit and family is validated, and the union
of each commit's additions is exactly the reference.

  $ strider "${strider_scrub_all[@]}" --chunk-size 3 > all.out
  $ grep -c ' status=validated$' all.out
  24
  $ strider_nodes all.out | diff -u walker.nodes -
  $ completed
  1
  $ LC_ALL=C sort all.out > all.sorted

Merge H adds only what differs from both parents: it changes dir1/file1 and
live, and reuses F's dir2 and dir2/file2 unodes.

  $ grep "^$H unode " all.out | grep -v ' status=' > h.out
  $ strider_nodes h.out
  UnodeFile:dir1/file1@H
  UnodeFile:live@H
  UnodeManifest:(root)@H
  UnodeManifest:dir1@H
  UnodeMapping:H

Each commit reports the same additions when every commit is its own chunk.

  $ strider "${strider_scrub_all[@]}" --chunk-size 1 > single.out
  $ LC_ALL=C sort single.out | diff -u all.sorted -
  $ completed
  1

Resume after C, as a pass restarting from a checkpoint does. Walker expands
only unodes linked to D-H. It still loads, but does not expand, unodes linked
to A-C that a tree entry or parent edge from the range reaches.

  $ LOWER=$(( $(sqlite3 "$TESTTMP/monsql/sqlite_dbs" "SELECT id FROM commit_graph_edges WHERE cs_id = x'$C'") + 1 ))
  $ walker_scrub_all --chunk-size 3 --repo-lower-bound "$LOWER" > walker-resume.out
  $ identities < walker-resume.out > walker-resume.nodes
  $ grep -E '^(Changeset|UnodeMapping):' walker-resume.nodes
  Changeset:D
  Changeset:E
  Changeset:F
  Changeset:G
  Changeset:H
  UnodeMapping:D
  UnodeMapping:E
  UnodeMapping:F
  UnodeMapping:G
  UnodeMapping:H
  $ grep 'NodeData: Some(OutsideChunk)' walker-resume.out | identities
  UnodeFile:dir1/file1@B
  UnodeFile:dir2/file2@A
  UnodeFile:live@A
  UnodeManifest:(root)@C
  UnodeManifest:dir1/sub@C
  UnodeManifest:dir1@C
  UnodeManifest:dir2@A

Strider audits only D to H and reports the same additions for them as the full
run. The unodes Walker loads below the range were added by A to C, so Strider
does not report them.

  $ strider "${strider_scrub_all[@]}" --chunk-size 3 --repo-lower-bound "$LOWER" > resume.out
  $ grep -E "^($D|$E|$F|$G|$H) " all.sorted > expected-resume
  $ LC_ALL=C sort resume.out | diff -u expected-resume -
  $ grep 'NodeData: Some(OutsideChunk)' walker-resume.out | identities > outside.nodes
  $ strider_nodes resume.out | LC_ALL=C comm -12 outside.nodes -
  $ completed
  1

scrub-unode-all chunks by UnodeMapping rather than Changeset, and selects only
unodes, fastlog and file content among the data reachable from a changeset. It
reaches the same nodes as scrub-all.

  $ walker_scrub_unode_all() {
  >   mononoke_walker scrub -q \
  >     -I deep -i bonsai -i FileContent -i derived_unodes \
  >     -i derived_fastlog -i derived_blame -x Blame \
  >     --chunk-direction OldestFirst --chunk-by-public UnodeMapping \
  >     "${unode_output_types[@]}" \
  >     --output-format Debug "$@" 2>&1
  > }
  $ walker_scrub_unode_all --chunk-size 3 > walker-unode.out
  $ grep -E 'Walking (edge|node) types' walker-unode.out
  [INFO] Walking edge types [ChangesetToBonsaiParent, ChangesetToFileContent, ChangesetToUnodeMapping, FastlogBatchToChangeset, FastlogBatchToPreviousBatch, FastlogDirToChangeset, FastlogDirToPreviousBatch, FastlogFileToChangeset, FastlogFileToPreviousBatch, UnodeFileToFastlogFile, UnodeFileToFileContent, UnodeFileToLinkedChangeset, UnodeFileToUnodeFileParent, UnodeManifestToFastlogDir, UnodeManifestToLinkedChangeset, UnodeManifestToUnodeFileChild, UnodeManifestToUnodeManifestChild, UnodeManifestToUnodeManifestParent, UnodeMappingToRootUnodeManifest]
  [INFO] Walking node types [Changeset, FastlogBatch, FastlogDir, FastlogFile, FileContent, UnodeFile, UnodeManifest, UnodeMapping]
  $ identities < walker-unode.out | diff -u walker.nodes -
  $ walker_scrub_unode_all --chunk-size 1 | identities | diff -u walker.nodes -

Strider has no profiles. Selecting only scrub-unode-all's families leaves the
core and unode output unchanged.

  $ strider "${strider_scrub_unode_all[@]}" --chunk-size 3 > unode-all.out
  $ grep -v ' deleted-manifest ' all.sorted > expected-unode-all
  $ LC_ALL=C sort unode-all.out | diff -u expected-unode-all -
  $ completed
  1

validate-all walks master's unode tree but no history: no parent unodes and
no linked changesets. Strider instead validates what each commit added, so
over full history it covers master's tree.

  $ mononoke_walker scrub -q -I shallow -i all -x Blame --bookmark master \
  >   "${unode_output_types[@]}" --output-format Debug 2>&1 | identities > walker-shallow.nodes
  $ cat walker-shallow.nodes
  Changeset:H
  UnodeFile:dir1/file1@H
  UnodeFile:dir1/sub/file3@C
  UnodeFile:dir2/file2@F
  UnodeFile:live@H
  UnodeManifest:(root)@H
  UnodeManifest:dir1/sub@C
  UnodeManifest:dir1@H
  UnodeManifest:dir2@F
  UnodeMapping:H

  $ strider "${strider_validate_all[@]}" --chunk-size 3 > validate.out
  $ LC_ALL=C sort validate.out | diff -u all.sorted -
  $ strider_nodes validate.out | LC_ALL=C comm -23 walker-shallow.nodes -
  $ grep 'Strider completed' strider.err | grep -E 'status="?complete"?' | grep -c 'validation_failed=0'
  1
