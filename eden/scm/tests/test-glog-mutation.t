#require no-eden

Test --mutation, -t of `log`.

  $ setconfig diff.git=true

Mutation log is empty without a working copy parent:

  $ newrepo
  $ sl log --mutation

  $ newrepo
  $ drawdag --no-files << 'EOS'
  >   B3  # B3/f=1\n2\n3\n4\n
  >  /    # amend: B1 -> B2 -> B3
  > | B2  # B2/f=1\n2\n3\n
  > |/
  > | B1  # B1/f=1\n2\n
  > |/
  > A     # A/f=1\n
  > EOS

Regular commit graph log:

  $ sl log -Gr: -T '{desc}\n'
  o  B3
  │
  │ x  B2
  ├─╯
  │ x  B1
  ├─╯
  o  A

Mutation graph log:

  $ sl log -Gtr: -T '{desc}\n'
  o  B3
  │
  x  B2
  │
  x  B1
  
  o  A

Without an explicit revision, mutation log defaults to the mutation history of
the current commit:

  $ sl goto -q $B3
  $ sl log -t -T '{desc}\n'
  B3
  B2
  B1
  $ sl log -Gt -T '{desc}\n'
  @  B3
  │
  x  B2
  │
  x  B1
  $ sl goto -q null

`-p` and `--stat` are affected by `--mutation` too, for both graph and non-graph log:

  $ sl log -Gr 'predecessors(desc(B3))' -T '{desc}\n' -p --stat --mutation
  o  B3
  │   f |  1 +
  │   1 files changed, 1 insertions(+), 0 deletions(-)
  │
  │  diff --git a/f b/f
  │  --- a/f
  │  +++ b/f
  │  @@ -1,3 +1,4 @@
  │   1
  │   2
  │   3
  │  +4
  │
  x  B2
  │   f |  1 +
  │   1 files changed, 1 insertions(+), 0 deletions(-)
  │
  │  diff --git a/f b/f
  │  --- a/f
  │  +++ b/f
  │  @@ -1,2 +1,3 @@
  │   1
  │   2
  │  +3
  │
  x  B1
      f |  1 +
      1 files changed, 1 insertions(+), 0 deletions(-)
  
     diff --git a/f b/f
     --- a/f
     +++ b/f
     @@ -1,1 +1,2 @@
      1
     +2

  $ sl log -r 'desc(B3)' -T '{desc}\n' --stat -p --mutation
  B3
   f |  1 +
   1 files changed, 1 insertions(+), 0 deletions(-)
  
  diff --git a/f b/f
  --- a/f
  +++ b/f
  @@ -1,3 +1,4 @@
   1
   2
   3
  +4

A predecessor that is no longer available is skipped, with a dashed line from
the commit to the nearest shown commit it was created from:

  $ sl debugstrip 'desc(B2)'
  $ sl log -Gr 'predecessors(desc(B3))' -T '{desc}\n' -p --stat --mutation --traceback
  o  B3
  ╷   f |  3 +++
  ╷   1 files changed, 3 insertions(+), 0 deletions(-)
  ╷
  ╷  diff --git a/f b/f
  ╷  --- a/f
  ╷  +++ b/f
  ╷  @@ -1,1 +1,4 @@
  ╷   1
  ╷  +2
  ╷  +3
  ╷  +4
  ╷
  x  B1
      f |  1 +
      1 files changed, 1 insertions(+), 0 deletions(-)
  
     diff --git a/f b/f
     --- a/f
     +++ b/f
     @@ -1,1 +1,2 @@
      1
     +2

Splits, folds, and an older version with a higher rev:

  $ newclientrepo
  $ echo a > a
  $ sl commit -qAm A
  $ echo b > b
  $ echo c > c
  $ sl commit -qAm B
  $ cat > $TESTTMP/partone.py << 'EOF'
  > import sys
  > from pathlib import Path
  > done = Path(__file__).with_name("partone.done")
  > if not done.exists():
  >     Path(sys.argv[1]).write_text("B part 1\n")
  >     done.touch()
  > EOF
  $ HGEDITOR="\"$PYTHON\" \"$TESTTMP/partone.py\"" sl split -q --config ui.interactive=true << EOF
  > y
  > y
  > n
  > y
  > EOF
  diff --git a/b b/b
  new file mode 100644
  examine changes to 'b'? [Ynesfdaq?] y
  
  @@ -0,0 +1,1 @@
  +b
  record change 1/2 to 'b'? [Ynesfdaq?] y
  
  diff --git a/c b/c
  new file mode 100644
  examine changes to 'c'? [Ynesfdaq?] n
  
  Done splitting? [yN] y
  $ echo d > d
  $ sl commit -qAm D
  $ sl fold -q --from '.^' -m 'B part 2 and D'

Every part of a split is shown as created from the split commit, and a fold
has every folded commit as a parent:

  $ sl log -G --mutation -T '{desc}\n' -r 'predecessors(.) + desc("B part 1")'
  @    B part 2 and D
  ├─╮
  │ x  D
  │
  x  B
  │
  │ o  B part 1
  ├─╯
  x  B

