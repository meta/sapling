
  $ newclientrepo repo
  >>> with open("a", "wb") as f: f.write(b"a\n" * 213) and None
  $ sl add a
  $ cp a b
  $ sl add b

Wide diffstat:

  $ sl diff --stat
   a |  213 ++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++
   b |  213 ++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++++
   2 files changed, 426 insertions(+), 0 deletions(-)

diffstat width:

  $ COLUMNS=24 sl diff --config ui.interactive=true --stat
   a |  213 ++++++++++++++
   b |  213 ++++++++++++++
   2 files changed, 426 insertions(+), 0 deletions(-)

  $ sl ci -m adda

  $ cat >> a <<EOF
  > a
  > a
  > a
  > EOF

Narrow diffstat:

  $ sl diff --stat
   a |  3 +++
   1 files changed, 3 insertions(+), 0 deletions(-)

  $ sl ci -m appenda

  >>> _ = open("c", "wb").write(b"\0")
  $ touch d
  $ sl add c d

Binary diffstat:

  $ sl diff --stat
   c |  Bin 
   1 files changed, 0 insertions(+), 0 deletions(-)

Binary git diffstat:

  $ sl diff --stat --git
   c |  Bin 
   d |    0 
   2 files changed, 0 insertions(+), 0 deletions(-)

  $ sl ci -m createb

  >>> _ = open("file with spaces", "wb").write(b"\0")
  $ sl add "file with spaces"

Filename with spaces diffstat:

  $ sl diff --stat
   file with spaces |  Bin 
   1 files changed, 0 insertions(+), 0 deletions(-)

Filename with spaces git diffstat:

  $ sl diff --stat --git
   file with spaces |  Bin 
   1 files changed, 0 insertions(+), 0 deletions(-)

