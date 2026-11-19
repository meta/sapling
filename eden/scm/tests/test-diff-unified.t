
#require no-eden



  $ sl init repo
  $ cd repo
  $ cat > a <<EOF
  > c
  > c
  > a
  > a
  > b
  > a
  > a
  > c
  > c
  > EOF
  $ sl ci -Am adda
  adding a

  $ cat > a <<EOF
  > c
  > c
  > a
  > a
  > dd
  > a
  > a
  > c
  > c
  > EOF

default context

  $ sl diff --nodates
  diff -r cf9f4ba66af2 a
  --- a/a
  +++ b/a
  @@ -2,7 +2,7 @@
   c
   a
   a
  -b
  +dd
   a
   a
   c

invalid --unified

  $ sl diff --nodates -U foo
  abort: diff context lines count must be an integer, not 'foo'
  [255]


  $ sl diff --nodates -U 2
  diff -r cf9f4ba66af2 a
  --- a/a
  +++ b/a
  @@ -3,5 +3,5 @@
   a
   a
  -b
  +dd
   a
   a

  $ sl --config diff.unified=2 diff --nodates
  diff -r cf9f4ba66af2 a
  --- a/a
  +++ b/a
  @@ -3,5 +3,5 @@
   a
   a
  -b
  +dd
   a
   a

  $ sl diff --nodates -U 1
  diff -r cf9f4ba66af2 a
  --- a/a
  +++ b/a
  @@ -4,3 +4,3 @@
   a
  -b
  +dd
   a

invalid diff.unified

  $ sl --config diff.unified=foo diff --nodates
  abort: diff context lines count must be an integer, not 'foo'
  [255]

diff.unified or -U of 2**63 or more shows the whole file, like 2**63-1

  $ sl --config diff.unified=9223372036854775807 diff --nodates
  diff -r cf9f4ba66af2 a
  --- a/a
  +++ b/a
  @@ -1,9 +1,9 @@
   c
   c
   a
   a
  -b
  +dd
   a
   a
   c
   c
  $ sl --config diff.unified=9223372036854775808 diff --nodates
  diff -r cf9f4ba66af2 a
  --- a/a
  +++ b/a
  @@ -1,9 +1,9 @@
   c
   c
   a
   a
  -b
  +dd
   a
   a
   c
   c
  $ sl diff --nodates -U 18446744073709551616
  diff -r cf9f4ba66af2 a
  --- a/a
  +++ b/a
  @@ -1,9 +1,9 @@
   c
   c
   a
   a
  -b
  +dd
   a
   a
   c
   c

noprefix config and option

  $ sl --config diff.noprefix=True diff --nodates
  diff -r cf9f4ba66af2 a
  --- a
  +++ a
  @@ -2,7 +2,7 @@
   c
   a
   a
  -b
  +dd
   a
   a
   c
  $ sl diff --noprefix --nodates
  diff -r cf9f4ba66af2 a
  --- a
  +++ a
  @@ -2,7 +2,7 @@
   c
   a
   a
  -b
  +dd
   a
   a
   c

noprefix config disabled in plain mode, but option still enabled

  $ HGPLAIN=1 sl --config diff.noprefix=True diff --nodates
  diff -r cf9f4ba66af2 a
  --- a/a
  +++ b/a
  @@ -2,7 +2,7 @@
   c
   a
   a
  -b
  +dd
   a
   a
   c
  $ HGPLAIN=1 sl diff --noprefix --nodates
  diff -r cf9f4ba66af2 a
  --- a
  +++ a
  @@ -2,7 +2,7 @@
   c
   a
   a
  -b
  +dd
   a
   a
   c

  $ cd ..


0 lines of context hunk header matches gnu diff hunk header

  $ sl init diffzero
  $ cd diffzero
  $ cat > f1 << EOF
  > c2
  > c4
  > c5
  > EOF
  $ sl commit -Am0
  adding f1

  $ cat > f2 << EOF
  > c1
  > c2
  > c3
  > c4
  > EOF
  $ mv f2 f1
  $ sl diff -U0 --nodates
  diff -r 55d8ff78db23 f1
  --- a/f1
  +++ b/f1
  @@ -0,0 +1,1 @@
  +c1
  @@ -1,0 +3,1 @@
  +c3
  @@ -3,1 +4,0 @@
  -c5

  $ sl diff -U0 --nodates --git
  diff --git a/f1 b/f1
  --- a/f1
  +++ b/f1
  @@ -0,0 +1,1 @@
  +c1
  @@ -1,0 +3,1 @@
  +c3
  @@ -3,1 +4,0 @@
  -c5

  $ sl diff -U0 --nodates -p
  diff -r 55d8ff78db23 f1
  --- a/f1
  +++ b/f1
  @@ -0,0 +1,1 @@
  +c1
  @@ -1,0 +3,1 @@ c2
  +c3
  @@ -3,1 +4,0 @@ c4
  -c5

  $ echo a > f1
  $ sl ci -m movef2

