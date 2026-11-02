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

Helper for making commit with a specific author:
  $ function commit_as() { # args: name date author
  >   hg commit -qAm $1 -d $2 -u "$3"
  >   export COMMIT_$1="$(hg --debug id -i)"
  > }

Create commits with different authors:
  $ echo "file1" > a
  $ commit_as A "2020-01-01" "Alice Smith <alice@example.com>"

  $ echo "file2" > b
  $ commit_as B "2020-01-02" "Bob Jones <bob@example.com>"

  $ echo "file3" > a
  $ commit_as C "2020-01-03" "Alice Smith <alice@example.com>"

  $ echo "file4" > c
  $ commit_as D "2020-01-04" "Charlie Brown <charlie@example.com>"

  $ echo "file5" > a
  $ commit_as E "2020-01-05" "alice jones <alice.j@example.com>"

start SCS server
  $ start_and_wait_for_scs_server --scuba-log-file "$TESTTMP/scuba.json"

configure repo to push to mononoke
  $ cat >> .hg/hgrc <<EOF
  > [paths]
  > default = mono:repo
  > EOF

import testing repo to mononoke
  $ hg push --to master_bookmark --create -r "$COMMIT_E" -q

Test: filter by author substring (case-insensitive)
  $ scsc log --repo repo -i "$COMMIT_E" --user "alice" --limit 10
  Commit: * (glob)
  Date: 2020-01-05 00:00:00 +00:00
  Author: alice jones <alice.j@example.com>
  Summary: E
  
  Commit: * (glob)
  Date: 2020-01-03 00:00:00 +00:00
  Author: Alice Smith <alice@example.com>
  Summary: C
  
  Commit: * (glob)
  Date: 2020-01-01 00:00:00 +00:00
  Author: Alice Smith <alice@example.com>
  Summary: A
  

Test: case-insensitive matching (uppercase)
  $ scsc log --repo repo -i "$COMMIT_E" --user "ALICE" --limit 10
  Commit: * (glob)
  Date: 2020-01-05 00:00:00 +00:00
  Author: alice jones <alice.j@example.com>
  Summary: E
  
  Commit: * (glob)
  Date: 2020-01-03 00:00:00 +00:00
  Author: Alice Smith <alice@example.com>
  Summary: C
  
  Commit: * (glob)
  Date: 2020-01-01 00:00:00 +00:00
  Author: Alice Smith <alice@example.com>
  Summary: A
  

Test: filter by author with limit
  $ scsc log --repo repo -i "$COMMIT_E" --user "alice" --limit 1
  Commit: * (glob)
  Date: 2020-01-05 00:00:00 +00:00
  Author: alice jones <alice.j@example.com>
  Summary: E
  

Test: no matching author returns empty
  $ scsc log --repo repo -i "$COMMIT_E" --user "nobody" --limit 10

Test: filter by author with path
  $ scsc log --repo repo -i "$COMMIT_E" --user "alice" --path a --limit 10
  Commit: * (glob)
  Date: 2020-01-05 00:00:00 +00:00
  Author: alice jones <alice.j@example.com>
  Summary: E
  
  Commit: * (glob)
  Date: 2020-01-03 00:00:00 +00:00
  Author: Alice Smith <alice@example.com>
  Summary: C
  
  Commit: * (glob)
  Date: 2020-01-01 00:00:00 +00:00
  Author: Alice Smith <alice@example.com>
  Summary: A
  

Test: filter by author with linear history
  $ scsc log --repo repo -i "$COMMIT_E" --user "bob" --linear --limit 10
  Commit: * (glob)
  Date: 2020-01-02 00:00:00 +00:00
  Author: Bob Jones <bob@example.com>
  Summary: B
  

Test: filter by author using short flag
  $ scsc log --repo repo -i "$COMMIT_E" -u "charlie" --limit 10
  Commit: * (glob)
  Date: 2020-01-04 00:00:00 +00:00
  Author: Charlie Brown <charlie@example.com>
  Summary: D
  
