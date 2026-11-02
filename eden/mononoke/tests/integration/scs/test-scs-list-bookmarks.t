# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"

setup configuration
  $ INFINITEPUSH_NAMESPACE_REGEX='^scratch/.+$' setup_common_config
  $ setup_configerator_configs
  $ cd "$TESTTMP"

Setup testing repo for mononoke:
  $ testtool_drawdag -R repo --print-hg-hashes << EOF
  > E F H I  L M
  > |/  |/   |/
  > D   G    K N
  > |   |    |/
  > C   C    J    P  A2
  > |        |    |  |
  > B        B    O  A1
  > |             |  |
  > Z             Z  Z
  > # bookmark: A1 alpha1
  > # bookmark: A2 alpha2
  > # bookmark: E echo
  > # bookmark: F foxtrot
  > # bookmark: G golf
  > # bookmark: H hotel
  > # bookmark: I indigo
  > # bookmark: L lima
  > # bookmark: M mike
  > # bookmark: N november
  > # bookmark: P papa
  > EOF
  A1=ccf481140345617a70cce32b64a7d8f6c2e9e1e3
  A2=dd219129e6cdc9bf6b9b59b268961210d5196709
  B=032cf38f21b441d26eddbc713b4a5c67f5a19c0f
  C=28b7b2f8150953a980e7aab5267172ec70e97427
  D=7156ff1f93c9bf961ceab1a1cc9647fb0ab1edb1
  E=beb948b5c723d9c85e7e60a98e540f316135e28a
  F=9f3777a91e953de50b76f3ecec0bf4dde340b6df
  G=939048797b2b4c70f758afed33a5984ab83a271b
  H=62bc4cf0afb58f4e5989b6465d289470052aa5ab
  I=2b4577b34b29fe9835c62a71c4b2afc50956f245
  J=84290b1bf72cf62411159dbb858f92e02d23a98c
  K=6343dcd023de1b36f2cd028f3de973a8e37c7eed
  L=955b44569cdd75d78f7beb1bdaa18c754630c026
  M=17cf527e6b6d3ea424f7a9fd47e4038feca4b4ed
  N=436a6e51b90b737582f0e2eba2758e915650dfaf
  O=131e34e3ae94f13c9c9019aff97c4b2eb20637bf
  P=6134d8967c8ed678c4590f3042932bd9fb943596
  Z=0f57cdea745674b7180da0fb0df5a3dbe54b2c19

start SCS server
  $ start_and_wait_for_scs_server

list bookmarks

  $ scsc list-bookmarks -R repo
  alpha1                                   ccf481140345617a70cce32b64a7d8f6c2e9e1e3
  alpha2                                   dd219129e6cdc9bf6b9b59b268961210d5196709
  echo                                     beb948b5c723d9c85e7e60a98e540f316135e28a
  foxtrot                                  9f3777a91e953de50b76f3ecec0bf4dde340b6df
  golf                                     939048797b2b4c70f758afed33a5984ab83a271b
  hotel                                    62bc4cf0afb58f4e5989b6465d289470052aa5ab
  indigo                                   2b4577b34b29fe9835c62a71c4b2afc50956f245
  lima                                     955b44569cdd75d78f7beb1bdaa18c754630c026
  mike                                     17cf527e6b6d3ea424f7a9fd47e4038feca4b4ed
  november                                 436a6e51b90b737582f0e2eba2758e915650dfaf
  papa                                     6134d8967c8ed678c4590f3042932bd9fb943596

list bookmarks with pagination

  $ scsc list-bookmarks -R repo --limit 5
  alpha1                                   ccf481140345617a70cce32b64a7d8f6c2e9e1e3
  alpha2                                   dd219129e6cdc9bf6b9b59b268961210d5196709
  echo                                     beb948b5c723d9c85e7e60a98e540f316135e28a
  foxtrot                                  9f3777a91e953de50b76f3ecec0bf4dde340b6df
  golf                                     939048797b2b4c70f758afed33a5984ab83a271b

  $ scsc list-bookmarks -R repo --limit 5 --after golf
  hotel                                    62bc4cf0afb58f4e5989b6465d289470052aa5ab
  indigo                                   2b4577b34b29fe9835c62a71c4b2afc50956f245
  lima                                     955b44569cdd75d78f7beb1bdaa18c754630c026
  mike                                     17cf527e6b6d3ea424f7a9fd47e4038feca4b4ed
  november                                 436a6e51b90b737582f0e2eba2758e915650dfaf

  $ scsc list-bookmarks -R repo --limit 5 --after november
  papa                                     6134d8967c8ed678c4590f3042932bd9fb943596

