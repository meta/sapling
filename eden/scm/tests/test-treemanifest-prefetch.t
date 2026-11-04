#modern-config-incompatible

#require no-eden


  $ CACHEDIR=`pwd`/hgcache

  $ . "$TESTDIR/library.sh"

  $ hginit master
  $ cd master
  $ cat >> .sl/config <<EOF
  > [remotefilelog]
  > server=True
  > EOF
  $ mkdir dir
  $ echo x > dir/x
  $ sl commit -qAm 'add x'
  $ mkdir subdir
  $ echo z > subdir/z
  $ sl commit -qAm 'add subdir/z'
  $ echo x >> dir/x
  $ sl commit -Am 'modify x'
  $ sl bookmark master
  $ cat >> .sl/config <<EOF
  > [remotefilelog]
  > server=True
  > name=master
  > cachepath=$CACHEDIR
  > 
  > [treemanifest]
  > server=True
  > sendtrees=True
  > EOF

  $ cd ..

  $ hgcloneshallow ssh://user@dummy/master client -q

  $ cd master

  $ cd ../client
  $ cat >> .sl/config <<EOF
  > [remotefilelog]
  > reponame = master
  > prefetchdays=0
  > cachepath = $CACHEDIR
  > EOF

Test prefetch
  $ sl prefetch -r 'desc("add x")' -r 'desc("add subdir/z")' -r 'desc("modify x")'
  1 files fetched over 1 fetches - (1 misses, 0.00% hit ratio) over * (glob) (?)

TODO(meyer): Fix debugindexedlogdatastore and debugindexedloghistorystore and add back output here.

