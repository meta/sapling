# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

add_executable(
  edenfs
  service/oss/main.cpp
  notifications/edenfs.exe.manifest
)

# Where edenfsctl looks for the daemon: <prefix>/libexec/eden, or <prefix>/libexec
# on Windows.
if(WIN32)
  install(TARGETS edenfs RUNTIME DESTINATION libexec)
else()
  install(TARGETS edenfs RUNTIME DESTINATION libexec/eden)
endif()
target_include_directories(
  edenfs
  PUBLIC
    ${CMAKE_SOURCE_DIR}
    ${CMAKE_BINARY_DIR}
)
target_link_libraries(
  edenfs
    eden_service
    Folly::folly
)

if(NOT WIN32)
  add_executable(
    edenfs_privhelper
    service/PrivHelperMain.cpp
  )
  # edenfsctl starts edenfs with /usr/local/libexec/eden/edenfs_privhelper, or
  # $EDENFS_PRIVHELPER_PATH, and runs it under sudo unless that's setuid root.
  install(TARGETS edenfs_privhelper RUNTIME DESTINATION libexec/eden)
  target_include_directories(
    edenfs_privhelper
    PUBLIC
      ${CMAKE_SOURCE_DIR}
      ${CMAKE_BINARY_DIR}
  )
  target_link_libraries(
    edenfs_privhelper
      eden_fuse_privhelper
      Folly::folly
  )
endif()

add_library(
  eden_build_config STATIC
  eden-config.cpp
)

if (WIN32)
  add_compile_definitions(_UNICODE UNICODE)
endif()

add_subdirectory(cli_rs/edenfsctl)
add_subdirectory(cli)
add_subdirectory(config)
add_subdirectory(digest)
add_subdirectory(fuse)
add_subdirectory(privhelper)
add_subdirectory(inodes)
add_subdirectory(journal)
add_subdirectory(model)
add_subdirectory(nfs)
add_subdirectory(notifications)
add_subdirectory(prjfs)
add_subdirectory(rust/backtrace_ffi)
add_subdirectory(rust/edenfs-staticlib)
add_subdirectory(rust/manifold_ffi)
add_subdirectory(rust/redirect_ffi)
if(EDEN_HAVE_THRIFT_PYTHON)
  add_subdirectory(py)
endif()
add_subdirectory(service)
add_subdirectory(store)
add_subdirectory(takeover)
add_subdirectory(telemetry)
if(BUILD_TESTS)
  add_subdirectory(testharness)
endif()
add_subdirectory(utils)
