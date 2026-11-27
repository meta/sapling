# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"

Over full history, the core family must report the same file content metadata
and alias mappings as the deployed scrub-derived Walker job, and report a
missing one against the commit that added its content. See
test-strider-chunked-deep-history.t for the Walker and Strider arguments.

C adds a file whose content A already added, so both report its metadata and
aliases.

  $ setup_common_config "blob_files"
  $ cd "$TESTTMP"
  $ testtool_drawdag -R repo --derive-all --no-default-files <<'EOF'
  > A-B-C
  > # modify: A "dir/a" "one"
  > # modify: A "b" "two"
  > # modify: B "dir/a" "one-b"
  > # delete: B "b"
  > # modify: C "c" "one"
  > # bookmark: C master
  > EOF
  A=* (glob)
  B=* (glob)
  C=* (glob)

  $ BLOBS="$TESTTMP/blobstore/blobs"
  $ names() {
  >   sed -e "s/$A/A/g" -e "s/$B/B/g" -e "s/$C/C/g"
  > }

Each identity is printed as type, kind and hash. Strider prints a seeded
Blake3 alias by its hash type alone.

  $ identities() {
  >   sed -nE \
  >     -e 's/^Node FileContentMetadataV2\(ContentId\(Blake2\(([0-9a-f]+)\)\)\).*/FileContentMetadataV2:\1/p' \
  >     -e 's/^Node AliasContentMapping\(AliasKey\(([A-Za-z0-9]+)\([A-Za-z0-9]+\(([0-9a-f]+)\)\)\)\).*/AliasContentMapping:\1:\2/p' \
  >     -e 's/^(FileContentMetadataV2):([0-9a-f]+)$/\1:\2/p' \
  >     -e 's/^(AliasContentMapping):Blake3\(([0-9a-f]+)\)$/\1:SeededBlake3:\2/p' \
  >     -e 's/^(AliasContentMapping):([A-Za-z0-9]+)\(([0-9a-f]+)\)$/\1:\2:\3/p' \
  >   | LC_ALL=C sort -u
  > }
  $ walker_scrub_derived() {
  >   mononoke_walker scrub -q \
  >     -I deep -i bonsai -i FileContent -i derived_deleted_manifest \
  >     -i derived_skeleton_manifests -i derived_changeset_info \
  >     -i FileContentMetadataV2 -i AliasContentMapping \
  >     --chunk-direction OldestFirst \
  >     --chunk-by-public Changeset --chunk-by-public ChangesetInfoMapping \
  >     --chunk-by-public DeletedManifestV2Mapping \
  >     --chunk-by-public SkeletonManifestMapping \
  >     --chunk-size 100 \
  >     --include-output-node-type FileContentMetadataV2 \
  >     --include-output-node-type AliasContentMapping \
  >     --output-format Debug "$@" > walker.out 2>&1
  > }
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
  >     --chunk-size 100 \
  >     "$@" > strider.out 2> strider.err
  > }

Each commit's results, then each failure as commit, family, stage, class and
object.

  $ outcomes() {
  >   grep ' status=' strider.out | names | LC_ALL=C sort
  >   grep 'Strider family incomplete' strider.err | sed -nE \
  >     's/.*changeset_id="?([0-9a-f]+)"? family="?([a-z-]+)"? failure_stage="?([a-z]+)"? failure_class="?([a-z_]+)"? object="?([^" ]+)"?.*/\1 \2 \3 \4 \5/p' \
  >   | names | LC_ALL=C sort
  > }

Three distinct contents, each with its metadata and four aliases.

  $ walker_scrub_derived
  $ cp walker.out walker-full.out
  $ identities < walker.out > walker.nodes
  $ cut -d: -f1 walker.nodes | uniq -c
       12 AliasContentMapping
        3 FileContentMetadataV2

