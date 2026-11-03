
#require no-eden


  $ eagerepo
  $ configure dummyssh
  $ enable amend rebase histedit fbhistedit fbcodereview absorb
  $ setconfig ui.interactive=true
  $ setconfig experimental.evolution=obsolete
  $ setconfig visibility.enabled=true
  $ setconfig mutation.record=true mutation.enabled=true
  $ setconfig commit.modify-obsolete-mode=ignore

  $ cat >> $HGRCPATH <<EOF
  > [templatealias]
  > mutation_nodes = "{join(mutations % '(Rewritten using {operation} into {join(successors % \'{node|short}\', \', \')})', ' ')}"
  > mutation_descs = "{join(mutations % '(Rewritten using {operation} into {join(successors % \'{desc|firstline}\', \', \')})', ' ')}"
  > EOF
  $ newrepo
  $ echo "base" > base
  $ sl commit -Aqm base
  $ echo "1" > file
  $ sl commit -Aqm c1

Amend

  $ for i in 2 3 4 5 6 7 8
  > do
  >   echo $i >> file
  >   sl amend -m "c1 (amended $i)"
  > done
  $ sl debugmutation
   *  cc809964b02448cb4c84c772b9beba99d4159cff amend by test at 1970-01-01T00:00:00 from:
      8b2e1bbf6c0bea98beb5615f7b1c49b8dc38a593 amend by test at 1970-01-01T00:00:00 from:
      4c454f4e96edd98561fa548e4c24acdcd11b4f75 amend by test at 1970-01-01T00:00:00 from:
      0b4427c985ad41ac0876748733cff668be15cb88 amend by test at 1970-01-01T00:00:00 from:
      5e4af9f7ddb8b12225ad17fadd7e3e6031d52f00 amend by test at 1970-01-01T00:00:00 from:
      5aeb3a2d36afb4cb50a6c491bc05584a1da2018d amend by test at 1970-01-01T00:00:00 from:
      6d60953c6009fdd3d6bd870ad37c7f48ea6d1311 amend by test at 1970-01-01T00:00:00 from:
      c5d0fa8770bdde6ef311cc640a78a2f686be28b4
  
JSON output exposes the existing predecessor lookup without user or time metadata.

  $ sl debugmutation -Tjson -r 6d60953c6009fdd3d6bd870ad37c7f48ea6d1311
  {"mutations": [{"operation": "amend", "predecessors": ["c5d0fa8770bdde6ef311cc640a78a2f686be28b4"], "split_successors": [], "successor": "6d60953c6009fdd3d6bd870ad37c7f48ea6d1311"}], "target": "6d60953c6009fdd3d6bd870ad37c7f48ea6d1311"}

JSON output accepts exactly one predecessor target.

  $ sl debugmutation -Tjson -r . -r .^
  abort: -Tjson requires exactly one revision
  [255]
  $ sl debugmutation -Tjson --successors
  abort: -Tjson does not support --successors
  [255]
  $ sl debugmutation -Tjson --time-range 0
  abort: -Tjson does not support --time-range
  [255]
  $ sl debugmutation -T '{node}'
  abort: debugmutation only supports -Tjson
  [255]

  $ sl log -r . -T '{dict(predecessors)|json}\n'
  {"predecessors": ["8b2e1bbf6c0bea98beb5615f7b1c49b8dc38a593"]}

Rebase

  $ echo "a" > file2
  $ sl commit -Aqm c2
  $ echo "a" > file3
  $ sl commit -Aqm c3
  $ sl rebase -q -s ".^" -d 'desc(base)'
  $ sl rebase -q -s ".^" -d c5d0fa8770bdde6ef311cc640a78a2f686be28b4 --hidden
  $ sl rebase -q -s ".^" -d 'max(desc(c1))' --hidden
  $ sl debugmutation -r ".^::."
   *  33ca17be2228dc288194daade1265b5de0222653 rebase by test at 1970-01-01T00:00:00 from:
      30184ea7dbf74f751464657e167173d1d531e700 rebase by test at 1970-01-01T00:00:00 from:
      dfd7d11783056958dfd2bb5479b3f84c71b698b9 rebase by test at 1970-01-01T00:00:00 from:
      a0d726ccf2422e2cbfe7b06d3dc3f81b064b05aa
  
   *  054edf9500f5e849563bf6515446d74654e14fd0 rebase by test at 1970-01-01T00:00:00 from:
      f6dac11b6941b475383af15d69cd0b7363e045d0 rebase by test at 1970-01-01T00:00:00 from:
      38dc6e5d067f289d0a1ad9c6eae9bb9ed111cd04 rebase by test at 1970-01-01T00:00:00 from:
      d139edd196dd2b5a298932fdd696b96cd8101982
  

Metaedit

 (Before metaedit)
  $ sl log -Gr 'all() + draft()' -T '{desc} {node|short} {phase}'
  @  c3 054edf9500f5 draft
  │
  o  c2 33ca17be2228 draft
  │
  o  c1 (amended 8) cc809964b024 draft
  │
  o  base d20a80d4def3 draft
  
  $ sl meta -m "c3 (metaedited)"
 (After metaedit)
  $ sl log -Gr 'all() + draft()' -T '{desc} {node|short} {phase}'
  @  c3 (metaedited) 374724d5279b draft
  │
  o  c2 33ca17be2228 draft
  │
  o  c1 (amended 8) cc809964b024 draft
  │
  o  base d20a80d4def3 draft
  
  $ sl debugmutation
   *  374724d5279b5992bf6ec2ccb3d326844e36b4ba metaedit by test at 1970-01-01T00:00:00 from:
      054edf9500f5e849563bf6515446d74654e14fd0 rebase by test at 1970-01-01T00:00:00 from:
      f6dac11b6941b475383af15d69cd0b7363e045d0 rebase by test at 1970-01-01T00:00:00 from:
      38dc6e5d067f289d0a1ad9c6eae9bb9ed111cd04 rebase by test at 1970-01-01T00:00:00 from:
      d139edd196dd2b5a298932fdd696b96cd8101982
  

Fold

 (Before fold)
  $ sl log -Gr 'all() + draft()' -T '{desc} {node|short} {phase}'
  @  c3 (metaedited) 374724d5279b draft
  │
  o  c2 33ca17be2228 draft
  │
  o  c1 (amended 8) cc809964b024 draft
  │
  o  base d20a80d4def3 draft
  
  $ sl fold --from ".^"
  2 changesets folded
  0 files updated, 0 files merged, 0 files removed, 0 files unresolved
 (After fold)
  $ sl log -Gr 'all() + draft()' -T '{desc} {node|short} {phase}'
  @  c2
  │
  │
  │  c3 (metaedited) f05234144e37 draft
  o  c1 (amended 8) cc809964b024 draft
  │
  o  base d20a80d4def3 draft
  
  $ sl debugmutation
   *  f05234144e37d59b175fa4283563aac4dfe81ec0 fold by test at 1970-01-01T00:00:00 from:
      |-  33ca17be2228dc288194daade1265b5de0222653 rebase by test at 1970-01-01T00:00:00 from:
      |   30184ea7dbf74f751464657e167173d1d531e700 rebase by test at 1970-01-01T00:00:00 from:
      |   dfd7d11783056958dfd2bb5479b3f84c71b698b9 rebase by test at 1970-01-01T00:00:00 from:
      |   a0d726ccf2422e2cbfe7b06d3dc3f81b064b05aa
      '-  374724d5279b5992bf6ec2ccb3d326844e36b4ba metaedit by test at 1970-01-01T00:00:00 from:
          054edf9500f5e849563bf6515446d74654e14fd0 rebase by test at 1970-01-01T00:00:00 from:
          f6dac11b6941b475383af15d69cd0b7363e045d0 rebase by test at 1970-01-01T00:00:00 from:
          38dc6e5d067f289d0a1ad9c6eae9bb9ed111cd04 rebase by test at 1970-01-01T00:00:00 from:
          d139edd196dd2b5a298932fdd696b96cd8101982
  

