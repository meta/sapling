#require no-eden

  $ enable snapshot
  $ newclientrepo

Other subcommands report a missing snapshot id as a usage error:

  $ sl snapshot show
  sl snapshot show: missing snapshot id
  (use 'sl snapshot show -h' to get help)
  [255]

'snapshot update' with no id reports the same usage error:

  $ sl snapshot update
  sl snapshot update: missing snapshot id
  (use 'sl snapshot update -h' to get help)
  [255]

Too many ids is a usage error too:

  $ sl snapshot update aa bb
  sl snapshot update: invalid arguments
  (use 'sl snapshot update -h' to get help)
  [255]
