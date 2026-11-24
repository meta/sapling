#require no-eden

  $ setconfig scmstore.fetch-tree-aux-data=true
  $ setconfig scmstore.tree-metadata-mode=always
  $ enable rebase

"dir/restricted" has an ACL, so the server marks it as an ACL child of "dir".

  $ newserver server
  $ drawdag << 'EOS'
  > C  # C/dir/other.txt = dest change
  > |
  > | B  # B/dir/new.txt = new file
  > |/
  > A  # A/dir/restricted/.slacl = acl config
  >    # A/dir/restricted/file.txt = secret
  >    # A/dir/open.txt = open
  > EOS

  $ newclientrepo client server
  $ sl go -q $B
  $ sl debugscmstore -r $B dir --mode=tree --fetch-mode=LOCAL | grep acl_children_indices
                          acl_children_indices: Some(

Rebasing B onto C rewrites "dir", and the new "dir" keeps its ACL child.

  $ sl rebase -q -r $B -d $C
  $ sl debugscmstore -r . dir --mode=tree --fetch-mode=LOCAL | grep acl_children_indices
                          acl_children_indices: Some(
