
#require no-eden

# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# Setup

  $ enable fbcodereview smartlog amend
  $ sl init repo
  $ cd repo
  $ echo 1 > foo
  $ sl ci -qAm 'base'
  $ echo 2 > foo
  $ HGPLAIN=1 sl amend -qm 'Differential Revision: https://phabricator.fb.com/D1'
  $ echo 3 > foo
  $ sl amend -q
  $ echo 4 > foo
  $ sl amend -q
  $ echo 5 > foo
  $ sl amend -q

# With an invalid arc configuration

  $ sl log -T '{diffversion}\n' -r .
  arcconfig configuration problem. No diff information can be provided.
  Error info: no .arcconfig found
  Error

# Configure arc...

  $ echo '{}' > .arcrc
  $ echo '{"config" : {"default" : "https://a.com/api"}, "hosts" : {"https://a.com/api/" : { "user" : "testuser", "oauth" : "garbage_cert"}}}' > .arcconfig

# And now with bad responses:

  $ cat > $TESTTMP/mockduit << 'EOF'
  > [{}]
  > EOF
  $ HG_ARC_CONDUIT_MOCK=$TESTTMP/mockduit sl log -T '{diffversion}\n' -r .
  Error talking to phabricator. No diff information can be provided.
  Error info: Unexpected graphql response format
  Error

  $ cat > $TESTTMP/mockduit << 'EOF'
  > [{"errors": [{"message": "failed, yo"}]}]
  > EOF
  $ HG_ARC_CONDUIT_MOCK=$TESTTMP/mockduit sl log -T '{diffversion}\n' -r .
  Error talking to phabricator. No diff information can be provided.
  Error info: failed, yo
  Error

# Test diff version matches all hashes.

  $ cat > $TESTTMP/mockduit << 'EOF'
  > [{"data": {"query": [{"results": {"nodes": [{
  >   "number": 1,
  >   "diff_status_name": "Approved",
  >   "latest_active_phabricator_version": {
  >     "commit_hash_best_effort": "ceef473a2ce1e50f05341e7b9f7ad8b4335248ac"
  >   },
  >   "phabricator_versions": {
  >     "nodes": [
  >       {
  >         "ordinal_label": {"abbreviated": "V1"},
  >         "commit_hash_best_effort": "ceef473a2ce1e50f05341e7b9f7ad8b4335248ac"
  >       }
  >     ]
  >   },
  >   "unpublished_phabricator_versions": [{
  >     "phabricator_version_migration": {
  >         "ordinal_label": {"abbreviated": "V0.1"},
  >         "commit_hash_best_effort": "fb01aa127551402e09df1d6e976e8ecea4983ea3"
  >       }
  >   }],
  >   "is_landing": false,
  >   "land_job_status": "NO_LAND_RUNNING",
  >   "needs_final_review_status": "NOT_NEEDED",
  >   "created_time": 123,
  >   "updated_time": 222
  > }]}}]}}]
  > EOF

  $ HG_ARC_CONDUIT_MOCK=$TESTTMP/mockduit sl log -T '{node|short}: {diffversion}\n' -r 'predecessors(.)'
  e08c689390e1: 
  fb01aa127551: V0.1
  aa5c3457d4ab: V0.1 (+ local changes)
  ceef473a2ce1: V1 (latest)
  589e1c5df135: V1 (latest + local changes)

