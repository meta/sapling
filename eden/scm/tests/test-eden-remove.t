
#require eden

setup backing repo

  $ newclientrepo backingrepo
  $ eden clone --allow-empty-repo $TESTTMP/backingrepo $TESTTMP/wcrepo
  Cloning new repository at $TESTTMP/wcrepo...
  Success.  Checked out commit 00000000

Do not provide both '-y' and '-n'
  $ EDENFSCTL_ONLY_RUST=true eden remove -q -y -n $TESTTMP/wcrepo/test_dir
  Error: Both '-y' and '-n' are provided. This is not supported.
  Exiting.
  [1]

touch a test file
  $ touch $TESTTMP/wcrepo/file.txt

eden remove this file, answer no when there is prompt
  $ EDENFSCTL_ONLY_RUST=true eden remove -q -n $TESTTMP/wcrepo/file.txt
  Error: User did not confirm the removal. Stopping. Nothing removed!
  [1]

the file is still there
  $ ls $TESTTMP/wcrepo/file.txt | wc -l
  1

eden remove this file with "yes": a file is refused, not deleted
  $ EDENFSCTL_ONLY_RUST=true eden remove -q -y $TESTTMP/wcrepo/file.txt
  error: $TESTTMP/wcrepo/file.txt exists but it's a file; remove it manually.
  [66]

the file is still there
  $ ls $TESTTMP/wcrepo/file.txt | wc -l
  1

create a directory outside of any eden repo
  $ mkdir $TESTTMP/i-am-not-eden

eden remove this directory, answer "no" when there is prompt
  $ EDENFSCTL_ONLY_RUST=true eden remove -q -n $TESTTMP/i-am-not-eden
  Error: User did not confirm the removal. Stopping. Nothing removed!
  [1]

the directory should still be there
  $ ls $TESTTMP/i-am-not-eden | wc -l
  0

eden remove this directory, answer "yes" when there is prompt
  $ EDENFSCTL_ONLY_RUST=true eden remove -q -y $TESTTMP/i-am-not-eden

the directory should now be gone
  $ ls $TESTTMP/i-am-not-eden
  ls: $TESTTMP/i-am-not-eden: $ENOENT$
  [1]

create the directory outside of any eden repo again
  $ mkdir $TESTTMP/i-am-not-eden

this time, put some files into it
  $ touch $TESTTMP/i-am-not-eden/file

eden remove this directory with "yes": a non-empty directory that is not a
checkout is refused, because the prompt that protects its content is skipped
  $ EDENFSCTL_ONLY_RUST=true eden remove -q -y $TESTTMP/i-am-not-eden
  error: $TESTTMP/i-am-not-eden is not an EdenFS mount and is not empty; refusing to delete it without confirmation
  [66]

the directory and its file are still there
  $ ls $TESTTMP/i-am-not-eden
  file

create a test directory inside eden

  $ mkdir $TESTTMP/wcrepo/test_dir

eden remove this directory should just give error since it's not the root dir
  $ EDENFSCTL_ONLY_RUST=true eden remove -q -y $TESTTMP/wcrepo/test_dir
  error: $TESTTMP/wcrepo/test_dir is not the root of checkout $TESTTMP/wcrepo, not removing
  [66]

eden list should give two repos
  $ eden list | wc -l
  2

remove wcrepo with eden rust cli should succeed

  $ EDENFSCTL_ONLY_RUST=true eden remove -q -y $TESTTMP/wcrepo

eden list should now give only one repo
  $ eden list | wc -l
  1

check to make sure the mount point is cleanly removed
  $ ls $TESTTMP/wcrepo
  ls: $TESTTMP/wcrepo: $ENOENT$
  [1]

reclone a repo for testing validation error
  $ eden clone --allow-empty-repo $TESTTMP/backingrepo $TESTTMP/wcrepo
  Cloning new repository at $TESTTMP/wcrepo...
  Success.  Checked out commit 00000000
  
remove with failpoint set so the validation step will fail
  $ FAILPOINTS=remove:validate=return EDENFSCTL_ONLY_RUST=true eden remove -q -y $TESTTMP/wcrepo/
  Error: failpoint: expected failure
  [1]

reclone for testing the removal of multiple checkouts
  $ eden clone --allow-empty-repo $TESTTMP/backingrepo $TESTTMP/wcrepo-1
  Cloning new repository at $TESTTMP/wcrepo-1...
  Success.  Checked out commit 00000000
  $ eden clone --allow-empty-repo $TESTTMP/backingrepo $TESTTMP/wcrepo-2
  Cloning new repository at $TESTTMP/wcrepo-2...
  Success.  Checked out commit 00000000

remove multiplt checkouts
  $ EDENFSCTL_ONLY_RUST=true eden remove -q -y $TESTTMP/wcrepo-2 $TESTTMP/wcrepo-1

check to make sure the checkouts are cleanly removed
  $ ls $TESTTMP/wcrepo-2
  ls: $TESTTMP/wcrepo-2: $ENOENT$
  [1]

  $ ls $TESTTMP/wcrepo-1
  ls: $TESTTMP/wcrepo-1: $ENOENT$
  [1]
