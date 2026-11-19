#require no-eden

  $ configure modernclient
  $ newclientrepo repo
  $ drawdag <<'EOS'
  > B
  > |
  > A
  > EOS
  $ sl hide -q $B
  $ sl log -r 'draft()' -T '{desc}\n'
  A

Pulling a commit the repo already has makes it visible, unless the pull asks
not to.

  $ sl debugshell -c "from sapling.node import bin; repo.pull(headnodes=[bin('$B')], visible=False)"
  $ sl log -r 'draft()' -T '{desc}\n'
  A
  $ sl debugshell -c "from sapling.node import bin; repo.pull(headnodes=[bin('$B')])"
  $ sl log -r 'draft()' -T '{desc}\n'
  A
  B
