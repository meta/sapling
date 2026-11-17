#require no-eden

Status across a commit with both ACL'd and non-ACL'd files:

  $ newserver server1
  $ drawdag << 'EOS'
  > B  # B/regular/file.txt = updated content
  >    # B/restricted/.slacl = acl config
  >    # B/restricted/secret.txt = updated secret
  > |
  > A  # A/regular/file.txt = regular content
  >    # A/restricted/.slacl = acl config
  >    # A/restricted/secret.txt = secret content
  > EOS

  $ cd
  $ setconfig scmstore.fetch-tree-aux-data=true
  $ setconfig scmstore.tree-metadata-mode=always
  $ setconfig slacl.server-acl-enforcement=true
  $ newclientrepo client1 server1
  $ sl go -q $B
  warning: results may be incomplete due to path ACLs
    'restricted' is restricted by ACL 'some-acl'
  [1]

  $ sl status --change $B
  A B
  A regular/file.txt
  warning: results may be incomplete due to path ACLs
    'restricted' is restricted by ACL 'some-acl'
  [1]

Status across a commit with only ACL'd files:

  $ newserver server2
  $ drawdag << 'EOS'
  > B  # B/restricted/.slacl = acl config
  >    # B/restricted/secret.txt = updated secret
  > |
  > A  # A/restricted/.slacl = acl config
  >    # A/restricted/secret.txt = secret content
  > EOS

  $ cd
  $ setconfig scmstore.fetch-tree-aux-data=true
  $ setconfig scmstore.tree-metadata-mode=always
  $ setconfig slacl.server-acl-enforcement=true
  $ newclientrepo client2 server2
  $ sl go -q $B
  warning: results may be incomplete due to path ACLs
    'restricted' is restricted by ACL 'some-acl'
  [1]

  $ sl status --change $B
  A B
  warning: results may be incomplete due to path ACLs
    'restricted' is restricted by ACL 'some-acl'
  [1]

Status across a commit that adds an ACL to an existing directory:

  $ newserver server3
  $ drawdag << 'EOS'
  > B  # B/dir/.slacl = acl config
  >    # B/dir/file.txt = content
  > |
  > A  # A/dir/file.txt = content
  > EOS

  $ cd
  $ setconfig scmstore.fetch-tree-aux-data=true
  $ setconfig scmstore.tree-metadata-mode=always
  $ setconfig slacl.server-acl-enforcement=true
  $ newclientrepo client3 server3
  $ sl go -q $B
  warning: results may be incomplete due to path ACLs
    'dir' is restricted by ACL 'some-acl'
  [1]

  $ sl status --change $B
  A B
  warning: results may be incomplete due to path ACLs
    'dir' is restricted by ACL 'some-acl'
  [1]

Status across a commit that removes an ACL from a directory:

  $ newserver server4
  $ drawdag << 'EOS'
  > B  # B/dir/file.txt = content
  > |
  > A  # A/dir/.slacl = acl config
  >    # A/dir/file.txt = content
  > EOS

  $ cd
  $ setconfig scmstore.fetch-tree-aux-data=true
  $ setconfig scmstore.tree-metadata-mode=always
  $ setconfig slacl.server-acl-enforcement=true
  $ newclientrepo client4 server4
  $ sl go -q $B
  warning: results may be incomplete due to path ACLs
    'dir' is restricted by ACL 'some-acl'
  [1]

  $ sl status --change $B
  A B
