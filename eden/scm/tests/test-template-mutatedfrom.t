  $ enable amend rebase
  $ setconfig diff.git=true
  $ newclientrepo

  $ echo a > a
  $ sl commit -qAm A
  $ echo b > b
  $ echo c > c
  $ sl commit -qAm B
  $ sl log -r . -T '[{mutatedfrom}]\n'
  []

Amend and metaedit record their operation, user, date and predecessor.

  $ echo b2 > b
  $ sl amend -q
  $ sl metaedit -q -m 'B v2'
  $ sl log -r . -T '{mutatedfrom}\n'
  metaedit
  $ sl log -r . -T "{mutatedfrom % '{operation} by {user} at {date|isodate} from {join(predecessors % \"{desc}\", \", \")}'}\n"
  metaedit by test at 1970-01-01 00:00 +0000 from B
  $ sl log -r . -T "{mutatedfrom % '{operation} from {predecessors}'}\n"
  metaedit from 4e1889790ff3
  $ sl log -r . -T "{mutatedfrom % '[{split}] {splitpart}'}\n"
  [] 0

Every part of a split shows the split, with its position.

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
  +b2
  record change 1/2 to 'b'? [Ynesfdaq?] y
  
  diff --git a/c b/c
  new file mode 100644
  examine changes to 'c'? [Ynesfdaq?] n
  
  Done splitting? [yN] y
  $ sl log -r 'desc("B part 1")::.' -T "{desc}: {mutatedfrom % '{operation} {splitpart} of {count(split)}: {join(split % \"{desc}\", \", \")}'}\n"
  B part 1: split 1 of 2: B part 1, B v2
  B v2: split 2 of 2: B part 1, B v2

A fold lists every folded commit.

  $ echo d > d
  $ sl commit -qAm D
  $ sl fold -q --from '.^' -m 'B part 2 and D'
  $ sl log -r . -T "{mutatedfrom % '{operation} from {join(predecessors % \"{desc}\", \", \")}'}\n"
  fold from B v2, D

A predecessor that is not available locally is still listed by its hash.

  $ sl debugstrip -q --hidden 'desc(D) - .'
  $ sl log -r . -T "{mutatedfrom % '{join(predecessors % \"{node|short}\", \" \")}'}\n"
  249525859eb3 50f7e5cdcca1

`{mutatedinto}` lists the mutations that replaced a commit, with the next
versions it was replaced by:

  $ sl log -r 'all()' --hidden -T "{desc}: {mutatedinto % '[{operation} by {user} at {date|isodate} into {join(successors % \"{desc}\", \", \")}]'}\n"
  A: 
  B: [amend by test at 1970-01-01 00:00 +0000 into B]
  B: [metaedit by test at 1970-01-01 00:00 +0000 into B v2]
  B v2: [split by test at 1970-01-01 00:00 +0000 into B part 1, B v2]
  B part 1: 
  B v2: [fold by test at 1970-01-01 00:00 +0000 into B part 2 and D]
  B part 2 and D: 

A commit replaced more than once lists each mutation, and a next version that
is not available locally is still listed by its hash:

  $ setconfig experimental.evolution.allowdivergence=true
  $ sl goto -q --hidden 'first(desc("B v2"))'
  $ sl amend -q -m 'B v3'
  $ sl log -r 'first(desc("B v2"))' --hidden -T "{mutatedinto}: {mutatedinto % '[{join(successors % \"{desc}\", \", \")}]'}\n"
  split amend: [B part 1, B v2][B v3]
  $ sl debugstrip -q --hidden 'desc("B v3")'
  $ sl log -r 'first(desc("B v2"))' --hidden -T "{mutatedinto % '[{join(successors % \"{node|short}\", \" \")}]'}\n"
  [8903feea5130 249525859eb3][2a69fa457056]
