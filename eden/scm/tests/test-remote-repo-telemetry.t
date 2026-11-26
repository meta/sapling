A remote-only command succeeds, but its command_info event omits the repo.

  $ eagerepo
  $ newserver remote-only
  $ echo payload > file
  $ sl commit -Aqm initial
  $ rev=$(sl log -r . -T '{node}')
  $ cd "$TESTTMP"
  $ SL_LOG=command_info=info sl cat -R test:remote-only -r "$rev" file 2> telemetry
  payload
  $ grep -o 'command_info: version=.*' telemetry
  command_info: version="[^"]*" is_linked_worktree=false (re)