Commits are ordered by mutation, not by rev. Here the older version has the
higher rev, as when it is pulled after its successor:

  $ sl goto -q 'desc(A)'
  $ echo e2 > e
  $ sl commit -qAm E2
  $ NEW=$(sl log -r . -T '{node}')
  $ sl goto -q 'desc(A)'
  $ echo e > e
  $ sl commit -qAm E
  $ OLD=$(sl log -r . -T '{node}')
  $ sl debugshell -c "
  > from sapling import mutation
  > from sapling.node import bin
  > e = mutation.createsyntheticentry(repo, [bin('$OLD')], bin('$NEW'), 'amend')
  > with repo.lock():
  >     mutation.recordentries(repo, [e], skipexisting=False)
  > "
  $ sl log -r 'desc(E)' -T '{rev} {desc}\n'
  281474976710662 E2
  281474976710663 E
  $ sl log -G --mutation -T '{desc}\n' -r 'desc(E)'
  o  E2
  │
  @  E
  $ sl log --mutation -T '{desc}\n' -r 'desc(E)'
  E2
  E

The default revisions are mutationhistory(.), the mutation history of the
current commit. It includes the visible commits created from the same commits,
here a divergent amend of an older version. -r selects exactly the given
revisions; mutationhistory() selects another commit's history. Hidden commits
are shown with --hidden:

  $ newclientrepo
  $ setconfig experimental.evolution.allowdivergence=true
  $ echo a > a
  $ sl commit -qAm A
  $ echo b > b
  $ sl commit -qAm B
  $ echo b2 > b
  $ sl amend -q -m B2
  $ sl goto -q --hidden 'predecessors(.) - .'
  $ echo b3 > b
  $ sl amend -q -m B3
  $ sl log -G --mutation -T '{desc}\n'
  @  B3
  │
  │ o  B2
  ├─╯
  v  B
  $ sl log -G --mutation -T '{desc}\n' -r 'desc(B2)'
  o  B2
  │
  ~
  $ sl log -G --mutation -T '{desc}\n' -r 'mutationhistory(desc(B2))'
  @  B3
  │
  │ o  B2
  ├─╯
  v  B
  $ sl log -T '{desc}\n' -r 'mutationhistory(desc(B2))'
  B
  B2
  B3
  $ echo b4 > b
  $ sl amend -q -m B4
  $ sl hide -q 'desc(B2)'
  $ sl log -G --mutation -T '{desc}\n'
  @  B4
  │
  x  B3
  │
  x  B
  $ sl log -G --mutation -T '{desc}\n' --hidden
  @  B4
  │
  x  B3
  │
  │ o  B2
  ├─╯
  v  B

Commits that are not available locally are shown with --show-missing:

  $ sl debugstrip -q --hidden 'desc(B3)'
  $ sl log -G --mutation -T '{desc}\n'
  @  B4
  ╷
  x  B
  $ sl log -G --mutation -T '{desc}\n' --show-missing
  @  B4
  │
  ?  40f2b507616e  (not available locally)
  │
  x  B
  $ sl debugstrip -q --hidden 'desc(B) - desc(B2) - desc(B4)'
  $ sl log -G --mutation -T '{desc}\n'
  @  B4
  │
  ~
  $ sl log -G --mutation -T '{desc}\n' --show-missing
  @  B4
  │
  ?  40f2b507616e  (not available locally)
  │
  ?  6c81ed0049f8  (not available locally)
  $ sl log -G --mutation -T '{desc}\n' --show-missing --hidden
  @  B4
  │
  ?  40f2b507616e  (not available locally)
  │
  │ o  B2
  ├─╯
  ?  6c81ed0049f8  (not available locally)

`{grandparents}` lists the commits a dashed line leads to, if they are available
locally:

  $ newclientrepo
  $ echo a > a
  $ sl commit -qAm A0
  $ sl amend -q -m A1
  $ sl amend -q -m A2
  $ sl log -G --mutation -r 'desc(A2) + desc(A0)' -T '{desc} [{grandparents % "{node|short}"}]\n'
  @  A2 [3bd0155f0241]
  ╷
  x  A0 []
  $ sl debugstrip -q --hidden 'desc(A0)'
  $ sl log -G --mutation --show-missing -r . -T '{desc} [{grandparents}]\n'
  @  A2 []
  ╷
  ?  3bd0155f0241  (not available locally)

  $ sl log --mutation --show-missing
  abort: --show-missing requires --mutation and --graph
  [255]
  $ sl log -G --show-missing
  abort: --show-missing requires --mutation and --graph
  [255]

