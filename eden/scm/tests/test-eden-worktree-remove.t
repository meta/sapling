
#require eden

sections that inject edenfsctl failures switch to the subprocess removal path
explicitly; everything else exercises the default in-process path

setup backing repo with linked worktrees

  $ newclientrepo myrepo
  $ touch file.txt
  $ sl add file.txt
  $ sl commit -m "init"
  $ sl worktree add $TESTTMP/linked1
  created linked worktree at $TESTTMP/linked1
  $ sl worktree add $TESTTMP/linked2 --label "feature-x"
  created linked worktree at $TESTTMP/linked2
  $ sl worktree add $TESTTMP/linked_from_subdir
  created linked worktree at $TESTTMP/linked_from_subdir

test worktree remove - missing PATH argument

  $ sl worktree remove
  abort: usage: sl worktree remove PATH [PATH...]
  [255]

test worktree remove - cannot remove main with linked worktrees

  $ sl worktree remove $TESTTMP/myrepo -y
  abort: cannot remove a main worktree with linked worktrees
  [255]

test worktree remove - subdirectory path gives clear error

  $ mkdir -p $TESTTMP/myrepo/subdir
  $ cd $TESTTMP/myrepo/subdir
  $ sl worktree remove . -y
  abort: $TESTTMP/myrepo/subdir is not the root of checkout $TESTTMP/myrepo, not removing
  [255]
  $ cd $TESTTMP/myrepo

test worktree remove - basic remove cleans redirection backing storage with read-only files

#if no-windows
  $ eden redirect add --mount $TESTTMP/linked_from_subdir scratch symlink
  $ backing="$(readlink $TESTTMP/linked_from_subdir/scratch)"
  $ echo contents > "$backing/contents"
  $ chmod a-w "$backing/contents"
#endif
#if linux
  $ eden redirect add --mount $TESTTMP/linked_from_subdir bind1 bind
  $ eden redirect list --json --mount $TESTTMP/linked_from_subdir > $TESTTMP/redirections.json
  $ bind_backing1="$(python -c 'import json, sys; print(next(r["target"] for r in json.load(open(sys.argv[-1])) if r["repo_path"] == "bind1"))' $TESTTMP/redirections.json)"
  $ echo contents > $TESTTMP/linked_from_subdir/bind1/contents
  $ chmod a-w $TESTTMP/linked_from_subdir/bind1/contents
  $ test -f "$bind_backing1/contents"
#endif
  $ sl worktree remove $TESTTMP/linked_from_subdir -y --force
  Unmounting redirections... (no-windows !)
  Deleting redirection backing storage: *bind1... (glob) (linux !)
  Deleting redirection backing storage: *scratch... (glob) (no-windows !)
  Unmounting checkout...
  Deleting EdenFS checkout state...
  removed $TESTTMP/linked_from_subdir
  $ test -d $TESTTMP/linked_from_subdir
  [1]
#if no-windows
  $ test -e "$backing"
  [1]
  $ python -c 'import os, sys; print(os.path.exists(os.path.dirname(sys.argv[-1])))' "$backing"
  False
#endif
#if linux
  $ test -e "$bind_backing1"
  [1]
#endif

test worktree remove - list after remove shows fewer entries

  $ sl worktree list
    linked  $TESTTMP/linked1
    linked  $TESTTMP/linked2   feature-x
  * main    $TESTTMP/myrepo

test worktree remove - multiple paths in one command

  $ sl worktree remove $TESTTMP/linked1 $TESTTMP/linked2 -y
  Unmounting checkout...
  Deleting EdenFS checkout state...
  removed $TESTTMP/linked1
  Unmounting checkout...
  Deleting EdenFS checkout state...
  removed $TESTTMP/linked2
  $ test -d $TESTTMP/linked1
  [1]
  $ test -d $TESTTMP/linked2
  [1]
  $ sl worktree list
  this worktree is not part of a group

