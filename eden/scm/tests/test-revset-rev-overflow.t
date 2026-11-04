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

  $ sl log -r 'rev(9223372036854775808)' -T '[{desc}]\n'
  $ sl log -r 'rev(18446744073709551616)' -T '[{desc}]\n'
  $ sl log -r 'rev(-9223372036854775809)' -T '[{desc}]\n'
  $ sl log -r 'children(rev(9223372036854775808))' -T '[{desc}]\n'
  $ sl log -r 'all() - rev(9223372036854775808)' -T '[{desc}]\n'
  [A]
  [B]

The same numbers in a revision list are unknown revisions, like a bare number.

  $ sl log -r '9223372036854775808' -T '[{desc}]\n'
  pulling '9223372036854775808' from 'test:repo1_server'
  pull failed: unknown revision '9223372036854775808'
  abort: unknown revision '9223372036854775808'!
  [255]
  $ sl log -r '9223372036854775808 + .' -T '[{desc}]\n'
  pulling '9223372036854775808' from 'test:repo1_server'
  pull failed: unknown revision '9223372036854775808'
  abort: unknown revision '9223372036854775808'!
  [255]

A non-number is still a parse error.

  $ sl log -r 'rev(x)' -T '[{desc}]\n'
  sl: parse error: rev expects a number
  [255]
