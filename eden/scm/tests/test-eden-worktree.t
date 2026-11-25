
#require eden

# ============================================================
# End-to-end lifecycle
# ============================================================

test full lifecycle

  $ newclientrepo e2e_repo
  $ touch file.txt && sl add file.txt && sl commit -m "init"

  $ sl worktree add $TESTTMP/e2e_linked1 --label "dev"
  created linked worktree at $TESTTMP/e2e_linked1
  $ sl worktree add $TESTTMP/e2e_linked2 --label "staging"
  created linked worktree at $TESTTMP/e2e_linked2

  $ sl worktree list
    linked  $TESTTMP/e2e_linked1   dev
    linked  $TESTTMP/e2e_linked2   staging
  * main    $TESTTMP/e2e_repo

  $ sl worktree label $TESTTMP/e2e_linked1 "dev-v2"
  label set for $TESTTMP/e2e_linked1

  $ sl worktree list
    linked  $TESTTMP/e2e_linked1   dev-v2
    linked  $TESTTMP/e2e_linked2   staging
  * main    $TESTTMP/e2e_repo

  $ sl worktree remove $TESTTMP/e2e_linked1 -y
  Unmounting checkout...
  Deleting EdenFS checkout state...
  removed $TESTTMP/e2e_linked1

  $ sl worktree list
    linked  $TESTTMP/e2e_linked2   staging
  * main    $TESTTMP/e2e_repo

  $ sl worktree label $TESTTMP/e2e_linked2 --remove
  label removed for $TESTTMP/e2e_linked2

  $ sl worktree list
    linked  $TESTTMP/e2e_linked2
  * main    $TESTTMP/e2e_repo

  $ sl worktree remove --all -y
  Unmounting checkout...
  Deleting EdenFS checkout state...
  removed $TESTTMP/e2e_linked2

  $ sl worktree list
  this worktree is not part of a group

# ============================================================
# Auto-cleanup
# ============================================================

test auto-cleanup: one missing linked worktree

  $ sl worktree add $TESTTMP/cleanup_wt1
  created linked worktree at $TESTTMP/cleanup_wt1
  $ sl worktree add $TESTTMP/cleanup_wt2
  created linked worktree at $TESTTMP/cleanup_wt2

  $ sl worktree list
    linked  $TESTTMP/cleanup_wt1
    linked  $TESTTMP/cleanup_wt2
  * main    $TESTTMP/e2e_repo

externally remove one linked worktree (eden remove, not sl worktree remove)

  $ EDENFSCTL_ONLY_RUST=true eden rm -y $TESTTMP/cleanup_wt1 > /dev/null 2>&1

  $ sl worktree list
    linked  $TESTTMP/cleanup_wt2
  * main    $TESTTMP/e2e_repo

test auto-cleanup: all linked worktrees missing - group dissolved

  $ EDENFSCTL_ONLY_RUST=true eden rm -y $TESTTMP/cleanup_wt2 > /dev/null 2>&1

  $ sl worktree list
  this worktree is not part of a group

  $ sl worktree list
  this worktree is not part of a group

#if unix-permissions no-root
test auto-cleanup: keeps a linked worktree whose path can't be checked

  $ mkdir $TESTTMP/unreadable
  $ sl worktree add $TESTTMP/unreadable/wt
  created linked worktree at $TESTTMP/unreadable/wt
  $ chmod 000 $TESTTMP/unreadable
  $ sl worktree list
  * main    $TESTTMP/e2e_repo
    linked  $TESTTMP/unreadable/wt
  $ chmod 755 $TESTTMP/unreadable
#endif

test corner case: eden rm removes worktree, then sl worktree remove cleans registry

  $ cd $TESTTMP
  $ newclientrepo orphan_repo
  $ touch file.txt
  $ sl add file.txt
  $ sl commit -m "init"
  $ sl worktree add $TESTTMP/orphan_wt
  created linked worktree at $TESTTMP/orphan_wt

eden rm removes the checkout but leaves the registry entry

  $ EDENFSCTL_ONLY_RUST=true eden rm -y $TESTTMP/orphan_wt > /dev/null 2>&1
  $ test -d $TESTTMP/orphan_wt
  [1]

sl worktree remove should clean the registry for the already-removed checkout

  $ sl worktree remove $TESTTMP/orphan_wt -y
  eden checkout $TESTTMP/orphan_wt not found on disk, continuing to remove from registry
  removed $TESTTMP/orphan_wt

  $ sl worktree list
  this worktree is not part of a group

sl worktree add to the same path should succeed after the registry is cleaned

  $ sl worktree add $TESTTMP/orphan_wt
  created linked worktree at $TESTTMP/orphan_wt

  $ sl worktree remove $TESTTMP/orphan_wt -y
  Unmounting checkout...
  Deleting EdenFS checkout state...
  removed $TESTTMP/orphan_wt
