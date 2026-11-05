#require no-eden

  $ configure mutation

  $ function log() {
  >   sl log -G -r 'all()' -T '{node|short} {desc} {remotebookmarks} {bookmarks}'
  > }

#testcases slapi 

#if wireproto
  $ setconfig push.edenapi=false
#else
  $ setconfig push.edenapi=true
#endif

Set up server repository

#if wireproto
  $ rm $TESTTMP/.eagerepo
#endif
  $ newserver server
  $ cat >> .sl/config << EOF
  > [extensions]
  > pushrebase=
  > EOF
  $ echo foo > a
  $ echo foo > b
  $ sl commit -Am 'initial'
  adding a
  adding b
  $ sl book master
  $ cd ..

Test fast forward push

  $ newclientrepo client server
  $ sl up -q master
  $ echo x >> a && sl commit -qm 'add a'
  $ sl commit --amend -qm 'changed message'
  $ sl log -r . -T '{node}\n'
  ea98a8f9539083f60b81315106c94227e8814d17
  $ sl push --to master -q
  $ sl show
  commit:      ea98a8f95390
  bookmark:    remote/master
  hoistedname: master
  user:        test
  date:        Thu Jan 01 00:00:00 1970 +0000
  files:       a
  description:
  changed message
  
  
  diff -r 2bb9d20e471c -r ea98a8f95390 a
  --- a/a	Thu Jan 01 00:00:00 1970 +0000
  +++ b/a	Thu Jan 01 00:00:00 1970 +0000
  @@ -1,1 +1,2 @@
   foo
  +x
  $ log
  @  ea98a8f95390 changed message remote/master
  │
  o  2bb9d20e471c initial

the master bookmark should point to the latest commit
  $ newclientrepo client2 server
  $ log
  @  ea98a8f95390 changed message remote/master
  │
  o  2bb9d20e471c initial

test pushrebase conflicts
  $ sl go -q 2bb9d20e471c
  $ echo y >> a && sl commit -qm "update a"
#if slapi
  $ sl push --to master -q
  abort: Server error: Conflicts while pushrebasing: [(RepoPathBuf("a"), RepoPathBuf("a"))]
  [255]
#endif

Test pushrebase a diff stack
  $ newclientrepo client3 server
  $ sl go -q 2bb9d20e471c
  $ echo 1 >> c && sl ci -qAm "add c"
  $ echo 2 >> c && sl ci -qm "update c"
  $ log
  @  adb87132efa9 update c
  │
  o  f46b94d12452 add c
  │
  │ o  ea98a8f95390 changed message remote/master
  ├─╯
  o  2bb9d20e471c initial
  $ sl push --to master -q
  $ log
  @  9237cb52bef6 update c remote/master
  │
  o  d7f85d6d9fc3 add c
  │
  o  ea98a8f95390 changed message
  │
  o  2bb9d20e471c initial

Test pushrebase of merges, which `experimental.edenapi-push-merges` sends over
SLAPI. D is a draft branch from the initial commit, and E merges it into
master. Master then moves on to F.
  $ newclientrepo client4 server
  $ setconfig experimental.edenapi-push-merges=true
  $ sl go -q 2bb9d20e471c
  $ echo d > d && sl ci -qAm "add d"
  $ sl go -q master
  $ sl merge -q 'desc("add d")' && sl ci -qm "merge d"
  $ sl go -q master
  $ echo f > f && sl ci -qAm "add f"
  $ sl push --to master -q
  $ sl go -q 'desc("merge d")'
  $ sl push --to master
  pushing rev a92b1c336a70 to destination eager:$TESTTMP/server bookmark master
  edenapi: queue 2 commits for upload
  edenapi: queue 1 file for upload
  edenapi: uploaded 1 file
  edenapi: queue 2 trees for upload
  edenapi: uploaded 2 trees
  edenapi: uploaded 2 changesets
  pushrebasing stack (9237cb52bef6, a92b1c336a70] (1 commit) to remote bookmark master
  1 files updated, 0 files merged, 0 files removed, 0 files unresolved
  updated remote bookmark master to 981e6d97f297

Only the merge descends from the merge base of master and the merge, so only
the merge is rebased and replaced; D lands as the merge's second parent:
  $ log
  @    981e6d97f297 merge d remote/master
  ├─╮
  │ o  31ff59e35789 add d
  │ │
  o │  be42e26452aa add f
  │ │
  o │  9237cb52bef6 update c
  │ │
  o │  d7f85d6d9fc3 add c
  │ │
  o │  ea98a8f95390 changed message
  ├─╯
  o  2bb9d20e471c initial
  $ sl log -r 'p1(.) + p2(.)' -T '{node|short} {phase}\n'
  be42e26452aa public
  31ff59e35789 public
  $ sl log -r 'predecessors(.)' -T '{node|short} {desc}\n' --hidden
  981e6d97f297 merge d
  a92b1c336a70 merge d
  $ sl files -r .
  a
  b
  c
  d
  f

