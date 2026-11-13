#require git no-windows no-eden

  $ eagerepo
  $ . $TESTDIR/git.sh
  $ setconfig diff.git=true ui.allowemptycommit=true

Prepare a git repo:

  $ git init -q gitrepo
  $ cd gitrepo
  $ git config core.autocrlf false
  $ echo 1 > alpha
  $ git add alpha
  $ git commit -q -malpha

  $ echo 2 > beta
  $ git add beta
  $ git commit -q -mbeta

Init an sl repo using the git changelog backend:

  $ cd $TESTTMP
  $ sl debuginitgit --git-dir gitrepo/.git repo1
  $ cd repo1

  $ sl log -Gr 'all()' -T '{node} {desc}'
  o  3f5848713286c67b8a71a450e98c7fa66787bde2 beta
  │
  o  b6c31add3e60ded7a9c9c803641edffb1dccd251 alpha
  
  $ sl debugchangelog
  The changelog is backed by Rust. More backend information:
  Backend (segmented git):
    Local:
      Segments + IdMap: $TESTTMP/repo1/.sl/store/segments/v1
      Git: $TESTTMP/gitrepo/.git
  Feature Providers:
    Commit Graph Algorithms:
      Segments
    Commit Hash / Rev Lookup:
      IdMap
    Commit Data (user, message):
      Git

Test log with --exclude:

  $ sl log --exclude yaml -T '{node} {desc}' -G
  o  3f5848713286c67b8a71a450e98c7fa66787bde2 beta
  │
  o  b6c31add3e60ded7a9c9c803641edffb1dccd251 alpha
  

Make sure tests aren't sensitive to the system git overrides file.
  $ LOG=config=info sl root 2>&1 | grep git_overrides.rc || true

Test checkout:

  $ sl up tip
  2 files updated, 0 files merged, 0 files removed, 0 files unresolved
  $ echo *
  alpha beta
  $ cat beta
  2

Test nullid:

  $ sl log -r null -T '{desc}'

Test non-existed commit hash:

  $ sl log -r deadbeef00000000000000000000000000000000 -T '{desc}'
  abort: unknown revision 'deadbeef00000000000000000000000000000000'!
  [255]

Test diff:

  $ sl log -r tip -p
  commit:      3f5848713286
  bookmark:    master
  user:        test <test@example.org>
  date:        Mon Jan 01 00:00:10 2007 +0000
  summary:     beta
  
  diff --git a/beta b/beta
  new file mode 100644
  --- /dev/null
  +++ b/beta
  @@ -0,0 +1,1 @@
  +2
  
Test status:

  $ sl status
  $ echo 3 > alpha
  $ sl status
  M alpha

Test commit:

  $ sl commit -m alpha3 -d '2001-02-03T14:56:01 +0800'
  $ sl log -Gr: -T '{desc}'
  @  alpha3
  │
  o  beta
  │
  o  alpha
  
Test log FILE:

  $ sl log -G -T '{desc}' alpha
  @  alpha3
  ╷
  o  alpha
  
Test file history via 'parents FILE':

  $ sl parents -T '{desc}\n' alpha
  alpha3

  $ sl parents -T '{desc}\n' alpha -r 'desc(beta)'
  alpha

Test log FILE with patches:

  $ sl log -p -G -T '{desc}\n' alpha
  @  alpha3
  ╷  diff --git a/alpha b/alpha
  ╷  --- a/alpha
  ╷  +++ b/alpha
  ╷  @@ -1,1 +1,1 @@
  ╷  -1
  ╷  +3
  ╷
  o  alpha
     diff --git a/alpha b/alpha
     new file mode 100644
     --- /dev/null
     +++ b/alpha
     @@ -0,0 +1,1 @@
     +1
  

Test bookmarks:

  $ sl bookmark -r. foo
  $ sl bookmarks
     foo                       57eda5013e06
     master                    3f5848713286

Test changes are readable via git:

  $ export GIT_DIR="$TESTTMP/gitrepo/.git"
  $ git log foo --pretty='format:%s %an %d'
  alpha3 test  *foo) (glob)
  beta test  (HEAD -> master)
  alpha test  (no-eol)
  $ git fsck --strict
  $ git show foo
  commit 57eda5013e068ac543a52ad073cec3d7750113b5
  Author: test <>
  Date:   Sat Feb 3 14:56:01 2001 +0800
  
      alpha3
  
  diff --git a/alpha b/alpha
  index d00491f..00750ed 100644
  --- a/alpha
  +++ b/alpha
  @@ -1 +1 @@
  -1
  +3

