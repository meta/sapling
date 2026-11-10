# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"

A repo multiplexing two sqlblob components, the first one packed, with the
retired types derived:
  $ export MULTIPLEXED=1
  $ export PACK_BLOB=0
  $ ADDITIONAL_DERIVED_DATA="history_manifests fastlog_v2 blame_v3" setup_common_config "blob_sqlite"
  $ F1=$(for i in $(seq 1 100); do echo "$i" | sha256sum | cut -c1-64; done | tr -d '\n')
  $ testtool_drawdag -R repo <<EOF
  > A-B
  > # modify: A "f1" "$F1"
  > # modify: B "f2" "${F1}x"
  > # bookmark: B main
  > EOF
  A=* (glob)
  B=* (glob)
  $ mononoke_admin derived-data -R repo derive -T unodes -T history_manifests -T fastlog_v2 -T blame_v3 -B main

  $ keys() {
  >   for s in 0 1; do sqlite3 "$TESTTMP/blobstore/$1/blobs/shard_$s.sqlite" "SELECT id FROM data"; done | sort
  > }
  $ retired() {
  >   grep -E '^repo0000\.(historymf|fastlogbatchv2|blame_v3|derived_root_history_manifest)\.'
  > }
  $ keys 0 | retired | sed -E 's/[0-9a-f]{64}/H/' | sort | uniq -c
        4 repo0000.blame_v3.v2.historymf.file.blake2.H.pack
        2 repo0000.fastlogbatchv2.hmdir.H.pack
        4 repo0000.fastlogbatchv2.hmfile.H.pack
        2 repo0000.historymf.dir.blake2.H.pack
        4 repo0000.historymf.file.blake2.H.pack
  $ keys 1 | retired | wc -l
  16
  $ keys 0 | grep -c fileunode
  4
  $ sqlite3 "$TESTTMP/monsql/sqlite_dbs" "SELECT derived_data_type, derived_data_version, COUNT(*) FROM commit_derived_data GROUP BY 1, 2"
  27|1|2
  28|2|2
  30|1|2

Pack the file contents together with the retired blame of each file, in the
first component:
  $ mkdir "$TESTTMP/pack_keys"
  $ keys 0 | grep -E '^repo0000\.(content\.blake2|blame_v3\.v2\.historymf)\.' | sed 's/\.pack$//' > "$TESTTMP/pack_keys/reporepo.store0.part0.keys.txt"
  $ wc -l < "$TESTTMP/pack_keys/reporepo.store0.part0.keys.txt"
  8
  $ packer --zstd-level 3 --keys-dir "$TESTTMP/pack_keys"
  $ for k in $(cat "$TESTTMP/pack_keys/reporepo.store0.part0.keys.txt"); do
  >   for s in 0 1; do sqlite3 "$TESTTMP/blobstore/0/blobs/shard_$s.sqlite" "SELECT chunk_id FROM data WHERE CAST(id AS TEXT) = '$k.pack'"; done
  > done | sort -u | wc -l
  1
  $ CONTENT_KEYS=$(grep content "$TESTTMP/pack_keys/reporepo.store0.part0.keys.txt")
  $ for k in $CONTENT_KEYS; do mononoke_admin blobstore -R repo fetch -q "${k#repo0000.}" -o "$TESTTMP/before.$k"; done

Only the compiled families can be selected:
  $ AAAA=$(printf 'a%.0s' $(seq 64))
  $ blob_type_annihilator check-key "repo0000.historymf.file.blake2.$AAAA.pack" "repo0000.fsnode.blake2.$AAAA" "repo0000.blame_v3.v1.historymf.file.blake2.$AAAA"
  selected repo0000.historymf.file.blake2.aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.pack: repo 0 recipe historymf.file hash aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa (PackEnvelope)
  not selected repo0000.fsnode.blake2.aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa: matches no recipe
  not selected repo0000.blame_v3.v1.historymf.file.blake2.aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa: matches no recipe
  $ blob_type_annihilator check-key --retired-type history-manifests "repo0000.historymf.file.blake2.$AAAA" "repo0000.blame_v3.v2.historymf.file.blake2.$AAAA"
  selected repo0000.historymf.file.blake2.aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa: repo 0 recipe historymf.file hash aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa (Plain)
  not selected repo0000.blame_v3.v2.historymf.file.blake2.aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa: recipe blame_v3.v2 of an unselected type
  $ blob_type_annihilator plan --state-dir "$TESTTMP/bta" --all-repos --retired-type fsnodes
  error: invalid value 'fsnodes' for '--retired-type <RETIRED_TYPES>'
    [possible values: history-manifests, fastlog-v2, blame-v3]
  
  For more information, try '--help'.
  [2]
  $ blob_type_annihilator plan --state-dir "$TESTTMP/bta" --all-repos --retired-type history-manifests
  Error: history-manifests cannot be retired without fastlog-v2, which refers to its blobs
  [1]

