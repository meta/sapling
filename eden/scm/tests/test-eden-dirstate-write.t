
#require eden

  $ newclientrepo
  $ touch a b c
  $ sl commit -Aqm init
  $ sl cp a copied

Writing the dirstate filters out clean entries and serializes the copy map.
FIXME: it computes the copy map once per clean entry, plus once to serialize it.

  $ cat > $TESTTMP/write.py <<'PYEOF'
  > from sapling import treestate
  > computed = []
  > copymap = treestate.treestatemap.copymap
  > treestate.treestatemap.copymap = property(
  >     lambda self: computed.append(1) or copymap.fget(self)
  > )
  > with repo.wlock(), repo.lock(), repo.transaction("test") as tr:
  >     for f in ["a", "b", "c"]:
  >         repo.dirstate.normal(f)
  >     repo.dirstate.write(tr)
  > print("copy map computed %d times" % len(computed))
  > PYEOF
  $ sl debugshell $TESTTMP/write.py
  copy map computed 4 times

  $ sl status -C
  A copied
    a
