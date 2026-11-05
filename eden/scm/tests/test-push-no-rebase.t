#require no-eden

  $ enable pushrebase
  $ setconfig push.edenapi=true

  $ newclientrepo
  $ drawdag <<'EOS'
  > M
  > |\
  > C D
  > |/
  > B
  > |
  > A
  > EOS

  $ log() {
  >   sl log -G -r 'all()' -T '{node|short} {desc} {phase} {remotebookmarks}'
  > }

  $ sl push -q -r $A --to master --create

A draft stack lands unchanged as a fast-forward of the remote bookmark:
  $ sl push -r $C --to master --no-rebase
  pushing rev 26805aba1e60 to destination eager:$TESTTMP/repo1_server bookmark master
  edenapi: queue 2 commits for upload
  edenapi: queue 2 files for upload
  edenapi: uploaded 2 files
  edenapi: queue 2 trees for upload
  edenapi: uploaded 2 trees
  edenapi: uploaded 2 changesets
  moving remote bookmark master from 426bada5c675 to 26805aba1e60
  $ log
  o    e28fb9c96eb2 M draft
  ├─╮
  │ o  be0ef73c17ad D draft
  │ │
  o │  26805aba1e60 C public remote/master
  ├─╯
  o  112478962961 B public
  │
  o  426bada5c675 A public

A commit that does not descend from the remote bookmark is not rebased; the
push is rejected as a non-fast-forward move:
  $ sl push -r $D --to master --no-rebase
  pushing rev be0ef73c17ad to destination eager:$TESTTMP/repo1_server bookmark master
  edenapi: queue 1 commit for upload
  edenapi: queue 1 file for upload
  edenapi: uploaded 1 file
  edenapi: queue 1 tree for upload
  edenapi: uploaded 1 tree
  edenapi: uploaded 1 changeset
  abort: non-fast-forward push to remote bookmark master from 26805aba1e60 to be0ef73c17ad
  (add '--force' or set pushvar NON_FAST_FORWARD=true for a non-fast-forward move)
  [255]

The NON_FAST_FORWARD pushvar allows the move:
  $ sl push -r $D --to master --no-rebase --pushvars NON_FAST_FORWARD=true
  pushing rev be0ef73c17ad to destination eager:$TESTTMP/repo1_server bookmark master
  moving remote bookmark master from 26805aba1e60 to be0ef73c17ad

So does --force:
  $ sl push -r $C --to master --no-rebase --force
  pushing rev 26805aba1e60 to destination eager:$TESTTMP/repo1_server bookmark master
  moving remote bookmark master from be0ef73c17ad to 26805aba1e60
  $ log
  o    e28fb9c96eb2 M draft
  ├─╮
  │ o  be0ef73c17ad D draft
  │ │
  o │  26805aba1e60 C public remote/master
  ├─╯
  o  112478962961 B public
  │
  o  426bada5c675 A public

--create creates the bookmark at exactly the pushed commit:
  $ sl push -r $D --to other --create --no-rebase
  pushing rev be0ef73c17ad to destination eager:$TESTTMP/repo1_server bookmark other
  creating remote bookmark other

Merge commits are pushed only with `experimental.edenapi-push-merges` on:
  $ sl push -r $M --to master --no-rebase
  abort: merge commit is not supported by EdenApi push yet
  [255]
  $ sl push -r $M --to master --no-rebase --config experimental.edenapi-push-merges=true
  pushing rev e28fb9c96eb2 to destination eager:$TESTTMP/repo1_server bookmark master
  edenapi: queue 1 commit for upload
  edenapi: queue 0 files for upload
  edenapi: queue 1 tree for upload
  edenapi: uploaded 1 tree
  edenapi: uploaded 1 changeset
  moving remote bookmark master from 26805aba1e60 to e28fb9c96eb2
  $ log
  o    e28fb9c96eb2 M public remote/master
  ├─╮
  │ o  be0ef73c17ad D public remote/other
  │ │
  o │  26805aba1e60 C public
  ├─╯
  o  112478962961 B public
  │
  o  426bada5c675 A public

--no-rebase, like --rebase, needs a destination bookmark:
  $ sl push -r $C --no-rebase
  abort: --no-rebase requires --to
  [255]
  $ sl push -r $C --rebase
  abort: --rebase requires --to
  [255]
  $ sl push --delete other --no-rebase
  abort: do not specify --delete and --no-rebase at the same time
  [255]
  $ sl push --delete other --rebase
  abort: do not specify --delete and --rebase at the same time
  [255]
