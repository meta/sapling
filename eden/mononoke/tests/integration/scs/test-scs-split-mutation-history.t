# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"
  $ enable commitcloud amend
  $ setup_common_config
  $ merge_just_knobs <<'EOF'
  > {"bools":{"scm/mononoke:enable_split_member_mutation_lookup":true}}
  > EOF
  $ cd "$TESTTMP"
  $ start_and_wait_for_mononoke_server
  $ sl clone -q mono:repo client
  $ cd client

Import a stack with the same mutation shape as a split followed by rebase and
metaedit. Importstack records the split only on its final child.

  $ sl debugimportstack > "$TESTTMP/marks.json" <<'EOF'
  > [["commit", {"text": "original", "mark": ":a", "files": {"a": {"data": "a"}}}],
  >  ["commit", {"text": "amended", "mark": ":p", "predecessors": [":a"], "operation": "amend", "files": {"a": {"data": "a"}, "b": {"data": "b"}, "c": {"data": "c"}}}],
  >  ["commit", {"text": "first split child", "mark": ":s1", "predecessors": [":p"], "files": {"a": {"data": "a"}}}],
  >  ["commit", {"text": "second split child", "mark": ":s2", "predecessors": [":p"], "parents": [":s1"], "files": {"b": {"data": "b"}}}],
  >  ["commit", {"text": "final split child", "mark": ":s3", "predecessors": [":p"], "parents": [":s2"], "operation": "split", "files": {"c": {"data": "c"}}}],
  >  ["commit", {"text": "rebased first child", "mark": ":r", "predecessors": [":s1"], "operation": "rebase", "files": {"a": {"data": "a"}}}],
  >  ["commit", {"text": "metaedited first child", "mark": ":m", "predecessors": [":r"], "operation": "metaedit", "files": {"a": {"data": "a"}}}],
  >  ["commit", {"text": "imported second child", "mark": ":i", "predecessors": [":s2"], "files": {"b": {"data": "b"}}}],
  >  ["commit", {"text": "second original", "mark": ":q", "files": {"x": {"data": "x"}}}],
  >  ["commit", {"text": "second amended", "mark": ":p2", "predecessors": [":q"], "operation": "amend", "files": {"x": {"data": "x"}, "y": {"data": "y"}, "z": {"data": "z"}}}],
  >  ["commit", {"text": "first importstack child", "mark": ":u1", "predecessors": [":p2"], "files": {"x": {"data": "x"}}}],
  >  ["commit", {"text": "second importstack child", "mark": ":u2", "predecessors": [":p2"], "parents": [":u1"], "files": {"y": {"data": "y"}}}],
  >  ["commit", {"text": "final importstack child", "mark": ":u3", "predecessors": [":p2"], "parents": [":u2"], "files": {"z": {"data": "z"}}}],
  >  ["commit", {"text": "rebased importstack child", "mark": ":r2", "predecessors": [":u1"], "operation": "rebase", "files": {"x": {"data": "x"}}}],
  >  ["commit", {"text": "metaedited importstack child", "mark": ":m2", "predecessors": [":r2"], "operation": "metaedit", "files": {"x": {"data": "x"}}}],
  >  ["commit", {"text": "imported importstack child", "mark": ":i2", "predecessors": [":u2"], "files": {"y": {"data": "y"}}}]]
  > EOF
  $ S1=$(jq -r '.[":s1"]' "$TESTTMP/marks.json")
  $ S2=$(jq -r '.[":s2"]' "$TESTTMP/marks.json")
  $ S3=$(jq -r '.[":s3"]' "$TESTTMP/marks.json")
  $ M=$(jq -r '.[":m"]' "$TESTTMP/marks.json")
  $ I=$(jq -r '.[":i"]' "$TESTTMP/marks.json")
  $ U1=$(jq -r '.[":u1"]' "$TESTTMP/marks.json")
  $ U2=$(jq -r '.[":u2"]' "$TESTTMP/marks.json")
  $ U3=$(jq -r '.[":u3"]' "$TESTTMP/marks.json")
  $ M2=$(jq -r '.[":m2"]' "$TESTTMP/marks.json")
  $ I2=$(jq -r '.[":i2"]' "$TESTTMP/marks.json")
  $ sl cloud backup -r "$S3" -r "$M" -r "$I" -r "$U3" -r "$M2" -r "$I2" -q
  $ start_and_wait_for_scs_server

