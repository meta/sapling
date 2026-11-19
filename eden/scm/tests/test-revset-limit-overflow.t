`limit()`, `first()` and `last()` select the whole set when the count is past
the set size, and nothing when the offset is past it, however big the value.
This also holds past the fixed-width limits of the APIs that slice sets: 2**64
for Rust-backed sets sliced in the dag's own (newest-first) order (the dag's
skip and take take a u64), and 2**63 for sets sliced lazily in Python
(`itertools.islice` takes at most `sys.maxsize`). `sl log -l` slices sets the
same way.

  $ newclientrepo
  $ drawdag <<'EOS'
  > C
  > |
  > B
  > |
  > A
  > EOS
  $ sl up -q $C

Controls: the largest values below those limits, an ascending Rust-backed set
(its slicing clamps the count first), and negative values, which are rejected.

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

Rust-backed sets in newest-first order, from 2**64:

  $ sl log -r 'last(all(), 18446744073709551616)' -T '{desc}\n'
  A
  B
  C
  $ sl log -r 'limit(reverse(all()), 18446744073709551616)' -T '{desc}\n'
  C
  B
  A
  $ sl log -r 'limit(reverse(all()), 1, 18446744073709551616)' -T '{desc}\n'

Sets sliced lazily in Python, from 2**63, including when an offset plus a count
or nested offsets add up past it:

  $ sl log -r 'first(desc(C), 9223372036854775808)' -T '{desc}\n'
  C
  $ sl log -r 'last(desc(C), 9223372036854775808)' -T '{desc}\n'
  C
  $ sl log -r 'limit(. + .^, 9223372036854775808)' -T '{desc}\n'
  C
  B
  $ sl log -r 'limit(. + .^, 1, 9223372036854775808)' -T '{desc}\n'
  $ sl log -r 'limit(desc(C), 9223372036854775807, 9223372036854775807)' -T '{desc}\n'
  $ sl log -r 'limit(limit(desc(C), 1, 4611686018427387904), 1, 4611686018427387904)' -T '{desc}\n'

`sl log -l`:

  $ sl log -l 9223372036854775808 -r 'desc(C)' -T '{desc}\n'
  C
  $ sl log -l 18446744073709551616 -r 'reverse(all())' -T '{desc}\n'
  C
  B
  A
