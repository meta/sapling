
#require no-eden

  $ setconfig scmstore.fetch-tree-aux-data=true
  $ setconfig scmstore.tree-metadata-mode=always
  $ setconfig slacl.server-acl-enforcement=true
  $ enable rebase histedit

  $ newserver server
  $ drawdag << 'EOS'
  > A  # A/restricted/.slacl = acl config
  >    # A/restricted/secret.txt = secret content
  >    # A/regular/file.txt = regular content\n
  > EOS

  $ sl clone --config clone.use-rust=True --config format.use-eager-repo=false --config format.use-remotefilelog=true --config remotefilelog.reponame=client -q "test:server" "$TESTTMP/client"
  $ cd "$TESTTMP/client"

  $ sl debugmanifestdirs -qr $A
  19d1f9c4aa6e6b299fa6a863b253889df872ae0f restricted
  48ce1df7933d0a1ad2493f30a53389eeab2394f9 regular
  636776545d9d741d0fcea98bd36478a343b43a8f /
  warning: results may be incomplete due to path ACLs
    'restricted' is restricted by ACL 'some-acl'
  [1]

Don't attempt to fetch 19d1f9c4 - it is restricted
  $ LOG=tree_fetches=trace sl go -q $A
  TRACE tree_fetches: attrs=["content"] keys=["@63677654"]
  TRACE tree_fetches: attrs=["content"] keys=["@48ce1df7"]
  warning: results may be incomplete due to path ACLs
    'restricted' is restricted by ACL 'some-acl'
  [1]

  $ find .
  ./A
  ./regular
  ./regular/file.txt

Give a specific message when referencing a restricted file:
  $ sl cat restricted/secret.txt
  restricted/secret.txt: restricted path
  warning: results may be incomplete due to path ACLs
    'restricted' is restricted by ACL 'some-acl'
  [1]

Doesn't having warning since it uses dirstate to walk.
  $ sl files restricted/secret.txt
  restricted/secret.txt: restricted path
  [1]

  $ sl files -r . restricted/secret.txt
  restricted/secret.txt: restricted path
  warning: results may be incomplete due to path ACLs
    'restricted' is restricted by ACL 'some-acl'
  [1]

The server's denial message, when it sends one, prints after the ACL line:
  $ sl files -r . restricted/secret.txt --config slacl.server-denial-message='Ask the repo owners for access.'
  restricted/secret.txt: restricted path
  warning: results may be incomplete due to path ACLs
    'restricted' is restricted by ACL 'some-acl'
    Ask the repo owners for access.
  [1]

Experimental fallback can treat matching Python manifest lookups as not found:
  $ cat > check_manifest_get.py <<'PY'
  > from sapling import error, hg, ui as uimod
  > repo = hg.repository(uimod.ui.load(), ".")
  > try:
  >     print(repo["."].manifest().get("restricted/secret.txt", b"missing"))
  > except error.PermissionDeniedError:
  >     print("permission denied")
  > PY
  $ sl debugpython -- check_manifest_get.py
  permission denied
  $ setconfig experimental.slacl-ignore-permission-denied-regex=check_manifest_get.py
  $ sl debugpython -- check_manifest_get.py
  b'missing'

Make sure root tree has acl indices populated in cache
  $ sl debugscmstore -r $A '' --mode=tree | grep -A 4 acl_children_indices
                          acl_children_indices: Some(
                              [
                                  2,
                              ],
                          ),

After committing a change outside the restricted directory, the new root
tree should still have acl_children_indices for the unchanged restricted directory:
  $ echo modified > regular/file.txt
  $ sl commit -m 'modify regular file'
  $ sl debugscmstore -r . '' --mode=tree | grep -A 4 acl_children_indices
                          acl_children_indices: Some(
                              [
                                  2,
                              ],
                          ),

Rust commands also warn about restricted paths:
  $ sl grep -r $A 'content'
  regular/file.txt:regular content
  warning: results may be incomplete due to path ACLs
    'restricted' is restricted by ACL 'some-acl'
  [1]

