# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.
#require slow

Trace derivation on cloud upload, without pushrebase or serve-time derivation.

  $ . "${TEST_FIXTURES}/library.sh"

  $ configure modern
  $ INFINITEPUSH_ALLOW_WRITES=true setup_common_config

Record augmented manifest ids under the restricted `stack/` root.
  $ cat >> "$TESTTMP/mononoke-config/repos/repo/server.toml" <<EOF
  > [restricted_paths_config]
  > path_restriction_metadata = { "stack" = { repo_region_acl = "REPO_REGION:config_acl" } }
  > enforcement_enabled = false
  > acl_manifest_mode = "Disabled"
  > EOF
  $ cd $TESTTMP

Use a local commit cloud backend.
  $ cat >> $HGRCPATH <<EOF
  > [commitcloud]
  > servicetype = local
  > EOF

Disable serving-time derivation and augmented routing until envelopes exist.
  $ merge_just_knobs <<EOF
  > {"bools": {"scm/mononoke:derive_hg_augmented_manifest_with_hg_changeset": true, "scm/mononoke:derive_hg_augmented_manifest_on_demand": false, "scm/mononoke:route_original_to_augmented_hg_manifest": false}}
  > EOF

Leave augmented manifests underived in the fixture.
  $ quiet testtool_drawdag -R repo --no-derive-hg-augmented <<EOF
  > A
  > # bookmark: A master_bookmark
  > EOF

  $ SCUBA="$TESTTMP/scuba.json"
  $ start_and_wait_for_mononoke_server --scuba-log-file "$SCUBA"

  $ sl clone -q mono:repo client1
  $ cd client1
  $ sl goto master_bookmark -q
  $ sl cloud join -q

Wait for the terminal request row before inspecting that upload's derivations.
  $ scuba_rows() { wc -l < "$SCUBA"; }
  $ wait_for_upload() {
  >   for _ in $(seq 1 150); do
  >     tail -n +$(($1 + 1)) "$SCUBA" \
  >       | jq -e 'select(.normal.log_tag == "EdenAPI Request Processed"
  >                       and .normal.edenapi_method == "upload_hg_changesets")' \
  >       > /dev/null 2>&1 && return 0
  >     sleep 0.1
  >   done
  >   echo "timed out waiting for upload_hg_changesets request row" >&2
  >   return 1
  > }
  $ derivations_since() {
  >   tail -n +$(($1 + 1)) "$SCUBA" \
  >     | jq -r 'select(.normal.log_tag == "Generated derived data batch")
  >              | "\(.normal.http_path // "<none>") \(.normal.msg | split(" ")[0])"' \
  >     | sort -u
  > }

Scenario 1 -- knob ON. Nested directory so there is more than one tree.
  $ BEFORE=$(scuba_rows)
  $ mkdir -p dir
  $ echo one > dir/file
  $ sl commit -qAm "commit with a nested dir"
  $ sl cloud upload
  commitcloud: head '*' hasn't been uploaded yet (glob)
  edenapi: queue 1 commit for upload
  edenapi: queue 1 file for upload
  edenapi: uploaded 1 file
  edenapi: queue 2 trees for upload
  edenapi: uploaded 2 trees
  edenapi: uploaded 1 changeset

Derivation belongs to changeset upload, not tree upload.
  $ wait_for_upload "$BEFORE"
  $ derivations_since "$BEFORE"
  /repo/upload/changesets acl_manifests
  /repo/upload/changesets hg_augmented_manifests

End state agrees. hgchangesets is present but absent from the rows above: the
upload stores the client-supplied changeset, nothing derives it.
  $ CS1=$(sl log -r . -T '{node}')
  $ cd $TESTTMP
  $ mononoke_admin derived-data -R repo exists -T hgchangesets -i "$CS1"
  Derived: * (glob)
  $ mononoke_admin derived-data -R repo exists -T hg_augmented_manifests -i "$CS1"
  Derived: * (glob)

