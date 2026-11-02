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

Helper for making commit:
  $ function commit() { # the arg is used both for commit message and variable name
  >   hg commit -qAm $1 -d $2 # create commit with a date
  >   export COMMIT_$1="$(hg --debug id -i)" # save hash to variable
  > }

Commits with dates to test time filters

  $ echo -e "a\nb\nc\nd\ne" > a
  $ commit A "2015-01-01"

  $ echo -e "a\nb\nd\ne\nf" > b
  $ commit B "2016-01-01"

  $ echo -e "b\nc\nd\ne\nf" > b
  $ commit C "2017-01-01"

  $ echo -e "b\nc\nd\ne\nf" > d
  $ commit D "2018-01-01"

  $ echo -e "p\nk\nt\ne\nf" > b
  $ commit E "2019-01-01"

  $ echo -e "b\nn\ne\nw\nl" > b
  $ commit F "2020-01-01"

  $ echo -e "a\nb\nc\ne\nf" > d
  $ commit G "2020-01-02"

  $ rm b
  $ echo -e "a\nb\nc\nd\ne" > d
  $ commit H "2020-01-03"

  $ echo -e "re-added b" > b
  $ commit I "2020-01-03"

Create a branch

  $ hg checkout -q $COMMIT_E
  $ echo -e "a" > c
  $ commit E1 "2019-02-01"

  $ echo -e "b" > c
  $ commit E2 "2019-02-02"

Simulate merging in another repo.

  $ hg checkout -q null
  $ echo 1 > m
  $ commit L "2024-01-01"

  $ hg checkout -q "$COMMIT_I"
  $ hg merge -q -r "$COMMIT_L"
  $ commit M "2024-02-01"

start SCS server
  $ start_and_wait_for_scs_server --scuba-log-file "$TESTTMP/scuba.json"

configure repo to push to mononoke
  $ cat >> .hg/hgrc <<EOF
  > [paths]
  > default = mono:repo
  > EOF

import testing repo to mononoke
  $ hg push --to master_bookmark --create -r "$COMMIT_M" -q
  $ hg push --to branch_e --create -r "$COMMIT_E2" -q

ignore the scuba logs logged while starting the server
  $ SCUBA_PREAMBLE=$(( $(wc -l < "$TESTTMP/scuba.json") + 1 ))

Test full log
  $ scsc log --repo repo -i "$COMMIT_H"
  Commit: 159ed529f60d23c614fe315d46a4b2eb5d27b569
  Date: 2020-01-03 00:00:00 +00:00
  Author: test
  Summary: H
  
  Commit: d603e69354506a00833ddb9422cac6053debb733
  Date: 2020-01-02 00:00:00 +00:00
  Author: test
  Summary: G
  
  Commit: 3a61e10442a9b76f8826b05e7ef1a60d33c3bc2d
  Date: 2020-01-01 00:00:00 +00:00
  Author: test
  Summary: F
  
  Commit: ecbf21bc13d7ec53c820078066ca1dfeb1e8191d
  Date: 2019-01-01 00:00:00 +00:00
  Author: test
  Summary: E
  
  Commit: aaff25985c53d3ba33e0292b2a271c6cdf34e521
  Date: 2018-01-01 00:00:00 +00:00
  Author: test
  Summary: D
  
  Commit: dba4093ee164cd983101ff9e37751e2a5465c6a9
  Date: 2017-01-01 00:00:00 +00:00
  Author: test
  Summary: C
  
  Commit: 88619a661eeda8bca794249b9852c83dacada01a
  Date: 2016-01-01 00:00:00 +00:00
  Author: test
  Summary: B
  
  Commit: 7b9ef4e3cdfffb431178532bb75534439c14c014
  Date: 2015-01-01 00:00:00 +00:00
  Author: test
  Summary: A
  

