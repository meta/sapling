# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"

Setup config repo:
  $ POPULATE_GIT_MAPPING=1 setup_common_config
  $ cd "$TESTTMP"

start mononoke
  $ start_and_wait_for_mononoke_server
setup repo
  $ hg clone -q mono:repo repo
  $ cd repo

  $ for path in a/b/c a/b/d a/e f/g f/h i
  > do
  >   mkdir -p $(dirname $path)
  >   echo $path > $path
  > done
  > ln -sf a/b/c j
  $ hg commit -Aqm base
  $ hg push -q --to main --create

start SCS server
  $ cd $TESTTMP
  $ start_and_wait_for_scs_server

Export whole repo.
  $ scsc export -R repo -B main -p "" -o export1
  $ find export1 -type f,l | sort
  export1/a/b/c
  export1/a/b/d
  export1/a/e
  export1/f/g
  export1/f/h
  export1/i
  export1/j

Check file is correctly exported.
  $ cat export1/a/b/c
  a/b/c

Check symlink is correctly exported.
  $ cat export1/j
  a/b/c

Export a single directory.
  $ scsc export -R repo -B main -p a -o export2
  $ find export2 -type f | sort
  export2/b/c
  export2/b/d
  export2/e

Export list of paths relative to root.
  $ cat >list3 <<EOF
  > a/b/
  > f
  > i
  > EOF
  $ scsc export -R repo -B main -p "" -o export3 --path-list-file list3
  $ find export3 -type f | sort
  export3/a/b/c
  export3/a/b/d
  export3/f/g
  export3/f/h
  export3/i

Export paths relative to a provided directory.  Note: "e" not included as "e/"
was requested, but "a/e" is a file, and "z" is ignored as "a/z" does not exist.
  $ cat >list4 <<EOF
  > b/c
  > e/
  > z
  > EOF
  $ scsc export -R repo -B main -p "a" -o export4 --path-list-file list4
  $ find export4 -type f
  export4/b/c

Case insensitive export
  $ scsc export -R repo -B main -p "A/B" -o export5 --case-insensitive
  $ find export5 -type f | sort
  export5/c
  export5/d

Case insensitive export of path list file.
  $ cat >list6 <<EOF
  > A/B/C
  > F
  > EOF
  $ scsc export -R repo -B main -p "" -o export6 --case-insensitive --path-list-file list6
  $ find export6 -type f | sort
  export6/a/b/c
  export6/f/g
  export6/f/h

Can specify a list of paths to exclude as well.
  $ cat >include <<EOF
  > a
  > f
  > EOF
  $ cat >exclude <<EOF
  > a/b
  > f/g
  > EOF
  $ scsc export -R repo -B main -p "" -o export7 --path-list-file include --exclude-path-list-file exclude
  $ find export7 -type f | sort
  export7/a/e
  export7/f/h

Can filter files by size.
  $ cat >include <<EOF
  > a/b/c
  > a/e
  > EOF
  $ scsc export -R repo -B main -p "" -o export8 --path-list-file include --file-size-threshold=5B
Meets size threshold - put in placeholder:
  $ cat export8/a/b/c
  This is a placeholder for a large file
  
  Original file id: 5011c6b96fb59126fb259a386b2cbbf44aad39afe03834bf00226cef9e8bcafc
  Original file size: 6
Below threshold - leave content:
  $ cat export8/a/e
  a/e


  $ cd ~/repo
  $ mkdir binary
  $ printf '\0\n' > binary/small
  $ printf 'really big \0 binary file' > binary/big
  $ hg ci -Aqm "binary files"
  $ hg push -q --to main
  $ mononoke_admin --log-level ERROR derived-data -R repo derive --all-types -B main

  $ cd
  $ scsc export -R repo -B main -p "" -o export9 --binary-file-size-threshold=3B
  $ find export9 -type f | sort
  export9/a/b/c
  export9/a/b/d
  export9/a/e
  export9/binary/big
  export9/binary/small
  export9/f/g
  export9/f/h
  export9/i

Binary and large - placeholder:
  $ cat export9/binary/big
  This is a placeholder for a large file
  
  Original file id: e39aa937e1b78a945834c4414b0282a1f8037f29b1206302714025dfa9183867
  Original file size: 24

Not above threshold - keep file:
  $ cat export9/binary/small | sed 's/\x0/\\0/g'
  \0

Non-binary files not affected:
  $ cat export9/a/b/c
  a/b/c
