
#require no-eden

# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# Load extensions

  $ eagerepo
  $ cat >> $HGRCPATH << 'EOF'
  > [extensions]
  > arcconfig=$TESTDIR/../sapling/ext/extlib/phabricator/arcconfig.py
  > arcdiff=
  > EOF

# Diff with no revision

  $ sl init repo
  $ cd repo
  $ touch foo
  $ sl add foo
  $ sl ci -qm 'No rev'
  $ sl diff --since-last-submit
  abort: local changeset is not associated with a differential revision
  (it has no diff yet: 'sl diff -c .' shows all its changes)
  [255]

  $ sl log -r 'lastsubmitted(.)' -T '{node} {desc}\n'
  abort: local changeset is not associated with a differential revision
  [255]

# Fake a diff

  $ echo bleet > foo
  $ sl ci -qm 'Differential Revision: https://phabricator.fb.com/D1'
  $ sl diff --since-last-submit
  abort: no .arcconfig found
  [255]

  $ sl log -r 'lastsubmitted(.)' -T '{node} {desc}\n'
  abort: no .arcconfig found
  [255]

# Prep configuration

  $ echo '{}' > .arcrc
  $ echo '{"config" : {"default" : "https://a.com/api"}, "hosts" : {"https://a.com/api/" : { "user" : "testuser", "oauth" : "garbage_cert"}}}' > .arcconfig

# Now progressively test the response handling for variations of missing data

  $ cat > $TESTTMP/mockduit << 'EOF'
  > [{}]
  > EOF
  $ HG_ARC_CONDUIT_MOCK=$TESTTMP/mockduit sl diff --since-last-submit
  Error calling graphql: Unexpected graphql response format
  abort: unable to determine previous changeset hash
  [255]

  $ HG_ARC_CONDUIT_MOCK=$TESTTMP/mockduit sl log -r 'lastsubmitted(.)' -T '{node} {desc}\n'
  Error calling graphql: Unexpected graphql response format
  abort: unable to determine previous changeset hash
  [255]

  $ cat > $TESTTMP/mockduit << 'EOF'
  > [{"data": {"query": [{"results": {"nodes": [{
  >   "number": 1,
  >   "diff_status_name": "Needs Review",
  >   "is_landing": false,
  >   "land_job_status": "NO_LAND_RUNNING",
  >   "needs_final_review_status": "NOT_NEEDED",
  >   "created_time": 123,
  >   "updated_time": 222
  > }]}}]}}]
  > EOF
  $ HG_ARC_CONDUIT_MOCK=$TESTTMP/mockduit sl diff --since-last-submit
  abort: unable to determine previous changeset hash
  [255]

  $ HG_ARC_CONDUIT_MOCK=$TESTTMP/mockduit sl log -r 'lastsubmitted(.)' -T '{node} {desc}\n'
  abort: unable to determine previous changeset hash
  [255]

  $ cat > $TESTTMP/mockduit << 'EOF'
  > [{"data": {"query": [{"results": {"nodes": [{
  >   "number": 1,
  >   "diff_status_name": "Needs Review",
  >   "is_landing": false,
  >   "land_job_status": "NO_LAND_RUNNING",
  >   "needs_final_review_status": "NOT_NEEDED",
  >   "created_time": 123,
  >   "updated_time": 222
  > }]}}]}}]
  > EOF
  $ HG_ARC_CONDUIT_MOCK=$TESTTMP/mockduit sl diff --since-last-submit
  abort: unable to determine previous changeset hash
  [255]

  $ HG_ARC_CONDUIT_MOCK=$TESTTMP/mockduit sl log -r 'lastsubmitted(.)' -T '{node} {desc}\n'
  abort: unable to determine previous changeset hash
  [255]

# This is the case when the diff is up to date with the current commit;
# there is no diff since what was landed.

  $ cat > $TESTTMP/mockduit << 'EOF'
  > [{"data": {"query": [{"results": {"nodes": [{
  >   "number": 1,
  >   "diff_status_name": "Needs Review",
  >   "latest_active_phabricator_version": {
  >      "commit_hash_best_effort": "2e6531b7dada2a3e5638e136de05f51e94a427f4"
  >   },
  >   "is_landing": false,
  >   "land_job_status": "NO_LAND_RUNNING",
  >   "needs_final_review_status": "NOT_NEEDED",
  >   "created_time": 123,
  >   "updated_time": 222
  > }]}}]}}]
  > EOF
  $ HG_ARC_CONDUIT_MOCK=$TESTTMP/mockduit sl diff --since-last-submit

  $ HG_ARC_CONDUIT_MOCK=$TESTTMP/mockduit sl log -r 'lastsubmitted(.)' -T '{node} {desc}\n'
  2e6531b7dada2a3e5638e136de05f51e94a427f4 Differential Revision: https://phabricator.fb.com/D1

# This is the case when the diff points at our parent commit, we expect to
# see the bleet text show up.  There's a fake hash that I've injected into
# the commit list returned from our mocked phabricator; it is present to
# assert that we order the commits consistently based on the time field.

  $ cat > $TESTTMP/mockduit << 'EOF'
  > [{"data": {"query": [{"results": {"nodes": [{
  >   "number": 1,
  >   "diff_status_name": "Needs Review",
  >   "latest_active_phabricator_version": {
  >      "commit_hash_best_effort": "88dd5a13bf28b99853a24bddfc93d4c44e07c6bd"
  >   },
  >   "is_landing": false,
  >   "land_job_status": "NO_LAND_RUNNING",
  >   "needs_final_review_status": "NOT_NEEDED",
  >   "created_time": 123,
  >   "updated_time": 222
  > }]}}]}}]
  > EOF
  $ HG_ARC_CONDUIT_MOCK=$TESTTMP/mockduit sl diff --since-last-submit --nodates
  diff -r 88dd5a13bf28 -r 2e6531b7dada foo
  --- a/foo
  +++ b/foo
  @@ -0,0 +1,1 @@
  +bleet

  $ HG_ARC_CONDUIT_MOCK=$TESTTMP/mockduit sl log -r 'lastsubmitted(.)' -T '{node} {desc}\n'
  88dd5a13bf28b99853a24bddfc93d4c44e07c6bd No rev

  $ HG_ARC_CONDUIT_MOCK=$TESTTMP/mockduit sl diff --since-last-submit-2o
  Phabricator rev: 88dd5a13bf28b99853a24bddfc93d4c44e07c6bd
  Local rev: 2e6531b7dada2a3e5638e136de05f51e94a427f4 (.)
  Changed: foo
  | ...
  | +bleet

# Make a new commit on top, and then use -r to look at the previous commit

  $ echo other > foo
  $ sl commit -m 'Other commmit'
  $ HG_ARC_CONDUIT_MOCK=$TESTTMP/mockduit sl diff --since-last-submit --nodates -r 2e6531b
  diff -r 88dd5a13bf28 -r 2e6531b7dada foo
  --- a/foo
  +++ b/foo
  @@ -0,0 +1,1 @@
  +bleet

  $ HG_ARC_CONDUIT_MOCK=$TESTTMP/mockduit sl log -r 'lastsubmitted(2e6531b)' -T '{node} {desc}\n'
  88dd5a13bf28b99853a24bddfc93d4c44e07c6bd No rev
