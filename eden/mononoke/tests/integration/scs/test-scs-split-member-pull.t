# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"
  $ configure modern
  $ enable amend commitcloud rebase
  $ export CHGDISABLE=1
  $ setconfig alias.restack="rebase --restack"
  $ setconfig mutation.enabled=true mutation.record=true mutation.date="0 0" visibility.enabled=true
  $ INFINITEPUSH_ALLOW_WRITES=true setup_common_config
  $ setconfig mutation.record=true
  $ sl config mutation.record
  true
  $ cd "$TESTTMP"
  $ testtool_drawdag -R repo --print-hg-hashes <<'EOF'
  > base_commit
  > # bookmark: base_commit master_bookmark
  > EOF
  base_commit=eb9c16dd0f62fa641290156c4988cf35c48c5fbe
  $ start_and_wait_for_mononoke_server --scuba-log-file "$TESTTMP/mononoke-scuba.json"
  $ sl clone -q mono:repo producer --noupdate
  $ cd producer
  $ sl debugimportstack > "$TESTTMP/original.json" <<'EOF'
  > [["commit", {"text": "combined", "mark": ":p", "parents": ["eb9c16dd0f62fa641290156c4988cf35c48c5fbe"], "files": {"a": {"data": "a"}, "b": {"data": "b"}, "c": {"data": "c"}}}]]
  > EOF
  $ P=$(jq -r '.[":p"]' "$TESTTMP/original.json")
  $ sl cloud backup -q -r "$P"

The receiver learns the original before the split is uploaded. Its local child
changes an independent file, so a wrong partial rebase destination is visible
in the resulting tree without needing a merge conflict.

  $ cd "$TESTTMP"
  $ sl clone -q mono:repo receiver --noupdate
  $ cd receiver
  $ sl debugchangelog --migrate lazy
  $ sl pull -q -r "$P"
  $ sl goto -q "$P"
  $ echo t > t
  $ sl commit -Aqm orphan
  $ T=$(sl log -r . -T '{node}')
  $ sl log -r . -T '{desc}: {files}\n'
  orphan: t
  $ sl manifest -r .
  a
  b
  base_commit
  c
  t

Only the producer creates the canonical split event. No mutation is seeded
or manufactured in the receiving client's local store.

  $ cd "$TESTTMP/producer"
  $ sl debugimportstack > "$TESTTMP/split.json" <<EOF
  > [["commit", {"text": "member C", "mark": ":c", "predecessors": ["$P"], "parents": ["eb9c16dd0f62fa641290156c4988cf35c48c5fbe"], "files": {"a": {"data": "a"}}}],
  >  ["commit", {"text": "member D", "mark": ":d", "predecessors": ["$P"], "parents": [":c"], "files": {"b": {"data": "b"}}}],
  >  ["commit", {"text": "canonical S", "mark": ":s", "predecessors": ["$P"], "parents": [":d"], "operation": "split", "files": {"c": {"data": "c"}}}]]
  > EOF
  $ C=$(jq -r '.[":c"]' "$TESTTMP/split.json")
  $ D=$(jq -r '.[":d"]' "$TESTTMP/split.json")
  $ S=$(jq -r '.[":s"]' "$TESTTMP/split.json")
  $ sl cloud backup -q -r "$S"
  $ jq -s '.[0] + .[1]' "$TESTTMP/original.json" "$TESTTMP/split.json" > "$TESTTMP/marks.json"

Read the actual receiving store, preserving the canonical successor, operation
and ordered split list. This readout does not add mutation entries. The
ordinary debugmutation command's direct lookup is not a split-aware readout.

  $ cat > "$TESTTMP/read-state.py" <<'PY'
  > import json
  > import sys
  > from sapling import hg, mutation, ui
  > from sapling.node import hex
  > repo = hg.repository(ui.ui(), ".")
  > with open(sys.argv[1]) as source:
  >     marks = json.load(source)
  > names = {bytes.fromhex(value): key for key, value in marks.items()}
  > p, c, s = (bytes.fromhex(marks[key]) for key in [":p", ":c", ":s"])
  > label = lambda node: names.get(node, hex(node))
  > known = set(repo.changelog.filternodes([p, c, s], local=True))
  > print("local P/C/S:", *(node in known for node in [p, c, s]))
  > entry = mutation.lookupsplit(repo, c)
  > if entry is None:
  >     print("member split entry: absent")
  > else:
  >     print("member split entry:", label(entry.succ()), entry.op(),
  >           "preds=" + ",".join(map(label, entry.preds())),
  >           "split=" + ",".join(map(label, entry.split())))
  > print("point obsolete P:", mutation.isobsolete(repo, p))
  > print("bulk obsolete P:", p in mutation.obsoletenodes(repo))
  > print("raw successors P:", ",".join(sorted(map(label, mutation.lookupsuccessors(repo, p)[0]))) if mutation.lookupsuccessors(repo, p) else "absent")
  > PY
  $ cd "$TESTTMP/receiver"
  $ sl debugpython -- "$TESTTMP/read-state.py" "$TESTTMP/marks.json"
  local P/C/S: True False False
  member split entry: absent
  point obsolete P: False
  bulk obsolete P: False
  raw successors P: absent