Test diff headers terminating with TAB when necessary (issue3357)
Regular diff --nodates, file creation

  $ sl mv f1 'f 1'
  $ echo b > 'f 1'
  $ sl diff --nodates 'f 1'
  diff -r 7574207d0d15 f 1
  --- /dev/null
  +++ b/f 1	
  @@ -0,0 +1,1 @@
  +b

Git diff, adding space

  $ sl diff --git
  diff --git a/f1 b/f 1
  rename from f1
  rename to f 1
  --- a/f1
  +++ b/f 1	
  @@ -1,1 +1,1 @@
  -a
  +b

Git diff, adding extended headers

  $ sl diff --git --config experimental.extendedheader.index=7 --config experimental.extendedheader.similarity=True
  diff --git a/f1 b/f 1
  similarity index 0%
  rename from f1
  rename to f 1
  index 7898192..6178079 100644
  --- a/f1
  +++ b/f 1	
  @@ -1,1 +1,1 @@
  -a
  +b

  $ sl diff --git --config experimental.extendedheader.index=-1
  invalid length for extendedheader.index: '-1'
  diff --git a/f1 b/f 1
  rename from f1
  rename to f 1
  --- a/f1
  +++ b/f 1	
  @@ -1,1 +1,1 @@
  -a
  +b

  $ sl diff --git --config experimental.extendedheader.index=whatever
  invalid value for extendedheader.index: 'whatever'
  diff --git a/f1 b/f 1
  rename from f1
  rename to f 1
  --- a/f1
  +++ b/f 1	
  @@ -1,1 +1,1 @@
  -a
  +b

Git diff with noprefix

  $ sl --config diff.noprefix=True diff --git --nodates
  diff --git f1 f 1
  rename from f1
  rename to f 1
  --- f1
  +++ f 1	
  @@ -1,1 +1,1 @@
  -a
  +b

noprefix config disabled in plain mode, but option still enabled

  $ HGPLAIN=1 sl --config diff.noprefix=True diff --git --nodates
  diff --git a/f1 b/f 1
  rename from f1
  rename to f 1
  --- a/f1
  +++ b/f 1	
  @@ -1,1 +1,1 @@
  -a
  +b
  $ HGPLAIN=1 sl diff --git --noprefix --nodates
  diff --git f1 f 1
  rename from f1
  rename to f 1
  --- f1
  +++ f 1	
  @@ -1,1 +1,1 @@
  -a
  +b

Regular diff --nodates, file deletion

  $ sl ci -m addspace
  $ sl mv 'f 1' f1
  $ echo a > f1
  $ sl diff --nodates 'f 1'
  diff -r ca50fe67c9c7 f 1
  --- a/f 1	
  +++ /dev/null
  @@ -1,1 +0,0 @@
  -b

Git diff, removing space

  $ sl diff --git
  diff --git a/f 1 b/f1
  rename from f 1
  rename to f1
  --- a/f 1	
  +++ b/f1
  @@ -1,1 +1,1 @@
  -b
  +a

showfunc diff
  $ cat > f1 << EOF
  > int main() {
  >     int a = 0;
  >     int b = 1;
  >     int c = 2;
  >     int d = 3;
  >     return a + b + c + d;
  > }
  > EOF
  $ sl commit -m addfunction
  $ cat > f1 << EOF
  > int main() {
  >     int a = 0;
  >     int b = 1;
  >     int c = 2;
  >     int e = 3;
  >     return a + b + c + e;
  > }
  > EOF
  $ sl diff --git
  diff --git a/f1 b/f1
  --- a/f1
  +++ b/f1
  @@ -2,6 +2,6 @@
       int a = 0;
       int b = 1;
       int c = 2;
  -    int d = 3;
  -    return a + b + c + d;
  +    int e = 3;
  +    return a + b + c + e;
   }
  $ sl diff --config diff.showfunc=True --git
  diff --git a/f1 b/f1
  --- a/f1
  +++ b/f1
  @@ -2,6 +2,6 @@ int main() {
       int a = 0;
       int b = 1;
       int c = 2;
  -    int d = 3;
  -    return a + b + c + d;
  +    int e = 3;
  +    return a + b + c + e;
   }

If [diff] git is set to true, but the user says --no-git, we should
*not* get git diffs
  $ sl diff --nodates --config diff.git=1 --no-git
  diff -r f2c7c817fa55 f1
  --- a/f1
  +++ b/f1
  @@ -2,6 +2,6 @@
       int a = 0;
       int b = 1;
       int c = 2;
  -    int d = 3;
  -    return a + b + c + d;
  +    int e = 3;
  +    return a + b + c + e;
   }

  $ cd ..
