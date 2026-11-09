#require no-eden

  $ enable snapshot
  $ newclientrepo

Other subcommands report a missing snapshot id as a usage error:

  $ sl snapshot show
  sl snapshot show: missing snapshot id
  (use 'sl snapshot show -h' to get help)
  [255]

'snapshot update' with no id crashes instead:

  $ sl snapshot update 2>&1 | grep -E '^(TypeError|sl snapshot update)'
  TypeError: update() missing 1 required positional argument: 'csid'
