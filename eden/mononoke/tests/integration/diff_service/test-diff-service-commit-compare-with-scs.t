# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

# Integration test for SCS -> diff_service commit_compare routing
# Tests that SCS correctly routes commit_compare to the diff_service

setup
  $ . "${TEST_FIXTURES}/library.sh"

Use a multiplexed store without ephemeral storage for ordinary commits.
  $ MULTIPLEXED=1 REPONAME="repo" setup_common_config

Keep internal paging disabled while capturing the one-shot response.
  $ merge_just_knobs <<EOF
  > {
  >   "ints": {
  >     "scm/mononoke:diff_service_commit_compare_internal_page_size": 0
  >   }
  > }
  > EOF

Create a test repository with files to diff
  $ testtool_drawdag -R repo --print-hg-hashes <<'EOF'
  > A-B-C
  > # bookmark: C main
  > # modify: A "file1.txt" "line 1\nline 2\nline 3\n"
  > # modify: B "file1.txt" "line 1\nmodified\nline 3\n"
  > # modify: B "new_file.txt" "new content\n"
  > # copy: C "copied.txt" "copied content\n" B "file1.txt"
  > # delete: C "new_file.txt"
  > EOF
  A=* (glob)
  B=* (glob)
  C=* (glob)

Convert HG hashes to bonsai for scsc commands
  $ A_BONSAI=$(mononoke_admin convert -R repo -f hg -t bonsai $A 2>/dev/null)
  $ B_BONSAI=$(mononoke_admin convert -R repo -f hg -t bonsai $B 2>/dev/null)
  $ C_BONSAI=$(mononoke_admin convert -R repo -f hg -t bonsai $C 2>/dev/null)

Start mononoke (needed for SCS)
  $ mononoke
  $ wait_for_mononoke

Start diff service in background with admission accounting enabled
  $ start_and_wait_for_diff_service --max-memory 1099511627776

Configure SCS to use local diff service via host:port
  $ cat >> "$TESTTMP/mononoke-config/repos/$REPONAME/server.toml" << CONFIG
  > [remote_diff_config]
  > host_port = "$(diff_service_address)"
  > CONFIG

Start SCS with remote diff enabled
  $ start_and_wait_for_scs_server --diff-remotely --scuba-log-file "$TESTTMP/scs_scuba.json"

== Test internally paged ordered commit_compare ==

Given an ordered request with internal paging disabled
  $ ONE_SHOT_GENERATION_LOGS_BEFORE=$(jq -s '[.[] | select(.normal.log_tag == "Commit compare generation difference")] | length' "$TESTTMP/diff-service.json")

When the existing one-shot path serves the request
  $ scsc diff --repo repo --paths-only --ordered --limit 3 -i "$A_BONSAI" -i "$B_BONSAI" > "$TESTTMP/one-shot.out"

Then it logs the generation difference exactly once
  $ ONE_SHOT_GENERATION_LOGS_AFTER=$(jq -s '[.[] | select(.normal.log_tag == "Commit compare generation difference")] | length' "$TESTTMP/diff-service.json")
  $ echo $((ONE_SHOT_GENERATION_LOGS_AFTER - ONE_SHOT_GENERATION_LOGS_BEFORE))
  1

Capture the remaining responses produced by the one-shot path
  $ scsc diff --repo repo --paths-only --ordered --limit 10 -i "$A_BONSAI" -i "$B_BONSAI" > "$TESTTMP/one-shot-short.out"
  $ scsc diff --repo repo --paths-only --ordered --after file1.txt --limit 3 -i "$A_BONSAI" -i "$B_BONSAI" > "$TESTTMP/one-shot-after.out"

When internal pages are limited to two entries
  $ merge_just_knobs <<EOF
  > {
  >   "ints": {
  >     "scm/mononoke:diff_service_commit_compare_internal_page_size": 2
  >   }
  > }
  > EOF
  $ sleep 2
  $ scsc diff --repo repo --paths-only --ordered --limit 10 -i "$A_BONSAI" -i "$B_BONSAI" > "$TESTTMP/paged-short.out"
  $ diff -u "$TESTTMP/one-shot-short.out" "$TESTTMP/paged-short.out"
  $ PAGE_CALLS_BEFORE=$(jq -s '[.[] | select(.normal.log_tag == "Commit compare generation difference")] | length' "$TESTTMP/diff-service.json")
  $ scsc diff --repo repo --paths-only --ordered --limit 3 -i "$A_BONSAI" -i "$B_BONSAI" > "$TESTTMP/paged.out"

