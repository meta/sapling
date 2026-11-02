#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# pyre-strict


import argparse
import collections
import copy
import datetime
import os
import time
from enum import Enum
from textwrap import wrap
from typing import Any, Dict, List, Optional, Tuple

from eden.fs.service.eden.thrift_types import AccessCounts

from . import cmd_util
from .hostname import get_normalized_hostname
from .util import format_cmd, format_mount


class State(Enum):
    INIT = "init"  # setting up
    MAIN = "main"  # normal operation
    HELP = "help"  # help window
    DONE = "done"  # should quit


HELP_SECTION_WHAT = "what "
HELP_SECTION_WHY = "why "
HELP_SECTION_CONCERN = "concern? "


# pyre-fixme[4]: Attribute annotation cannot be `Any`.
# pyre-fixme[2]: Parameter annotation cannot be `Any`.
Row = collections.namedtuple(
    "Row",
    "top_pid mount fuse_reads fuse_writes fuse_total fuse_fetch fuse_memory_cache_imports fuse_disk_cache_imports fuse_backing_store_imports fuse_duration fuse_last_access command",
)

COLUMN_TITLES = Row(
    top_pid="TOP PID",
    mount="MOUNT",
    fuse_reads="FUSE R",
    fuse_writes="FUSE W",
    fuse_total="FUSE COUNT",
    fuse_fetch="FUSE FETCH",
    fuse_memory_cache_imports="MEMORY",
    fuse_disk_cache_imports="DISK",
    fuse_backing_store_imports="IMPORTS",
    fuse_duration="FUSE TIME",
    fuse_last_access="FUSE LAST",
    command="CMD",
)
COLUMN_SPACING = Row(
    top_pid=7,
    mount=12,
    fuse_reads=10,
    fuse_writes=10,
    fuse_total=10,
    fuse_fetch=10,
    fuse_memory_cache_imports=7,
    fuse_disk_cache_imports=7,
    fuse_backing_store_imports=7,
    fuse_duration=10,
    fuse_last_access=10,
    command=25,
)
COLUMN_ALIGNMENT = Row(
    top_pid=">",
    mount="<",
    fuse_reads=">",
    fuse_writes=">",
    fuse_total=">",
    fuse_fetch=">",
    fuse_memory_cache_imports=">",
    fuse_disk_cache_imports=">",
    fuse_backing_store_imports=">",
    fuse_duration=">",
    fuse_last_access=">",
    command="<",
)
COLUMN_REVERSE_SORT = Row(
    top_pid=False,
    mount=False,
    fuse_reads=True,
    fuse_writes=True,
    fuse_total=True,
    fuse_fetch=True,
    fuse_memory_cache_imports=True,
    fuse_disk_cache_imports=True,
    fuse_backing_store_imports=True,
    fuse_duration=True,
    fuse_last_access=True,
    command=False,
)

COUNTER_REGEX = r"((store\.sapling.*)|(store\.hg.*)|(fuse\.([^\.]*)\..*requests.*)|(object_store\..*)|(fs\..*))"


# pyre-fixme[2]: Parameter must be annotated.
def format_duration(duration) -> str:
    modulos = (1000, 1000, 1000, 60, 60, 24)
    suffixes = ("ns", "us", "ms", "s", "m", "h", "d")
    return format_time(duration, modulos, suffixes)


def format_last_access(last_access: float) -> str:
    elapsed = int(time.monotonic() - last_access)

    modulos = (60, 60, 24)
    suffixes = ("s", "m", "h", "d")
    return format_time(elapsed, modulos, suffixes)


# pyre-fixme[2]: Parameter must be annotated.
def format_time(elapsed, modulos, suffixes) -> str:
    for modulo, suffix in zip(modulos, suffixes):
        if elapsed < modulo:
            return f"{elapsed}{suffix}"
        elapsed //= modulo

    last_suffix = suffixes[-1]
    return f"{elapsed}{last_suffix}"


COLUMN_FORMATTING = Row(
    top_pid=lambda x: x,
    mount=lambda x: x,
    fuse_reads=lambda x: x,
    fuse_writes=lambda x: x,
    fuse_total=lambda x: x,
    fuse_fetch=lambda x: x,
    fuse_memory_cache_imports=lambda x: x,
    fuse_disk_cache_imports=lambda x: x,
    fuse_backing_store_imports=lambda x: x,
    fuse_duration=format_duration,
    fuse_last_access=format_last_access,
    command=lambda x: x,
)

ESC_KEY = 27
# by default curses will wait 1000 ms before it delivers the
# escape key press, this is to allow escape key sequences, we are
# reducing this waiting time here to make leaving the help page more
# snappy
ESC_DELAY_MS = 300

DEFAULT_COLOR_PAIR = 0  # white text, black background
COLOR_SELECTED = 1
LOWER_WARN_THRESHOLD_COLOR = 2
UPPER_WARN_THRESHOLD_COLOR = 3