Scenario 2 -- derive at changeset upload OFF, build and store at tree upload ON.
Keep tree-upload storage OFF in scenario 1 to avoid pre-empting derivation's put.
  $ merge_just_knobs <<EOF
  > {"bools": {"scm/mononoke:derive_hg_augmented_manifest_with_hg_changeset": false, "scm/mononoke:build_augmented_manifests_at_tree_upload": true, "scm/mononoke:store_augmented_manifests_at_tree_upload": true}}
  > EOF
  $ force_update_configerator

  $ cd "$TESTTMP/client1"
  $ BEFORE=$(scuba_rows)
  $ echo two > dir/file2
  $ sl commit -qAm "second commit with a nested dir"
  $ sl cloud upload
  commitcloud: head '*' hasn't been uploaded yet (glob)
  edenapi: queue 1 commit for upload
  edenapi: queue 1 file for upload
  edenapi: uploaded 1 file
  edenapi: queue 2 trees for upload
  edenapi: uploaded 2 trees
  edenapi: uploaded 1 changeset

No derivation or build failure.
  $ wait_for_upload "$BEFORE"
  $ derivations_since "$BEFORE"

  $ tail -n +$((BEFORE + 1)) "$SCUBA" \
  >   | jq -r 'select(.normal.log_tag == "Failed to build augmented Hg manifests at tree upload")
  >            | .normal.msg'

  $ CS2=$(sl log -r . -T '{node}')
  $ ROOT_MFID_2=$(sl log -r . -T '{manifest}')
  $ cd $TESTTMP
  $ mononoke_admin derived-data -R repo exists -T hgchangesets -i "$CS2"
  Derived: * (glob)
  $ mononoke_admin derived-data -R repo exists -T hg_augmented_manifests -i "$CS2"
  Not Derived: * (glob)

The root envelope exists independently of changeset-derived-data mapping.
  $ echo "hgaugmentedmanifest.sha1.$ROOT_MFID_2" > envelope_keys
  $ mononoke_admin blobstore -R repo fetch-many --keys-file envelope_keys
  present: 1
  missing: 0
  failed: 0

Request only manifest bytes; routing adds metadata regardless of these flags.
  $ cat > tree_attrs << EOF
  > {
  >     "manifest_blob": True,
  >     "parents": False,
  >     "child_metadata": False,
  >     "augmented_trees": False
  > }
  > EOF
  $ cat > tree_keys << EOF
  > [
  >     ("", "$ROOT_MFID_2")
  > ]
  > EOF

Serve the stored augmented tree without on-demand derivation.
  $ merge_just_knobs <<EOF
  > {"bools": {"scm/mononoke:route_original_to_augmented_hg_manifest": true}}
  > EOF
  $ force_update_configerator

Reconstructed Hg manifest bytes retain their hash.
  $ hg debugapi mono:repo -e trees -f tree_keys -f tree_attrs --sort > "$TESTTMP/served.out" 2>&1
  $ python3 -c "
  > import hashlib
  > bin = lambda x: x
  > e = eval(open('$TESTTMP/served.out').read())[0]
  > print('manifest_blob_sha1=%s' % hashlib.sha1(e['data']).hexdigest())
  > print('tree_aux_data=%s' % (e.get('tree_aux_data') is not None))
  > print('has_acl=%s' % e.get('has_acl'))
  > print('parents_present=%s' % (e.get('parents') is not None))
  > print('children=%s' % len(e.get('children') or []))
  > "
  manifest_blob_sha1=a06e8b2b61feaa5b804299db3dd2f707ff6bd8ae
  tree_aux_data=True
  has_acl=False
  parents_present=True
  children=2

Scenario 3 -- reuse an unchanged child envelope stored by tree upload.
  $ cd "$TESTTMP/client1"
  $ BEFORE=$(scuba_rows)
  $ echo three > rootfile
  $ sl commit -qAm "third commit, root file only"
  $ sl cloud upload
  commitcloud: head '*' hasn't been uploaded yet (glob)
  edenapi: queue 1 commit for upload
  edenapi: queue 1 file for upload
  edenapi: uploaded 1 file
  edenapi: queue 1 tree for upload
  edenapi: uploaded 1 tree
  edenapi: uploaded 1 changeset
  $ wait_for_upload "$BEFORE"
  $ tail -n +$((BEFORE + 1)) "$SCUBA" \
  >   | jq -r 'select(.normal.log_tag == "Failed to build augmented Hg manifests at tree upload")
  >            | "\(.normal.failure_kind): \(.normal.msg)"' \
  >   | sed -E 's/[0-9a-f]{40}/HASH/g'

