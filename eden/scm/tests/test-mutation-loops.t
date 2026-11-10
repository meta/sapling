
#require no-eden

  $ eagerepo
  $ enable amend rebase
  $ setconfig experimental.evolution=obsolete
  $ setconfig visibility.enabled=true
  $ setconfig mutation.record=true mutation.enabled=true

  $ newrepo
  $ echo "base" > base
  $ sl commit -Aqm base
  $ echo 1 > file
  $ sl commit -Aqm commit1
  $ for i in 2 3 4 5
  > do
  >   echo $i >> file
  >   sl amend -m "commit$i"
  > done
  $ sl debugmutation
   *  21c93100b04c543843a7dab4fa0d5bada061b7a0 amend by test at 1970-01-01T00:00:00 from:
      672a4910c364d425231d2dd2fb0486f32a2d88f4 amend by test at 1970-01-01T00:00:00 from:
      d3c8fd338cf40a496d981b2ada8df4108f575897 amend by test at 1970-01-01T00:00:00 from:
      932f02c9fad3fa46e55b62560c88eb67528b02f0 amend by test at 1970-01-01T00:00:00 from:
      e6c779c67aa947c951f334f4f312bd2b21d27e55


Loops are not normally possible, but they can sneak in through backfilling complex
obsmarker graphs.  Create a fake one to check behaviour.

  $ sl debugsh -c "with repo.lock(): s.mutation.recordentries(repo, [s.mutation.createsyntheticentry(repo, [s.node.bin(\"e6c779c67aa947c951f334f4f312bd2b21d27e55\"), s.node.bin(\"672a4910c364d425231d2dd2fb0486f32a2d88f4\")], s.node.bin(\"932f02c9fad3fa46e55b62560c88eb67528b02f0\"), \"loop\")], skipexisting=False)"
  $ tglogm --hidden
  @  21c93100b04c 'commit5'
  │
  │ v  672a4910c364 'commit4'  (Rewritten using amend into 21c93100b04c) (Rewritten using loop into 932f02c9fad3)
  ├─╯
  │ x  d3c8fd338cf4 'commit3'  (Rewritten using amend into 672a4910c364)
  ├─╯
  │ x  932f02c9fad3 'commit2'  (Rewritten using amend into d3c8fd338cf4)
  ├─╯
  │ x  e6c779c67aa9 'commit1'  (Rewritten using loop into 932f02c9fad3)
  ├─╯
  o  d20a80d4def3 'base'


  $ sl unhide e6c779c67aa9

Check the normal revsets.
  $ sl log -r 'predecessors(21c93100b04c)' -T '{node} {desc}\n'
  e6c779c67aa947c951f334f4f312bd2b21d27e55 commit1
  932f02c9fad3fa46e55b62560c88eb67528b02f0 commit2
  d3c8fd338cf40a496d981b2ada8df4108f575897 commit3
  672a4910c364d425231d2dd2fb0486f32a2d88f4 commit4
  21c93100b04c543843a7dab4fa0d5bada061b7a0 commit5
  $ sl log -r 'successors(e6c779c67aa9)' -T '{node} {desc}\n'
  e6c779c67aa947c951f334f4f312bd2b21d27e55 commit1
  21c93100b04c543843a7dab4fa0d5bada061b7a0 commit5

If successorssets doesn't handle loops, this next command will hang as it
continuously cycles round the commit2 to commit4 loop.

  $ tglogm
  @  21c93100b04c 'commit5'
  │
  │ x  e6c779c67aa9 'commit1'  (Rewritten using rewrite into 21c93100b04c)
  ├─╯
  o  d20a80d4def3 'base'

Similarly, check that predecessorsset is also safe.

  $ sl debugsh -c "ui.write(str([s.node.hex(n) for n in s.mutation.predecessorsset(repo, s.node.bin(\"21c93100b04c543843a7dab4fa0d5bada061b7a0\"))]) + '\n')"
  ['e6c779c67aa947c951f334f4f312bd2b21d27e55']

A loop that nothing leads out of is not reachable from the heads of its
history. A commit rewritten both into a visible commit and into such a loop is
still drawn, as diverged.

  $ newrepo noexit
  $ echo base > base
  $ sl commit -Aqm base
  $ for name in A B C; do
  >   sl goto -q 'desc(base)'
  >   echo $name > $name
  >   sl commit -Aqm $name
  > done
  $ sl goto -q 'desc(base)'
  $ echo X > X
  $ sl commit -Aqm X
  $ echo Z > Z
  $ sl commit -Aqm Z
  $ sl prev -q
  [*] X (glob)
  $ echo Y > X
  $ sl amend -q -m Y --config amend.autorestack=none
  invalid amend.autorestack config of "none"; falling back to only-trivial
  hint[amend-restack]: descendants of 015050e97578 are left behind - use 'sl restack' to rebase them
  hint[hint-ack]: use 'sl hint --ack amend-restack' to silence these hints
  $ cat > $TESTTMP/loop.py << 'PYEOF'
  > nodes = {repo[r].description(): repo[r].node() for r in repo.revs("all()")}
  > entries = [
  >     s.mutation.createsyntheticentry(repo, [nodes[pred]], nodes[succ], "loop")
  >     for pred, succ in [("X", "A"), ("A", "B"), ("B", "C"), ("C", "A")]
  > ]
  > with repo.lock():
  >     s.mutation.recordentries(repo, entries, skipexisting=False)
  > PYEOF
  $ sl debugsh $TESTTMP/loop.py
  $ tglogm
  @  be2e557708d7 'Y'
  │
  │ o  3bae41dfad74 'Z'
  │ │
  │ v  015050e97578 'X'  (Rewritten using rewrite into 9d73aac1b2ed) (Rewritten using amend into be2e557708d7)
  ├─╯
  │ x  541f8cbccc44 'C'  (Rewritten using loop into 9d73aac1b2ed)
  ├─╯
  │ x  58c9665b7b97 'B'  (Rewritten using loop into 541f8cbccc44)
  ├─╯
  │ x  9d73aac1b2ed 'A'  (Rewritten using loop into 58c9665b7b97)
  ├─╯
  o  d20a80d4def3 'base'