class RequestStage(Enum):
    PENDING = "pending"
    LIVE = "live"


class ImportObject(Enum):
    BLOB = "blob"
    TREE = "tree"
    BLOB_AUX = "blob_aux"
    TREE_AUX = "tree_aux"
    BATCHED_BLOB = "batched_blob"
    BATCHED_TREE = "batched_tree"
    BATCHED_BLOB_AUX = "batched_blob_aux"
    BATCHED_TREE_META = "batched_tree_aux"
    PREFETCH = "prefetch"


class RequestMetric(Enum):
    COUNT = "count"
    MAX_DURATION = "max_duration_us"


STATS_NOT_AVAILABLE = -1
STATS_NOT_IMPLEMENTED = -2

IMPORT_TIME_LOWER_WARN_THRESHOLD = 10  # seconds
IMPORT_TIME_UPPER_WARN_THRESHOLD = 30  # seconds


class Window:
    """
    Class to manage writing to the screen.
    To write to the screen: reset, write lines from top down, and refresh

      ```
      window.reset()

      window.write_line(...)
      ...
      window.write_line(...)

      window.refresh()
      ```
    """

    # pyre-fixme[2]: Parameter must be annotated.
    def __init__(self, stdscr, refresh_rate: int) -> None:
        # pyre-fixme[4]: Attribute must be annotated.
        self.stdscr = stdscr
        self.refresh_rate = refresh_rate
        self.stdscr.timeout(self.refresh_rate * 1000)

        # pyre-fixme[4]: Attribute must be annotated.
        self.height = 0
        # pyre-fixme[4]: Attribute must be annotated.
        self.width = 0
        self._update_screen_size()

        self.current_line = 0  # current line printed
        self.current_x_offset = 0  # horizontal position up to which
        # has been printed on the current line

        # where on the screen does the scrollable section start
        self.scrollable_section_window_start = 0
        # how many lines will show up on the screen max
        self.scrollable_section_window_size = 0
        # internal state, is the current section being written to the
        # scrollable section
        self.scrollable = False
        # how many lines down has the scrollable section been scrolled
        self.scrollable_offset = 0
        # how many total lines have been written to the scrollable section
        self.number_scrollable_lines = 0

    def _update_screen_size(self) -> None:
        self.height, self.width = self.stdscr.getmaxyx()

    def screen_resize(self) -> None:
        self._update_screen_size()
        self.stdscr.redrawwin()

    def reset(self) -> None:
        self.stdscr.erase()
        self.current_line = 0
        self.current_x_offset = 0
        self.number_scrollable_lines = 0

    def refresh(self) -> None:
        self.stdscr.refresh()

    def get_height(self) -> int:
        return self.height

    def get_width(self) -> int:
        return self.width

    # the number of remaining rows left in the window, if in the scrollable
    # section, there is no limit, so this returns None
    def get_remaining_rows(self) -> Optional[int]:
        if self.scrollable:
            return None
        return self.height - self.current_line

    # how much horizontal space remains in the window
    def get_remaining_columns(self) -> int:
        return self.width - self.current_x_offset

    def write_new_line(self, scrollable_ended: bool = False) -> None:
        self.current_line += 1
        self.current_x_offset = 0
        if self.scrollable:
            self.number_scrollable_lines += 1

    # note: this will start writing at what ever the `current_x_offset` is, it
    # will not start writing on a new line, adds a new line to the end of the
    # line printed
    def write_line(
        self, line: str, max_width: int = -1, attr: Optional[int] = None
    ) -> None:
        if max_width == -1:
            max_width = self.width - self.current_x_offset
        self._write(self.current_line, self.current_x_offset, line, max_width, attr)
        self.write_new_line()

    # prints starting from the `current_x_offset`, does NOT add a newline
    def write_part_of_line(
        self, part: str, max_width: int, attr: Optional[int] = None
    ) -> None:
        self._write(self.current_line, self.current_x_offset, part, max_width, attr)
        self.current_x_offset += min(max_width, len(part))

    # prints a line with the line right justified, adds a new line after printing
    # the line
    def write_line_right_justified(
        self, line: str, max_width: int, attr: Optional[int] = None
    ) -> None:
        max_width = min(max_width, self.width - self.current_x_offset)
        width = min(max_width, len(line))
        x = self.width - width
        self._write(self.current_line, x, line, max_width, attr)
        self.write_new_line()

    # pyre-fixme[2]: Parameter must be annotated.
    def write_labeled_rows(self, rows) -> None:
        longest_row_name = max(len(row_name) for row_name in rows)
        for row_name in rows:
            self.write_part_of_line(f"{row_name:<{longest_row_name}}", longest_row_name)
            self.write_lines(rows[row_name])

    # splits the string lines into lines of length at most max_width without
    # spreading words across lines
    # each line will be indented at the `current_x_offset`
    def write_lines(
        self, lines: str, max_width: int = -1, attr: Optional[int] = None
    ) -> None:
        if max_width == -1:
            max_width = self.width - self.current_x_offset
        split_lines = wrap(lines, max_width)
        indent = self.current_x_offset
        for line in split_lines:
            self.current_x_offset = indent
            self.write_line(line, max_width, attr)

    def _write(
        self, y: int, x: int, text: str, max_width: int, attr: Optional[int] = None
    ) -> None:
        if self.scrollable:
            if self.in_scrollable_section(y):
                y = y - self.scrollable_offset
                self._write_to_scr(y, x, text, max_width, attr)
        else:
            self._write_to_scr(y, x, text, max_width, attr)

    def _write_to_scr(
        self, y: int, x: int, text: str, max_width: int, attr: Optional[int] = None
    ) -> None:
        try:
            if attr is None:
                attr = DEFAULT_COLOR_PAIR
            self.stdscr.addnstr(y, x, text, max_width, attr)
        except Exception as ex:
            # When attempting to write to the very last terminal cell curses will
            # successfully display the data but will return an error since the logical
            # cursor cannot be advanced to the next cell.
            #
            # We just ignore the error to handle this case.
            # If you do want to look at errors during development you can enable the
            # following code, but note that the error messages from the curses module
            # usually are not very informative.
            if False:
                with open("/tmp/eden_top.log", "a") as f:
                    f.write(f"error at ({y}, {x}): {ex}\n")

    def get_keypress(self) -> int:
        return int(self.stdscr.getch())

    # Anything written after this before end_scrollable_section will be included
    # in the scrollable section.
    # By default the scrollable section will take up the rest of the vertical
    # space on the screen. Set scrollable_section_height to change this.
    #
    # note: scrollable sections must be started on new lines, if the current
    # cursor is not on a new line, then will move to a new line
    def start_scrollable_section(self, scrollable_section_height: int = -1) -> None:
        if self.scrollable:
            return None
        if self.current_x_offset != 0:
            self.write_new_line()

        self.scrollable_section_window_start = self.current_line
        if scrollable_section_height == -1:
            rows = self.get_remaining_rows()
            if rows is None:
                return None
            self.scrollable_section_window_size = rows
        else:
            self.scrollable_section_window_size = scrollable_section_height
        self.scrollable = True

    # The rest of the writes are not included in the scrollable section
    #
    # note: scrollable sections must be ended on new lines, if the current
    # cursor is not on a new line, then will move to a new line
    def end_scrollable_section(self) -> None:
        if self.current_x_offset != 0:
            self.write_new_line()
        self.current_line = (
            self.scrollable_section_window_start + self.scrollable_section_window_size
        )
        self.scrollable = False
        farthest_scroll = max(
            0, (self.number_scrollable_lines - self.scrollable_section_window_size)
        )
        self.scrollable_offset = min(farthest_scroll, self.scrollable_offset)

    # first line number to be written in the scrollable section
    def get_scrollable_top_line(self) -> int:
        return self.scrollable_section_window_start + self.scrollable_offset

    # last line to be written in the scrollable section
    def get_scrollable_bottom_line(self) -> int:
        return self.get_scrollable_top_line() + self.scrollable_section_window_size - 1

    # does the yth line fall with in the part of the scrollable section that is
    # visible on the screen
    def in_scrollable_section(self, y: int) -> bool:
        return (
            y >= self.get_scrollable_top_line()
            and y <= self.get_scrollable_bottom_line()
        )

    # moves one line down in the scrollable section
    def move_scrollable_up(self) -> None:
        self.scrollable_offset = max(0, self.scrollable_offset - 1)

    # moves one line up in the scrllable section
    def move_scrollable_down(self) -> None:
        farthest_scroll = max(
            0, (self.number_scrollable_lines - self.scrollable_section_window_size)
        )
        self.scrollable_offset = min(farthest_scroll, self.scrollable_offset + 1)

    # resets to the top of the scrollable section
    def reset_offset(self) -> None:
        self.scrollable_offset = 0


