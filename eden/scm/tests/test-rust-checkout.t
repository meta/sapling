
  $ setconfig checkout.use-rust=true
  $ setconfig experimental.nativecheckout=true

  $ newclientrepo
  $ drawdag <<'EOS'
  > A   # A/foo = foo
  >     # A/bar = bar
  > EOS

#if eden

Quick check for making sure this test is capable of using EdenFS
  $ ls -a $TESTTMP/.eden-backing-repos
  repo1

#endif


Unknown file w/ different content - conflict:
  $ echo nope > foo
  $ sl go $A
  abort: 1 conflicting file changes:
   foo
  (commit, shelve, goto --clean to discard all your changes, or goto --merge to merge them)
  [255]

Checking out to diff without file where file removed locally
  $ newclientrepo
  $ drawdag <<EOS
  > B  # B/file = foo
  > |
  > A
  > EOS
  $ sl go $B -qC
  $ sl rm file
  $ sl go $A
  abort: 1 conflicting file changes:
   file
  (commit, shelve, goto --clean to discard all your changes, or goto --merge to merge them)
  [255]

Checking out to diff that modifies a file deleted locally without sl rm
  $ newclientrepo
  $ drawdag <<'EOS'
  > B  # B/file = changed
  > |
  > A  # A/file = foo
  > EOS
  $ sl go $B -qC
  $ rm file
  $ sl go $A
  abort: 1 conflicting file changes:
   file
  (commit, shelve, goto --clean to discard all your changes, or goto --merge to merge them)
  [255]
  $ sl status
  ! file

Respect merge marker file:
  $ newclientrepo
  $ drawdag <<'EOS'
  > B   # B/foo = changed
  > |
  > A   # A/foo = foo
  > EOS

  $ sl go -qC $A
  $ echo diverged > foo
  $ sl go -q --merge $B
  warning: 1 conflicts while merging foo! (edit, then use 'sl resolve --mark')
  [1]

  $ sl go $B
  abort: outstanding merge conflicts
  (use 'sl resolve --list' to list, 'sl resolve --mark FILE' to mark resolved)
  [255]

Run it again to make sure we didn't clear out state file:
  $ sl go $B
  abort: outstanding merge conflicts
  (use 'sl resolve --list' to list, 'sl resolve --mark FILE' to mark resolved)
  [255]

  $ sl go --continue
  abort: outstanding merge conflicts
  (use 'sl resolve --list' to list, 'sl resolve --mark FILE' to mark resolved)
  [255]

  $ sl resolve --mark foo
  (no more unresolved files)
  continue: sl goto --continue

  $ sl go --continue
  $ sl st
  M foo
  ? foo.orig


Can continue interrupted checkout:
  $ newclientrepo
  $ drawdag <<'EOS'
  > A   # A/foo = foo
  >     # A/bar = bar
  > EOS

  $ sl go -q null
  $ FAILPOINTS=checkout-post-progress=return sl go $A
  abort: checkout errors:
   Error set by checkout-post-progress FAILPOINTS
  [255]

  $ sl whereami
  0000000000000000000000000000000000000000

  $ sl go --continue $A --rev $A
  abort: can't specify a destination commit and --continue
  [255]

  $ LOG=checkout=debug sl go -q --continue 2>&1 | grep skipped_count
  DEBUG checkout:apply_store: checkout: skipped files based on progress skipped_count=3 (no-eden !)
  DEBUG checkout:apply_store: checkout: skipped files based on progress skipped_count=0 (eden !)
  $ sl st
  $ tglog
  @  a19fc4bcafed 'A'

  $ sl go --continue
  abort: not in an interrupted goto state
  [255]


Continue checkout if its saved target became obsolete after checkout started:
  $ newclientrepo obsolete_continue
  $ drawdag <<'EOS'
  > B A
  > EOS

  $ sl go -q null
  $ FAILPOINTS=checkout-post-progress=return sl go $A
  abort: checkout errors:
   Error set by checkout-post-progress FAILPOINTS
  [255]
  $ sl debugobsolete $A $B 2>/dev/null
  $ CODING_AGENT_METADATA=id=test_agent sl go --continue
  1 files updated, 0 files merged, 0 files removed, 0 files unresolved
  $ sl log -r . -T '{desc|firstline}\n'
  A


Don't fail with open files that can't be deleted:
  $ newclientrepo unlink_fail
  $ drawdag <<'EOS'
  > B   # B/foo = (removed)
  > |   # B/bar = different
  > |
  > A   # A/foo = foo
  >     # A/bar = bar
  > EOS


  $ sl go -q $A

    with open("unlink_fail/foo"), open("unlink_fail/bar"):

      $ sl go $B
      update failed to remove foo: *: The process cannot access the file because it is being used by another process. (os error 32)! (glob) (windows !) (no-eden !)
      2 files updated, 0 files merged, 1 files removed, 0 files unresolved