Scenario 4 -- build ON, store OFF: shadow building writes no envelope.
  $ merge_just_knobs <<EOF
  > {"bools": {"scm/mononoke:store_augmented_manifests_at_tree_upload": false}}
  > EOF
  $ force_update_configerator

  $ cd "$TESTTMP/client1"
  $ BEFORE=$(scuba_rows)
  $ echo four > dir/file4
  $ sl commit -qAm "fourth commit, shadow"
  $ sl cloud upload
  commitcloud: head '*' hasn't been uploaded yet (glob)
  edenapi: queue 1 commit for upload
  edenapi: queue 1 file for upload
  edenapi: uploaded 1 file
  edenapi: queue 2 trees for upload
  edenapi: uploaded 2 trees
  edenapi: uploaded 1 changeset
  $ wait_for_upload "$BEFORE"

Verify the build succeeded before checking the envelope is absent.
  $ tail -n +$((BEFORE + 1)) "$SCUBA" \
  >   | jq -r 'select(.normal.log_tag == "Failed to build augmented Hg manifests at tree upload")
  >            | .normal.msg'

  $ ROOT_MFID_4=$(sl log -r . -T '{manifest}')
  $ cd $TESTTMP
  $ echo "hgaugmentedmanifest.sha1.$ROOT_MFID_4" > envelope_keys
  $ mononoke_admin blobstore -R repo fetch-many --keys-file envelope_keys
  present: 0
  missing: 1
  failed: 0

Scenario 5 -- a stored child without an augmented envelope is `child_not_derived`,
not a client error. Its failed build must not fail the upload.
  $ cd "$TESTTMP/client1"
  $ BEFORE=$(scuba_rows)
  $ echo five > rootfile
  $ sl commit -qAm "fifth commit, root file only, shadow"
  $ sl cloud upload
  commitcloud: head '*' hasn't been uploaded yet (glob)
  edenapi: queue 1 commit for upload
  edenapi: queue 1 file for upload
  edenapi: uploaded 1 file
  edenapi: queue 1 tree for upload
  edenapi: uploaded 1 tree
  edenapi: uploaded 1 changeset
  $ wait_for_upload "$BEFORE"
  $ tail -n +$((BEFORE + 1)) "$SCUBA" \
  >   | jq -r 'select(.normal.log_tag == "Failed to build augmented Hg manifests at tree upload")
  >            | "\(.normal.failure_kind): \(.normal.msg)"' \
  >   | sed -E 's/[0-9a-f]{40}/HASH/g'
  child_not_derived: building the augmented manifest for uploaded tree HASH: tree HASH contains dir (HASH), which is neither built in this batch nor already derived

The request row carries the build's tree count and a nonzero build time, failed
build included.
  $ tail -n +$((BEFORE + 1)) "$SCUBA" \
  >   | jq -r 'select(.normal.log_tag == "EdenAPI Request Processed"
  >                   and .normal.edenapi_method == "upload_trees")
  >            | "\(.int.EdenapiUploadTreesAugmentedTrees) \(.int.EdenapiUploadTreesAugmentedBuildUs > 0)"'
  1 true