Split, leaving some changes left over at the end

  $ echo "b" >> file2
  $ echo "b" >> file3
  $ sl commit -qm c4
  $ sl split << EOF
  > y
  > y
  > n
  > y
  > EOF
  2 files updated, 0 files merged, 0 files removed, 0 files unresolved
  reverting file2
  reverting file3
  diff --git a/file2 b/file2
  1 hunks, 1 lines changed
  examine changes to 'file2'? [Ynesfdaq?] y
  
  @@ -1,1 +1,2 @@
   a
  +b
  record change 1/2 to 'file2'? [Ynesfdaq?] y
  
  diff --git a/file3 b/file3
  1 hunks, 1 lines changed
  examine changes to 'file3'? [Ynesfdaq?] n
  
  Done splitting? [yN] y
  $ sl debugmutation -r ".^::."
   *  7d383d1b236d896a5adeea8dc390b681e4ccb217
  
   *  9c2c451b82d046da459d807b11c42992324e4e33 split by test at 1970-01-01T00:00:00 (split into this and: 7d383d1b236d896a5adeea8dc390b681e4ccb217) from:
      07f94070ed0943f8108119a726522ec4879ed36a
  

Split parent, selecting all changes at the end

  $ echo "c" >> file2
  $ echo "c" >> file3
  $ sl commit -qm c5
  $ echo "d" >> file3
  $ sl commit -qm c6
  $ sl split ".^" << EOF
  > y
  > y
  > n
  > n
  > y
  > y
  > EOF
  2 files updated, 0 files merged, 0 files removed, 0 files unresolved
  reverting file2
  reverting file3
  diff --git a/file2 b/file2
  1 hunks, 1 lines changed
  examine changes to 'file2'? [Ynesfdaq?] y
  
  @@ -1,2 +1,3 @@
   a
   b
  +c
  record change 1/2 to 'file2'? [Ynesfdaq?] y
  
  diff --git a/file3 b/file3
  1 hunks, 1 lines changed
  examine changes to 'file3'? [Ynesfdaq?] n
  
  Done splitting? [yN] n
  diff --git a/file3 b/file3
  1 hunks, 1 lines changed
  examine changes to 'file3'? [Ynesfdaq?] y
  
  @@ -1,2 +1,3 @@
   a
   b
  +c
  record this change to 'file3'? [Ynesfdaq?] y
  
  no more change to split
  rebasing 0529c1ec7df6 "c6"

Split leaves the checkout at the top of the split commits

  $ sl debugmutation -r ".^::tip"
   *  36e4e93ec194346c3e5a0afefd426dbc14dcaf4a
  
   *  aa10382521dc0799a9ebc1235aa0783149ffcc4e split by test at 1970-01-01T00:00:00 (split into this and: 36e4e93ec194346c3e5a0afefd426dbc14dcaf4a) from:
      be81d74b508c48b66c74f7c111188be611bb56a7
  
   *  0623f07d148d6446aeb15deb7ead4cb6f62135ef rebase by test at 1970-01-01T00:00:00 from:
      0529c1ec7df66092602017d0a5f372316d0bc360
  

Amend with rebase afterwards (split info should not be propagated)

 (Before amend)
  $ sl log -Gr 'all() + draft()' -T '{desc} {node|short} {phase}'
  o  c6 0623f07d148d draft
  │
  @  c5 aa10382521dc draft
  │
  o  c5 36e4e93ec194 draft
  │
  o  c4 9c2c451b82d0 draft
  │
  o  c4 7d383d1b236d draft
  │
  o  c2
  │
  │
  │  c3 (metaedited) f05234144e37 draft
  o  c1 (amended 8) cc809964b024 draft
  │
  o  base d20a80d4def3 draft
  
  $ sl amend --rebase -m "c5 (split)"
  rebasing 0623f07d148d "c6"
 (After amend)
  $ sl log -Gr 'all() + draft()' -T '{desc} {node|short} {phase}'
  o  c6 c3b5428c707b draft
  │
  @  c5 (split) 48b076c1640c draft
  │
  o  c5 36e4e93ec194 draft
  │
  o  c4 9c2c451b82d0 draft
  │
  o  c4 7d383d1b236d draft
  │
  o  c2
  │
  │
  │  c3 (metaedited) f05234144e37 draft
  o  c1 (amended 8) cc809964b024 draft
  │
  o  base d20a80d4def3 draft
  
  $ sl debugmutation -r ".::tip"
   *  48b076c1640c53afc98cc99922d034e17830a65d amend by test at 1970-01-01T00:00:00 from:
      aa10382521dc0799a9ebc1235aa0783149ffcc4e split by test at 1970-01-01T00:00:00 (split into this and: 36e4e93ec194346c3e5a0afefd426dbc14dcaf4a) from:
      be81d74b508c48b66c74f7c111188be611bb56a7
  
   *  c3b5428c707bb5ec79935064ec9a83084fee1afb rebase by test at 1970-01-01T00:00:00 from:
      0623f07d148d6446aeb15deb7ead4cb6f62135ef rebase by test at 1970-01-01T00:00:00 from:
      0529c1ec7df66092602017d0a5f372316d0bc360
  