# The mutation history shows the diff version each commit was submitted as,
# including hidden earlier versions, with one request for all of their diffs.

  $ newrepo history
  $ cp $TESTTMP/repo/.arcrc $TESTTMP/repo/.arcconfig .
  $ echo base > base
  $ sl commit -qAm base
  $ printf 'A\n\nDifferential Revision: https://phabricator.fb.com/D2\n' > $TESTTMP/msg2
  $ printf 'B\n\nDifferential Revision: https://phabricator.fb.com/D3\n' > $TESTTMP/msg3
  $ echo a > a
  $ HGPLAIN=1 sl commit -qA -l $TESTTMP/msg2
  $ echo a2 > a
  $ sl amend -q
  $ echo a3 > a
  $ sl amend -q
  $ echo b > b
  $ HGPLAIN=1 sl commit -qA -l $TESTTMP/msg3
  $ HGPLAIN=1 sl fold -q --from '.^' -l $TESTTMP/msg2

  $ cat > $TESTTMP/mockduit << 'EOF'
  > [{"data": {"query": [{"results": {"nodes": [{
  >   "number": 2,
  >   "diff_status_name": "Needs Review",
  >   "latest_active_phabricator_version": {
  >     "commit_hash_best_effort": "5278ac833b522186ab4f6e63cacec656d97cf3b7"
  >   },
  >   "phabricator_versions": {
  >     "nodes": [
  >       {
  >         "ordinal_label": {"abbreviated": "V1"},
  >         "commit_hash_best_effort": "87ad26c9aa9deb7c682021bf3afbea997b130c5b"
  >       },
  >       {
  >         "ordinal_label": {"abbreviated": "V2"},
  >         "commit_hash_best_effort": "426706ca3aa85d1bfc1d44d9f4ab31c3e378865a"
  >       },
  >       {
  >         "ordinal_label": {"abbreviated": "V3"},
  >         "commit_hash_best_effort": "5278ac833b522186ab4f6e63cacec656d97cf3b7"
  >       }
  >     ]
  >   },
  >   "unpublished_phabricator_versions": [],
  >   "is_landing": false,
  >   "land_job_status": "NO_LAND_RUNNING",
  >   "needs_final_review_status": "NOT_NEEDED",
  >   "created_time": 123,
  >   "updated_time": 222
  > }, {
  >   "number": 3,
  >   "diff_status_name": "Needs Review",
  >   "latest_active_phabricator_version": {
  >     "commit_hash_best_effort": "338462ff91817581efd516ed5062fcbdb9833150"
  >   },
  >   "phabricator_versions": {
  >     "nodes": [
  >       {
  >         "ordinal_label": {"abbreviated": "V1"},
  >         "commit_hash_best_effort": "338462ff91817581efd516ed5062fcbdb9833150"
  >       }
  >     ]
  >   },
  >   "unpublished_phabricator_versions": [],
  >   "is_landing": false,
  >   "land_job_status": "NO_LAND_RUNNING",
  >   "needs_final_review_status": "NOT_NEEDED",
  >   "created_time": 123,
  >   "updated_time": 222
  > }]}}]}}]
  > EOF

  $ HG_ARC_CONDUIT_MOCK=$TESTTMP/mockduit sl log -G --mutation -T '{node|short} {diffversion}\n'
  @    5d4db2d99bdb V3 (latest + local changes)
  ├─╮
  │ x  338462ff9181 V1 (latest)
  │
  x  5278ac833b52 V3 (latest)
  │
  x  426706ca3aa8 V2
  │
  x  87ad26c9aa9d V1
  $ HG_ARC_CONDUIT_MOCK=$TESTTMP/mockduit sl log -G --mutation -T '{diffversion}\n' --debug 2>&1 >/dev/null | grep 'prefetch phabstatus'
  prefetch phabstatus for ['2', '3']

# `ssl -t` (smartlog --mutation) shows each commit's diff version in the `ssl`
# layout. `ssl` still shows diff versions only with -v.

  $ HG_ARC_CONDUIT_MOCK=$TESTTMP/mockduit TEST_PROD_CONFIGS=1 sl ssl -t
  @    fold from 5278ac833b52, 338462ff9181  test  Today at 00:00
  ├─╮  5d4db2d99  Today at 00:00  test  D2 Needs Review V3 (latest + local changes)
  │ │  A
  │ │
  │ x  338462ff9 [Folded into 5d4db2d99bdb]  Today at 00:00  test  D3 Needs Review V1 (latest)
  │    B
  │
  x  amend from 426706ca3aa8  test  Today at 00:00
  │  5278ac833 [Folded into 5d4db2d99bdb]  Today at 00:00  test  D2 Needs Review V3 (latest)
  │  A
  │
  x  amend from 87ad26c9aa9d  test  Today at 00:00
  │  426706ca3 [Rewritten into 5d4db2d99bdb]  Today at 00:00  test  D2 Needs Review V2
  │  A
  │
  x  87ad26c9a [Rewritten into 5d4db2d99bdb]  Today at 00:00  test  D2 Needs Review V1
     A
  $ HG_ARC_CONDUIT_MOCK=$TESTTMP/mockduit TEST_PROD_CONFIGS=1 sl ssl
  @  5d4db2d99  Today at 00:00  test  D2 Needs Review (local changes)
  │  A
  │
  o  2d7d13640  Today at 00:00  test
     base