Then exact-boundary and initial-continuation responses are also identical, and paging uses a one-page rolling floor
  $ diff -u "$TESTTMP/one-shot.out" "$TESTTMP/paged.out"
  $ PAGE_CALLS_AFTER=$(jq -s '[.[] | select(.normal.log_tag == "Commit compare generation difference")] | length' "$TESTTMP/diff-service.json")
  $ echo $((PAGE_CALLS_AFTER - PAGE_CALLS_BEFORE))
  1
  $ jq -s -r '[.[] | select(.normal.log_tag == "Computed commit compare")] | last | .int.commit_compare_internal_page_count' "$TESTTMP/diff-service.json"
  2
  $ jq -r 'select(.normal.log_tag == "Computed commit compare" and .normal.commit_compare_admission_mode == "rolling") | "\(.int.commit_compare_initial_reservation_estimated_bytes) \(.int.inflight_bytes)"' "$TESTTMP/diff-service.json" | tail -1
  2048 6144
  $ scsc diff --repo repo --paths-only --ordered --after file1.txt --limit 3 -i "$A_BONSAI" -i "$B_BONSAI" > "$TESTTMP/paged-after.out"
  $ diff -u "$TESTTMP/one-shot-after.out" "$TESTTMP/paged-after.out"

== Test rolling admission for internally paged commit_compare ==

Given internal pages are reduced to one entry
  $ merge_just_knobs <<EOF
  > {
  >   "ints": {
  >     "scm/mononoke:diff_service_commit_compare_internal_page_size": 1
  >   }
  > }
  > EOF
  $ sleep 2

When the same three-entry ordered comparison runs
  $ scsc diff --repo repo --paths-only --ordered --limit 3 -i "$A_BONSAI" -i "$B_BONSAI" > "$TESTTMP/rolling.out"

Then the response is unchanged and admission holds only the one-page floor
  $ diff -u "$TESTTMP/one-shot.out" "$TESTTMP/rolling.out"
  $ jq -r 'select(.normal.log_tag == "Computed commit compare" and .normal.commit_compare_admission_mode == "rolling") | "\(.int.commit_compare_initial_reservation_estimated_bytes) \(.int.inflight_bytes)"' "$TESTTMP/diff-service.json" | tail -1
  1024 3072

When a later page would exceed the configured memory limit
  $ merge_just_knobs <<EOF
  > {
  >   "ints": {
  >     "scm/mononoke:diff_service_commit_compare_estimated_bytes_per_path": 200000000000
  >   }
  > }
  > EOF
  $ sleep 2
  $ if scsc diff --repo repo --paths-only --ordered --limit 3 -i "$A_BONSAI" -i "$B_BONSAI" > "$TESTTMP/rolling-overload.out" 2> "$TESTTMP/rolling-overload.err"; then echo "unexpected success"; cat "$TESTTMP/rolling-overload.out"; else echo "remote overload surfaced"; fi
  remote overload surfaced

Then partial work is discarded and the structured overload reaches SCS without a local fallback
  $ test ! -s "$TESTTMP/rolling-overload.out" && echo "no local fallback output"
  no local fallback output
  $ grep -q 'overload_error(OverloadError)' "$TESTTMP/rolling-overload.err" && echo "structured overload returned"
  structured overload returned
  $ jq -s -r '[.[] | select(.normal.log_tag == "Computed commit compare")] | last | "\(.normal.commit_compare_admission_mode) \(.int.commit_compare_internal_page_count) \(.int.commit_compare_completed_path_count) \(.normal.commit_compare_partial_work_failure)"' "$TESTTMP/diff-service.json"
  rolling 1 1 true
  $ jq -s -r '[.[] | select(.normal.log_tag == "Request complete" and .normal.method == "commit_compare")] | last | "\(.normal.status) \(.normal.error)"' "$TESTTMP/diff-service.json"
  OVERLOADED Insufficient memory:* (glob)

When the normal estimate is restored
  $ merge_just_knobs <<EOF
  > {
  >   "ints": {
  >     "scm/mononoke:diff_service_commit_compare_estimated_bytes_per_path": 1024
  >   }
  > }
  > EOF
  $ sleep 2
  $ scsc diff --repo repo --paths-only --ordered --limit 3 -i "$A_BONSAI" -i "$B_BONSAI" > "$TESTTMP/rolling-after-overload.out"