Histedit

  $ . "$TESTDIR/histedit-helpers.sh"

  $ sl up tip
  1 files updated, 0 files merged, 0 files removed, 0 files unresolved
  $ echo "e" >> file4
  $ sl commit -Aqm c7
  $ echo "f" >> file4
  $ sl commit -Aqm c8
  $ echo "g" >> file4
  $ sl commit -Aqm c9
 (Before histedit)
  $ sl log -Gr 'all() + draft()' -T '{desc} {node|short} {phase}'
  @  c9 b6ea0faadebf draft
  │
  o  c8 64a3bc96c043 draft
  │
  o  c7 c4484fcb5ac0 draft
  │
  o  c6 c3b5428c707b draft
  │
  o  c5 (split) 48b076c1640c draft
  │
  o  c5 36e4e93ec194 draft
  │
  o  c4 9c2c451b82d0 draft
  │
  o  c4 7d383d1b236d draft
  │
  o  c2
  │
  │
  │  c3 (metaedited) f05234144e37 draft
  o  c1 (amended 8) cc809964b024 draft
  │
  o  base d20a80d4def3 draft
  
  $ sl histedit 'max(desc(c1))' --commands - 2>&1 <<EOF | fixbundle
  > pick cc809964b024
  > pick f05234144e37
  > fold 7d383d1b236d
  > roll 9c2c451b82d0
  > fold 36e4e93ec194
  > roll 48b076c1640c
  > pick c3b5428c707b
  > roll c4484fcb5ac0
  > roll 64a3bc96c043
  > pick b6ea0faadebf
  > EOF
  folded f05234144e37, 7d383d1b236d, 9c2c451b82d0, 36e4e93ec194, 48b076c1640c -> 1851fa2d6ef0 "c2"
  folded c3b5428c707b, c4484fcb5ac0, 64a3bc96c043 -> dd5d0e1bc12e "c6"
 (After histedit)
  $ sl log -Gr 'all() + draft()' -T '{desc} {node|short} {phase}'
  @  c9 3c3b86a5a351 draft
  │
  o  c6 dd5d0e1bc12e draft
  │
  o  c2
  │
  │
  │  c3 (metaedited)
  │  ***
  │  c4
  │  ***
  │  c5 1851fa2d6ef0 draft
  o  c1 (amended 8) cc809964b024 draft
  │
  o  base d20a80d4def3 draft
  
  $ sl debugmutation -r cc809964b02448cb4c84c772b9beba99d4159cff::tip
   *  cc809964b02448cb4c84c772b9beba99d4159cff amend by test at 1970-01-01T00:00:00 from:
      8b2e1bbf6c0bea98beb5615f7b1c49b8dc38a593 amend by test at 1970-01-01T00:00:00 from:
      4c454f4e96edd98561fa548e4c24acdcd11b4f75 amend by test at 1970-01-01T00:00:00 from:
      0b4427c985ad41ac0876748733cff668be15cb88 amend by test at 1970-01-01T00:00:00 from:
      5e4af9f7ddb8b12225ad17fadd7e3e6031d52f00 amend by test at 1970-01-01T00:00:00 from:
      5aeb3a2d36afb4cb50a6c491bc05584a1da2018d amend by test at 1970-01-01T00:00:00 from:
      6d60953c6009fdd3d6bd870ad37c7f48ea6d1311 amend by test at 1970-01-01T00:00:00 from:
      c5d0fa8770bdde6ef311cc640a78a2f686be28b4
  
   *  1851fa2d6ef001f121536b4d076e8ec6c01e3b34 histedit by test at 1970-01-01T00:00:00 from:
      |-  76fad0d9f8585b5d315b140cf784130e4a23ba28 histedit by test at 1970-01-01T00:00:00 from:
      |   |-  5dbe0bac3aa7743362af3b46d69ea19ea84fd35a histedit by test at 1970-01-01T00:00:00 from:
      |   |   |-  419fc47d2ae4909d2cdff5f873c3d9c18eeaa057 histedit by test at 1970-01-01T00:00:00 from:
      |   |   |   |-  f05234144e37d59b175fa4283563aac4dfe81ec0 fold by test at 1970-01-01T00:00:00 from:
      |   |   |   |   |-  33ca17be2228dc288194daade1265b5de0222653 rebase by test at 1970-01-01T00:00:00 from:
      |   |   |   |   |   30184ea7dbf74f751464657e167173d1d531e700 rebase by test at 1970-01-01T00:00:00 from:
      |   |   |   |   |   dfd7d11783056958dfd2bb5479b3f84c71b698b9 rebase by test at 1970-01-01T00:00:00 from:
      |   |   |   |   |   a0d726ccf2422e2cbfe7b06d3dc3f81b064b05aa
      |   |   |   |   '-  374724d5279b5992bf6ec2ccb3d326844e36b4ba metaedit by test at 1970-01-01T00:00:00 from:
      |   |   |   |       054edf9500f5e849563bf6515446d74654e14fd0 rebase by test at 1970-01-01T00:00:00 from:
      |   |   |   |       f6dac11b6941b475383af15d69cd0b7363e045d0 rebase by test at 1970-01-01T00:00:00 from:
      |   |   |   |       38dc6e5d067f289d0a1ad9c6eae9bb9ed111cd04 rebase by test at 1970-01-01T00:00:00 from:
      |   |   |   |       d139edd196dd2b5a298932fdd696b96cd8101982
      |   |   |   '-  7d383d1b236d896a5adeea8dc390b681e4ccb217
      |   |   '-  9c2c451b82d046da459d807b11c42992324e4e33 split by test at 1970-01-01T00:00:00 (split into this and: 7d383d1b236d896a5adeea8dc390b681e4ccb217) from:
      |   |       07f94070ed0943f8108119a726522ec4879ed36a
      |   '-  36e4e93ec194346c3e5a0afefd426dbc14dcaf4a
      '-  48b076c1640c53afc98cc99922d034e17830a65d amend by test at 1970-01-01T00:00:00 from:
          aa10382521dc0799a9ebc1235aa0783149ffcc4e split by test at 1970-01-01T00:00:00 (split into this and: 36e4e93ec194346c3e5a0afefd426dbc14dcaf4a) from:
          be81d74b508c48b66c74f7c111188be611bb56a7
  
   *  dd5d0e1bc12eb7fb11debaa39287fb24c16a80d8 histedit by test at 1970-01-01T00:00:00 from:
      |-  e1a0d5ae83cecdbf2a65995535ea1a3cd2009ab8 histedit by test at 1970-01-01T00:00:00 from:
      |   |-  e0e94ae5d0b0429f35bb3e14d1532fc861122e32 histedit by test at 1970-01-01T00:00:00 from:
      |   |   c3b5428c707bb5ec79935064ec9a83084fee1afb rebase by test at 1970-01-01T00:00:00 from:
      |   |   0623f07d148d6446aeb15deb7ead4cb6f62135ef rebase by test at 1970-01-01T00:00:00 from:
      |   |   0529c1ec7df66092602017d0a5f372316d0bc360
      |   '-  c4484fcb5ac0f15058c6595a56d239d4ed707bee
      '-  64a3bc96c043ea50b808b3ace4a4c6d2ca92b2d2
  
   *  3c3b86a5a351839b5fe6905587497121b4b05777 histedit by test at 1970-01-01T00:00:00 from:
      b6ea0faadebf4576be2b7cff316c5f9aa9fbc295
  

