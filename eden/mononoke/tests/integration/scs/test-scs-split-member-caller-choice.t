# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

#testcases follow-splits default-request emergency-off

  $ . "${TEST_FIXTURES}/library.sh"
  $ configure modern
  $ enable amend commitcloud
  $ export CHGDISABLE=1
  $ INFINITEPUSH_ALLOW_WRITES=true setup_common_config
  $ setconfig mutation.enabled=true mutation.record=true visibility.enabled=true
  $ merge_just_knobs <<'EOF'
  > {"bools":{"scm/mononoke:enable_split_member_mutation_lookup":true}}
  > EOF
#if follow-splits
  $ FOLLOW_SPLITS=--follow-splits
#endif
#if default-request
  $ FOLLOW_SPLITS=
#endif
#if emergency-off
  $ FOLLOW_SPLITS=--follow-splits
  $ merge_just_knobs <<'EOF'
  > {"bools":{"scm/mononoke:enable_split_member_mutation_lookup":false}}
  > EOF
#endif
  $ cd "$TESTTMP"
  $ start_and_wait_for_mononoke_server
  $ sl clone -q mono:repo producer
  $ cd producer
  $ sl debugimportstack > "$TESTTMP/marks.json" <<'EOF'
  > [["commit", {"text": "original", "mark": ":p", "files": {"a": {"data": "a"}, "b": {"data": "b"}}}],
  >  ["commit", {"text": "member", "mark": ":c", "predecessors": [":p"], "files": {"a": {"data": "a"}}}],
  >  ["commit", {"text": "canonical", "mark": ":s", "predecessors": [":p"], "parents": [":c"], "operation": "split", "files": {"b": {"data": "b"}}}]]
  > EOF
  $ C=$(jq -r '.[":c"]' "$TESTTMP/marks.json")
  $ S=$(jq -r '.[":s"]' "$TESTTMP/marks.json")
  $ sl cloud backup -q -r "$S"
  $ cat > "$TESTTMP/received.py" <<'PY'
  > import sys
  > from sapling import hg, mutation, node, ui
  > repo = hg.repository(ui.ui(), ".")
  > c, s = map(node.bin, sys.argv[1:])
  > known = set(repo.changelog.filternodes([c, s], local=True))
  > print("member/canonical local:", c in known, s in known)
  > entry = mutation.lookupsplit(repo, c)
  > print("canonical entry received:", entry is not None)
  > if entry is not None:
  >     print("canonical owner/op/split:", entry.succ() == s, entry.op(), entry.split() == [c])
  > PY
  $ cat > "$TESTTMP/scs-entry.py" <<'PY'
  > import json
  > import sys
  > entries = json.load(sys.stdin)["mutations"]
  > print("SCS history entries:", len(entries))
  > if entries:
  >     entry = entries[0]
  >     print("SCS canonical owner/op/split:", entry["successor"] == sys.argv[1], entry["op"], entry["split"] == [sys.argv[2]])
  > PY

A new HTTP member pull uses the unchanged client path in both states. An empty
client result must not be replaced by the SCS-only canonical member result.
Mononoke does not advertise the legacy getbundle capability: the debug command
must keep refusing for both member and canonical heads, with no bundle file.
These checks do not claim to decode a mutation bundle that was never produced.

  $ cd "$TESTTMP"
  $ sl clone -q mono:repo receiver --noupdate
  $ cd receiver
  $ sl debugchangelog --migrate lazy
  $ LOG=pull sl pull -q -r "$C" > "$TESTTMP/pull.stdout" 2> "$TESTTMP/pull.stderr"
  $ grep -q 'pull::httpgraph' "$TESTTMP/pull.stderr"
  $ sl debugpython -- "$TESTTMP/received.py" "$C" "$S"
  member/canonical local: True False
  canonical entry received: False
  $ sl debuggetbundle mono:repo "$TESTTMP/member.bundle" --head "$C" --type bundle2
  abort: getbundle() not supported by target repository
  [255]
  $ test ! -e "$TESTTMP/member.bundle"