Exercise pathcopies code path:

  $ sl diff -r '.^^' -r .
  diff --git a/alpha b/alpha
  --- a/alpha
  +++ b/alpha
  @@ -1,1 +1,1 @@
  -1
  +3
  diff --git a/beta b/beta
  new file mode 100644
  --- /dev/null
  +++ b/beta
  @@ -0,0 +1,1 @@
  +2

Prepare a new git "client" repo:

  $ unset GIT_DIR
  $ git init -q --bare $TESTTMP/gitrepo2
  $ cd "$TESTTMP/gitrepo2"
  $ git remote add origin "$TESTTMP/gitrepo/.git"
  $ sl debuginitgit --git-dir="$TESTTMP/gitrepo2" "$TESTTMP/repo2"
  $ cd "$TESTTMP/repo2"

Test pull:

  $ sl paths -a origin "file://$TESTTMP/gitrepo/.git"

- tree prefetch config is ignored

  $ setconfig treemanifest.pullprefetchrevs=tip

- pull with -B
  $ sl pull origin -B foo
  pulling from file:/*/$TESTTMP/gitrepo/.git (glob)
  From file:/*/$TESTTMP/gitrepo/ (glob)
   * [new ref]         57eda5013e068ac543a52ad073cec3d7750113b5 -> origin/foo
  $ sl log -r origin/foo -T '{desc}\n'
  alpha3

- pull with -B and --update
  $ sl pull -q origin -B master --update
  $ sl log -r . -T '{remotenames}\n'
  origin/master

  $ sl pull -q origin -B foo --update
  $ sl log -r . -T '{remotenames}\n'
  origin/foo

- pull with -B and --update with wrong tweakdefaults internalconfig configuration
  $ cat > "$TESTTMP/buggy.rc" << EOF
  > [tweakdefaults]
  > defaultdest=nonexisted
  > EOF
  $ HG_TEST_INTERNALCONFIG="$TESTTMP/buggy.rc" sl pull origin -B master --update --config extensions.tweakdefaults=
  pulling from file:/*/$TESTTMP/gitrepo/.git (glob)
  1 files updated, 0 files merged, 0 files removed, 0 files unresolved

- pull without arguments
  $ sl paths -a default "file://$TESTTMP/gitrepo/.git"
  $ sl pull
  pulling from file:/*/$TESTTMP/gitrepo/.git (glob)

Test error message display:

  $ mkdir $TESTTMP/errortest
  $ cd $TESTTMP/errortest
  $ sl clone --git "$TESTTMP/nonexisted"
  abort: git command failed with exit code 128
    git * (glob)
      fatal: '$TESTTMP/nonexisted' does not appear to be a git repository
      fatal: Could not read from remote repository.
  
      Please make sure you have the correct access rights
      and the repository exists.
  [255]

Test clone with flags (--noupdate, --updaterev):

  $ mkdir $TESTTMP/clonetest
  $ cd $TESTTMP/clonetest

  $ sl clone -q --noupdate --git "$TESTTMP/gitrepo"
  $ cd gitrepo
  $ sl log -r . -T '{node|short}\n'
  000000000000
  $ sl bookmarks --list-subscriptions
     remote/master             3f5848713286
  $ cd ..

  $ sl clone --git "$TESTTMP/gitrepo" -u foo cloned1
  From $TESTTMP/gitrepo
   * [new ref]         3f5848713286c67b8a71a450e98c7fa66787bde2 -> remote/master
   * [new ref]         57eda5013e068ac543a52ad073cec3d7750113b5 -> remote/foo
  2 files updated, 0 files merged, 0 files removed, 0 files unresolved
  $ sl --cwd cloned1 log -r . -T '{node|short} {remotenames} {desc}\n'
  57eda5013e06 remote/foo alpha3

  $ sl clone --updaterev foo --git "$TESTTMP/gitrepo" cloned2
  From $TESTTMP/gitrepo
   * [new ref]         3f5848713286c67b8a71a450e98c7fa66787bde2 -> remote/master
   * [new ref]         57eda5013e068ac543a52ad073cec3d7750113b5 -> remote/foo
  2 files updated, 0 files merged, 0 files removed, 0 files unresolved
  $ sl --cwd cloned2 log -r . -T '{node|short} {remotenames} {desc}\n'
  57eda5013e06 remote/foo alpha3

  $ NODE=$(git --git-dir ~/gitrepo/.git for-each-ref | grep visibleheads | sed 's# .*##')
  $ sl clone --updaterev $NODE --git "$TESTTMP/gitrepo" cloned3
  From $TESTTMP/gitrepo
   * [new ref]         3f5848713286c67b8a71a450e98c7fa66787bde2 -> remote/master
   * [new ref]         57eda5013e068ac543a52ad073cec3d7750113b5 -> refs/visibleheads/57eda5013e068ac543a52ad073cec3d7750113b5
  2 files updated, 0 files merged, 0 files removed, 0 files unresolved

