# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"

Setup config repo:
  $ setup_common_config
  $ setup_configerator_configs
  $ start_and_wait_for_mononoke_server
  $ cd "$TESTTMP"

Setup testing repo for mononoke:
  $ hginit_treemanifest repo
  $ cd repo

Create root commit:
  $ echo "hello" > a
  $ hg commit -qAm "root" -u "root" -d "2020-01-01"
  $ export ROOT_HASH="$(hg --debug id -i)"

Create commit B from root (adds file b="world"):
  $ echo "world" > b
  $ hg commit -qAm "first" -u "alice" -d "2021-01-01"
  $ export COMMIT_B="$(hg --debug id -i)"

Go back to root and create commit C with same tree but different metadata:
  $ hg checkout -q "$ROOT_HASH"
  $ echo "world" > b
  $ hg commit -qAm "second" -u "bob" -d "2022-06-15"
  $ export COMMIT_C="$(hg --debug id -i)"

Create commit D from root with different file content:
  $ hg checkout -q "$ROOT_HASH"
  $ echo "different" > c
  $ hg commit -qAm "third" -u "charlie" -d "2023-01-01"
  $ export COMMIT_D="$(hg --debug id -i)"

Start SCS server:
  $ start_and_wait_for_scs_server --scuba-log-file "$TESTTMP/scuba.json"

Configure repo to push to mononoke:
  $ cat >> .hg/hgrc <<EOF
  > [paths]
  > default = mono:repo
  > EOF

Push all branches to mononoke:
  $ hg push --to master_bookmark --create -r "$COMMIT_B" -q
  $ hg push --to branch_c --create -r "$COMMIT_C" -q
  $ hg push --to branch_d --create -r "$COMMIT_D" -q

Look up bonsai IDs:
  $ BONSAI_B=$(scsc lookup --repo repo -i "$COMMIT_B" -S bonsai)
  $ BONSAI_C=$(scsc lookup --repo repo -i "$COMMIT_C" -S bonsai)
  $ BONSAI_D=$(scsc lookup --repo repo -i "$COMMIT_D" -S bonsai)

The default V2 (ContentManifest) fingerprint returns a deterministic hash:
  $ scsc commit-fingerprint -R repo --bonsai-id "$BONSAI_B"
  9b56d2f11674d12120e8c6048ae5d4b36192c943b24a6cec05e6647e2a4de388

The default JSON output identifies the fingerprint as V2:
  $ scsc commit-fingerprint -R repo --bonsai-id "$BONSAI_B" --json
  {"fingerprint":"9b56d2f11674d12120e8c6048ae5d4b36192c943b24a6cec05e6647e2a4de388","version":"V2"}

Explicit V2 selection remains supported:
  $ scsc commit-fingerprint -R repo --bonsai-id "$BONSAI_B" --version 2
  9b56d2f11674d12120e8c6048ae5d4b36192c943b24a6cec05e6647e2a4de388

The same tree with different metadata produces the same fingerprint:
  $ FP_B=$(scsc commit-fingerprint -R repo --bonsai-id "$BONSAI_B")
  $ FP_C=$(scsc commit-fingerprint -R repo --bonsai-id "$BONSAI_C")
  $ [ "$FP_B" = "$FP_C" ] && echo "PASS: same fingerprint" || echo "FAIL: fingerprints differ: $FP_B vs $FP_C"
  PASS: same fingerprint

Fingerprint should be 64 hex chars (32 bytes):
  $ echo "$FP_B" | wc -c
  65

Different trees produce different fingerprints:
  $ FP_D=$(scsc commit-fingerprint -R repo --bonsai-id "$BONSAI_D")
  $ [ "$FP_B" != "$FP_D" ] && echo "PASS: different fingerprints" || echo "FAIL: fingerprints are the same"
  PASS: different fingerprints

A non-existent commit fails:
  $ scsc commit-fingerprint -R repo --bonsai-id 0000000000000000000000000000000000000000000000000000000000000000 2>&1
  * error: SourceControlService::commit_fingerprint failed with request_error(RequestError): RequestError: commit not found (repo=repo commit=0000000000000000000000000000000000000000000000000000000000000000) (glob)
  [1]

V1 selection is rejected by scsc:
  $ scsc commit-fingerprint -R repo --bonsai-id "$BONSAI_B" --version 1
  error: invalid value '1' for '--version <VERSION>'
    [possible values: 2]
  
  For more information, try '--help'.
  [2]

Scuba logging captures only V2 requests:
  $ jq -r 'select(.normal.method == "commit_fingerprint") | .normal.fingerprint_version // empty' "$TESTTMP/scuba.json" | sort -u
  CommitFingerprintVersion::V2