Strider validates every commit and reports exactly Walker's content types.
Each content, its metadata and its four aliases are reported once per commit
that adds the content.

  $ strider
  $ outcomes
  A core status=validated
  B core status=validated
  C core status=validated
  $ grep -v ' status=' strider.out | cut -d' ' -f3 | identities > strider.nodes
  $ diff -u walker.nodes strider.nodes
  $ grep -v ' status=' strider.out | names | cut -d' ' -f1,3 | cut -d: -f1 | LC_ALL=C sort | uniq -c
        8 A AliasContentMapping
        1 A Changeset
        2 A FileContent
        2 A FileContentMetadataV2
        4 B AliasContentMapping
        1 B Changeset
        1 B FileContent
        1 B FileContentMetadataV2
        4 C AliasContentMapping
        1 C Changeset
        1 C FileContent
        1 C FileContentMetadataV2

Only B adds "one-b". With its SHA-256 alias mapping removed, both tools fail on
it, and Strider reports it against B alone.

  $ ONE_B=$(printf 'one-b' | sha256sum | cut -d' ' -f1)
  $ grep -c "^AliasContentMapping:Sha256:$ONE_B$" walker.nodes
  1
  $ mv "$BLOBS/blob-repo0000.alias.sha256.$ONE_B" "$TESTTMP/alias"
  $ walker_scrub_derived
  [1]
  $ grep -m1 'Could not step to' walker.out | grep -c "target: AliasContentMapping(AliasKey(Sha256(Sha256($ONE_B))))"
  1
  $ strider
  [3]
  $ outcomes | sed "s/$ONE_B/ONE_B/"
  A core status=validated
  B core status=incomplete
  C core status=validated
  B core load missing AliasContentMapping:Sha256(ONE_B)
  $ mv "$TESTTMP/alias" "$BLOBS/blob-repo0000.alias.sha256.$ONE_B"

With that alias blob present but not decodable, Strider reports it unreadable
with its storage key, the run's status is `unreadable`, and it exits 3.

  $ cp "$BLOBS/blob-repo0000.alias.sha256.$ONE_B" "$TESTTMP/alias"
  $ printf 'garbage' > "$BLOBS/blob-repo0000.alias.sha256.$ONE_B"
  $ strider
  [3]
  $ outcomes | sed "s/$ONE_B/ONE_B/"
  A core status=validated
  B core status=incomplete
  C core status=validated
  B core load unreadable AliasContentMapping:Sha256(ONE_B)
  $ grep 'Strider family incomplete' strider.err | grep -oE 'storage_key="?[^" ]+' | tr -d '"' | sed "s/$ONE_B/ONE_B/"
  storage_key=alias.sha256.ONE_B
  $ grep 'Strider completed' strider.err | grep -oE 'status="?[a-z]+"?' | tr -d '"'
  status=unreadable
  $ mv "$TESTTMP/alias" "$BLOBS/blob-repo0000.alias.sha256.$ONE_B"

Only A adds "two". With its metadata removed, Walker's read-only lookup
treats it as not yet computed: it reports no failure and skips the four
aliases. Strider recomputes the metadata from the content, as production
readers do, checks the SHA-1, SHA-256 and Git SHA-1 aliases against it, counts
the absent record and stores nothing. It does not check the seeded Blake3
alias, which was stored together with current metadata.

  $ TWO=$(grep -F "AliasKey(Sha256(Sha256($(printf 'two' | sha256sum | cut -d' ' -f1)))))" walker-full.out \
  >   | sed -nE 's/.*NodeData: Some\(AliasContentMapping\(ContentId\(Blake2\(([0-9a-f]+)\)\)\)\).*/\1/p')
  $ grep -c "^FileContentMetadataV2:$TWO$" walker.nodes
  1
  $ mv "$BLOBS/blob-repo0000.content_metadata2.blake2.$TWO" "$TESTTMP/metadata"
  $ walker_scrub_derived
  $ grep -cF "Node FileContentMetadataV2(ContentId(Blake2($TWO))): NodeData: Some(FileContentMetadataV2(None))" walker.out
  1
  $ identities < walker.out | diff walker.nodes - | grep '^[<>]' | cut -d: -f1,2
  < AliasContentMapping:GitSha1
  < AliasContentMapping:SeededBlake3
  < AliasContentMapping:Sha1
  < AliasContentMapping:Sha256
  $ strider
  $ outcomes
  A core status=validated
  B core status=validated
  C core status=validated
  $ grep -v ' status=' strider.out | cut -d' ' -f3 | identities | diff walker.nodes - | grep '^[<>]' | cut -d: -f1,2
  < AliasContentMapping:SeededBlake3
  $ grep 'Strider notice' strider.err | names | sed "s/$TWO/TWO/"
  [INFO] Strider notices repo="repo" notice="old_format_content_metadata" count=0
  [INFO] Strider notices repo="repo" notice="absent_content_metadata" count=1
  $ test -e "$BLOBS/blob-repo0000.content_metadata2.blake2.$TWO"
  [1]