class Top:
    def __init__(self) -> None:
        import curses

        # pyre-fixme[4]: Attribute must be annotated.
        self.curses = curses

        os.environ.setdefault("ESCDELAY", str(ESC_DELAY_MS))

        # pyre-fixme[4]: Attribute must be annotated.
        self.state = State.INIT
        self.ephemeral = False
        self.refresh_rate = 1

        # Processes are stored by PID
        self.processes: Dict[int, Process] = {}
        # pyre-fixme[24]: Generic type `list` expects 1 type parameter, use
        #  `typing.List[<element type>]` to avoid runtime subscripting errors.
        self.rows: List = []
        # pyre-fixme[4]: Attribute must be annotated.
        self.selected_column = COLUMN_TITLES.index("FUSE LAST")

        # pyre-fixme[4]: Attribute must be annotated.
        self.pending_imports = {}
        # pyre-fixme[4]: Attribute must be annotated.
        self.fuse_requests_summary = {}

    def start(self, args: argparse.Namespace) -> int:
        self.state = State.MAIN
        self.ephemeral = args.ephemeral
        self.refresh_rate = args.refresh_rate

        eden = cmd_util.get_eden_instance(args)
        with eden.get_thrift_client() as client:
            try:
                self.curses.wrapper(self.run(client))
            except KeyboardInterrupt:
                pass
        return 0

    # pyre-fixme[3]: Return type must be annotated.
    # pyre-fixme[2]: Parameter must be annotated.
    def run(self, client):
        # pyre-fixme[53]: Captured variable `client` is not annotated.
        # pyre-fixme[3]: Return type must be annotated.
        # pyre-fixme[2]: Parameter must be annotated.
        def mainloop(stdscr):
            window = Window(stdscr, self.refresh_rate)

            try:
                # This hides the cursor, which is generally a better UX,
                # but some terminals do not support this. It's fine to continue
                # without this.
                self.curses.curs_set(0)
            except Exception:
                pass

            self.curses.use_default_colors()
            self.curses.init_pair(COLOR_SELECTED, self.curses.COLOR_GREEN, -1)
            self.curses.init_pair(
                LOWER_WARN_THRESHOLD_COLOR, self.curses.COLOR_YELLOW, -1
            )
            self.curses.init_pair(UPPER_WARN_THRESHOLD_COLOR, self.curses.COLOR_RED, -1)

            while self.running():
                if self.state == State.MAIN:
                    self.update(client)
                    self.render(window)
                elif self.state == State.HELP:
                    self.render_help(window)
                self.get_keypress(window)

        return mainloop

    # pyre-fixme[3]: Return type must be annotated.
    def running(self):
        return self.state == State.MAIN or self.state == State.HELP

    # pyre-fixme[2]: Parameter must be annotated.
    def update(self, client) -> None:
        if self.ephemeral:
            self.processes.clear()

        self._update_summary_stats(client)
        counts = client.getAccessCounts(self.refresh_rate)

        for mount, accesses in counts.accessesByMount.items():
            for pid, access_counts in accesses.accessCountsByPid.items():
                if pid not in self.processes:
                    cmd = counts.cmdsByPid.get(pid, b"<kernel>")
                    self.processes[pid] = Process(pid, cmd, mount)

                self.processes[pid].increment_counts(access_counts)
                self.processes[pid].last_access = time.monotonic()

            # When querying older versions of EdenFS fetchCountsByPid will be None
            fetch_counts_by_pid = accesses.fetchCountsByPid or {}
            for pid, fetch_counts in fetch_counts_by_pid.items():
                if pid not in self.processes:
                    cmd = counts.cmdsByPid.get(pid, b"<kernel>")
                    self.processes[pid] = Process(pid, cmd, mount)

                self.processes[pid].set_fetches(fetch_counts)
                self.processes[pid].last_access = time.monotonic()

        for pid in self.processes.keys():
            self.processes[pid].is_running = os.path.exists(f"/proc/{pid}/")

    # pyre-fixme[2]: Parameter must be annotated.
    def _update_summary_stats(self, client) -> None:
        client.flushStatsNow()
        counters = client.getRegexCounters(COUNTER_REGEX)

        self.pending_imports = self._update_import_stats(counters)
        self.fuse_requests_summary = self._update_fuse_request_stats(counters)

    # pyre-fixme[3]: Return type must be annotated.
    # pyre-fixme[2]: Parameter must be annotated.
    def _update_import_stats(self, counters):
        import_stats = {}
        for import_stage in RequestStage:
            import_stats[import_stage] = {}
            for import_type in ImportObject:
                import_stats[import_stage][import_type] = {}
                stage_counter_piece = f"{import_stage.value}_import"
                type_counter_piece = import_type.value
                counter_prefix = (
                    f"store.sapling.{stage_counter_piece}.{type_counter_piece}"
                )
                number_requests = counters.get(
                    f"{counter_prefix}.count", STATS_NOT_AVAILABLE
                )
                longest_outstanding_request = counters.get(
                    f"{counter_prefix}.max_duration_us", STATS_NOT_AVAILABLE
                )  # us
                longest_outstanding_request = (
                    STATS_NOT_AVAILABLE
                    if longest_outstanding_request == STATS_NOT_AVAILABLE
                    else (longest_outstanding_request / 1000000)
                )  # s
                import_stats[import_stage][import_type][RequestMetric.COUNT] = (
                    number_requests
                )
                import_stats[import_stage][import_type][RequestMetric.MAX_DURATION] = (
                    longest_outstanding_request
                )
        return import_stats

    # pyre-fixme[3]: Return type must be annotated.
    # pyre-fixme[2]: Parameter must be annotated.
    def _update_fuse_request_stats(self, counters):
        # collect all the counters for each stage and metric -- this is
        # needed since the mount name is part of the counter name
        # and this should work no matter what the mount is
        raw_metrics = {
            stage: {metric: [] for metric in RequestMetric} for stage in RequestStage
        }
        for key in counters:
            pieces = self.parse_fuse_summary_counter_name(key)
            if pieces is None:
                continue
            stage = pieces[0]
            metric = pieces[1]

            raw_metrics[stage][metric].append(counters[key])

        fuse_requests_stats = {}
        # combine the counters for each stage and metric
        for stage in RequestStage:
            fuse_requests_stats[stage] = {}
            for metric in RequestMetric:
                if raw_metrics[stage][metric] == []:
                    fuse_requests_stats[stage][metric] = STATS_NOT_AVAILABLE
                    continue

                if metric == RequestMetric.COUNT:
                    fuse_requests_stats[stage][metric] = sum(raw_metrics[stage][metric])
                elif metric == RequestMetric.MAX_DURATION:
                    fuse_requests_stats[stage][metric] = (
                        max(raw_metrics[stage][metric]) / 1000000
                    )  # s
                else:
                    raise Exception(f"Aggregation not implemented for: {metric.value}")

        fuse_requests_stats[RequestStage.PENDING][RequestMetric.MAX_DURATION] = (
            STATS_NOT_IMPLEMENTED
        )
        return fuse_requests_stats

    # fuse summary counters have the form:
    # fuse.mount.stage.metric
    # returns a tuple of the parsed stage and metric if this counter
    # fits this form and None otherwise
    def parse_fuse_summary_counter_name(
        self, counter_name: str
    ) -> Optional[Tuple[RequestStage, RequestMetric]]:
        pieces = counter_name.split(".")
        if pieces[0] != "fuse":
            return None

        raw_stage = pieces[2]
        raw_stage = raw_stage.split("_")[0]

        try:
            stage = RequestStage(raw_stage)
            metric = RequestMetric(pieces[3])
            return (stage, metric)
        except KeyError:
            return None
        except ValueError:
            return None

    def render(self, window: Window) -> None:
        window.reset()

        self.render_top_bar(window)
        window.write_new_line()
        self.render_summary_section(window)
        # TODO: daemon memory/inode stats on line 2
        self.render_column_titles(window)

        window.start_scrollable_section()
        self.render_rows(window)
        window.end_scrollable_section()

        window.refresh()

    def render_top_bar(self, window: Window) -> None:
        width = window.get_width()
        TITLE = "eden top"
        hostname = get_normalized_hostname()[:width]
        date = datetime.datetime.now().strftime("%x %X")[:width]
        extra_space = width - len(TITLE + hostname + date)

        # left: title
        window.write_part_of_line(TITLE + " " * (extra_space // 2), width)
        if extra_space >= 0:
            # center: date
            window.write_part_of_line(date, width)
            # right: hostname
            window.write_line_right_justified(hostname, width)

    def render_summary_section(self, window: Window) -> None:
        len_longest_stage = max(
            len(self.get_display_name_for_import_stage(stage)) for stage in RequestStage
        )

        fuse_request_header = "outstanding fuse requests:"
        window.write_line(
            fuse_request_header, window.get_width(), self.curses.A_UNDERLINE
        )
        self.render_fuse_request_section(window, len_longest_stage)

        imports_header = "outstanding object imports:"
        window.write_line(imports_header, window.get_width(), self.curses.A_UNDERLINE)
        self.render_import_section(window, len_longest_stage)

    # pyre-fixme[2]: Parameter must be annotated.
    def render_fuse_request_section(self, window, len_longest_stage) -> None:
        section_size = window.get_width() // 2
        separator = ""
        for stage in RequestStage:
            window.write_part_of_line(separator, len(separator))
            self.render_fuse_request_part(
                window, stage, len_longest_stage, section_size - len(separator)
            )

            separator = "  |  "
        window.write_new_line()

    def render_fuse_request_part(
        self,
        # pyre-fixme[2]: Parameter must be annotated.
        window,
        import_stage: RequestStage,
        # pyre-fixme[2]: Parameter must be annotated.
        len_longest_stage,
        # pyre-fixme[2]: Parameter must be annotated.
        section_size,
    ) -> None:
        stage_display = self.get_display_name_for_import_stage(import_stage)
        header = f"{stage_display:<{len_longest_stage}} -- "
        window.write_part_of_line(header, len(header))

        self.render_request_metrics(
            window, self.fuse_requests_summary[import_stage], section_size - len(header)
        )

    def render_import_section(self, window: Window, len_longest_stage: int) -> None:
        for import_stage in RequestStage:
            self.render_import_row(window, import_stage, len_longest_stage)

    def render_import_row(
        self, window: Window, import_stage: RequestStage, len_longest_stage: int
    ) -> None:
        width = window.get_width()
        stage_display = self.get_display_name_for_import_stage(import_stage)
        header = f"{stage_display:<{len_longest_stage}} -- "
        window.write_part_of_line(header, len(header))

        whole_mid_section_size = width - len(header)
        mid_section_size = whole_mid_section_size // len(ImportObject)

        separator = ""
        for import_type in ImportObject:
            label = f"{separator}{import_type.value}: "
            window.write_part_of_line(label, len(label))

            self.render_request_metrics(
                window,
                self.pending_imports[import_stage][import_type],
                mid_section_size - len(label),
            )

            separator = " | "
        window.write_new_line()

    # pyre-fixme[2]: Parameter must be annotated.
    def render_request_metrics(self, window, metrics, section_size) -> None:
        # split the section between the number of imports and
        # the duration of the longest import
        request_count_size = section_size // 2
        request_time_size = max(3, section_size - request_count_size)

        # number of requests
        requests_for_type = metrics[RequestMetric.COUNT]
        if requests_for_type == STATS_NOT_AVAILABLE:
            requests_for_type = "N/A"
        elif requests_for_type == STATS_NOT_IMPLEMENTED:
            requests_for_type = ""
        request_count_padding = max(request_count_size - 1, 0)
        requests_for_type_display = f"{requests_for_type:>{request_count_padding}} "

        # duration of the longest request
        longest_request = metrics[RequestMetric.MAX_DURATION]  # s
        color = self._get_color_for_pending_import_display(longest_request)
        if longest_request == STATS_NOT_AVAILABLE:
            longest_request_display = "N/A"
            # wrap time in parens
            longest_request_display = f"({longest_request_display})"
        elif longest_request == STATS_NOT_IMPLEMENTED:
            longest_request_display = ""
        else:
            # 3 places after the decimal
            longest_request_display = f"{longest_request:.3f}"
            # set the maximum number of digits, will remove decimals if
            # not enough space & adds unit
            longest_request_display = (
                f"{longest_request_display:.{request_time_size - 3}s}s"
            )
            # wrap time in parens
            longest_request_display = f"({longest_request_display})"

        # right align
        longest_request_display = f"{longest_request_display:>{request_time_size}}"

        request_display = f"{requests_for_type_display}{longest_request_display}"
        window.write_part_of_line(request_display, section_size, color)

    def get_display_name_for_import_stage(self, import_stage: RequestStage) -> str:
        if import_stage == RequestStage.PENDING:
            return "total pending"
        else:
            return str(import_stage.value)

    def _get_color_for_pending_import_display(
        self, longest_request: float
    ) -> Optional[int]:
        if longest_request == 0 or longest_request < IMPORT_TIME_LOWER_WARN_THRESHOLD:
            return None
        elif longest_request < IMPORT_TIME_UPPER_WARN_THRESHOLD:
            return int(self.curses.color_pair(LOWER_WARN_THRESHOLD_COLOR))
        else:
            return int(self.curses.color_pair(UPPER_WARN_THRESHOLD_COLOR))

    def render_column_titles(self, window: Window) -> None:
        self.render_row(window, COLUMN_TITLES, self.curses.A_REVERSE)

    def render_rows(self, window: Window) -> None:
        aggregated_processes: Dict[Tuple[str, str], Process] = {}
        for process in self.processes.values():
            key = process.get_key()
            if key in aggregated_processes:
                aggregated_processes[key].aggregate(process)
            else:
                aggregated_processes[key] = copy.deepcopy(process)

        sorted_processes = sorted(
            aggregated_processes.values(),
            key=lambda p: p.get_row()[self.selected_column],
            reverse=COLUMN_REVERSE_SORT[self.selected_column],
        )

        for process in sorted_processes:
            row = process.get_row()
            row = (fmt(data) for fmt, data in zip(COLUMN_FORMATTING, row))

            style = self.curses.A_BOLD if process.is_running else self.curses.A_NORMAL
            # pyre-fixme[6]: Expected `Row` for 2nd param but got
            #  `Generator[typing.Any, None, None]`.
            self.render_row(window, row, style)

    # pyre-fixme[2]: Parameter must be annotated.
    def render_row(self, window: Window, row: Row, style) -> None:
        row_data = zip(row, COLUMN_ALIGNMENT, COLUMN_SPACING)
        for i, (raw_text, align, space) in enumerate(row_data):
            remaining_space = window.get_remaining_columns()
            if remaining_space <= 0:
                break

            space = min(space, remaining_space)
            if i == len(COLUMN_SPACING) - 1:
                space = max(space, remaining_space)

            text = f"{raw_text:{align}{space}} "

            color = DEFAULT_COLOR_PAIR
            if i == self.selected_column:
                color = self.curses.color_pair(COLOR_SELECTED)
            window.write_part_of_line(text, space + 1, color | style)
        window.write_new_line()

    def render_help(self, window: Window) -> None:
        window.reset()

        self.render_top_bar(window)
        window.write_new_line()

        # pyre-fixme[58]: `-` is not supported for operand types `Optional[int]` and
        #  `int`.
        window.start_scrollable_section(window.get_remaining_rows() - 1)
        self.render_fuse_help(window)
        window.write_new_line()
        self.render_import_help(window)
        window.write_new_line()
        self.render_process_table_help(window)
        window.write_new_line()
        self.render_fuse_fetch_help(window)
        window.end_scrollable_section()

        instruction_line = (
            "use up and down arrow keys to scroll, press ESC to return to the main page"
        )
        window.write_line(instruction_line)

        window.refresh()

    # pyre-fixme[2]: Parameter must be annotated.
    def render_fuse_help(self, window) -> None:
        fuse_req_header = "Outstanding FUSE request:"
        fuse_what = (
            "This section contains the total number and duration "
            "of all the current fuse requests. 'total pending' refers to "
            "all the current FUSE requests that are queued or live from the "
            "kernels view. 'live' only refers to FUSE requests that are "
            "currently being processed by EdenFS."
        )
        fuse_why = (
            "This indicates of for the health of the communication with FUSE, "
            "processing of FUSE requests, as well as the overall health of "
            "EdenFS, as all direct user interaction with the filesystem goes "
            "through here."
        )
        fuse_concern = (
            "When the duration of the longest request starts to get large or "
            "the number of fuse requests is stuck this is concerning. The "
            "metrics will turn yellow when they are abnormal and red if they are "
            "concerning."
        )
        window.write_line(
            fuse_req_header, len(fuse_req_header), self.curses.A_UNDERLINE
        )
        window.write_labeled_rows(
            {
                HELP_SECTION_WHAT: fuse_what,
                HELP_SECTION_WHY: fuse_why,
                HELP_SECTION_CONCERN: fuse_concern,
            }
        )

    # pyre-fixme[2]: Parameter must be annotated.
    def render_fuse_fetch_help(self, window) -> None:
        fuse_fetch_header = "FUSE FETCH:"
        fuse_what = (
            "This column contains the total number of imports cause by "
            "fuse requests for each process listed since the edenfs daemon "
            "started."
        )
        fuse_why = (
            "This indicates which recently running processes are causing a lot of "
            "fetching activities. FUSE requests such as recursive searching will "
            "cause a large number of fetches with significant overhead. "
        )
        fuse_concern = (
            "When this number becomes large, the corresponding process "
            "is very likely to be causing EdenFS running slow. (Those slow processes "
            "will be detected and de-prioritized in our future releases.) "
        )
        window.write_line(
            fuse_fetch_header, len(fuse_fetch_header), self.curses.A_UNDERLINE
        )
        window.write_labeled_rows(
            {
                HELP_SECTION_WHAT: fuse_what,
                HELP_SECTION_WHY: fuse_why,
                HELP_SECTION_CONCERN: fuse_concern,
            }
        )

    # pyre-fixme[2]: Parameter must be annotated.
    def render_import_help(self, window) -> None:
        object_imports_header = "Outstanding object imports:"
        import_what = (
            "This section contains the total number and duration "
            "of all the current object imports from the backing store. "
            "'total pending' refers to all the imports which queued, checking "
            "cache, or live. 'live' only refers to requests that are importing "
            "from the backing store. "
        )
        import_why = (
            "This is useful as an indicator of for the health of the imports "
            "process. Live imports indicate if there are issues importing from "
            "sapling, pending indicate if there is a problem with importing in "
            "general. If the pending imports metrics are concerning but the live "
            "metrics are not, this indicates an issue with queueing, possibly "
            "that a request is being starved :("
        )
        import_concern = (
            "When the duration of the longest import starts to get large or "
            "the number of fuse requests is stuck this is concerning. The "
            "metrics will turn yellow when they are abnormal and red if they are "
            "concerning."
        )
        window.write_line(
            object_imports_header, len(object_imports_header), self.curses.A_UNDERLINE
        )
        window.write_labeled_rows(
            {
                HELP_SECTION_WHAT: import_what,
                HELP_SECTION_WHY: import_why,
                HELP_SECTION_CONCERN: import_concern,
            }
        )

    # pyre-fixme[2]: Parameter must be annotated.
    def render_process_table_help(self, window) -> None:
        process_table_header = "Process table:"
        process_what = (
            "This section contains a list of all the process that have accessed "
            "EdenFS through FUSE since eden top started. The columns in order are  "
            "the process id of the accessing process, the name of the EdenFS "
            "checkout accessed, number of FUSE reads, FUSE writes, total FUSE "
            "requests, total number of imports cause by fuse requests since this "
            "edenfs daemon started (see FUSE FETCH section below for more info), "
            "number of imports from the backing store, sum of the duration "
            "of all the FUSE requests, how long ago the last FUSE request "
            "was, and the command that was run. Use left and right arrow "
            "keys  to change the column the processes are sorted by "
            "(highlighted in green). Use up and down arrow keys to move through "
            "the list."
        )
        process_why = (
            "This can be used to see what work loads EdenFS is processing, to see "
            "that it is making progress, and give more details on what might have "
            "caused EdenFS issues when summary metrics are concerning."
        )
        process_concern = (
            "If the summary stats show something concerning this can tell you "
            "what processes may be causing the issue."
        )
        window.write_line(
            process_table_header, len(process_table_header), self.curses.A_UNDERLINE
        )
        window.write_labeled_rows(
            {
                HELP_SECTION_WHAT: process_what,
                HELP_SECTION_WHY: process_why,
                HELP_SECTION_CONCERN: process_concern,
            }
        )

    # pyre-fixme[2]: Parameter must be annotated.
    def get_keypress(self, window) -> None:
        key = window.get_keypress()
        if key == self.curses.KEY_RESIZE:
            self.curses.update_lines_cols()
            window.screen_resize()
        elif key == ord("q") and self.state == State.MAIN:  # quit
            self.state = State.DONE
        elif key == ord("h"):  # help page
            self.state = State.HELP
            window.reset_offset()
        elif key == ESC_KEY and self.state == State.HELP:  # leave help page
            self.state = State.MAIN
            window.reset_offset()
        elif self.state == State.MAIN and key == self.curses.KEY_LEFT:
            self.move_selector(-1)
            window.reset_offset()
        elif self.state == State.MAIN and key == self.curses.KEY_RIGHT:
            self.move_selector(1)
            window.reset_offset()
        elif key == self.curses.KEY_UP:  # scroll up
            window.move_scrollable_up()
        elif key == self.curses.KEY_DOWN:  # scroll down
            window.move_scrollable_down()

    def move_selector(self, dx: int) -> None:
        self.selected_column = (self.selected_column + dx) % len(COLUMN_TITLES)


class Process:
    pid: int
    cmd: str
    mount: str
    access_counts: AccessCounts
    fuseFetch: int
    last_access: float
    is_running: bool

    def __init__(self, pid: int, cmd: bytes, mount: bytes) -> None:
        self.pid = pid
        self.cmd = format_cmd(cmd)
        self.mount = format_mount(mount)
        self.access_counts = AccessCounts(
            fsChannelTotal=0,
            fsChannelReads=0,
            fsChannelWrites=0,
            fsChannelBackingStoreImports=0,
            fsChannelDurationNs=0,
            fsChannelMemoryCacheImports=0,
            fsChannelDiskCacheImports=0,
        )
        self.fuseFetch = 0
        self.last_access = time.monotonic()
        self.is_running = True

    def get_key(self) -> Tuple[str, str]:
        return (self.cmd, self.mount)

    def aggregate(self, other: "Process") -> None:
        self.increment_counts(other.access_counts)
        self.fuseFetch += other.fuseFetch
        self.is_running |= other.is_running

        # Check if other is more relevant
        if other.is_running or other.last_access > self.last_access:
            self.pid = other.pid
        self.last_access = max(self.last_access, other.last_access)

    def increment_counts(self, access_counts: AccessCounts) -> None:
        # Modern thrift-python types have immutable fields, so create a new object
        self.access_counts = AccessCounts(
            fsChannelReads=self.access_counts.fsChannelReads
            + access_counts.fsChannelReads,
            fsChannelWrites=self.access_counts.fsChannelWrites
            + access_counts.fsChannelWrites,
            fsChannelTotal=self.access_counts.fsChannelTotal
            + access_counts.fsChannelTotal,
            fsChannelMemoryCacheImports=self.access_counts.fsChannelMemoryCacheImports
            + access_counts.fsChannelMemoryCacheImports,
            fsChannelDiskCacheImports=self.access_counts.fsChannelDiskCacheImports
            + access_counts.fsChannelDiskCacheImports,
            fsChannelBackingStoreImports=self.access_counts.fsChannelBackingStoreImports
            + access_counts.fsChannelBackingStoreImports,
            fsChannelDurationNs=self.access_counts.fsChannelDurationNs
            + access_counts.fsChannelDurationNs,
        )

    def set_fetches(self, fetch_counts: int) -> None:
        self.fuseFetch = fetch_counts

    def get_row(self) -> Row:
        return Row(
            top_pid=self.pid,
            mount=self.mount,
            fuse_reads=self.access_counts.fsChannelReads,
            fuse_writes=self.access_counts.fsChannelWrites,
            fuse_total=self.access_counts.fsChannelTotal,
            fuse_fetch=self.fuseFetch,
            fuse_memory_cache_imports=self.access_counts.fsChannelMemoryCacheImports,
            fuse_disk_cache_imports=self.access_counts.fsChannelDiskCacheImports,
            fuse_backing_store_imports=self.access_counts.fsChannelBackingStoreImports,
            fuse_duration=self.access_counts.fsChannelDurationNs,
            fuse_last_access=self.last_access,
            command=self.cmd,
        )
