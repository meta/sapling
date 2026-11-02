# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"

setup configuration
  $ setup_common_config

  $ testtool_drawdag -R repo << 'EOF'
  > J G
  > | |
  > I F
  > | |
  > H E
  > |/|
  > C D
  > |/
  > B
  > |
  > A
  > EOF
  A=aa53d24251ff3f54b1b2c29ae02826701b2abeb0079f1bb13b8434b54cd87675
  B=f8c75e41a0c4d29281df765f39de47bca1dcadfdc55ada4ccc2f6df567201658
  C=e32a1e342cdb1e38e88466b4c1a01ae9f410024017aa21dc0a1c5da6b3963bf2
  D=5a25c0a76794bbcc5180da0949a652750101597f0fbade488e611d5c0917e7be
  E=f0c81a03319da010415f712831abe8469ba3c30b93b0b07af175302b8c15f0e6
  F=48779d8d497815015031dc3f3e9888abc8cf8273184ebd9ca8a395e24d501c90
  G=9711852ec4f4b42937dd5b760c7b3f84345bf48c74b7ef3ca7118d1d7928744d
  H=64642fc5a09343c1699e2ecaa5fa1c31fdb19f1e125428cd745327911c0b1d83
  I=03ffabc887d3d9a81be514037b1dfa3020466af9145bafbc33a8880fd8808c01
  J=55e5dbaa7f26e0cfa1c2ee95479e2af088bf81caae4c2356d6eb8dfa6c114284

start SCS server
  $ start_and_wait_for_scs_server

Query linear history of G. Note that D is not printed as it's an ignored merge commit
  $ scsc log -R repo -i $G --linear
  Commit: 39c98a3e58bcc6d77783d505f05029cfc8c9f7c4
  Date: 1970-01-01 00:00:00 +00:00
  Author: author
  Summary: G
  
  Commit: 4eb3fa6303998fa900a272ec99c4ca110bda4727
  Date: 1970-01-01 00:00:00 +00:00
  Author: author
  Summary: F
  
  Commit: b460f118b5da34717001229206cde2741bf9dbfb
  Date: 1970-01-01 00:00:00 +00:00
  Author: author
  Summary: E
  
  Commit: d3b399ca8757acdb81c3681b052eb978db6768d8
  Date: 1970-01-01 00:00:00 +00:00
  Author: author
  Summary: C
  
  Commit: 80521a640a0c8f51dcc128c2658b224d595840ac
  Date: 1970-01-01 00:00:00 +00:00
  Author: author
  Summary: B
  
  Commit: 20ca2a4749a439b459125ef0f6a4f26e88ee7538
  Date: 1970-01-01 00:00:00 +00:00
  Author: author
  Summary: A
  
Query linear history of G excluding linear ancestors of J.
  $ scsc log -R repo -i $G --exclude-ancestors-of -i $J --linear
  Commit: 39c98a3e58bcc6d77783d505f05029cfc8c9f7c4
  Date: 1970-01-01 00:00:00 +00:00
  Author: author
  Summary: G
  
  Commit: 4eb3fa6303998fa900a272ec99c4ca110bda4727
  Date: 1970-01-01 00:00:00 +00:00
  Author: author
  Summary: F
  
  Commit: b460f118b5da34717001229206cde2741bf9dbfb
  Date: 1970-01-01 00:00:00 +00:00
  Author: author
  Summary: E
  
Query linear history of G including only descendants of C.
  $ scsc log -R repo -i $G --descendants-of -i $C --linear
  Commit: 39c98a3e58bcc6d77783d505f05029cfc8c9f7c4
  Date: 1970-01-01 00:00:00 +00:00
  Author: author
  Summary: G
  
  Commit: 4eb3fa6303998fa900a272ec99c4ca110bda4727
  Date: 1970-01-01 00:00:00 +00:00
  Author: author
  Summary: F
  
  Commit: b460f118b5da34717001229206cde2741bf9dbfb
  Date: 1970-01-01 00:00:00 +00:00
  Author: author
  Summary: E
  
  Commit: d3b399ca8757acdb81c3681b052eb978db6768d8
  Date: 1970-01-01 00:00:00 +00:00
  Author: author
  Summary: C
  
Linear history is not supported when any of the "path", "after" or "before" arguments are specified 
  $ scsc log -R repo -i $G --linear --path ""
  error: the argument '--linear' cannot be used with '--path <PATH>'
  
  Usage: scsc log --repo <REPO> --commit-id <commit-id> --linear
  
  For more information, try '--help'.
  [2]

  $ scsc log -R repo -i $G --linear --after "1970-01-01 00:00:00 UTC"
  error: the argument '--linear' cannot be used with '--after <AFTER>'
  
  Usage: scsc log --repo <REPO> --commit-id <commit-id> --linear
  
  For more information, try '--help'.
  [2]

  $ scsc log -R repo -i $G --linear --before "1970-01-01 00:00:00 UTC"
  error: the argument '--linear' cannot be used with '--before <BEFORE>'
  
  Usage: scsc log --repo <REPO> --commit-id <commit-id> --linear
  
  For more information, try '--help'.
  [2]
