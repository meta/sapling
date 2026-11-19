
#require no-eden


  $ eagerepo

Set up a repo

  $ setconfig ui.interactive=true

  $ newclientrepo a

Select no files

  $ touch empty-rw
  $ sl add empty-rw

  $ sl record --config ui.interactive=false
  abort: running non-interactively, use commit instead
  [255]
  $ sl commit -i --config ui.interactive=false
  abort: running non-interactively
  [255]
  $ sl commit -i empty-rw<<EOF
  > n
  > EOF
  diff --git a/empty-rw b/empty-rw
  new file mode 100644
  examine changes to 'empty-rw'? [Ynesfdaq?] n
  
  no changes to record
  [1]

  $ sl tip -p
  commit:      000000000000
  user:        
  date:        Thu Jan 01 00:00:00 1970 +0000
  
  

Select files but no hunks

  $ sl commit -i  empty-rw<<EOF
  > y
  > n
  > EOF
  diff --git a/empty-rw b/empty-rw
  new file mode 100644
  examine changes to 'empty-rw'? [Ynesfdaq?] y
  
  abort: empty commit message
  [255]

  $ sl tip -p
  commit:      000000000000
  user:        
  date:        Thu Jan 01 00:00:00 1970 +0000
  
  

Abort for untracked

  $ touch untracked
  $ sl commit -i -m should-fail empty-rw untracked
  abort: untracked: file not tracked!
  [255]
  $ rm untracked

Record empty file

  $ sl commit -i -d '0 0' -m empty empty-rw<<EOF
  > y
  > y
  > EOF
  diff --git a/empty-rw b/empty-rw
  new file mode 100644
  examine changes to 'empty-rw'? [Ynesfdaq?] y
  

  $ sl tip -p
  commit:      c0708cf4e46e
  user:        test
  date:        Thu Jan 01 00:00:00 1970 +0000
  summary:     empty
  
  

Summary shows we updated to the new cset

  $ sl summary
  parent: c0708cf4e46e 
   empty
  commit: (clean)
  phases: 1 draft

Rename empty file

  $ sl mv empty-rw empty-rename
  $ sl commit -i -d '1 0' -m rename<<EOF
  > y
  > EOF
  diff --git a/empty-rw b/empty-rename
  rename from empty-rw
  rename to empty-rename
  examine changes to 'empty-rw' and 'empty-rename'? [Ynesfdaq?] y
  

  $ sl tip -p
  commit:      d695e8dcb197
  user:        test
  date:        Thu Jan 01 00:00:01 1970 +0000
  summary:     rename
  
  

Copy empty file

  $ sl cp empty-rename empty-copy
  $ sl commit -i -d '2 0' -m copy<<EOF
  > y
  > EOF
  diff --git a/empty-rename b/empty-copy
  copy from empty-rename
  copy to empty-copy
  examine changes to 'empty-rename' and 'empty-copy'? [Ynesfdaq?] y
  

  $ sl tip -p
  commit:      1d4b90bea524
  user:        test
  date:        Thu Jan 01 00:00:02 1970 +0000
  summary:     copy
  
  

Delete empty file

  $ sl rm empty-copy
  $ sl commit -i -d '3 0' -m delete<<EOF
  > y
  > EOF
  diff --git a/empty-copy b/empty-copy
  deleted file mode 100644
  examine changes to 'empty-copy'? [Ynesfdaq?] y
  

  $ sl tip -p
  commit:      b39a238f01a1
  user:        test
  date:        Thu Jan 01 00:00:03 1970 +0000
  summary:     delete
  
  

Add binary file

  $ function sed_hashes() {
  >   sed 's/\b[0-9a-f]{12}\b/aaaaaaaaaaaa/g'
  > }

  $ sl bundle --base -2 tip.bundle
  1 changesets found
  $ sl add tip.bundle
  $ sl commit -i -d '4 0' -m binary<<EOF
  > y
  > EOF
  diff --git a/tip.bundle b/tip.bundle
  new file mode 100644
  this is a binary file
  examine changes to 'tip.bundle'? [Ynesfdaq?] y

  $ sl tip -p | sed_hashes
  commit:      aaaaaaaaaaaa
  user:        test
  date:        Thu Jan 01 00:00:04 1970 +0000
  summary:     binary
  
  diff -r aaaaaaaaaaaa -r aaaaaaaaaaaa tip.bundle
  Binary file tip.bundle has changed

Change binary file

  $ sl bundle --base -2 tip.bundle
  1 changesets found
  $ sl commit -i -d '5 0' -m binary-change<<EOF
  > y
  > EOF
  diff --git a/tip.bundle b/tip.bundle
  this modifies a binary file (all or nothing)
  examine changes to 'tip.bundle'? [Ynesfdaq?] y

  $ sl tip -p | sed_hashes
  commit:      aaaaaaaaaaaa
  user:        test
  date:        Thu Jan 01 00:00:05 1970 +0000
  summary:     binary-change
  
  diff -r aaaaaaaaaaaa -r aaaaaaaaaaaa tip.bundle
  Binary file tip.bundle has changed

Rename and change binary file

  $ sl mv tip.bundle top.bundle
  $ sl bundle --base -2 top.bundle
  1 changesets found
  $ sl commit -i -d '6 0' -m binary-change-rename<<EOF
  > y
  > EOF
  diff --git a/tip.bundle b/top.bundle
  rename from tip.bundle
  rename to top.bundle
  this modifies a binary file (all or nothing)
  examine changes to 'tip.bundle' and 'top.bundle'? [Ynesfdaq?] y

  $ sl tip -p | sed_hashes
  commit:      aaaaaaaaaaaa
  user:        test
  date:        Thu Jan 01 00:00:06 1970 +0000
  summary:     binary-change-rename
  
  diff -r aaaaaaaaaaaa -r aaaaaaaaaaaa tip.bundle
  Binary file tip.bundle has changed
  diff -r aaaaaaaaaaaa -r aaaaaaaaaaaa top.bundle
  Binary file top.bundle has changed

