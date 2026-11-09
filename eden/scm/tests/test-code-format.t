
#require no-eden no-windows

  $ configure modernclient

Create a fake formatter that prepends "===formatted===" to each file (idempotent).
  $ cat > "$TESTTMP/formatter.py" <<EOF
  > import sys, os
  > for line in sys.stdin:
  >     fn = line.strip()
  >     if fn and os.path.isfile(fn):
  >         data = open(fn).read()
  >         if not data.startswith("===formatted===\n"):
  >             open(fn, "w").write("===formatted===\n" + data)
  > EOF

Setup repo:
  $ newclientrepo
  $ enable absorb
  $ setconfig hooks.pre-commit.sl_code_format=python:sapling.agent.fb.code_format.main
  $ setconfig hooks.pre-amend.sl_code_format=python:sapling.agent.fb.code_format.main
  $ setconfig hooks.pre-absorb.sl_code_format=python:sapling.agent.fb.code_format.main
  $ setconfig fix.code-format-mode=on
  $ setconfig fix.code-format-command="sl debugpython $TESTTMP/formatter.py"

Test basic formatting on commit:
  $ echo "hello" > a.txt
  $ sl add a.txt
  $ sl commit -m "add a"
  running code formatter: '*' (glob)
  code formatter completed successfully in * secs (glob)
  $ cat a.txt
  ===formatted===
  hello

Formatter is idempotent - already formatted files are not modified:
  $ echo "more" >> a.txt
  $ sl amend
  running code formatter: '*' (glob)
  code formatter completed successfully in * secs (glob)
  $ cat a.txt
  ===formatted===
  hello
  more

Test formatting on amend with new file:
  $ echo "world" > b.txt
  $ sl add b.txt
  $ sl amend
  running code formatter: '*' (glob)
  code formatter completed successfully in * secs (glob)
  $ cat b.txt
  ===formatted===
  world

Test formatting on absorb (modifications get absorbed into owning commit):
  $ echo "extra" >> b.txt
  $ sl absorb -aq
  running code formatter: '*' (glob)
  code formatter completed successfully in * secs (glob)
  $ cat b.txt
  ===formatted===
  world
  extra

Test disabled via config:
  $ setconfig 'fix.code-format-mode=off'
  $ echo "not formatted" > c.txt
  $ sl add c.txt
  $ sl amend
  $ cat c.txt
  not formatted
Re-enable for subsequent tests:
  $ setconfig 'fix.code-format-mode=on'

Test skipped when not agent and code-format-mode is agent:
  $ setconfig 'fix.code-format-mode=agent'
  $ echo "not formatted either" > d.txt
  $ sl add d.txt
  $ sl amend
  $ cat d.txt
  not formatted either
Re-enable for subsequent tests:
  $ setconfig 'fix.code-format-mode=on'

Test max-files limit skips formatting:
  $ setconfig 'fix.code-format-max-files=1'
  $ echo "x" > e.txt
  $ echo "y" > f.txt
  $ sl add e.txt f.txt
  $ sl amend
  $ cat e.txt
  x
  $ setconfig 'fix.code-format-max-files=200'

Test no modified/added files skips formatting:
  $ sl amend
  nothing changed
  [1]

Test max-file-size limit skips formatting:
  $ setconfig 'fix.code-format-max-file-size=10'
  $ echo "this is a long line that exceeds the size limit" > g.txt
  $ sl add g.txt
  $ sl amend
  code formatter: leaving out files over 10 bytes: g.txt
  $ cat g.txt
  this is a long line that exceeds the size limit

Test max-file-size limit leaves out only the large files, in one line:
  $ echo "this is another line that exceeds the size limit" > big.txt
  $ echo "this is a third line that exceeds the size limit" > big2.txt
  $ echo "small" > k.txt
  $ sl add big.txt big2.txt k.txt
  $ sl amend
  code formatter: leaving out files over 10 bytes: big.txt, big2.txt
  running code formatter: '*' (glob)
  code formatter completed successfully in * secs (glob)
  $ cat big.txt
  this is another line that exceeds the size limit
  $ cat k.txt
  ===formatted===
  small
  $ setconfig 'fix.code-format-max-file-size=500KB'

Test an unreadable file (broken symlink) is left out and the rest are formatted:
  $ ln -s missing-target broken-link
  $ echo "readable" > l.txt
  $ sl add broken-link l.txt
  $ sl amend
  code formatter: leaving out broken-link: * (glob)
  running code formatter: '*' (glob)
  code formatter completed successfully in * secs (glob)
  $ cat l.txt
  ===formatted===
  readable

Test mode=on skips formatting in automation (HGPLAIN):
  $ HGPLAIN=1 sl amend
  nothing changed
  [1]

Test mode=all runs formatting even in automation (HGPLAIN):
  $ setconfig 'fix.code-format-mode=all'
  $ echo "automated" > j.txt
  $ sl add j.txt
  $ HGPLAIN=1 sl amend
  running code formatter: '*' (glob)
  code formatter completed successfully in * secs (glob)
  $ cat j.txt
  ===formatted===
  automated

Test invalid code-format-mode config:
  $ setconfig 'fix.code-format-mode=invalid'
  $ echo "i" > i.txt
  $ sl add i.txt
  $ sl amend
  error: pre-amend.sl_code_format hook failed: invalid fix.code-format-mode: invalid (valid: off, agent, on (non-automation), all)
  abort: invalid fix.code-format-mode: invalid (valid: off, agent, on (non-automation), all)
  [255]

Re-enable for subsequent tests:
  $ setconfig 'fix.code-format-mode=on'

Test formatter failure does not abort amend:
  $ cat > "$TESTTMP/fail_formatter.py" <<EOF
  > import sys
  > sys.stderr.write("something went wrong\n")
  > sys.exit(1)
  > EOF
  $ setconfig fix.code-format-command="sl debugpython $TESTTMP/fail_formatter.py"
  $ echo "h" > h.txt
  $ sl add h.txt
  $ sl amend
  running code formatter: '*' (glob)
  skipping code formatter: failed to run '*': something went wrong (glob)