A split part that diverged is drawn as v, while the commit that was split,
replaced by one mutation, stays x. A mutation whose successors are all hidden
does not count:

  $ newclientrepo
  $ setconfig experimental.evolution.allowdivergence=true
  $ echo a > a
  $ sl commit -qAm A
  $ echo b > b
  $ echo c > c
  $ sl commit -qAm B
  $ rm $TESTTMP/partone.done
  $ HGEDITOR="\"$PYTHON\" \"$TESTTMP/partone.py\"" sl split -q --config ui.interactive=true > /dev/null << EOF
  > y
  > y
  > n
  > y
  > EOF
  $ echo c2 > c
  $ sl amend -q -m B2
  $ sl goto -q --hidden 'children(desc("B part 1")) - .'
  $ echo c3 > c
  $ sl amend -q -m B3
  $ sl log -G --mutation -T '{desc}\n'
  @  B3
  │
  │ o  B2
  ├─╯
  v  B
  │
  │ o  B part 1
  ├─╯
  x  B
  $ sl hide -q 'desc(B2)'
  $ sl log -G --mutation -T '{desc}\n'
  @  B3
  │
  x  B
  │
  │ o  B part 1
  ├─╯
  x  B

An obsolete commit replaced by more than one visible version is drawn as v.
A split is not divergence, and earlier versions of the diverged commit are not
diverged themselves:

  $ newclientrepo
  $ setconfig experimental.evolution.allowdivergence=true
  $ echo a > a
  $ sl commit -qAm A
  $ echo b > b
  $ echo c > c
  $ sl commit -qAm B0
  $ sl amend -q -m B
  $ rm $TESTTMP/partone.done
  $ HGEDITOR="\"$PYTHON\" \"$TESTTMP/partone.py\"" sl split -q --config ui.interactive=true << EOF
  > y
  > y
  > n
  > y
  > EOF
  diff --git a/b b/b
  new file mode 100644
  examine changes to 'b'? [Ynesfdaq?] y
  
  @@ -0,0 +1,1 @@
  +b
  record change 1/2 to 'b'? [Ynesfdaq?] y
  
  diff --git a/c b/c
  new file mode 100644
  examine changes to 'c'? [Ynesfdaq?] n
  
  Done splitting? [yN] y
  $ sl log -G --mutation -T '{desc}\n'
  @  B
  │
  │ o  B part 1
  ├─╯
  x  B
  │
  x  B0
  $ sl goto -q --hidden 'first(predecessors(.) & desc(B) - desc(B0) - .)'
  $ echo b2 > b
  $ sl amend -q -m B2
  $ sl log -G --mutation -T '{desc}\n'
  @  B2
  │
  │ o  B
  ├─╯
  │ o  B part 1
  ├─╯
  v  B
  │
  x  B0
  $ sl log -G --mutation -T '{desc}\n' --config experimental.graph.divergent-node=false
  @  B2
  │
  │ o  B
  ├─╯
  │ o  B part 1
  ├─╯
  x  B
  │
  x  B0

smartlog shows the same graph with --mutation, for the mutation history of the
current commit or of the given commits, where log -r shows exactly the given
commits:

  $ enable smartlog
  $ sl smartlog --mutation -T '{desc}\n'
  @  B2
  │
  │ o  B
  ├─╯
  │ o  B part 1
  ├─╯
  v  B
  │
  x  B0
  $ sl smartlog --mutation -T '{desc}\n' 'desc("B part 1")'
  @  B2
  │
  │ o  B
  ├─╯
  │ o  B part 1
  ├─╯
  v  B
  │
  x  B0
  $ sl log -G --mutation -T '{desc}\n' -r 'desc("B part 1")'
  o  B part 1
  │
  ~
  $ sl smartlog --mutation -T '{desc}\n' -r 'roots(all())'
  o  A
  $ sl smartlog --show-missing
  abort: --show-missing requires --mutation
  [255]
  $ sl smartlog --mutation --interactive
  abort: --mutation cannot be used with --interactive
  [255]

With the Meta templates, `sl sl -t` starts each row with how the commit was
created, and labels an obsolete commit with what it became next:

  $ TEST_PROD_CONFIGS=1 sl sl -t
  @  amend from 208bc801695e  test  Today at 00:00
  │  a36872291 (Backup pending)  Today at 00:00  test
  │  B2
  │
  │ o  split 2 of 2 from 208bc801695e  test  Today at 00:00
  ├─╯  65bba286d (Backup pending)  Today at 00:00  test
  │    B
  │
  │ o  split 1 of 2 from 208bc801695e  test  Today at 00:00
  ├─╯  4fb693f9a (Backup pending)  Today at 00:00  test
  │    B part 1
  │
  v  amend from d37eb5104304  test  Today at 00:00
  │  208bc8016 [Split into 4fb693f9a213, 65bba286d5f4] [Amended as a368722919e0]  Today at 00:00  test
  │  B
  │
  x  d37eb5104 [Amended as 208bc801695e]  Today at 00:00  test
     B0

Elsewhere the label says what the commit became in the end:

  $ TEST_PROD_CONFIGS=1 sl log -r 'desc(B0)' -T '{sl_node_info}\n'
  d37eb5104 [Split into 4fb693f9a213, 65bba286d5f4] [Rewritten into a368722919e0]