Test log with path
  $ scsc log --repo repo -i "$COMMIT_C" --path b 
  Commit: dba4093ee164cd983101ff9e37751e2a5465c6a9
  Date: 2017-01-01 00:00:00 +00:00
  Author: test
  Summary: C
  
  Commit: 88619a661eeda8bca794249b9852c83dacada01a
  Date: 2016-01-01 00:00:00 +00:00
  Author: test
  Summary: B
  
  $ scsc log --verbose --repo repo -i "$COMMIT_C" --path b 
  Commit: dba4093ee164cd983101ff9e37751e2a5465c6a9
  Parent: 88619a661eeda8bca794249b9852c83dacada01a
  Date: 2017-01-01 00:00:00 +00:00
  Author: test
  Generation: 3
  
  C
  
  Commit: 88619a661eeda8bca794249b9852c83dacada01a
  Parent: 7b9ef4e3cdfffb431178532bb75534439c14c014
  Date: 2016-01-01 00:00:00 +00:00
  Author: test
  Generation: 2
  
  B
  
  $ scsc --json log --repo repo -i "$COMMIT_F" --path b --limit 2 | jq -S .
  [
    {
      "author": "test",
      "committer": null,
      "committer_date": null,
      "date": "2020-01-01T00:00:00Z",
      "extra": {},
      "extra_hex": {},
      "generation": 6,
      "git_extra_headers": null,
      "ids": {
        "bonsai": "c695b75bf2396285cec024a7c63dcffef19d9ba3aaa902f409c0bdbb6d268414",
        "hg": "3a61e10442a9b76f8826b05e7ef1a60d33c3bc2d"
      },
      "linear_depth": 5,
      "message": "F",
      "parents": [
        {
          "bonsai": "175af9015b2ca0b133689b73de2ae4e6c34892d62a3c5614ad7170efb0b475fa",
          "hg": "ecbf21bc13d7ec53c820078066ca1dfeb1e8191d"
        }
      ],
      "timestamp": 1577836800,
      "timezone": 0,
      "type": "commit"
    },
    {
      "author": "test",
      "committer": null,
      "committer_date": null,
      "date": "2019-01-01T00:00:00Z",
      "extra": {},
      "extra_hex": {},
      "generation": 5,
      "git_extra_headers": null,
      "ids": {
        "bonsai": "175af9015b2ca0b133689b73de2ae4e6c34892d62a3c5614ad7170efb0b475fa",
        "hg": "ecbf21bc13d7ec53c820078066ca1dfeb1e8191d"
      },
      "linear_depth": 4,
      "message": "E",
      "parents": [
        {
          "bonsai": "4e1e8b4466f38fc8f37fe637d47edf0953d4d4d289813bf89b6ff7ff092638f2",
          "hg": "aaff25985c53d3ba33e0292b2a271c6cdf34e521"
        }
      ],
      "timestamp": 1546300800,
      "timezone": 0,
      "type": "commit"
    }
  ]

  $ scsc log --repo repo -i "$COMMIT_F" --path b --limit 1 --skip 1
  Commit: ecbf21bc13d7ec53c820078066ca1dfeb1e8191d
  Date: 2019-01-01 00:00:00 +00:00
  Author: test
  Summary: E
  
log between 2017/01/01 and 2019/05/05
  $ scsc log --repo repo -i "$COMMIT_F" --path b --after 1483228800 --before 1557061200
  Commit: ecbf21bc13d7ec53c820078066ca1dfeb1e8191d
  Date: 2019-01-01 00:00:00 +00:00
  Author: test
  Summary: E
  
  Commit: dba4093ee164cd983101ff9e37751e2a5465c6a9
  Date: 2017-01-01 00:00:00 +00:00
  Author: test
  Summary: C
  
  $ scsc log --repo repo -i "$COMMIT_F" --path b --after "2017-01-01 00:00:00" --before "2019-05-05 13:00:00"
  Commit: ecbf21bc13d7ec53c820078066ca1dfeb1e8191d
  Date: 2019-01-01 00:00:00 +00:00
  Author: test
  Summary: E
  
  Commit: dba4093ee164cd983101ff9e37751e2a5465c6a9
  Date: 2017-01-01 00:00:00 +00:00
  Author: test
  Summary: C
  