#if no-windows
test worktree remove - multiple paths preserves partial progress after eden remove failure

  $ cd $TESTTMP
  $ newclientrepo multipath_partial_repo
  $ touch file.txt
  $ sl add file.txt
  $ sl commit -m "init"
  $ sl worktree add $TESTTMP/multipath_ok
  created linked worktree at $TESTTMP/multipath_ok
  $ sl worktree add $TESTTMP/multipath_z_fail
  created linked worktree at $TESTTMP/multipath_z_fail
  $ original_eden_command="$(sl config edenfs.command)"
  $ cat > $TESTTMP/failing_eden_multipath <<EOF
  > #!/bin/sh
  > set -eu
  > original='$original_eden_command'
  > is_remove=0
  > last=
  > for arg in "\$@"; do
  >   if [ "\$arg" = "remove" ]; then
  >     is_remove=1
  >   fi
  >   last="\$arg"
  > done
  > if [ "\$is_remove" = "1" ] && [ "\$last" = "$TESTTMP/multipath_z_fail" ]; then
  >   echo "injected eden remove failure for \$last" >&2
  >   exit 1
  > fi
  > exec "\$original" "\$@"
  > EOF
  $ chmod +x $TESTTMP/failing_eden_multipath
  $ setconfig edenfs.command=$TESTTMP/failing_eden_multipath
  $ setconfig worktree.direct-eden-removal=false
  $ sl worktree remove $TESTTMP/multipath_ok $TESTTMP/multipath_z_fail -y
  removed $TESTTMP/multipath_ok
  failed to remove $TESTTMP/multipath_z_fail: eden remove failed for $TESTTMP/multipath_z_fail: injected eden remove failure for $TESTTMP/multipath_z_fail
  removed 1 of 2 worktrees; 1 failed
  abort: eden remove failed for $TESTTMP/multipath_z_fail: injected eden remove failure for $TESTTMP/multipath_z_fail
  [255]
  $ test -d $TESTTMP/multipath_ok
  [1]
  $ test -d $TESTTMP/multipath_z_fail
  $ sl worktree list
  * main    $TESTTMP/multipath_partial_repo
    linked  $TESTTMP/multipath_z_fail
  $ cd $TESTTMP/myrepo
#endif

test worktree remove - duplicate paths are deduplicated

  $ sl worktree add $TESTTMP/dedup1
  created linked worktree at $TESTTMP/dedup1
  $ sl worktree remove $TESTTMP/dedup1 $TESTTMP/dedup1 -y
  Unmounting checkout...
  Deleting EdenFS checkout state...
  removed $TESTTMP/dedup1
  $ test -d $TESTTMP/dedup1
  [1]

