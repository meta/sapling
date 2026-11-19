FIXME(T296818089): `limit()`, `first()` and `last()` crash once the count or
offset is too big for the API that slices the set. A count past the set size
should select the whole set, and an offset past it nothing, as smaller values
already do. Rust-backed sets sliced in the dag's own (newest-first) order crash
at 2**64 (the dag's skip and take take a u64). Sets sliced lazily in Python
crash at 2**63 (`itertools.islice` takes at most `sys.maxsize`). `sl log -l`
slices sets the same way.

  $ newclientrepo
  $ drawdag <<'EOS'
  > C
  > |
  > B
  > |
  > A
  > EOS
  $ sl up -q $C

Controls that work today: the largest values below those limits, an ascending
Rust-backed set (its slicing clamps the count first), and negative values, which
are rejected.

  $ sl log -r 'last(all(), 18446744073709551615)' -T '{desc}\n'
  A
  B
  C
  $ sl log -r 'first(all(), 18446744073709551616)' -T '{desc}\n'
  A
  B
  C
  $ sl log -r 'limit(reverse(all()), 1, 18446744073709551615)' -T '{desc}\n'
  $ sl log -r 'first(desc(C), 9223372036854775807)' -T '{desc}\n'
  C
  $ sl log -r 'limit(. + .^, 1, 9223372036854775806)' -T '{desc}\n'
  $ sl log -l 9223372036854775807 -r 'desc(C)' -T '{desc}\n'
  C
  $ sl log -r 'first(desc(C), -9223372036854775808)' -T '{desc}\n'
  sl: parse error: negative number to select
  [255]
  $ sl log -r 'limit(desc(C), 1, -9223372036854775808)' -T '{desc}\n'
  sl: parse error: negative offset
  [255]

FIXME(T296818089): Rust-backed sets in newest-first order crash at 2**64. These
should print "A B C", "C B A" and nothing.

  $ sl log -r 'last(all(), 18446744073709551616)' -T '{desc}\n' 2>&1 | egrep -v '^  '
  ** * has crashed: (glob)
  Traceback (most recent call last):
  OverflowError: int too big to convert
  $ sl log -r 'limit(reverse(all()), 18446744073709551616)' -T '{desc}\n' 2>&1 | egrep -v '^  '
  ** * has crashed: (glob)
  Traceback (most recent call last):
  OverflowError: int too big to convert
  $ sl log -r 'limit(reverse(all()), 1, 18446744073709551616)' -T '{desc}\n' 2>&1 | egrep -v '^  '
  ** * has crashed: (glob)
  Traceback (most recent call last):
  OverflowError: int too big to convert

FIXME(T296818089): sets sliced lazily in Python crash at 2**63, including when
offset + count or nested offsets add up past it. These should print "C", "C",
"C B" and then nothing three times.

  $ sl log -r 'first(desc(C), 9223372036854775808)' -T '{desc}\n' 2>&1 | egrep -v '^  '
  ** * has crashed: (glob)
  Traceback (most recent call last):
  ValueError: Stop argument for islice() must be None or an integer: * (glob)
  $ sl log -r 'last(desc(C), 9223372036854775808)' -T '{desc}\n' 2>&1 | egrep -v '^  '
  ** * has crashed: (glob)
  Traceback (most recent call last):
  ValueError: Stop argument for islice() must be None or an integer: * (glob)
  $ sl log -r 'limit(. + .^, 9223372036854775808)' -T '{desc}\n' 2>&1 | egrep -v '^  '
  ** * has crashed: (glob)
  Traceback (most recent call last):
  ValueError: Stop argument for islice() must be None or an integer: * (glob)
  $ sl log -r 'limit(. + .^, 1, 9223372036854775808)' -T '{desc}\n' 2>&1 | egrep -v '^  '
  ** * has crashed: (glob)
  Traceback (most recent call last):
  ValueError: Stop argument for islice() must be None or an integer: * (glob)
  $ sl log -r 'limit(desc(C), 9223372036854775807, 9223372036854775807)' -T '{desc}\n' 2>&1 | egrep -v '^  '
  ** * has crashed: (glob)
  Traceback (most recent call last):
  ValueError: Stop argument for islice() must be None or an integer: * (glob)
  $ sl log -r 'limit(limit(desc(C), 1, 4611686018427387904), 1, 4611686018427387904)' -T '{desc}\n' 2>&1 | egrep -v '^  '
  ** * has crashed: (glob)
  Traceback (most recent call last):
  ValueError: Stop argument for islice() must be None or an integer: * (glob)

FIXME(T296818089): `sl log -l` crashes the same way. These should print "C" and
"C B A".

  $ sl log -l 9223372036854775808 -r 'desc(C)' -T '{desc}\n' 2>&1 | egrep -v '^  '
  ** * has crashed: (glob)
  Traceback (most recent call last):
  ValueError: Stop argument for islice() must be None or an integer: * (glob)
  $ sl log -l 18446744073709551616 -r 'reverse(all())' -T '{desc}\n' 2>&1 | egrep -v '^  '
  ** * has crashed: (glob)
  Traceback (most recent call last):
  OverflowError: int too big to convert
