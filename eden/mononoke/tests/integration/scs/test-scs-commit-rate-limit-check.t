# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"

Setup repo with commit rate limit config and SCS
  $ REPONAME="test/repo"
  $ setup_common_config
  $ reponame_urlencoded="$(urlencode encode "$REPONAME")"
  $ cat >> "$TESTTMP/mononoke-config/repos/$reponame_urlencoded/server.toml" <<CONFIG
  > [commit_rate_limit_config]
  > [[commit_rate_limit_config.rules]]
  > name = "global_rate_limit"
  > directories = []
  > per_user = false
  > [[commit_rate_limit_config.rules.eligibility_checks]]
  > commit_message_tag = "AUTO_APPROVED"
  > [[commit_rate_limit_config.rules.limits]]
  > window_secs = 3600
  > max_commits = 2
  > [[commit_rate_limit_config.rules]]
  > name = "users_dir_rate_limit"
  > directories = ["users/"]
  > per_user = false
  > [[commit_rate_limit_config.rules.eligibility_checks]]
  > commit_message_tag = "AUTO_APPROVED"
  > [[commit_rate_limit_config.rules.limits]]
  > window_secs = 3600
  > max_commits = 1
  > [[commit_rate_limit_config.rules]]
  > name = "users_per_user_rate_limit"
  > directories = ["users/"]
  > per_user = true
  > [[commit_rate_limit_config.rules.eligibility_checks]]
  > commit_message_tag = "AUTO_APPROVED"
  > [[commit_rate_limit_config.rules.limits]]
  > window_secs = 3600
  > max_commits = 1
  > CONFIG

Compute a recent timestamp so commits fall within the 1-hour window.
  $ RECENT_DATE="$(date -u -d '30 minutes ago' '+%Y-%m-%dT%H:%M:%S+00:00')"

Create a DAG where master has NO eligible ancestors. The eligible
commits (B, C, D) are on a separate branch that simulates draft
commits being pushed to master.

Authors are set so the per-user rule (users_per_user_rate_limit) can
be exercised: B and D are authored by alice and touch users/, while C
is authored by bob and touches fbcode/. E uses the default author
"author", which parses as a bare unixname under
JK:scm/mononoke:allow_bare_author_unixname.

Draft eligible-ancestor counts (excluding the commit itself):
B: 0
C: 1 (B)
D: 2 (B, C)
E: not eligible, skipped

