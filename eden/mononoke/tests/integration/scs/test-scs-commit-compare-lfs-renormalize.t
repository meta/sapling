# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

# Integration test for commit_compare over LFS-renormalize commits (SEV S657050).
# D's parent C has no FileChange entry for the renormalized paths, so LFS state
# is inherited from A through B and C. Text flips diff raw text against pointer
# text; non-text flips diff a short sentinel against pointer text.

  $ . "${TEST_FIXTURES}/library.sh"

Setup config repo with LFS pointer interpretation enabled:
  $ GIT_LFS_INTERPRET_POINTERS=1 setup_common_config
  $ setup_configerator_configs
  $ start_and_wait_for_mononoke_server
  $ start_and_wait_for_scs_server

Build the renormalize stack. Resolved content is identical on both sides
for every renormalized path.
  $ testtool_drawdag -R repo <<'EOF'
  > A-B-C-D
  > # bookmark: D main
  > # modify: A "raw_to_lfs.txt" "raw content here"
  > # modify: A "lfs_to_raw.txt" regular lfs_pointer "shared blob"
  > # modify: A "raw_binary_to_lfs.bin" "\x00raw binary here"
  > # modify: A "lfs_to_raw_binary.bin" regular lfs_pointer "\x00shared binary"
  > # modify: A "raw_nonutf8_to_lfs.bin" "\xffraw binary here"
  > # modify: A "anchor.txt" "anchor A"
  > # modify: B "anchor.txt" "anchor B"
  > # modify: C "anchor.txt" "anchor C"
  > # modify: D "raw_to_lfs.txt" regular lfs_pointer "raw content here"
  > # modify: D "lfs_to_raw.txt" "shared blob"
  > # modify: D "raw_binary_to_lfs.bin" regular lfs_pointer "\x00raw binary here"
  > # modify: D "lfs_to_raw_binary.bin" "\x00shared binary"
  > # modify: D "raw_nonutf8_to_lfs.bin" regular lfs_pointer "\xffraw binary here"
  > EOF
  A=* (glob)
  B=* (glob)
  C=* (glob)
  D=* (glob)

== All renormalized paths surface in commit_compare D vs C ==
  $ scsc diff --repo repo --bonsai-id "$C" --bonsai-id "$D" --paths-only --ordered 2>&1 | grep -v 'BatchPriorityResolver'
  A D
  M lfs_to_raw.txt
  M lfs_to_raw_binary.bin
  M raw_binary_to_lfs.bin
  M raw_nonutf8_to_lfs.bin
  M raw_to_lfs.txt

== Body for raw_to_lfs.txt (enabling LFS): unified diff ==
  $ scsc diff --repo repo --bonsai-id "$C" --bonsai-id "$D" --path "raw_to_lfs.txt" 2>&1 | grep -v 'BatchPriorityResolver'
  diff --git a/raw_to_lfs.txt b/raw_to_lfs.txt
  --- a/raw_to_lfs.txt
  +++ b/raw_to_lfs.txt
  @@ -1,1 +1,3 @@
  -raw content here
  \ No newline at end of file
  +version https://git-lfs.github.com/spec/v1
  +oid sha256:* (glob)
  +size 16

== Body for lfs_to_raw.txt (disabling LFS): unified diff ==
  $ scsc diff --repo repo --bonsai-id "$C" --bonsai-id "$D" --path "lfs_to_raw.txt" 2>&1 | grep -v 'BatchPriorityResolver'
  diff --git a/lfs_to_raw.txt b/lfs_to_raw.txt
  --- a/lfs_to_raw.txt
  +++ b/lfs_to_raw.txt
  @@ -1,3 +1,1 @@
  -version https://git-lfs.github.com/spec/v1
  -oid sha256:* (glob)
  -size 11
  +shared blob
  \ No newline at end of file

== Body for raw_binary_to_lfs.bin (enabling LFS, binary content): LFS status hunk ==
  $ scsc diff --repo repo --bonsai-id "$C" --bonsai-id "$D" --path "raw_binary_to_lfs.bin" 2>&1 | grep -v 'BatchPriorityResolver'
  diff --git a/raw_binary_to_lfs.bin b/raw_binary_to_lfs.bin
  --- a/raw_binary_to_lfs.bin
  +++ b/raw_binary_to_lfs.bin
  @@ -1,1 +1,3 @@
  -Git LFS: none (non-text content omitted)
  +version https://git-lfs.github.com/spec/v1
  +oid sha256:* (glob)
  +size 16

== Body for lfs_to_raw_binary.bin (disabling LFS, binary content): LFS status hunk ==
  $ scsc diff --repo repo --bonsai-id "$C" --bonsai-id "$D" --path "lfs_to_raw_binary.bin" 2>&1 | grep -v 'BatchPriorityResolver'
  diff --git a/lfs_to_raw_binary.bin b/lfs_to_raw_binary.bin
  --- a/lfs_to_raw_binary.bin
  +++ b/lfs_to_raw_binary.bin
  @@ -1,3 +1,1 @@
  -version https://git-lfs.github.com/spec/v1
  -oid sha256:* (glob)
  -size 14
  +Git LFS: none (non-text content omitted)

== Body for raw_nonutf8_to_lfs.bin (no NUL byte, still non-text): LFS status hunk ==
  $ scsc diff --repo repo --bonsai-id "$C" --bonsai-id "$D" --path "raw_nonutf8_to_lfs.bin" 2>&1 | grep -v 'BatchPriorityResolver'
  diff --git a/raw_nonutf8_to_lfs.bin b/raw_nonutf8_to_lfs.bin
  --- a/raw_nonutf8_to_lfs.bin
  +++ b/raw_nonutf8_to_lfs.bin
  @@ -1,1 +1,3 @@
  -Git LFS: none (non-text content omitted)
  +version https://git-lfs.github.com/spec/v1
  +oid sha256:* (glob)
  +size 16
