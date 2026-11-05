# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

add_library(eden_takeover_thrift)
thrift_generate(
  eden_takeover_thrift takeover.thrift
  LANGUAGE cpp2
  INCLUDE_DIRECTORIES ${PROJECT_SOURCE_DIR} ${THRIFT_INCLUDE_DIR}
)
target_include_directories(eden_takeover_thrift PUBLIC ${PROJECT_BINARY_DIR})

include(generated_targets.cmake)
eden_add_directory_library(eden_takeover)

if(BUILD_TESTS)
  add_subdirectory(test)
endif()