Plan, check and inventory.  Component ids depend on store paths, so they are
replaced by names below:
  $ plan_all() {
  >   blob_type_annihilator plan --state-dir "$1" --retired-type history-manifests --retired-type fastlog-v2 --retired-type blame-v3 "${@:2}"
  > }
  $ plan_all "$TESTTMP/bta" --all-repos > "$TESTTMP/plan.out"
  $ C0=$(jq -r '.components[0].id' "$TESTTMP/bta/plan.json")
  $ C1=$(jq -r '.components[1].id' "$TESTTMP/bta/plan.json")
  $ DIGEST=$(jq -r '.digest' "$TESTTMP/bta/plan.json")
  $ named() {
  >   sed -E -e "s/$C0/C0/g" -e "s/$C1/C1/g" -e "s/$DIGEST/DIGEST/g" -e 's/verify-[0-9]+/verify-TIME/'
  > }
  $ bta() {
  >   blob_type_annihilator "$@" 2>&1 | named
  >   return "${PIPESTATUS[0]}"
  > }
  $ named < "$TESTTMP/plan.out"
  Plan DIGEST
  Retired types: history-manifests, fastlog-v2, blame-v3
  Repositories (all configured, 1): repo (0)
  Component C0 Sqlite { path: "$TESTTMP/blobstore/0" }: supported, 1 repo
    main multiplex 1 component 0 (Normal)/pack/ (packed): 1 repo
  Component C1 Sqlite { path: "$TESTTMP/blobstore/1" }: supported, 1 repo
    main multiplex 1 component 1 (Normal)/: 1 repo
  Not covered in 1 repo (0): mutable blobstore: holds mutable state, not derived data; not inventoried
  $ jq -c '.families' "$TESTTMP/bta/plan.json"
  ["historymf.","derived_root_history_manifest.","fastlogbatchv2.","blame_v3."]
  $ bta scan --state-dir "$TESTTMP/bta" --exists
  present component C0 repo 0 recipe historymf.file
  present component C0 repo 0 recipe historymf.dir
  absent component C0 repo 0 recipe historymf.delnode
  absent component C0 repo 0 recipe historymf.map2node
  absent component C0 repo 0 recipe derived_root_history_manifest
  present component C0 repo 0 recipe fastlogbatchv2.hmfile
  present component C0 repo 0 recipe fastlogbatchv2.hmdir
  present component C0 repo 0 recipe blame_v3.v2
  absent component C0 repo 0 recipe blame_v3.unversioned
  present component C1 repo 0 recipe historymf.file
  present component C1 repo 0 recipe historymf.dir
  absent component C1 repo 0 recipe historymf.delnode
  absent component C1 repo 0 recipe historymf.map2node
  absent component C1 repo 0 recipe derived_root_history_manifest
  present component C1 repo 0 recipe fastlogbatchv2.hmfile
  present component C1 repo 0 recipe fastlogbatchv2.hmdir
  present component C1 repo 0 recipe blame_v3.v2
  absent component C1 repo 0 recipe blame_v3.unversioned
  $ bta scan --state-dir "$TESTTMP/bta" --sql-page-size 2
  component C0 repo 0 recipe historymf.file: 16/16 partitions, 4 keys (plain 0, single 4, packed 0, unreadable 0), vanished 0, unknown 0, duplicates 0
  component C0 repo 0 recipe historymf.dir: 16/16 partitions, 2 keys (plain 0, single 2, packed 0, unreadable 0), vanished 0, unknown 0, duplicates 0
  component C0 repo 0 recipe fastlogbatchv2.hmfile: 16/16 partitions, 4 keys (plain 0, single 4, packed 0, unreadable 0), vanished 0, unknown 0, duplicates 0
  component C0 repo 0 recipe fastlogbatchv2.hmdir: 16/16 partitions, 2 keys (plain 0, single 2, packed 0, unreadable 0), vanished 0, unknown 0, duplicates 0
  component C0 repo 0 recipe blame_v3.v2: 16/16 partitions, 4 keys (plain 0, single 0, packed 4, unreadable 0), vanished 0, unknown 0, duplicates 0
  component C1 repo 0 recipe historymf.file: 16/16 partitions, 4 keys (plain 4, single 0, packed 0, unreadable 0), vanished 0, unknown 0, duplicates 0
  component C1 repo 0 recipe historymf.dir: 16/16 partitions, 2 keys (plain 2, single 0, packed 0, unreadable 0), vanished 0, unknown 0, duplicates 0
  component C1 repo 0 recipe fastlogbatchv2.hmfile: 16/16 partitions, 4 keys (plain 4, single 0, packed 0, unreadable 0), vanished 0, unknown 0, duplicates 0
  component C1 repo 0 recipe fastlogbatchv2.hmdir: 16/16 partitions, 2 keys (plain 2, single 0, packed 0, unreadable 0), vanished 0, unknown 0, duplicates 0
  component C1 repo 0 recipe blame_v3.v2: 16/16 partitions, 4 keys (plain 4, single 0, packed 0, unreadable 0), vanished 0, unknown 0, duplicates 0
  8 cells hold no retired keys

