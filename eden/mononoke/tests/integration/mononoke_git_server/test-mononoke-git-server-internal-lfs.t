# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

# Verify that --internal-lfs makes mononoke_git_service resolve LFS pointers by
# looking the object bytes up in the local Mononoke filestore (by SHA256 alias)
# instead of fetching them over HTTP from an upstream LFS server.
#
# Setup mirrors production but uses zero upstream-LFS plumbing:
#   1. mononoke_lfs_server is started WITHOUT --upstream, serving repo "repo".
#      It accepts client `git lfs push` uploads and writes them to the shared
#      blobstore under the object's SHA256 alias.
#   2. mononoke_git_service is started WITH --internal-lfs and WITHOUT
#      --upstream-lfs-server. On seeing an LFS pointer during a `git push`, it
#      looks up the SHA256 in the *same* blobstore the LFS server wrote to —
#      no HTTP fetch.
#
# So `git_client push` ends up talking to BOTH services:
#   * the git-lfs pre-push hook uploads bytes to mononoke_lfs_server, then
#   * `git push` sends the commit (containing the pointer) to mononoke_git_service.

  $ . "${TEST_FIXTURES}/library.sh"
  $ REPOTYPE="blob_files"
  $ GIT_LFS_INTERPRET_POINTERS=1 setup_common_config $REPOTYPE
  $ testtool_drawdag -R repo << EOF
  > A-B-C
  > # bookmark: C heads/master_bookmark
  > EOF
  A=aa53d24251ff3f54b1b2c29ae02826701b2abeb0079f1bb13b8434b54cd87675
  B=f8c75e41a0c4d29281df765f39de47bca1dcadfdc55ada4ccc2f6df567201658
  C=e32a1e342cdb1e38e88466b4c1a01ae9f410024017aa21dc0a1c5da6b3963bf2
  $ mononoke_admin derived-data -R repo derive -T git_commits -T git_delta_manifests_v2 -T unodes --all-bookmarks
  $ mononoke_admin git-symref -R repo create --symref-name HEAD --ref-name master_bookmark --ref-type branch
  Symbolic ref HEAD pointing to branch master_bookmark has been added

# Start a Mononoke LFS server with NO upstream. It only accepts client uploads
# into repo "repo"'s blobstore. The Scuba log gives us a per-request record we
# can later inspect to prove the git server stayed off the HTTP path.
# Use --tls so the server binds to `localhost` (reachable in the
# disable-all-network-access sandbox) rather than $LOCALIP (which is not).
  $ LFS_LOG="${TESTTMP}/lfs.log"
  $ SCUBA_LFS="${TESTTMP}/scuba_lfs.json"
  $ LFS_URL="$(lfs_server --tls --log "$LFS_LOG" --scuba-log-file "$SCUBA_LFS")/repo"

# Start mononoke_git_service with --internal-lfs and NO --upstream-lfs-server.
# The git server resolves pointers from the local filestore only.
  $ mononoke_git_service --internal-lfs
  $ set_mononoke_as_source_of_truth_for_git

# Clone the Git repo from Mononoke; wire the client's lfs.url to the LFS
# server above so `git lfs push` knows where to upload.
  $ quiet git_client lfs install
  $ CLONE_URL="$MONONOKE_GIT_SERVICE_BASE_URL/repo.git"
  $ quiet git_client clone --config "lfs.url=$LFS_URL" "$CLONE_URL"

# Make and push an LFS commit. The pre-push hook uploads object bytes to the
# LFS server (filling the filestore under the SHA256 alias); then git push
# sends the commit (containing only the pointer) to mononoke_git_service, which
# resolves the pointer from the *same* blobstore.
  $ cd $REPONAME
# Point git-lfs at the TLS LFS server and give it the client cert + identity
# header it needs to authenticate (mirrors configure_lfs_client_with_mononoke_server).
  $ git config --local lfs.url "$LFS_URL"
  $ git config --local http.sslCAInfo "$TEST_CERTDIR/root-ca.crt"
  $ git config --local http.sslCert "$TEST_CERTDIR/client0.crt"
  $ git config --local http.sslKey "$TEST_CERTDIR/client0.key"
  $ git config --local http.extraHeader "x-client-info: {\"request_info\": {\"entry_point\": \"CurlTest\", \"correlator\": \"test\"}}"
  $ echo "contents of LFS file resolved via internal filestore" > large_file
  $ git lfs track large_file
  Tracking "large_file"
  $ git add .gitattributes large_file
  $ git commit -aqm "new LFS change"
  $ LFS_HEAD=$(git rev-parse HEAD)
  $ quiet git_client push