Repoless cat warns when a traversal skips restricted paths.
  $ sl cat -R test:server -r $A 'glob:**.txt'
  regular content
  warning: results may be incomplete due to path ACLs
    'restricted' is restricted by ACL 'some-acl'
  [1]

Matcher-scoped BFS should not check ACLs under directories it will not visit:

  $ newserver server2
  $ drawdag << 'EOS'
  > A  # A/some_dir/public.txt = public content
  >    # A/some_dir/secret/.slacl = acl config
  >    # A/some_dir/secret/private.txt = private content
  > EOS

  $ cd
  $ newclientrepo client2 server2

No permission check is needed for `some_dir/secret` when only listing `some_dir/public.txt`.
  $ SL_LOG=eagerepo::api=debug sl files -r $A some_dir/public.txt 2>&1 | grep check_manifest_permission || true

Copy/move with .slacl paths:

  $ newserver server_path_acl_copy_move
  $ drawdag << 'EOS'
  > A  # A/restricted/.slacl = acl config
  >    # A/restricted/subdir/secret.txt = secret v1
  >    # A/parent/public.txt = visible
  >    # A/parent/restricted/.slacl = acl config
  >    # A/parent/restricted/secret.txt = secret v1
  >    # A/public/file.txt = public
  > EOS

Current behavior: users with access can copy and move restricted files to
unrestricted paths without warning.
  $ cd
  $ setconfig slacl.server-acl-enforcement=false
  $ newclientrepo client_path_acl_access server_path_acl_copy_move
  $ sl go -q $A
  $ sl cp restricted/subdir/secret.txt public/copied-secret.txt
  $ cat public/copied-secret.txt
  secret v1 (no-eol)
  $ sl mv restricted/subdir/secret.txt public/moved-secret.txt
  $ cat public/moved-secret.txt
  secret v1 (no-eol)
  $ sl status
  A public/copied-secret.txt
  A public/moved-secret.txt
  R restricted/subdir/secret.txt

Users with access can copy and move restricted files within the restricted tree.
  $ cd
  $ setconfig slacl.server-acl-enforcement=false
  $ newclientrepo client_path_acl_access_self server_path_acl_copy_move
  $ sl go -q $A
  $ sl cp restricted/subdir/secret.txt restricted/subdir/copied-secret.txt
  $ cat restricted/subdir/copied-secret.txt
  secret v1 (no-eol)
  $ sl mv restricted/subdir/secret.txt restricted/subdir/moved-secret.txt
  $ cat restricted/subdir/moved-secret.txt
  secret v1 (no-eol)
  $ sl status
  A restricted/subdir/copied-secret.txt
  A restricted/subdir/moved-secret.txt
  R restricted/subdir/secret.txt

People without access cannot copy or move a restricted file to an unrestricted path.
  $ cd
  $ setconfig slacl.server-acl-enforcement=true
  $ newclientrepo client_path_acl_no_access server_path_acl_copy_move
  $ sl go -q $A
  warning: results may be incomplete due to path ACLs
    'parent/restricted' [and 1 more] are restricted by ACL 'some-acl'
  [1]
  $ sl cp restricted/subdir/secret.txt public/copied-secret.txt
  restricted/subdir/secret.txt: $ENOENT$ (no-windows !)
  restricted\subdir\secret.txt: $ENOTDIR$ (windows !)
  abort: no files to copy
  (use '--amend --mark' if you want to amend the current commit)
  warning: results may be incomplete due to path ACLs
    'restricted' is restricted by ACL 'some-acl'
  [255]
  $ sl mv restricted/subdir/secret.txt public/moved-secret.txt
  restricted/subdir/secret.txt: $ENOENT$ (no-windows !)
  restricted\subdir\secret.txt: $ENOTDIR$ (windows !)
  abort: no files to copy
  (use '--amend --mark' if you want to amend the current commit)
  warning: results may be incomplete due to path ACLs
    'restricted' is restricted by ACL 'some-acl'
  [255]

