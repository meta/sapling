
  $ eagerepo
  $ enable fbcodereview
  $ setconfig fbcodereview.allow-diff-revision-drop=true

Setup repo

  $ sl init repo
  $ cd repo

Test phabdiff template mapping

  $ echo a > a
  $ sl commit -Aqm "Differential Revision: https://phabricator.fb.com/D1234
  > Task ID: 2312"
  $ sl log --template "{phabdiff}\n"
  D1234

  $ echo c > c
  $ sl commit -Aqm "Differential Revision: http://phabricator.intern.facebook.com/D1245
  > Task ID: 2312"
  $ sl log -r . --template "{phabdiff}\n"
  D1245

  $ echo b > b
  $ sl commit -Aqm "Differential Revision: https://phabricator.fb.com/D5678
  > Don't be fooled - Remaining Tasks: 15
  > Tasks:32, 44    55"
  $ sl log -r . --template "{phabdiff}: {tasks}\n"
  D5678: 32 44 55

  $ echo d > d
  $ sl commit -Aqm "Differential Revision: http://phabricator.intern.facebook.com/D1245
  > Task: t123456,456"
  $ sl log -r . --template "{phabdiff}: {tasks}\n"
  D1245: 123456 456

Only match the Differential Revision label at the start of a line

  $ echo e > e
  $ sl commit -Aqm "Test Commit
  > Test Plan: tested on Differential Revision: http://phabricator.intern.facebook.com/D1000
  > Differential Revision: http://phabricator.intern.facebook.com/D6789
  > "
  $ sl log -r . --template "{phabdiff}\n"
  D6789

Test reviewers label

  $ echo f > f
  $ sl commit -Aqm "Differential Revision: http://phabricator.intern.facebook.com/D9876
  > Reviewers: xlwang, quark durham, rmcelroy"
  $ sl log -r . --template '{reviewers}\n'
  xlwang quark durham rmcelroy
  $ sl log -r . --template '{reviewers % "- {reviewer}\n"}\n'
  - xlwang
  - quark
  - durham
  - rmcelroy
  
  $ echo g > g
  $ sl commit -Aqm "Differential Revision: http://phabricator.intern.facebook.com/D9876
  > Reviewers: xlwang quark"
  $ sl log -r . --template "{join(reviewers, ', ')}\n"
  xlwang, quark

Test reviewers for working copy

  $ enable debugcommitmessage
  $ sl debugcommitmessage --config 'committemplate.changeset={reviewers}' --config 'committemplate.reviewers=foo, {x}' --config 'committemplate.x=bar'
  foo, bar (no-eol)

  $ sl debugcommitmessage --config 'committemplate.changeset=A{reviewers}B'
  AB (no-eol)

Without `committemplate.reviewers`, the working copy has an empty list of
reviewers, so the list operations work on it:

  $ sl log -r 'wdir()' -T '{reviewers % "[{reviewer}]"}|{join(reviewers, ",")}|\n'
  ||

Make sure the template keywords are documented correctly

  $ sl help templates | grep -E 'phabdiff|tasks'
      phabdiff      String. Return the phabricator diff id for a given sl rev.
      tasks         String. Return the tasks associated with given sl rev.
      blame_phabdiffid

Check singlepublicbase

  $ sl log -r . --template "{singlepublicbase}\n"
  

  $ sl debugmakepublic -r ::0528335601bcec6b27caa75e1091bd32151ca916

  $ sl log -r . --template "{singlepublicbase}\n"
  0528335601bcec6b27caa75e1091bd32151ca916

Check sl backout template listing the diff properly
  $ echo h > h
  $ sl commit -Aqm "Differential Revision: https://phabricator.intern.facebook.com/D98765"
  $ sl log -l 1 --template "{phabdiff}\n"
  D98765
  $ sl backout -r . -m "Some default message to avoid the interactive editor" -q
  $ sl log -l 1 --template '{desc}' | grep -q "Original Phabricator Diff: D98765" && echo found
  found
