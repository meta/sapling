A revision number outside the signed 64-bit range is not a revision: `rev(n)`
gives the empty set and a revision list aborts with "unknown revision", as for
any other number that is not in the repo.

  $ newclientrepo
  $ drawdag <<'EOS'
  > B
  > |
  > A
  > EOS

In-range numbers that are not in the repo are the empty set.

  $ sl log -r 'rev(1000)' -T '[{desc}]\n'
  $ sl log -r 'rev(9223372036854775806)' -T '[{desc}]\n'
  $ sl log -r 'rev(-9223372036854775808)' -T '[{desc}]\n'

The largest i64, wdirrev, still names the working directory.

  $ sl log -r 'rev(9223372036854775807)' -T '[{node}]\n'
  [ffffffffffffffffffffffffffffffffffffffff]

Numbers outside the i64 range are the empty set too, not a crash.

# FIXME(T296478177): should be the empty set, not crash; the next diff fixes it.
  $ sl log -r 'rev(9223372036854775808)' -T '[{desc}]\n' 2>&1 | grep -v '^  '
  ** Sapling SCM (version *) has crashed: (glob)
  Traceback (most recent call last):
  OverflowError: * (glob)

# FIXME(T296478177): should be the empty set, not crash; the next diff fixes it.
  $ sl log -r 'rev(18446744073709551616)' -T '[{desc}]\n' 2>&1 | grep -v '^  '
  ** Sapling SCM (version *) has crashed: (glob)
  Traceback (most recent call last):
  OverflowError: * (glob)

# FIXME(T296478177): should be the empty set, not crash; the next diff fixes it.
  $ sl log -r 'rev(-9223372036854775809)' -T '[{desc}]\n' 2>&1 | grep -v '^  '
  ** Sapling SCM (version *) has crashed: (glob)
  Traceback (most recent call last):
  OverflowError: * (glob)

# FIXME(T296478177): should be the empty set, not crash; the next diff fixes it.
  $ sl log -r 'children(rev(9223372036854775808))' -T '[{desc}]\n' 2>&1 | grep -v '^  '
  ** Sapling SCM (version *) has crashed: (glob)
  Traceback (most recent call last):
  OverflowError: * (glob)

# FIXME(T296478177): should list [A] and [B], not crash; the next diff fixes it.
  $ sl log -r 'all() - rev(9223372036854775808)' -T '[{desc}]\n' 2>&1 | grep -v '^  '
  ** Sapling SCM (version *) has crashed: (glob)
  Traceback (most recent call last):
  OverflowError: * (glob)

The same numbers in a revision list are unknown revisions, like a bare number.

  $ sl log -r '9223372036854775808' -T '[{desc}]\n'
  pulling '9223372036854775808' from 'test:repo1_server'
  pull failed: unknown revision '9223372036854775808'
  abort: unknown revision '9223372036854775808'!
  [255]

# FIXME(T296478177): should abort with "unknown revision" like the bare number,
# not crash; the next diff fixes it.
  $ sl log -r '9223372036854775808 + .' -T '[{desc}]\n' 2>&1 | grep -v '^  '
  pulling '9223372036854775808' from 'test:repo1_server'
  pull failed: unknown revision '9223372036854775808'
  ** Sapling SCM (version *) has crashed: (glob)
  Traceback (most recent call last):
  OverflowError: * (glob)

A non-number is still a parse error.

  $ sl log -r 'rev(x)' -T '[{desc}]\n'
  sl: parse error: rev expects a number
  [255]