Scenario 6 -- map stored roots rather than deriving, preserving restricted paths.
  $ merge_just_knobs <<EOF
  > {"bools": {"scm/mononoke:derive_hg_augmented_manifest_with_hg_changeset": true, "scm/mononoke:store_augmented_manifests_at_tree_upload": true, "scm/mononoke:check_uploaded_root_hg_augmented_manifest": true, "scm/mononoke:map_uploaded_root_hg_augmented_manifest": true}}
  > EOF
  $ root_checks_since() {
  >   tail -n +$(($1 + 1)) "$SCUBA" \
  >     | jq -r 'select(.normal.log_tag == "EdenAPI Request Processed"
  >                     and .normal.edenapi_method == "upload_hg_changesets")
  >              | .int | to_entries[] | select(.key | startswith("UploadedRootAugmented"))
  >              | "\(.key): \(.value)"'
  > }
  $ force_update_configerator

  $ cd "$TESTTMP/client1"
  $ sl goto -q master_bookmark
  $ BEFORE=$(scuba_rows)
  $ mkdir stack
  $ echo five > stack/file5
  $ sl commit -qAm "sixth commit, new dir"
  $ echo six > stack/file6
  $ sl commit -qAm "seventh commit"
  $ sl cloud upload
  commitcloud: head '*' hasn't been uploaded yet (glob)
  edenapi: queue 2 commits for upload
  edenapi: queue 2 files for upload
  edenapi: uploaded 2 files
  edenapi: queue 4 trees for upload
  edenapi: uploaded 4 trees
  edenapi: uploaded 2 changesets
  $ wait_for_upload "$BEFORE"
  $ derivations_since "$BEFORE"
  $ root_checks_since "$BEFORE"
  UploadedRootAugmentedMapped: 2

Mapping still records each root's restricted directory, as derivation would.
  $ sqlite3 "$TESTTMP/monsql/sqlite_dbs" "SELECT CAST(path AS TEXT) FROM restricted_paths_manifest_ids WHERE hex(manifest_type) = hex('HgAugmented')"
  stack
  stack

Each commit maps to the envelope tree upload stored at its own root.
  $ CS6=$(sl log -r .^ -T '{node}')
  $ ROOT_MFID_6=$(sl log -r .^ -T '{manifest}')
  $ CS7=$(sl log -r . -T '{node}')
  $ ROOT_MFID_7=$(sl log -r . -T '{manifest}')
  $ cd $TESTTMP
  $ mononoke_admin derived-data -R repo fetch -T hg_augmented_manifests -i "$CS6" | grep -c "$ROOT_MFID_6"
  1
  $ mononoke_admin derived-data -R repo fetch -T hg_augmented_manifests -i "$CS7" | grep -c "$ROOT_MFID_7"
  1

With no ACL file in the repo, the mapped ACL manifest is the empty one.
  $ mononoke_admin derived-data -R repo fetch -T acl_manifests -i "$CS6" | sed 's/.* -> //'
  RootAclManifestId(AclManifestId(Blake2(ccdc1160ab59cd5c9179719f5f725f2802b9891a2ca6f2f618624ce446b12a1c)))

Scenario 7 -- store OFF, derive ON: the missing root must still be derived.
  $ merge_just_knobs <<EOF
  > {"bools": {"scm/mononoke:store_augmented_manifests_at_tree_upload": false}}
  > EOF
  $ force_update_configerator

  $ cd "$TESTTMP/client1"
  $ sl goto -q master_bookmark
  $ BEFORE=$(scuba_rows)
  $ mkdir other
  $ echo seven > other/file7
  $ sl commit -qAm "eighth commit, store off"
  $ sl cloud upload
  commitcloud: head '*' hasn't been uploaded yet (glob)
  edenapi: queue 1 commit for upload
  edenapi: queue 1 file for upload
  edenapi: uploaded 1 file
  edenapi: queue 2 trees for upload
  edenapi: uploaded 2 trees
  edenapi: uploaded 1 changeset
  $ wait_for_upload "$BEFORE"
  $ derivations_since "$BEFORE"
  /repo/upload/changesets acl_manifests
  /repo/upload/changesets hg_augmented_manifests
  $ root_checks_since "$BEFORE"
  UploadedRootAugmentedMissing: 1

  $ CS8=$(sl log -r . -T '{node}')
  $ cd $TESTTMP
  $ mononoke_admin derived-data -R repo exists -T hg_augmented_manifests -i "$CS8"
  Derived: * (glob)