Per-user/users/ counts (alice's eligible ancestors that touch users/):
B: 0
C: skipped — C touches fbcode/
D: 1 (B)

Total = ancestors + 1 (the commit itself):
B: global 1, users_dir 1, users_per_user(alice) 1
C: global 2, users_dir skipped, users_per_user(bob) skipped
D: global 3, users_dir 2, users_per_user(alice) 2

With max_commits=2 (global) and max_commits=1 (users_dir, users_per_user):
B: all PASS
C: PASS (directory-scoped rules skipped)
D: global FAIL (3>2), users_dir FAIL (2>1), users_per_user FAIL (2>1)

  $ testtool_drawdag -R test/repo <<EOF
  > R-B-C-D-E
  > # bookmark: R master
  > # author_date: * "$RECENT_DATE"
  > # author: B "Alice <alice@meta.com>"
  > # author: C "Bob <bob@meta.com>"
  > # author: D "Alice <alice@meta.com>"
  > # message: R "root commit"
  > # message: B "eligible commit 1 AUTO_APPROVED"
  > # message: C "eligible commit 2 AUTO_APPROVED"
  > # message: D "eligible commit 3 AUTO_APPROVED"
  > # message: E "not eligible tip"
  > # modify: B "users/alice/foo.txt" "hello"
  > # modify: C "fbcode/bar.txt" "world"
  > # modify: D "users/alice/baz.txt" "baz"
  > EOF
  B=* (glob)
  C=* (glob)
  D=* (glob)
  E=* (glob)
  R=* (glob)

  $ start_and_wait_for_scs_server --scuba-log-file "$TESTTMP/scuba.json"

Check a non-eligible commit (E): all rules pass because the commit
doesn't have the AUTO_APPROVED tag. E uses the default author "author",
which is accepted as a bare unixname (JK: allow_bare_author_unixname),
so the per-user rule still populates user_filter.
  $ scsc commit-rate-limit-check -R test/repo --commit-id $E --to master
  Rate limit check for * to bookmark master: (glob)
  
  Outcome: PASSED
  
  global_rate_limit => ALLOWED
  users_dir_rate_limit (directories: [users/]) => ALLOWED
  users_per_user_rate_limit (user: author) (directories: [users/]) => ALLOWED






B is alice's first eligible commit in the draft stack. Total = 0 + 1 = 1.
Global: 1 not > 2 -> ALLOWED. users_dir: 1 not > 1 -> ALLOWED.
users_per_user(alice): 1 not > 1 -> ALLOWED, user_filter populated.
  $ scsc commit-rate-limit-check -R test/repo --commit-id $B --to master
  Rate limit check for * to bookmark master: (glob)
  
  Outcome: PASSED
  
  global_rate_limit => ALLOWED
  users_dir_rate_limit (directories: [users/]) => ALLOWED
  users_per_user_rate_limit (user: alice) (directories: [users/]) => ALLOWED






C is bob's only eligible commit and touches fbcode/, so the
directory-scoped rules skip it. The per-user rule still reports
user_filter=bob.
Global: 2 not > 2 -> ALLOWED.
  $ scsc commit-rate-limit-check -R test/repo --commit-id $C --to master
  Rate limit check for * to bookmark master: (glob)
  
  Outcome: PASSED
  
  global_rate_limit => ALLOWED
  users_dir_rate_limit (directories: [users/]) => ALLOWED
  users_per_user_rate_limit (user: bob) (directories: [users/]) => ALLOWED






D is alice's second eligible commit in users/ and the FIRST to exceed
all three rules.
Global: 2 ancestors (B,C) + 1 = 3, 3 > 2 -> EXCEEDED.
users_dir: 1 ancestor (B) + 1 = 2, 2 > 1 -> EXCEEDED.
users_per_user(alice): 1 alice ancestor in users/ (B) + 1 = 2, 2 > 1 -> EXCEEDED.
  $ scsc commit-rate-limit-check -R test/repo --commit-id $D --to master
  Rate limit check for * to bookmark master: (glob)
  
  Outcome: FAILED
  
  global_rate_limit => EXCEEDED: 3 commits in the last 3600 seconds (limit: 2)
  users_dir_rate_limit (directories: [users/]) => EXCEEDED: 2 commits in the last 3600 seconds (limit: 1)
  users_per_user_rate_limit (user: alice) (directories: [users/]) => EXCEEDED: 2 commits in the last 3600 seconds (limit: 1)






Create a draft commit F (alice) on top of E that touches users/ with the tag.
Global: 3 draft ancestors (B,C,D) + 1 = 4, 4 > 2 -> EXCEEDED.
users_dir: 2 draft ancestors in users/ (B,D) + 1 = 3, 3 > 1 -> EXCEEDED.
users_per_user(alice): 2 alice ancestors in users/ (B,D) + 1 = 3, 3 > 1 -> EXCEEDED.
  $ testtool_drawdag -R test/repo <<EOF
  > E-F
  > # exists: E $E
  > # author_date: F "$RECENT_DATE"
  > # author: F "Alice <alice@meta.com>"
  > # modify: F "users/alice/new.txt" "new file"
  > # message: F "new eligible commit AUTO_APPROVED"
  > EOF
  E=* (glob)
  F=* (glob)

  $ scsc commit-rate-limit-check -R test/repo --commit-id $F --to master
  Rate limit check for * to bookmark master: (glob)
  
  Outcome: FAILED
  
  global_rate_limit => EXCEEDED: 4 commits in the last 3600 seconds (limit: 2)
  users_dir_rate_limit (directories: [users/]) => EXCEEDED: 3 commits in the last 3600 seconds (limit: 1)
  users_per_user_rate_limit (user: alice) (directories: [users/]) => EXCEEDED: 3 commits in the last 3600 seconds (limit: 1)






Create a draft commit G (bob) on top of E that touches fbcode/ (not users/).
Global: 3 draft ancestors (B,C,D) + 1 = 4, 4 > 2 -> EXCEEDED.
users_dir: G doesn't touch users/ -> directory filter skips -> ALLOWED.
users_per_user(bob): G doesn't touch users/ -> directory filter skips -> ALLOWED.
The per-user rule still reports user_filter=bob even when ALLOWED.
  $ testtool_drawdag -R test/repo <<EOF
  > E-G
  > # exists: E $E
  > # author_date: G "$RECENT_DATE"
  > # author: G "Bob <bob@meta.com>"
  > # modify: G "fbcode/server/new.txt" "server file"
  > # message: G "eligible outside users AUTO_APPROVED"
  > EOF
  E=* (glob)
  G=* (glob)

  $ scsc commit-rate-limit-check -R test/repo --commit-id $G --to master
  Rate limit check for * to bookmark master: (glob)
  
  Outcome: FAILED
  
  global_rate_limit => EXCEEDED: 4 commits in the last 3600 seconds (limit: 2)
  users_dir_rate_limit (directories: [users/]) => ALLOWED
  users_per_user_rate_limit (user: bob) (directories: [users/]) => ALLOWED






JSON output mode — verify structured output format and that user_filter
is populated for the per-user rule when the author is parseable.
  $ scsc --json commit-rate-limit-check -R test/repo --commit-id $D --to master | jq -S .
  {
    "bookmark": "master",
    "commit": "*", (glob)
    "passed": false,
    "rule_results": [
      {
        "outcome": {
          "current_count": 3,
          "max_commits": 2,
          "status": "Exceeded",
          "window_secs": 3600
        },
        "rule_name": "global_rate_limit"
      },
      {
        "directories": [
          "users/"
        ],
        "outcome": {
          "current_count": 2,
          "max_commits": 1,
          "status": "Exceeded",
          "window_secs": 3600
        },
        "rule_name": "users_dir_rate_limit"
      },
      {
        "directories": [
          "users/"
        ],
        "outcome": {
          "current_count": 2,
          "max_commits": 1,
          "status": "Exceeded",
          "window_secs": 3600
        },
        "rule_name": "users_per_user_rate_limit",
        "user_filter": "alice"
      }
    ]
  }

Sanity check: non-eligible commit R (master tip) passes. R uses the
default author "author", which parses as a bare unixname, so the
per-user rule populates user_filter even though the commit isn't
eligible.
  $ scsc commit-rate-limit-check -R test/repo --commit-id $R --to master
  Rate limit check for * to bookmark master: (glob)
  
  Outcome: PASSED
  
  global_rate_limit => ALLOWED
  users_dir_rate_limit (directories: [users/]) => ALLOWED
  users_per_user_rate_limit (user: author) (directories: [users/]) => ALLOWED


