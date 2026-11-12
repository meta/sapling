# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"
  $ setup_common_config "blob_files"

Derive-all persists phases for publishing-bookmark ancestry while leaving
unpublished descendants and unrelated commits draft.

  $ quiet testtool_drawdag -R repo --derive-all <<'EOF'
  > A-B-C
  > D
  > # bookmark: B main
  > EOF

Read persisted phases without deriving them in the assertion.

  $ mononoke_admin phases -R repo list-public | sort > actual_public
  $ printf '%s\n' "$A" "$B" | sort > expected_public
  $ diff -u expected_public actual_public
