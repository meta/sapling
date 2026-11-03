
#require no-eden



We assume that log basically works (it has its own tests). This just covers uses
of show that might break even if log works.

Show on empty repository: checking consistency

  $ sl init empty
  $ cd empty
  $ sl show
  commit:      000000000000
  user:        
  date:        Thu Jan 01 00:00:00 1970 +0000
  
  

Add log alias to and make sure show still works
  $ sl show --config alias.log=log
  commit:      000000000000
  user:        
  date:        Thu Jan 01 00:00:00 1970 +0000
  
  

  $ sl show 1
  abort: unknown revision '1'!
  [255]
  $ sl show 'branch(name)'
  abort: unknown revision branch(name)
  (if branch(name) is a file, try `sl show . branch(name)`)
  [255]
  $ sl show null -q
  commit:      000000000000
  user:        
  date:        Thu Jan 01 00:00:00 1970 +0000
  
  
Check various git-like options:

  $ sl init gitlike
  $ echo one > one
  $ echo two > two
  $ sl commit -qAm twofiles
  $ sl show --template status
  commit:      bf7b98b60f6f
  user:        test
  date:        Thu Jan 01 00:00:00 1970 +0000
  description:
  twofiles
  
  files:
  A one
  A two
  
  diff -r 000000000000 -r bf7b98b60f6f one
  --- /dev/null	Thu Jan 01 00:00:00 1970 +0000
  +++ b/one	Thu Jan 01 00:00:00 1970 +0000
  @@ -0,0 +1,1 @@
  +one
  diff -r 000000000000 -r bf7b98b60f6f two
  --- /dev/null	Thu Jan 01 00:00:00 1970 +0000
  +++ b/two	Thu Jan 01 00:00:00 1970 +0000
  @@ -0,0 +1,1 @@
  +two
  

Check that the command parser always treats the first argument as a revision:

  $ sl show two
  abort: unknown revision 'two'!
  [255]
  $ sl show . two
  commit:      bf7b98b60f6f
  user:        test
  date:        Thu Jan 01 00:00:00 1970 +0000
  files:       one two
  description:
  twofiles
  
  
  diff -r 000000000000 -r bf7b98b60f6f two
  --- /dev/null	Thu Jan 01 00:00:00 1970 +0000
  +++ b/two	Thu Jan 01 00:00:00 1970 +0000
  @@ -0,0 +1,1 @@
  +two
  

Check --stat

  $ sl init stat
  $ cd stat
  $ echo show > x
  $ sl commit -qAm x
  $ sl show --stat
  commit:      852a8d467a01
  user:        test
  date:        Thu Jan 01 00:00:00 1970 +0000
  files:       x
  description:
  x
  
  
   x |  1 +
   1 files changed, 1 insertions(+), 0 deletions(-)
  




  $ echo more >> x
  $ sl commit -qAm longer
  $ sl show --stat
  commit:      b73358b94785
  user:        test
  date:        Thu Jan 01 00:00:00 1970 +0000
  files:       x
  description:
  longer
  
  
   x |  1 +
   1 files changed, 1 insertions(+), 0 deletions(-)
  




  $ echo remove > x
  $ sl commit -qAm remove
  $ sl show --stat
  commit:      3d74ea61c11c
  user:        test
  date:        Thu Jan 01 00:00:00 1970 +0000
  files:       x
  description:
  remove
  
  
   x |  3 +--
   1 files changed, 1 insertions(+), 2 deletions(-)
  



  $ sl show --stat 'desc(x)'
  commit:      852a8d467a01
  user:        test
  date:        Thu Jan 01 00:00:00 1970 +0000
  files:       x
  description:
  x
  
  
   x |  1 +
   1 files changed, 1 insertions(+), 0 deletions(-)
  
