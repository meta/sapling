# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"

Redacted content is still stored, and Walker reads it with redaction disabled.
Strider does too: a redacted file's content is checked like any other, so a
missing blob is still found.

  $ setup_common_config "blob_files"
  $ cd "$TESTTMP"
  $ testtool_drawdag -R repo --derive-all --no-default-files <<'EOF'
  > A-B
  > # modify: A "file" "a"
  > # modify: B "secret" "secret"
  > # bookmark: B master_bookmark
  > EOF
  A=* (glob)
  B=* (glob)

  $ mononoke_admin redaction create-key-list -R repo -i "$B" secret --main-bookmark master_bookmark --force --output-file rs --skip-aws-sync > /dev/null 2>&1
  $ cat > "$REDACTION_CONF/redaction_sets" <<EOF
  > {"all_redactions": [{"reason": "T1", "id": "$(cat rs)", "enforce": true}]}
  > EOF
  $ mononoke_admin redaction list -R repo -i "$B" 2>&1 | grep -v Searching
  Found 1 redacted paths
  T1                  : secret

  $ strider() {
  >   render_mononoke_config
  >   GLOG_minloglevel=5 "$MONONOKE_STRIDER" \
  >     "${CACHE_ARGS[@]}" \
  >     "${COMMON_ARGS[@]}" \
  >     --repo-id "$REPOID" \
  >     --config-tier "$MONONOKE_CONFIG_TIER" \
  >     --tracing-test-format \
  >     --max-concurrency 4 \
  >     --max-loads-per-commit 4 \
  >     --print-visited-nodes \
  >     --family core \
  >     --chunk-size 2 > strider.out 2> strider.err
  > }

B's redacted content is read and validated.

  $ strider
  $ grep ' status=' strider.out | sed -e "s/$A/A/" -e "s/$B/B/" | LC_ALL=C sort
  A core status=validated
  B core status=validated

With that content's blob removed, B's core reports it missing.

  $ SECRET=$(grep "^$B core FileContent:" strider.out | cut -d: -f2)
  $ rm "$TESTTMP/blobstore/blobs/blob-repo0000.content.blake2.$SECRET"
  $ strider
  [3]
  $ grep 'Strider family incomplete' strider.err | sed -nE \
  >   's/.*changeset_id="?([0-9a-f]+)"? family="?([a-z-]+)"? failure_stage="?([a-z]+)"? failure_class="?([a-z_]+)"?.*/\1 \2 \3 \4/p' |
  >   sed -e "s/$B/B/"
  B core load missing
