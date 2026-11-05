# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

include(FindPkgConfig)
include(FBFetchFromManifest)

set(CMAKE_THREAD_PREFER_PTHREAD ON)
set(THREADS_PREFER_PTHREAD_FLAG ON)
find_package(Threads REQUIRED)

# We need to probe for libevent because the current stable version
# of libevent doesn't publish the -L libdir in its exported interface
# which means that folly simply exports `event` to us, leaving us
# unable to resolve and link it.  Pulling in the package via its
# config causes the event target to be defined and satisfies the
# linker.
find_package(Libevent CONFIG QUIET)

# The Boost that folly fetches has only the libraries that folly links.
# TODO(T295360504): remove Boost from eden and edencommon.
list(APPEND BOOST_INCLUDE_LIBRARIES filesystem)
fb_find_or_fetch(folly)
include_directories(${FOLLY_INCLUDE_DIR})

# After folly, so that what folly fetched satisfies these.
find_package(Boost 1.69.0 REQUIRED COMPONENTS filesystem)
find_package(gflags CONFIG REQUIRED)
include_directories(${GFLAGS_INCLUDE_DIR})
find_package(Glog REQUIRED)
include_directories(${GLOG_INCLUDE_DIR})
find_package(fmt CONFIG REQUIRED)

fb_find_or_fetch(fizz)
include_directories(${FIZZ_INCLUDE_DIR})

fb_find_or_fetch(wangle)
include_directories(${WANGLE_INCLUDE_DIR})

fb_find_or_fetch(FBThrift)
include_directories(${FBTHRIFT_INCLUDE_DIR})

# The Python CLI, which eden doesn't build on Windows, uses fb303's
# thrift-python modules.
if (NOT WIN32)
  set(FB303_THRIFT_PYTHON ON)
endif()
fb_find_or_fetch(fb303)
include_directories(${FB303_INCLUDE_DIR})

# The thrift crates' build scripts run THRIFT, or else a thrift1 on PATH, which
# finds .thrift includes through THRIFT_INCLUDE_PATH.
if (WIN32)
  set(eden_thrift_path_separator "$<SEMICOLON>")
else()
  set(eden_thrift_path_separator ":")
endif()
set(EDEN_RUST_THRIFT_ENV
  "THRIFT=$<TARGET_FILE:FBThrift::thrift>"
  "THRIFT_INCLUDE_PATH=$<JOIN:${FBTHRIFT_INCLUDE_DIR}$<SEMICOLON>$<TARGET_PROPERTY:fb303::fb303_thrift_cpp.thrift_includes,INTERFACE_INCLUDE_DIRECTORIES>,${eden_thrift_path_separator}>"
)

if (BUILD_TESTS)
  fb_find_or_fetch_gmock()
  include_directories(${GMOCK_INCLUDEDIR} ${LIBGMOCK_INCLUDE_DIR})
  include(GoogleTest)
  enable_testing()
  # The tests that run Sapling's hg get it as EDEN_HG_BINARY. Like
  # eden/fs/testharness/HgBinary.cpp, prefer hg.real to hg in each directory.
  # Mercurial, which is hg on GitHub's runners, can't stand in for it.
  function(eden_is_sapling_hg result candidate)
    execute_process(
      COMMAND "${candidate}" --version
      OUTPUT_VARIABLE version
      ERROR_QUIET
    )
    if (NOT "${version}" MATCHES "^Sapling ")
      set(${result} FALSE PARENT_SCOPE)
    endif()
  endfunction()
  if (CMAKE_VERSION VERSION_LESS 3.25)
    # Without VALIDATOR, a Mercurial found first ends the search. Don't cache
    # it, so that a later configure searches again.
    find_program(EDEN_HG_BINARY NAMES hg.real hg NAMES_PER_DIR)
    set(is_sapling TRUE)
    if (EDEN_HG_BINARY)
      eden_is_sapling_hg(is_sapling "${EDEN_HG_BINARY}")
    endif()
    if (NOT is_sapling)
      unset(EDEN_HG_BINARY CACHE)
      set(EDEN_HG_BINARY "")
    endif()
  else()
    find_program(
      EDEN_HG_BINARY NAMES hg.real hg NAMES_PER_DIR VALIDATOR eden_is_sapling_hg
    )
  endif()
  if (NOT EDEN_HG_BINARY)
    message(STATUS "Sapling's hg not found, so the tests that run it are not built")
    set(EDEN_HG_BINARY "")
  endif()
endif()

find_package(OpenSSL MODULE REQUIRED)

find_package(Re2 MODULE REQUIRED)
include_directories(${RE2_INCLUDE_DIR})

fb_find_or_fetch(edencommon)

# The following packages ship with their own CMake configuration files
find_package(cpptoml CONFIG QUIET)
if (NOT cpptoml_FOUND)
  # cpptoml asks for CMake 3.1, which leaves CMP0077 unset, so its option()s
  # would clear these settings, and which CMake 4 refuses.
  set(CMAKE_POLICY_DEFAULT_CMP0077 NEW)
  set(CMAKE_POLICY_VERSION_MINIMUM 3.10)
  set(CPPTOML_BUILD_EXAMPLES OFF)
  set(ENABLE_LIBCXX OFF)
  fb_fetch_from_manifest(cpptoml cpptoml)
  unset(CMAKE_POLICY_VERSION_MINIMUM)
  unset(CMAKE_POLICY_DEFAULT_CMP0077)