log check the timezone parsing
  $ scsc log --repo repo -i "$COMMIT_F" --path b --after "2019-01-01 05:00:00 +08:00"
  Commit: 3a61e10442a9b76f8826b05e7ef1a60d33c3bc2d
  Date: 2020-01-01 00:00:00 +00:00
  Author: test
  Summary: F
  
  Commit: ecbf21bc13d7ec53c820078066ca1dfeb1e8191d
  Date: 2019-01-01 00:00:00 +00:00
  Author: test
  Summary: E
  
log for the "zero" timestamp
  $ scsc log --repo repo -i "$COMMIT_F" --path b --before 0 --limit 1
  *scsc: error: The given date or timestamp must be after 1970-01-01 00:00:00 UTC: "0" (glob)
  [1]
  $ scsc log --repo repo -i "$COMMIT_F" --path b --after "1969-01-01 00:00:00" --limit 1
  *scsc: error: The given date or timestamp must be after 1970-01-01 00:00:00 UTC: "1969-01-01 00:00:00" (glob)
  [1]

log skip and time filters conflict
  $ scsc log --repo repo -i "$COMMIT_F" --path b --after "2017-01-01 05:00:00 +08:00" --skip 5
  error: the argument '--after <AFTER>' cannot be used with '--skip <SKIP>'
  
  Usage: scsc log --repo <REPO> --commit-id <commit-id> --path <PATH> --after <AFTER>
  
  For more information, try '--help'.
  [2]

log request a single commit
  $ scsc log --repo repo -i "$COMMIT_F" --path b --limit 1
  Commit: 3a61e10442a9b76f8826b05e7ef1a60d33c3bc2d
  Date: 2020-01-01 00:00:00 +00:00
  Author: test
  Summary: F
  

log request history for deleted file
  $ scsc log --repo repo -i "$COMMIT_H" --path b --limit 2
  Commit: 159ed529f60d23c614fe315d46a4b2eb5d27b569
  Date: 2020-01-03 00:00:00 +00:00
  Author: test
  Summary: H
  
  Commit: 3a61e10442a9b76f8826b05e7ef1a60d33c3bc2d
  Date: 2020-01-01 00:00:00 +00:00
  Author: test
  Summary: F
  

log request history across deletions
  $ scsc log --repo repo -i "$COMMIT_I" --path b --limit 3
  Commit: 4eddc88ca261a8115dd01c3af50a17aad50287de
  Date: 2020-01-03 00:00:00 +00:00
  Author: test
  Summary: I
  
  $ scsc log --repo repo -i "$COMMIT_I" --path b --limit 3 --history-across-deletions
  Commit: 4eddc88ca261a8115dd01c3af50a17aad50287de
  Date: 2020-01-03 00:00:00 +00:00
  Author: test
  Summary: I
  
  Commit: 159ed529f60d23c614fe315d46a4b2eb5d27b569
  Date: 2020-01-03 00:00:00 +00:00
  Author: test
  Summary: H
  
  Commit: 3a61e10442a9b76f8826b05e7ef1a60d33c3bc2d
  Date: 2020-01-01 00:00:00 +00:00
  Author: test
  Summary: F
  
log request limited to descendendants of certain commit only

Test full log
  $ scsc log --repo repo -i "$COMMIT_H" -i "$COMMIT_E"
  Commit: 159ed529f60d23c614fe315d46a4b2eb5d27b569
  Date: 2020-01-03 00:00:00 +00:00
  Author: test
  Summary: H
  
  Commit: d603e69354506a00833ddb9422cac6053debb733
  Date: 2020-01-02 00:00:00 +00:00
  Author: test
  Summary: G
  
  Commit: 3a61e10442a9b76f8826b05e7ef1a60d33c3bc2d
  Date: 2020-01-01 00:00:00 +00:00
  Author: test
  Summary: F
  
  Commit: ecbf21bc13d7ec53c820078066ca1dfeb1e8191d
  Date: 2019-01-01 00:00:00 +00:00
  Author: test
  Summary: E
  