Revsets

  $ sl log -T '{node}\n' -r 'predecessors(1851fa2d6ef0)' --hidden
  a0d726ccf2422e2cbfe7b06d3dc3f81b064b05aa
  d139edd196dd2b5a298932fdd696b96cd8101982
  dfd7d11783056958dfd2bb5479b3f84c71b698b9
  38dc6e5d067f289d0a1ad9c6eae9bb9ed111cd04
  30184ea7dbf74f751464657e167173d1d531e700
  f6dac11b6941b475383af15d69cd0b7363e045d0
  33ca17be2228dc288194daade1265b5de0222653
  054edf9500f5e849563bf6515446d74654e14fd0
  374724d5279b5992bf6ec2ccb3d326844e36b4ba
  f05234144e37d59b175fa4283563aac4dfe81ec0
  07f94070ed0943f8108119a726522ec4879ed36a
  7d383d1b236d896a5adeea8dc390b681e4ccb217
  9c2c451b82d046da459d807b11c42992324e4e33
  be81d74b508c48b66c74f7c111188be611bb56a7
  36e4e93ec194346c3e5a0afefd426dbc14dcaf4a
  aa10382521dc0799a9ebc1235aa0783149ffcc4e
  48b076c1640c53afc98cc99922d034e17830a65d
  419fc47d2ae4909d2cdff5f873c3d9c18eeaa057
  5dbe0bac3aa7743362af3b46d69ea19ea84fd35a
  76fad0d9f8585b5d315b140cf784130e4a23ba28
  1851fa2d6ef001f121536b4d076e8ec6c01e3b34
  $ sl log -T '{node}\n' -r 'predecessors(1851fa2d6ef0,3)' --hidden
  9c2c451b82d046da459d807b11c42992324e4e33
  be81d74b508c48b66c74f7c111188be611bb56a7
  36e4e93ec194346c3e5a0afefd426dbc14dcaf4a
  aa10382521dc0799a9ebc1235aa0783149ffcc4e
  48b076c1640c53afc98cc99922d034e17830a65d
  419fc47d2ae4909d2cdff5f873c3d9c18eeaa057
  5dbe0bac3aa7743362af3b46d69ea19ea84fd35a
  76fad0d9f8585b5d315b140cf784130e4a23ba28
  1851fa2d6ef001f121536b4d076e8ec6c01e3b34
  $ sl log -T '{node}\n' -r 'predecessors(c3b5428c707b)' --hidden
  0529c1ec7df66092602017d0a5f372316d0bc360
  0623f07d148d6446aeb15deb7ead4cb6f62135ef
  c3b5428c707bb5ec79935064ec9a83084fee1afb
  $ sl log -T '{node}\n' -r 'predecessors(0529c1ec7df6)' --hidden
  0529c1ec7df66092602017d0a5f372316d0bc360

  $ sl log -T '{node}\n' -r 'successors(0529c1ec7df6)' --hidden
  0529c1ec7df66092602017d0a5f372316d0bc360
  0623f07d148d6446aeb15deb7ead4cb6f62135ef
  c3b5428c707bb5ec79935064ec9a83084fee1afb
  e0e94ae5d0b0429f35bb3e14d1532fc861122e32
  e1a0d5ae83cecdbf2a65995535ea1a3cd2009ab8
  dd5d0e1bc12eb7fb11debaa39287fb24c16a80d8
  $ sl log -T '{node}\n' -r 'successors(0529c1ec7df6,2)' --hidden
  0529c1ec7df66092602017d0a5f372316d0bc360
  0623f07d148d6446aeb15deb7ead4cb6f62135ef
  c3b5428c707bb5ec79935064ec9a83084fee1afb
  $ sl log -T '{node}\n' -r 'successors(a0d726ccf242)' --hidden
  a0d726ccf2422e2cbfe7b06d3dc3f81b064b05aa
  dfd7d11783056958dfd2bb5479b3f84c71b698b9
  30184ea7dbf74f751464657e167173d1d531e700
  33ca17be2228dc288194daade1265b5de0222653
  f05234144e37d59b175fa4283563aac4dfe81ec0
  419fc47d2ae4909d2cdff5f873c3d9c18eeaa057
  5dbe0bac3aa7743362af3b46d69ea19ea84fd35a
  76fad0d9f8585b5d315b140cf784130e4a23ba28
  1851fa2d6ef001f121536b4d076e8ec6c01e3b34
  $ sl log -T '{node}\n' -r 'successors(.)' --hidden
  3c3b86a5a351839b5fe6905587497121b4b05777

Unhide some old commits and show their mutations in the log
  $ sl unhide -q dd5d0e1bc12eb7fb11debaa39287fb24c16a80d8
  $ sl unhide -q 07f94070ed0943f8108119a726522ec4879ed36a
  $ sl unhide -q 5dbe0bac3aa7743362af3b46d69ea19ea84fd35a
  $ sl unhide -q 6d60953c6009fdd3d6bd870ad37c7f48ea6d1311
  $ sl unhide -q c5d0fa8770bdde6ef311cc640a78a2f686be28b4
  $ tglogm
  @  3c3b86a5a351 'c9'
  │
  o  dd5d0e1bc12e 'c6'
  │
  o  1851fa2d6ef0 'c2'
  │
  │ x  5dbe0bac3aa7 'c2'  (Rewritten using rewrite into 1851fa2d6ef0)
  ├─╯
  │ x  07f94070ed09 'c4'  (Rewritten using rewrite into 5dbe0bac3aa7)
  │ │
  │ x  f05234144e37 'c2'  (Rewritten using rewrite into 5dbe0bac3aa7)
  ├─╯
  o  cc809964b024 'c1 (amended 8)'
  │
  │ x  6d60953c6009 'c1 (amended 2)'  (Rewritten using rewrite into cc809964b024)
  ├─╯
  │ x  c5d0fa8770bd 'c1'  (Rewritten using amend into 6d60953c6009)
  ├─╯
  o  d20a80d4def3 'base'
  
Debugmutatation looking forward
  $ sl debugmutation -s -r c4484fcb5ac0f15058c6595a56d239d4ed707bee --hidden
   *  c4484fcb5ac0f15058c6595a56d239d4ed707bee histedit by test at 1970-01-01T00:00:00 (folded with: e0e94ae5d0b0429f35bb3e14d1532fc861122e32) into:
      e1a0d5ae83cecdbf2a65995535ea1a3cd2009ab8 histedit by test at 1970-01-01T00:00:00 (folded with: 64a3bc96c043ea50b808b3ace4a4c6d2ca92b2d2) into:
      dd5d0e1bc12eb7fb11debaa39287fb24c16a80d8
  
  $ sl debugmutation -s -r 07f94070ed0943f8108119a726522ec4879ed36a
   *  07f94070ed0943f8108119a726522ec4879ed36a split by test at 1970-01-01T00:00:00 into:
      |-  7d383d1b236d896a5adeea8dc390b681e4ccb217 histedit by test at 1970-01-01T00:00:00 (folded with: f05234144e37d59b175fa4283563aac4dfe81ec0) into:
      |   419fc47d2ae4909d2cdff5f873c3d9c18eeaa057 histedit by test at 1970-01-01T00:00:00 (folded with: 9c2c451b82d046da459d807b11c42992324e4e33) into:
      |   5dbe0bac3aa7743362af3b46d69ea19ea84fd35a histedit by test at 1970-01-01T00:00:00 (folded with: 36e4e93ec194346c3e5a0afefd426dbc14dcaf4a) into:
      |   76fad0d9f8585b5d315b140cf784130e4a23ba28 histedit by test at 1970-01-01T00:00:00 (folded with: 48b076c1640c53afc98cc99922d034e17830a65d) into:
      |   1851fa2d6ef001f121536b4d076e8ec6c01e3b34
      '-  9c2c451b82d046da459d807b11c42992324e4e33 histedit by test at 1970-01-01T00:00:00 (folded with: 419fc47d2ae4909d2cdff5f873c3d9c18eeaa057) into:
          5dbe0bac3aa7743362af3b46d69ea19ea84fd35a histedit by test at 1970-01-01T00:00:00 (folded with: 36e4e93ec194346c3e5a0afefd426dbc14dcaf4a) into:
          76fad0d9f8585b5d315b140cf784130e4a23ba28 histedit by test at 1970-01-01T00:00:00 (folded with: 48b076c1640c53afc98cc99922d034e17830a65d) into:
          1851fa2d6ef001f121536b4d076e8ec6c01e3b34
  