#if no-windows
test worktree remove - the invoking checkout is removed last
(Windows cannot delete a directory that is a process's working directory.)

  $ sl worktree add $TESTTMP/self_a
  created linked worktree at $TESTTMP/self_a
  $ sl worktree add $TESTTMP/self_b
  created linked worktree at $TESTTMP/self_b
  $ cd $TESTTMP/self_a
  $ sl worktree remove $TESTTMP/self_a $TESTTMP/self_b -y
  Unmounting checkout...
  Deleting EdenFS checkout state...
  removed $TESTTMP/self_b
  Unmounting checkout...
  Deleting EdenFS checkout state...
  removed $TESTTMP/self_a
  $ cd $TESTTMP/myrepo
  $ test -d $TESTTMP/self_a
  [1]
  $ test -d $TESTTMP/self_b
  [1]

test worktree remove - direct removal reports skipped backing cleanup

  $ sl worktree add $TESTTMP/direct_missing_config
  created linked worktree at $TESTTMP/direct_missing_config
  $ rm $TESTTMP/direct_missing_config/.eden/client/config.toml
  $ sl worktree remove $TESTTMP/direct_missing_config -y
  Skipping redirection backing cleanup for $TESTTMP/direct_missing_config because its checkout config is missing. APFS volumes left behind require manual cleanup. (osx !)
  Skipping redirection backing cleanup for $TESTTMP/direct_missing_config because its checkout config is missing. Run `eden du --clean-orphaned` to reclaim leftover backing storage. (no-osx !)
  Unmounting redirections...
  Error unmounting redirections for $TESTTMP/direct_missing_config: *config.toml does not exist* (glob)
  Unmounting checkout...
  Deleting EdenFS checkout state...
  removed $TESTTMP/direct_missing_config
  $ test -d $TESTTMP/direct_missing_config
  [1]
#endif

#if linux
test worktree remove - direct removal empties backing when a redirection can't be removed
(macOS removes the redirection symlink even though the checkout root is read-only.)

  $ sl worktree add $TESTTMP/direct_stuck_redirection
  created linked worktree at $TESTTMP/direct_stuck_redirection
  $ eden redirect add --mount $TESTTMP/direct_stuck_redirection scratch symlink
  $ backing="$(readlink $TESTTMP/direct_stuck_redirection/scratch)"
  $ mkdir "$backing/nested"
  $ echo contents > "$backing/nested/contents"
  $ chmod a-w "$backing/nested/contents"
  $ chmod a-w $TESTTMP/direct_stuck_redirection
  $ sl worktree remove $TESTTMP/direct_stuck_redirection -y --force > $TESTTMP/stuck-remove.log 2>&1
  $ python -c 'import pathlib, sys; log = pathlib.Path(sys.argv[-1]).read_text(); print("Error unmounting redirections for" in log and "failed to unmount redirection scratch" in log)' $TESTTMP/stuck-remove.log
  True
  $ test -d $TESTTMP/direct_stuck_redirection
  [1]
  $ test -d "$backing"
  $ python -c 'import os, sys; print(os.listdir(sys.argv[-1]))' "$backing"
  []
#endif

#if no-windows
test worktree remove - direct removal deletes read-only directories in backing storage

  $ sl worktree add $TESTTMP/direct_read_only_dirs
  created linked worktree at $TESTTMP/direct_read_only_dirs
  $ eden redirect add --mount $TESTTMP/direct_read_only_dirs scratch symlink
  $ backing="$(readlink $TESTTMP/direct_read_only_dirs/scratch)"
  $ mkdir "$backing/out"
  $ echo contents > "$backing/out/contents"
  $ chmod a-w "$backing/out"
  $ sl worktree remove $TESTTMP/direct_read_only_dirs -y --force
  Unmounting redirections...
  Deleting redirection backing storage: *scratch... (glob)
  Unmounting checkout...
  Deleting EdenFS checkout state...
  removed $TESTTMP/direct_read_only_dirs
  $ test -d $TESTTMP/direct_read_only_dirs
  [1]
  $ test -e "$backing"
  [1]
#endif

re-create linked worktrees for remaining tests

  $ sl worktree add $TESTTMP/linked1
  created linked worktree at $TESTTMP/linked1
  $ sl worktree add $TESTTMP/linked2 --label "feature-x"
  created linked worktree at $TESTTMP/linked2

test worktree remove --all

  $ sl worktree remove --all -y
  Unmounting checkout...
  Deleting EdenFS checkout state...
  removed $TESTTMP/linked1
  Unmounting checkout...
  Deleting EdenFS checkout state...
  removed $TESTTMP/linked2

test worktree remove - group dissolved after all linked removed

  $ sl worktree list
  this worktree is not part of a group

test worktree remove - names the target when this checkout has no linked worktrees

  $ sl worktree remove $TESTTMP/linked1 -y
  abort: cannot remove $TESTTMP/linked1: $TESTTMP/myrepo has no linked worktrees
  [255]

#if no-windows
test worktree remove --all preserves partial progress after eden remove failure

  $ cd $TESTTMP
  $ newclientrepo partial_progress_repo
  $ touch file.txt
  $ sl add file.txt
  $ sl commit -m "init"
  $ sl worktree add $TESTTMP/remove_all_fail
  created linked worktree at $TESTTMP/remove_all_fail
  $ sl worktree add $TESTTMP/remove_all_ok
  created linked worktree at $TESTTMP/remove_all_ok
  $ original_eden_command="$(sl config edenfs.command)"
  $ cat > $TESTTMP/failing_eden <<EOF
  > #!/bin/sh
  > set -eu
  > original='$original_eden_command'
  > is_remove=0
  > last=
  > for arg in "\$@"; do
  >   if [ "\$arg" = "remove" ]; then
  >     is_remove=1
  >   fi
  >   last="\$arg"
  > done
  > if [ "\$is_remove" = "1" ] && [ "\$last" = "$TESTTMP/remove_all_fail" ]; then
  >   echo "injected eden remove failure for \$last" >&2
  >   exit 1
  > fi
  > exec "\$original" "\$@"
  > EOF
  $ chmod +x $TESTTMP/failing_eden
  $ setconfig edenfs.command=$TESTTMP/failing_eden
  $ setconfig worktree.direct-eden-removal=false
  $ sl worktree remove --all -y
  failed to remove $TESTTMP/remove_all_fail: eden remove failed for $TESTTMP/remove_all_fail: injected eden remove failure for $TESTTMP/remove_all_fail
  removed $TESTTMP/remove_all_ok
  removed 1 of 2 worktrees; 1 failed
  abort: eden remove failed for $TESTTMP/remove_all_fail: injected eden remove failure for $TESTTMP/remove_all_fail
  [255]
  $ test -d $TESTTMP/remove_all_fail
  $ test -d $TESTTMP/remove_all_ok
  [1]
  $ sl worktree list
  * main    $TESTTMP/partial_progress_repo
    linked  $TESTTMP/remove_all_fail
#endif

test worktree remove - pre-worktree-remove hook fires with correct env vars

  $ cd $TESTTMP
  $ newclientrepo pre_hook_remove_repo
  $ touch file.txt
  $ sl add file.txt
  $ sl commit -m "init"
  $ sl worktree add $TESTTMP/pre_hook_rm1
  created linked worktree at $TESTTMP/pre_hook_rm1
#if windows
  $ setconfig hooks.pre-worktree-remove="echo PATH:%HG_PATH%"
  $ sl worktree remove $TESTTMP/pre_hook_rm1 -y
  PATH:$TESTTMP?pre_hook_rm1\r (esc) (glob)
  Unmounting checkout...
  Deleting EdenFS checkout state...
  removed $TESTTMP/pre_hook_rm1
#else
  $ setconfig hooks.pre-worktree-remove="echo PATH:\$HG_PATH"
  $ sl worktree remove $TESTTMP/pre_hook_rm1 -y
  PATH:$TESTTMP/pre_hook_rm1
  Unmounting checkout...
  Deleting EdenFS checkout state...
  removed $TESTTMP/pre_hook_rm1
#endif

test worktree remove - pre-worktree-remove hook failure is best effort for single remove

  $ sl worktree add $TESTTMP/pre_hook_blocked
  created linked worktree at $TESTTMP/pre_hook_blocked
#if windows
  $ setconfig "hooks.pre-worktree-remove=cmd /c exit 1"
#else
  $ setconfig hooks.pre-worktree-remove=false
#endif
  $ sl worktree remove $TESTTMP/pre_hook_blocked -y
  Unmounting checkout...
  Deleting EdenFS checkout state...
  removed $TESTTMP/pre_hook_blocked
  $ test -d $TESTTMP/pre_hook_blocked
  [1]

test worktree remove - pre-worktree-remove hook failure in --all mode still removes worktree

  $ sl worktree add $TESTTMP/pre_hook_all1
  created linked worktree at $TESTTMP/pre_hook_all1
#if windows
  $ setconfig "hooks.pre-worktree-remove=cmd /c exit 1"
#else
  $ setconfig hooks.pre-worktree-remove=false
#endif
  $ sl worktree remove --all -y
  Unmounting checkout...
  Deleting EdenFS checkout state...
  removed $TESTTMP/pre_hook_all1
  $ test -d $TESTTMP/pre_hook_all1
  [1]
  $ sl worktree list
  this worktree is not part of a group

test worktree remove - no prompt without a terminal

  $ sl worktree add $TESTTMP/no_tty
  created linked worktree at $TESTTMP/no_tty
  $ setconfig hooks.pre-worktree-remove="echo HOOK_FIRED"
  $ echo n | sl worktree remove $TESTTMP/no_tty
  HOOK_FIRED
  Unmounting checkout...
  Deleting EdenFS checkout state...
  removed $TESTTMP/no_tty
  $ test -d $TESTTMP/no_tty
  [1]

test worktree remove - modified or untracked files require --force

  $ sl worktree add $TESTTMP/dirty_modified
  created linked worktree at $TESTTMP/dirty_modified
  $ sl worktree add $TESTTMP/dirty_untracked
  created linked worktree at $TESTTMP/dirty_untracked
  $ echo modified > $TESTTMP/dirty_modified/file.txt
  $ touch $TESTTMP/dirty_untracked/untracked.txt
  $ sl worktree remove $TESTTMP/dirty_modified -y
  abort: $TESTTMP/dirty_modified contains modified or untracked files, use --force to remove it
  [255]
  $ sl worktree remove --all -y
  failed to remove $TESTTMP/dirty_modified: $TESTTMP/dirty_modified contains modified or untracked files, use --force to remove it
  failed to remove $TESTTMP/dirty_untracked: $TESTTMP/dirty_untracked contains modified or untracked files, use --force to remove it
  removed 0 of 2 worktrees; 2 failed
  abort: $TESTTMP/dirty_modified contains modified or untracked files, use --force to remove it
  [255]
  $ test -f $TESTTMP/dirty_untracked/untracked.txt

--force also skips the confirmation prompt

  $ sl worktree remove --all --force
  HOOK_FIRED
  Unmounting checkout...
  Deleting EdenFS checkout state...
  removed $TESTTMP/dirty_modified
  HOOK_FIRED
  Unmounting checkout...
  Deleting EdenFS checkout state...
  removed $TESTTMP/dirty_untracked

#if no-windows
test worktree remove - shelved changes require --force

  $ newclientrepo shelve_repo
  $ echo base > file.txt
  $ sl commit -Aqm base
  $ sl worktree add $TESTTMP/shelved
  created linked worktree at $TESTTMP/shelved
  $ echo modified > $TESTTMP/shelved/file.txt
  $ sl --cwd $TESTTMP/shelved shelve -q --config extensions.shelve=
  $ sl --cwd $TESTTMP/shelved status
  $ sl worktree remove $TESTTMP/shelved -y
  abort: $TESTTMP/shelved has shelved changes, use --force to remove it
  [255]
  $ sl worktree remove $TESTTMP/shelved --force
  Unmounting checkout...
  Deleting EdenFS checkout state...
  removed $TESTTMP/shelved

The check can be turned off.

  $ sl worktree add $TESTTMP/shelved
  created linked worktree at $TESTTMP/shelved
  $ echo modified > $TESTTMP/shelved/file.txt
  $ sl --cwd $TESTTMP/shelved shelve -q --config extensions.shelve=
  $ sl worktree remove $TESTTMP/shelved -y --config worktree.check-shelves-on-remove=false
  Unmounting checkout...
  Deleting EdenFS checkout state...
  removed $TESTTMP/shelved

A linked checkout whose dot dir flavour differs from the main checkout's is checked too.

  $ SL_REPO_IDENTITY=hg sl worktree add $TESTTMP/shelved_hg
  failed to write worktree-name marker for $TESTTMP/shelved_hg: failed to write worktree-name marker at $TESTTMP/shelved_hg/.sl/worktreename: failed to create file `$TESTTMP/shelved_hg/.sl/worktreename`: $ENOENT$
  created linked worktree at $TESTTMP/shelved_hg
  $ test -d $TESTTMP/shelved_hg/.hg && echo linked-is-hg
  linked-is-hg
  $ echo modified > $TESTTMP/shelved_hg/file.txt
  $ sl --cwd $TESTTMP/shelved_hg shelve -q --config extensions.shelve=
  $ sl worktree remove $TESTTMP/shelved_hg -y
  abort: $TESTTMP/shelved_hg has shelved changes, use --force to remove it
  [255]
  $ sl worktree remove $TESTTMP/shelved_hg --force
  Unmounting checkout...
  Deleting EdenFS checkout state...
  removed $TESTTMP/shelved_hg
#endif

test worktree remove - deletes a worktree whose files were read
(The layout is fbsource's eden/scm/lib/types/src/serde_with, which reproduced
the Windows failure when smaller layouts didn't.)

  $ cd $TESTTMP
  $ newclientrepo read_files_repo
  $ mkdir -p src/serde_with/hgid src/serde_with/sha256
  $ echo serde_with.rs > src/serde_with.rs
  $ echo serde_with/hgid.rs > src/serde_with/hgid.rs
  $ echo serde_with/hgid/bytes.rs > src/serde_with/hgid/bytes.rs
  $ echo serde_with/hgid/hex.rs > src/serde_with/hgid/hex.rs
  $ echo serde_with/hgid/tuple.rs > src/serde_with/hgid/tuple.rs
  $ echo serde_with/key.rs > src/serde_with/key.rs
  $ echo serde_with/nodeinfo.rs > src/serde_with/nodeinfo.rs
  $ echo serde_with/sha256.rs > src/serde_with/sha256.rs
  $ echo serde_with/sha256/bytes.rs > src/serde_with/sha256/bytes.rs
  $ echo serde_with/sha256/hex.rs > src/serde_with/sha256/hex.rs
  $ echo serde_with/sha256/tuple.rs > src/serde_with/sha256/tuple.rs
  $ sl commit -qAm init
  $ sl worktree add $TESTTMP/read_files
  created linked worktree at $TESTTMP/read_files
  $ ls $TESTTMP/read_files/src/serde_with $TESTTMP/read_files/src/serde_with/hgid $TESTTMP/read_files/src/serde_with/sha256 > /dev/null
  $ cat $TESTTMP/read_files/src/serde_with/hgid.rs $TESTTMP/read_files/src/serde_with/hgid/bytes.rs $TESTTMP/read_files/src/serde_with/hgid/hex.rs $TESTTMP/read_files/src/serde_with/hgid/tuple.rs $TESTTMP/read_files/src/serde_with/key.rs $TESTTMP/read_files/src/serde_with/nodeinfo.rs $TESTTMP/read_files/src/serde_with/sha256.rs $TESTTMP/read_files/src/serde_with/sha256/bytes.rs $TESTTMP/read_files/src/serde_with/sha256/hex.rs $TESTTMP/read_files/src/serde_with/sha256/tuple.rs > /dev/null
#if windows
FIXME: deleting the unmounted mount point fails on Windows once files in the
checkout have been listed and read, which leaves the folder behind and the
worktree listed. This should print "removed $TESTTMP/read_files" and delete the
folder, as on other platforms.
  $ sl worktree remove $TESTTMP/read_files -y
  Unmounting checkout...
  Deleting EdenFS checkout state...
  abort: direct EdenFS removal failed for *read_files: failed to remove mount point *read_files: The provider that supports file system virtualization is temporarily unavailable. (os error 369) (glob)
  [255]
  $ test -d $TESTTMP/read_files
  $ sl worktree list
    linked  $TESTTMP/read_files
  * main    $TESTTMP/read_files_repo
#else
  $ sl worktree remove $TESTTMP/read_files -y
  Unmounting checkout...
  Deleting EdenFS checkout state...
  removed $TESTTMP/read_files
  $ test -d $TESTTMP/read_files
  [1]
  $ sl worktree list
  this worktree is not part of a group
#endif
