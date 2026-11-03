# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

# A path is a sequence of bytes that need not be valid UTF-8. Mercurial writes
# the file names of a changeset into the changeset text verbatim, and that text
# is what the hg client gets from the commit-data endpoint and what the hg
# changeset id hashes, so the file name must come back as the bytes that were
# committed.
# The file names are written as raw bytes, so the text is bit-for-bit the one
# Mercurial writes.

  $ . "${TEST_FIXTURES}/library.sh"

  $ GIT_REPO="${TESTTMP}/repo-git"
  $ HG_REPO="${TESTTMP}/repo"
  $ NAME="$(printf 'caf\xe9')"

  $ setup_common_config

# Only git can create a file whose name is not valid UTF-8 here, so the commit
# is made with git and imported (as in test-getpackv2-multibyte-filenames.t)
  $ mkdir "$GIT_REPO"
  $ cd "$GIT_REPO"
  $ git init -q
  $ echo "foo" > "$NAME"
  $ git add "$NAME"
  $ git commit -qm "Add a latin-1 file name"

# The file name git committed: the raw bytes "caf\xe9"
  $ git -c core.quotePath=false ls-tree --name-only HEAD | cat -v
  cafM-i

  $ quiet gitimport "$GIT_REPO" --derive-hg full-repo
  $ BONSAI="$(mononoke_admin convert -R repo --from git --to bonsai "$(git rev-parse HEAD)")"
  $ HGID="$(mononoke_admin convert -R repo --from bonsai --to hg "$BONSAI")"
  $ mononoke_admin bookmarks -R repo set master_bookmark "$BONSAI"
  Creating publishing bookmark master_bookmark at * (glob)

  $ start_and_wait_for_mononoke_server
  $ cd "$TESTTMP"
  $ hg clone -q mono:repo "$HG_REPO" --noupdate
  $ cd "$HG_REPO"

# The changeset text the server serves for that commit carries the file name as
# "caf\xe9", the bytes of the path.
  $ hg debugapi -e commitdata -i "['$HGID']"
  [{"hgid": bin("*"), (glob)
    "revlog_data": b"*\ncaf\xe9\n\nAdd a latin-1 file name\n"}] (glob)
