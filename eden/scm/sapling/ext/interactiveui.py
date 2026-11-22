# Portions Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# interactiveui.py: display information and allow for left/right control


import collections
import os
import sys
from enum import Enum
from typing import Callable, Union

from sapling import error, scmutil, util
from sapling.i18n import _, _x

if not util.iswindows:
    import termios
    import tty


def clearscreen(out):
    if util.istest():
        out.write(_x("===== Screen Refresh =====\n"))
    else:
        out.write(_x("\033[2J"))  # clear screen
        out.write(_x("\033[;H"))  # move cursor


# From:
# https://github.com/pallets/click/blob/master/click/_termui_impl.py#L534
# As per licence:
# Copyright (c) 2014 by Armin Ronacher.
#
# Click uses parts of optparse written by Gregory P. Ward and maintained by
# the Python software foundation.  This is limited to code in the parser.py
# module:
#
# Copyright (c) 2001-2006 Gregory P. Ward.  All rights reserved.
# Copyright (c) 2002-2006 Python Software Foundation.  All rights reserved.
#
# Some rights reserved.
#
# Redistribution and use in source and binary forms, with or without
# modification, are permitted provided that the following conditions are
# met:
#
#    * Redistributions of source code must retain the above copyright
#      notice, this list of conditions and the following disclaimer.
#
#    * Redistributions in binary form must reproduce the above
#      copyright notice, this list of conditions and the following
#      disclaimer in the documentation and/or other materials provided
#      with the distribution.
#
#    * The names of the contributors may not be used to endorse or
#      promote products derived from this software without specific
#      prior written permission.
#
# THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS
# "AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT
# LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR
# A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT
# OWNER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
# SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT
# LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE,
# DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY
# THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
# (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
# OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

# Note: some changes have been made from the source code


def _readraw(fd: int) -> bytes:
    """read the bytes currently available on `fd` with the tty in raw mode"""
    attr = termios.tcgetattr(fd)
    try:
        tty.setraw(fd)
        return os.read(fd, 32)
    finally:
        termios.tcsetattr(fd, termios.TCSADRAIN, attr)


def getchar(
    stdin=None, readraw: Callable[[int], bytes] = _readraw
) -> Union[None, bytes, str]:
    # `stdin` and `readraw` are seams for tests. Production callers leave them
    # unset and get sys.stdin plus the real raw terminal read.
    if stdin is None:
        stdin = sys.stdin
    fd = stdin.fileno()
    if not os.isatty(fd):
        return None
    ch = None
    try:
        ch = readraw(fd)
    except termios.error:
        if ch is None:
            ch = ""
    if ch == b"\x03" or ch == b"\x04":
        return None
    return ch


# End of code from link


def _splitkeypresses(output):
    if output is None:
        return []
    if isinstance(output, str):
        output = output.encode()

    keys = []
    index = 0
    escape_keys = {
        viewframe.KEY_UP,
        viewframe.KEY_DOWN,
        viewframe.KEY_RIGHT,
        viewframe.KEY_LEFT,
    }
    while index < len(output):
        matched = False
        for key in escape_keys:
            if output.startswith(key, index):
                keys.append(key)
                index += len(key)
                matched = True
                break
        if matched:
            continue
        keys.append(output[index : index + 1])
        index += 1
    return keys


class Alignment(Enum):
    top = 1
    bottom = 2


renderstate = collections.namedtuple(
    "renderstate",
    [
        "width",
        "height",
        "statuslines",
        "logsize",
        "visible_start",
        "visible_end",
        "visible_lines",
    ],
)


