# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

thrift_add_library(
  eden_config_thrift_cpp
  eden_config.thrift
  LANGUAGE cpp2
)

if (EDEN_HAVE_THRIFT_PYTHON)
  add_fbthrift_python_library(
    eden_config_thrift_python
    eden_config.thrift
    NAMESPACE eden.fs.config
  )
endif()

include(generated_targets.cmake)
eden_add_directory_library(eden_config)

if(BUILD_TESTS)
  add_subdirectory(test)
endif()