A plan can name its repositories, by id or from a file, and keep each cell in
a single partition:
  $ echo 0 > "$TESTTMP/repo_ids"
  $ plan_all "$TESTTMP/bta-one" --repo-id 0 --repo-ids-file "$TESTTMP/repo_ids" --partition-digits 0 | named | head -3
  Plan * (glob)
  Retired types: history-manifests, fastlog-v2, blame-v3
  Repositories (partial scope, 1): repo (0)
  $ bta scan --state-dir "$TESTTMP/bta-one" --component "$C1"
  component C1 repo 0 recipe historymf.file: 1/1 partitions, 4 keys (plain 4, single 0, packed 0, unreadable 0), vanished 0, unknown 0, duplicates 0
  component C1 repo 0 recipe historymf.dir: 1/1 partitions, 2 keys (plain 2, single 0, packed 0, unreadable 0), vanished 0, unknown 0, duplicates 0
  component C1 repo 0 recipe fastlogbatchv2.hmfile: 1/1 partitions, 4 keys (plain 4, single 0, packed 0, unreadable 0), vanished 0, unknown 0, duplicates 0
  component C1 repo 0 recipe fastlogbatchv2.hmdir: 1/1 partitions, 2 keys (plain 2, single 0, packed 0, unreadable 0), vanished 0, unknown 0, duplicates 0
  component C1 repo 0 recipe blame_v3.v2: 1/1 partitions, 4 keys (plain 4, single 0, packed 0, unreadable 0), vanished 0, unknown 0, duplicates 0
  4 cells hold no retired keys
  $ ls "$TESTTMP/bta-one/inventory/$C1/0/historymf.file"
  all.jsonl
  all.summary.json