People without access can copy or move the unrestricted files under a parent that contains restricted paths.
  $ cd
  $ setconfig slacl.server-acl-enforcement=true
  $ newclientrepo client_path_acl_parent_no_access server_path_acl_copy_move
  $ sl go -q $A
  warning: results may be incomplete due to path ACLs
    'parent/restricted' [and 1 more] are restricted by ACL 'some-acl'
  [1]
  $ sl cp parent public/copied-parent
  copying parent/public.txt to public/copied-parent/public.txt
  warning: results may be incomplete due to path ACLs
    'parent/restricted' is restricted by ACL 'some-acl'
  [1]
  $ cat public/copied-parent/public.txt
  visible (no-eol)
  $ test ! -e public/copied-parent/restricted/secret.txt || echo BUG: restricted path leaked
  $ sl mv parent public/moved-parent
  moving parent/public.txt to public/moved-parent/public.txt
  warning: results may be incomplete due to path ACLs
    'parent/restricted' is restricted by ACL 'some-acl'
  [1]
  $ cat public/moved-parent/public.txt
  visible (no-eol)
  $ test ! -e public/moved-parent/restricted/secret.txt || echo BUG: restricted path leaked

Graft and backout of commits that also touch restricted .slacl paths:

  $ newserver server_path_acl_graft_backout
  $ drawdag << 'EOS'
  > C B
  > |/
  > A
  >   # A/public.txt = public v1
  >   # A/restricted/.slacl = acl config
  >   # A/restricted/secret.txt = secret v1
  >   # B/public.txt = public v2
  >   # B/restricted/secret.txt = secret v2
  >   # C/other.txt = other
  >   # drawdag.defaultfiles=false
  > EOS

People without access can graft the visible changes after dropping the
restricted side.
  $ cd
  $ setconfig slacl.server-acl-enforcement=true
  $ newclientrepo client_path_acl_graft_no_access server_path_acl_graft_backout
  $ sl go -q $C
  warning: results may be incomplete due to path ACLs
    'restricted' is restricted by ACL 'some-acl'
  [1]
  $ sl graft $B
  pulling '*' from 'test:server_path_acl_graft_backout' (glob)
  grafting * "B" (glob)
  warning: results may be incomplete due to path ACLs
    'restricted' is restricted by ACL 'some-acl'
  [1]
  $ sl status
  $ sl cat public.txt
  public v2 (no-eol)
  $ test ! -e restricted/secret.txt || echo BUG: restricted path leaked

People without access can back out the visible changes without materializing
restricted files.
  $ cd
  $ setconfig slacl.server-acl-enforcement=true
  $ newclientrepo client_path_acl_backout_no_access server_path_acl_graft_backout
  $ sl go -q $B
  warning: results may be incomplete due to path ACLs
    'restricted' is restricted by ACL 'some-acl'
  [1]
  $ sl backout -r $B
  reverting public.txt
  changeset * backs out changeset * (glob)
  warning: results may be incomplete due to path ACLs
    'restricted' is restricted by ACL 'some-acl'
  [1]
  $ sl status
  $ sl cat public.txt
  public v1 (no-eol)
  $ test ! -e restricted/secret.txt || echo BUG: restricted path leaked

Subtree copy/merge with .slacl paths:

  $ cd
  $ setconfig subtree.allow-any-source-commit=True
  $ setconfig subtree.min-path-depth=1
  $ newserver server_subtree_acl
  $ drawdag << 'EOS'
  > B
  > |
  > A
  >   # A/restricted/.slacl = acl config
  >   # A/restricted/subdir/secret.txt = secret v1
  >   # A/public/file.txt = public
  >   # A/public/copied-subdir/secret.txt = secret v1
  >   # B/restricted/subdir/secret.txt = secret v2
  > EOS