Respect other repo states:
  $ newclientrepo
  $ drawdag <<'EOS'
  > B   # B/foo = two
  > 
  > A   # A/foo = one
  > EOS

  $ sl go -q $A
  $ sl graft -r $B
  grafting e57212eac5db "B"
  merging foo
  warning: 1 conflicts while merging foo! (edit, then use 'sl resolve --mark')
  abort: unresolved conflicts, can't continue
  (use 'sl resolve' and 'sl graft --continue')
  [255]
  
  $ sl go $B
  abort: graft in progress
  (use 'sl graft --continue' to continue or
       'sl graft --abort' to abort)
  [255]

Various invalid arg combos:

  $ newclientrepo
  $ sl go foo --rev bar
  abort: goto requires exactly one destination commit but got: ["foo", "bar"]
  [255]

  $ sl go
  abort: you must specify a destination to update to, for example "sl goto main".
  [255]

--clean overwrites conflicts:
  $ newclientrepo
  $ drawdag <<'EOS'
  > A   # A/foo = foo
  >     # A/bar = bar
  > B   # B/foo = baz
  > EOS
  $ sl go -q $B
  $ echo diverged > foo
  $ sl st
  M foo
  $ sl go $A
  abort: 1 conflicting file changes:
   foo
  (commit, shelve, goto --clean to discard all your changes, or goto --merge to merge them)
  [255]

  $ echo untracked > bar
  $ sl rm B
  $ sl st
  M foo
  R B
  ? bar
  $ sl go $A
  abort: * conflicting file changes: (glob)
   B
   bar
   foo
  (commit, shelve, goto --clean to discard all your changes, or goto --merge to merge them)
  [255]

  $ sl go -q --clean $A
  $ sl st
  $ cat foo
  foo (no-eol)
  $ cat bar
  bar (no-eol)
  $ cat B
  cat: B: $ENOENT$
  [1]

--clean gets you out of merge state:
  $ newclientrepo
  $ drawdag <<'EOS'
  > B   # B/foo = two
  > |
  > A   # A/foo = one
  > EOS
  $ sl go -q $A
  $ echo diverged > foo
  $ sl go --merge $B
  merging foo
  warning: 1 conflicts while merging foo! (edit, then use 'sl resolve --mark')
  1 files updated, 0 files merged, 0 files removed, 1 files unresolved
  use 'sl resolve' to retry unresolved file merges
  [1]
  $ sl go -qC $B
  $ sl st
  ? foo.orig
  $ cat foo
  two (no-eol)

--clean doesn't delete added files:
  $ newclientrepo
  $ touch a b c d
  $ sl commit -Aqm foo
  $ touch foo
  $ sl add foo
  $ rm a
  $ sl rm b
  $ echo c > c
  $ sl st
  M c
  A foo
  R b
  ! a
  $ sl go -qC .
  $ sl st
  ? foo

Non --clean keeps unconflicting changes:
  $ newclientrepo
  $ drawdag <<'EOS'
  > B
  > |
  > A
  > EOS
  $ sl go -q $A
  $ echo foo >> A
  $ touch foo
  $ mkdir bar
  $ echo bar > bar/bar
  $ sl st
  M A
  ? bar/bar
  ? foo
  $ sl go -q $B
  $ sl st
  M A
  ? bar/bar
  ? foo

Update active bookmark
  $ newclientrepo
  $ drawdag <<'EOS'
  > B  # bookmark BOOK_B = B
  > |
  > A  # bookmark BOOK_A = A
  > EOS
  $ sl go -q BOOK_A --inactive
  $ cat .sl/bookmarks.current
  cat: .sl/bookmarks.current: $ENOENT$
  [1]
  $ sl go BOOK_B
  1 files updated, 0 files merged, 0 files removed, 0 files unresolved
  (activating bookmark BOOK_B)
  $ cat .sl/bookmarks.current
  BOOK_B (no-eol)
  $ sl go BOOK_A
  0 files updated, 0 files merged, 1 files removed, 0 files unresolved
  (changing active bookmark from BOOK_B to BOOK_A)
  $ cat .sl/bookmarks.current
  BOOK_A (no-eol)
  $ sl go $B
  1 files updated, 0 files merged, 0 files removed, 0 files unresolved
  (leaving bookmark BOOK_A)
  $ cat .sl/bookmarks.current
  cat: .sl/bookmarks.current: $ENOENT$
  [1]

#if no-windows
Support "update" and "goto" hooks:
  $ newclientrepo
  $ sl go -q . --config 'hooks.pre-update=echo update' --config 'hooks.pre-goto=echo goto'
  goto
  update
