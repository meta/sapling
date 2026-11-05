# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

include(generated_targets.cmake)

add_subdirectory(rpc)
add_subdirectory(xdr)
if(BUILD_TESTS)
  add_subdirectory(test)
  add_subdirectory(testharness)
endif()
