# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

thrift_add_library(
  eden_service_thrift_cpp
  eden.thrift
  LANGUAGE cpp2
  SERVICES
    EdenService
  DEPENDS
    eden_config_thrift_cpp
    fb303::fb303_thrift_cpp
)
# eden.thrift's GlobPath adapter is in edencommon.
target_link_libraries(eden_service_thrift_cpp PUBLIC edencommon::edencommon_utils)

if (EDEN_HAVE_THRIFT_PYTHON)
  add_fbthrift_python_library(
    eden_service_thrift_python
    eden.thrift
    NAMESPACE eden.fs.service
    SERVICES
      EdenService
    DEPENDS
      eden_config_thrift_python
      fb303::fb303_core_thrift_python
  )
endif()

thrift_add_library(
  streamingeden_thrift_cpp
  streamingeden.thrift
  LANGUAGE cpp2
  SERVICES
    StreamingEdenService
  DEPENDS
    eden_service_thrift_cpp
)

include(generated_targets.cmake)
eden_add_directory_library(eden_service)

if(BUILD_TESTS)
  add_subdirectory(test)
endif()
