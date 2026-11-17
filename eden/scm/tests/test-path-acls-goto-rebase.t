#testcases rustcheckout pythoncheckout

#if rustcheckout
  $ setconfig checkout.use-rust=true
#endif

#if pythoncheckout
  $ setconfig checkout.use-rust=false
#endif

  $ setconfig scmstore.fetch-tree-aux-data=true
  $ setconfig scmstore.tree-metadata-mode=always
  $ setconfig slacl.server-acl-enforcement=true

  $ enable rebase

Goto: check out a commit where a previously-unrestricted file becomes restricted

  $ newserver server1
  $ drawdag << 'EOS'
  > B  # B/dir/.slacl = acl config
  >    # B/dir/file.txt = content v2
  > |
  > A  # A/dir/file.txt = content v1
  > EOS

  $ cd
  $ newclientrepo client1 server1
  $ sl go -q $A
#if no-eden
  $ sl go -q $B
  warning: results may be incomplete due to path ACLs
    'dir' is restricted by ACL 'some-acl'
  [1]
#else
  $ sl go -q $B
#endif

Goto: local modifications to a file that becomes restricted

  $ newserver server2
  $ drawdag << 'EOS'
  > B  # B/dir/.slacl = acl config
  >    # B/dir/file.txt = content v2
  > |
  > A  # A/dir/file.txt = content v1
  > EOS

  $ cd
  $ newclientrepo client2 server2
  $ sl go -q $A
  $ echo 'local change' > dir/file.txt
#if eden
  $ sl go -q $B
  abort: 1 conflicting file changes: (rustcheckout !)
   dir/file.txt (rustcheckout !)
  (commit, shelve, goto --clean to discard all your changes, or goto --merge to merge them) (rustcheckout !)
  abort: conflicting changes: (pythoncheckout !)
    dir/file.txt (pythoncheckout !)
  (commit or goto --clean to discard changes) (pythoncheckout !)
  [255]
#else
  $ sl go -q $B
  abort: 1 conflicting file changes:
   dir/file.txt
  (commit, shelve, goto --clean to discard all your changes, or goto --merge to merge them)
  warning: results may be incomplete due to path ACLs
    'dir' is restricted by ACL 'some-acl'
  [255]
#endif

Rebase: commit modifies a file that is restricted in destination

  $ newserver server3
  $ drawdag << 'EOS'
  > C  # C/dir/.slacl = acl config
  >    # C/dir/file.txt = content v2
  > |
  > | B  # B/dir/file.txt = modified by user
  > |/
  > A  # A/dir/file.txt = content v1
  > EOS

  $ cd
  $ newclientrepo client3 server3
  $ sl go -q $B
  $ sl rebase -r $B -d $C
  pulling '3af88752c97bb3f6651d0a57a3d16a696f28de48' from 'test:server3'
  rebasing c416137c0b61 "B"
  other [source] changed dir/file.txt which is restricted in local [dest]
  (d)elete/drop this file, input (m)oved path, or leave (u)nresolved? u
  unresolved conflicts (see sl resolve, then sl rebase --continue)
  warning: results may be incomplete due to path ACLs
    'dir' is restricted by ACL 'some-acl'
  [1]

Rebase: commit adds a file under a path that is restricted in destination

  $ newserver server4
  $ drawdag << 'EOS'
  > C  # C/restricted/.slacl = acl config
  >    # C/restricted/existing.txt = existing
  > |
  > | B  # B/restricted/new.txt = new file
  > |/
  > A  # A/dummy = dummy
  > EOS

  $ cd
  $ newclientrepo client4 server4
  $ sl go -q $B
  $ sl rebase -q -r $B -d $C
  other [source] changed restricted/new.txt which is restricted in local [dest]
  (d)elete/drop this file, input (m)oved path, or leave (u)nresolved? u
  unresolved conflicts (see sl resolve, then sl rebase --continue)
  warning: results may be incomplete due to path ACLs
    'restricted' is restricted by ACL 'some-acl'
  [1]

Rebase: two commits where only the second touches a restricted path

  $ newserver server5
  $ drawdag << 'EOS'
  > D  # D/dir/.slacl = acl config
  >    # D/dir/file.txt = dest content
  >    # D/other.txt = original
  > |
  > | C  # C/dir/file.txt = restricted change
  > | |
  > | B  # B/other.txt = safe change
  > |/
  > A  # A/dir/file.txt = original
  >    # A/other.txt = original
  > EOS

  $ cd
  $ newclientrepo client5 server5
  $ sl go -q $C
  $ sl rebase -r $B::$C -d $D
  pulling '16e6c5ae0beee858c20c00828646da495a094d26' from 'test:server5'
  rebasing 01f209e23a69 "B"
  rebasing 5f76ba0bb512 "C"
  other [source] changed dir/file.txt which is restricted in local [dest]
  (d)elete/drop this file, input (m)oved path, or leave (u)nresolved? u
  unresolved conflicts (see sl resolve, then sl rebase --continue)
  warning: results may be incomplete due to path ACLs
    'dir' is restricted by ACL 'some-acl'
  [1]