Scenario 8 -- map an empty commit's reused root, then derive its child's new root.
  $ cd "$TESTTMP/client1"
  $ sl goto -q master_bookmark
  $ BEFORE=$(scuba_rows)
  $ sl commit -q --config ui.allowemptycommit=true -m "ninth commit, empty"
  $ mkdir after_empty
  $ echo nine > after_empty/file9
  $ sl commit -qAm "tenth commit, on top of an empty one"
  $ sl cloud upload
  commitcloud: head '*' hasn't been uploaded yet (glob)
  edenapi: queue 2 commits for upload
  edenapi: queue 1 file for upload
  edenapi: uploaded 1 file
  edenapi: queue 2 trees for upload
  edenapi: uploaded 2 trees
  edenapi: uploaded 2 changesets
  $ wait_for_upload "$BEFORE"
  $ derivations_since "$BEFORE"
  /repo/upload/changesets acl_manifests
  /repo/upload/changesets hg_augmented_manifests
  $ root_checks_since "$BEFORE"
  UploadedRootAugmentedMapped: 1
  UploadedRootAugmentedMissing: 1

  $ CS9=$(sl log -r .^ -T '{node}')
  $ CS10=$(sl log -r . -T '{node}')
  $ ROOT_MFID_A=$(sl log -r master_bookmark -T '{manifest}')
  $ cd $TESTTMP
  $ mononoke_admin derived-data -R repo fetch -T hg_augmented_manifests -i "$CS9" | grep -c "$ROOT_MFID_A"
  1
  $ mononoke_admin derived-data -R repo exists -T hg_augmented_manifests -i "$CS10"
  Derived: * (glob)

Scenario 9 -- a restricted directory, storing ON, so the mapped ACL manifest is
not the empty one.
  $ merge_just_knobs <<EOF
  > {"bools": {"scm/mononoke:store_augmented_manifests_at_tree_upload": true}}
  > EOF
  $ force_update_configerator

  $ cd "$TESTTMP/client1"
  $ sl goto -q master_bookmark
  $ BEFORE=$(scuba_rows)
  $ mkdir secret
  $ printf 'repo_region_acl = "REPO_REGION:repos/hg/fbsource/=project1"\n' > secret/.slacl
  $ echo hidden > secret/file
  $ sl commit -qAm "eleventh commit, restricted dir"
  $ sl cloud upload
  commitcloud: head '*' hasn't been uploaded yet (glob)
  edenapi: queue 1 commit for upload
  edenapi: queue 2 files for upload
  edenapi: uploaded 2 files
  edenapi: queue 2 trees for upload
  edenapi: uploaded 2 trees
  edenapi: uploaded 1 changeset
  $ wait_for_upload "$BEFORE"
  $ derivations_since "$BEFORE"
  $ root_checks_since "$BEFORE"
  UploadedRootAugmentedMapped: 1
  $ CS11=$(sl log -r . -T '{node}')
  $ cd $TESTTMP
  $ mononoke_admin derived-data -R repo fetch -T acl_manifests -i "$CS11" | sed 's/.* -> //' > acl_11

The stored envelope matches derivation, ACL pointers included.
  $ BCS11=$(mononoke_admin convert -R repo -f hg -t bonsai "$CS11")
  $ mononoke_admin derived-data -R repo verify-aug-direct --end-id "$BCS11" --last 1 --batch-size 1 --concurrency 1 && echo success
  [INFO] verifying up to 1 changesets
  [INFO] progress: batch=1 size=1 processed=1 direct=1 full-v2-fallback=0
  done: processed=1 direct=1 full-v2-fallback=0
  success

The mapped ACL manifest matches the one derived for a twin commit with the same
tree, uploaded with mapping off.
  $ merge_just_knobs <<EOF
  > {"bools": {"scm/mononoke:map_uploaded_root_hg_augmented_manifest": false}}
  > EOF
  $ force_update_configerator

  $ cd "$TESTTMP/client1"
  $ sl goto -q master_bookmark
  $ BEFORE=$(scuba_rows)
  $ mkdir secret
  $ printf 'repo_region_acl = "REPO_REGION:repos/hg/fbsource/=project1"\n' > secret/.slacl
  $ echo hidden > secret/file
  $ sl commit -qAm "twelfth commit, the same restricted dir"
  $ sl cloud upload
  commitcloud: head '*' hasn't been uploaded yet (glob)
  edenapi: queue 1 commit for upload
  edenapi: queue 0 files for upload
  edenapi: queue 0 trees for upload
  edenapi: uploaded 1 changeset
  $ wait_for_upload "$BEFORE"
  $ derivations_since "$BEFORE"
  /repo/upload/changesets acl_manifests
  /repo/upload/changesets hg_augmented_manifests
  $ root_checks_since "$BEFORE"
  UploadedRootAugmentedStored: 1
  $ CS12=$(sl log -r . -T '{node}')
  $ cd $TESTTMP
  $ mononoke_admin derived-data -R repo fetch -T acl_manifests -i "$CS12" | sed 's/.* -> //' > acl_12
  $ cmp acl_11 acl_12 && echo same
  same

