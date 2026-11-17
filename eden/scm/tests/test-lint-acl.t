#require no-eden

Linting must not fetch ACL-restricted sibling trees that the linted
commits never touch, including during linter configuration file discovery.

  $ setconfig scmstore.fetch-tree-aux-data=true
  $ setconfig scmstore.tree-metadata-mode=always
  $ setconfig slacl.server-acl-enforcement=true

  $ newserver server
  $ drawdag << 'EOS'
  > A  # A/restricted/.slacl = acl config
  >    # A/restricted/secret.txt = secret content
  >    # A/regular/x.txt = one\n
  >    # A/.arcconfig = {}\n
  > EOS

  $ sl clone --config clone.use-rust=True --config format.use-eager-repo=false --config format.use-remotefilelog=true --config remotefilelog.reponame=client -q "test:server" "$TESTTMP/client"
  $ cd "$TESTTMP/client"

The checkout itself walks the full manifest and reports the restricted tree.

  $ sl goto -q $A
  warning: results may be incomplete due to path ACLs
    'restricted' is restricted by ACL 'some-acl'
  [1]

  $ cat > "$TESTTMP/linter.py" <<'PY'
  > import sys
  > from pathlib import Path
  > for line in sys.stdin:
  >     path = Path(line.strip())
  >     if path.read_bytes() == b"two\n":
  >         path.write_bytes(b"TWO\n")
  > PY
  $ setconfig "filelint.linter.test.command=$PYTHON $TESTTMP/linter.py @-"
  $ setconfig "filelint.linter.test.mode=staging-tree"
  $ setconfig "filelint.linter.test.fix=true"
  $ setconfig "filelint.linter.test.config-file.test=.arcconfig"

  $ printf 'two\n' > regular/x.txt
  $ sl commit -qm edit

Configuration file discovery lists the root directory, where the restricted
tree is a sibling; linting must succeed without touching it.

  $ sl lint -r .
  running linters: test
  Found 1 "test" issue:
    regular/x.txt
  fixed 1 files and rewrote 1 commits
  $ sl cat -r . regular/x.txt
  TWO