endif()
find_package(gflags CONFIG REQUIRED)

find_package(BLAKE3 CONFIG QUIET)
if (NOT BLAKE3_FOUND)
  fb_fetch_from_manifest(BLAKE3 blake3)
  # On arm64, the Rust blake3 crate exports its own
  # blake3_compress_in_place_portable() for its NEON code, and binaries link
  # both libraries.
  target_compile_definitions(
    blake3
    PRIVATE
    blake3_compress_in_place_portable=eden_blake3_c_compress_in_place_portable
  )
endif()
include_directories(${BLAKE3_INCLUDE_DIR})

# The Python CLI, not built on Windows, uses thrift-python and the
# thrift-python modules that fb303 builds with FB303_THRIFT_PYTHON.
if (ENABLE_THRIFT_PYTHON AND NOT WIN32)
  include(FBThriftPythonLibrary)
  if (ENABLE_THRIFT_PYTHON STREQUAL "AUTO")
    fb_find_thrift_python_runtime(QUIET)
  else()
    fb_find_thrift_python_runtime()
  endif()
endif()
# Sets <out> to the modules in ARGN that the Python the build uses can't
# import. The Python CLI and the integration tests import them from that
# Python's site-packages, as they do thrift-python.
function(eden_missing_python_modules out)
  set(missing "")
  foreach(module IN LISTS ARGN)
    execute_process(
      COMMAND "${Python3_EXECUTABLE}" -c "import ${module}"
      RESULT_VARIABLE result
      OUTPUT_QUIET
      ERROR_QUIET
    )
    if (NOT result EQUAL 0)
      list(APPEND missing ${module})
    endif()
  endforeach()
  set(${out} "${missing}" PARENT_SCOPE)
endfunction()

set(eden_no_python_cli "")
if (NOT ENABLE_THRIFT_PYTHON)
  set(eden_no_python_cli "ENABLE_THRIFT_PYTHON is OFF")
elseif (NOT (THRIFT_PYTHON_SITE_PACKAGES AND FOLLY_PYTHON_SITE_PACKAGES))
  set(eden_no_python_cli "thrift-python not found")
elseif (NOT TARGET fb303::fb303_core_thrift_python.py_lib)
  set(eden_no_python_cli "fb303 was built without FB303_THRIFT_PYTHON")
else()
  eden_missing_python_modules(eden_missing toml filelock)
  if (eden_missing)
    list(JOIN eden_missing " and " eden_missing)
    set(eden_no_python_cli "${Python3_EXECUTABLE} can't import ${eden_missing}")
  endif()
endif()
if (eden_no_python_cli)
  set(eden_no_python_cli
    "${eden_no_python_cli}, so the Python CLI that edenfsctl hands "
    "commands like clone, start and stop to is not built")
  if (ENABLE_THRIFT_PYTHON AND NOT ENABLE_THRIFT_PYTHON STREQUAL "AUTO")
    message(FATAL_ERROR ${eden_no_python_cli})
  endif()
  message(STATUS ${eden_no_python_cli})
else()
  set(EDEN_HAVE_THRIFT_PYTHON ON)
endif()

# The integration tests drive the Python CLI, so they need thrift-python too.
if (BUILD_TESTS AND EDEN_HAVE_THRIFT_PYTHON)
  set(EDEN_BUILD_INTEGRATION_TESTS ON)
  eden_missing_python_modules(eden_missing psutil parameterized)
  if (eden_missing)
    list(JOIN eden_missing " and " eden_missing)
    message(FATAL_ERROR
      "The integration tests need ${eden_missing}, which ${Python3_EXECUTABLE} "
      "can't import")
  endif()
  # Without pexpect, the integration tests that use it aren't built.
  eden_missing_python_modules(eden_missing pexpect)
  if (NOT eden_missing)
    set(EDEN_HAVE_PEXPECT ON)
  endif()
elseif (BUILD_TESTS)
  message(STATUS
    "The Python CLI isn't built, so neither are the integration tests, which "
    "drive it")
endif()

if (NOT WIN32)
  find_package(CURL REQUIRED)
  find_library(SYSTEMD_LIBRARY systemd)
  if(SYSTEMD_LIBRARY)
    set(SYSTEMD_LIBRARIES "${SYSTEMD_LIBRARY}")
  endif()
endif()

if (WIN32)
  find_package(Prjfs MODULE REQUIRED)
endif()

if (WIN32)
  set(DEFAULT_ETC_EDEN_DIR "C:/ProgramData/Facebook/eden")
else()
  set(DEFAULT_ETC_EDEN_DIR "/etc/eden")
endif()
set(
  ETC_EDEN_DIR "${DEFAULT_ETC_EDEN_DIR}" CACHE STRING
  "The directory for system-wide EdenFS configuration files."
)
