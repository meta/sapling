# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

file(GLOB EDENFSCTL_SRCS "*.py" "doctor/*.py")

if (IS_FB_BUILD)
  file(GLOB EDENFSCTL_FB_DOCTOR_SRCS "doctor/facebook/*.py")
  file(GLOB EDENFSCTL_FB_SRCS "facebook/*.py")
  list(APPEND EDENFSCTL_SRCS ${EDENFSCTL_FB_DOCTOR_SRCS} ${EDENFSCTL_FB_SRCS})
endif()

if (EDEN_HAVE_THRIFT_PYTHON)
  add_fb_thrift_python_executable(
    edenfsctl.real
    MAIN_MODULE eden.fs.cli.main:zipapp_main
    SOURCES ${EDENFSCTL_SRCS}
    DEPENDS
      eden_py
      eden_overlay_thrift_python
      eden_scm_redact
  )
  install_fb_python_executable(edenfsctl.real LIB_DESTINATION lib/eden)
endif()