Test clone using scp-like path:

#if execbit no-windows
  $ cat > "$TESTTMP/ssh.sh" << 'EOF'
  > #!/bin/sh
  > # $GIT_SSH host "git-upload-pack '/path/to/repo'"
  > ssh() {
  >   local cmd=$1
  >   local path=$(eval echo $2)  # unquote '/path' to /path
  >   $cmd "$TESTTMP$path"
  > }
  > ssh $2
  > EOF
  $ chmod +x "$TESTTMP/ssh.sh"
  $ GIT_SSH="$TESTTMP/ssh.sh" sl clone localhost:gitrepo2 cloned-gitrepo2
  $ sl status --cwd cloned-gitrepo2
  $ sl paths --cwd cloned-gitrepo2
  default = ssh://localhost/gitrepo2
#endif

Test clone when destination is a file:

  $ touch "$TESTTMP/already_exists"
  $ sl clone -q --git "$TESTTMP/gitrepo" "$TESTTMP/already_exists"
  abort: destination '$TESTTMP/already_exists' exists and is not a directory
  [255]

Test clone when folder is not empty:

  $ mkdir "$TESTTMP/not_quite_empty"
  $ touch "$TESTTMP/not_quite_empty/some_file"
  $ sl clone -q --git "$TESTTMP/gitrepo" "$TESTTMP/not_quite_empty"
  abort: destination '$TESTTMP/not_quite_empty' is not empty
  [255]

Test clone into the current folder, if empty:

  $ mkdir "$TESTTMP/empty_folder"
  $ cd "$TESTTMP/empty_folder"
  $ sl clone -q --git "$TESTTMP/gitrepo" .
  $ ls | sort
  alpha
  beta

Test init without URL on existing repo:

  $ sl init --git .
  abort: repository `$TESTTMP/empty_folder` already exists
  [255]
Make sure we didn't delete the folder contents:
  $ ls | sort
  alpha
  beta

Make sure we clean up if repo init fails:

  $ FAILPOINTS=repo-init=panic sl clone -q --git "$TESTTMP/gitrepo" "$TESTTMP/init_fails" 2> /dev/null
  [1]
  $ test -d "$TESTTMP/init_fails"
  [1]

Test push:

  $ cd "$TESTTMP/clonetest/cloned1"
  $ echo 3 > beta
  $ sl commit -m 'beta.change'

- --to without -r
  $ sl push -q --to book_change_beta
  $ sl push -q --to remote/book_change_beta1
  $ sl push -q remote/book_change_beta2

- --to with -r
  $ sl push -r '.^' --to parent_change_beta
  To $TESTTMP/gitrepo
   * [new branch]      57eda5013e068ac543a52ad073cec3d7750113b5 -> parent_change_beta

  $ sl log -r '.^+.' -T '{desc} {remotenames}\n'
  alpha3 remote/foo remote/parent_change_beta
  beta.change remote/book_change_beta remote/book_change_beta1 remote/book_change_beta2

- delete bookmark
  $ sl push --delete book_change_beta
  To $TESTTMP/gitrepo
   - [deleted]         book_change_beta

  $ sl push -q --delete remote/book_change_beta1
  $ sl push -q --delete default/book_change_beta2

  $ sl log -r '.^+.' -T '{desc} {remotenames}\n'
  alpha3 remote/foo remote/parent_change_beta
  beta.change 