An inventory can be seeded from a list of keys instead, such as a Manifold
key dump.  Keys from component C1's list are recorded for both components,
with the packed component read to find how each key is stored there:
  $ plan_all "$TESTTMP/bta-import" --all-repos > /dev/null
  $ (keys 1 | retired | sed 's|^|flat/|'; echo "flat/repo0000.content.blake2.$AAAA"; echo "flat/repo0007.historymf.file.blake2.$AAAA") > "$TESTTMP/dump.keys"
  $ bta import-keys --state-dir "$TESTTMP/bta-import" --keys-file "$TESTTMP/dump.keys"
  staged 16 of 18 keys; 1 outside the plan, 1 rejected
    rejected flat/repo0000.content.blake2.aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
  component C0 repo 0 recipe historymf.file: 16/16 partitions, 4 keys (plain 0, single 4, packed 0, unreadable 0), vanished 0, unknown 0, duplicates 0, seeded 4
  component C0 repo 0 recipe historymf.dir: 16/16 partitions, 2 keys (plain 0, single 2, packed 0, unreadable 0), vanished 0, unknown 0, duplicates 0, seeded 2
  component C0 repo 0 recipe fastlogbatchv2.hmfile: 16/16 partitions, 4 keys (plain 0, single 4, packed 0, unreadable 0), vanished 0, unknown 0, duplicates 0, seeded 4
  component C0 repo 0 recipe fastlogbatchv2.hmdir: 16/16 partitions, 2 keys (plain 0, single 2, packed 0, unreadable 0), vanished 0, unknown 0, duplicates 0, seeded 2
  component C0 repo 0 recipe blame_v3.v2: 16/16 partitions, 4 keys (plain 0, single 0, packed 4, unreadable 0), vanished 0, unknown 0, duplicates 0, seeded 4
  component C1 repo 0 recipe historymf.file: 16/16 partitions, 4 keys (plain 4, single 0, packed 0, unreadable 0), vanished 0, unknown 0, duplicates 0, seeded 4
  component C1 repo 0 recipe historymf.dir: 16/16 partitions, 2 keys (plain 2, single 0, packed 0, unreadable 0), vanished 0, unknown 0, duplicates 0, seeded 2
  component C1 repo 0 recipe fastlogbatchv2.hmfile: 16/16 partitions, 4 keys (plain 4, single 0, packed 0, unreadable 0), vanished 0, unknown 0, duplicates 0, seeded 4
  component C1 repo 0 recipe fastlogbatchv2.hmdir: 16/16 partitions, 2 keys (plain 2, single 0, packed 0, unreadable 0), vanished 0, unknown 0, duplicates 0, seeded 2
  component C1 repo 0 recipe blame_v3.v2: 16/16 partitions, 4 keys (plain 4, single 0, packed 0, unreadable 0), vanished 0, unknown 0, duplicates 0, seeded 4
  8 cells hold no retired keys

A run takes keys from one source only:
  $ bta import-keys --state-dir "$TESTTMP/bta-import" --from-component "$C1"
  Error: this run already imported keys from Files { paths: ["$TESTTMP/dump.keys"] }; use a new state directory for another source
  [1]

A directory of key files, such as the export's output, can be imported too,
here into the single-partition run for the other component:
  $ mkdir "$TESTTMP/dump"
  $ cp "$TESTTMP/dump.keys" "$TESTTMP/dump/part1.keys"
  $ touch "$TESTTMP/dump/part2.keys.tmp"
  $ bta import-keys --state-dir "$TESTTMP/bta-one" --keys-dir "$TESTTMP/dump" --component "$C0"
  staged 16 of 18 keys; 1 outside the plan, 1 rejected
    rejected flat/repo0000.content.blake2.aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
  component C0 repo 0 recipe historymf.file: 1/1 partitions, 4 keys (plain 0, single 4, packed 0, unreadable 0), vanished 0, unknown 0, duplicates 0, seeded 4
  component C0 repo 0 recipe historymf.dir: 1/1 partitions, 2 keys (plain 0, single 2, packed 0, unreadable 0), vanished 0, unknown 0, duplicates 0, seeded 2
  component C0 repo 0 recipe fastlogbatchv2.hmfile: 1/1 partitions, 4 keys (plain 0, single 4, packed 0, unreadable 0), vanished 0, unknown 0, duplicates 0, seeded 4
  component C0 repo 0 recipe fastlogbatchv2.hmdir: 1/1 partitions, 2 keys (plain 0, single 2, packed 0, unreadable 0), vanished 0, unknown 0, duplicates 0, seeded 2
  component C0 repo 0 recipe blame_v3.v2: 1/1 partitions, 4 keys (plain 0, single 0, packed 4, unreadable 0), vanished 0, unknown 0, duplicates 0, seeded 4
  4 cells hold no retired keys