Add plain file

  $ for i in 1 2 3 4 5 6 7 8 9 10; do
  >     echo $i >> plain
  > done

  $ sl add plain
  $ sl commit -i -d '7 0' -m plain plain<<EOF
  > y
  > y
  > EOF
  diff --git a/plain b/plain
  new file mode 100644
  examine changes to 'plain'? [Ynesfdaq?] y
  
  @@ -0,0 +1,10 @@
  +1
  +2
  +3
  +4
  +5
  +6
  +7
  +8
  +9
  +10
  record this change to 'plain'? [Ynesfdaq?] y
  
  $ sl tip -p | sed_hashes
  commit:      aaaaaaaaaaaa
  user:        test
  date:        Thu Jan 01 00:00:07 1970 +0000
  summary:     plain
  
  diff -r aaaaaaaaaaaa -r aaaaaaaaaaaa plain
  --- /dev/null	Thu Jan 01 00:00:00 1970 +0000
  +++ b/plain	Thu Jan 01 00:00:07 1970 +0000
  @@ -0,0 +1,10 @@
  +1
  +2
  +3
  +4
  +5
  +6
  +7
  +8
  +9
  +10
Modify end of plain file with username unset

  $ echo 11 >> plain
  $ unset HGUSER
  $ sl commit -i --config ui.username= -d '8 0' -m end plain
  abort: no username supplied
  (use `sl config --user ui.username "First Last <me@example.com>"` to set your username)
  [255]


Modify end of plain file, also test that diffopts are accounted for

  $ HGUSER="test"
  $ export HGUSER
  $ sl commit -i --config diff.showfunc=true -d '8 0' -m end plain <<EOF
  > y
  > y
  > EOF
  diff --git a/plain b/plain
  1 hunks, 1 lines changed
  examine changes to 'plain'? [Ynesfdaq?] y
  
  @@ -8,3 +8,4 @@ 7
   8
   9
   10
  +11
  record this change to 'plain'? [Ynesfdaq?] y
  

Modify end of plain file, no EOL

  $ echo -n 9b5b0d948b4337a6766950f9da247b44068e38b2 >> plain
  $ sl commit -i -d '9 0' -m noeol plain <<EOF
  > y
  > y
  > EOF
  diff --git a/plain b/plain
  1 hunks, 1 lines changed
  examine changes to 'plain'? [Ynesfdaq?] y
  
  @@ -9,3 +9,4 @@ 8
   9
   10
   11
  +9b5b0d948b4337a6766950f9da247b44068e38b2
  \ No newline at end of file
  record this change to 'plain'? [Ynesfdaq?] y