Histedit with exec that amends in between folds

  $ cd ..
  $ newrepo
  $ for i in 1 2 3 4
  > do
  >   echo $i >> file
  >   sl commit -Aqm "commit $i"
  > done
  $ sl histedit c2a29f8b7d7a23d58e698384280df426802a1465 --commands - 2>&1 <<EOF | fixbundle
  > pick c2a29f8b7d7a
  > pick 08d8367dafb9
  > fold 15a208dbcdc5
  > exec sl amend -m "commit 3 amended"
  > fold 0d4155d128bf
  > EOF
  0 files updated, 0 files merged, 0 files removed, 0 files unresolved
  folded 08d8367dafb9, 15a208dbcdc5, 0d4155d128bf -> a2235e1011a0 "commit 3 amended"
  $ tglog
  @  a2235e1011a0 'commit 3 amended
  │  ***
  │  commit 4'
  o  c2a29f8b7d7a 'commit 1'
  
  $ sl debugmutation -r "all()" --hidden
   *  c2a29f8b7d7a23d58e698384280df426802a1465
  
   *  08d8367dafb9bb90c58101707eca32b726ca635a
  
   *  15a208dbcdc54b4f841ffecf9d13f98675933242
  
   *  0d4155d128bf7fff3f12582a65b52be84ad44809
  
   *  7e96860f6790189e613eb93c3d8edc2e4432c204 histedit by test at 1970-01-01T00:00:00 from:
      |-  08d8367dafb9bb90c58101707eca32b726ca635a
      '-  15a208dbcdc54b4f841ffecf9d13f98675933242
  
   *  cc92d7c90d06d08784aed399397f8cb68eb25325 amend by test at 1970-01-01T00:00:00 from:
      7e96860f6790189e613eb93c3d8edc2e4432c204 histedit by test at 1970-01-01T00:00:00 from:
      |-  08d8367dafb9bb90c58101707eca32b726ca635a
      '-  15a208dbcdc54b4f841ffecf9d13f98675933242
  
   *  a2235e1011a071a02b80aadd371c2fb15308ce15 histedit by test at 1970-01-01T00:00:00 from:
      |-  cc92d7c90d06d08784aed399397f8cb68eb25325 amend by test at 1970-01-01T00:00:00 from:
      |   7e96860f6790189e613eb93c3d8edc2e4432c204 histedit by test at 1970-01-01T00:00:00 from:
      |   |-  08d8367dafb9bb90c58101707eca32b726ca635a
      |   '-  15a208dbcdc54b4f841ffecf9d13f98675933242
      '-  0d4155d128bf7fff3f12582a65b52be84ad44809
  

Histedit with stop, extra commit, and fold

  $ cd ..
  $ newrepo
  $ for i in 1 2 3 4
  > do
  >   echo $i >> file
  >   sl commit -Aqm "commit $i"
  > done
  $ sl histedit c2a29f8b7d7a23d58e698384280df426802a1465 --commands - 2>&1 <<EOF | fixbundle
  > pick c2a29f8b7d7a
  > pick 08d8367dafb9
  > stop 15a208dbcdc5
  > fold 0d4155d128bf
  > EOF
  Changes committed as f8ba6373a87e. You may amend the changeset now.
  When you are done, run sl histedit --continue to resume
  $ echo extra >> file2
  $ sl commit -Aqm "extra commit"
  $ sl histedit --continue | fixbundle
  folded 15a208dbcdc5, 0d4155d128bf -> d313be93f9b7 "extra commit"
  $ tglog
  @  d313be93f9b7 'extra commit
  │  ***
  │  commit 4'
  o  f8ba6373a87e 'commit 3'
  │
  o  08d8367dafb9 'commit 2'
  │
  o  c2a29f8b7d7a 'commit 1'
  
  $ sl debugmutation -r "all()" --hidden
   *  c2a29f8b7d7a23d58e698384280df426802a1465
  
   *  08d8367dafb9bb90c58101707eca32b726ca635a
  
   *  15a208dbcdc54b4f841ffecf9d13f98675933242
  
   *  0d4155d128bf7fff3f12582a65b52be84ad44809
  
   *  f8ba6373a87ea735d0ec10f15816ea7121c25257 histedit by test at 1970-01-01T00:00:00 from:
      15a208dbcdc54b4f841ffecf9d13f98675933242
  
   *  59401578013ab5382082b65eed82d6a465c081a0
  
   *  d313be93f9b7ee46e11581641241d356c346a001 histedit by test at 1970-01-01T00:00:00 from:
      |-  59401578013ab5382082b65eed82d6a465c081a0
      '-  0d4155d128bf7fff3f12582a65b52be84ad44809
  

Drawdag

  $ cd ..
  $ newrepo
  $ sl debugdrawdag <<'EOS'
  >       G
  >       |
  > I D C F   # split: B -> E, F, G
  >  \ \| |   # rebase: C -> D -> H
  >   H B E   # prune: F, I
  >    \|/
  >     A
  > EOS

  $ tglogm
  o  b2faf047aa50 'I' I
  │
  o  a1093b439e1b 'H' H
  │
  │ o  dd319aacbb51 'G' G
  │ │
  │ o  64a8289d2492 'F' F
  │ │
  │ o  7fb047a69f22 'E' E
  ├─╯
  o  426bada5c675 'A' A
  
  $ sl debugmutation -r "all()" --hidden
   *  426bada5c67598ca65036d57d9e4b64b0c1ce7a0
  
   *  112478962961147124edd43549aedd1a335e44bf
  
   *  26805aba1e600a82e93661149f2313866a221a7b
  
   *  7fb047a69f220c21711122dfd94305a9efb60cba
  
   *  17d61397e601357ae1dd94c787f794ff95aa2d59 rebase by test at 1970-01-01T00:00:00 from:
      26805aba1e600a82e93661149f2313866a221a7b
  
   *  64a8289d249234b9886244d379f15e6b650b28e3
  
   *  dd319aacbb516094646b9ee5a24a942e62110121 split by test at 1970-01-01T00:00:00 (split into this and: 7fb047a69f220c21711122dfd94305a9efb60cba, 64a8289d249234b9886244d379f15e6b650b28e3) from:
      112478962961147124edd43549aedd1a335e44bf
  
   *  a1093b439e1bc272490fd1749b526a3f1463a41e rebase by test at 1970-01-01T00:00:00 from:
      17d61397e601357ae1dd94c787f794ff95aa2d59 rebase by test at 1970-01-01T00:00:00 from:
      26805aba1e600a82e93661149f2313866a221a7b
  
   *  b2faf047aa50279686b1635bfad505cd51300b3c
  
JSON output includes the primary successor in a split's complete successor set.

  $ sl debugmutation -Tjson -r G
  {"mutations": [{"operation": "split", "predecessors": ["112478962961147124edd43549aedd1a335e44bf"], "split_successors": ["64a8289d249234b9886244d379f15e6b650b28e3", "7fb047a69f220c21711122dfd94305a9efb60cba", "dd319aacbb516094646b9ee5a24a942e62110121"], "successor": "dd319aacbb516094646b9ee5a24a942e62110121"}], "target": "dd319aacbb516094646b9ee5a24a942e62110121"}