Filename without "a/" or "b/" (issue5759):

  $ sl diff --config 'diff.noprefix=1' -c'desc(appenda)' --stat --git
   a |  3 +++
   1 files changed, 3 insertions(+), 0 deletions(-)
  $ sl diff --config 'diff.noprefix=1' -c'desc(createb)' --stat --git
   c |  Bin 
   d |    0 
   2 files changed, 0 insertions(+), 0 deletions(-)

  $ sl log --config 'diff.noprefix=1' -r 'desc(appenda):' -p --stat --git
  commit:      3a95b07bb77f
  user:        test
  date:        Thu Jan 01 00:00:00 1970 +0000
  summary:     appenda
  
   a |  3 +++
   1 files changed, 3 insertions(+), 0 deletions(-)
  
  diff --git a a
  --- a
  +++ a
  @@ -211,3 +211,6 @@
   a
   a
   a
  +a
  +a
  +a
  
  commit:      c60a6c753773
  user:        test
  date:        Thu Jan 01 00:00:00 1970 +0000
  summary:     createb
  
   c |  Bin 
   d |    0 
   2 files changed, 0 insertions(+), 0 deletions(-)
  
  diff --git c c
  new file mode 100644
  index e69de29bb2d1d6434b8b29ae775ad8c2e48c5391..f76dd238ade08917e6712764a16a22005a50573d
  GIT binary patch
  literal 1
  Ic${MZ000310RR91
  
  diff --git d d
  new file mode 100644
  

diffstat within directories:

  $ sl rm -f 'file with spaces'

  $ mkdir dir1 dir2
  $ echo new1 > dir1/new
  $ echo new2 > dir2/new
  $ sl add dir1/new dir2/new
  $ sl diff --stat
   dir1/new |  1 +
   dir2/new |  1 +
   2 files changed, 2 insertions(+), 0 deletions(-)

  $ sl diff --stat --root dir1
   new |  1 +
   1 files changed, 1 insertions(+), 0 deletions(-)

  $ sl diff --stat --root dir1 dir2
  warning: dir2 not inside relative root dir1

  $ sl diff --stat --root dir1 -I dir1/old

  $ cd dir1
  $ sl diff --stat .
   dir1/new |  1 +
   1 files changed, 1 insertions(+), 0 deletions(-)
  $ sl diff --stat --root .
   new |  1 +
   1 files changed, 1 insertions(+), 0 deletions(-)

  $ sl diff --stat --root ../dir1 ../dir2
  warning: ../dir2 not inside relative root .

  $ sl diff --stat --root . -I old

  $ cd ..

Files with lines beginning with '--' or '++' should be properly counted in diffstat

  $ sl up -Cr tip
  0 files updated, 0 files merged, 0 files removed, 0 files unresolved
  $ rm dir1/new
  $ rm dir2/new
  $ rm "file with spaces"
  $ cat > file << EOF
  > line 1
  > line 2
  > line 3
  > EOF
  $ sl commit -Am file
  adding file

Lines added starting with '--' should count as additions
  $ cat > file << EOF
  > line 1
  > -- line 2, with dashes
  > line 3
  > EOF

  $ sl diff --root .
  diff -r be1569354b24 file
  --- a/file	Thu Jan 01 00:00:00 1970 +0000
  +++ b/file	* (glob)
  @@ -1,3 +1,3 @@
   line 1
  -line 2
  +-- line 2, with dashes
   line 3

  $ sl diff --root . --stat
   file |  2 +-
   1 files changed, 1 insertions(+), 1 deletions(-)

Lines changed starting with '--' should count as deletions
  $ sl commit -m filev2
  $ cat > file << EOF
  > line 1
  > -- line 2, with dashes, changed again
  > line 3
  > EOF

  $ sl diff --root .
  diff -r 160f7c034df6 file
  --- a/file	Thu Jan 01 00:00:00 1970 +0000
  +++ b/file	* (glob)
  @@ -1,3 +1,3 @@
   line 1
  --- line 2, with dashes
  +-- line 2, with dashes, changed again
   line 3

  $ sl diff --root . --stat
   file |  2 +-
   1 files changed, 1 insertions(+), 1 deletions(-)

Lines changed starting with '--' should count as deletions
and starting with '++' should count as additions
  $ cat > file << EOF
  > line 1
  > ++ line 2, switched dashes to plusses
  > line 3
  > EOF

  $ sl diff --root .
  diff -r 160f7c034df6 file
  --- a/file	Thu Jan 01 00:00:00 1970 +0000
  +++ b/file	* (glob)
  @@ -1,3 +1,3 @@
   line 1
  --- line 2, with dashes
  +++ line 2, switched dashes to plusses
   line 3

  $ sl diff --root . --stat
   file |  2 +-
   1 files changed, 1 insertions(+), 1 deletions(-)

The diffstat is computed without rendering the diff. It matches the counts of
the rendered diff, including for binary files, files without a trailing newline,
copies, renames, mode changes, empty files and whitespace options.

  $ newclientrepo statrepo
  >>> with open("text", "wb") as f: f.write(b"1\n2\n3\n4\n5\n") and None
  >>> with open("nonewline", "wb") as f: f.write(b"a\nb") and None
  >>> with open("binary", "wb") as f: f.write(b"\0a") and None
  >>> with open("spaces", "wb") as f: f.write(b"a b\n\nc\n") and None
  $ echo x > tocopy
  $ echo y > torename
  $ echo z > tochmod
  $ touch empty
  $ sl commit -Aqm base
  >>> with open("text", "wb") as f: f.write(b"1\n2x\n3\n5\n6\n") and None
  >>> with open("nonewline", "wb") as f: f.write(b"a\nc") and None
  >>> with open("binary", "wb") as f: f.write(b"\0b") and None
  >>> with open("spaces", "wb") as f: f.write(b"a  b\nc\n\n") and None
  $ sl cp tocopy copied
  $ sl mv torename renamed
#if execbit
  $ chmod +x tochmod
#endif
  $ sl rm empty
  $ touch newempty
  $ sl add newempty

  $ sl diff --stat
   binary    |  Bin 
   copied    |    1 +
   nonewline |    2 +-
   renamed   |    1 +
   spaces    |    4 ++--
   text      |    4 ++--
   torename  |    1 -
   7 files changed, 7 insertions(+), 6 deletions(-)
  $ sl diff --stat --git
   binary    |  Bin 
   copied    |    0 
   empty     |    0 
   newempty  |    0 
   nonewline |    2 +-
   renamed   |    0 
   spaces    |    4 ++--
   text      |    4 ++--
   tochmod   |    0  (execbit !)
   9 files changed, 5 insertions(+), 5 deletions(-) (execbit !)
   8 files changed, 5 insertions(+), 5 deletions(-) (no-execbit !)
  $ sl diff --stat --git -w -B
   binary    |  Bin 
   copied    |    0 
   empty     |    0 
   newempty  |    0 
   nonewline |    2 +-
   renamed   |    0 
   text      |    4 ++--
   tochmod   |    0  (execbit !)
   8 files changed, 3 insertions(+), 3 deletions(-) (execbit !)
   7 files changed, 3 insertions(+), 3 deletions(-) (no-execbit !)

  $ for opts in "" "--git" "--git -w" "--git -B" "--git -b" "--git -w -B" "--git --binary" "--git --config diff.index=7" "-q --config diff.git=false" "--root . text"; do
  >   sl diff --stat $opts > $TESTTMP/fast
  >   sl diff --stat $opts --config experimental.fast-diffstat=false > $TESTTMP/slow
  >   cmp $TESTTMP/fast $TESTTMP/slow || echo "differs with: $opts"
  > done