Then both reservations from the failed request have been released
  $ diff -u "$TESTTMP/one-shot.out" "$TESTTMP/rolling-after-overload.out"
  $ jq -r 'select(.normal.log_tag == "Computed commit compare" and .normal.commit_compare_admission_mode == "rolling") | "\(.int.commit_compare_initial_reservation_estimated_bytes) \(.int.inflight_bytes)"' "$TESTTMP/diff-service.json" | tail -1
  1024 3072

== Test internally paged unordered commit_compare ==

Given internal pages are limited to one entry
  $ merge_just_knobs <<EOF
  > {
  >   "ints": {
  >     "scm/mononoke:diff_service_commit_compare_internal_page_size": 1
  >   }
  > }
  > EOF
  $ sleep 2

When an eligible unordered comparison runs, its response is unchanged
  $ scsc diff --repo repo --paths-only -i "$A_BONSAI" -i "$B_BONSAI" | sort
  A B
  A new_file.txt
  M file1.txt

Then the request uses ordered internal pages with rolling admission
  $ jq -s -r '[.[] | select(.normal.log_tag == "Computed commit compare")] | last | "\(.normal.commit_compare_admission_mode) \(.int.commit_compare_internal_page_count) \(.int.commit_compare_completed_path_count)"' "$TESTTMP/diff-service.json"
  rolling 4 3

Test copy detection
  $ scsc diff --repo repo --paths-only -i "$B_BONSAI" -i "$C_BONSAI" | sort
  A C
  C file1.txt -> copied.txt
  D new_file.txt

== Verify diff service was called ==

Check that commit_compare method was logged
  $ cat "$TESTTMP/diff-service.json" | jq -r '.normal.method' | grep -v null | sort | uniq
  commit_compare

The diff service should have logged successful operations
  $ cat "$TESTTMP/diff-service.json" | jq -r 'select(.normal.log_tag == "Request complete" and .normal.method == "commit_compare" and .normal.status == "SUCCESS") | .normal.status' | sort | uniq
  SUCCESS

Verify client metadata forwarding
The internal client forwards X-Client-Info metadata. The OSS plaintext client
has no authenticated request-header channel, so the field remains absent.
  $ CLIENT_ENTRY_POINT=$(jq -r 'select(.normal.log_tag == "Request complete" and .normal.method == "commit_compare") | .normal.client_entry_point' "$TESTTMP/diff-service.json" | sort | uniq)
  $ if [ -n "$FB_TEST_FIXTURES" ]; then test "$CLIENT_ENTRY_POINT" = scs; else test "$CLIENT_ENTRY_POINT" = null; fi

Verify request type is logged
  $ cat "$TESTTMP/diff-service.json" | jq -r 'select(.normal.log_tag == "Request complete" and .normal.method == "commit_compare") | .normal.type' | sort | uniq
  thrift

== Test remote commit_compare failures are not retried locally ==

Given remote commit_compare is enabled and the diff service becomes unavailable.
  $ kill "$DIFF_SERVICE_PID" 2>/dev/null && wait "$DIFF_SERVICE_PID" 2>/dev/null; true

When SCS receives a commit_compare request.
  $ if scsc diff --repo repo --paths-only -i "$A_BONSAI" -i "$B_BONSAI" > "$TESTTMP/no_fallback.out" 2> "$TESTTMP/no_fallback.err"; then echo "unexpected success"; cat "$TESTTMP/no_fallback.out"; else echo "remote diff failure surfaced"; fi
  remote diff failure surfaced

Then SCS returns its standard internal error for the unavailable remote service.
  $ grep -q 'SourceControlService::commit_compare failed with internal_error(InternalError)' "$TESTTMP/no_fallback.err" && echo "SCS internal error returned"
  SCS internal error returned

Then SCS does not produce the local commit_compare result.
  $ test ! -s "$TESTTMP/no_fallback.out" && echo "no local fallback output"
  no local fallback output
Then SCS does not log a local diff fallback.
  $ cat "$TESTTMP/scs_scuba.json" | jq -r 'select(.normal.diff_fallback != null) | .normal.diff_fallback' | wc -l | sed 's/^[[:space:]]*//'
  0