Rebase: mixed visible and restricted source changes with no restricted conflict

  $ newserver server6
  $ drawdag << 'EOS'
  > C B
  > |/
  > A
  >   # A/public.txt = public v1
  >   # A/other.txt = other v1
  >   # A/restricted/.slacl = acl config
  >   # A/restricted/secret.txt = secret v1
  >   # B/public.txt = public v2
  >   # B/restricted/secret.txt = secret v2
  >   # C/other.txt = other v2
  >   # drawdag.defaultfiles=false
  > EOS

  $ cd
  $ newclientrepo client6 server6
#if eden
  $ sl --config slacl.mixed-commit-mode=ignore go -q $B
#else
  $ sl go -q $B
  warning: results may be incomplete due to path ACLs
    'restricted' is restricted by ACL 'some-acl'
  [1]
#endif
  $ HGPLAIN=1 sl rebase -q -r $B -d $C
  abort: cannot rewrite commits with restricted paths
    'restricted' is restricted by ACL 'some-acl'
  (use '--config slacl.mixed-commit-mode=warn' to bypass)
  [255]
  $ sl status
  $ sl log -r . -T '{files}\n'
  public.txt restricted/secret.txt
  $ sl cat public.txt
  public v2 (no-eol)
  $ sl cat other.txt
  other v1 (no-eol)

Rebase: mixed visible and restricted source changes with an unseen restricted conflict

  $ newserver server7
  $ drawdag << 'EOS'
  > C B
  > |/
  > A
  >   # A/public.txt = public v1
  >   # A/restricted/.slacl = acl config
  >   # A/restricted/secret.txt = secret v1
  >   # B/public.txt = public v2
  >   # B/restricted/secret.txt = source secret v2
  >   # C/restricted/secret.txt = dest secret v2
  >   # drawdag.defaultfiles=false
  > EOS

  $ cd
  $ newclientrepo client7 server7
#if eden
  $ sl --config slacl.mixed-commit-mode=ignore go -q $B
#else
  $ sl go -q $B
  warning: results may be incomplete due to path ACLs
    'restricted' is restricted by ACL 'some-acl'
  [1]
#endif
  $ HGPLAIN=1 sl rebase -q -r $B -d $C
  abort: cannot rewrite commits with restricted paths
    'restricted' is restricted by ACL 'some-acl'
  (use '--config slacl.mixed-commit-mode=warn' to bypass)
  [255]
  $ sl status
  $ sl log -r . -T '{files}\n'
  public.txt restricted/secret.txt
  $ sl cat public.txt
  public v2 (no-eol)

Rebase: ACL checks are repeated for the same restricted tree

  $ newserver server8
  $ drawdag << 'EOS'
  > E  # E/dir/.slacl = acl config
  >    # E/dir/file.txt = destination content
  >    # E/other.txt = original
  > |
  > | D  # D/other.txt = stack change 3
  > | |
  > | C  # C/other.txt = stack change 2
  > | |
  > | B  # B/other.txt = stack change 1
  > |/
  > A  # A/dir/file.txt = original
  >    # A/other.txt = original
  > EOS

  $ cd
  $ newclientrepo client8 server8
  $ sl go -q $D

EdenFS rebase does not check the destination-only restricted tree; non-Eden checkout does.
  $ SL_LOG=eagerepo::api=debug sl rebase -r $B::$D -d $E 2>&1 | grep check_manifest_permission || true
  DEBUG eagerepo::api: check_manifest_permission d4ef899346f65d1984b2a14db0f44f42df35d2d4 (no-eden !)

#if eden
Rebase currently fetches a restricted sibling even when the rebased commit does not touch it

  $ newserver server9
  $ drawdag << 'EOS'
  > C B
  > |/
  > A
  >   # C/users/restricted_owner/.slacl = acl config
  >   # B/users/active_user/note.txt = updated public content
  >   # A/users/restricted_owner/private.txt = private content
  >   # A/users/active_user/note.txt = public content
  >   # drawdag.defaultfiles=false
  > EOS

  $ cd
  $ newclientrepo client9 server9
  $ setconfig rebase.experimental.inmemory=True
  $ sl go -q $B
  $ HGPLAIN=1 sl rebase -q -r $B -d $C
  $ sl log -r . -T '{files}\n'
  users/active_user/note.txt
#endif
