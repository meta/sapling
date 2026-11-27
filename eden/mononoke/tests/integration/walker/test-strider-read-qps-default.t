# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"

Without --blobstore-read-qps, Strider limits reads to Walker's default rate.

  $ "$MONONOKE_STRIDER" --help | awk '/^ *--blobstore-read-qps /{p=1; print; next} /^ *-/{p=0} p' | grep -o '\[default: [^]]*\]'
  [default: 20000]
