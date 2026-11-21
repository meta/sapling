The `revset()` template function passes template values to
`revsetlang.formatspec`. Template node keywords (`node`, `p1node`, ...) are
40-digit hex strings, but `%n` and `%ln` take 20-byte binary nodes.

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

`%n` and `%ln` with a hex str abort with a clear message.

  $ sl log -r . -T '{revset("%n", node) % "{desc}"}\n'
  abort: %n expects a binary node, not '26805aba1e600a82e93661149f2313866a221a7b'
  (use %s for a hex node)
  [255]
  $ sl log -r . -T '{revset("parents(%n)", node) % "{desc}"}\n'
  abort: %n expects a binary node, not '26805aba1e600a82e93661149f2313866a221a7b'
  (use %s for a hex node)
  [255]
  $ sl log -r . -T '{revset("%n", p1node) % "{desc}"}\n'
  abort: %n expects a binary node, not '112478962961147124edd43549aedd1a335e44bf'
  (use %s for a hex node)
  [255]
  $ sl log -r . -T '{revset("successors(%n) - %n", node, node) % "{desc}"}\n'
  abort: %n expects a binary node, not '26805aba1e600a82e93661149f2313866a221a7b'
  (use %s for a hex node)
  [255]
  $ sl log -r . -T '{revset("%ln", node) % "{desc}"}\n'
  abort: %ln expects a list of binary nodes, not '26805aba1e600a82e93661149f2313866a221a7b'
  (use %s for a hex node)
  [255]

So does a str that is not a hex node.

  $ sl log -r . -T '{revset("%n", "abc")}\n'
  abort: %n expects a binary node, not 'abc'
  (use %s for a hex node)
  [255]