list bookmarks with prefix

  $ scsc list-bookmarks -R repo --prefix alpha
  alpha1                                   ccf481140345617a70cce32b64a7d8f6c2e9e1e3
  alpha2                                   dd219129e6cdc9bf6b9b59b268961210d5196709

list bookmarks with multiple commit identity schemes

  $ scsc list-bookmarks -R repo --prefix alpha -S bonsai,hg
  alpha1:
      bonsai=91a6313af381202c902f16ec5e1f0d34e71c5f849e4d043c250b4fa346fd271c
      hg=ccf481140345617a70cce32b64a7d8f6c2e9e1e3
  alpha2:
      bonsai=4bb642891e467bf4dfb550f8dc1c67baf11a6e96e48f34c3ae442e0e114196d1
      hg=dd219129e6cdc9bf6b9b59b268961210d5196709

list descendant bookmarks

  $ scsc list-bookmarks -R repo -i $C
  echo                                     beb948b5c723d9c85e7e60a98e540f316135e28a
  foxtrot                                  9f3777a91e953de50b76f3ecec0bf4dde340b6df
  golf                                     939048797b2b4c70f758afed33a5984ab83a271b
  hotel                                    62bc4cf0afb58f4e5989b6465d289470052aa5ab
  indigo                                   2b4577b34b29fe9835c62a71c4b2afc50956f245

  $ scsc list-bookmarks -R repo -B golf
  golf                                     939048797b2b4c70f758afed33a5984ab83a271b
  hotel                                    62bc4cf0afb58f4e5989b6465d289470052aa5ab
  indigo                                   2b4577b34b29fe9835c62a71c4b2afc50956f245

list descendant bookmarks with pagination

  $ scsc list-bookmarks -R repo -i $B --limit 5
  echo                                     beb948b5c723d9c85e7e60a98e540f316135e28a
  foxtrot                                  9f3777a91e953de50b76f3ecec0bf4dde340b6df
  golf                                     939048797b2b4c70f758afed33a5984ab83a271b
  hotel                                    62bc4cf0afb58f4e5989b6465d289470052aa5ab
  indigo                                   2b4577b34b29fe9835c62a71c4b2afc50956f245

  $ scsc list-bookmarks -R repo -i $B --limit 5 --after indigo
  lima                                     955b44569cdd75d78f7beb1bdaa18c754630c026
  mike                                     17cf527e6b6d3ea424f7a9fd47e4038feca4b4ed
  november                                 436a6e51b90b737582f0e2eba2758e915650dfaf

list descendant bookmarks with prefix

  $ scsc list-bookmarks -R repo --prefix alpha -i $Z
  alpha1                                   ccf481140345617a70cce32b64a7d8f6c2e9e1e3
  alpha2                                   dd219129e6cdc9bf6b9b59b268961210d5196709

add a couple of scratch bookmarks

  $ start_and_wait_for_mononoke_server

  $ hg clone -q mono:repo client --noupdate
  $ cd client
  $ cat >> .hg/hgrc <<EOF
  > [extensions]
  > commitcloud=
  > [infinitepush]
  > server=False
  > branchpattern=re:scratch/.+
  > EOF
  $ hg checkout -q $H
  $ echo scratch > scratch
  $ hg commit -Aqm "scratch commit"

  $ start_and_wait_for_mononoke_server
  $ hg push -qr . --to scratch/hotel1 --create
  $ hg push -qr . --to scratch/hotel2 --create

  $ hg checkout -q $I
  $ echo scratch > scratch
  $ hg commit -Aqm "scratch commit 2"
  $ hg push -qr . --to scratch/indigo1 --create

normal lists don't include them

  $ scsc list-bookmarks -R repo --prefix scratch

add --include-scratch to list the scratch bookmarks

  $ scsc list-bookmarks -R repo --include-scratch --prefix scratch
  scratch/hotel1                           e72dadd4ea37cd22f45ddd120eebc2ad56fa3017
  scratch/hotel2                           e72dadd4ea37cd22f45ddd120eebc2ad56fa3017
  scratch/indigo1                          a465bebc0c9b020ff97692b202f34fef2bda9a24

this works for descendant requests, too

  $ scsc list-bookmarks -R repo --include-scratch --prefix scratch -B hotel
  scratch/hotel1                           e72dadd4ea37cd22f45ddd120eebc2ad56fa3017
  scratch/hotel2                           e72dadd4ea37cd22f45ddd120eebc2ad56fa3017

the --include-scratch option requires the prefix

  $ scsc list-bookmarks -R repo --include-scratch
  *scsc: error: SourceControlService::repo_list_bookmarks failed with request_error(RequestError): RequestError: prefix required to list scratch bookmarks (glob)
  [1]