Record showfunc should preserve function across sections

  $ cat > f1.py <<EOF
  > def annotate(ui, repo, *pats, **opts):
  >     """show changeset information by line for each file
  > 
  >     List changes in files, showing the revision id responsible for
  >     each line.
  > 
  >     This command is useful for discovering when a change was made and
  >     by whom.
  > 
  >     If you include -f/-u/-d, the revision number is suppressed unless
  >     you also include -the revision number is suppressed unless
  >     you also include -n.
  > 
  >     Without the -a/--text option, annotate will avoid processing files
  >     it detects as binary. With -a, annotate will annotate the file
  >     anyway, although the results will probably be neither useful
  >     nor desirable.
  > 
  >     Returns 0 on success.
  >     """
  >     return 0
  > def archive(ui, repo, dest, **opts):
  >     '''create an unversioned archive of a repository revision
  > 
  >     By default, the revision used is the parent of the working
  >     directory; use -r/--rev to specify a different revision.
  > 
  >     The archive type is automatically detected based on file
  >     extension (to override, use -t/--type).
  > 
  >     .. container:: verbose
  > 
  >     Valid types are:
  > EOF
  $ sl add f1.py
  $ sl commit -m funcs
  $ cat > f1.py <<EOF
  > def annotate(ui, repo, *pats, **opts):
  >     """show changeset information by line for each file
  > 
  >     List changes in files, showing the revision id responsible for
  >     each line
  > 
  >     This command is useful for discovering when a change was made and
  >     by whom.
  > 
  >     Without the -a/--text option, annotate will avoid processing files
  >     it detects as binary. With -a, annotate will annotate the file
  >     anyway, although the results will probably be neither useful
  >     nor desirable.
  > 
  >     Returns 0 on success.
  >     """
  >     return 0
  > def archive(ui, repo, dest, **opts):
  >     '''create an unversioned archive of a repository revision
  > 
  >     By default, the revision used is the parent of the working
  >     directory; use -r/--rev to specify a different revision.
  > 
  >     The archive type is automatically detected based on file
  >     extension (or override using -t/--type).
  > 
  >     .. container:: verbose
  > 
  >     Valid types are:
  > EOF
  $ sl commit -i -m interactive <<EOF
  > y
  > y
  > y
  > y
  > EOF
  diff --git a/f1.py b/f1.py
  3 hunks, 6 lines changed
  examine changes to 'f1.py'? [Ynesfdaq?] y
  
  @@ -2,8 +2,8 @@ def annotate(ui, repo, *pats, **opts):
       """show changeset information by line for each file
   
       List changes in files, showing the revision id responsible for
  -    each line.
  +    each line
   
       This command is useful for discovering when a change was made and
       by whom.
   
  record change 1/3 to 'f1.py'? [Ynesfdaq?] y
  
  @@ -6,11 +6,7 @@ def annotate(ui, repo, *pats, **opts):
   
       This command is useful for discovering when a change was made and
       by whom.
   
  -    If you include -f/-u/-d, the revision number is suppressed unless
  -    you also include -the revision number is suppressed unless
  -    you also include -n.
  -
       Without the -a/--text option, annotate will avoid processing files
       it detects as binary. With -a, annotate will annotate the file
       anyway, although the results will probably be neither useful
  record change 2/3 to 'f1.py'? [Ynesfdaq?] y
  
  @@ -26,7 +22,7 @@ def archive(ui, repo, dest, **opts):
       directory; use -r/--rev to specify a different revision.
   
       The archive type is automatically detected based on file
  -    extension (to override, use -t/--type).
  +    extension (or override using -t/--type).
   
       .. container:: verbose
   
  record change 3/3 to 'f1.py'? [Ynesfdaq?] y
  

Modify end of plain file, add EOL

  $ echo >> plain
  $ echo 1 > plain2
  $ sl add plain2
  $ sl commit -i -d '10 0' -m eol plain plain2 <<EOF
  > y
  > y
  > y
  > y
  > EOF
  diff --git a/plain b/plain
  1 hunks, 1 lines changed
  examine changes to 'plain'? [Ynesfdaq?] y
  
  @@ -9,4 +9,4 @@ 8
   9
   10
   11
  -9b5b0d948b4337a6766950f9da247b44068e38b2
  \ No newline at end of file
  +9b5b0d948b4337a6766950f9da247b44068e38b2
  record change 1/2 to 'plain'? [Ynesfdaq?] y
  
  diff --git a/plain2 b/plain2
  new file mode 100644
  examine changes to 'plain2'? [Ynesfdaq?] y
  
  @@ -0,0 +1,1 @@
  +1
  record change 2/2 to 'plain2'? [Ynesfdaq?] y
Modify beginning, trim end, record both, add another file to test
changes numbering

  $ rm plain
  $ for i in 2 2 3 4 5 6 7 8 9 10; do
  >   echo $i >> plain
  > done
  $ echo 2 >> plain2

  $ sl commit -i -d '10 0' -m begin-and-end plain plain2 <<EOF
  > y
  > y
  > y
  > y
  > y
  > EOF
  diff --git a/plain b/plain
  2 hunks, 3 lines changed
  examine changes to 'plain'? [Ynesfdaq?] y
  
  @@ -1,4 +1,4 @@
  -1
  +2
   2
   3
   4
  record change 1/3 to 'plain'? [Ynesfdaq?] y
  
  @@ -8,5 +8,3 @@ 7
   8
   9
   10
  -11
  -9b5b0d948b4337a6766950f9da247b44068e38b2
  record change 2/3 to 'plain'? [Ynesfdaq?] y
  
  diff --git a/plain2 b/plain2
  1 hunks, 1 lines changed
  examine changes to 'plain2'? [Ynesfdaq?] y
  
  @@ -1,1 +1,2 @@
   1
  +2
  record change 3/3 to 'plain2'? [Ynesfdaq?] y

  $ sl tip -p | sed_hashes
  commit:      aaaaaaaaaaaa
  user:        test
  date:        Thu Jan 01 00:00:10 1970 +0000
  summary:     begin-and-end
  
  diff -r aaaaaaaaaaaa -r aaaaaaaaaaaa plain
  --- a/plain	Thu Jan 01 00:00:10 1970 +0000
  +++ b/plain	Thu Jan 01 00:00:10 1970 +0000
  @@ -1,4 +1,4 @@
  -1
  +2
   2
   3
   4
  @@ -8,5 +8,3 @@
   8
   9
   10
  -11
  -9b5b0d948b4337a6766950f9da247b44068e38b2
  diff -r aaaaaaaaaaaa -r aaaaaaaaaaaa plain2
  --- a/plain2	Thu Jan 01 00:00:10 1970 +0000
  +++ b/plain2	Thu Jan 01 00:00:10 1970 +0000
  @@ -1,1 +1,2 @@
   1
  +2

Trim beginning, modify end

  $ rm plain
  > for i in 4 5 6 7 8 9 10.new; do
  >   echo $i >> plain
  > done

Record end

  $ sl commit -i -d '11 0' -m end-only plain <<EOF
  > y
  > n
  > y
  > EOF
  diff --git a/plain b/plain
  2 hunks, 4 lines changed
  examine changes to 'plain'? [Ynesfdaq?] y
  
  @@ -1,9 +1,6 @@
  -2
  -2
  -3
   4
   5
   6
   7
   8
   9
  record change 1/2 to 'plain'? [Ynesfdaq?] n
  
  @@ -4,7 +1,7 @@
   4
   5
   6
   7
   8
   9
  -10
  +10.new
  record change 2/2 to 'plain'? [Ynesfdaq?] y
  

  $ sl tip -p | sed_hashes
  commit:      aaaaaaaaaaaa
  user:        test
  date:        Thu Jan 01 00:00:11 1970 +0000
  summary:     end-only
  
  diff -r aaaaaaaaaaaa -r aaaaaaaaaaaa plain
  --- a/plain	Thu Jan 01 00:00:10 1970 +0000
  +++ b/plain	Thu Jan 01 00:00:11 1970 +0000
  @@ -7,4 +7,4 @@
   7
   8
   9
  -10
  +10.new

Record beginning

  $ sl commit -i -d '12 0' -m begin-only plain <<EOF
  > y
  > y
  > EOF
  diff --git a/plain b/plain
  1 hunks, 3 lines changed
  examine changes to 'plain'? [Ynesfdaq?] y
  
  @@ -1,6 +1,3 @@
  -2
  -2
  -3
   4
   5
   6
  record this change to 'plain'? [Ynesfdaq?] y
  

  $ sl tip -p | sed_hashes
  commit:      aaaaaaaaaaaa
  user:        test
  date:        Thu Jan 01 00:00:12 1970 +0000
  summary:     begin-only
  
  diff -r aaaaaaaaaaaa -r aaaaaaaaaaaa plain
  --- a/plain	Thu Jan 01 00:00:11 1970 +0000
  +++ b/plain	Thu Jan 01 00:00:12 1970 +0000
  @@ -1,6 +1,3 @@
  -2
  -2
  -3
   4
   5
   6

Add to beginning, trim from end

  $ rm plain
  $ for i in 1 2 3 4 5 6 7 8 9; do
  >  echo $i >> plain
  > done

Record end

  $ sl commit -i --traceback -d '13 0' -m end-again plain<<EOF
  > y
  > n
  > y
  > EOF
  diff --git a/plain b/plain
  2 hunks, 4 lines changed
  examine changes to 'plain'? [Ynesfdaq?] y
  
  @@ -1,6 +1,9 @@
  +1
  +2
  +3
   4
   5
   6
   7
   8
   9
  record change 1/2 to 'plain'? [Ynesfdaq?] n
  
  @@ -1,7 +4,6 @@
   4
   5
   6
   7
   8
   9
  -10.new
  record change 2/2 to 'plain'? [Ynesfdaq?] y
  

Add to beginning, middle, end

  $ rm plain
  $ for i in 1 2 3 4 5 5.new 5.reallynew 6 7 8 9 10 11; do
  >   echo $i >> plain
  > done

Record beginning, middle, and test that format-breaking diffopts are ignored

  $ sl commit -i --config diff.noprefix=True -d '14 0' -m middle-only plain <<EOF
  > y
  > y
  > y
  > n
  > EOF
  diff --git a/plain b/plain
  3 hunks, 7 lines changed
  examine changes to 'plain'? [Ynesfdaq?] y
  
  @@ -1,2 +1,5 @@
  +1
  +2
  +3
   4
   5
  record change 1/3 to 'plain'? [Ynesfdaq?] y
  
  @@ -1,6 +4,8 @@
   4
   5
  +5.new
  +5.reallynew
   6
   7
   8
   9
  record change 2/3 to 'plain'? [Ynesfdaq?] y
  
  @@ -3,4 +8,6 @@
   6
   7
   8
   9
  +10
  +11
  record change 3/3 to 'plain'? [Ynesfdaq?] n
  

  $ sl tip -p | sed_hashes
  commit:      aaaaaaaaaaaa
  user:        test
  date:        Thu Jan 01 00:00:14 1970 +0000
  summary:     middle-only
  
  diff -r aaaaaaaaaaaa -r aaaaaaaaaaaa plain
  --- a/plain	Thu Jan 01 00:00:13 1970 +0000
  +++ b/plain	Thu Jan 01 00:00:14 1970 +0000
  @@ -1,5 +1,10 @@
  +1
  +2
  +3
   4
   5
  +5.new
  +5.reallynew
   6
   7
   8

Record end

  $ sl commit -i -d '15 0' -m end-only plain <<EOF
  > y
  > y
  > EOF
  diff --git a/plain b/plain
  1 hunks, 2 lines changed
  examine changes to 'plain'? [Ynesfdaq?] y
  
  @@ -9,3 +9,5 @@ 6
   7
   8
   9
  +10
  +11
  record this change to 'plain'? [Ynesfdaq?] y
  

  $ sl tip -p | sed_hashes
  commit:      aaaaaaaaaaaa
  user:        test
  date:        Thu Jan 01 00:00:15 1970 +0000
  summary:     end-only
  
  diff -r aaaaaaaaaaaa -r aaaaaaaaaaaa plain
  --- a/plain	Thu Jan 01 00:00:14 1970 +0000
  +++ b/plain	Thu Jan 01 00:00:15 1970 +0000
  @@ -9,3 +9,5 @@
   7
   8
   9
  +10
  +11

  $ mkdir subdir
  $ cd subdir
  $ echo a > a
  $ sl ci -d '16 0' -Amsubdir
  adding subdir/a

  $ echo a >> a
  $ sl commit -i -d '16 0' -m subdir-change a <<EOF
  > y
  > y
  > EOF
  diff --git a/subdir/a b/subdir/a
  1 hunks, 1 lines changed
  examine changes to 'subdir/a'? [Ynesfdaq?] y
  
  @@ -1,1 +1,2 @@
   a
  +a
  record this change to 'subdir/a'? [Ynesfdaq?] y
  

  $ sl tip -p | sed_hashes
  commit:      aaaaaaaaaaaa
  user:        test
  date:        Thu Jan 01 00:00:16 1970 +0000
  summary:     subdir-change
  
  diff -r aaaaaaaaaaaa -r aaaaaaaaaaaa subdir/a
  --- a/subdir/a	Thu Jan 01 00:00:16 1970 +0000
  +++ b/subdir/a	Thu Jan 01 00:00:16 1970 +0000
  @@ -1,1 +1,2 @@
   a
  +a

  $ echo a > f1
  $ echo b > f2
  $ sl add f1 f2

  $ sl ci -mz -d '17 0'

  $ echo a >> f1
  $ echo b >> f2

Help, quit

  $ sl commit -i <<EOF
  > ?
  > q
  > EOF
  diff --git a/subdir/f1 b/subdir/f1
  1 hunks, 1 lines changed
  examine changes to 'subdir/f1'? [Ynesfdaq?] ?
  
  y - yes, record this change
  n - no, skip this change
  e - edit this change manually
  s - skip remaining changes to this file
  f - record remaining changes to this file
  d - done, skip remaining changes and files
  a - record all changes to all remaining files
  q - quit, recording no changes
  ? - ? (display help)
  examine changes to 'subdir/f1'? [Ynesfdaq?] q
  
  abort: user quit
  [255]

Skip

  $ sl commit -i <<EOF
  > s
  > EOF
  diff --git a/subdir/f1 b/subdir/f1
  1 hunks, 1 lines changed
  examine changes to 'subdir/f1'? [Ynesfdaq?] s
  
  diff --git a/subdir/f2 b/subdir/f2
  1 hunks, 1 lines changed
  examine changes to 'subdir/f2'? [Ynesfdaq?] abort: response expected
  [255]

No

  $ sl commit -i <<EOF
  > n
  > EOF
  diff --git a/subdir/f1 b/subdir/f1
  1 hunks, 1 lines changed
  examine changes to 'subdir/f1'? [Ynesfdaq?] n
  
  diff --git a/subdir/f2 b/subdir/f2
  1 hunks, 1 lines changed
  examine changes to 'subdir/f2'? [Ynesfdaq?] abort: response expected
  [255]

f, quit

  $ sl commit -i <<EOF
  > f
  > q
  > EOF
  diff --git a/subdir/f1 b/subdir/f1
  1 hunks, 1 lines changed
  examine changes to 'subdir/f1'? [Ynesfdaq?] f
  
  diff --git a/subdir/f2 b/subdir/f2
  1 hunks, 1 lines changed
  examine changes to 'subdir/f2'? [Ynesfdaq?] q
  
  abort: user quit
  [255]

s, all

  $ sl commit -i -d '18 0' -mx <<EOF
  > s
  > a
  > EOF
  diff --git a/subdir/f1 b/subdir/f1
  1 hunks, 1 lines changed
  examine changes to 'subdir/f1'? [Ynesfdaq?] s
  
  diff --git a/subdir/f2 b/subdir/f2
  1 hunks, 1 lines changed
  examine changes to 'subdir/f2'? [Ynesfdaq?] a
  

  $ sl tip -p | sed_hashes
  commit:      aaaaaaaaaaaa
  user:        test
  date:        Thu Jan 01 00:00:18 1970 +0000
  summary:     x
  
  diff -r aaaaaaaaaaaa -r aaaaaaaaaaaa subdir/f2
  --- a/subdir/f2	Thu Jan 01 00:00:17 1970 +0000
  +++ b/subdir/f2	Thu Jan 01 00:00:18 1970 +0000
  @@ -1,1 +1,2 @@
   b
  +b

f

  $ sl commit -i -d '19 0' -my <<EOF
  > f
  > EOF
  diff --git a/subdir/f1 b/subdir/f1
  1 hunks, 1 lines changed
  examine changes to 'subdir/f1'? [Ynesfdaq?] f
  

  $ sl tip -p | sed_hashes
  commit:      aaaaaaaaaaaa
  user:        test
  date:        Thu Jan 01 00:00:19 1970 +0000
  summary:     y
  
  diff -r aaaaaaaaaaaa -r aaaaaaaaaaaa subdir/f1
  --- a/subdir/f1	Thu Jan 01 00:00:18 1970 +0000
  +++ b/subdir/f1	Thu Jan 01 00:00:19 1970 +0000
  @@ -1,1 +1,2 @@
   a
  +a

#if execbit

Preserve chmod +x

  $ chmod +x f1
  $ echo a >> f1
  $ sl commit -i -d '20 0' -mz <<EOF
  > y
  > y
  > y
  > EOF
  diff --git a/subdir/f1 b/subdir/f1
  old mode 100644
  new mode 100755
  1 hunks, 1 lines changed
  examine changes to 'subdir/f1'? [Ynesfdaq?] y
  
  @@ -1,2 +1,3 @@
   a
   a
  +a
  record this change to 'subdir/f1'? [Ynesfdaq?] y
  

  $ sl tip --config diff.git=True -p | sed_hashes
  commit:      aaaaaaaaaaaa
  user:        test
  date:        Thu Jan 01 00:00:20 1970 +0000
  summary:     z
  
  diff --git a/subdir/f1 b/subdir/f1
  old mode 100644
  new mode 100755
  --- a/subdir/f1
  +++ b/subdir/f1
  @@ -1,2 +1,3 @@
   a
   a
  +a

Preserve execute permission on original

  $ echo b >> f1
  $ sl commit -i -d '21 0' -maa <<EOF
  > y
  > y
  > y
  > EOF
  diff --git a/subdir/f1 b/subdir/f1
  1 hunks, 1 lines changed
  examine changes to 'subdir/f1'? [Ynesfdaq?] y
  
  @@ -1,3 +1,4 @@
   a
   a
   a
  +b
  record this change to 'subdir/f1'? [Ynesfdaq?] y
  

  $ sl tip --config diff.git=True -p | sed_hashes
  commit:      aaaaaaaaaaaa
  user:        test
  date:        Thu Jan 01 00:00:21 1970 +0000
  summary:     aa
  
  diff --git a/subdir/f1 b/subdir/f1
  --- a/subdir/f1
  +++ b/subdir/f1
  @@ -1,3 +1,4 @@
   a
   a
   a
  +b

Preserve chmod -x

  $ chmod -x f1
  $ echo c >> f1
  $ sl commit -i -d '22 0' -mab <<EOF
  > y
  > y
  > y
  > EOF
  diff --git a/subdir/f1 b/subdir/f1
  old mode 100755
  new mode 100644
  1 hunks, 1 lines changed
  examine changes to 'subdir/f1'? [Ynesfdaq?] y
  
  @@ -2,3 +2,4 @@ a
   a
   a
   b
  +c
  record this change to 'subdir/f1'? [Ynesfdaq?] y
  

  $ sl tip --config diff.git=True -p | sed_hashes
  commit:      aaaaaaaaaaaa
  user:        test
  date:        Thu Jan 01 00:00:22 1970 +0000
  summary:     ab
  
  diff --git a/subdir/f1 b/subdir/f1
  old mode 100755
  new mode 100644
  --- a/subdir/f1
  +++ b/subdir/f1
  @@ -2,3 +2,4 @@
   a
   a
   b
  +c

#else

Slightly bogus tests to get almost same repo structure as when x bit is used
- but with different hashes.

Mock "Preserve chmod +x"

  $ echo a >> f1
  $ sl commit -i -d '20 0' -mz <<EOF
  > y
  > y
  > y
  > EOF
  diff --git a/subdir/f1 b/subdir/f1
  1 hunks, 1 lines changed
  examine changes to 'subdir/f1'? [Ynesfdaq?] y
  
  @@ -1,2 +1,3 @@
   a
   a
  +a
  record this change to 'subdir/f1'? [Ynesfdaq?] y
  

  $ sl tip --config diff.git=True -p
  changeset:   24:c26cfe2c4eb0
  user:        test
  date:        Thu Jan 01 00:00:20 1970 +0000
  summary:     z
  
  diff --git a/subdir/f1 b/subdir/f1
  --- a/subdir/f1
  +++ b/subdir/f1
  @@ -1,2 +1,3 @@
   a
   a
  +a
  

Mock "Preserve execute permission on original"

  $ echo b >> f1
  $ sl commit -i -d '21 0' -maa <<EOF
  > y
  > y
  > y
  > EOF
  diff --git a/subdir/f1 b/subdir/f1
  1 hunks, 1 lines changed
  examine changes to 'subdir/f1'? [Ynesfdaq?] y
  
  @@ -1,3 +1,4 @@
   a
   a
   a
  +b
  record this change to 'subdir/f1'? [Ynesfdaq?] y
  

  $ sl tip --config diff.git=True -p
  changeset:   25:a48d2d60adde
  user:        test
  date:        Thu Jan 01 00:00:21 1970 +0000
  summary:     aa
  
  diff --git a/subdir/f1 b/subdir/f1
  --- a/subdir/f1
  +++ b/subdir/f1
  @@ -1,3 +1,4 @@
   a
   a
   a
  +b
  

Mock "Preserve chmod -x"

  $ chmod -x f1
  $ echo c >> f1
  $ sl commit -i -d '22 0' -mab <<EOF
  > y
  > y
  > y
  > EOF
  diff --git a/subdir/f1 b/subdir/f1
  1 hunks, 1 lines changed
  examine changes to 'subdir/f1'? [Ynesfdaq?] y
  
  @@ -2,3 +2,4 @@ a
   a
   a
   b
  +c
  record this change to 'subdir/f1'? [Ynesfdaq?] y
  

  $ sl tip --config diff.git=True -p
  changeset:   26:5cc89ae210fa
  user:        test
  date:        Thu Jan 01 00:00:22 1970 +0000
  summary:     ab
  
  diff --git a/subdir/f1 b/subdir/f1
  --- a/subdir/f1
  +++ b/subdir/f1
  @@ -2,3 +2,4 @@
   a
   a
   b
  +c
  

#endif

  $ cd ..


Abort early when a merge is in progress

  $ newclientrepo
  $ drawdag <<'EOS'
  > Y Z
  > |/
  > X
  > EOS

  $ sl up -Cq $Y
  $ sl merge $Z
  1 files updated, 0 files merged, 0 files removed, 0 files unresolved
  (branch merge, don't forget to commit)

  $ sl commit -i -m'will abort'
  abort: cannot partially commit a merge (use "sl commit" instead)
  [255]

Editing patch (and ignoring trailing text)

  $ cd $TESTTMP/a
  $ cat > editor.sh << '__EOF__'
  > sed -e 7d -e '5s/^-/ /' -e '/^# ---/i\
  > trailing\nditto' "$1" > tmp
  > mv tmp "$1"
  > __EOF__
  $ cat > editedfile << '__EOF__'
  > This is the first line
  > This is the second line
  > This is the third line
  > __EOF__
  $ sl add editedfile
  $ sl commit -medit-patch-1
  $ cat > editedfile << '__EOF__'
  > This line has changed
  > This change will be committed
  > This is the third line
  > __EOF__
  $ HGEDITOR="\"sh\" \"`pwd`/editor.sh\"" sl commit -i -d '23 0' -medit-patch-2 <<EOF
  > y
  > e
  > EOF
  diff --git a/editedfile b/editedfile
  1 hunks, 2 lines changed
  examine changes to 'editedfile'? [Ynesfdaq?] y
  
  @@ -1,3 +1,3 @@
  -This is the first line
  -This is the second line
  +This line has changed
  +This change will be committed
   This is the third line
  record this change to 'editedfile'? [Ynesfdaq?] e
  
  $ cat editedfile
  This line has changed
  This change will be committed
  This is the third line
  $ sl cat -r tip editedfile
  This is the first line
  This change will be committed
  This is the third line
  $ sl revert editedfile

Trying to edit patch for whole file

  $ echo "This is the fourth line" >> editedfile
  $ sl commit -i <<EOF
  > e
  > q
  > EOF
  diff --git a/editedfile b/editedfile
  1 hunks, 1 lines changed
  examine changes to 'editedfile'? [Ynesfdaq?] e
  
  cannot edit patch for whole file
  examine changes to 'editedfile'? [Ynesfdaq?] q
  
  abort: user quit
  [255]
  $ sl revert editedfile

Removing changes from patch

  $ sed -e '3s/third/second/' -e '2s/will/will not/' -e 1d editedfile > tmp
  $ mv tmp editedfile
  $ echo "This line has been added" >> editedfile
  $ cat > editor.sh << '__EOF__'
  > sed -e 's/^[-+]/ /' "$1" > tmp
  > mv tmp "$1"
  > __EOF__
  $ HGEDITOR="\"sh\" \"`pwd`/editor.sh\"" sl commit -i <<EOF
  > y
  > e
  > EOF
  diff --git a/editedfile b/editedfile
  1 hunks, 3 lines changed
  examine changes to 'editedfile'? [Ynesfdaq?] y
  
  @@ -1,3 +1,3 @@
  -This is the first line
  -This change will be committed
  -This is the third line
  +This change will not be committed
  +This is the second line
  +This line has been added
  record this change to 'editedfile'? [Ynesfdaq?] e
  
  no changes to record
  [1]
  $ cat editedfile
  This change will not be committed
  This is the second line
  This line has been added
  $ sl cat -r tip editedfile
  This is the first line
  This change will be committed
  This is the third line
  $ sl revert editedfile

Invalid patch

  $ sed -e '3s/third/second/' -e '2s/will/will not/' -e 1d editedfile > tmp
  $ mv tmp editedfile
  $ echo "This line has been added" >> editedfile
  $ cat > editor.sh << '__EOF__'
  > sed s/This/That/ "$1" > tmp
  > mv tmp "$1"
  > __EOF__
  $ HGEDITOR="\"sh\" \"`pwd`/editor.sh\"" sl commit -i <<EOF
  > y
  > e
  > EOF
  diff --git a/editedfile b/editedfile
  1 hunks, 3 lines changed
  examine changes to 'editedfile'? [Ynesfdaq?] y
  
  @@ -1,3 +1,3 @@
  -This is the first line
  -This change will be committed
  -This is the third line
  +This change will not be committed
  +This is the second line
  +This line has been added
  record this change to 'editedfile'? [Ynesfdaq?] e
  
  patching file editedfile
  Hunk #1 FAILED at 0
  abort: selected changes do not apply to editedfile
  (an edited hunk no longer matches the file; select it again without editing, or keep its context lines unchanged)
  [255]
  $ cat editedfile
  This change will not be committed
  This is the second line
  This line has been added
  $ sl cat -r tip editedfile
  This is the first line
  This change will be committed
  This is the third line

The selection is applied in memory, so no rejects file is written to the
working copy.

  $ test -f editedfile.rej
  [1]

Malformed patch - error handling

  $ cat > editor.sh << '__EOF__'
  > sed -e '/^@/p' "$1" > tmp
  > mv tmp "$1"
  > __EOF__
  $ HGEDITOR="\"sh\" \"`pwd`/editor.sh\"" sl commit -i <<EOF
  > y
  > e
  > EOF
  diff --git a/editedfile b/editedfile
  1 hunks, 3 lines changed
  examine changes to 'editedfile'? [Ynesfdaq?] y
  
  @@ -1,3 +1,3 @@
  -This is the first line
  -This change will be committed
  -This is the third line
  +This change will not be committed
  +This is the second line
  +This line has been added
  record this change to 'editedfile'? [Ynesfdaq?] e
  
  abort: error parsing patch: unhandled transition: range -> range
  [255]

Exiting editor with status 1, ignores the edit but does not stop the recording
session

  $ HGEDITOR=false sl commit -i <<EOF
  > y
  > e
  > n
  > EOF
  diff --git a/editedfile b/editedfile
  1 hunks, 3 lines changed
  examine changes to 'editedfile'? [Ynesfdaq?] y
  
  @@ -1,3 +1,3 @@
  -This is the first line
  -This change will be committed
  -This is the third line
  +This change will not be committed
  +This is the second line
  +This line has been added
  record this change to 'editedfile'? [Ynesfdaq?] e
  
  editor exited with exit code 1
  record this change to 'editedfile'? [Ynesfdaq?] n
  
  no changes to record
  [1]


random text in random positions is still an error

  $ cat > editor.sh << '__EOF__'
  > sed -e '/^@/i\
  > other' "$1" > tmp
  > mv tmp "$1"
  > __EOF__
  $ HGEDITOR="\"sh\" \"`pwd`/editor.sh\"" sl commit -i <<EOF
  > y
  > e
  > EOF
  diff --git a/editedfile b/editedfile
  1 hunks, 3 lines changed
  examine changes to 'editedfile'? [Ynesfdaq?] y
  
  @@ -1,3 +1,3 @@
  -This is the first line
  -This change will be committed
  -This is the third line
  +This change will not be committed
  +This is the second line
  +This line has been added
  record this change to 'editedfile'? [Ynesfdaq?] e
  
  abort: error parsing patch: unhandled transition: file -> other
  [255]

  $ sl up -C .
  1 files updated, 0 files merged, 0 files removed, 0 files unresolved

Ignore win32text deprecation warning for now:

  $ echo '[win32text]' >> .sl/config
  $ echo 'warn = no' >> .sl/config

  $ echo d >> subdir/f1
  $ sl commit -i -d '24 0' -mw1 <<EOF
  > y
  > y
  > EOF
  diff --git a/subdir/f1 b/subdir/f1
  1 hunks, 1 lines changed
  examine changes to 'subdir/f1'? [Ynesfdaq?] y
  
  @@ -3,3 +3,4 @@ a
   a
   b
   c
  +d
  record this change to 'subdir/f1'? [Ynesfdaq?] y
  

  $ sl status -A subdir/f1
  C subdir/f1
  $ sl tip -p | sed_hashes
  commit:      aaaaaaaaaaaa
  user:        test
  date:        Thu Jan 01 00:00:24 1970 +0000
  summary:     w1
  
  diff -r ???????????? -r ???????????? subdir/f1 (glob)
  --- a/subdir/f1	Thu Jan 01 00:00:23 1970 +0000
  +++ b/subdir/f1	Thu Jan 01 00:00:24 1970 +0000
  @@ -3,3 +3,4 @@
   a
   b
   c
  +d


Test --user when ui.username not set
  $ unset HGUSER
  $ echo e >> subdir/f1
  $ sl commit -i  --config ui.username= -d '8 0' --user xyz -m "user flag" <<EOF
  > y
  > y
  > EOF
  diff --git a/subdir/f1 b/subdir/f1
  1 hunks, 1 lines changed
  examine changes to 'subdir/f1'? [Ynesfdaq?] y
  
  @@ -4,3 +4,4 @@ a
   b
   c
   d
  +e
  record this change to 'subdir/f1'? [Ynesfdaq?] y
  
  $ sl status -A subdir/f1
  C subdir/f1
  $ sl log --template '{author}\n' -l 1
  xyz
  $ HGUSER="test"
  $ export HGUSER


Moving files

  $ sl goto -C .
  0 files updated, 0 files merged, 0 files removed, 0 files unresolved
  $ sl mv plain plain3
  $ echo somechange >> plain3
  $ sl commit -i -d '23 0' -mmoving_files << EOF
  > y
  > y
  > EOF
  diff --git a/plain b/plain3
  rename from plain
  rename to plain3
  1 hunks, 1 lines changed
  examine changes to 'plain' and 'plain3'? [Ynesfdaq?] y
  
  @@ -11,3 +11,4 @@ 8
   9
   10
   11
  +somechange
  record this change to 'plain3'? [Ynesfdaq?] y
  
The #if execbit block above changes the hash here on some systems
  $ sl status -A plain3
  C plain3
  $ sl tip | sed_hashes
  commit:      aaaaaaaaaaaa
  user:        test
  date:        Thu Jan 01 00:00:23 1970 +0000
  summary:     moving_files
Editing patch of newly added file

  $ sl goto -C .
  0 files updated, 0 files merged, 0 files removed, 0 files unresolved
  $ cat > editor.sh << '__EOF__'
  > cat "$1"  | sed "s/first/very/g"  > tt
  > mv tt  "$1"
  > __EOF__
  $ cat > newfile << '__EOF__'
  > This is the first line
  > This is the second line
  > This is the third line
  > __EOF__
  $ sl add newfile
  $ HGEDITOR="\"sh\" \"`pwd`/editor.sh\"" sl commit -i -d '23 0' -medit-patch-new <<EOF
  > y
  > e
  > EOF
  diff --git a/newfile b/newfile
  new file mode 100644
  examine changes to 'newfile'? [Ynesfdaq?] y
  
  @@ -0,0 +1,3 @@
  +This is the first line
  +This is the second line
  +This is the third line
  record this change to 'newfile'? [Ynesfdaq?] e
  
  $ sl cat -r tip newfile
  This is the very line
  This is the second line
  This is the third line

  $ cat newfile
  This is the first line
  This is the second line
  This is the third line

Add new file from within a subdirectory
  $ sl goto -C .
  1 files updated, 0 files merged, 0 files removed, 0 files unresolved
  $ mkdir folder
  $ cd folder
  $ echo "foo" > bar
  $ sl add bar
  $ sl commit -i -d '23 0' -mnewfilesubdir  <<EOF
  > y
  > y
  > EOF
  diff --git a/folder/bar b/folder/bar
  new file mode 100644
  examine changes to 'folder/bar'? [Ynesfdaq?] y
  
  @@ -0,0 +1,1 @@
  +foo
  record this change to 'folder/bar'? [Ynesfdaq?] y
  
The #if execbit block above changes the hashes here on some systems
  $ sl tip -p | sed_hashes
  commit:      aaaaaaaaaaaa
  user:        test
  date:        Thu Jan 01 00:00:23 1970 +0000
  summary:     newfilesubdir
  
  diff -r * -r * folder/bar (glob)
  --- /dev/null	Thu Jan 01 00:00:00 1970 +0000
  +++ b/folder/bar	Thu Jan 01 00:00:23 1970 +0000
  @@ -0,0 +1,1 @@
  +foo
  $ cd ..

  $ sl status -A folder/bar
  C folder/bar

Clear win32text configuration before size/timestamp sensitive test

  $ cat >> .sl/config <<EOF
  > [extensions]
  > win32text = !
  > [patch]
  > eol = strict
  > EOF
  $ sl goto -q -C null
  $ sl goto -q -C tip

Test that partially committed file is still treated as "modified",
even if none of mode, size and timestamp is changed on the filesystem
(see also issue4583).

  $ cat > subdir/f1 <<EOF
  > A
  > a
  > a
  > b
  > c
  > d
  > E
  > EOF
  $ sl diff --git subdir/f1
  diff --git a/subdir/f1 b/subdir/f1
  --- a/subdir/f1
  +++ b/subdir/f1
  @@ -1,7 +1,7 @@
  -a
  +A
   a
   a
   b
   c
   d
  -e
  +E

  $ touch -t 200001010000 subdir/f1

  $ cat >> .sl/config <<EOF
  > # emulate invoking patch.internalpatch() at 2000-01-01 00:00
  > [fakepatchtime]
  > fakenow = 2000-01-01 00:00:0
  > 
  > [extensions]
  > fakepatchtime = $TESTDIR/fakepatchtime.py
  > EOF
  $ sl commit -i -m 'commit subdir/f1 partially' <<EOF
  > y
  > y
  > n
  > EOF
  diff --git a/subdir/f1 b/subdir/f1
  2 hunks, 2 lines changed
  examine changes to 'subdir/f1'? [Ynesfdaq?] y
  
  @@ -1,6 +1,6 @@
  -a
  +A
   a
   a
   b
   c
   d
  record change 1/2 to 'subdir/f1'? [Ynesfdaq?] y
  
  @@ -2,6 +2,6 @@
   a
   a
   b
   c
   d
  -e
  +E
  record change 2/2 to 'subdir/f1'? [Ynesfdaq?] n
  
  $ cat >> .sl/config <<EOF
  > [extensions]
  > fakepatchtime = !
  > EOF

  $ sl debugstate | grep ' subdir/f1$'
  n   0         -1 unset               subdir/f1
  $ sl status -A subdir/f1
  M subdir/f1

Test that filemode changes work on interactive
  $ sl goto -C . -q
  $ chmod +x subdir/f1
  $ sl commit -i -d '24 0' -m bleh <<EOF
  > y
  > EOF
  diff --git a/subdir/f1 b/subdir/f1
  old mode 100644
  new mode 100755
  examine changes to 'subdir/f1'? [Ynesfdaq?] y
  $ sl st -m
  $ sl diff -r .^ --git
  diff --git a/subdir/f1 b/subdir/f1
  old mode 100644
  new mode 100755

Hunks with no context lines between them (diff.unified=0) are offered
separately.

  $ newclientrepo zerocontext
  $ printf 'a\nkeep\nb\n' > f
  $ sl commit -qAm base
  $ printf 'A\nkeep\nB\n' > f
  $ sl commit -i -m second --config diff.unified=0 --config ui.interface=text <<EOF
  > y
  > n
  > y
  > EOF
  diff --git a/f b/f
  2 hunks, 2 lines changed
  examine changes to 'f'? [Ynesfdaq?] y
  
  @@ -1,1 +1,1 @@
  -a
  +A
  record change 1/2 to 'f'? [Ynesfdaq?] n
  
  @@ -3,1 +3,1 @@ keep
  -b
  +B
  record change 2/2 to 'f'? [Ynesfdaq?] y
  $ sl cat -r . f
  a
  keep
  B