With the config off, the push takes the wireproto path:
  $ sl go -q 2bb9d20e471c
  $ echo g > g && sl ci -qAm "add g"
  $ sl go -q master
  $ sl merge -q 'desc("add g")' && sl ci -qm "merge g"
  $ sl push --to master --config experimental.edenapi-push-merges=false
  fallback reason: merge commit is not supported by EdenApi push yet
  pushing rev d2e8ce43bc11 to destination test:server bookmark master
  searching for changes
  updating bookmark master

A merge whose merge base with master is its second parent (master, which has
not moved since the merge) is rejected when the push does not ask for the swap:
  $ sl go -q 2bb9d20e471c
  $ echo h > h && sl ci -qAm "add h"
  $ sl merge -q master && sl ci -qm "merge master into h"
  $ sl push --to master --pushvars MAINLINE_PARENT_FIRST=false
  pushing rev e4679e1f76e7 to destination eager:$TESTTMP/server bookmark master
  edenapi: queue 2 commits for upload
  edenapi: queue 1 file for upload
  edenapi: uploaded 1 file
  edenapi: queue 2 trees for upload
  edenapi: uploaded 2 trees
  edenapi: uploaded 2 changesets
  pushrebasing stack (d2e8ce43bc11, e4679e1f76e7] (1 commit) to remote bookmark master
  abort: Server error: Forbid pushrebase because root (d2e8ce43bc1195a2122adb1527d30617d61c135d) is not a p1 of master bookmark
  [255]

The client asks for the swap for a push with merges. The merge lands with
master as its first parent and the `parents_swapped` extra, and replaces the
pushed merge:
  $ sl push --to master
  pushing rev e4679e1f76e7 to destination eager:$TESTTMP/server bookmark master
  pushrebasing stack (d2e8ce43bc11, e4679e1f76e7] (1 commit) to remote bookmark master
  0 files updated, 0 files merged, 0 files removed, 0 files unresolved
  updated remote bookmark master to ebde518be45d
  $ sl log -r 'p1(.) + p2(.)' -T '{desc}\n'
  merge g
  add h
  $ sl log -r . -T '{extras % "{extra}\n"}' | grep parents_swapped
  parents_swapped=1
  $ sl log -r 'predecessors(.)' -T '{node|short} {desc}\n' --hidden
  ebde518be45d merge master into h
  e4679e1f76e7 merge master into h

A stack on a public branch other than the destination would rebase that
branch's public commits, so the client refuses to push it:
  $ sl go -q 2bb9d20e471c
  $ echo r > r && sl ci -qAm "add r"
  $ sl push -q --to release --create
  $ echo s > s && sl ci -qAm "add s"
  $ sl push --to master
  pushing rev dc21af901f7e to destination eager:$TESTTMP/server bookmark master
  edenapi: queue 1 commit for upload
  edenapi: queue 1 file for upload
  edenapi: uploaded 1 file
  edenapi: queue 1 tree for upload
  edenapi: uploaded 1 tree
  edenapi: uploaded 1 changeset
  abort: cannot rebase public changesets: 1a720c149345
  [255]

The server rejects a push whose base is not the merge base of master and the
head, as when master has moved back since the client last saw it:
  $ sl go -q master
  $ echo t > t && sl ci -qAm "add t"
  $ newclientrepo client5 server
  $ sl push -q -r 'master^' --to master --force
  $ cd ../client4
  $ sl push --to master --config push.use_local_bookmark_value=true
  pushing rev 0be08e3c1a88 to destination eager:$TESTTMP/server bookmark master
  edenapi: queue 1 commit for upload
  edenapi: queue 1 file for upload
  edenapi: uploaded 1 file
  edenapi: queue 1 tree for upload
  edenapi: uploaded 1 tree
  edenapi: uploaded 1 changeset
  pushrebasing stack (ebde518be45d, 0be08e3c1a88] (1 commit) to remote bookmark master
  abort: Server error: bookmark 'master' has moved incompatibly with this push: the merge base of 'master' and 0be08e3c1a882833f7a230cfd5b881fc801cf134 is d2e8ce43bc1195a2122adb1527d30617d61c135d, but the push is based on ebde518be45de607b97fbd35a189f67184b04f3e. Pull and retry
  [255]