The SCS read selects split-member lookup only with both caller opt-in and the
emergency switch enabled. Overrides here are local to the fixture.

  $ start_and_wait_for_scs_server --scuba-log-file "$TESTTMP/scs-scuba.json"
#if follow-splits
  $ scsc --json hg-mutation-history --repo repo --hg-commit-id "$C" --format hg-mutation $FOLLOW_SPLITS | python "$TESTTMP/scs-entry.py" "$S" "$C"
  SCS history entries: 1
  SCS canonical owner/op/split: True split True
#endif
#if default-request
  $ scsc --json hg-mutation-history --repo repo --hg-commit-id "$C" --format hg-mutation $FOLLOW_SPLITS | python "$TESTTMP/scs-entry.py" "$S" "$C"
  SCS history entries: 0
#endif
#if emergency-off
  $ scsc --json hg-mutation-history --repo repo --hg-commit-id "$C" --format hg-mutation $FOLLOW_SPLITS | python "$TESTTMP/scs-entry.py" "$S" "$C"
  SCS history entries: 0
#endif
  $ scsc --json hg-mutation-history --repo repo --hg-commit-id "$C" --format hg-mutation | python "$TESTTMP/scs-entry.py" "$S" "$C"
  SCS history entries: 0

A fresh HTTP receiver stays legacy even after SCS populated its split-member
cache. This checks the caller-selected policy, not just the split-row codec.

  $ cd "$TESTTMP"
  $ sl clone -q mono:repo after-scs --noupdate
  $ cd after-scs
  $ sl debugchangelog --migrate lazy
  $ sl pull -q -r "$C"
  $ sl debugpython -- "$TESTTMP/received.py" "$C" "$S"
  member/canonical local: True False
  canonical entry received: False

The final child's direct mutation entry is unchanged for both interfaces and
both caller-selected modes. Receiving it is not enabling client member lookup.

  $ sl pull -q -r "$S"
  $ sl debugpython -- "$TESTTMP/received.py" "$C" "$S"
  member/canonical local: True True
  canonical entry received: True
  canonical owner/op/split: True split True
  $ sl debuggetbundle mono:repo "$TESTTMP/final.bundle" --head "$S" --type bundle2
  abort: getbundle() not supported by target repository
  [255]
  $ test ! -e "$TESTTMP/final.bundle"
  $ scsc --json hg-mutation-history --repo repo --hg-commit-id "$S" --format hg-mutation | python "$TESTTMP/scs-entry.py" "$S" "$C"
  SCS history entries: 1
  SCS canonical owner/op/split: True split True
  $ scsc --json hg-mutation-history --repo repo --hg-commit-id "$S" --format hg-mutation $FOLLOW_SPLITS | python "$TESTTMP/scs-entry.py" "$S" "$C"
  SCS history entries: 1
  SCS canonical owner/op/split: True split True

Opt-in request logs distinguish the requested flag from the effective read.

  $ cat > "$TESTTMP/check-mode.py" <<'PY'
  > import json
  > import sys
  > with open(sys.argv[1]) as source:
  >     samples = [json.loads(line) for line in source if line.strip()]
  > modes = [sample["normal"]["follow_splits_effective"] for sample in samples
  >          if "follow_splits_effective" in sample.get("normal", {})]
  > expected = int(sys.argv[2])
  > assert modes and all(mode == str(bool(expected)).lower() for mode in modes), modes
  > print("effective split following:", expected)
  > PY
#if follow-splits
  $ python "$TESTTMP/check-mode.py" "$TESTTMP/scs-scuba.json" 1
  effective split following: 1
#endif
#if emergency-off
  $ python "$TESTTMP/check-mode.py" "$TESTTMP/scs-scuba.json" 0
  effective split following: 0
#endif