- push with --force

  $ cd "$TESTTMP"
  $ git init -qb main --bare "pushforce.git"
  $ sl clone --git "$TESTTMP/pushforce.git"
  $ cd pushforce
  $ git --git-dir=.sl/store/git config advice.pushUpdateRejected false

  $ drawdag << 'EOS'
  > B C
  > |/
  > A
  > EOS

  $ sl push -qr $B --to foo
  $ sl push -qr $C --to foo
  To $TESTTMP/pushforce.git
   ! [rejected]        5d38a953d58b0c80a4416ba62e62d3f2985a3726 -> foo (non-fast-forward)
  error: failed to push some refs to '$TESTTMP/pushforce.git'
  [1]
  $ sl push -qr $C --to foo --force

- push without --to

  $ cd "$TESTTMP"
  $ git init -qb main --bare "pushto.git"
  $ sl clone --git "$TESTTMP/pushto.git"
  $ cd pushto

  $ drawdag << 'EOS'
  > B
  > |
  > A
  > EOS

  $ sl push -qr $A --to stable
  $ sl push -qr $B --to main
  $ sl up -q $B
  $ sl commit -m C

 (pick "main" automatically)
  $ sl push
  To $TESTTMP/pushto.git
     0de3093..a9d5bd6  a9d5bd6ac8bcf89de9cd99fd215cca243e8aeed9 -> main
  $ sl push -q --to stable

 (nothing to push - already at a remote bookmark)
  $ sl push
  abort: nothing to push - current commit is already at remote/main, remote/stable
  [255]

 (cannot pick with multiple candidates)
  $ sl commit -m D
  $ sl push
  abort: use '--to' to specify destination bookmark
  [255]

"files" metadata:

  $ sl log -r $A+$B -T '{files}\n'
  A
  B

Submodule does not cause a crash:

  $ cd
  $ git init -q submod
  $ cd submod

  $ git submodule --quiet add ../gitrepo b
  $ echo 1 > a
  $ echo 2 > c
  $ git add a c
  $ git commit --quiet -m s

- checkout silently ignores the submodule

  $ cd
  $ setconfig git.submodules=false
  $ sl clone --git "$TESTTMP/submod" cloned-submod
  From $TESTTMP/submod
   * [new ref]         a4c97159e197fb3aaab3f24fc3b39d7942b311ff -> remote/master
  3 files updated, 0 files merged, 0 files removed, 0 files unresolved
  $ cd cloned-submod
  $ echo *
  a c

- changing the tree does not lose submodule

  $ touch d
  $ sl commit -m d -A d
  $ sl book changed
  $ git --git-dir=.sl/store/git cat-file -p changed:
  100644 blob 703feeadc77c10eeec4dfe76ae58506b6a77ab11	.gitmodules
  100644 blob d00491fd7e5bb6fa28c517a0bb32b8b506539d4d	a
  160000 commit 3f5848713286c67b8a71a450e98c7fa66787bde2	b
  100644 blob 0cfbf08886fca9a91cb753ec8734c84fcbe52c9f	c
  100644 blob e69de29bb2d1d6434b8b29ae775ad8c2e48c5391	d

Tags are ignored during clone and pull:

  $ cd
  $ git init -b main -q gittag
  $ cd gittag
  $ echo 1 > a
  $ git add a
  $ git commit -q -m a
  $ git tag v1

  $ cd
  $ sl clone -q git+file://$TESTTMP/gittag cloned-gittag
  $ cd cloned-gittag
  $ sl pull -q
  $ sl bookmarks
  no bookmarks set
  $ sl bookmarks --list-subscriptions
     remote/main               379d702a285c
  $ git --git-dir=.sl/store/git for-each-ref
  379d702a285c1e34e6365cc347249ec73bcd6b40 commit	refs/remotes/remote/main

Cloud sync does not crash:

  $ enable commitcloud
  $ sl cloud sync
  abort: commitcloud: workspace error: undefined workspace
  (your repo is not connected to any workspace)
  (use 'sl cloud join --help' for more details)
  [255]

Init with --git:

  $ cd
  $ sl init --git init-git
  $ cd init-git
  $ [ -d $TESTTMP/init-git/.sl/store/git ]
  $ sl log
  $ sl status

