
#require eden

  $ eagerepo
  $ enable edensparse
  $ setconfig clone.use-rust=true

  $ newrepo server
  $ echo foo > included
  $ echo foo > excluded
  $ echo path:included > sparse
  $ touch eden-sparse
  $ sl commit -Aqm a
  $ sl book master

  $ cd
  $ sl clone -q --eden test:server client --config clone.eden-sparse-filter=eden-sparse
  $ cd client

Allow adhoc use of sparse commands to debug sparse profiles:
  $ sl debugsparsematch -q --sparse-profile=sparse excluded --config extensions.sparse=

Reading a revision still works when sparse is loaded but edensparse is not. The
repo keeps the edensparse requirement while neither reposetup supplies
sparsematch(), so cat must not assume the method exists:
  $ sl cat -r . included --config extensions.sparse= --config extensions.edensparse=!
  foo

Test diff command against a commit that updated files excluded by the sparse profile

  $ cd
  $ newrepo server-diff
  $ echo aaa > a.txt
  $ sl commit -Aqm a
  $ echo bbb > b.txt
  $ sl commit -Aqm b
  $ echo ccc > a.txt
  $ echo ccc > b.txt
  $ sl commit -Aqm c
  $ cat >> eden-sparse << EOF
  > [include]
  > *
  > [exclude]
  > b.txt
  > EOF
  $ sl commit -Aqm d
  $ sl book master

  $ cd
  $ sl clone -q --eden test:server-diff client-diff --config clone.eden-sparse-filter=eden-sparse
  $ cd client-diff
  $ sl diff -r 'desc(b)' --stat
   a.txt       |  2 +-
   b.txt       |  2 +-
   eden-sparse |  4 ++++
   3 files changed, 6 insertions(+), 2 deletions(-)
  $ sl diff -r 'desc(b)' --stat --sparse
   a.txt       |  2 +-
   eden-sparse |  4 ++++
   2 files changed, 5 insertions(+), 1 deletions(-)

Don't crash with extensions in this state
  $ sl diff -r 'desc(b)' --stat --config extensions.sparse= --config extensions.edensparse=!
  abort: *$ENOENT$* (glob)
  [255]

Profiles that %include other profiles are not valid filters

  $ cd
  $ newrepo server-nested
  $ echo foo > file
  $ touch flat base
  $ cat > nested << EOF
  > %include base
  > [include]
  > *
  > EOF
  $ sl commit -Aqm a
  $ sl book master

  $ cd
  $ sl clone -q --eden test:server-nested client-nested --config clone.eden-sparse-filter=flat
  $ cd client-nested
  $ sl filteredfs enable nested
  Manually modifying filters is not recommended. Do you still wish to continue? (yn)? y
  abort: 'nested' is not a valid filter: filters cannot %include other profiles
  (if enabled, run 'sl filteredfs disable nested')
  [255]

A checkout that already has one enabled gets the same abort and can disable it

  $ cd
  $ sl clone -q --eden test:server-nested client-enabled --config clone.eden-sparse-filter=nested
  $ cd client-enabled
  $ echo bar > file
  $ sl commit -qm b
  abort: 'nested' is not a valid filter: filters cannot %include other profiles
  (if enabled, run 'sl filteredfs disable nested')
  [255]
  $ sl filteredfs show
  Enabled Profiles:
  
  abort: 'nested' is not a valid filter: filters cannot %include other profiles
  (if enabled, run 'sl filteredfs disable nested')
  [255]
  $ sl filteredfs disable nested
  Manually modifying filters is not recommended. Do you still wish to continue? (yn)? y
  $ sl commit -qm b
