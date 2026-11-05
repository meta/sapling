# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

add_library(eden_overlay_thrift_cpp)
thrift_generate(
  eden_overlay_thrift_cpp overlay.thrift
  LANGUAGE cpp2
  INCLUDE_DIRECTORIES ${PROJECT_SOURCE_DIR} ${THRIFT_INCLUDE_DIR}
)
target_include_directories(eden_overlay_thrift_cpp PUBLIC ${PROJECT_BINARY_DIR})

if (EDEN_HAVE_THRIFT_PYTHON)
  add_fbthrift_python_library(
    eden_overlay_thrift_python
    overlay.thrift
    NAMESPACE facebook.eden
  )
endif()

include(generated_targets.cmake)
