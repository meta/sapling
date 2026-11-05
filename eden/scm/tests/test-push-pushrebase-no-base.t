#require no-eden

  $ setconfig push.edenapi=true

Server with an existing bookmark:

  $ newserver server
  $ echo foo > a
  $ sl commit -Aqm initial
  $ sl book master
  $ cd ..

Client with a draft root commit that shares no history with the server:

  $ newclientrepo client server
  $ drawdag <<'EOS'
  > A
  > EOS
  $ sl log -G -r 'all()' -T '{desc} {phase} {remotebookmarks}'
  o  A draft
  
  @  initial public remote/master
  

Pushrebasing a stack whose root has no parent, so that it shares no history with
the bookmark, aborts cleanly:

  $ sl push -r $A --to master
  pushing rev 426bada5c675 to destination eager:$TESTTMP/server bookmark master
  edenapi: queue 1 commit for upload
  edenapi: queue 1 file for upload
  edenapi: uploaded 1 file
  edenapi: queue 1 tree for upload
  edenapi: uploaded 1 tree
  edenapi: uploaded 1 changeset
  abort: cannot pushrebase 426bada5c675 onto remote bookmark master: they have no common ancestor
  (rebase the stack onto master first)
  [255]

`push_rebase` aborts the same way when called directly with the rootless stack:

  $ sl debugshell -c '
  > from sapling import push, scmutil
  > head = scmutil.revsingle(repo, "desc(A)").node()
  > bookmark_node = scmutil.revsingle(repo, "remote/master").node()
  > stack = repo.dageval(lambda: only([head], public()))
  > try:
  >     push.push_rebase(repo, "default", head, stack, "master", bookmark_node, {})
  > except Exception as e:
  >     print("%s: %s" % (type(e).__name__, e))
  > '
  Abort: cannot pushrebase 426bada5c675 onto remote bookmark master: they have no common ancestor
