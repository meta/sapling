Test [agent] max-{file,tree}-fetch-count limits applied to scmstore for AI
coding agents. The guard is gated on `agentdetect::is_agent()` (set via the
`CODING_AGENT_METADATA` env var) and disabled in plain mode.

  $ newclientrepo

  $ mkdir dir1 dir2
  $ echo 1 > dir1/a
  $ echo 2 > dir1/b
  $ echo 3 > dir2/c
  $ sl ci -Aqm A

  $ echo 1mod > dir1/a
  $ echo 2mod > dir1/b
  $ echo 3mod > dir2/c
  $ sl ci -Aqm B

Without an agent the limit does nothing, even when set to 1:

  $ sl diff -r .~1 -r . --stat --config agent.max-file-fetch-count=1
   dir1/a |  2 +-
   dir1/b |  2 +-
   dir2/c |  2 +-
   3 files changed, 3 insertions(+), 3 deletions(-)

With an agent and a low file limit, sl diff aborts:

  $ CODING_AGENT_METADATA=id=test sl diff -r .~1 -r . --stat --config agent.max-file-fetch-count=1
  abort: command accessed over 1 files from store
  (narrow it to the paths you need, and find files by name with code search; see 'sl help agent performance')
  [255]

A high-enough limit admits the diff:

  $ CODING_AGENT_METADATA=id=test sl diff -r .~1 -r . --stat --config agent.max-file-fetch-count=100
   dir1/a |  2 +-
   dir1/b |  2 +-
   dir2/c |  2 +-
   3 files changed, 3 insertions(+), 3 deletions(-)

A limit of 0 explicitly disables the guard:

  $ CODING_AGENT_METADATA=id=test sl diff -r .~1 -r . --stat --config agent.max-file-fetch-count=0
   dir1/a |  2 +-
   dir1/b |  2 +-
   dir2/c |  2 +-
   3 files changed, 3 insertions(+), 3 deletions(-)

Plain mode bypasses the guard:

  $ HGPLAIN=1 CODING_AGENT_METADATA=id=test sl diff -r .~1 -r . --stat --config agent.max-file-fetch-count=1
   dir1/a |  2 +-
   dir1/b |  2 +-
   dir2/c |  2 +-
   3 files changed, 3 insertions(+), 3 deletions(-)

`agent.enable-fetch-guard=false` disables the guard wholesale (used by daemons
like EdenFS to opt out of the per-process counter):

  $ CODING_AGENT_METADATA=id=test sl diff -r .~1 -r . --stat \
  >   --config agent.enable-fetch-guard=false \
  >   --config agent.max-file-fetch-count=1
   dir1/a |  2 +-
   dir1/b |  2 +-
   dir2/c |  2 +-
   3 files changed, 3 insertions(+), 3 deletions(-)

The tree limit is independent and uses its own config key:

  $ CODING_AGENT_METADATA=id=test sl diff -r .~1 -r . --stat --config agent.max-tree-fetch-count=1
  abort: command accessed over 1 trees from store
  (narrow it to the paths you need, and find files by name with code search; see 'sl help agent performance')
  [255]
