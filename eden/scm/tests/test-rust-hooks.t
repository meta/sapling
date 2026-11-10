
#require no-eden


Note that Rust hook execution escapes the debugruntest execution environment.
For now that is desirable since I also want to test the platform specific Rust spawn logic.


  $ sl debugtestcommand

Make sure alias marked "legacy:" works.
  $ sl debugoldtestcommand

Test hooks outside of repo:

#if windows
  $ setconfig 'hooks.pre-debugtestcommand.args=echo PRE_ARGS: %HG_ARGS%'
  $ setconfig 'hooks.pre-debugtestcommand.pwd=cd'
  $ setconfig 'hooks.pre-debugtestcommand.background=background:echo background & type nul > touched'
  $ setconfig 'hooks.fail-debugtestcommand=echo FAIL'
  $ sl debugtestcommand "hello ' there" foo
  PRE_ARGS: debugtestcommand "hello ' there" foo\r (esc)
  $TESTTMP\r (esc)
#else
  $ setconfig 'hooks.pre-debugtestcommand=echo PRE_ARGS: $HG_ARGS && echo SL: $HG && echo PWD: $PWD'
  $ setconfig 'hooks.pre-debugtestcommand.background=background:echo background > touched'
  $ setconfig 'hooks.post-debugtestcommand=echo POST_ARGS: $HG_ARGS && echo SL: $HG && echo RESULT: $HG_RESULT && echo PWD: $PWD'
  $ setconfig 'hooks.fail-debugtestcommand=echo FAIL'
  $ sl debugtestcommand "hello ' there" foo
  PRE_ARGS: debugtestcommand 'hello '\'' there' foo
  SL: *hg* (glob)
  PWD: $TESTTMP
  POST_ARGS: debugtestcommand 'hello '\'' there' foo
  SL: *hg* (glob)
  RESULT: 0
  PWD: $TESTTMP
#endif

Wait for background hook to touch file.
  $ sleep 1
  $ ls touched
  touched

  $ newclientrepo
  $ mkdir subdir
  $ cd subdir

When in repo, hooks run from repo root
#if windows
  $ sl debugtestcommand
  PRE_ARGS: debugtestcommand\r (esc)
  $TESTTMP\repo1\r (esc)
#else
  $ sl debugtestcommand
  PRE_ARGS: debugtestcommand
  SL: *hg* (glob)
  PWD: $TESTTMP/repo1
  POST_ARGS: debugtestcommand
  SL: *hg* (glob)
  RESULT: 0
  PWD: $TESTTMP/repo1
#endif

  $ cd ..

Wait for background hook to touch file.
  $ sleep 1
  $ sl st
  ? touched

Reset our hook config:
  $ sed -i "/\[hooks\]/q" $HGRCPATH

Test fail hooks:
  $ setconfig 'hooks.pre-debugtestcommand=echo PRE'
  $ setconfig 'hooks.post-debugtestcommand=echo POST'
  $ setconfig 'hooks.fail-debugtestcommand=echo FAIL'

#if windows
  $ sl debugtestcommand --abort
  PRE\r (esc)
  FAIL\r (esc)
  abort: aborting
  [255]
#else
  $ sl debugtestcommand --abort
  PRE
  FAIL
  abort: aborting
  [255]
#endif

Test hooks work for legacy command name:
  $ setconfig 'hooks.pre-debugoldtestcommand=echo LEGACY'
Both fire for new command name:
#if windows
  $ sl debugtestcommand
  PRE\r (esc)
  LEGACY\r (esc)
  POST\r (esc)
#else
  $ sl debugtestcommand
  PRE
  LEGACY
  POST
#endif

Both fire for old command name:
#if windows
  $ sl debugoldtestcommand
  PRE\r (esc)
  LEGACY\r (esc)
  POST\r (esc)
#else
  $ sl debugoldtestcommand
  PRE
  LEGACY
  POST
#endif

Python hooks now work in Rust commands:
  $ newclientrepo
  $ setconfig 'hooks.pre-debugtestcommand.python=python:foo.py:main'
  $ cat > foo.py << 'EOF'
  > def main(io, repo, **kwargs):
  >     io.write_err(('repo is %s. kwargs: %r\n' % (repo.path, kwargs)).encode())
  > EOF
