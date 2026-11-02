# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"

Set up Mononoke config.
  $ setup_common_config
  $ setup_configerator_configs
  $ cd $TESTTMP

Initialize test repo.
  $ testtool_drawdag -R repo <<'EOS'
  >     I
  >   H |
  >   |/
  >   G
  >   |
  >   F
  >  /|
  > D |
  > | E
  > C |
  >  \|
  >   B
  >   |
  >   A
  > # bookmark: H main
  > EOS
  A=aa53d24251ff3f54b1b2c29ae02826701b2abeb0079f1bb13b8434b54cd87675
  B=f8c75e41a0c4d29281df765f39de47bca1dcadfdc55ada4ccc2f6df567201658
  C=e32a1e342cdb1e38e88466b4c1a01ae9f410024017aa21dc0a1c5da6b3963bf2
  D=f41e886d61d03021b73d006acf237244086eb7a5d9c7989e44e59b76d3c3f2b5
  E=34a8be8c9dc6e46c954a330f06f792cb62a133d3f7ce9b46e70b91d358970c70
  F=98af487c9b5d2480a596f26bc966978ff787bc5b893112c15e3cb8e99f139b22
  G=8a805eab2f489bea870b845c558585e2b622378e1392dc6809bfc0fcec22338c
  H=8b33e233ecf492e212d58fb634cf95e4fe21216f48434a1e1cb02d9ac2cfde32
  I=9e69737f8a2e4977da64effac9d7a8f5461b14998b37e6f6f9f239dab3fc45e0

start SCS server
  $ start_and_wait_for_scs_server --scuba-log-file "$TESTTMP/scuba.json"

two commits from mainline
  $ scsc is-ancestor --repo repo -i $G -i $A
  false

  $ scsc is-ancestor --repo repo -i $A -i $F
  true

two commits from mainline without ancestry relationship
  $ scsc is-ancestor --repo repo -i $E -i $C
  false

non-mainline commit (I)
  $ scsc is-ancestor --repo repo -i $B -i $I
  true
  $ scsc is-ancestor --repo repo -i $H -i $I
  false

batch ancestry check: filter ancestors of I from a mix of ancestors and non-ancestors
  $ scsc filter-ancestors --repo repo -S bonsai -i "$I" --candidates "$A" "$B" "$C" "$E" "$H" | sort
  34a8be8c9dc6e46c954a330f06f792cb62a133d3f7ce9b46e70b91d358970c70
  aa53d24251ff3f54b1b2c29ae02826701b2abeb0079f1bb13b8434b54cd87675
  e32a1e342cdb1e38e88466b4c1a01ae9f410024017aa21dc0a1c5da6b3963bf2
  f8c75e41a0c4d29281df765f39de47bca1dcadfdc55ada4ccc2f6df567201658

all ancestors of I are returned
  $ scsc filter-ancestors --repo repo -S bonsai -i "$I" --candidates "$A" "$B" "$C" "$D" "$E" "$F" "$G" | sort
  34a8be8c9dc6e46c954a330f06f792cb62a133d3f7ce9b46e70b91d358970c70
  8a805eab2f489bea870b845c558585e2b622378e1392dc6809bfc0fcec22338c
  98af487c9b5d2480a596f26bc966978ff787bc5b893112c15e3cb8e99f139b22
  aa53d24251ff3f54b1b2c29ae02826701b2abeb0079f1bb13b8434b54cd87675
  e32a1e342cdb1e38e88466b4c1a01ae9f410024017aa21dc0a1c5da6b3963bf2
  f41e886d61d03021b73d006acf237244086eb7a5d9c7989e44e59b76d3c3f2b5
  f8c75e41a0c4d29281df765f39de47bca1dcadfdc55ada4ccc2f6df567201658

non-ancestors are excluded (A has no ancestors among I, H, G)
  $ scsc filter-ancestors --repo repo -S bonsai -i "$A" --candidates "$I" "$H" "$G"

self-ancestry (a commit is its own ancestor)
  $ scsc filter-ancestors --repo repo -S bonsai -i "$D" --candidates "$D"
  f41e886d61d03021b73d006acf237244086eb7a5d9c7989e44e59b76d3c3f2b5