#endif

#if no-windows
Support "preupdate" and "update" hooks:
  $ newclientrepo
  $ drawdag <<'EOS'
  > A
  > EOS
  $ setconfig 'hooks.preupdate=echo PRE PARENT1: $HG_PARENT1 && echo PRE PARENT2: $HG_PARENT2; exit 1'
  $ setconfig 'hooks.update=echo POST PARENT1: $HG_PARENT1 && echo POST PARENT2: $HG_PARENT2 && echo POST ERROR: $HG_ERROR'
  $ sl go -q $A
  PRE PARENT1: 0000000000000000000000000000000000000000
  PRE PARENT2: 426bada5c67598ca65036d57d9e4b64b0c1ce7a0
  abort: preupdate hook exited with status 1
  [255]
  $ sl whereami
  0000000000000000000000000000000000000000
  $ setconfig 'hooks.preupdate=echo PARENT1: $HG_PARENT1 && echo PARENT2: $HG_PARENT2; exit 0'
  $ sl go -q $A
  PARENT1: 0000000000000000000000000000000000000000
  PARENT2: 426bada5c67598ca65036d57d9e4b64b0c1ce7a0
  POST PARENT1: 426bada5c67598ca65036d57d9e4b64b0c1ce7a0
  POST PARENT2:
  POST ERROR: 0
  $ sl whereami
  426bada5c67598ca65036d57d9e4b64b0c1ce7a0
#endif

Test --check
  $ newclientrepo
  $ drawdag <<'EOS'
  > A
  > EOS
  $ touch B
  $ sl go --check -q $A
  $ sl st
  ? B
  $ rm A
  $ SL_LOG=checkout_info=debug sl go --check -q null
  DEBUG checkout_info: checkout_mode="rust"
  abort: uncommitted changes
  [255]
  $ sl go --clean --check -q null
  abort: can only specify one of -C/--clean, -c/--check, or -m/--merge
  [255]

Bail on dir/path conflict with added file:
  $ newclientrepo
  $ drawdag <<'EOS'
  > B  # B/dir/foo=foo
  > |
  > A
  > EOS
  $ sl go -q $A
  $ touch dir
  $ sl add dir
  $ sl go $B
  abort: 1 conflicting file changes: (no-eden !)
   dir (no-eden !)
  (commit, shelve, goto --clean to discard all your changes, or goto --merge to merge them) (no-eden !)
  abort: dir: local file conflicts with a directory in the destination commit (eden !)
  [255]

Bail on untracked file conflict only if contents differ:
  $ newclientrepo
  $ drawdag <<'EOS'
  > B  # B/foo=foo\n
  > |
  > A
  > EOS
  $ sl go -q $A
  $ echo bar > foo
  $ sl go $B
  abort: 1 conflicting file changes:
   foo
  (commit, shelve, goto --clean to discard all your changes, or goto --merge to merge them)
  [255]
  $ echo foo > foo
  $ sl go -q $B

Bail on untracked file path conflict:
  $ newclientrepo
  $ drawdag <<'EOS'
  > B  # B/foo/bar=foo\n
  > |
  > A
  > EOS
  $ sl go -q $A
  $ echo foo > foo
  $ sl go $B
  abort: 1 conflicting file changes: (no-eden !)
   foo (no-eden !)
  (commit, shelve, goto --clean to discard all your changes, or goto --merge to merge them) (no-eden !)
  abort: foo: local file conflicts with a directory in the destination commit (eden !)
  [255]
  $ rm foo
  $ mkdir -p foo/bar
  $ echo foo > foo/bar/baz
  $ sl go $B
  abort: 1 conflicting file changes: (no-eden !)
   foo/bar/baz (no-eden !)
  (commit, shelve, goto --clean to discard all your changes, or goto --merge to merge them) (no-eden !)
  abort: nonempty directories conflict with files in the destination commit: (eden !)
   foo/bar (eden !)
  (remove the local files or goto --clean to discard them) (eden !)
  [255]
#if no-eden
  $ sl go -q $B --config experimental.checkout.rust-path-conflicts=false
#else
  $ sl go -qC $B
#endif
  $ sl st