Keys can also come from a component that lists quickly, such as a SQL
component, scanned first in the same run:
  $ plan_all "$TESTTMP/bta-seed" --all-repos > /dev/null
  $ bta import-keys --state-dir "$TESTTMP/bta-seed" --from-component "$C1" --component "$C0"
  Error: partition 0 of component C1 repo 0 recipe historymf.file has no inventory; scan component C1 first
  [1]
  $ bta scan --state-dir "$TESTTMP/bta-seed" --component "$C1" > /dev/null
  $ bta import-keys --state-dir "$TESTTMP/bta-seed" --from-component "$C1" --component "$C0"
  staged 16 of 16 keys; 0 outside the plan, 0 rejected
  component C0 repo 0 recipe historymf.file: 16/16 partitions, 4 keys (plain 0, single 4, packed 0, unreadable 0), vanished 0, unknown 0, duplicates 0, seeded 4
  component C0 repo 0 recipe historymf.dir: 16/16 partitions, 2 keys (plain 0, single 2, packed 0, unreadable 0), vanished 0, unknown 0, duplicates 0, seeded 2
  component C0 repo 0 recipe fastlogbatchv2.hmfile: 16/16 partitions, 4 keys (plain 0, single 4, packed 0, unreadable 0), vanished 0, unknown 0, duplicates 0, seeded 4
  component C0 repo 0 recipe fastlogbatchv2.hmdir: 16/16 partitions, 2 keys (plain 0, single 2, packed 0, unreadable 0), vanished 0, unknown 0, duplicates 0, seeded 2
  component C0 repo 0 recipe blame_v3.v2: 16/16 partitions, 4 keys (plain 0, single 0, packed 4, unreadable 0), vanished 0, unknown 0, duplicates 0, seeded 4
  4 cells hold no retired keys

Apply requires the reviewed plan's digest:
  $ bta apply --state-dir "$TESTTMP/bta" --confirm-plan-digest wrong
  Error: confirmed digest wrong is not the plan's digest DIGEST
  [1]

Applying to one component leaves the other alone.  This run also caps the
unlink rate and the partitions in flight:
  $ bta apply --state-dir "$TESTTMP/bta" --confirm-plan-digest "$DIGEST" --component "$C1" --max-unlinks-per-second 100 --partition-concurrency 2 --batch-size 3
  C1 repo 0: 0 pack tasks covering 0 packed aliases
  repo 0 metadata: 6 mapping rows deleted, 0 shared-cache entries evicted, 3 batches not invalidated, unapproved rows left {}
  component C1 repo 0 recipe historymf.file: 16/16 partitions done; unlinked 4, already absent 0, failed 0, blocked 0, packed 0
  component C1 repo 0 recipe historymf.dir: 16/16 partitions done; unlinked 2, already absent 0, failed 0, blocked 0, packed 0
  component C1 repo 0 recipe fastlogbatchv2.hmfile: 16/16 partitions done; unlinked 4, already absent 0, failed 0, blocked 0, packed 0
  component C1 repo 0 recipe fastlogbatchv2.hmdir: 16/16 partitions done; unlinked 2, already absent 0, failed 0, blocked 0, packed 0
  component C1 repo 0 recipe blame_v3.v2: 16/16 partitions done; unlinked 4, already absent 0, failed 0, blocked 0, packed 0
  4 cells had nothing to delete
  repo 0 final metadata pass: 0 mapping rows deleted, 0 shared-cache entries evicted again
  $ keys 1 | retired | wc -l
  0
  $ keys 0 | retired | wc -l
  16

The full run finishes the other component and repeats the recorded outcomes
of the finished partitions:
  $ bta apply --state-dir "$TESTTMP/bta" --confirm-plan-digest "$DIGEST"
  C0 repo 0: 1 pack tasks covering 4 packed aliases
  C1 repo 0: 0 pack tasks covering 0 packed aliases
  repo 0 metadata: 0 mapping rows deleted, 0 shared-cache entries evicted, 0 batches not invalidated, unapproved rows left {}
  component C0 repo 0 recipe historymf.file: 16/16 partitions done; unlinked 4, already absent 0, failed 0, blocked 0, packed 0
  component C0 repo 0 recipe historymf.dir: 16/16 partitions done; unlinked 2, already absent 0, failed 0, blocked 0, packed 0
  component C0 repo 0 recipe fastlogbatchv2.hmfile: 16/16 partitions done; unlinked 4, already absent 0, failed 0, blocked 0, packed 0
  component C0 repo 0 recipe fastlogbatchv2.hmdir: 16/16 partitions done; unlinked 2, already absent 0, failed 0, blocked 0, packed 0
  component C0 repo 0 recipe blame_v3.v2: 16/16 partitions done; unlinked 0, already absent 0, failed 0, blocked 0, packed 4; pack tasks complete 1 (rewritten 1), retired aliases unlinked 4, survivors relinked 4, source missing 0, conflicts 0, failed 0
  component C1 repo 0 recipe historymf.file: 16/16 partitions done; unlinked 4, already absent 0, failed 0, blocked 0, packed 0
  component C1 repo 0 recipe historymf.dir: 16/16 partitions done; unlinked 2, already absent 0, failed 0, blocked 0, packed 0
  component C1 repo 0 recipe fastlogbatchv2.hmfile: 16/16 partitions done; unlinked 4, already absent 0, failed 0, blocked 0, packed 0
  component C1 repo 0 recipe fastlogbatchv2.hmdir: 16/16 partitions done; unlinked 2, already absent 0, failed 0, blocked 0, packed 0
  component C1 repo 0 recipe blame_v3.v2: 16/16 partitions done; unlinked 4, already absent 0, failed 0, blocked 0, packed 0
  8 cells had nothing to delete
  repo 0 final metadata pass: 0 mapping rows deleted, 0 shared-cache entries evicted again

