# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
#     http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.

thrift_add_library(
  fb303_thrift_cpp
  fb303_core.thrift
  LANGUAGE cpp2
  SERVICES
    BaseService
)
target_include_directories(
  fb303_thrift_cpp PUBLIC $<INSTALL_INTERFACE:${INCLUDE_INSTALL_DIR}>
)
target_include_directories(
  fb303_thrift_cpp.thrift_includes
  INTERFACE $<INSTALL_INTERFACE:${INCLUDE_INSTALL_DIR}/thrift-files>
)
if (BUILD_SHARED_LIBS)
  set_property(TARGET fb303_thrift_cpp PROPERTY VERSION ${PACKAGE_VERSION})
endif ()
# The names fb303-targets.cmake exports, for projects that build fb303 from
# source.
add_library(fb303::fb303_thrift_cpp ALIAS fb303_thrift_cpp)
add_library(
  fb303::fb303_thrift_cpp.thrift_includes ALIAS fb303_thrift_cpp.thrift_includes
)

install(
  TARGETS fb303_thrift_cpp
  EXPORT fb303-exports
  LIBRARY DESTINATION "${LIB_INSTALL_DIR}"
  ARCHIVE DESTINATION "${LIB_INSTALL_DIR}"
  PUBLIC_HEADER DESTINATION
    "${INCLUDE_INSTALL_DIR}/$<TARGET_PROPERTY:fb303_thrift_cpp,THRIFT_HEADER_DIR>"
)
install(
  TARGETS fb303_thrift_cpp.thrift_includes
  EXPORT fb303-exports
)
install(
  FILES fb303_core.thrift
  DESTINATION "${INCLUDE_INSTALL_DIR}/thrift-files/fb303/thrift"
)

if (PYTHON_EXTENSIONS)
  add_fbthrift_py_library(
    fb303_thrift_py
    fb303_core.thrift
    THRIFT_INCLUDE_DIR "${INCLUDE_INSTALL_DIR}/thrift-files"
    SERVICES
      BaseService
  )
  install_fb_python_library(fb303_thrift_py EXPORT fb303-exports)
  install(
    TARGETS fb303_thrift_py.thrift_includes
    EXPORT fb303-exports
  )
endif ()

if (FB303_THRIFT_PYTHON)
  include(FBThriftPythonLibrary)
  add_fbthrift_python_library(
    fb303_core_thrift_python
    fb303_core.thrift
    THRIFT_INCLUDE_DIR "${INCLUDE_INSTALL_DIR}/thrift-files"
    SERVICES
      BaseService
  )
  add_library(
    fb303::fb303_core_thrift_python.py_lib ALIAS fb303_core_thrift_python.py_lib
  )
  add_library(
    fb303::fb303_core_thrift_python.thrift_includes
    ALIAS fb303_core_thrift_python.thrift_includes
  )
  install_fb_python_library(fb303_core_thrift_python EXPORT fb303-exports)
  install(
    TARGETS fb303_core_thrift_python.thrift_includes
    EXPORT fb303-exports
  )
endif ()