A new member pull goes through the modern HTTP graph path. Caller-selected
SCS split lookup does not activate this client-facing response.

  $ LOG=pull sl pull -q -r "$C" > "$TESTTMP/member-pull.stdout" 2> "$TESTTMP/member-pull.stderr"
  $ grep 'pull::httpgraph' "$TESTTMP/member-pull.stderr" | sed 's/.*pull::httpgraph: /httpgraph: /'
  httpgraph: edenapi fetched 1 graph nodes
  httpgraph: edenapi fetched graph with known 1 draft commits
  $ sl debugpython -- "$TESTTMP/read-state.py" "$TESTTMP/marks.json"
  local P/C/S: True True False
  member split entry: absent
  point obsolete P: False
  bulk obsolete P: False
  raw successors P: absent
  $ sl log --hidden -r "predecessors($C) - $C" -T '{desc}\n'
  $ sl log --hidden -r "successors($P) - $P" -T '{desc}\n'
  $ sl log --hidden -r 'obsolete()' -T '{desc}\n'
  $ CHGDISABLE=1 sl log --hidden -r "$P" -T '{desc} point={if(obsolete, "yes", "no")}\n'
  combined point=no
  $ CHGDISABLE=1 sl log --hidden -r "$P + obsolete()" -T '{desc} bulk-primed={if(obsolete, "yes", "no")}\n'
  combined bulk-primed=no

No split history is supplied by this legacy client read. Restack and next
therefore keep the original child on P rather than replay it onto partial C.

  $ sl restack > "$TESTTMP/restack.stdout" 2> "$TESTTMP/restack.stderr"
  $ cat "$TESTTMP/restack.stdout"
  nothing to rebase - empty destination
  $ test "$(sl log -r . -T '{node}')" = "$T"
  $ sl log --hidden -r '.^' -T '{desc}\n'
  combined
  $ sl goto -q "$C"
  $ sl next --rebase > "$TESTTMP/next.stdout" 2> "$TESTTMP/next.stderr"
  [255]
  $ cat "$TESTTMP/next.stderr"
  abort: current commit has no children
  $ test "$(sl log -r . -T '{node}')" = "$C"
  $ sl log --hidden -r "successors($T) - $T" -T '{desc}\n'
  $ sl log --hidden -r "$T^" -T '{desc}\n'
  combined
  $ sl manifest --hidden -r "$T"
  a
  b
  base_commit
  c
  t
  $ sl debugpython -c 'import sys; from sapling import hg, node, ui; r = hg.repository(ui.ui(), "."); ns = list(map(node.bin, sys.argv[1:])); known = set(r.changelog.filternodes(ns, local=True)); print("other split bodies present:", [n in known for n in ns])' "$D" "$S"
  other split bodies present: [False, False]

A fresh receiver that pulls the canonical final child as well has the full
stack. It is a control for the absent-canonical case, not a client-side fix.

  $ cd "$TESTTMP"
  $ sl clone -q mono:repo canonical-receiver --noupdate
  $ cd canonical-receiver
  $ sl debugchangelog --migrate lazy
  $ sl pull -q -r "$P" -r "$S"
  $ sl debugpython -- "$TESTTMP/read-state.py" "$TESTTMP/marks.json"
  local P/C/S: True True True
  member split entry: :s split preds=:p split=:c,:d
  point obsolete P: True
  bulk obsolete P: True
  raw successors P: :c,:d,:s
  $ sl log --hidden -r 'obsolete()' -T '{desc}\n'
  combined
  $ sl manifest -r "$S"
  a
  b
  base_commit
  c

Even a known canonical head with missing local mutation data is not refreshed
by today's client pull. The earlier fresh canonical receiver is the control
showing that the server has the direct entry. This old known-head behaviour is
kept here; client-side mutation refresh is tested separately.

  $ cd "$TESTTMP"
  $ sl clone -q mono:repo known-receiver --noupdate
  $ cd known-receiver
  $ sl debugchangelog --migrate lazy
  $ sl pull -q -r "$P"
  $ setconfig pull.httpmutation=false
  $ sl pull -q -r "$S"
  $ setconfig pull.httpmutation=true
  $ sl pull -q -r "$S"
  $ sl debugpython -- "$TESTTMP/read-state.py" "$TESTTMP/marks.json"
  local P/C/S: True True True
  member split entry: absent
  point obsolete P: False
  bulk obsolete P: False
  raw successors P: absent
