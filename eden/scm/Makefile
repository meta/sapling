# Portions Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# Copyright Olivia Mackall <olivia@selenic.com> and others
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2 or any later version.

# If you want to change PREFIX, do not just edit it below. The changed
# value won't get passed on to recursive make calls. You should instead
# override the variable on the command like:
#
# % make PREFIX=/opt/ install

export PREFIX=/usr/local

ifeq ($(OS),Windows_NT)
PYTHON := python
endif
PYTHON3 := python3

PYTHON_SYS_EXECUTABLE=$(shell $(PYTHON3) contrib/pick_python.py $(PYTHON3))

BUCK_BUILD_MODE = @fbcode//mode/opt
ifeq ($(OS),Windows_NT)
  BUCK_BUILD_MODE := @fbcode//mode/opt-win
else
  UNAME_S := $(shell uname -s)
  ifeq ($(UNAME_S),Darwin)
    BUCK_BUILD_MODE := @fbcode//mode/opt-mac
  endif
endif

PYTHON_MINOR_VERSION=$(shell $(PYTHON_SYS_EXECUTABLE) -c "import sys; print(sys.version_info.minor)")

BUILD_PY = $(PYTHON_SYS_EXECUTABLE) ./build.py
BUILD_MODE_FLAG = $(if $(OSS),--oss,)
BUILD_ARGS = $(BUILD_MODE_FLAG) --with-python $(PYTHON_SYS_EXECUTABLE)

$(eval HGROOT := $(shell pwd))
HGPYTHONS ?= $(HGROOT)/build/pythons
PURE=
export LANGUAGE=C
export LC_ALL=C
TESTFLAGS ?= $(shell echo $$HGTESTFLAGS)
OSXVERSIONFLAGS ?= $(shell echo $$OSXVERSIONFLAGS)

HGNAME ?= hg
ifeq ($(OS),Windows_NT)
HG_BIN_NAME ?= hg.exe
SL_BIN_NAME ?= sl.exe
else
HG_BIN_NAME ?= hg
SL_BIN_NAME ?= sl
endif
SL_NAME = sl
ISL_NAME ?= isl-dist.tar.xz

MAKE_PID := $(shell echo $$PPID)
JOBS := $(shell ps T | sed -n -Ee 's%.*$(MAKE_PID).*$(MAKE).* (-j|--jobs=) *([0-9][0-9]*).*%\2%p')

# Mac Big Sur doesn't find the standard library without this.
export SDKROOT=/Library/Developer/CommandLineTools/SDKs/MacOSX.sdk

help:
	@echo 'Commonly used make targets:'
	@echo '  local        - build for inplace usage'
	@echo '  tests        - run all tests in the automatic test suite'
	@echo
	@echo 'Example for a local installation (usable in this directory):'
	@echo '  make sl && ./sl version'

all: build

oss: OSS=true
oss: HGNAME=$(SL_NAME)
oss: HG_BIN_NAME=$(SL_BIN_NAME)
oss: local

install-oss: oss
	$(SL_NAME) --kill-chg-daemon
	killall $(SL_NAME)
	sleep 1
	mkdir -p $(DESTDIR)/$(PREFIX)/bin
	cp out/$(SL_BIN_NAME) $(DESTDIR)/$(PREFIX)/bin/$(SL_BIN_NAME)
	mkdir -p $(DESTDIR)/$(PREFIX)/lib
	cp out/isl-dist.tar.xz $(DESTDIR)/$(PREFIX)/lib

local:
	$(BUILD_PY) $(BUILD_ARGS)
	$(RM) $(SL_BIN_NAME)
	ln -s out/$(SL_BIN_NAME) $(SL_BIN_NAME)

hg:
	$(BUILD_PY) $(BUILD_ARGS) sl
	$(RM) $(HG_BIN_NAME)
	ln -s out/$(SL_BIN_NAME) $(HG_BIN_NAME)

sl:
	$(BUILD_PY) $(BUILD_ARGS) sl
	$(RM) $(SL_BIN_NAME)
	ln -s out/$(SL_BIN_NAME) $(SL_BIN_NAME)

build:
	$(BUILD_PY) $(BUILD_ARGS)

getdepsbuild: HGNAME=$(SL_NAME)
getdepsbuild:
	@test -n "$(GETDEPS_BUILD_DIR)" || (echo "GETDEPS_BUILD_DIR is required" >&2; exit 1)
	@test -n "$(GETDEPS_INSTALL_DIR)" || (echo "GETDEPS_INSTALL_DIR is required" >&2; exit 1)
	mkdir -p "$(GETDEPS_BUILD_DIR)/sapling"
	rm -rf out
	ln -sfn "$(GETDEPS_BUILD_DIR)/sapling" out
	HGNAME=$(HGNAME) GETDEPS_BUILD=1 \
		PYTHON_SYS_EXECUTABLE=$(PYTHON_SYS_EXECUTABLE) \
		THRIFT="$(GETDEPS_INSTALL_DIR)/fbthrift/bin/thrift1" \
		$(PYTHON_SYS_EXECUTABLE) ./build.py --getdeps \
		--with-python $(PYTHON_SYS_EXECUTABLE) sl

install-getdeps:
	mkdir -p "$(PREFIX)/bin"
	cp out/$(SL_BIN_NAME) "$(PREFIX)/bin/$(SL_BIN_NAME)"
	cd "$(PREFIX)/bin" && ln -f "$(SL_BIN_NAME)" "$(HG_BIN_NAME)"

check: tests

.PHONY: tests
tests:
	cd tests && PYTHON_SYS_EXECUTABLE=$(PYTHON_SYS_EXECUTABLE) \
	    $(PYTHON_SYS_EXECUTABLE) run-tests.py

buck:
	cp $(shell buck2 build $(BUCK_BUILD_MODE) fbcode//eden/scm:hg --show-full-simple-output) $(HG_BIN_NAME)

buck-isl:
	cp $(shell buck2 build $(BUCK_BUILD_MODE) fbcode//eden/addons:isl-tar --show-full-simple-output) $(ISL_NAME)

# Packaging targets

.PHONY: help all local build install-getdeps getdepsbuild hg sl buck
