# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"

Generate a small history: 50 commits with deletions under nested directories,
merges whose branches delete different files, and one directory of 2,100
entries that one commit deletes and a later commit partly restores. Manual
benchmark runs use the same generator with larger parameters.

  $ setup_common_config "blob_files"
  $ cd "$TESTTMP"
  $ bash "${TEST_FIXTURES}/walker/strider-benchmark-history.sh" \
  >   --commits 50 --merge-interval 10 --large-dir-size 2100 --depth 3 > history.dag
  $ testtool_drawdag -R repo --derive-all < history.dag 2> drawdag.out
  $ sed -nE 's/^C[0-9]{5}(_[a-z]+)?=.*/commit\1/p' drawdag.out | sort | uniq -c
       35 commit
        1 commit_bigadd
        1 commit_bigdel
        1 commit_bigrestore
        4 commit_left
        4 commit_merge
        4 commit_right

Parity gate. Over the whole SQL-ID range, Walker walks every commit's
deleted-manifest tree oldest first in chunks, using only tree edges, and
Strider audits what each commit added. The union of Strider's manifest nodes
must equal Walker's node set. Mapping records are left out, and so are
sharded-map nodes, for which Walker has no node type. Walker must expand every
node it reaches rather than leave it outside its chunk.

  $ mononoke_walker scrub -q --scheduled-max 4 \
  >   -p DeletedManifestV2Mapping -d OldestFirst --chunk-size 20 \
  >   -i DeletedManifestV2Mapping -i DeletedManifestV2 \
  >   -I DeletedManifestV2MappingToRootDeletedManifestV2 \
  >   -I DeletedManifestV2ToDeletedManifestV2Child \
  >   --include-output-node-type DeletedManifestV2 \
  >   --output-format Debug > walker-gate.out 2>&1
  $ grep -E '^\[INFO\] Walking (edge|node) types' walker-gate.out
  [INFO] Walking edge types [DeletedManifestV2MappingToRootDeletedManifestV2, DeletedManifestV2ToDeletedManifestV2Child]
  [INFO] Walking node types [DeletedManifestV2, DeletedManifestV2Mapping]
  $ grep -c 'Starting chunk' walker-gate.out
  3
  $ ! grep -q OutsideChunk walker-gate.out
  $ sed -nE 's/^Node DeletedManifestV2\(DeletedManifestV2Id\(Blake2\(([0-9a-f]+)\)\)\).*/DeletedManifestV2:\1/p' \
  >   walker-gate.out | LC_ALL=C sort -u > walker.nodes

  $ strider() {
  >   render_mononoke_config
  >   GLOG_minloglevel=5 "$MONONOKE_STRIDER" \
  >     "${CACHE_ARGS[@]}" \
  >     "${COMMON_ARGS[@]}" \
  >     --repo-id "$REPOID" \
  >     --config-tier "$MONONOKE_CONFIG_TIER" \
  >     --tracing-test-format \
  >     --family deleted-manifest \
  >     --operation validate \
  >     --chunk-size 20 \
  >     --max-concurrency 4 \
  >     --max-loads-per-commit 4 \
  >     "$@"
  > }
  $ strider --print-visited-nodes > strider-gate.out 2> strider-gate.err
  $ grep ' status=' strider-gate.out | cut -d' ' -f2- | sort | uniq -c
       50 deleted-manifest status=validated
  $ grep -v ' status=' strider-gate.out | cut -d' ' -f3 | grep '^DeletedManifestV2:' |
  >   LC_ALL=C sort -u > strider.nodes
  $ diff -u walker.nodes strider.nodes
  $ wc -l < walker.nodes
  285

The commit that deletes the large directory adds six manifest nodes: the root,
the directory, the leaf that every file it deletes shares, and the three
subdirectories. The directory keeps its entries in stored shards, so the last
four nodes are reachable only through a shard, and Walker reaches them too. Of
its four shards, two hold the same entries relative to their prefixes and are
one object.

  $ BIGDEL=$(sed -n 's/^C[0-9]*_bigdel=//p' drawdag.out)
  $ grep "^$BIGDEL " strider-gate.out | grep -v ' status=' | cut -d' ' -f3 | cut -d: -f1 | sort | uniq -c
        6 DeletedManifestV2
        3 DeletedManifestV2MapNode
        1 Mapping
  $ grep "^$BIGDEL deleted-manifest DeletedManifestV2:" strider-gate.out | cut -d' ' -f3 |
  >   LC_ALL=C sort | LC_ALL=C comm -23 - walker.nodes

The restore leaves 1,900 deleted entries, few enough for the directory's map to
be inline again, so the commit adds only a root and the directory.

  $ RESTORE=$(sed -n 's/^C[0-9]*_bigrestore=//p' drawdag.out)
  $ grep "^$RESTORE " strider-gate.out | grep -v ' status=' | cut -d' ' -f3 | cut -d: -f1 | sort | uniq -c
        2 DeletedManifestV2
        1 Mapping

