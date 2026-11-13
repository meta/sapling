# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

"""Subset of curses APIs implemented via termwiz.

Curses "window" (newwin) and "pad" (newpad) are both represented by termwiz
"Surface". They are buffered in memory until "refresh".

Curses "getch" is emulated by termwiz "poll_input".
"""

import bindings

termwiz = bindings.termwiz


class error(RuntimeError):
    pass


class Surface:
    def __init__(self, width, height, begin_y=0, begin_x=0):
        if width == 0 or height == 0:
            main_surface = _get_main_surface()
            screen_width, screen_height = main_surface.surface.dimensions()
            width = width or screen_width
            height = height or screen_height
        self.surface = termwiz.Surface(width, height)
        self.begin_y = begin_y
        self.begin_x = begin_x

    def refresh(
        self,
        pminrow=None,
        pmincol=None,
        sminrow=None,
        smincol=None,
        smaxrow=None,
        smaxcol=None,
        immediate=True,
    ):
        """write changes to the actual screen"""
        # draw this surface to screen
        # pminrow, pmincol - left-hand corner of this (self) surface.
        # sminrow, smincol, smaxrow, smaxcol - rectangle of the (draw
        # destination) screen (inclusive).
        main_surface = _get_main_surface()
        if self is not main_surface:
            # Draw to the main surface first. This happens all in memory.
            if all(
                x is None
                for x in [pminrow, pmincol, smincol, smaxcol, smaxrow, smaxcol]
            ):
                screen_width, screen_height = main_surface.surface.dimensions()
                pminrow = pmincol = 0
                sminrow = self.begin_y
                smincol = self.begin_x
                smaxrow = screen_height - 1
                smaxcol = screen_width - 1
            width = smaxcol - smincol + 1
            height = smaxrow - sminrow + 1
            if width <= 0 or height <= 0:
                return
            # diff_region(self, x, y, width, height, other, other_x, other_y)
            # Computes the change stream required to make the region within
            # self at coordinates x, y and size width, height look like the
            # same sized region within other at coordinates other_x, other_y.
            changes = main_surface.surface.diff_region(
                smincol, sminrow, width, height, self.surface, pmincol, pminrow
            )
            main_surface.surface.add_changes(changes)

        if immediate:
            doupdate()

    def resize(self, height, width):
        self.surface.resize(width, height)
        _check_for_screen_resize()

    def erase(self):
        self.surface.add_change(termwiz.Change({"ClearScreen": "Default"}))

    def clear(self):
        # Curses doc says this function is "like erase(), but also cause the
        # whole window to be repainted.". Practically, crecord has sufficient
        # refresh() calls so it seems okay to ignore the repaint here.
        self.erase()

    def addstr(self, *args):
        """write to this surface, changes are buffered until refresh()

        addstr(str[, attr])
        addstr(y, x, str[, attr])
        """
        y = x = 0
        attr = None
        match len(args):
            case 1:
                text = args[0]
            case 2:
                text, attr = args
            case 3:
                y, x, text = args
            case 4:
                y, x, text, attr = args
            case _:
                raise TypeError("wrong arguments passed to addstr")

        changes = []
        if y != 0 or x != 0:
            changes.append(
                termwiz.Change(
                    {
                        "CursorPosition": {
                            "x": {"Absolute": x},
                            "y": {"Absolute": y},
                        }
                    }
                )
            )
        if attr is not None:
            changes += _curses_attr_to_termwiz_changes(attr)
        if isinstance(text, bytes):
            text = text.decode()
        # avoid scroll down when appending at the last line.
        if self.surface.dimensions()[1] <= self.surface.cursor_position()[1] + 1:
            text = text.rstrip("\n")
        text = text.replace("\n", "\r\n")
        changes.append(termwiz.Change({"Text": text}))
        if attr is not None:
            # reset attributes
            changes += CHANGES_RESET_ATTR
        self.surface.add_changes(changes)

    addch = addstr

    def getyx(self):
        x, y = self.surface.cursor_position()
        return y, x

    def getmaxyx(self):
        x, y = self.surface.dimensions()
        return y, x

    def keypad(self, flag):
        pass

    def getch(self):
        # special: KEY_RESIZE: screen resized
        terminal = _get_main_terminal()
        event = terminal.poll_input()
        if event is None:
            return -1
        if event.get("Resized"):
            if _check_for_screen_resize():
                return KEY_RESIZE
        key_event = event.get("Key")
        if key_event is None:
            return -1
        # See https://docs.rs/termwiz/latest/termwiz/input/struct.KeyEvent.html
        # Examples:
        #   {'key': {'Char': 'f'}, 'modifiers': {'bits': 0}}
        #   {'Key': {'key': 'Escape', 'modifiers': {'bits': 0}}}
        #   {'Key': {'key': 'RightArrow', 'modifiers': {'bits': 0}}}
        #   {'Key': {'key': {'Char': 'l'}, 'modifiers': {'bits': 8}}} # Ctrl+L
        key_code = key_event["key"]
        modifiers = key_event["modifiers"]["bits"]
        # https://docs.rs/termwiz/latest/termwiz/input/enum.KeyCode.html
        match key_code:
            case {"Char": ch}:
                code = ord(ch)
                if modifiers == Modifiers.NONE:
                    return code
                if modifiers & Modifiers.SHIFT:
                    # Ghostty with modifyOtherKeys=2 sends Shift+A as
                    # Char('a')+SHIFT instead of plain 'A' (0x41).
                    # Normalize to uppercase so crecord keybindings like
                    # 'A' (toggle all), 'G', 'J', 'H', 'F' work.
                    return ord(ch.upper())
                if modifiers & (Modifiers.CTRL | Modifiers.RIGHT_CTRL):
                    # emulate curses behavior, ord(upper) & 0x1f
                    return ord(ch.upper()) & 0x1F
            case {"Function": fn}:
                # curses.KEY_F0 = 264
                if modifiers == Modifiers.NONE:
                    return 264 + fn
            case _:
                converted = _TERMWIZ_TO_CURSES.get(key_code)
                if converted is None:
                    return -1
                if modifiers == Modifiers.NONE:
                    return converted[0]
        # not recognized, or not implemented
        return -1

    def getkey(self):
        code = -1
        while code == -1:
            code = self.getch()
        return keyname(code).decode()

    def noutrefresh(self):
        self.refresh(immediate=False)

    def box(self):
        width, height = self.surface.dimensions()
        if width < 2 or height < 2:
            return
        top_line = "┌" + "─" * (width - 2) + "┐"
        self.addstr(0, 0, top_line)
        bottom_line = "└" + "─" * (width - 2) + "┘"
        self.addstr(height - 1, 0, bottom_line)
        for y in range(1, height - 1):
            self.addstr(y, 0, "│")
            self.addstr(y, width - 1, "│")