A lost upload leaves both the content and its metadata absent. Walker then
fails on the content, and so does Strider, rather than on the metadata.

  $ mv "$BLOBS/blob-repo0000.content.blake2.$TWO" "$TESTTMP/content"
  $ walker_scrub_derived
  [1]
  $ grep -m1 'Could not step to' walker.out | grep -c "target: FileContent(ContentId(Blake2($TWO)))"
  1
  $ strider
  [3]
  $ outcomes | sed "s/$TWO/TWO/"
  A core status=incomplete
  B core status=validated
  C core status=validated
  A core load missing FileContent:TWO
  $ mv "$TESTTMP/content" "$BLOBS/blob-repo0000.content.blake2.$TWO"
  $ mv "$TESTTMP/metadata" "$BLOBS/blob-repo0000.content_metadata2.blake2.$TWO"

Metadata stored before spring 2023 lacks `is_generated`,
`is_partially_generated` and `seeded_blake3`, the record's last three fields,
so it does not decode. Rewrite A's metadata of "two" that way: drop the 36
bytes those fields take and keep the struct's stop byte.

  $ META="$BLOBS/blob-repo0000.content_metadata2.blake2.$TWO"
  $ cp "$META" "$TESTTMP/metadata"
  $ head -c -37 "$TESTTMP/metadata" > "$META"
  $ printf '\0' >> "$META"
  $ cp "$META" "$TESTTMP/old-metadata"

Walker fails on it, unless the record is treated as data, as a scrub job can
be configured to do: then it skips the four aliases, as for absent metadata.

  $ walker_scrub_derived
  [1]
  $ grep -m1 'Could not step to' walker.out | grep -c "target: FileContentMetadataV2(ContentId(Blake2($TWO)))"
  1
  $ walker_scrub_derived --error-as-data-node-type FileContentMetadataV2
  $ identities < walker.out | diff walker.nodes - | grep '^[<>]' | cut -d: -f1,2
  < AliasContentMapping:GitSha1
  < AliasContentMapping:SeededBlake3
  < AliasContentMapping:Sha1
  < AliasContentMapping:Sha256

Strider recomputes the metadata from the content, checks the three aliases
other than seeded Blake3 against it, agrees with Walker on everything else,
counts the record and stores nothing.

  $ strider
  $ outcomes
  A core status=validated
  B core status=validated
  C core status=validated
  $ grep -v ' status=' strider.out | cut -d' ' -f3 | identities | diff walker.nodes - | grep '^[<>]' | cut -d: -f1,2
  < AliasContentMapping:SeededBlake3
  $ grep 'Strider notice' strider.err | names | sed "s/$TWO/TWO/"
  [INFO] Strider notices repo="repo" notice="old_format_content_metadata" count=1
  [INFO] Strider notices repo="repo" notice="absent_content_metadata" count=0
  $ cmp "$META" "$TESTTMP/old-metadata"
  $ mv "$TESTTMP/metadata" "$META"