Test log with path and limited to descendants of, using two parameters.

  $ scsc log --repo repo -i "$COMMIT_I" -i "$COMMIT_C" --path b --limit 10 --history-across-deletions
  Commit: 4eddc88ca261a8115dd01c3af50a17aad50287de
  Date: 2020-01-03 00:00:00 +00:00
  Author: test
  Summary: I
  
  Commit: 159ed529f60d23c614fe315d46a4b2eb5d27b569
  Date: 2020-01-03 00:00:00 +00:00
  Author: test
  Summary: H
  
  Commit: 3a61e10442a9b76f8826b05e7ef1a60d33c3bc2d
  Date: 2020-01-01 00:00:00 +00:00
  Author: test
  Summary: F
  
  Commit: ecbf21bc13d7ec53c820078066ca1dfeb1e8191d
  Date: 2019-01-01 00:00:00 +00:00
  Author: test
  Summary: E
  
  Commit: dba4093ee164cd983101ff9e37751e2a5465c6a9
  Date: 2017-01-01 00:00:00 +00:00
  Author: test
  Summary: C
  
Test scuba logging
  $ cat "$TESTTMP/scuba.json" | grep -E '"method".*"(commit_history|commit_path_history)"' | jq -c 'select(.normal.log_tag | startswith("Request"))' | tail -n 4 | summarize_scuba_json "Request.*" \
  >     .normal.log_tag .normal.msg .normal.method \
  >     .normal.commit .normal.path \
  >     .normal.param_descendants_of \
  >     .normvector.identity_schemes \
  >     .normal.status .normal.error
  {
    "commit": "872c9cc71d0e0f30614fae7c2bd3e6c39e07393e51470a82df8f921c36335e07",
    "identity_schemes": [],
    "log_tag": "Request start",
    "method": "commit_history",
    "param_descendants_of": "175af9015b2ca0b133689b73de2ae4e6c34892d62a3c5614ad7170efb0b475fa"
  }
  {
    "commit": "872c9cc71d0e0f30614fae7c2bd3e6c39e07393e51470a82df8f921c36335e07",
    "identity_schemes": [],
    "log_tag": "Request complete",
    "method": "commit_history",
    "param_descendants_of": "175af9015b2ca0b133689b73de2ae4e6c34892d62a3c5614ad7170efb0b475fa",
    "status": "SUCCESS"
  }
  {
    "commit": "5724aa50cc5e8d25848ead441e976994cf3c41f50b01afe98da9b90b53b313af",
    "identity_schemes": [],
    "log_tag": "Request start",
    "method": "commit_path_history",
    "param_descendants_of": "d92795577c549a45cb6914b59983b8e28e634966a697da688107b10239a8f76d",
    "path": "b"
  }
  {
    "commit": "5724aa50cc5e8d25848ead441e976994cf3c41f50b01afe98da9b90b53b313af",
    "identity_schemes": [],
    "log_tag": "Request complete",
    "method": "commit_path_history",
    "param_descendants_of": "d92795577c549a45cb6914b59983b8e28e634966a697da688107b10239a8f76d",
    "path": "b",
    "status": "SUCCESS"
  }


We can get an *inclusive* range of commits using --descendants-of
  $ scsc log --repo repo -i "$COMMIT_M" --descendants-of -i "$COMMIT_H"
  Commit: fc6a43862ab96729167f45045bbfd37532789a5c
  Date: 2024-02-01 00:00:00 +00:00
  Author: test
  Summary: M
  
  Commit: 4eddc88ca261a8115dd01c3af50a17aad50287de
  Date: 2020-01-03 00:00:00 +00:00
  Author: test
  Summary: I
  
  Commit: 159ed529f60d23c614fe315d46a4b2eb5d27b569
  Date: 2020-01-03 00:00:00 +00:00
  Author: test
  Summary: H
  
