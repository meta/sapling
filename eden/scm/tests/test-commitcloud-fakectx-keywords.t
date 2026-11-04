
#require no-eden

Template keywords on the fake contexts of `cloud log` / `cloud sl`: {p1node},
{p2node}, {p1rev}, {p2rev}, {parents}, {rev} and -T json crash (FakeCtx has no
p1(), parents() returns raw nodes and rev() returns the node bytes).

  $ enable commitcloud fbcodereview
  $ setconfig commitcloud.hostname=testhost
  $ newclientrepo client
  $ setconfig commitcloud.servicetype=local commitcloud.servicelocation="$TESTTMP"

Public base 1111 (its parent is outside the smartlog), drafts 2222 <- 1111,
3333 <- 2222 and a merge 4444 <- (3333, 2222).

  $ cat > $TESTTMP/usersmartlogdata << EOF
  > {
  >   "smartlog": {
  >     "nodes": [
  >       { "node": "1111111111111111111111111111111111111111", "phase": "public", "author": "Test User", "date": 1527625388, "message": "public base", "parents": ["0000000000000000000000000000000000000001"], "bookmarks": [] },
  >       { "node": "2222222222222222222222222222222222222222", "phase": "draft", "author": "Test User", "date": 1527626185, "message": "draft A\n\nDifferential Revision: https://phabricator.intern.facebook.com/D1234", "parents": ["1111111111111111111111111111111111111111"], "bookmarks": [] },
  >       { "node": "3333333333333333333333333333333333333333", "phase": "draft", "author": "Test User", "date": 1527626186, "message": "draft B", "parents": ["2222222222222222222222222222222222222222"], "bookmarks": ["book"] },
  >       { "node": "4444444444444444444444444444444444444444", "phase": "draft", "author": "Test User", "date": 1527626187, "message": "merge", "parents": ["3333333333333333333333333333333333333333", "2222222222222222222222222222222222222222"], "bookmarks": [] }]
  >   }
  > }
  > EOF

{p1node}/{p2node}: AttributeError, FakeCtx has no p1().

  $ sl cloud log -T '{node} {p1node} {p2node}\n' 2>&1 | grep -E '^(AttributeError|TypeError)'
  AttributeError: 'FakeCtx' object has no attribute 'p1'

{p1rev}/{p2rev}: same.

  $ sl cloud log -T '{node} {p1rev} {p2rev}\n' 2>&1 | grep -E '^(AttributeError|TypeError)'
  AttributeError: 'FakeCtx' object has no attribute 'p1'

{parents} on `cloud sl`: the parents are raw nodes, so p.rev() fails.

  $ sl cloud sl -T '{node} parents={parents}\n' 2>&1 | grep -E '^(AttributeError|TypeError)'
  AttributeError: 'bytes' object has no attribute 'rev'

{rev} on `cloud sl`: rev() is the node (bytes), revf64encode compares it to an int.

  $ sl cloud sl -T '{rev} {node}\n' 2>&1 | grep -E '^(AttributeError|TypeError)'
  TypeError: '>=' not supported between instances of 'bytes' and 'int'

-T json: same comparison in cmdutil.jsonchangeset.

  $ sl cloud log -T json 2>&1 | grep -E '^(AttributeError|TypeError)'
  TypeError: '>=' not supported between instances of 'bytes' and 'int'

{phabdiff} reads the (str) description.

  $ sl cloud log -T '{node} {phabdiff}\n'
  the repository is not connected to any workspace, assuming the 'default' workspace
  4444444444444444444444444444444444444444 
  3333333333333333333333333333333333333333 
  2222222222222222222222222222222222222222 D1234

Unit level: the fake contexts that `makedagwalker` (cloud sl) and
`makedagwalkerwithparents` (cloud log) build, read the way the `rev`,
`parents`, `p1node` and `p2node` keywords read them:

  $ sl debugshell -c '
  > from sapling import scmutil
  > from sapling.ext.commitcloud import service, util as ccutil, workspace
  > from sapling.node import hex
  > def show(f):
  >     try:
  >         return f()
  >     except Exception as e:
  >         return "%s: %s" % (type(e).__name__, e)
  > serv = service.get(ui, repo)
  > slinfo = serv.getsmartlog(ccutil.getreponame(repo), workspace.parseworkspaceordefault(ui, repo, {}), repo, 0)
  > for walker in (serv.makedagwalker, serv.makedagwalkerwithparents):
  >     print(walker.__name__)
  >     for node, _type, ctx, _parents in walker(slinfo, repo)[1]:
  >         print("  %s rev=%r" % (hex(node)[:4], show(lambda: scmutil.revf64encode(scmutil.intrev(ctx)))))
  >         print("    parent revs: %r" % (show(lambda: [p.rev() for p in ctx.parents()]),))
  >         print("    p1node/p2node: %r" % (show(lambda: (ctx.p1().hex()[:4], ctx.p2().hex()[:4])),))
  > '
  the repository is not connected to any workspace, assuming the 'default' workspace
  makedagwalker
    4444 rev="TypeError: '>=' not supported between instances of 'bytes' and 'int'"
      parent revs: "AttributeError: 'bytes' object has no attribute 'rev'"
      p1node/p2node: "AttributeError: 'FakeCtx' object has no attribute 'p1'"
    3333 rev="TypeError: '>=' not supported between instances of 'bytes' and 'int'"
      parent revs: "AttributeError: 'bytes' object has no attribute 'rev'"
      p1node/p2node: "AttributeError: 'FakeCtx' object has no attribute 'p1'"
    2222 rev="TypeError: '>=' not supported between instances of 'bytes' and 'int'"
      parent revs: "AttributeError: 'bytes' object has no attribute 'rev'"
      p1node/p2node: "AttributeError: 'FakeCtx' object has no attribute 'p1'"
    1111 rev="TypeError: '>=' not supported between instances of 'bytes' and 'int'"
      parent revs: "AttributeError: 'bytes' object has no attribute 'rev'"
      p1node/p2node: "AttributeError: 'FakeCtx' object has no attribute 'p1'"
  makedagwalkerwithparents
    4444 rev="TypeError: '>=' not supported between instances of 'bytes' and 'int'"
      parent revs: [None, None]
      p1node/p2node: "AttributeError: 'FakeCtx' object has no attribute 'p1'"
    3333 rev="TypeError: '>=' not supported between instances of 'bytes' and 'int'"
      parent revs: [None]
      p1node/p2node: "AttributeError: 'FakeCtx' object has no attribute 'p1'"
    2222 rev="TypeError: '>=' not supported between instances of 'bytes' and 'int'"
      parent revs: [None]
      p1node/p2node: "AttributeError: 'FakeCtx' object has no attribute 'p1'"
    1111 rev="TypeError: '>=' not supported between instances of 'bytes' and 'int'"
      parent revs: []
      p1node/p2node: "AttributeError: 'FakeCtx' object has no attribute 'p1'"
