When a command runs with a local repository, use the name from its default
URL even if the configured repo name differs. Without a default path, Sapling
omits the repo field from its command_info event.

  $ eagerepo
  $ newclientrepo checkout upstream
  $ SL_LOG=command_info=info sl root > /dev/null 2> telemetry
  $ grep -o 'command_info: version=.*' telemetry
  command_info: version="[^"]*" repo="upstream" is_linked_worktree=false (re)
  $ sl paths --delete default
  $ SL_LOG=command_info=info sl root > /dev/null 2> telemetry
  $ grep -o 'command_info: version=.*' telemetry
  command_info: version="[^"]*" is_linked_worktree=false (re)

A command run outside a repository also omits the repo field.

  $ cd "$TESTTMP"
  $ SL_LOG=command_info=info sl version -q > /dev/null 2> telemetry
  $ grep -o 'command_info: version=.*' telemetry
  command_info: version="[^"]*" is_linked_worktree=false (re)