# Show the resulting bonsai as JSON.
  $ mononoke_admin fetch -R repo -B heads/master_bookmark --json | jq
  {
    "changeset_id": "*", (glob)
    "parents": [
      "e32a1e342cdb1e38e88466b4c1a01ae9f410024017aa21dc0a1c5da6b3963bf2"
    ],
    "author": "mononoke <mononoke@mononoke>",
    "author_date": "2000-01-01T00:00:00Z",
    "committer": "mononoke <mononoke@mononoke>",
    "committer_date": "2000-01-01T00:00:00Z",
    "message": "new LFS change\n",
    "hg_extra": {
      "convert_revision": "eea09ac2c99400ba0434a4f95afb49dca099247c",
      "hg-git-rename-source": "git"
    },
    "file_changes": {
      ".gitattributes": {
        "Change": {
          "inner": {
            "content_id": "*", (glob)
            "file_type": "Regular",
            "size": 47,
            "git_lfs": "FullContent"
          },
          "copy_from": null
        }
      },
      "large_file": {
        "Change": {
          "inner": {
            "content_id": "*", (glob)
            "file_type": "Regular",
            "size": 53,
            "git_lfs": {
              "GitLfsPointer": {
                "non_canonical_pointer": null
              }
            }
          },
          "copy_from": null
        }
      }
    },
    "subtree_changes": {}
  }

# Verify the LFS content was resolved from the local filestore and stored in
# Mononoke's blobstore.
  $ CONTENT_ID=$(mononoke_admin fetch -R repo -B heads/master_bookmark --json | jq -r '.file_changes.large_file.Change.inner.content_id')
  $ mononoke_admin filestore -R repo fetch --content-id "$CONTENT_ID"
  contents of LFS file resolved via internal filestore

# Prove the git server never reached out to the LFS server while processing
# the push. The LFS server's Scuba log should contain only the client's
# pre-push activity: a `batch` API call followed by an `upload` PUT. If the
# git server had taken the HTTP path it would have issued `download_sha256`
# (or `download`) requests, which would show up here too. Anything we run
# after this point (notably `git lfs pull` below) will add `batch`/`download`
# entries — but it's the *client* doing them, not the git server.
  $ jq -r 'select(.normal.method != null) | .normal.method' "$SCUBA_LFS" | sort -u
  batch
  upload

  $ wait_for_git_bookmark_move refs/heads/master_bookmark "" "$LFS_HEAD"

# Re-clone the repo with GIT_LFS_SKIP_SMUDGE=1 so git-lfs does NOT replace
# pointer files with their content during checkout. `large_file` stays as the
# raw LFS pointer text we can inspect and hash-check.
  $ cd "$TESTTMP"
  $ rm -rf "$REPONAME"
  $ GIT_LFS_SKIP_SMUDGE=1 quiet git_client clone --config "lfs.url=$LFS_URL" "$CLONE_URL"
  $ cd $REPONAME
# Re-apply the LFS client auth config in the fresh clone so `git lfs pull` below
# can reach the TLS LFS server.
  $ git config --local lfs.url "$LFS_URL"
  $ git config --local http.sslCAInfo "$TEST_CERTDIR/root-ca.crt"
  $ git config --local http.sslCert "$TEST_CERTDIR/client0.crt"
  $ git config --local http.sslKey "$TEST_CERTDIR/client0.key"
  $ git config --local http.extraHeader "x-client-info: {\"request_info\": {\"entry_point\": \"CurlTest\", \"correlator\": \"test\"}}"
  $ cat large_file
  version https://git-lfs.github.com/spec/v1
  oid sha256:79f9e0f42c2fb07686f85ace2668024f867f8b65d04c572bd0f1b4a39c56cb4a
  size 53

# Read the pointer's sha256, fetch the bytes back out of the Mononoke filestore
# via `mononoke_admin filestore fetch`, hash them with sha256sum, and verify
# the sha256 of the fetched bytes matches the value embedded in the pointer.
  $ POINTER_SHA256=$(sed -n 's/^oid sha256://p' large_file)
  $ FILESTORE_SHA256=$(mononoke_admin filestore -R repo fetch --content-id "$CONTENT_ID" | sha256sum | awk '{print $1}')
  $ echo "$FILESTORE_SHA256"
  79f9e0f42c2fb07686f85ace2668024f867f8b65d04c572bd0f1b4a39c56cb4a
  $ [ "$POINTER_SHA256" = "$FILESTORE_SHA256" ] && echo "pointer matches filestore sha256"
  pointer matches filestore sha256

# Pull the actual LFS content down from the LFS server. `git lfs pull` is
# `git lfs fetch` (download bytes to the local LFS cache) followed by
# `git lfs checkout` (replace working-copy pointers with the cached bytes) —
# `git lfs checkout` alone is a no-op until the bytes are in the cache.
  $ quiet git_client lfs pull
  $ cat large_file
  contents of LFS file resolved via internal filestore
