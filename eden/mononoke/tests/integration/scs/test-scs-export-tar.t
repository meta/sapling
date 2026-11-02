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
  $ scsc export -R repo -B main -p "" --tar -o export1.tar
  $ tar tf export1.tar | sort
  repo/a/b/c
  repo/a/b/d
  repo/a/e
  repo/f/g
  repo/f/h
  repo/i
  repo/j

Check file is correctly exported.
  $ tar xfO export1.tar repo/a/b/c
  a/b/c

Check symlink is correctly exported.
  $ mkdir exported
  $ tar -xf export1.tar -C exported
  $ cat exported/repo/j
  a/b/c (no-eol)

Export a single directory.
  $ scsc export -R repo -B main -p a --tar -o export2.tar
  $ tar tf export2.tar | sort
  repo/b/c
  repo/b/d
  repo/e

Export list of paths relative to root.
  $ cat >list3 <<EOF
  > a/b/
  > f
  > i
  > EOF
  $ scsc export -R repo -B main -p "" --tar -o export3.tar --path-list-file list3
  $ tar tf export3.tar | sort
  repo/a/b/c
  repo/a/b/d
  repo/f/g
  repo/f/h
  repo/i

Export paths relative to a provided directory.  Note: "e" not included as "e/"
was requested, but "a/e" is a file, and "z" is ignored as "a/z" does not exist.
  $ cat >list4 <<EOF
  > b/c
  > e/
  > z
  > EOF
  $ scsc export -R repo -B main -p "a" --tar -o export4.tar --path-list-file list4
  $ tar tf export4.tar | sort
  repo/b/c

Case insensitive export
  $ scsc export -R repo -B main -p "A/B" --tar -o export5.tar --case-insensitive
  $ tar tf export5.tar | sort
  repo/c
  repo/d

Case insensitive export of path list file.
  $ cat >list6 <<EOF
  > A/B/C
  > F
  > EOF
  $ scsc export -R repo -B main -p "" --tar -o export6.tar --case-insensitive --path-list-file list6
  $ tar tf export6.tar | sort
  repo/a/b/c
  repo/f/g
  repo/f/h

Export to stdout.
  $ scsc export -R repo -B main -p "a" --tar -o - | gzip > export7.tar.gz
  $ tar tzf export7.tar.gz | sort
  repo/b/c
  repo/b/d
  repo/e