Deleted file replaced by untracked directory:
  $ newclientrepo
  $ drawdag <<'EOS'
  > B  # B/foo=bar\n
  > |
  > A  # A/foo=foo\n
  > EOS
  $ sl go -q $A
  $ rm foo
  $ mkdir foo
  $ echo foo > foo/bar
  $ sl st
  ! foo
  ? foo/bar
  $ sl go $B
  abort: 1 conflicting file changes: (no-eden !)
  abort: nonempty directories conflict with files in the destination commit: (eden !)
   foo
  (commit, shelve, goto --clean to discard all your changes, or goto --merge to merge them) (no-eden !)
  (remove the local files or goto --clean to discard them) (eden !)
  [255]
  $ sl rm foo --mark
  $ sl add foo/bar
  $ sl st
  A foo/bar
  R foo
  $ sl go $B
  abort: 1 conflicting file changes: (no-eden !)
  abort: nonempty directories conflict with files in the destination commit: (eden !)
   foo
  (commit, shelve, goto --clean to discard all your changes, or goto --merge to merge them) (no-eden !)
  (remove the local files or goto --clean to discard them) (eden !)
  [255]
  $ sl go -qC $B
  $ sl st

#if no-eden
Don't output too many conflicts. This behavior only occurs on non-EdenFS (no need to fix):
  $ newclientrepo
  $ drawdag <<'EOS'
  > B  # B/foo=bar\n
  > |
  > A
  > EOS
  $ sl go -q $A
  $ mkdir foo
  $ for i in `seq 100`; do
  >   touch foo/file$i
  > done
  $ sl go -q $B
  abort: 100 conflicting file changes:
   foo/file* (glob)
   foo/file* (glob)
   foo/file* (glob)
   foo/file* (glob)
   foo/file* (glob)
   ...and 95 more
  (commit, shelve, goto --clean to discard all your changes, or goto --merge to merge them)
  [255]
#endif

Test update_distance logging:
  $ newclientrepo
  $ drawdag <<'EOS'
  > C
  > |
  > B D
  > |/
  > A
  > EOS
  $ LOG=update_size=trace sl go -q $A
   INFO update_size: update_distance=1
  $ LOG=update_size=trace sl go -q $A
   INFO update_size: update_distance=0
  $ LOG=update_size=trace sl go -q $D
   INFO update_size: update_distance=1
  $ LOG=update_size=trace sl go -q $C
   INFO update_size: update_distance=3
  $ LOG=update_size=trace sl go -q $B
   INFO update_size: update_distance=1
  $ LOG=update_size=trace sl go -q null
   INFO update_size: update_distance=2

#if unix-permissions no-eden
# Test output when there are lots of filesystem errors:

  $ newclientrepo repo-unix-perm
  $ mkdir dir
  $ for i in `seq 10`; do touch dir/file_$i; done
  $ sl commit -Aqm foo
  $ sl go -q null
  $ mkdir dir
  $ chmod 444 dir
  $ sl go tip
  abort: error writing files:
   dir/file_1: can't clear conflicts after handling error "failed to write to file `dir/file_1`: Permission denied (os error 13)": Permission denied (os error 13)
   dir/file_10: can't clear conflicts after handling error "failed to write to file `dir/file_10`: Permission denied (os error 13)": Permission denied (os error 13)
   dir/file_2: can't clear conflicts after handling error "failed to write to file `dir/file_2`: Permission denied (os error 13)": Permission denied (os error 13)
   dir/file_3: can't clear conflicts after handling error "failed to write to file `dir/file_3`: Permission denied (os error 13)": Permission denied (os error 13)
   dir/file_4: can't clear conflicts after handling error "failed to write to file `dir/file_4`: Permission denied (os error 13)": Permission denied (os error 13)
   ...and 5 more
  [255]
#endif

# Test output when there are lots of edenapi errors:
#if no-eden

  $ newclientrepo broken_client broken_server
  $ cd ~/broken_server
  $ for i in `seq 10`; do touch file_$i; done
  $ sl commit -Aqm foo
  $ sl book master
  $ cd ~/broken_client
  $ sl pull -q
  $ FAILPOINTS=eagerepo::api::files_attrs=return sl go master
  abort: error fetching files:
   b80de5d138758541c5f05265ad144ab9fa86d1db file_1: Network Error: server responded 500 Internal Server Error for eager://$TESTTMP/broken_server/files_attrs: failpoint. Headers: {}
   b80de5d138758541c5f05265ad144ab9fa86d1db file_10: Network Error: server responded 500 Internal Server Error for eager://$TESTTMP/broken_server/files_attrs: failpoint. Headers: {}
   b80de5d138758541c5f05265ad144ab9fa86d1db file_2: Network Error: server responded 500 Internal Server Error for eager://$TESTTMP/broken_server/files_attrs: failpoint. Headers: {}
   b80de5d138758541c5f05265ad144ab9fa86d1db file_3: Network Error: server responded 500 Internal Server Error for eager://$TESTTMP/broken_server/files_attrs: failpoint. Headers: {}
   b80de5d138758541c5f05265ad144ab9fa86d1db file_4: Network Error: server responded 500 Internal Server Error for eager://$TESTTMP/broken_server/files_attrs: failpoint. Headers: {}
   ...and 5 more
  [255]

#endif