Check --unified and -U

  $ sl init diff
  $ cd diff
  $ cat >file <<EOF
  > line1
  > line2
  > line3
  > line4
  > line5
  > EOF
  $ sl commit -qAm file
  $ cat >>file <<EOF
  > line6
  > line7
  > line8
  > line9
  > line10
  > EOF
  $ sl commit -qm file
  $ sl show --unified=1
  commit:      8e33115c1596
  user:        test
  date:        Thu Jan 01 00:00:00 1970 +0000
  files:       file
  description:
  file
  
  
  diff -r fd78c1ae39e0 -r 8e33115c1596 file
  --- a/file	Thu Jan 01 00:00:00 1970 +0000
  +++ b/file	Thu Jan 01 00:00:00 1970 +0000
  @@ -5,1 +5,6 @@
   line5
  +line6
  +line7
  +line8
  +line9
  +line10
  
  $ sl show --unified=2
  commit:      8e33115c1596
  user:        test
  date:        Thu Jan 01 00:00:00 1970 +0000
  files:       file
  description:
  file
  
  
  diff -r fd78c1ae39e0 -r 8e33115c1596 file
  --- a/file	Thu Jan 01 00:00:00 1970 +0000
  +++ b/file	Thu Jan 01 00:00:00 1970 +0000
  @@ -4,2 +4,7 @@
   line4
   line5
  +line6
  +line7
  +line8
  +line9
  +line10
  

Check behavior with nonsensical integers.
  $ sl show --unified=-7
  commit:      8e33115c1596
  user:        test
  date:        Thu Jan 01 00:00:00 1970 +0000
  files:       file
  description:
  file
  
  
  diff -r fd78c1ae39e0 -r 8e33115c1596 file
  --- a/file	Thu Jan 01 00:00:00 1970 +0000
  +++ b/file	Thu Jan 01 00:00:00 1970 +0000
  @@ -3,3 +3,8 @@
   line3
   line4
   line5
  +line6
  +line7
  +line8
  +line9
  +line10
  



Check whitespace handling options
  $ sl init whitespace
  $ cd whitespace
  $ echo "some  text" > file
  $ sl commit -qAm file
  $ echo "some text " > file
  $ sl commit -qAm file
  $ sl show
  commit:      6dbf2c12e2e2
  user:        test
  date:        Thu Jan 01 00:00:00 1970 +0000
  files:       file
  description:
  file
  
  
  diff -r 5b445d2a372e -r 6dbf2c12e2e2 file
  --- a/file	Thu Jan 01 00:00:00 1970 +0000
  +++ b/file	Thu Jan 01 00:00:00 1970 +0000
  @@ -1,1 +1,1 @@
  -some  text
  +some text 
  
  $ sl show -b
  commit:      6dbf2c12e2e2
  user:        test
  date:        Thu Jan 01 00:00:00 1970 +0000
  files:       file
  description:
  file
  
  
  
  $ echo "some text" > file
  $ sl commit -qAm file
  $ sl show -Z
  commit:      600038806867
  user:        test
  date:        Thu Jan 01 00:00:00 1970 +0000
  files:       file
  description:
  file
  
  
  
  $ echo "some text " > file
  $ sl commit -qAm file
  $ sl show -Z
  commit:      747594f0817c
  user:        test
  date:        Thu Jan 01 00:00:00 1970 +0000
  files:       file
  description:
  file
  
  
  

  $ printf "some\n\ntext" > file
  $ sl commit -qAm file
  $ printf "some\ntext" > file
  $ sl commit -qAm file
  $ sl show -B
  commit:      10f3fc1d00d6
  user:        test
  date:        Thu Jan 01 00:00:00 1970 +0000
  files:       file
  description:
  file
  
  
  

Check --git and -g

  $ sl init git
  $ cd git
  $ echo git > file
  $ sl commit -qAm file
  $ sl show --git
  commit:      2a575d662478
  user:        test
  date:        Thu Jan 01 00:00:00 1970 +0000
  files:       file
  description:
  file
  
  
  diff --git a/file b/file
  new file mode 100644
  --- /dev/null
  +++ b/file
  @@ -0,0 +1,1 @@
  +git
  


  $ echo more >> file
  $ sl commit -qAm file
  $ sl show -g
  commit:      a23f7b259024
  user:        test
  date:        Thu Jan 01 00:00:00 1970 +0000
  files:       file
  description:
  file
  
  
  diff --git a/file b/file
  --- a/file
  +++ b/file
  @@ -1,1 +1,2 @@
   git
  +more
  


  $ sl show -g 2a575d662478590c06bc0cb3988882b46c0b2fee
  commit:      2a575d662478
  user:        test
  date:        Thu Jan 01 00:00:00 1970 +0000
  files:       file
  description:
  file
  
  
  diff --git a/file b/file
  new file mode 100644
  --- /dev/null
  +++ b/file
  @@ -0,0 +1,1 @@
  +git
  