def doupdate(repaint=None):
    global _screen_resized

    if repaint is None:
        repaint = _screen_resized
        _screen_resized = False

    main_surface = _get_main_surface()
    changes = []

    # Force clear & redraw after screen resize.
    if repaint:
        changes.append(CHANGE_CLEAR_SCREEN)

    # Make the terminal match the main surface, with minimal changes (just diff).
    terminal = _get_main_terminal()
    changes += terminal.surface.diff_screens(main_surface.surface)
    changes += CHANGES_RESET_ATTR

    terminal.surface.add_changes(changes)

    if repaint:
        terminal.repaint()
    else:
        terminal.flush()


# The real terminal. It's not directly written to.
# Updated by diffing against the main surface, and write the diffs.
_main_terminal = None

# Similar to curses stdscr. Changes are written to this first.
_main_surface = None

_screen_resized = False

# curses compatibility


def newpad(nlines, ncols):
    return Surface(ncols, nlines)


def newwin(nlines, ncols, begin_y=0, begin_x=0):
    return Surface(ncols, nlines, begin_y, begin_x)


def raw():
    terminal = _get_main_terminal()
    terminal.set_raw_mode()
    terminal.enter_alternate_screen()
    terminal.surface.add_change(CHANGE_HIDE_CURSOR)
    terminal.flush()


def noraw():
    terminal = _get_main_terminal()
    terminal.set_cooked_mode()
    terminal.exit_alternate_screen()
    terminal.surface.add_change(CHANGE_SHOW_CURSOR)
    terminal.flush()


cbreak = raw


def initscr():
    global _main_terminal, _main_surface
    if _main_terminal is None:
        _main_terminal = termwiz.BufferedTerminal()
        _main_surface = None
    if _main_surface is None:
        width, height = _main_terminal.surface.dimensions()
        _main_surface = Surface(width, height)
    return _main_terminal, _main_surface


def _get_main_terminal():
    return initscr()[0]


def _get_main_surface():
    return initscr()[1]


def endwin():
    global _main_terminal
    if _main_terminal is not None:
        noraw()
        # Intentionally keep _main_terminal to prevent its "Drop" alters the
        # screen unexpectedly.


_color_pairs = {}


def init_pair(pair_number, fg, bg):
    _color_pairs[pair_number] = (fg, bg)


def color_pair(pair_number):
    assert pair_number in _color_pairs, f"{pair_number} was not registered by init_pair"
    # emulate the curses "color pair" index.
    # crecord.py tests pair_num with "< 256".
    return pair_number << 8


def start_color():
    pass


def use_default_colors():
    pass


def def_prog_mode():
    noraw()


def wrapper(func, *args, **kwargs):
    # force a redraw (by setting _screen_resized) when re-entering the wrapper.
    global _main_terminal, _screen_resized
    if _main_terminal is not None:
        _screen_resized = True
    surface = _get_main_surface()
    try:
        raw()
        return func(surface, *args, **kwargs)
    finally:
        endwin()


def curs_set(visible):
    terminal = _get_main_terminal()
    terminal.surface.add_change(CHANGE_SHOW_CURSOR if visible else CHANGE_HIDE_CURSOR)
    terminal.flush()


def resizeterm(*_args):
    # Resize is handled automatically. Just repaint.
    if _check_for_screen_resize():
        surface = _get_main_surface()
        surface.refresh()


