# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

include(generated_targets.cmake)

add_subdirectory(sl)
add_subdirectory(filter)

if(BUILD_TESTS)
  add_subdirectory(test)
endif()