Revsets obey visibility rules

  $ cd ..
  $ newrepo
  $ drawdag <<'EOS'
  >  E
  >  |
  >  B C D  # amend: B -> C -> D
  >   \|/   # prune: D
  >    A    # revive: C
  > EOS

  $ sl debugmutation -r "all()" --hidden
   *  426bada5c67598ca65036d57d9e4b64b0c1ce7a0
  
   *  112478962961147124edd43549aedd1a335e44bf
  
   *  2cb21a570bd242eb1225414c6634ed29cc9cfe93 amend by test at 1970-01-01T00:00:00 from:
      112478962961147124edd43549aedd1a335e44bf
  
   *  49cb92066bfd0763fff729c354345650b7428554
  
   *  82b1bbd9d7bb25fa8b9354ca7f6cfd007a6291af amend by test at 1970-01-01T00:00:00 from:
      2cb21a570bd242eb1225414c6634ed29cc9cfe93 amend by test at 1970-01-01T00:00:00 from:
      112478962961147124edd43549aedd1a335e44bf
  
  $ sl log -T '{node} {desc}\n' -r "successors(desc(B))"
  112478962961147124edd43549aedd1a335e44bf B
  2cb21a570bd242eb1225414c6634ed29cc9cfe93 C
  $ sl log -T '{node} {desc}\n' -r "successors(desc(B))" --hidden
  112478962961147124edd43549aedd1a335e44bf B
  2cb21a570bd242eb1225414c6634ed29cc9cfe93 C
  82b1bbd9d7bb25fa8b9354ca7f6cfd007a6291af D
  $ sl log -T '{node} {desc}\n' -r "predecessors(desc(C))"
  112478962961147124edd43549aedd1a335e44bf B
  2cb21a570bd242eb1225414c6634ed29cc9cfe93 C
  $ sl hide -q 'desc(B)'
  $ sl log -T '{node} {desc}\n' -r "predecessors(desc(C))"
  112478962961147124edd43549aedd1a335e44bf B
  2cb21a570bd242eb1225414c6634ed29cc9cfe93 C

Revsets for filtering commits based on mutated status

  $ cd ..
  $ newrepo
  $ drawdag << EOS
  >            P
  >            |\        # amend: C -> E -> G
  >  D F     M O S       # rebase: D -> F
  >  | |     | | |
  >  C E G   L N R U     # fold: L, M -> N
  >   \|/     \| | |
  >    B       K Q T     # amend: Q -> T
  >    |        \|/      # rebase: R -> U
  >    A         A
  > EOS

  $ sl log -r "obsolete()" -T '{desc}\n'
  Q
  R
  E
  $ sl log -r "obsolete()" -T '{desc}\n' --hidden
  Q
  C
  L
  R
  D
  E
  M

Successors Sets

  $ cd ..
  $ newrepo
  $ drawdag --print << EOS
  >  Z P         F H    # amend: A -> B -> C
  >  |/          | |    # amend: A -> D
  >  Y   A B C D E G    # split: D -> E, F
  >  |    \|/   \|/     # amend: E -> G
  >  X     X     X      # rebase: F -> H
  >                     # amend: Z -> P
  > EOS
  a3d17304151f A
  a3a02814b8b7 B
  2f9a29935e68 C
  02f5790aa53c D
  8bab98b2a161 E
  f6c9a27925b0 F
  5236c38a7e4b G
  6583166b698f H
  131b22b23838 P
  ba2b7fa7166d X
  54fe561aeb5b Y
  e67cd4473b7c Z
  $ sl debugmakepublic $Z --hidden

  $ sl debugsuccessorssets 'all()'
  ba2b7fa7166d
      ba2b7fa7166d
  54fe561aeb5b
      54fe561aeb5b
  e67cd4473b7c
      e67cd4473b7c
  2f9a29935e68
      2f9a29935e68
  131b22b23838
      131b22b23838
  5236c38a7e4b
      5236c38a7e4b
  6583166b698f
      6583166b698f
  $ sl debugsuccessorssets 'all()' --hidden
  ba2b7fa7166d
      ba2b7fa7166d
  a3d17304151f
      2f9a29935e68
      5236c38a7e4b 6583166b698f
      5236c38a7e4b f6c9a27925b0
  54fe561aeb5b
      54fe561aeb5b
  a3a02814b8b7
      2f9a29935e68
  02f5790aa53c
      2f9a29935e68
      5236c38a7e4b 6583166b698f
      5236c38a7e4b f6c9a27925b0
  e67cd4473b7c
      e67cd4473b7c
  2f9a29935e68
      2f9a29935e68
  8bab98b2a161
      5236c38a7e4b
  131b22b23838
      131b22b23838
  f6c9a27925b0
      5236c38a7e4b 6583166b698f
      6583166b698f
  5236c38a7e4b
      5236c38a7e4b
  6583166b698f
      6583166b698f
  $ sl debugsuccessorssets 'all()' --hidden --closest
  ba2b7fa7166d
      ba2b7fa7166d
  a3d17304151f
      02f5790aa53c
      a3a02814b8b7
  54fe561aeb5b
      54fe561aeb5b
  a3a02814b8b7
      2f9a29935e68
  02f5790aa53c
      2f9a29935e68
      8bab98b2a161 6583166b698f
      8bab98b2a161 f6c9a27925b0
  e67cd4473b7c
      e67cd4473b7c
  2f9a29935e68
      2f9a29935e68
  8bab98b2a161
      5236c38a7e4b
  131b22b23838
      131b22b23838
  f6c9a27925b0
      6583166b698f
      8bab98b2a161 6583166b698f
  5236c38a7e4b
      5236c38a7e4b
  6583166b698f
      6583166b698f

  $ cd ..
  $ newrepo
  $ drawdag --print <<'EOS'
  >                  # amend: A -> B
  >       E  G       # amend: A -> C
  >       |  |       # split: A -> D, E
  > A B C D  F H I   # fold: F, G -> H
  >  \|/  |   \|/    # amend: F -> I
  >   Z   Z    Z
  > EOS
  ac2f7407182b A
  d0b9032f313b B
  f102e5df2a1d C
  6c7c301750f1 D
  ecd3acbeabe4 E
  847007ced9a7 F
  e1beb503e4fb G
  9e63cfda1f79 H
  e0ad3106c6e7 I
  48b9aae0607f Z

  $ sl debugsuccessorssets 'all()' --hidden
  48b9aae0607f
      48b9aae0607f
  ac2f7407182b
      6c7c301750f1 ecd3acbeabe4
      d0b9032f313b
      f102e5df2a1d
  847007ced9a7
      9e63cfda1f79
      e0ad3106c6e7
  d0b9032f313b
      d0b9032f313b
  f102e5df2a1d
      f102e5df2a1d
  6c7c301750f1
      6c7c301750f1
  e1beb503e4fb
      9e63cfda1f79
  e0ad3106c6e7
      e0ad3106c6e7
  ecd3acbeabe4
      ecd3acbeabe4
  9e63cfda1f79
      9e63cfda1f79
  $ sl debugsuccessorssets 'all()' --closest
  48b9aae0607f
      48b9aae0607f
  d0b9032f313b
      d0b9032f313b
  f102e5df2a1d
      f102e5df2a1d
  6c7c301750f1
      6c7c301750f1
  e0ad3106c6e7
      e0ad3106c6e7
  ecd3acbeabe4
      ecd3acbeabe4
  9e63cfda1f79
      9e63cfda1f79
  $ sl debugsuccessorssets 'all()' --closest --hidden
  48b9aae0607f
      48b9aae0607f
  ac2f7407182b
      6c7c301750f1 ecd3acbeabe4
      d0b9032f313b
      f102e5df2a1d
  847007ced9a7
      9e63cfda1f79
      e0ad3106c6e7
  d0b9032f313b
      d0b9032f313b
  f102e5df2a1d
      f102e5df2a1d
  6c7c301750f1
      6c7c301750f1
  e1beb503e4fb
      9e63cfda1f79
  e0ad3106c6e7
      e0ad3106c6e7
  ecd3acbeabe4
      ecd3acbeabe4
  9e63cfda1f79
      9e63cfda1f79

  $ cd ..
  $ newrepo
  $ drawdag --print <<'EOS'
  >                         # amend: A -> B
  >       E K       G   O   # amend: A -> C
  >       | |       |   |   # split: A -> D, E
  > A B C D J L M N F H I   # fold: F, G -> H
  >  \|/   \|/   \|  \|/    # amend: F -> I
  >   Z     Z     Z   Z     # amend: D -> J
  >                         # rebase: E -> K
  >                         # fold: J, K -> L
  >                         # amend: B -> M -> N
  >                         # rebase: C -> O
  > EOS
  ac2f7407182b A
  d0b9032f313b B
  f102e5df2a1d C
  6c7c301750f1 D
  ecd3acbeabe4 E
  847007ced9a7 F
  e1beb503e4fb G
  9e63cfda1f79 H
  e0ad3106c6e7 I
  da7ad28f0dba J
  4411f298bdd6 K
  c784f3cd8bdc L
  22d356388a54 M
  a50d498b7a3c N
  5f50ab0b5b00 O
  48b9aae0607f Z

  $ sl debugsuccessorssets 'all()'
  48b9aae0607f
      48b9aae0607f
  e0ad3106c6e7
      e0ad3106c6e7
  9e63cfda1f79
      9e63cfda1f79
  5f50ab0b5b00
      5f50ab0b5b00
  a50d498b7a3c
      a50d498b7a3c
  c784f3cd8bdc
      c784f3cd8bdc
  $ sl debugsuccessorssets 'all()' --hidden
  48b9aae0607f
      48b9aae0607f
  ac2f7407182b
      22d356388a54
      5f50ab0b5b00
      c784f3cd8bdc
      c784f3cd8bdc 22d356388a54
      c784f3cd8bdc 5f50ab0b5b00
      c784f3cd8bdc a50d498b7a3c
  847007ced9a7
      9e63cfda1f79
      e0ad3106c6e7
  d0b9032f313b
      a50d498b7a3c
  f102e5df2a1d
      5f50ab0b5b00
  6c7c301750f1
      c784f3cd8bdc
  e1beb503e4fb
      9e63cfda1f79
  e0ad3106c6e7
      e0ad3106c6e7
  ecd3acbeabe4
      22d356388a54
      5f50ab0b5b00
      a50d498b7a3c
      c784f3cd8bdc
  9e63cfda1f79
      9e63cfda1f79
  da7ad28f0dba
      c784f3cd8bdc
  22d356388a54
      a50d498b7a3c
  5f50ab0b5b00
      5f50ab0b5b00
  4411f298bdd6
      c784f3cd8bdc
  a50d498b7a3c
      a50d498b7a3c
  c784f3cd8bdc
      c784f3cd8bdc
  $ sl debugsuccessorssets 'all()' --closest
  48b9aae0607f
      48b9aae0607f
  e0ad3106c6e7
      e0ad3106c6e7
  9e63cfda1f79
      9e63cfda1f79
  5f50ab0b5b00
      5f50ab0b5b00
  a50d498b7a3c
      a50d498b7a3c
  c784f3cd8bdc
      c784f3cd8bdc
  $ sl debugsuccessorssets 'all()' --closest --hidden
  48b9aae0607f
      48b9aae0607f
  ac2f7407182b
      6c7c301750f1 ecd3acbeabe4
      d0b9032f313b
      f102e5df2a1d
  847007ced9a7
      9e63cfda1f79
      e0ad3106c6e7
  d0b9032f313b
      22d356388a54
  f102e5df2a1d
      5f50ab0b5b00
  6c7c301750f1
      da7ad28f0dba
  e1beb503e4fb
      9e63cfda1f79
  e0ad3106c6e7
      e0ad3106c6e7
  ecd3acbeabe4
      22d356388a54
      4411f298bdd6
      5f50ab0b5b00
      a50d498b7a3c
  9e63cfda1f79
      9e63cfda1f79
  da7ad28f0dba
      c784f3cd8bdc
  22d356388a54
      a50d498b7a3c
  5f50ab0b5b00
      5f50ab0b5b00
  4411f298bdd6
      c784f3cd8bdc
  a50d498b7a3c
      a50d498b7a3c
  c784f3cd8bdc
      c784f3cd8bdc