The same commits with --exclude-ancestors-of gives us the ancestors difference instead.  Note
that H is excluded, and the merged in branch (L) is included.
  $ scsc log --repo repo -i "$COMMIT_M" --exclude-ancestors-of -i "$COMMIT_H"
  Commit: fc6a43862ab96729167f45045bbfd37532789a5c
  Date: 2024-02-01 00:00:00 +00:00
  Author: test
  Summary: M
  
  Commit: 4eddc88ca261a8115dd01c3af50a17aad50287de
  Date: 2020-01-03 00:00:00 +00:00
  Author: test
  Summary: I
  
  Commit: acefe42b83b3db3ae8b4d05ec0f503f5bbba196e
  Date: 2024-01-01 00:00:00 +00:00
  Author: test
  Summary: L
  
We can get the ancestors difference of a branch using --exclude-ancestors-of
  $ scsc log --repo repo -i "$COMMIT_E2" --exclude-ancestors-of -i "$COMMIT_I"
  Commit: 1653d4c262668855b1effa405b3f777b08e46c72
  Date: 2019-02-02 00:00:00 +00:00
  Author: test
  Summary: E2
  
  Commit: 08ded96707b112bb7b058737b469346de3cbff49
  Date: 2019-02-01 00:00:00 +00:00
  Author: test
  Summary: E1
  
We can combine both options to apply both filters
  $ scsc log --repo repo -i "$COMMIT_M" --exclude-ancestors-of -i "$COMMIT_E2" --descendants-of -i "$COMMIT_C"
  Commit: fc6a43862ab96729167f45045bbfd37532789a5c
  Date: 2024-02-01 00:00:00 +00:00
  Author: test
  Summary: M
  
  Commit: 4eddc88ca261a8115dd01c3af50a17aad50287de
  Date: 2020-01-03 00:00:00 +00:00
  Author: test
  Summary: I
  
  Commit: 159ed529f60d23c614fe315d46a4b2eb5d27b569
  Date: 2020-01-03 00:00:00 +00:00
  Author: test
  Summary: H
  
  Commit: d603e69354506a00833ddb9422cac6053debb733
  Date: 2020-01-02 00:00:00 +00:00
  Author: test
  Summary: G
  
  Commit: 3a61e10442a9b76f8826b05e7ef1a60d33c3bc2d
  Date: 2020-01-01 00:00:00 +00:00
  Author: test
  Summary: F
  

If we specify the same commit as both filters, we get a half-open range of commits.
  $ scsc log --repo repo -i "$COMMIT_F" --descendants-of -i "$COMMIT_C" --exclude-ancestors-of -i "$COMMIT_C"
  Commit: 3a61e10442a9b76f8826b05e7ef1a60d33c3bc2d
  Date: 2020-01-01 00:00:00 +00:00
  Author: test
  Summary: F
  
  Commit: ecbf21bc13d7ec53c820078066ca1dfeb1e8191d
  Date: 2019-01-01 00:00:00 +00:00
  Author: test
  Summary: E
  
  Commit: aaff25985c53d3ba33e0292b2a271c6cdf34e521
  Date: 2018-01-01 00:00:00 +00:00
  Author: test
  Summary: D
  

But we can't specify these arguments multiple times
  $ scsc log --repo repo -i "$COMMIT_F" --descendants-of -i "$COMMIT_C" --descendants-of -i "$COMMIT_E"
  error: the argument '--descendants-of' cannot be used multiple times
  
  Usage: scsc log [OPTIONS] --repo <REPO>
  
  For more information, try '--help'.
  [2]
