The `revset()` template function passes template values to
`revsetlang.formatspec`. Template node keywords (`node`, `p1node`, ...) are
40-digit hex strings, but `%n` only accepts a 20-byte binary node and crashes
with an uncaught TypeError on a str.

  $ newclientrepo
  $ drawdag <<'EOS'
  > C
  > |
  > B
  > |
  > A
  > EOS
  $ sl up -q $C

Control: `%s` with the same value works.

  $ sl log -r . -T '{revset("%s", node) % "{desc}"}\n'
  C
  $ sl log -r . -T '{revset("parents(%s)", node) % "{desc}"}\n'
  B

Bug: `%n` and `%ln` with a hex str crash.

  $ sl log -r . -T '{revset("%n", node) % "{desc}"}\n' 2>&1 | tail -1
  TypeError: descriptor 'hex' for 'bytes' objects doesn't apply to a 'str' object
  $ sl log -r . -T '{revset("parents(%n)", node) % "{desc}"}\n' 2>&1 | tail -1
  TypeError: descriptor 'hex' for 'bytes' objects doesn't apply to a 'str' object
  $ sl log -r . -T '{revset("%n", p1node) % "{desc}"}\n' 2>&1 | tail -1
  TypeError: descriptor 'hex' for 'bytes' objects doesn't apply to a 'str' object
  $ sl log -r . -T '{revset("successors(%n) - %n", node, node) % "{desc}"}\n' 2>&1 | tail -1
  TypeError: descriptor 'hex' for 'bytes' objects doesn't apply to a 'str' object
  $ sl log -r . -T '{revset("%ln", node) % "{desc}"}\n' 2>&1 | tail -1
  TypeError: descriptor 'hex' for 'bytes' objects doesn't apply to a 'str' object

A non-hex str also crashes instead of giving a clean error.

  $ sl log -r . -T '{revset("%n", "abc")}\n' 2>&1 | tail -1
  TypeError: descriptor 'hex' for 'bytes' objects doesn't apply to a 'str' object