def echo():
    noraw()


def _check_for_screen_resize():
    global _main_terminal, _main_surface, _screen_resized
    if _main_terminal is None:
        return False
    old_size = _main_terminal.surface.dimensions()
    maybe_resized = _main_terminal.check_for_resize()
    resized = False
    if maybe_resized:
        new_size = _main_terminal.surface.dimensions()
        if new_size != old_size:
            width, height = new_size
            _main_surface = Surface(width, height)
            _screen_resized = resized = True
    return resized


# https://docs.rs/wezterm-input-types/0.1.0/src/wezterm_input_types/lib.rs.html#483-498
class Modifiers:
    NONE = 0
    SHIFT = 1 << 1
    ALT = 1 << 2
    CTRL = 1 << 3
    SUPER = 1 << 4
    LEFT_ALT = 1 << 5
    RIGHT_ALT = 1 << 6
    LEFT_CTRL = 1 << 8
    RIGHT_CTRL = 1 << 9
    LEFT_SHIFT = 1 << 10
    RIGHT_SHIFT = 1 << 11
    ENHANCED_KEY = 1 << 12


KEY_RESIZE = -2

# See also curses.__dict__
_TERMWIZ_TO_CURSES = {
    "Backspace": [263, "KEY_BACKSPACE"],
    "Clear": [333, "KEY_CLEAR"],
    "Enter": [343, "KEY_ENTER"],
    "Escape": [361, "KEY_EXIT"],
    "PageUp": [339, "KEY_PPAGE"],
    "PageDown": [338, "KEY_NPAGE"],
    "End": [360, "KEY_END"],
    "Home": [262, "KEY_HOME"],
    "LeftArrow": [260, "KEY_LEFT"],
    "RightArrow": [261, "KEY_RIGHT"],
    "UpArrow": [259, "KEY_UP"],
    "DownArrow": [258, "KEY_DOWN"],
    "Select": [385, "KEY_SELECT"],
    "Print": [346, "KEY_PRINT"],
    "Insert": [331, "KEY_IC"],
    "Delete": [330, "KEY_DC"],
    "Help": [363, "KEY_HELP"],
    "Copy": [358, "KEY_COPY"],
    "Resize": [KEY_RESIZE, "KEY_RESIZE"],
}
_CURSES_KEY_CODE_TO_NAME = dict(_TERMWIZ_TO_CURSES.values())


def keyname(code):
    if code >= 1 and code <= 26:
        # ^A -> ^Z (note: tab conflicts with ^I)
        name = f"^{chr(code + 64)}"
    else:
        name = _CURSES_KEY_CODE_TO_NAME.get(code) or chr(code)
    return name.encode()


A_BOLD = 2097152
A_DIM = 1048576
A_ITALIC = 2147483648
A_NORMAL = 0
A_REVERSE = 262144
A_UNDERLINE = 131072

A_COLOR = 65280

COLOR_BLACK = 0
COLOR_BLUE = 4
COLOR_CYAN = 6
COLOR_GREEN = 2
COLOR_MAGENTA = 5
COLOR_RED = 1
COLOR_WHITE = 7
COLOR_YELLOW = 3

ACS_CKBOARD = "░"

# https://docs.rs/termwiz/latest/termwiz/cell/enum.AttributeChange.html
_CURSES_ATTR_TO_TERMWIZ_ATTR = [
    [A_BOLD, {"Intensity": "Bold"}],
    [A_ITALIC, {"Italic": True}],
    [A_REVERSE, {"Reverse": True}],
    [A_UNDERLINE, {"Underline": "Single"}],
]


def _attr_change(attr_change):
    return termwiz.Change({"Attribute": attr_change})


def _curses_attr_to_termwiz_changes(attr):
    changes = []

    def append_attr_change(attr_change):
        changes.append(_attr_change(attr_change))

    for bit, attr_change in _CURSES_ATTR_TO_TERMWIZ_ATTR:
        if attr & bit:
            append_attr_change(attr_change)

    color = (attr & A_COLOR) >> 8
    if color != 0:
        fg, bg = _color_pairs[color]
        if fg >= 0:
            append_attr_change({"Foreground": {"PaletteIndex": fg}})
        if bg >= 0:
            append_attr_change({"Background": {"PaletteIndex": bg}})

    return changes


CHANGE_HIDE_CURSOR = termwiz.Change(
    {"CursorVisibility": "Hidden"},
)
CHANGE_SHOW_CURSOR = termwiz.Change(
    {"CursorVisibility": "Visible"},
)
CHANGE_CLEAR_LINE = termwiz.Change({"ClearToEndOfLine": "Default"})
CHANGE_CLEAR_SCREEN = termwiz.Change({"ClearScreen": "Default"})
CHANGES_RESET_ATTR = [
    _attr_change({"Foreground": "Default"}),
    _attr_change({"Background": "Default"}),
    _attr_change({"Intensity": "Normal"}),
    _attr_change({"Italic": False}),
    _attr_change({"Reverse": False}),
    _attr_change({"Underline": "None"}),
]