Current behavior: users with access can use subtree copy and merge to copy
restricted data to unrestricted paths without warning.
  $ cd
  $ setconfig slacl.server-acl-enforcement=false
  $ newclientrepo client_subtree_acl_access server_subtree_acl
  $ sl go -q $A
  $ sl subtree copy --from-path restricted/subdir --to-path public/access-copied-subdir -m "copy restricted subdir"
  copying restricted/subdir to public/access-copied-subdir
  $ sl cat public/access-copied-subdir/secret.txt
  secret v1 (no-eol)
  $ sl subtree merge -r $B --from-path restricted/subdir --to-path public/access-copied-subdir
  pulling '57ca17e287bf46953d8daa9b20a45ce411862a87' from 'test:server_subtree_acl'
  searching for merge base ...
  found the last subtree copy commit a00c251d2596
  merge base: 2a5a3c105427
  1 files updated, 0 files merged, 0 files removed, 0 files unresolved
  (subtree merge, don't forget to commit)
  $ cat public/access-copied-subdir/secret.txt
  secret v2 (no-eol)

Users with access can use subtree copy within the restricted tree.
  $ cd
  $ setconfig slacl.server-acl-enforcement=false
  $ newclientrepo client_subtree_acl_access_self server_subtree_acl
  $ sl go -q $A
  $ sl subtree copy --from-path restricted/subdir --to-path restricted/copied-subdir -m "copy restricted subdir within restricted tree"
  copying restricted/subdir to restricted/copied-subdir
  $ sl cat restricted/copied-subdir/secret.txt
  secret v1 (no-eol)

Current behavior: subtree copy and merge can copy restricted .slacl data to
unrestricted paths for users without access.
  $ cd
  $ setconfig slacl.server-acl-enforcement=true
  $ newclientrepo client_subtree_acl_no_access server_subtree_acl
  $ sl go -q $A
  warning: results may be incomplete due to path ACLs
    'restricted' is restricted by ACL 'some-acl'
  [1]
  $ sl subtree copy --from-path restricted --to-path public/copied-restricted
  copying restricted to public/copied-restricted
  $ test ! -e public/copied-restricted/subdir/secret.txt || echo BUG: restricted path leaked
  BUG: restricted path leaked
  $ sl subtree merge -r $B --from-path restricted/subdir --to-path public/copied-subdir
  pulling '57ca17e287bf46953d8daa9b20a45ce411862a87' from 'test:server_subtree_acl'
  searching for merge base ...
  merge base: 2a5a3c105427
  1 files updated, 0 files merged, 0 files removed, 0 files unresolved
  (subtree merge, don't forget to commit)
  warning: results may be incomplete due to path ACLs
    'restricted' is restricted by ACL 'some-acl'
  [1]
  $ test "$(cat public/copied-subdir/secret.txt)" != "secret v2" || echo BUG: restricted merge leaked
  BUG: restricted merge leaked

Subtree copy filters a restricted child directory from child and parent sources,
but aborts when the source is a restricted file.
  $ cd
  $ newserver server_subtree_acl_restricted_child
  $ drawdag << 'EOS'
  > B
  > |
  > A  # A/parent/public.txt = visible
  >    # A/parent/restricted/.slacl = acl config
  >    # A/parent/restricted/secret.txt = secret
  >    # B/parent/public.txt = visible v2
  >    # B/parent/restricted/secret.txt = secret v2
  > EOS
  $ cd
  $ setconfig slacl.server-acl-enforcement=true
  $ newclientrepo client_subtree_acl_restricted_child_no_access server_subtree_acl_restricted_child
  $ sl go -q $A
  warning: results may be incomplete due to path ACLs
    'parent/restricted' is restricted by ACL 'some-acl'
  [1]
  $ sl subtree copy --from-path parent/restricted --to-path public/copied-restricted-child -m "copy restricted child"
  copying parent/restricted to public/copied-restricted-child
  warning: results may be incomplete due to path ACLs
    'parent/restricted' is restricted by ACL 'some-acl'
  [1]
  $ test ! -e public/copied-restricted-child/secret.txt || echo BUG: restricted path leaked

  $ cd
  $ setconfig slacl.server-acl-enforcement=true
  $ newclientrepo client_subtree_acl_restricted_file_no_access server_subtree_acl_restricted_child
  $ sl go -q $A
  warning: results may be incomplete due to path ACLs
    'parent/restricted' is restricted by ACL 'some-acl'
  [1]
  $ sl subtree copy --from-path parent/restricted/secret.txt --to-path public/copied-secret.txt -m "copy restricted file"
  abort: path 'parent/restricted' is restricted by ACL 'some-acl'
  [255]
  $ sl subtree copy --from-path parent/restricted/secret.txt --to-path public/copied-secret.txt -m "copy restricted file" --config slacl.server-denial-message='Ask the repo owners for access.'
  abort: path 'parent/restricted' is restricted by ACL 'some-acl'
  Ask the repo owners for access.
  [255]
  $ test ! -e public/copied-secret.txt || echo BUG: restricted path leaked

  $ cd
  $ setconfig slacl.server-acl-enforcement=true
  $ newclientrepo client_subtree_acl_parent_no_access server_subtree_acl_restricted_child
  $ sl go -q $A
  warning: results may be incomplete due to path ACLs
    'parent/restricted' is restricted by ACL 'some-acl'
  [1]
  $ sl subtree copy --from-path parent --to-path public/copied-parent -m "copy parent"
  copying parent to public/copied-parent
  warning: results may be incomplete due to path ACLs
    'parent/restricted' is restricted by ACL 'some-acl'
  [1]
  $ cat public/copied-parent/public.txt
  visible (no-eol)
  $ test ! -e public/copied-parent/restricted/secret.txt || echo BUG: restricted path leaked
  $ sl subtree merge -r $B --from-path parent --to-path public/copied-parent
  pulling 'fb7a80340b81d303bc6588780dbc64b9e8f98c7a' from 'test:server_subtree_acl_restricted_child'
  searching for merge base ...
  found the last subtree copy commit 8562d37d3e20
  merge base: 45e0dd9b6199
  1 files updated, 0 files merged, 0 files removed, 0 files unresolved
  (subtree merge, don't forget to commit)
  warning: results may be incomplete due to path ACLs
    'parent/restricted' [and 1 more] are restricted by ACL 'some-acl'
  [1]
  $ cat public/copied-parent/public.txt
  visible v2 (no-eol)
  $ test ! -e public/copied-parent/restricted/secret.txt || echo BUG: restricted merge leaked

  $ cd
  $ setconfig slacl.server-acl-enforcement=true
  $ newclientrepo client_subtree_acl_parent_graft_no_access server_subtree_acl_restricted_child
  $ sl go -q $A
  warning: results may be incomplete due to path ACLs
    'parent/restricted' is restricted by ACL 'some-acl'
  [1]
  $ sl subtree copy -r $A --from-path parent --to-path public/copied-parent -m "copy parent"
  copying parent to public/copied-parent
  warning: results may be incomplete due to path ACLs
    'parent/restricted' is restricted by ACL 'some-acl'
  [1]
  $ sl subtree graft -r $B --from-path parent --to-path public/copied-parent
  pulling '*' from 'test:server_subtree_acl_restricted_child' (glob)
  grafting * "B" (glob)
  warning: results may be incomplete due to path ACLs
    'parent/restricted'*restricted by ACL 'some-acl' (glob)
  [1]
  $ cat public/copied-parent/public.txt
  visible v2 (no-eol)
  $ test ! -e public/copied-parent/restricted/secret.txt || echo BUG: restricted graft leaked

Subtree merge filters a restricted file source from an unrestricted path.
  $ cd
  $ newserver server_subtree_acl_restricted_file_merge
  $ drawdag << 'EOS'
  > B
  > |
  > A  # A/parent/restricted/.slacl = acl config
  >    # A/parent/restricted/secret.txt = secret
  >    # A/public/copied-secret.txt = secret
  >    # B/parent/restricted/secret.txt = secret v2
  > EOS
  $ cd
  $ setconfig slacl.server-acl-enforcement=true
  $ newclientrepo client_subtree_acl_restricted_file_merge_no_access server_subtree_acl_restricted_file_merge
  $ sl go -q $A
  warning: results may be incomplete due to path ACLs
    'parent/restricted' is restricted by ACL 'some-acl'
  [1]
  $ sl subtree merge -r $B --from-path parent/restricted/secret.txt --to-path public/copied-secret.txt
  pulling '*' from 'test:server_subtree_acl_restricted_file_merge' (glob)
  abort: path 'parent/restricted' is restricted by ACL 'some-acl'
  [255]
  $ test "$(cat public/copied-secret.txt)" != "secret v2" || echo BUG: restricted file merge leaked

Diff stat from restricted to unrestricted currently aborts instead of filtering
the restricted side.

  $ newserver server3
  $ drawdag << 'EOS'
  > B  # B/public.txt = public v2
  >    # B/restricted/.slacl = acl config
  >    # B/restricted/secret.txt = secret v2
  > |
  > A  # A/public.txt = public v1
  >    # A/restricted/secret.txt = secret v1
  > EOS

  $ cd
  $ newclientrepo client3 server3
  $ sl go -q $A

  $ sl diff --stat -r $A -r $B
  pulling '9ca8bdfec14241f36077c849d46b93e38afd340c' from 'test:server3'
   A                     |  1 -
   B                     |  1 +
   public.txt            |  2 +-
   restricted/secret.txt |  1 -
   4 files changed, 2 insertions(+), 3 deletions(-)
  warning: results may be incomplete due to path ACLs
    'restricted' is restricted by ACL 'some-acl'
  [1]
  $ sl diff --stat -r $B -r $A
   A                     |  1 +
   B                     |  1 -
   public.txt            |  2 +-
   restricted/secret.txt |  1 +
   4 files changed, 3 insertions(+), 2 deletions(-)
  warning: results may be incomplete due to path ACLs
    'restricted' is restricted by ACL 'some-acl'
  [1]

Backout of a commit that touches restricted and unrestricted paths:

  $ newserver server4
  $ drawdag << 'EOS'
  > C
  > |
  > B
  > |
  > A
  >   # C/restricted/.slacl = acl config
  >   # B/public.txt = public v2
  >   # B/restricted/secret.txt = secret v2
  >   # A/public.txt = public v1
  >   # A/restricted/secret.txt = secret v1
  > EOS

  $ cd
  $ newclientrepo client4 server4
  $ sl go -q $C
  warning: results may be incomplete due to path ACLs
    'restricted' is restricted by ACL 'some-acl'
  [1]

  $ sl backout -r $B --config ui.interactive=True << 'EOF'
  > u
  > EOF
  other changed restricted/secret.txt which is restricted in local
  (d)elete/drop this file, input (m)oved path, or leave (u)nresolved? u
  1 files updated, 0 files merged, 1 files removed, 1 files unresolved
  use 'sl resolve' to retry unresolved file merges
  warning: results may be incomplete due to path ACLs
    'restricted' is restricted by ACL 'some-acl'
  [1]
  $ sl status
  M public.txt
  R B
  ! restricted/secret.txt
  $ sl resolve --tool internal:local restricted/secret.txt
  (no more unresolved files)
  warning: results may be incomplete due to path ACLs
    'restricted' is restricted by ACL 'some-acl'
  [1]
  $ sl cat public.txt
  public v2 (no-eol)

Restricted-path conflicts can be resolved by dropping the inaccessible path:

  $ cd
  $ newclientrepo client5 server4
  $ sl go -q $C
  warning: results may be incomplete due to path ACLs
    'restricted' is restricted by ACL 'some-acl'
  [1]
  $ sl backout -r $B --config ui.interactive=True << 'EOF'
  > d
  > EOF
  other changed restricted/secret.txt which is restricted in local
  (d)elete/drop this file, input (m)oved path, or leave (u)nresolved? d
  1 files updated, 1 files merged, 1 files removed, 0 files unresolved
  changeset * backs out changeset * (glob)
  warning: results may be incomplete due to path ACLs
    'restricted' is restricted by ACL 'some-acl'
  [1]
  $ sl status
  $ sl resolve -l
  $ sl cat public.txt
  public v1 (no-eol)

Restricted-path conflicts can be resolved by moving the file to a visible path:

  $ cd
  $ newclientrepo client6 server4
  $ sl go -q $C
  warning: results may be incomplete due to path ACLs
    'restricted' is restricted by ACL 'some-acl'
  [1]
  $ sl backout -r $B --config ui.interactive=True << 'EOF'
  > m
  > moved-secret.txt
  > EOF
  other changed restricted/secret.txt which is restricted in local
  (d)elete/drop this file, input (m)oved path, or leave (u)nresolved? m
  move path 'restricted/secret.txt' to [what path relative to repo root] ? moved-secret.txt
  1 files updated, 1 files merged, 1 files removed, 0 files unresolved
  changeset * backs out changeset * (glob)
  warning: results may be incomplete due to path ACLs
    'restricted' is restricted by ACL 'some-acl'
  [1]
  $ sl status
  $ sl cat moved-secret.txt
  secret v1 (no-eol)
  $ sl resolve -l

Backout of a commit that touches restricted paths the user never had access to:

  $ newserver server5
  $ drawdag << 'EOS'
  > B
  > |
  > A
  >   # A/public.txt = public v1
  >   # A/restricted/.slacl = acl config
  >   # A/restricted/secret.txt = secret v1
  >   # B/public.txt = public v2
  >   # B/restricted/secret.txt = secret v2
  > EOS

  $ cd
  $ newclientrepo client7 server5
  $ sl go -q $B
  warning: results may be incomplete due to path ACLs
    'restricted' is restricted by ACL 'some-acl'
  [1]

  $ sl backout -r $B
  removing B
  reverting public.txt
  changeset * backs out changeset * (glob)
  warning: results may be incomplete due to path ACLs
    'restricted' is restricted by ACL 'some-acl'
  [1]
  $ sl status
  $ sl cat public.txt
  public v1 (no-eol)

Amend of a commit that touches restricted paths the user never had access to:

  $ newserver server6
  $ drawdag << 'EOS'
  > B
  > |
  > A
  >   # A/public.txt = public v1
  >   # A/restricted/.slacl = acl config
  >   # A/restricted/secret.txt = secret v1
  >   # B/public.txt = public v2
  >   # B/restricted/secret.txt = secret v2
  > EOS

  $ cd
  $ newclientrepo client9 server6
  $ sl go -q $B
  warning: results may be incomplete due to path ACLs
    'restricted' is restricted by ACL 'some-acl'
  [1]
  $ echo public v3 > public.txt

Amend aborts before rewriting commits with restricted paths.
  $ sl amend
  abort: cannot rewrite commits with restricted paths
    'restricted' is restricted by ACL 'some-acl'
  (use '--config slacl.mixed-commit-mode=warn' to bypass)
  [255]

The `slacl.mixed-commit-mode=warn` bypass permits rewriting visible paths and
reports the restricted path warning.
  $ sl amend --config slacl.mixed-commit-mode=warn
  warning: rewriting commits with restricted paths (slacl.mixed-commit-mode=warn)
    'restricted' is restricted by ACL 'some-acl'
  warning: results may be incomplete due to path ACLs
    'restricted' is restricted by ACL 'some-acl'
  [1]
  $ sl status
  $ sl cat public.txt
  public v3

Fold of commits when one touches restricted paths the user never had access to:

  $ newserver server7
  $ drawdag << 'EOS'
  > C
  > |
  > B
  > |
  > A
  >   # A/public.txt = public v1
  >   # A/restricted/.slacl = acl config
  >   # A/restricted/secret.txt = secret v1
  >   # B/public.txt = public v2
  >   # B/restricted/secret.txt = secret v2
  >   # C/public.txt = public v3
  > EOS

  $ cd
  $ newclientrepo client10 server7
  $ sl go -q $C
  warning: results may be incomplete due to path ACLs
    'restricted' is restricted by ACL 'some-acl'
  [1]

Fold aborts before rewriting commits with restricted paths.
  $ sl fold --from .^ -m folded
  abort: cannot rewrite commits with restricted paths
    'restricted' is restricted by ACL 'some-acl'
  (use '--config slacl.mixed-commit-mode=warn' to bypass)
  [255]

The `slacl.mixed-commit-mode=warn` bypass permits folding visible paths and
reports the restricted path warning.
  $ sl fold --from .^ -m folded --config slacl.mixed-commit-mode=warn
  warning: rewriting commits with restricted paths (slacl.mixed-commit-mode=warn)
    'restricted' is restricted by ACL 'some-acl'
  2 changesets folded
  1 files updated, 0 files merged, 0 files removed, 0 files unresolved
  warning: results may be incomplete due to path ACLs
    'restricted' is restricted by ACL 'some-acl'
  [1]
  $ sl status
  $ sl cat public.txt
  public v3 (no-eol)

Histedit roll when one commit touches restricted paths the user never had access to:

  $ newserver server8
  $ drawdag << 'EOS'
  > C
  > |
  > B
  > |
  > A
  >   # A/public.txt = public v1
  >   # A/restricted/.slacl = acl config
  >   # A/restricted/secret.txt = secret v1
  >   # B/public.txt = public v2
  >   # B/restricted/secret.txt = secret v2
  >   # C/public.txt = public v3
  > EOS

  $ cd
  $ newclientrepo client11 server8
  $ sl go -q $C
  warning: results may be incomplete due to path ACLs
    'restricted' is restricted by ACL 'some-acl'
  [1]

Histedit roll aborts before rewriting commits with restricted paths.
  $ sl histedit $B --commands - << EOF
  > pick $B
  > roll $C
  > EOF
  abort: cannot rewrite commits with restricted paths
    'restricted' is restricted by ACL 'some-acl'
  (use '--config slacl.mixed-commit-mode=warn' to bypass)
  [255]

The `slacl.mixed-commit-mode=warn` bypass permits rolling visible paths and
reports the restricted path warning.
  $ sl histedit $B --config slacl.mixed-commit-mode=warn --commands - << EOF
  > pick $B
  > roll $C
  > EOF
  warning: rewriting commits with restricted paths (slacl.mixed-commit-mode=warn)
    'restricted' is restricted by ACL 'some-acl'
  folded 375b4a524931, 8c7b41a4bb4e -> 4579aeb1695c "B"
  warning: results may be incomplete due to path ACLs
    'restricted' is restricted by ACL 'some-acl'
  [1]
  $ sl status
  $ sl cat public.txt
  public v3 (no-eol)

Partial uncommit when preserving restricted paths the user never had access to:

  $ newserver server9
  $ drawdag << 'EOS'
  > B
  > |
  > A
  >   # A/public.txt = public v1
  >   # A/restricted/.slacl = acl config
  >   # A/restricted/secret.txt = secret v1
  >   # B/public.txt = public v2
  >   # B/restricted/secret.txt = secret v2
  > EOS

  $ cd
  $ newclientrepo client12 server9
  $ sl go -q $B
  warning: results may be incomplete due to path ACLs
    'restricted' is restricted by ACL 'some-acl'
  [1]

Partial uncommit aborts before rewriting commits with restricted paths.
  $ sl uncommit public.txt
  abort: cannot rewrite commits with restricted paths
    'restricted' is restricted by ACL 'some-acl'
  (use '--config slacl.mixed-commit-mode=warn' to bypass)
  [255]

The `slacl.mixed-commit-mode=warn` bypass permits uncommitting visible paths
and reports the restricted path warning.
  $ sl uncommit public.txt --config slacl.mixed-commit-mode=warn
  warning: rewriting commits with restricted paths (slacl.mixed-commit-mode=warn)
    'restricted' is restricted by ACL 'some-acl'
  warning: results may be incomplete due to path ACLs
    'restricted' is restricted by ACL 'some-acl'
  [1]
  $ sl status
  M public.txt
  $ sl cat public.txt
  public v1 (no-eol)