Many splits and folds:

  $ cd ..
  $ newrepo
  $ drawdag --print <<'EOS'
  >     G    R   P           # split: A -> B, C
  >     |    |   |           # split: B -> D, E
  >     F  J Q N O           # split: C -> F, G
  >     |  |/  |/            # split: A -> H, I, J
  >   C E  I L M             # fold: H, I -> K
  >   | |  | |/              # rebase: J -> L
  > A B D  H K               # split: L -> M, N
  >  \|/    \|               # split: N -> O, P
  >   Z      Z               # split: J -> Q, R
  > EOS
  ac2f7407182b A
  f0a671a46792 B
  e8d08dcdab1d C
  6c7c301750f1 D
  7cd6c6978add E
  5ac9f6030240 F
  4c1829ae45a4 G
  45724aa2168b H
  34d53c2267d8 I
  70dd76fd55e1 J
  d91873bbc3e2 K
  7acf57a544c8 L
  096075241d66 M
  cfe3132d4f90 N
  c3cd5a5aad51 O
  b5712e65f604 P
  444227ba9301 Q
  114f9718bb14 R
  48b9aae0607f Z

  $ A='desc(A)'
  $ L='desc(L)'
  $ sl debugsuccessorssets $A --hidden
  ac2f7407182b
      6c7c301750f1 7cd6c6978add 5ac9f6030240 4c1829ae45a4
      d91873bbc3e2 096075241d66 b5712e65f604
      d91873bbc3e2 444227ba9301 114f9718bb14
      d91873bbc3e2 7acf57a544c8
      d91873bbc3e2 cfe3132d4f90
  $ sl debugsuccessorssets $A --closest --hidden
  ac2f7407182b
      45724aa2168b 34d53c2267d8 70dd76fd55e1
      f0a671a46792 e8d08dcdab1d
  $ sl unhide $A $L
  $ sl debugsuccessorssets $A
  ac2f7407182b
      6c7c301750f1 7cd6c6978add 5ac9f6030240 4c1829ae45a4
      d91873bbc3e2
      d91873bbc3e2 096075241d66 b5712e65f604
      d91873bbc3e2 444227ba9301 114f9718bb14
      d91873bbc3e2 7acf57a544c8
  $ sl debugsuccessorssets $A --closest
  ac2f7407182b
      45724aa2168b 34d53c2267d8
      45724aa2168b 34d53c2267d8 096075241d66 b5712e65f604
      45724aa2168b 34d53c2267d8 444227ba9301 114f9718bb14
      45724aa2168b 34d53c2267d8 7acf57a544c8
      6c7c301750f1 7cd6c6978add 5ac9f6030240 4c1829ae45a4
  $ sl hide $P
  hiding commit b5712e65f604 "P"
  1 changeset hidden
  $ sl debugsuccessorssets $A
  ac2f7407182b
      6c7c301750f1 7cd6c6978add 5ac9f6030240 4c1829ae45a4
      d91873bbc3e2
      d91873bbc3e2 096075241d66
      d91873bbc3e2 444227ba9301 114f9718bb14
      d91873bbc3e2 7acf57a544c8
  $ sl log -r "all()" -T "{desc} {mutation_descs}\n"
  Z 
  A (Rewritten using split into H, I) (Rewritten using split into H, I, M) (Rewritten using split into H, I, Q, R) (Rewritten using split into H, I, L) (Rewritten using split into D, E, F, G)
  H (Rewritten using fold into K)
  D 
  I (Rewritten using fold into K)
  E 
  K 
  F 
  L (Rewritten using split into M, O) (Rewritten using rewrite into O)
  Q 
  G 
  M 
  R (Rewritten using rewrite into M)
  O 