(hook can be defined in base64, too)
  $ setconfig "hooks.post-debugtestcommand.python=python:base64:$(base64 -w0 foo.py):main"
  $ sl debugtestcommand
  repo is $TESTTMP/repo2. kwargs: {'args': ['debugtestcommand']}
  repo is $TESTTMP/repo2. kwargs: {'args': ['debugtestcommand'], 'result': 0}

Python hooks in one command share one interpreter. Run `sl` as its own process,
because the test runner's Python, like the always-on profiler, would keep the
interpreter alive by itself:
  $ newclientrepo hookcount
  $ cat > count.py << 'EOF'
  > import sys
  > def main(io, repo, **kwargs):
  >     sys.hook_calls = getattr(sys, "hook_calls", 0) + 1
  >     io.write_err(b"hook call %d\n" % sys.hook_calls)
  > EOF
  $ setconfig profiling.always-on-enabled=false hooks.pre-debugtestcommand.first=python:count.py:main hooks.pre-debugtestcommand.second=python:count.py:main hooks.post-debugtestcommand.python=python:count.py:main
  >>> import os, subprocess
  >>> repo = os.path.join(os.environ["TESTTMP"], "hookcount")
  >>> proc = subprocess.run([os.environ["HGEXECUTABLEPATH"], "debugtestcommand"], cwd=repo, capture_output=True)
  >>> print(proc.stderr.decode(), end="")
  hook call 1
  hook call 2
  hook call 3

A hook that runs other hooks, here checkout hooks through `repo.goto()`, shares
the interpreter too. (`repo.goto()` deadlocks unless the working copy is loaded
first.)
  $ newclientrepo nestedhooks
  $ echo a > f
  $ sl commit -qAm A
  $ sl log -r . -T '{node}' > $TESTTMP/target
  $ echo b > f
  $ sl commit -qm B
  $ cp $TESTTMP/hookcount/count.py .
  $ cat > goto.py << 'EOF'
  > import os, sys, bindings
  > def main(io, repo, **kwargs):
  >     sys.hook_calls = getattr(sys, "hook_calls", 0) + 1
  >     io.write_err(b"hook call %d\n" % sys.hook_calls)
  >     repo.workingcopy()
  >     ctx = bindings.context.context().withconfig(repo.config)
  >     target = bytes.fromhex(open(os.path.join(os.environ["TESTTMP"], "target")).read())
  >     repo.goto(ctx=ctx, target=target, bookmark={"action": "none"}, mode="abort_if_conflicts", report_mode="quiet")
  > EOF
  $ setconfig profiling.always-on-enabled=false hooks.pre-debugtestcommand.goto=python:goto.py:main hooks.preupdate.count=python:count.py:main hooks.update.count=python:count.py:main hooks.post-debugtestcommand.first=python:count.py:main hooks.post-debugtestcommand.second=python:count.py:main
  >>> import os, subprocess
  >>> repo = os.path.join(os.environ["TESTTMP"], "nestedhooks")
  >>> proc = subprocess.run([os.environ["HGEXECUTABLEPATH"], "debugtestcommand"], cwd=repo, capture_output=True)
  >>> print(proc.stderr.decode(), end="")
  hook call 1
  hook call 2
  hook call 3
  hook call 4
  hook call 5

Python fallback gets the original arguments after Python pre-hooks:
  $ newclientrepo fallbackargv
  $ echo a > f
  $ sl commit -qAm A
  $ echo b > f
  $ sl commit -qm B
  $ cat > count.py << 'EOF'
  > import sys
  > def main(io, repo, **kwargs):
  >     sys.hook_calls = getattr(sys, "hook_calls", 0) + 1
  >     io.write_err(b"hook call %d\n" % sys.hook_calls)
  > EOF
  $ cat > argv.py << 'EOF'
  > import sys
  > def main(io, repo, **kwargs):
  >     io.write_err(("argv: %r\n" % sys.argv[1:]).encode())
  >     assert sys.hook_calls == 4
  > EOF
  $ setconfig profiling.always-on-enabled=false 'alias.argv-goto=goto --inactive' hooks.pre-goto.first=python:count.py:main hooks.pre-goto.second=python:count.py:main hooks.update.argv=python:argv.py:main
  >>> repo = os.path.join(os.environ["TESTTMP"], "fallbackargv")
  >>> proc = subprocess.run([os.environ["HGEXECUTABLEPATH"], "argv-goto", ".^"], cwd=repo, capture_output=True)
  >>> assert proc.returncode == 0, proc.stderr.decode()
  >>> print(proc.stderr.decode(), end="")
  hook call 1
  hook call 2
  hook call 3
  hook call 4
  argv: ['argv-goto', '.^']