Test prefetch with base node (subdir/ shouldn't show up in the pack)
  $ rm -rf $CACHEDIR/master

Multiple trees are fetched in this case because the file prefetching code path
requires tree manifest for the base commit.

  $ sl prefetch -r 'desc("modify x")' --base $(sl log -r 'desc("add subdir/z")' -T '{node}')
  1 files fetched over 1 fetches - (1 misses, 0.00% hit ratio) over * (glob) (?)
TODO(meyer): Fix debugindexedlogdatastore and debugindexedloghistorystore and add back output here.

Test prefetching when a draft commit is marked public
  $ mkdir $TESTTMP/cachedir.bak
  $ mv $CACHEDIR/* $TESTTMP/cachedir.bak

- Create a draft commit, and force it to be public
  $ sl prefetch -r .
  2 files fetched over 1 fetches - (2 misses, 0.00% hit ratio) over * (glob) (?)
  $ echo foo > foo
  $ sl commit -Aqm 'add foo'
  $ sl debugmakepublic -r .
  $ sl log -G -T '{phase} {manifest}'
  @  public c27238bca92c2e3a599ce9f27fe799ed0d79af82
  │
  o  public 22febde2554a1c6f8e4d8052a0501e3d895d73d9
  │
  o  public e445299a39f9006c2aec78dcc04dceeb102252b2
  │
  o  public 287ee6e53d4fbc5fab2157eb0383fdff1c3277c8
  
- Add remotenames for the remote heads
  $ sl pull -q

- Attempt to download the latest server commit. Verify there's no error about a
- missing manifest from the server.
  $ clearcache
  $ sl status --change 'desc("modify x")'
  M dir/x
  $ sl debugstrip -r 'desc("add foo")'
  0 files updated, 0 files merged, 1 files removed, 0 files unresolved

  $ clearcache
  $ mv $TESTTMP/cachedir.bak/* $CACHEDIR

Test auto prefetch during normal access
  $ rm -rf $CACHEDIR/master
|| ( exit 1 ) is needed because ls on OSX and Linux exits differently
  $ ls $CACHEDIR/master/packs/manifests || ( exit 1 )
  * $ENOENT$ (glob)
  [1]
  $ sl log -r tip --stat --pager=off
  2 files fetched over 1 fetches - (2 misses, 0.00% hit ratio) over * (glob) (?)
  commit:      311cac64787d
  bookmark:    public/0bcf7fbaf4e603953fe5af7ffc26b3568512046c
  bookmark:    remote/master
  hoistedname: master
  user:        test
  date:        Thu Jan 01 00:00:00 1970 +0000
  summary:     modify x
  
   dir/x |  1 +
   1 files changed, 1 insertions(+), 0 deletions(-)
TODO(meyer): Fix debugindexedlogdatastore and debugindexedloghistorystore and add back output here.
Test that auto prefetch scans up the changelog for base trees
  $ rm -rf $CACHEDIR/master
  $ sl prefetch -r 'tip^'
  2 files fetched over 1 fetches - (2 misses, 0.00% hit ratio) over * (glob) (?)
  $ rm -rf $CACHEDIR/master
  $ sl prefetch -r tip
  2 files fetched over 1 fetches - (2 misses, 0.00% hit ratio) over * (glob) (?)
- Only 2 of the 3 trees from tip^ are downloaded as part of --stat's fetch
  $ sl log -r tip --stat --pager=off > /dev/null
  1 files fetched over 1 fetches - (1 misses, 0.00% hit ratio) over * (glob) (?)

Test auto prefetch during pull

- Prefetch everything
  $ echo a >> a
  $ sl commit -Aqm 'draft commit that shouldnt affect prefetch'
  $ rm -rf $CACHEDIR/master
  $ sl pull --config treemanifest.pullprefetchcount=10 --traceback
  pulling from ssh://user@dummy/master
  prefetching trees for 3 commits
TODO(meyer): Fix debugindexedlogdatastore and debugindexedloghistorystore and add back output here.

  $ sl debugstrip -q -r 'draft()'

- Agents don't get the prefetch status line
  $ rm -rf $CACHEDIR/master
  $ CODING_AGENT_METADATA=id=test_agent sl pull --config treemanifest.pullprefetchcount=10 --config agent.hide-pull-prefetch-status=true
  pulling from ssh://user@dummy/master

- Prefetch just the top manifest (but the full one)
  $ rm -rf $CACHEDIR/master
  $ sl pull --config treemanifest.pullprefetchcount=1 --traceback
  pulling from ssh://user@dummy/master
  prefetching tree for 311cac64787d
TODO(meyer): Fix debugindexedlogdatastore and debugindexedloghistorystore and add back output here.

- Prefetch commit 1 then minimally prefetch commit 2
  $ rm -rf $CACHEDIR/master
  $ sl prefetch -r 'desc("add subdir/z")'
  2 files fetched over 1 fetches - (2 misses, 0.00% hit ratio) over * (glob) (?)
  $ sl pull --config treemanifest.pullprefetchcount=1 --traceback
  pulling from ssh://user@dummy/master
  prefetching tree for 311cac64787d
TODO(meyer): Fix debugindexedlogdatastore and debugindexedloghistorystore and add back output here.

Test prefetching certain revs during pull
  $ cd ../master
  $ echo x >> dir/x
  $ sl commit -qm "modify dir/x a third time"
  $ echo x >> dir/x
  $ sl commit -qm "modify dir/x a fourth time"

  $ cd ../client
  $ rm -rf $CACHEDIR/master
  $ sl pull --config treemanifest.pullprefetchrevs='tip~2'
  pulling from ssh://user@dummy/master
  imported commit graph for 2 commits (1 segment)
  prefetching tree for 311cac64787d
TODO(meyer): Fix debugindexedlogdatastore and debugindexedloghistorystore and add back output here.

- Test prefetching only the new tree parts for a commit who's parent tree is not
- downloaded already. Note that subdir/z was not downloaded this time.
  $ sl pull --config treemanifest.pullprefetchrevs='tip'
  pulling from ssh://user@dummy/master
  prefetching tree for 47bb1c5075af
TODO(meyer): Fix debugindexedlogdatastore and debugindexedloghistorystore and add back output here.

Test that prefetch refills just part of a tree when the cache is deleted

  $ echo >> dir/x
  $ sl commit -m 'edit x locally'
  1 files fetched over 1 fetches - (1 misses, 0.00% hit ratio) over * (glob) (?)
  $ rm -rf $CACHEDIR/master/*
  $ sl cat subdir/z
  z
  1 files fetched over 1 fetches - (1 misses, 0.00% hit ratio) over * (glob) (?)

Test prefetch non-parent commits with no base node (should fetch minimal
trees - in this case 3 trees for commit 2, and 2 for commit 4 despite it having
3 directories)
  $ rm -rf $CACHEDIR/master
  $ sl prefetch -r 'desc("modify x") + desc("modify dir/x a fourth time")'
  3 files fetched over 1 fetches - (3 misses, 0.00% hit ratio) over * (glob) (?)

Test prefetching with no options works. The expectation is to prefetch the stuff
required for working with the draft commits which happens to be only revision 5
in this case.

  $ rm -rf $CACHEDIR/master

The tree prefetching code path fetches no trees for revision 5. However, the
file prefetching code path fetches 1 file for revision 5 and while doing so,
also fetches 3 trees dealing with the tree manifest of the base revision 2.

  $ sl prefetch
  1 files fetched over 1 fetches - (1 misses, 0.00% hit ratio) over * (glob) (?)

Prefetching with treemanifest.ondemandfetch=True should fall back to normal
fetch is the server doesn't support it.

  $ rm -rf $CACHEDIR/master
  $ sl prefetch --config treemanifest.ondemandfetch=True

Running prefetch in the master repository should exit gracefully

  $ cd ../master
  $ sl prefetch
