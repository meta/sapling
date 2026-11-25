#modern-config-incompatible

#require no-eden

# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

  $ cat >> $HGRCPATH << 'EOF'
  > [extensions]
  > fbcodereview=
  > EOF
  $ enable amend
  $ sl init repo
  $ cd repo
  $ echo 1 > 1
  $ sl add 1
  $ HGPLAIN=1 sl commit -m "$(printf 'title\n\nDifferential Revision: http.ololo.com/D1234')"
  $ sl up -q 'desc(title)'
  $ sl up D1234
  Could not determine Phabricator callsign to resolve D1234.
    To fix this configuration issue:
      1. Edit repo root's .arcconfig (JSON) to include:
        {"repository.callsign": "YOUR_PHABRICATOR_REPO_CALLSIGN"}
      2. Commit the change.
    To test callsign config temporarily, re-run with:
      --config phrevset.callsign=CALLSIGN
  abort: unknown revision 'D1234'!
  [255]

  $ sl log -r D1234 --config phrevset.graphqlonly=False
  Could not determine Phabricator callsign to resolve D1234.
    To fix this configuration issue:
      1. Edit repo root's .arcconfig (JSON) to include:
        {"repository.callsign": "YOUR_PHABRICATOR_REPO_CALLSIGN"}
      2. Commit the change.
    To test callsign config temporarily, re-run with:
      --config phrevset.callsign=CALLSIGN
  Scanning D1234. This can diverage from Phabricator source of truth.
  commit:      26c3df7062ea
  user:        test
  date:        Thu Jan 01 00:00:00 1970 +0000
  summary:     title

  $ drawdag << 'EOS'
  > A  > EOS
  $ setconfig phrevset.mock-D1234=$A phrevset.callsign=CALLSIGN
  $ sl log -r D1234 -T '{desc}\n'
  A

# Callsign is invalid

  $ sl log -r D1234 --config phrevset.callsign=C -T '{desc}\n'
  abort: Diff callsign 'CALLSIGN' does not match repo callsigns '['C']'
  [255]

# Now we have two callsigns, and one of them is correct. Make sure it works

  $ sl log -r D1234 --config phrevset.callsign=C,CALLSIGN -T '{desc}\n'
  A

# Callsign set by .arcconfig works when phrevset.callsign is absent

  $ echo '{"repository.callsign":"CALLSIGN"}' > .arcconfig
  $ sl commit -m 'add arcconfig' -A .arcconfig
  $ sl log -r D1234 --config phrevset.callsign= -T '{desc}\n'
  A

# Phabricator provides an unknown commit hash.

  $ setconfig phrevset.mock-D1234=6008bb23d775556ff6c3528541ca5a2177b4bb92
  $ sl log -r D1234 -T '{desc}\n'
  abort: unknown revision 'D1234'!
  [255]

# Agent automation gets actionable guidance instead of a generic missing-revision error.

  $ HGPLAIN=1 CODING_AGENT_METADATA=id=test_agent sl pull -r D1234 --config paths.default=test:fake_server --config devel.print-metrics=phrevset.automation_diff_id
  pulling from test:fake_server
  abort: Differential ID resolution is disabled for automation because the resolved commit hash is unstable
  (D1234 resolves to 6008bb23d775556ff6c3528541ca5a2177b4bb92)
  phrevset.automation_diff_id_rejected: 1
  [255]

# Omit the hint when Phabricator does not resolve the Differential ID.

  $ HGPLAIN=1 CODING_AGENT_METADATA=id=test_agent sl pull -r D1234 --config phrevset.mock-D1234=missing --config paths.default=test:fake_server --config devel.print-metrics=phrevset.automation_diff_id
  pulling from test:fake_server
  abort: Differential ID resolution is disabled for automation because the resolved commit hash is unstable
  phrevset.automation_diff_id_rejected: 1
  [255]

# 'pull -r Dxxx' will be rewritten to 'pull -r HASH' for interactive users

  $ sl pull -r D1234 --config paths.default=test:fake_server
  pulling from test:fake_server
  rewriting pull rev 'D1234' into '6008bb23d775556ff6c3528541ca5a2177b4bb92'
  abort: unknown revision '6008bb23d775556ff6c3528541ca5a2177b4bb92'!
  [255]