Rebase merging conflicts

  $ cd
  $ sl init --git rebase
  $ cd rebase
  $ enable rebase
  $ drawdag << 'EOS'
  > B C  # A/f=1\n2\n3\n
  > |/   # B/f=1\n1.5\n2\n3\n
  > A    # C/f=1\n2\n2.5\n3\n
  > EOS
  $ sl rebase -r $B -d $C
  rebasing e03992db70e4 "B"
  merging f

Rebasing an add conflict
  $ cd
  $ sl init --git rebase-add
  $ cd rebase-add
  $ enable rebase
  $ touch base
  $ sl commit -Aqm base
  $ echo 2 > file1
  $ sl commit -Aqm file1.a
  $ sl book dest
  $ sl up .^
  0 files updated, 0 files merged, 1 files removed, 0 files unresolved
  (leaving bookmark dest)
  $ echo 1 > file1
  $ sl commit -Aqm file1.b
  $ sl rebase -d dest
  rebasing ce2d49965394 "file1.b"
  merging file1
  warning: 1 conflicts while merging file1! (edit, then use 'sl resolve --mark')
  unresolved conflicts (see sl resolve, then sl rebase --continue)
  [1]

Test amend:

  $ cd
  $ sl init --git amend
  $ cd amend
  $ enable amend
  $ echo 1 > file
  $ sl commit -Aqm 'one'
  $ echo 2 > file
  $ sl commit -Aqm 'base'
  $ echo 3 > file
# This used to trigger a bug.
  $ HGEDITOR=cat sl commit --amend --config committemplate.changeset='{diff()}'
  diff --git a/file b/file
  --- a/file
  +++ b/file
  @@ -1,1 +1,1 @@
  -1
  +3

Init with --git works without a reponame:

  $ cd
  $ grep -v reponame $HGRCPATH > $TESTTMP/config-no-reponame
  $ HGRCPATH=$TESTTMP/config-no-reponame sl init --git init-git-no-reponame

Can fetch remote refs:

  $ cd
  $ git init -b first-branch -q remote-refs
  $ cd remote-refs
  $ echo 1 > a
  $ git add a
  $ git commit -q -m a
  $ git tag v1
  $ git checkout -qb second-branch
  $ echo 2 >> a
  $ git commit -aq -m b
  $ git tag v2

  $ cd
  $ git clone -q remote-refs remote-refs2
  $ cd remote-refs2
  $ git branch other-remote-branch

  $ cd
  $ sl clone -q git+file://$TESTTMP/remote-refs cloned-remote-refs
  $ cd cloned-remote-refs
  $ sl paths --add banana file://$TESTTMP/remote-refs2
  $ sl bookmarks --remote
     remote/first-branch              379d702a285c1e34e6365cc347249ec73bcd6b40
     remote/second-branch             c828c570a4109d85a6cee02b8bd2bdf355faf969
  $ sl bookmarks --remote branches
     remote/first-branch              379d702a285c1e34e6365cc347249ec73bcd6b40
     remote/second-branch             c828c570a4109d85a6cee02b8bd2bdf355faf969
  $ sl bookmarks --remote tags
     remote/v1                        379d702a285c1e34e6365cc347249ec73bcd6b40
     remote/v2                        c828c570a4109d85a6cee02b8bd2bdf355faf969
  $ sl bookmarks --remote branches tags
     remote/first-branch              379d702a285c1e34e6365cc347249ec73bcd6b40
     remote/second-branch             c828c570a4109d85a6cee02b8bd2bdf355faf969
     remote/v1                        379d702a285c1e34e6365cc347249ec73bcd6b40
     remote/v2                        c828c570a4109d85a6cee02b8bd2bdf355faf969
  $ sl bookmarks --remote 'refs/heads/*'
     remote/refs/heads/first-branch   379d702a285c1e34e6365cc347249ec73bcd6b40
     remote/refs/heads/second-branch  c828c570a4109d85a6cee02b8bd2bdf355faf969
  $ sl bookmarks --remote --remote-path banana
     banana/other-remote-branch       c828c570a4109d85a6cee02b8bd2bdf355faf969
     banana/second-branch             c828c570a4109d85a6cee02b8bd2bdf355faf969
