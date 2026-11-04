#require no-eden
#inprocess-hg-incompatible

  $ setconfig scmstore.fetch-tree-aux-data=true
  $ setconfig scmstore.tree-metadata-mode=always
  $ setconfig experimental.restricted-tree-mode=enforced
  $ setconfig slacl.server-acl-enforcement=true
  $ setconfig sampling.filepath=$TESTTMP/samples sampling.key.command_info=dct

  $ newserver server
  $ drawdag << 'EOS'
  > A  # A/restricted/.slacl = acl config
  >    # A/restricted/secret.txt = secret content
  >    # A/regular/file.txt = regular content\n
  > EOS

  $ sl clone --config clone.use-rust=True --config format.use-eager-repo=false --config format.use-remotefilelog=true --config remotefilelog.reponame=client -q "test:server" "$TESTTMP/client"
  $ cd "$TESTTMP/client"

  $ cat > $TESTTMP/denied.py <<'PY'
  > import json, os
  > path = os.path.join(os.environ["TESTTMP"], "samples")
  > flags = []
  > if os.path.exists(path):
  >     with open(path, "rb") as f:
  >         for record in f.read().split(b"\0"):
  >             if record:
  >                 data = json.loads(record)["data"]
  >                 if "slacl_permission_denied" in data:
  >                     flags.append(data["slacl_permission_denied"])
  >     os.unlink(path)
  > print(flags)
  > PY

A command denied access to a restricted path reports it through `command_info`
sampling. The Rust dispatcher:
  $ sl go -q $A
  warning: results may be incomplete due to path ACLs
    'restricted' is restricted by ACL 'some-acl'
  [1]
  $ sl debugpython -- $TESTTMP/denied.py
  [True]

The Python dispatcher, and `slacl.on-permission-denied` does not matter:
  $ sl cat -r $A 'glob:**.txt' --config slacl.on-permission-denied=ignore
  regular content
  $ sl debugpython -- $TESTTMP/denied.py
  [True]

A command that never touches a restricted path reports nothing:
  $ sl cat -r $A regular/file.txt
  regular content
  $ sl debugpython -- $TESTTMP/denied.py
  []