# Ambiguous local successors warn and default to the newest revision.

  $ setconfig mutation.record=true mutation.enabled=true phrevset.mock-local-D1234=$A
  $ sl goto -q $A
  $ HGPLAIN=1 sl amend -q -d "1000 0" -m "$(printf 'older successor\n\nDifferential Revision: https://phabricator.intern.facebook.com/D1234')"
  $ sl goto -q --hidden --config commit.modify-obsolete-mode=ignore $A
  $ HGPLAIN=1 sl amend -q -d "2000 0" -m "$(printf 'newer successor\n\nDifferential Revision: https://phabricator.intern.facebook.com/D1234')"

  $ sl log -r D1234 -T '{desc|firstline}\n'
  D1234 resolves ambiguously to multiple local commits:
    1: * 1970-01-01 00:33:20 +0000 newer successor (glob)
    2: * 1970-01-01 00:16:40 +0000 older successor (glob)
  which commit to select [1-2/(c)ancel]?  1
  warning: selected * for D1234 (glob)
  newer successor

# An interactive user may still cancel.

  $ sl log -r D1234 --config ui.interactive=true -T '{desc|firstline}\n' <<EOF
  > c
  > EOF
  D1234 resolves ambiguously to multiple local commits:
    1: * 1970-01-01 00:33:20 +0000 newer successor (glob)
    2: * 1970-01-01 00:16:40 +0000 older successor (glob)
  which commit to select [1-2/(c)ancel]?  c
  abort: ambiguous commit for D1234
  (set 'phrevset.prompt-ambiguous-successors=false' to restore automatic selection)
  [255]

# Plain mode preserves newest-revision selection for automation.

  $ HGPLAIN=1 sl log -r D1234 -T '{desc|firstline}\n'
  newer successor

  $ sl log -r D1234 --config ui.interactive=true -T '{desc|firstline}\n' <<EOF
  > 2
  > EOF
  D1234 resolves ambiguously to multiple local commits:
    1: * 1970-01-01 00:33:20 +0000 newer successor (glob)
    2: * 1970-01-01 00:16:40 +0000 older successor (glob)
  which commit to select [1-2/(c)ancel]?  2
  warning: selected * for D1234 (glob)
  older successor

# The config override preserves newest-revision selection.

  $ sl log -r D1234 --config phrevset.prompt-ambiguous-successors=false -T '{desc|firstline}\n'
  newer successor

# Agents should abort with full hashes and obsolete status instead of guessing.

  $ CODING_AGENT_METADATA=id=test_agent sl log -r D1234 -T '{desc|firstline}\n'
  D1234 resolves ambiguously to multiple local commits:
    1: [0-9a-f]{40} 1970-01-01 00:33:20 \+0000 newer successor \(not obsolete\) (re)
    2: [0-9a-f]{40} 1970-01-01 00:16:40 \+0000 older successor \(not obsolete\) (re)
  abort: ambiguous commit for D1234
  (inspect mutation history with 'sl log -G --mutation -r "mutationhistory(HASH)"', then specify the intended commit hash directly)
  [255]

# Plain mode and the config override retain automatic selection for agents.

  $ HGPLAIN=1 CODING_AGENT_METADATA=id=test_agent sl log -r D1234 -T '{desc|firstline}\n'
  newer successor
  $ CODING_AGENT_METADATA=id=test_agent sl log -r D1234 --config phrevset.prompt-ambiguous-successors=false -T '{desc|firstline}\n'
  newer successor

# An obsolete matching successor can remain visible through its descendants.

  $ newclientrepo
  $ drawdag <<'EOS'
  > A
  > EOS
  $ setconfig mutation.record=true mutation.enabled=true phrevset.callsign=CALLSIGN phrevset.mock-local-D1234=$A
  $ sl goto -q $A
  $ HGPLAIN=1 sl amend -q -d "1000 0" -m "$(printf 'stale successor\n\nDifferential Revision: https://phabricator.intern.facebook.com/D1234')"
  $ STALE=$(sl log -r . -T '{node}')
  $ echo child > child
  $ sl commit -qAm child
  $ sl goto -q $STALE
  $ HGPLAIN=1 sl amend -q --no-rebase -d "2000 0" -m "$(printf 'latest successor\n\nDifferential Revision: https://phabricator.intern.facebook.com/D1234')" > /dev/null
  $ CODING_AGENT_METADATA=id=test_agent sl log -r D1234 -T '{desc|firstline}\n'
  D1234 resolves ambiguously to multiple local commits:
    1: [0-9a-f]{40} 1970-01-01 00:33:20 \+0000 latest successor \(not obsolete\) (re)
    2: [0-9a-f]{40} 1970-01-01 00:16:40 \+0000 stale successor \(obsolete\) (re)
  abort: ambiguous commit for D1234
  (inspect mutation history with 'sl log -G --mutation -r "mutationhistory(HASH)"', then specify the intended commit hash directly)
  [255]
  $ sl log -r . -T '{desc|firstline}\n'
  latest successor