class viewframe:
    # Useful Keycode Constants
    KEY_J = b"j"
    KEY_K = b"k"
    KEY_Q = b"q"
    KEY_R = b"r"
    KEY_S = b"s"
    KEY_SHIFT_H = b"H"
    KEY_RETURN = b"\r"
    KEY_UP = b"\x1b[A"
    KEY_DOWN = b"\x1b[B"
    KEY_RIGHT = b"\x1b[C"
    KEY_LEFT = b"\x1b[D"

    # framework for view
    def __init__(self, ui, repo):
        self.ui = ui
        self.repo = repo
        self.status = ""
        self._active = True
        ui.disablepager()
        repo.ui.disablepager()

    def render(self):
        # returns list of strings (rows) to print, and an optional tuple of (index, position)
        # Ensures that the row `index` is aligned to the `position` side of the screen if the list is longer than the screen height
        pass

    def handlekeypress(self, key):
        # handle user keypress
        pass

    def handlekeypresses(self, keys):
        redraw = False
        for key in keys:
            self.handlekeypress(key)
            if not self._active:
                return False
            redraw = True
        return redraw

    def finish(self):
        # End interactive session
        self._active = False


def getrenderstate(viewobj, lines, alignment):
    width, height = scmutil.termsize(viewobj.ui)
    statuslines = viewobj.status.splitlines()
    statussize = len(statuslines)
    logsize = height - statussize
    visible_start = 0
    visible_end = len(lines)
    if alignment is not None and len(lines) > logsize:
        index, direction = alignment
        if direction == Alignment.top:
            visible_end = min(len(lines), index + logsize)
            visible_start = min(index, visible_end - logsize)
        elif direction == Alignment.bottom:
            visible_start = max(0, index - logsize)
            visible_end = max(index, visible_start + logsize)
    visible_lines = lines[visible_start:visible_end]
    return renderstate(
        width=width,
        height=height,
        statuslines=statuslines,
        logsize=logsize,
        visible_start=visible_start,
        visible_end=visible_end,
        visible_lines=visible_lines,
    )


def rewrite_rows(viewobj, row_updates) -> None:
    if not row_updates:
        return
    row_updates = sorted(row_updates)
    if util.istest():
        viewobj.ui.write(_x("===== Screen Refresh =====\n"))
        render_cache = getattr(viewobj, "_render_cache", None)
        if render_cache is not None:
            state = render_cache.get("renderstate")
            if state is not None:
                output_lines = state.visible_lines + state.statuslines
                viewobj.ui.write("\n".join(output_lines))
                viewobj.ui.flush()
                return
        viewobj.ui.write("\n".join(line for _screen_row, line in row_updates))
        viewobj.ui.flush()
        return
    for screen_row, line in row_updates:
        viewobj.ui.write(_x("\033[%d;1H") % (screen_row + 1))
        viewobj.ui.write(_x("\033[2K"))
        viewobj.ui.write(_x("\r") + line)
    viewobj.ui.flush()


def _write_output(viewobj):
    clearscreen(viewobj.ui)
    lines, alignment = viewobj.render()
    state = getrenderstate(viewobj, lines, alignment)
    output_lines = state.visible_lines + state.statuslines
    if util.istest():
        viewobj.ui.write("\n".join(output_lines))
    else:
        viewobj.ui.write("\n".join("\r" + line for line in output_lines))
    viewobj.ui.flush()


def view(viewobj, readinput=getchar) -> None:
    if util.iswindows:
        raise error.Abort(_("interactive UI does not support Windows"))
    if viewobj.ui.pageractive:
        raise error.Abort(_("interactiveui doesn't work with pager"))
    # Enter alternate screen
    # TODO: Investigate portability - may only work for xterm
    if not util.istest():
        viewobj.ui.write(_x("\033[?1049h\033[H"))
        # disable line wrapping
        # this is from curses.tigetstr('rmam')
        viewobj.ui.write(_x("\x1b[?7l"))
        viewobj.ui.write(_x("\033[?25l"))  # hide cursor
    try:
        redraw = True
        while viewobj._active:
            if redraw:
                _write_output(viewobj)
                redraw = False
            output = readinput()
            if output is None:
                break
            redraw = viewobj.handlekeypresses(_splitkeypresses(output))
    finally:
        if not util.istest():
            viewobj.ui.write(_x("\033[?25h"))  # show cursor
            # re-enable line wrapping
            # this is from curses.tigetstr('smam')
            viewobj.ui.write(_x("\x1b[?7h"))
            viewobj.ui.flush()
            # Exit alternate screen
            viewobj.ui.write(_x("\033[?1049l"))