Check nodates
  $ sl show --nodates
  commit:      a23f7b259024
  user:        test
  date:        Thu Jan 01 00:00:00 1970 +0000
  files:       file
  description:
  file
  
  
  diff -r 2a575d662478 -r a23f7b259024 file
  --- a/file
  +++ b/file
  @@ -1,1 +1,2 @@
   git
  +more
  

Check noprefix
  $ sl show --noprefix
  commit:      a23f7b259024
  user:        test
  date:        Thu Jan 01 00:00:00 1970 +0000
  files:       file
  description:
  file
  
  
  diff -r 2a575d662478 -r a23f7b259024 file
  --- file	Thu Jan 01 00:00:00 1970 +0000
  +++ file	Thu Jan 01 00:00:00 1970 +0000
  @@ -1,1 +1,2 @@
   git
  +more
  

Check sl show '' fails to parse the revision

  $ sl show ''
  sl: parse error: empty query
  [255]

Check --mutation compares the commit with its mutation predecessor

  $ sl init mutation
  $ cd mutation
  $ setconfig diff.git=true
  $ echo before > file
  $ sl commit -qAm original
  $ echo after >> file
  $ sl amend -qm amended
  $ sl show --mutation -T '{desc}\n'
  amended
  diff --git a/file b/file
  --- a/file
  +++ b/file
  @@ -1,1 +1,2 @@
   before
  +after

