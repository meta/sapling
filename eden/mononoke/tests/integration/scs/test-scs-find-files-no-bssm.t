# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"

Setup
  $ setup_common_config
  $ testtool_drawdag -R repo --derive-all <<EOF
  > A-B-C-D-E
  > # modify: A file content
  > # modify: B dir1/file content
  > # modify: C dir2/file content
  > # modify: D dir1/subdir1_1/file content
  > # modify: E dir1/other_file content
  > # bookmark: E master
  > EOF
  A=a5b7adf93c34a1966e723cbb7ccb6e6955d4fe3dfacac8f2929dfb08752df21f
  B=70fcf0e1e3f91bdb7a9ecbe49965152c77407881115a34f7f862c10de49e98d3
  C=5cc74f186c39c21f15b1c95b23448765b7dffbd5e602a22fc98d6140dc51e3f3
  D=b2f4058d06e5125bbf9accfd520a0742886839aa3d329b791b567a6a708cf3c2
  E=390e0eae00468dbcb433e55a424c18180ea94d78af9be0099213a6459bd943e9
  $ start_and_wait_for_scs_server
Test out
  $ scsc find-files -R repo -B master | sort
  A
  B
  C
  D
  E
  dir1/file
  dir1/other_file
  dir1/subdir1_1/file
  dir2/file
  file
  $ scsc find-files -R repo -B master -p dir1 | sort
  dir1/file
  dir1/other_file
  dir1/subdir1_1/file
  $ scsc find-files -R repo -B master -p dir2 | sort
  dir2/file
  $ scsc find-files -R repo -B master -f file | sort
  dir1/file
  dir1/subdir1_1/file
  dir2/file
  file
  $ scsc find-files -R repo -B master -f A -f B | sort
  A
  B
  $ scsc find-files -R repo -B master -p dir1 -f file | sort
  dir1/file
  dir1/subdir1_1/file
Test with ordered requests
  $ scsc find-files -R repo -B master --after "" --limit 3
  A
  B
  C
  $ scsc find-files -R repo -B master --after "C" --limit 3
  D
  E
  dir1/file
  $ scsc find-files -R repo -B master --after "dir1/other_file" --limit 3
  dir1/subdir1_1/file
  dir2/file
  file
  $ scsc find-files -R repo -B master --after "" --limit 2 -f file
  dir1/file
  dir1/subdir1_1/file
  $ scsc find-files -R repo -B master --after "dir1/file" --limit 2 -f file
  dir1/subdir1_1/file
  dir2/file
  $ scsc find-files -R repo -B master --after "" --limit 10 -p dir2 -f file
  dir2/file
