# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

file(GLOB INTEGRATION_SRCS "*.py")

# Linux-specific integration tests.
if(WIN32 OR APPLE)
  list(
    REMOVE_ITEM INTEGRATION_SRCS
    ${CMAKE_CURRENT_SOURCE_DIR}/linux_cgroup_test.py
    ${CMAKE_CURRENT_SOURCE_DIR}/stale_test.py
  )
endif()
if(WIN32)
  list(
    REMOVE_ITEM INTEGRATION_SRCS
    ${CMAKE_CURRENT_SOURCE_DIR}/chown_test.py
    ${CMAKE_CURRENT_SOURCE_DIR}/mmap_test.py
    ${CMAKE_CURRENT_SOURCE_DIR}/takeover_test.py
    ${CMAKE_CURRENT_SOURCE_DIR}/userinfo_test.py
    ${CMAKE_CURRENT_SOURCE_DIR}/stale_inode_test.py
  )
endif()
if (NOT WIN32)
  list(
    REMOVE_ITEM INTEGRATION_SRCS
    ${CMAKE_CURRENT_SOURCE_DIR}/prjfs_stress.py
  )
endif()

# The following integration test files contain one or more tests that require
# pexpect.  For now we disable the entire files when pexpect is unavailable.
if(NOT EDEN_HAVE_PEXPECT)
  list(
    REMOVE_ITEM INTEGRATION_SRCS
    ${CMAKE_CURRENT_SOURCE_DIR}/health_test.py
    ${CMAKE_CURRENT_SOURCE_DIR}/restart_test.py
    ${CMAKE_CURRENT_SOURCE_DIR}/service_log_test.py
    ${CMAKE_CURRENT_SOURCE_DIR}/stop_test.py
    ${CMAKE_CURRENT_SOURCE_DIR}/takeover_test.py
  )
endif()

add_fb_python_unittest(
  integration_tests.egg
  TYPE "dir"
  SOURCES ${INTEGRATION_SRCS}
  WORKING_DIRECTORY "${CMAKE_BINARY_DIR}"
  DEPENDS
    edenfsctl.real.main_lib
    eden_integration_lib
    eden_hg_integration_lib
  ENV
    "CMAKE_SOURCE_DIR=${CMAKE_SOURCE_DIR}"
)

add_subdirectory(hg)
add_subdirectory(helpers)
add_subdirectory(posix)

if(IS_FB_BUILD)
  add_subdirectory(facebook/lib)
endif()

add_subdirectory(lib)