Confirm that --help works (it didn't when we used an alias)

  $ sl show --help
  sl show [OPTION]... [-r REV | REV] [FILE]...
  
  show commit in detail
  
      Show the commit message and contents for the specified commit. If no
      commit is specified, shows the current commit.
  
      The revision can be given positionally or via "-r/--rev":
  
      - "sl show REV [FILE]..." — first positional is the revision, the rest are
        files.
      - "sl show -r REV [FILE]..." — all positionals are files.
  
      A bare "sl show FILE" does not work, because "FILE" is parsed as a
      revision.
  
      'sl show' behaves similarly to 'sl log -vp -r REV [OPTION]... [FILE]...',
      or if called without a "REV", 'sl log -vp -r . [OPTION]...' Use 'sl log'
      for more powerful operations than supported by 'sl show'.
  
  Options ([+] can be repeated):
  
      --nodates             omit dates from diff headers (but keeps it in commit
                            header)
      --noprefix            omit a/ and b/ prefixes from filenames
      --stat                output diffstat-style summary of changes
   -g --git                 use git extended diff format
   -U --unified VALUE       number of lines of diff context to show (default: 3)
   -r --rev VALUE [+]       show the specified revision
   -t --mutation            diff against the mutation predecessor
   -w --ignore-all-space    ignore white space when comparing lines
   -b --ignore-space-change ignore changes in the amount of white space
   -B --ignore-blank-lines  ignore changes whose lines are all blank
   -Z --ignore-space-at-eol ignore changes in whitespace at EOL
   -T --template TEMPLATE   display with template
   -I --include PATTERN [+] include files matching the given patterns
   -X --exclude PATTERN [+] exclude files matching the given patterns
  
  (some details hidden, use --verbose to show complete help)
  $ sl show --help --verbose
  sl show [OPTION]... [-r REV | REV] [FILE]...
  
  show commit in detail
  
      Show the commit message and contents for the specified commit. If no
      commit is specified, shows the current commit.
  
      The revision can be given positionally or via "-r/--rev":
  
      - "sl show REV [FILE]..." — first positional is the revision, the rest are
        files.
      - "sl show -r REV [FILE]..." — all positionals are files.
  
      A bare "sl show FILE" does not work, because "FILE" is parsed as a
      revision.
  
      'sl show' behaves similarly to 'sl log -vp -r REV [OPTION]... [FILE]...',
      or if called without a "REV", 'sl log -vp -r . [OPTION]...' Use 'sl log'
      for more powerful operations than supported by 'sl show'.
  
  Options ([+] can be repeated):
  
      --nodates             omit dates from diff headers (but keeps it in commit
                            header)
      --noprefix            omit a/ and b/ prefixes from filenames
      --stat                output diffstat-style summary of changes
   -g --git                 use git extended diff format
   -U --unified VALUE       number of lines of diff context to show (default: 3)
   -r --rev VALUE [+]       show the specified revision
   -t --mutation            diff against the mutation predecessor
   -w --ignore-all-space    ignore white space when comparing lines
   -b --ignore-space-change ignore changes in the amount of white space
   -B --ignore-blank-lines  ignore changes whose lines are all blank
   -Z --ignore-space-at-eol ignore changes in whitespace at EOL
      --style STYLE         display using template map file (DEPRECATED)
   -T --template TEMPLATE   display with template
   -I --include PATTERN [+] include files matching the given patterns
   -X --exclude PATTERN [+] exclude files matching the given patterns
  
  Global options ([+] can be repeated):
  
   -R --repository REPO       repository root directory or name of overlay
                              bundle file
      --cwd DIR               change working directory
   -y --noninteractive        do not prompt, use the default answer for all
                              prompts
   -q --quiet                 suppress output
   -v --verbose               enable additional output
      --color TYPE            when to colorize (boolean, always, auto, never, or
                              debug)
      --config CONFIG [+]     set/override config option (use
                              'section.name=value')
      --configfile FILE [+]   enables the given config file
      --debug                 enable debugging output
      --debugger              start debugger
      --encoding ENCODE       set the charset encoding (default: utf-8)
      --encodingmode MODE     set the charset encoding mode (default: strict)
      --insecure              do not verify server certificate
      --outputencoding ENCODE set the output encoding (default: utf-8)
      --traceback             always print a traceback on exception
      --trace                 enable more detailed tracing
      --time                  time how long the command takes
      --profile               print command execution profile
      --version               output version information and exit
   -h --help                  display help and exit
      --hidden                consider hidden changesets
      --pager TYPE            when to paginate (boolean, always, auto, or never)
                              (default: auto)
      --reason VALUE [+]      why this runs, usually set by automation
                              (ADVANCED)

Test -r/--rev option:

  $ newclientrepo
  $ echo a > a
  $ sl commit -qAm "commit a"
  $ echo b > b
  $ sl commit -qAm "commit b"

Test --rev with single revision:
  $ sl show --rev . --stat -T '{desc}\n'
  commit b
   b |  1 +
   1 files changed, 1 insertions(+), 0 deletions(-)

Test -r with single revision:
  $ sl show -r . --stat -T '{desc}\n'
  commit b
   b |  1 +
   1 files changed, 1 insertions(+), 0 deletions(-)

Test --rev with multiple revisions:
  $ sl show --rev . --rev .^ --stat -T '{desc}\n'
  commit b
   b |  1 +
   1 files changed, 1 insertions(+), 0 deletions(-)
  
  commit a
   a |  1 +
   1 files changed, 1 insertions(+), 0 deletions(-)

Test -r with multiple revisions:
  $ sl show -r . -r .^ --stat -T '{desc}\n'
  commit b
   b |  1 +
   1 files changed, 1 insertions(+), 0 deletions(-)
  
  commit a
   a |  1 +
   1 files changed, 1 insertions(+), 0 deletions(-)

Test that positional argument still works:
  $ sl show . --stat -T '{desc}\n'
  commit b
   b |  1 +
   1 files changed, 1 insertions(+), 0 deletions(-)

Test -r with FILE filter:
  $ sl show -r . b --stat -T '{desc}\n'
  commit b
   b |  1 +
   1 files changed, 1 insertions(+), 0 deletions(-)

Test -r with FILE filter that does not match:
  $ sl show -r . a --stat -T '{desc}\n'

Test -r with multiple FILE filters:
  $ sl show -r .^ -r . a b --stat -T '{desc}\n'
  commit a
   a |  1 +
   1 files changed, 1 insertions(+), 0 deletions(-)
  
  commit b
   b |  1 +
   1 files changed, 1 insertions(+), 0 deletions(-)