Metaedit with descendant amended commits

  $ cd ..
  $ newrepo
  $ drawdag << 'EOS'
  > D     E
  > |     |
  > C  C1 C2 C3 C4 # amend: C -> C1 -> C2 -> C3 -> C4
  >  \ | /    \ /
  >   \|/      B
  >    B
  >    |
  >    A
  >    |
  >    Z
  > EOS
  $ sl metaedit -r $A -m A1
  $ sl log -G -T "{desc} {mutation_descs}\n" -r "all()"
  o  C4
  │
  │ o  E
  │ │
  │ x  C2 (Rewritten using rewrite into C4)
  ├─╯
  │ o  D
  │ │
  │ v  C (Rewritten using amend-copy into C4) (Rewritten using amend-copy into C2)
  ├─╯
  o  B
  │
  o  A1
  │
  o  Z
  
Metaedit with descendant folded commits

  $ cd ..
  $ newrepo
  $ drawdag << 'EOS'
  > D E
  >  \|      # fold: C, E -> F
  >   C F
  >   |/
  >   B
  >   |
  >   A
  >   |
  >   Z
  > EOS
  $ sl log -G -T "{desc} {mutation_descs}\n" -r "all()"
  o  F
  │
  │ o  D
  │ │
  │ x  C (Rewritten using fold into F)
  ├─╯
  o  B
  │
  o  A
  │
  o  Z
  
  $ sl metaedit -r $A -m "A1"
  $ sl log -G -T "{desc} {mutation_descs}\n" -r "all()"
  o  F
  │
  │ o  D
  │ │
  │ x  C (Rewritten using fold-copy into F)
  ├─╯
  o  B
  │
  o  A1
  │
  o  Z
  

Metaedit automatic rebase of amended commit

  $ cd ..
  $ newrepo
  $ drawdag << 'EOS'
  > D
  > |
  > C  C1 C2  # amend: C -> C1 -> C2
  >  \ | /
  >   \|/
  >    B
  >    |
  >    A
  > EOS
  $ sl metaedit -r $B -m B1
  $ sl log -G -T "{desc} {mutation_descs}\n" -r "all()"
  o  C2
  │
  │ o  D
  │ │
  │ x  C (Rewritten using amend-copy into C2)
  ├─╯
  o  B1
  │
  o  A
  
Absorb

  $ cd ..
  $ newrepo
  $ drawdag << 'EOS'
  > E
  > |
  > D
  > |
  > C
  > |
  > B
  > |
  > A
  > EOS
  $ sl up -q $E
  $ echo extra >> E
  $ echo extra >> C
  $ sl absorb -a
  showing changes for C
          @@ -0,1 +0,1 @@
  26805ab -C
  26805ab +Cextra
  showing changes for E
          @@ -0,1 +0,1 @@
  9bc730a -E
  9bc730a +Eextra
  
  2 changesets affected
  9bc730a E
  26805ab C
  2 of 2 chunks applied
  $ 
  $ tglogm
  @  22b319b2c5c1 'E'
  │
  o  c383b2e801f7 'D'
  │
  o  a2d898e6f479 'C'
  │
  o  112478962961 'B'
  │
  o  426bada5c675 'A'
  $ tglogm
  @  22b319b2c5c1 'E'
  │
  o  c383b2e801f7 'D'
  │
  o  a2d898e6f479 'C'
  │
  o  112478962961 'B'
  │
  o  426bada5c675 'A'

Landing

  $ cd ..
  $ newrepo
  $ drawdag << EOS
  > Y  A B C  # amend: A -> B -> C
  > |   \|/
  > Z    Z
  > EOS

Simulate pushrebase happening remotely and stripping the mutation information.

  $ drawdag --config mutation.enabled=false << EOS
  > X
  > |
  > $Y  $C  # rebase: $C -> X
  > EOS
  $ sl debugmakepublic $X

If we unhide B, we don't know that it was landed.

  $ sl unhide 'desc(B)'
  $ sl log -G -r "all()" -T "{desc} {mutation_descs}\n"
  o  X
  │
  │ o  B
  │ │
  o │  Y
  ├─╯
  o  Z
  
We can restore it by re-introducing the links via mutation records.  This is a temporary
hack until we write an indexed changelog that lets us do the successor lookup for any
commit cheaply.  Normally the pullcreatemarkers and pushrebase extensions will do this
for us, but for this test we do it manually.

  $ sl debugsh --hidden -c "with repo.lock(): s.mutation.recordentries(repo, [s.mutation.createsyntheticentry(repo, [repo[\"$C\"].node()], repo[\"$X\"].node(), \"land\")], skipexisting=False)"
  $ sl log -G -r "all()" -T "{desc} {mutation_descs}\n"
  o  X
  │
  │ x  B (Rewritten using land into X)
  │ │
  o │  Y
  ├─╯
  o  Z
  
Test debugmutation filtering of mutation info by date
  $ cd ..
  $ newrepo
  $ echo "base" > base
  $ sl commit -Aqm base
  $ echo "18316800" > file
  $ sl commit -Aqm c1
  $ for i in 18403200 18489600 18576000 18662400 18748800
  > do
  >   echo $i >> file
  >   sl amend -m "c1 (amended $i)" --config devel.default-date="$i 0"
  > done
  $ sl debugmutation
   *  5ace6d97ba5801022fb3b2b47eba651ee0d6fb00 amend by test at 1970-08-06T00:00:00 from:
      59a4ec3303deb98d755eda1ee2319b543b0db5a6 amend by test at 1970-08-05T00:00:00 from:
      fa37f25c1fbfd999bb6230235f33e2fbdf82944d amend by test at 1970-08-04T00:00:00 from:
      9192e2082d2773b40faea0dae95a4479fdcec7c2 amend by test at 1970-08-03T00:00:00 from:
      8567425fe92afee0db280f2c01783085691903b6 amend by test at 1970-08-02T00:00:00 from:
      315843a7e2114894c0e7345436313f20282907bd
  $ sl debugmutation -t "1970-08-05 to 1970-08-10"
   *  5ace6d97ba5801022fb3b2b47eba651ee0d6fb00 amend by test at 1970-08-06T00:00:00 from:
      59a4ec3303deb98d755eda1ee2319b543b0db5a6 amend by test at 1970-08-05T00:00:00 from:
      fa37f25c1fbfd999bb6230235f33e2fbdf82944d ...
  $ sl debugmutation -s -r 315843a -t "1970-08-01 to 1970-08-04" --hidden
   *  315843a7e2114894c0e7345436313f20282907bd amend by test at 1970-08-02T00:00:00 into:
      8567425fe92afee0db280f2c01783085691903b6 amend by test at 1970-08-03T00:00:00 into:
      9192e2082d2773b40faea0dae95a4479fdcec7c2 amend by test at 1970-08-04T00:00:00 into:
      fa37f25c1fbfd999bb6230235f33e2fbdf82944d ...