Scenario 10 -- mapping stored roots works with changeset-upload derivation OFF.
  $ merge_just_knobs <<EOF
  > {"bools": {"scm/mononoke:derive_hg_augmented_manifest_with_hg_changeset": false, "scm/mononoke:map_uploaded_root_hg_augmented_manifest": true}}
  > EOF
  $ force_update_configerator

  $ cd "$TESTTMP/client1"
  $ sl goto -q master_bookmark
  $ BEFORE=$(scuba_rows)
  $ mkdir late
  $ echo ten > late/file
  $ sl commit -qAm "commit with derivation off"
  $ sl cloud upload
  commitcloud: head '*' hasn't been uploaded yet (glob)
  edenapi: queue 1 commit for upload
  edenapi: queue 1 file for upload
  edenapi: uploaded 1 file
  edenapi: queue 2 trees for upload
  edenapi: uploaded 2 trees
  edenapi: uploaded 1 changeset
  $ wait_for_upload "$BEFORE"
  $ derivations_since "$BEFORE"
  $ root_checks_since "$BEFORE"
  UploadedRootAugmentedMapped: 1
  $ CS_OFF=$(sl log -r . -T '{node}')
  $ cd $TESTTMP
  $ mononoke_admin derived-data -R repo exists -T acl_manifests -i "$CS_OFF"
  Derived: * (glob)
  $ mononoke_admin derived-data -R repo exists -T hg_augmented_manifests -i "$CS_OFF"
  Derived: * (glob)

Scenario 11 -- lookup resends trees stored without augmented envelopes.
  $ merge_just_knobs <<EOF
  > {"bools": {"scm/mononoke:store_augmented_manifests_at_tree_upload": false, "scm/mononoke:map_uploaded_root_hg_augmented_manifest": false}}
  > EOF
  $ force_update_configerator

  $ cd "$TESTTMP/client1"
  $ sl goto -q master_bookmark
  $ mkdir early
  $ echo eleven > early/file
  $ sl commit -qAm "commit uploaded before storing"
  $ sl cloud upload
  commitcloud: head '*' hasn't been uploaded yet (glob)
  edenapi: queue 1 commit for upload
  edenapi: queue 1 file for upload
  edenapi: uploaded 1 file
  edenapi: queue 2 trees for upload
  edenapi: uploaded 2 trees
  edenapi: uploaded 1 changeset

  $ merge_just_knobs <<EOF
  > {"bools": {"scm/mononoke:store_augmented_manifests_at_tree_upload": true, "scm/mononoke:map_uploaded_root_hg_augmented_manifest": true}}
  > EOF
  $ force_update_configerator

With lookup requiring the augmented manifest, a twin commit with the same tree
resends both trees, so tree upload stores their augmented manifests and the
commit is mapped.
  $ merge_just_knobs <<EOF
  > {"bools": {"scm/mononoke:require_augmented_manifest_for_tree_lookup": true}}
  > EOF
  $ force_update_configerator

  $ sl goto -q master_bookmark
  $ BEFORE=$(scuba_rows)
  $ mkdir early
  $ echo eleven > early/file
  $ sl commit -qAm "the same tree, uploaded with mapping on"
  $ sl cloud upload
  commitcloud: head '*' hasn't been uploaded yet (glob)
  edenapi: queue 1 commit for upload
  edenapi: queue 0 files for upload
  edenapi: queue 2 trees for upload
  edenapi: uploaded 2 trees
  edenapi: uploaded 1 changeset
  $ wait_for_upload "$BEFORE"
  $ derivations_since "$BEFORE"
  $ root_checks_since "$BEFORE"
  UploadedRootAugmentedMapped: 1
  $ tail -n +$((BEFORE + 1)) "$SCUBA" \
  >   | jq -r 'select(.normal.log_tag == "EdenAPI Request Processed"
  >                   and .normal.edenapi_method == "lookup")
  >            | .int.TreeLookupAugmentedMissing // empty'
  2
  $ CS_EARLY=$(sl log -r . -T '{node}')
  $ cd $TESTTMP
  $ mononoke_admin derived-data -R repo exists -T hg_augmented_manifests -i "$CS_EARLY"
  Derived: * (glob)