A nested Python command leaves its caller's arguments intact:
  $ newclientrepo nestedargv
  $ cat > argv.py << 'EOF'
  > import sys, bindings
  > def main(io, repo, **kwargs):
  >     saved_argv = sys.argv
  >     assert bindings.commands.run(["sl", "debuglistpythonstd", "-T", ""]) == 0
  >     assert sys.argv is saved_argv
  >     io.write_err(b"caller argv restored\n")
  > EOF
  $ setconfig profiling.always-on-enabled=false hooks.post-debugtestcommand.argv=python:argv.py:main
  >>> repo = os.path.join(os.environ["TESTTMP"], "nestedargv")
  >>> proc = subprocess.run([os.environ["HGEXECUTABLEPATH"], "debugtestcommand"], cwd=repo, capture_output=True)
  >>> assert proc.returncode == 0, proc.stderr.decode()
  >>> print(proc.stderr.decode(), end="")
  caller argv restored


"pre" hooks abort on error:
  $ newclientrepo
  $ setconfig hooks.pre-debugtestcommand=oopsie-daisy hooks.post-debugtestcommand=oopsie-daisy hooks.fail-debugtestcommand=oopsie-daisy
#if windows
  $ sl debugtestcommand --echo running
  'oopsie-daisy' is not recognized as an internal or external command,\r (esc)
  operable program or batch file.\r (esc)
  abort: pre-debugtestcommand hook exited with status 1
  [255]
#else
  $ sl debugtestcommand --echo running
  * not found (glob)
  abort: pre-debugtestcommand hook exited with status 127
  [255]
#endif

#if no-windows
Test client correlator:
  $ newclientrepo
  $ sl debugtestcommand --config 'hooks.pre-debugtestcommand=echo ENTRY POINT: $SAPLING_CLIENT_ENTRY_POINT && echo CORRELATOR: $SAPLING_CLIENT_CORRELATOR'
  ENTRY POINT: sapling
  CORRELATOR: test-correlator
#endif

Python hooks share one interpreter even when a hook runs another command with
`bindings.commands.run`:
  $ newclientrepo nestedcmd
  $ cp $TESTTMP/hookcount/count.py .
  $ cat > cmd.py << 'EOF'
  > import sys, bindings
  > def main(io, repo, **kwargs):
  >     sys.hook_calls = getattr(sys, "hook_calls", 0) + 1
  >     io.write_err(b"hook call %d\n" % sys.hook_calls)
  >     bindings.commands.run(["sl", "root"])
  >     io.write_err(b"survived the nested command\n")
  > EOF
  $ setconfig profiling.always-on-enabled=false hooks.pre-debugtestcommand.first=python:count.py:main hooks.pre-debugtestcommand.second=python:count.py:main hooks.post-debugtestcommand.python=python:cmd.py:main
  >>> import os, subprocess
  >>> repo = os.path.join(os.environ["TESTTMP"], "nestedcmd")
  >>> proc = subprocess.run([os.environ["HGEXECUTABLEPATH"], "debugtestcommand"], cwd=repo, capture_output=True)
  >>> print(proc.stderr.decode(), end="")
  hook call 1
  hook call 2
  hook call 3
  survived the nested command

A hook that runs another command with `bindings.commands.run` survives it:
  $ newclientrepo nestedcmdcrash
  $ cp $TESTTMP/nestedcmd/count.py $TESTTMP/nestedcmd/cmd.py .
  $ setconfig profiling.always-on-enabled=false hooks.pre-debugtestcommand.python=python:count.py:main hooks.post-debugtestcommand.python=python:cmd.py:main
  >>> repo = os.path.join(os.environ["TESTTMP"], "nestedcmdcrash")
  >>> proc = subprocess.run([os.environ["HGEXECUTABLEPATH"], "debugtestcommand"], cwd=repo, capture_output=True)
  >>> lines = [l for l in proc.stderr.decode().splitlines() if l.startswith(("hook call", "survived"))]
  >>> print("\n".join(lines + ["exit 0" if proc.returncode == 0 else "crashed"]))
  hook call 1
  hook call 2
  survived the nested command
  exit 0
