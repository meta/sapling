A revision number outside the signed 64-bit range is in no revision set:
`_intlist` (what `%d` in a revset expands to) gives the empty set and a template
`ifcontains` is false, as for any other number that is not in the repo.

  $ newclientrepo
  $ drawdag <<'EOS'
  > B
  > |
  > A
  > EOS

In-range numbers behave as before: a commit's number is present; other numbers,
including nullrev, wdirrev and the i64 bounds, are absent.

  $ REVA=$(sl log -r 'desc(A)' -T '{rev}' --config experimental.revf64compat=false)
  $ sl log -r "_intlist('$REVA')" -T '[{desc}]\n'
  [A]
  $ sl log -r "_intlist('1000')" -T '[{desc}]\n'
  $ sl log -r "_intlist('-1')" -T '[{desc}]\n'
  $ sl log -r "_intlist('9223372036854775807')" -T '[{desc}]\n'
  $ sl log -r "_intlist('-9223372036854775808')" -T '[{desc}]\n'
  $ sl log -r 'desc(B)' -T '[{ifcontains(rev, revset("all()"), "y", "n")}]\n'
  [y]
  $ sl log -r 'desc(B)' -T '[{ifcontains(1000, revset("all()"), "y", "n")}]\n'
  [n]

Numbers outside the i64 range are absent too, not a crash.

# FIXME(T296478177): should be the empty set, not crash; the next diff fixes it.
  $ sl log -r "_intlist('9223372036854775808')" -T '[{desc}]\n' 2>&1 | grep -v '^  '
  ** Sapling SCM (version *) has crashed: (glob)
  Traceback (most recent call last):
  OverflowError: * (glob)

# FIXME(T296478177): should be the empty set, not crash; the next diff fixes it.
  $ sl log -r "_intlist('-9223372036854775809')" -T '[{desc}]\n' 2>&1 | grep -v '^  '
  ** Sapling SCM (version *) has crashed: (glob)
  Traceback (most recent call last):
  OverflowError: * (glob)

# FIXME(T296478177): should list [A] and [B], not crash; the next diff fixes it.
  $ sl log -r "all() - _intlist('9223372036854775808')" -T '[{desc}]\n' 2>&1 | grep -v '^  '
  ** Sapling SCM (version *) has crashed: (glob)
  Traceback (most recent call last):
  OverflowError: * (glob)

# FIXME(T296478177): should be the empty set, not crash; the next diff fixes it.
  $ sl log -r "draft() & _intlist('9223372036854775808')" -T '[{desc}]\n' 2>&1 | grep -v '^  '
  ** Sapling SCM (version *) has crashed: (glob)
  Traceback (most recent call last):
  OverflowError: * (glob)

# FIXME(T296478177): should print [], not crash; the next diff fixes it.
  $ sl log -r 'desc(B)' -T '[{revset("%d", 9223372036854775808)}]\n' 2>&1 | grep -v '^  '
  ** Sapling SCM (version *) has crashed: (glob)
  Traceback (most recent call last):
  OverflowError: * (glob)

# FIXME(T296478177): should print [n], not crash; the next diff fixes it.
  $ sl log -r 'desc(B)' -T '[{ifcontains(9223372036854775808, revset("all()"), "y", "n")}]\n' 2>&1 | grep -v '^  '
  ** Sapling SCM (version *) has crashed: (glob)
  Traceback (most recent call last):
  OverflowError: * (glob)

# FIXME(T296478177): should print [n], not crash; the next diff fixes it.
  $ sl log -r 'desc(B)' -T '[{ifcontains(-9223372036854775809, revset("all()"), "y", "n")}]\n' 2>&1 | grep -v '^  '
  ** Sapling SCM (version *) has crashed: (glob)
  Traceback (most recent call last):
  OverflowError: * (glob)