Render commit IDs using the importstack marks, leaving canonical entry IDs,
predecessors, split order and operations visible in the expected output.

  $ cat > "$TESTTMP/show-mutations.py" <<'PY'
  > import json
  > import sys
  > with open(sys.argv[1]) as source:
  >     names = {value: key for key, value in json.load(source).items()}
  > mutations = json.load(sys.stdin)["mutations"]
  > for entry in sorted(mutations, key=lambda entry: names[entry["successor"]]):
  >     print("{} {} predecessors={} split={}".format(
  >         names[entry["successor"]], entry["op"],
  >         ",".join(names[node] for node in entry["predecessors"]),
  >         ",".join(names[node] for node in entry["split"])))
  > PY
  $ legacy_mutations() { scsc --json hg-mutation-history --repo repo --hg-commit-id "$1" --format hg-mutation | python "$TESTTMP/show-mutations.py" "$TESTTMP/marks.json"; }
  $ mutations() { scsc --json hg-mutation-history --repo repo --hg-commit-id "$1" --format hg-mutation --follow-splits | python "$TESTTMP/show-mutations.py" "$TESTTMP/marks.json"; }

Without the flag, intermediate members retain the legacy empty answer; final
successors retain their own canonical history.

  $ legacy_mutations "$S1"
  $ legacy_mutations "$U1"
  $ legacy_mutations "$S3"
  :p amend predecessors=:a split=
  :s3 split predecessors=:p split=:s1,:s2
  $ legacy_mutations "$U3"
  :p2 amend predecessors=:q split=
  :u3 importstack predecessors=:p2 split=:u1,:u2

Both intermediate children return the final child's split entry and original
history. The final successor returns exactly the same canonical entries.

  $ mutations "$S1"
  :p amend predecessors=:a split=
  :s3 split predecessors=:p split=:s1,:s2
  $ mutations "$S2"
  :p amend predecessors=:a split=
  :s3 split predecessors=:p split=:s1,:s2
  $ mutations "$S3"
  :p amend predecessors=:a split=
  :s3 split predecessors=:p split=:s1,:s2

The later rebase/metaedit and importstack entries do not break split lineage.

  $ mutations "$M"
  :m metaedit predecessors=:r split=
  :p amend predecessors=:a split=
  :r rebase predecessors=:s1 split=
  :s3 split predecessors=:p split=:s1,:s2
  $ mutations "$I"
  :i importstack predecessors=:s2 split=
  :p amend predecessors=:a split=
  :s3 split predecessors=:p split=:s1,:s2

Repeated reads through the cache preserve the corrected response.

  $ mutations "$S1"
  :p amend predecessors=:a split=
  :s3 split predecessors=:p split=:s1,:s2

Default ISL importstack also records multiple children only on its final entry.
The operation is importstack, not split; membership is carried by the split list.

  $ mutations "$U1"
  :p2 amend predecessors=:q split=
  :u3 importstack predecessors=:p2 split=:u1,:u2
  $ mutations "$U2"
  :p2 amend predecessors=:q split=
  :u3 importstack predecessors=:p2 split=:u1,:u2
  $ mutations "$U3"
  :p2 amend predecessors=:q split=
  :u3 importstack predecessors=:p2 split=:u1,:u2
  $ mutations "$M2"
  :m2 metaedit predecessors=:r2 split=
  :p2 amend predecessors=:q split=
  :r2 rebase predecessors=:u1 split=
  :u3 importstack predecessors=:p2 split=:u1,:u2
  $ mutations "$I2"
  :i2 importstack predecessors=:u2 split=
  :p2 amend predecessors=:q split=
  :u3 importstack predecessors=:p2 split=:u1,:u2
  $ mutations "$U1"
  :p2 amend predecessors=:q split=
  :u3 importstack predecessors=:p2 split=:u1,:u2
