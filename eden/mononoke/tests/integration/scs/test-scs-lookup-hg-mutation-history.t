# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"

Setup config repo:
  $ enable commitcloud amend
  $ POPULATE_GIT_MAPPING=1 setup_common_config
  $ cd "$TESTTMP"

  $ start_and_wait_for_mononoke_server

  $ hg clone -q mono:repo repo
  $ cd repo

First simple commit and amend:
  $ echo -e "a\nb\nc\nd\ne" > a
  $ hg commit -Aqm A
  $ hg whereami
  0ff73aedebab61e96d59d9fbe12b543b8f05b7af

  $ echo -e "f\nf\nh\ni\nj" >> a
  $ hg amend -Aqm A1
  $ COMMIT_A1=$(hg whereami)
  $ echo $COMMIT_A1
  57a4aac8aa711f496bf7cf7f972bc352f45086f5
  $ hg log
  commit:      57a4aac8aa71
  user:        test
  date:        Thu Jan 01 00:00:00 1970 +0000
  summary:     A1
  

Backup the commit to commit cloud (note that mutation data is only stored for commit-cloud commits).
  $ hg cloud backup -r .
  commitcloud: head '57a4aac8aa71' hasn't been uploaded yet
  edenapi: queue 1 commit for upload
  edenapi: queue 1 file for upload
  edenapi: uploaded 1 file
  edenapi: queue 1 tree for upload
  edenapi: uploaded 1 tree
  edenapi: uploaded 1 changeset

start SCS server
  $ start_and_wait_for_scs_server --scuba-log-file "$TESTTMP/scuba.json"

  $ scsc info --repo repo --hg-commit-id $COMMIT_A1
  Commit: 57a4aac8aa711f496bf7cf7f972bc352f45086f5
  Date: 1970-01-01 00:00:00 +00:00
  Author: test
  Generation: 1
  Extra:
      amend_source=0ff73aedebab61e96d59d9fbe12b543b8f05b7af
  
  A1


lookup hg_mutation_history for commit A1 - should return A (default format: commit-id)
  $ scsc hg-mutation-history --repo repo --hg-commit-id $COMMIT_A1
  0ff73aedebab61e96d59d9fbe12b543b8f05b7af

lookup hg_mutation_history for commit A1 - explicit commit-id format
  $ scsc hg-mutation-history --repo repo --hg-commit-id $COMMIT_A1 --format commit-id
  0ff73aedebab61e96d59d9fbe12b543b8f05b7af

lookup hg_mutation_history for commit A1 - hg-mutation format should show detailed mutation info
  $ scsc hg-mutation-history --repo repo --hg-commit-id $COMMIT_A1 --format hg-mutation
  Successor: 57a4aac8aa711f496bf7cf7f972bc352f45086f5
  Predecessors: 0ff73aedebab61e96d59d9fbe12b543b8f05b7af
  Operation: amend
  User: test
  Date: 1970-01-01 00:00:00 +00:00

Add a new commit and fold them together
  $ echo b1 > b
  $ hg add b
  $ hg commit -m "B1"
  $ hg fold --from .^
  2 changesets folded
  0 files updated, 0 files merged, 0 files removed, 0 files unresolved
  $ echo b2 > b
  $ hg amend
  $ hg debugmutation -r .
   *  21f5fa768b3369482def6e64b5c2de36d92bccaa amend by test at 1970-01-01T00:00:00 from:
      564931c09c4195684130dc0e0b7fbaf7bd9324f6 fold by test at 1970-01-01T00:00:00 from:
      |-  57a4aac8aa711f496bf7cf7f972bc352f45086f5 amend by test at 1970-01-01T00:00:00 from:
      |   0ff73aedebab61e96d59d9fbe12b543b8f05b7af
      '-  33675a5df4c06b8939ea8c16780b36858b9d1ed4
  

  $ hg cloud backup -r .
  commitcloud: head '21f5fa768b33' hasn't been uploaded yet
  edenapi: queue 1 commit for upload
  edenapi: queue 1 file for upload
  edenapi: uploaded 1 file
  edenapi: queue 1 tree for upload
  edenapi: uploaded 1 tree
  edenapi: uploaded 1 changeset
  $ COMMIT_AB=$(hg whereami)
  $ scsc hg-mutation-history --repo repo --hg-commit-id $COMMIT_AB --format hg-mutation
  Successor: 57a4aac8aa711f496bf7cf7f972bc352f45086f5
  Predecessors: 0ff73aedebab61e96d59d9fbe12b543b8f05b7af
  Operation: amend
  User: test
  Date: 1970-01-01 00:00:00 +00:00
  
  Successor: 564931c09c4195684130dc0e0b7fbaf7bd9324f6
  Predecessors: 57a4aac8aa711f496bf7cf7f972bc352f45086f5, 33675a5df4c06b8939ea8c16780b36858b9d1ed4
  Operation: fold
  User: test
  Date: 1970-01-01 00:00:00 +00:00
  
  Successor: 21f5fa768b3369482def6e64b5c2de36d92bccaa
  Predecessors: 564931c09c4195684130dc0e0b7fbaf7bd9324f6
  Operation: amend
  User: test
  Date: 1970-01-01 00:00:00 +00:00
  $ scsc --json hg-mutation-history --repo repo --hg-commit-id $COMMIT_AB --format hg-mutation
  {"mutations":[{"successor":"57a4aac8aa711f496bf7cf7f972bc352f45086f5","predecessors":["0ff73aedebab61e96d59d9fbe12b543b8f05b7af"],"split":[],"op":"amend","user":"test","timestamp":0,"tz":0,"date":"1970-01-01T00:00:00Z"},{"successor":"564931c09c4195684130dc0e0b7fbaf7bd9324f6","predecessors":["57a4aac8aa711f496bf7cf7f972bc352f45086f5","33675a5df4c06b8939ea8c16780b36858b9d1ed4"],"split":[],"op":"fold","user":"test","timestamp":0,"tz":0,"date":"1970-01-01T00:00:00Z"},{"successor":"21f5fa768b3369482def6e64b5c2de36d92bccaa","predecessors":["564931c09c4195684130dc0e0b7fbaf7bd9324f6"],"split":[],"op":"amend","user":"test","timestamp":0,"tz":0,"date":"1970-01-01T00:00:00Z"}]}