Scenario 12 -- the bonsai changeset upload builds. A stack with a modify, a
delete, a copy, a file replaced by a directory and a merge is uploaded, and the
file changes of each commit's bonsai are printed.
  $ file_changes() {
  >   mononoke_admin fetch -R repo -i "$1" | grep -E 'ADDED|COPY|REMOVED' | sed 's/^\s*//'
  > }
  $ make_stack() {
  >   sl goto -q master_bookmark
  >   mkdir -p twelve/dir
  >   echo a > twelve/dir/a
  >   echo b > twelve/b
  >   sl commit -qAm "twelfth stack base $1"
  >   sl cp -q twelve/dir/a twelve/copied
  >   echo changed >> twelve/dir/a
  >   sl rm -q twelve/b
  >   mkdir twelve/b
  >   echo inner > twelve/b/inner
  >   sl commit -qAm "twelfth stack top $1"
  >   TOP=$(sl log -r . -T '{node}')
  >   sl goto -q .^
  >   echo side > twelve/side
  >   sl commit -qAm "twelfth stack side $1"
  >   SIDE=$(sl log -r . -T '{node}')
  >   sl goto -q "$TOP"
  >   sl merge -q "$SIDE"
  >   sl commit -qm "twelfth stack merge $1"
  > }
  $ stack_file_changes() {
  >   for rev in '.^1^' '.^1' '.^2' '.'; do
  >     sl log -r "$rev" -T '{desc|firstline}:\n'
  >     file_changes "$(sl log -r "$rev" -T '{node}')"
  >   done
  > }

  $ cd "$TESTTMP/client1"
  $ make_stack hg
  $ BEFORE=$(scuba_rows)
  $ sl cloud upload
  commitcloud: head '*' hasn't been uploaded yet (glob)
  edenapi: queue 4 commits for upload
  edenapi: queue 6 files for upload
  edenapi: uploaded 6 files
  edenapi: queue 11 trees for upload
  edenapi: uploaded 11 trees
  edenapi: uploaded 4 changesets
  $ wait_for_upload "$BEFORE"
  $ stack_file_changes | sed 's/ hg:$/:/' | tee "$TESTTMP/hg_file_changes"
  twelfth stack base:
  ADDED/MODIFIED: twelve/b 21c519fe0eb401bc97888f270902935f858d0c5361211f892fd26ed9ce127ff9
  ADDED/MODIFIED: twelve/dir/a b5785d8c57755a9174b40a9b3dfb19f2480fd7e592341e41f12c42be94e1df41
  twelfth stack top:
  REMOVED: twelve/b
  ADDED/MODIFIED: twelve/b/inner c8657be1f6ac476ae814bb2afee7e0a76ab32baea729132bc4fd71fd88f67ee9
  COPY/MOVE: twelve/copied b5785d8c57755a9174b40a9b3dfb19f2480fd7e592341e41f12c42be94e1df41
  ADDED/MODIFIED: twelve/dir/a 8b7339de93cae7a6e5da1f294e49d44895d9c855bce3ac686bf9cfeb6b6e2c17
  twelfth stack side:
  ADDED/MODIFIED: twelve/side f185e9966cc5a4d664059c62f145b9bffa4f5e619e29f6997b53ea27dc6c21dd
  twelfth stack merge:
  REMOVED: twelve/b
  ADDED/MODIFIED: twelve/dir/a 8b7339de93cae7a6e5da1f294e49d44895d9c855bce3ac686bf9cfeb6b6e2c17