The retired keys and mappings are gone from both components, unodes are
untouched, and the packed contents read back unchanged from their new pack:
  $ keys 0 | retired | wc -l
  0
  $ keys 1 | retired | wc -l
  0
  $ keys 0 | grep -c fileunode
  4
  $ sqlite3 "$TESTTMP/monsql/sqlite_dbs" "SELECT derived_data_type, derived_data_version, COUNT(*) FROM commit_derived_data GROUP BY 1, 2"
  $ for k in $CONTENT_KEYS; do
  >   mononoke_admin blobstore -R repo fetch -q "${k#repo0000.}" -o "$TESTTMP/after.$k"
  >   cmp "$TESTTMP/before.$k" "$TESTTMP/after.$k" && echo same
  > done
  same
  same
  same
  same
  $ for k in $CONTENT_KEYS; do
  >   for s in 0 1; do sqlite3 "$TESTTMP/blobstore/0/blobs/shard_$s.sqlite" "SELECT chunk_id FROM data WHERE CAST(id AS TEXT) = '$k.pack'"; done
  > done | sort -u | wc -l
  1

Verification scans afresh.  Tests run without a shared cache, so the cache
evictions cannot be certified and the result stays incomplete:
  $ bta verify --state-dir "$TESTTMP/bta"
  Plan DIGEST
  Repositories: all configured repositories; repositories absent from the configuration are not covered
  Cells: 18 checked, 0 not verified absent
  Metadata repo 0: clean; shared cache: {NoSharedCache: 3}
  Packs C0 repo 0: 1/1 tasks complete, 1 rewritten, 0 packs not found
  Not covered: repo 0 mutable blobstore: holds mutable state, not derived data; not inventoried
  Physical reclamation: C0: pending; unlinked keys leave chunks for sqlblob GC to reclaim
  Physical reclamation: C1: pending; unlinked keys leave chunks for sqlblob GC to reclaim
  Physical reclamation: 1 packs rewritten; each original pack is released when its last link is gone
  Operator action: Restart or redeploy every process holding cachelib entries of the retired types' mappings (derived data service, derivation workers, SCS and other readers) before any rebuild.
  Operator action: Confirm replicas of the metadata databases have converged before replica reads resume.
  Operator action: Keep producers, consumers, healing and packing of the affected stores fenced until the rebuild starts.
  Verdict: Incomplete
  Report: $TESTTMP/bta/reports/verify-TIME.json
  Error: verification found retired data, failures or unchecked cells
  [1]

A retired key written again, and a key of a retired family in a format no
recipe covers, are both caught:
  $ echo value > "$TESTTMP/value"
  $ mononoke_admin blobstore -R repo upload --key "historymf.file.blake2.$AAAA" --value-file "$TESTTMP/value" > /dev/null
  $ mononoke_admin blobstore -R repo upload --key "historymf.v9.$AAAA" --value-file "$TESTTMP/value" > /dev/null
  $ bta verify --state-dir "$TESTTMP/bta" | grep -E '^(Cells|  |Family|Verdict)'
  Cells: 18 checked, 2 not verified absent
    Present C0 repo 0 historymf.file
    Present C1 repo 0 historymf.file
  Family historymf. in C0 repo 0: 1 registered keys, 1 unknown keys
    unknown key repo0000.historymf.v9.aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.pack
  Family historymf. in C1 repo 0: 1 registered keys, 1 unknown keys
    unknown key repo0000.historymf.v9.aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
  Verdict: Incomplete