Measure Walker and Strider validation over the same range with the same
selection. The gate passed, so the samples may be recorded as semantic matches.
This smoke fixture does not control the filesystem cache, so it records that
condition without treating the measurements as a performance conclusion.

  $ HARNESS="${TEST_FIXTURES}/walker/strider-parity-benchmark.sh"
  $ render_mononoke_config
  $ bash "$HARNESS" \
  >   --implementation walker \
  >   --output-prefix "$TESTTMP/benchmark/walker" \
  >   --concurrency 4 \
  >   --visited-capacity unbounded \
  >   --cache-condition unspecified-smoke \
  >   --semantic-result match \
  >   -- env GLOG_minloglevel=5 "$MONONOKE_WALKER" \
  >     "${CACHE_ARGS[@]}" \
  >     "${COMMON_ARGS[@]}" \
  >     --repo-id "$REPOID" \
  >     --config-tier "$MONONOKE_CONFIG_TIER" \
  >     --tracing-test-format \
  >     validate -q \
  >     --scheduled-max 4 \
  >     -p DeletedManifestV2Mapping -d OldestFirst --chunk-size 20 \
  >     -i DeletedManifestV2Mapping -i DeletedManifestV2 \
  >     -I DeletedManifestV2MappingToRootDeletedManifestV2 \
  >     -I DeletedManifestV2ToDeletedManifestV2Child
  $ bash "$HARNESS" \
  >   --implementation strider \
  >   --output-prefix "$TESTTMP/benchmark/strider" \
  >   --concurrency 4 \
  >   --visited-capacity unbounded \
  >   --cache-condition unspecified-smoke \
  >   --semantic-result match \
  >   -- env GLOG_minloglevel=5 "$MONONOKE_STRIDER" \
  >     "${CACHE_ARGS[@]}" \
  >     "${COMMON_ARGS[@]}" \
  >     --repo-id "$REPOID" \
  >     --config-tier "$MONONOKE_CONFIG_TIER" \
  >     --tracing-test-format \
  >     --family deleted-manifest \
  >     --operation validate \
  >     --chunk-size 20 \
  >     --max-concurrency 4 \
  >     --max-loads-per-commit 4

Both successful samples contain every required field, and each timed run loaded
what its gated run loaded. Walker loaded the 285 manifest nodes and 50 mappings;
Strider also loaded the three sharded-map nodes.

  $ for implementation in walker strider; do
  >   sample="$TESTTMP/benchmark/$implementation.sample"
  >   grep -Eq '^sample_status=success$' "$sample" || exit 1
  >   grep -Eq "^implementation=$implementation$" "$sample" || exit 1
  >   grep -Eq '^elapsed_seconds=[0-9]+\.[0-9]+$' "$sample" || exit 1
  >   grep -Eq '^visited_nodes=[0-9]+$' "$sample" || exit 1
  >   grep -Eq '^nodes_per_second=[0-9]+\.[0-9]+$' "$sample" || exit 1
  >   grep -Eq '^peak_rss_kb=[0-9]+$' "$sample" || exit 1
  >   grep -Eq '^concurrency=4$' "$sample" || exit 1
  >   grep -Eq '^visited_capacity=unbounded$' "$sample" || exit 1
  >   grep -Eq '^cache_condition=unspecified-smoke$' "$sample" || exit 1
  >   grep -Eq '^semantic_result=match$' "$sample" || exit 1
  >   grep -Eq '^exit_status=0$' "$sample" || exit 1
  >   test -s "$TESTTMP/benchmark/$implementation.command" || exit 1
  >   test -s "$TESTTMP/benchmark/$implementation.log" || exit 1
  > done
  $ grep -H '^visited_nodes=' benchmark/walker.sample benchmark/strider.sample
  benchmark/walker.sample:visited_nodes=335
  benchmark/strider.sample:visited_nodes=338
  $ sed -nE 's/.*Seen,Loaded: [0-9]+,([0-9]+).*/\1/p' walker-gate.out |
  >   awk '{ n += $1 } END { print "visited_nodes=" n }' | diff - <(grep '^visited_nodes=' benchmark/walker.sample)
  $ sed -nE 's/.*(visited_nodes=[0-9]+).*/\1/p' strider-gate.err |
  >   diff - <(grep '^visited_nodes=' benchmark/strider.sample)

A failed command retains its exit status and cannot produce a successful sample.

  $ bash "$HARNESS" \
  >   --implementation walker \
  >   --output-prefix "$TESTTMP/benchmark/failure" \
  >   --concurrency 4 \
  >   --visited-capacity unbounded \
  >   --cache-condition unspecified-smoke \
  >   --semantic-result match \
  >   -- sh -c 'exit 23'
  [23]
  $ grep -E '^(sample_status|exit_status|visited_nodes|nodes_per_second)=' "$TESTTMP/benchmark/failure.sample"
  sample_status=failed
  visited_nodes=unavailable
  nodes_per_second=unavailable
  exit_status=23

A command killed by a signal, such as an out-of-memory kill, is a failed sample
with the status a shell reports for that signal.

  $ bash "$HARNESS" \
  >   --implementation strider \
  >   --output-prefix "$TESTTMP/benchmark/killed" \
  >   --concurrency 4 \
  >   --visited-capacity unbounded \
  >   --cache-condition unspecified-smoke \
  >   --semantic-result match \
  >   -- sh -c 'kill -9 $$'
  [137]
  $ grep -E '^(sample_status|exit_status)=' "$TESTTMP/benchmark/killed.sample"
  sample_status=failed
  exit_status=137
