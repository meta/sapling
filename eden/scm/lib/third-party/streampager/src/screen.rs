//! A screen displaying a single file.
//!
//! Some terms are used for specific meanings within this file:
//!
//! * `line` means a line in the file.
//! * `row` means a row on the screen.
//! * `height` means a height in rows.
//! * `portion` means the portion within a line shown on a single row
//!    when a line has been wrapped onto multiple rows.
//!
//! An example of how these might map to the screen is shown below:
//!
//! ```text
//! File                Screen
//! ====                ======
//! LINE 0 PORTION 0 \  +------------------+__v top_line = 0, top_line_portion = 1
//! LINE 0 PORTION 1 \  | LINE 0 PORTION 1 |  ^
//! LINE 0 PORTION 2    | LINE 0 PORTION 2 |  |
//! LINE 1              | LINE 1           |  |
//! LINE 2 PORTION 0 \  | LINE 2 PORTION 0 |  | height = 8
//! LINE 2 PORTION 1    | LINE 2 PORTION 1 |  |
//! LINE 3              | LINE 3           |  |
//! LINE 4 PORTION 0 \  | LINE 4 PORTION 0 |__|___v bottom_line = 4
//! LINE 4 PORTION 1    |<= RULER ========>|__v___  overlay_height = 1
//!                     +------------------+      ^
//!
//! ```

use std::cmp::{max, min};
use std::num::NonZeroUsize;
use std::sync::Arc;

use termwiz::cell::{CellAttributes, Intensity};
use termwiz::color::{AnsiColor, ColorAttribute};
use termwiz::input::KeyEvent;
use termwiz::surface::change::Change;
use termwiz::surface::{CursorVisibility, Position};

use crate::action::Action;
use crate::bindings::{Binding, Keymap};
use crate::command;
use crate::config::{Config, WrappingMode};
use crate::display::Capabilities;
use crate::display::DisplayAction;
use crate::error::Error;
use crate::event::EventSender;
use crate::file::{File, FileInfo};
use crate::line::Line;
use crate::line_cache::LineCache;
use crate::progress::Progress;
use crate::prompt::Prompt;
use crate::prompt_history;
use crate::refresh::Refresh;
use crate::ruler::Ruler;
use crate::search::{MatchMotion, Search, SearchKind};
use crate::util::number_width;

const LINE_CACHE_SIZE: usize = 1000;

/// The state of the previous render.
#[derive(Clone, Debug, Default)]
struct RenderState {
    /// The number of columns on screen.
    width: usize,

    /// The number of rows on screen.
    height: usize,

    /// The file line at the top of the screen.
    top_line: usize,

    /// The porition of the file line at the top of the screen.
    top_line_portion: usize,

    /// The file line at the bottom of the screen.
    bottom_line: usize,

    /// The column at the left of the screen.
    left: usize,

    /// The height of the overlay.
    overlay_height: usize,

    /// The number of lines in the file.
    file_lines: usize,

    /// The number of searched lines.
    searched_lines: usize,

    /// The number of lines in the error file.
    error_file_lines: usize,

    /// The last line portion of the error file.  This may be incomplete and needs to be
    /// re-rendered every time.
    error_file_last_line_portion: Option<(usize, usize)>,

    /// The number of rows in the progress indicator.
    progress_height: usize,

    /// The number of rows showing the error file.
    error_file_height: usize,

    /// The row the ruler was rendered to.
    ruler_row: Option<usize>,

    /// The row the prompt was rendered to.
    prompt_row: Option<usize>,

    /// The row the error message was rendered to.
    error_row: Option<usize>,

    /// The row search status was rendered to.
    search_row: Option<usize>,

    /// The start and end row of each file line in view.
    file_line_rows: Vec<(usize, usize)>,
}

impl RenderState {
    /// Returns the start and end row of the file line on the screen, if the
    /// file line is currently visible.
    fn file_line_rows(&self, file_line_index: usize) -> Option<(usize, usize)> {
        if file_line_index >= self.top_line && file_line_index < self.bottom_line {
            self.file_line_rows
                .get(file_line_index - self.top_line)
                .cloned()
        } else {
            None
        }
    }
}

/// A screen that is displaying a single file.
pub(crate) struct Screen {
    /// The file being displayed.
    pub(crate) file: File,

    /// An error file potentially being overlayed.
    error_file: Option<File>,

    /// The progress indicator potentially being overlayed.
    progress: Option<Progress>,

    /// The keymap in use.
    keymap: Arc<Keymap>,

    /// The current width.
    width: usize,

    /// The current height.
    height: usize,

    /// The current left-most column when not wrapping
    left: usize,

    /// The current top-most line
    top_line: usize,

    /// The top-most portion of the top-most line
    top_line_portion: usize,

    /// Wrapping mode.
    wrapping_mode: WrappingMode,

    /// The state of the previous render.
    rendered: RenderState,

    /// Whether line numbers are being displayed.
    line_numbers: bool,

    /// Cache of `Line`s to display.
    line_cache: LineCache,

    /// Cache of `Line`s for the current search.
    search_line_cache: LineCache,

    /// The current error that should be displayed to the user.
    pub(crate) error: Option<String>,

    /// The current prompt that the user is entering a response into.
    prompt: Option<Prompt>,

    /// The current ongoing search.
    search: Option<Search>,

    /// The ruler.
    ruler: Ruler,

    /// Whether the ruler should be shown.
    show_ruler: bool,

    /// Whether we are following the end of the file.  If `true`, we will scroll down to the
    /// end as new input arrives.
    following_end: bool,

    /// Scroll to a particular line in the file.
    pending_absolute_scroll: Option<usize>,

    /// Scroll relative number of rows.
    pending_relative_scroll: isize,

    /// Which parts of the screens need to be re-rendered.
    pending_refresh: Refresh,

    /// Configuration set by the top-level `Pager`.
    config: Arc<Config>,

    /// Repeat the next operation for the given times.
    repeat_count: Option<usize>,
}

impl Screen {
    /// Create a screen that displays a file.
    pub(crate) fn new(file: File, config: Arc<Config>) -> Result<Screen, Error> {
        Ok(Screen {
            error_file: None,
            progress: None,
            keymap: config.keymap.load()?,
            width: 0,
            height: 0,
            left: 0,
            top_line: 0,
            top_line_portion: 0,
            wrapping_mode: config.wrapping_mode,
            rendered: RenderState::default(),
            line_numbers: false,
            line_cache: LineCache::new(NonZeroUsize::new(LINE_CACHE_SIZE).unwrap()),
            search_line_cache: LineCache::new(NonZeroUsize::new(LINE_CACHE_SIZE).unwrap()),
            error: None,
            prompt: None,
            search: None,
            ruler: Ruler::new(file.clone()),
            show_ruler: config.show_ruler,
            following_end: false,
            pending_absolute_scroll: None,
            pending_relative_scroll: 0,
            pending_refresh: Refresh::None,
            config,
            file,
            repeat_count: None,
        })
    }

    /// Resize the screen
    pub(crate) fn resize(&mut self, width: usize, height: usize) {
        if self.width != width || self.height != height {
            self.width = width;
            self.height = height;
            self.pending_refresh = Refresh::All;
        }
    }

    /// Get the screen width
    pub(crate) fn width(&self) -> usize {
        self.width
    }

    /// Get the current overlay height
    pub(crate) fn overlay_height(&self) -> usize {
        self.rendered.overlay_height
    }

    /// Get the screen's keymap
    pub(crate) fn keymap(&self) -> &Keymap {
        &self.keymap
    }

    /// Renders the part of the screen that has changed.
    pub(crate) fn render(&mut self, caps: &Capabilities) -> Vec<Change> {
        let mut changes = vec![
            // Hide the cursor while we render things.
            Change::CursorVisibility(CursorVisibility::Hidden),
        ];

        // Set up the render state.
        let mut render = RenderState {
            width: self.width,
            height: self.height,
            file_lines: self.file.lines(),
            error_file_lines: self.error_file.as_ref().map_or(0, |f| f.lines()),
            ..Default::default()
        };
        if let Some(search) = self.search.as_ref() {
            render.searched_lines = search.searched_lines();
        }
        let mut pending_refresh = self.pending_refresh.clone();
        let file_loaded = self.file.loaded();
        let file_width = if self.line_numbers {
            render.width - number_width(render.file_lines) - 2
        } else {
            render.width
        };

        #[derive(Copy, Clone, Debug)]
        enum RowContent {
            Empty,
            FileLinePortions {
                line: usize,
                first_portion: usize,
                rows: usize,
            },
            Blank,
            Error,
            Prompt,
            Search,
            Ruler,
            ErrorFileLinePortion(usize, usize),
            ProgressLine(usize),
        }

        let mut row_contents = vec![RowContent::Empty; render.height];

        // Assign the lines of the error file to rows (in reverse order).
        let error_file_line_portions: Vec<_> = (0..render.error_file_lines)
            .rev()
            .flat_map(|line_index| {
                let line = self
                    .error_file
                    .as_ref()
                    .and_then(|f| f.with_line(line_index, |line| Line::new(line_index, line)));
                if let Some(line) = line {
                    let height = line.height(render.width, WrappingMode::WordBoundary);
                    (0..height)
                        .rev()
                        .map(|portion| (line_index, portion))
                        .collect()
                } else {
                    Vec::new()
                }
            })
            .take(8)
            .collect();

        // Compute where the overlay will go
        let ruler_height = self.show_ruler as usize;
        render.progress_height = self.progress.as_ref().map_or(0, |f| f.lines());
        render.error_file_height = error_file_line_portions.len();
        render.overlay_height = render.progress_height
            + render.error_file_height
            + ruler_height
            + self.search.is_some() as usize
            + self.prompt.is_some() as usize
            + self.error.is_some() as usize;

        if render.overlay_height < render.height {
            let mut row = render.height - render.progress_height;
            for progress_line in 0..render.progress_height {
                row_contents[row + progress_line] = RowContent::ProgressLine(progress_line);
            }
            row -= render.error_file_height;
            render.error_file_last_line_portion = error_file_line_portions.first().cloned();
            for (error_file_row, error_file_line_portion) in
                error_file_line_portions.into_iter().rev().enumerate()
            {
                row_contents[row + error_file_row] = RowContent::ErrorFileLinePortion(
                    error_file_line_portion.0,
                    error_file_line_portion.1,
                );
            }
            if self.show_ruler {
                row -= 1;
                row_contents[row] = RowContent::Ruler;
                render.ruler_row = Some(row);
            }
            if self.search.is_some() {
                row -= 1;
                row_contents[row] = RowContent::Search;
                render.search_row = Some(row);
            }
            if self.prompt.is_some() {
                row -= 1;
                row_contents[row] = RowContent::Prompt;
                render.prompt_row = Some(row);
            }
            if self.error.is_some() {
                row -= 1;
                row_contents[row] = RowContent::Error;
                render.error_row = Some(row);
            }
        } else {
            // The overlay doesn't fit.  Only show the prompt (if any).
            render.overlay_height = self.prompt.is_some() as usize;
            render.progress_height = 0;
            render.error_file_height = 0;
            render.error_file_last_line_portion = None;
            if self.prompt.is_some() {
                let prompt_row = render.height.saturating_sub(1);
                row_contents[prompt_row] = RowContent::Prompt;
                render.prompt_row = Some(prompt_row);
            }
        }

        let file_view_height = render.height - render.overlay_height;

        let (end_top_line, end_top_line_portion) = {
            let mut top_line = render.file_lines;
            let mut top_line_portion = 0;
            let mut remaining = file_view_height;
            while top_line > 0 && remaining > 0 {
                top_line -= 1;
                if let Some(line) = self.line_cache.get_or_create(&self.file, top_line, None) {
                    let line_height = line.height(file_width, self.wrapping_mode);
                    if line_height > remaining {
                        top_line_portion = line_height - remaining;
                        break;
                    }
                    remaining -= line_height;
                }
            }
            (top_line, top_line_portion)
        };

        // Scroll to end
        if self.following_end {
            // See if this is a small relative downwards scroll
            let mut relative_scroll = None;
            if (end_top_line, end_top_line_portion) >= (self.top_line, self.top_line_portion) {
                let mut scroll_by = 0;
                let mut scroll_line = self.top_line;
                let mut scroll_line_portion = self.top_line_portion;
                while scroll_line < end_top_line {
                    if let Some(line) = self.line_cache.get_or_create(&self.file, scroll_line, None)
                    {
                        let line_height = line.height(file_width, self.wrapping_mode);
                        scroll_by += line_height.saturating_sub(scroll_line_portion);
                        if scroll_by > file_view_height {
                            // We've scrolled an entire screen, just jump straight to the end.
                            break;
                        }
                    }
                    scroll_line += 1;
                    scroll_line_portion = 0;
                }
                if scroll_line == end_top_line {
                    scroll_by += end_top_line_portion.saturating_sub(scroll_line_portion);
                    relative_scroll = Some(scroll_by);
                }
            }
            if let Some(relative_scroll) = relative_scroll {
                self.pending_relative_scroll = relative_scroll as isize;
            } else {
                self.top_line = end_top_line;
                self.top_line_portion = end_top_line_portion;
                pending_refresh.add_range(0, file_view_height);
            }
        }

        // Perform pending absolute scroll
        if let Some(line) = self.pending_absolute_scroll.take() {
            self.top_line = line;
            self.top_line_portion = 0;
            pending_refresh.add_range(0, file_view_height);
            // Scroll up so that the target line is in the center of the
            // file view.
            self.pending_relative_scroll -= (file_view_height / 2) as isize;
        }

        enum Direction {
            None,
            Up,
            Down,
        }

        // Perform pending relative scroll
        let mut scroll_direction = Direction::None;
        let mut scroll_distance = 0;
        if self.pending_relative_scroll < 0 {
            scroll_direction = Direction::Up;
            let mut scroll_up = (-self.pending_relative_scroll) as usize;
            let mut top_line = self.top_line;
            let mut top_line_portion = self.top_line_portion;
            if top_line_portion > 0 {
                let top_line_remaining = min(top_line_portion, scroll_up);
                top_line_portion -= top_line_remaining;
                scroll_up -= top_line_remaining;
                scroll_distance += top_line_remaining;
            }
            while scroll_up > 0 && top_line > 0 {
                top_line -= 1;
                top_line_portion = 0;
                if let Some(line) = self.line_cache.get_or_create(&self.file, top_line, None) {
                    let line_height = line.height(file_width, self.wrapping_mode);
                    if line_height > scroll_up {
                        scroll_distance += scroll_up;
                        top_line_portion = line_height - scroll_up;
                        break;
                    }
                    scroll_distance += line_height;
                    scroll_up -= line_height;
                }
            }
            self.top_line = top_line;
            self.top_line_portion = top_line_portion;
        } else if self.pending_relative_scroll > 0 {
            scroll_direction = Direction::Down;
            let mut scroll_down = self.pending_relative_scroll as usize;
            let mut top_line = self.top_line;
            let mut top_line_portion = self.top_line_portion;
            let (max_top_line, max_top_line_portion) = if self.config.scroll_past_eof {
                let last_line = render.file_lines.saturating_sub(1);
                let line_height = if let Some(line) =
                    self.line_cache.get_or_create(&self.file, last_line, None)
                {
                    line.height(file_width, self.wrapping_mode)
                } else {
                    1
                };
                (last_line, line_height.saturating_sub(1))
            } else {
                (end_top_line, end_top_line_portion)
            };
            while scroll_down > 0
                && (top_line, top_line_portion) < (max_top_line, max_top_line_portion)
            {
                if let Some(line) = self.line_cache.get_or_create(&self.file, top_line, None) {
                    let line_height = line.height(file_width, self.wrapping_mode);
                    let line_height_remaining = line_height.saturating_sub(top_line_portion);
                    if line_height_remaining > scroll_down {
                        scroll_distance += scroll_down;
                        top_line_portion += scroll_down;
                        break;
                    }
                    scroll_distance += line_height_remaining;
                    scroll_down -= line_height_remaining;
                }
                top_line += 1;
                top_line_portion = 0;
            }
            self.top_line = top_line;
            self.top_line_portion = top_line_portion;
        }
        render.top_line = self.top_line;
        render.top_line_portion = self.top_line_portion;
        render.left = self.left;
        self.pending_relative_scroll = 0;

        // Scroll the region of the screen that had and still has file lines
        if pending_refresh != Refresh::All {
            let scroll_start = 0;
            let scroll_end = min(
                file_view_height,
                self.rendered.height - self.rendered.overlay_height,
            );
            match scroll_direction {
                Direction::None => {}
                _ if scroll_distance > scroll_end - scroll_start => {
                    pending_refresh.add_range(scroll_start, scroll_end);
                }
                Direction::Up if caps.scroll_up => {
                    changes.push(Change::ScrollRegionDown {
                        first_row: scroll_start,
                        region_size: scroll_end - scroll_start,
                        scroll_count: scroll_distance,
                    });
                    pending_refresh.rotate_range_down(
                        scroll_start,
                        scroll_end,
                        scroll_distance,
                        true,
                    );
                }
                Direction::Down if caps.scroll_down => {
                    changes.push(Change::ScrollRegionUp {
                        first_row: scroll_start,
                        region_size: scroll_end - scroll_start,
                        scroll_count: scroll_distance,
                    });
                    pending_refresh.rotate_range_up(
                        scroll_start,
                        scroll_end,
                        scroll_distance,
                        true,
                    );
                }
                _ if scroll_distance > 0 => {
                    pending_refresh.add_range(scroll_start, scroll_end);
                }
                _ => {}
            }
            if file_view_height > scroll_end {
                pending_refresh.add_range(scroll_end, file_view_height);
            }
        }

        // Assign lines to the rows on screen
        {
            let mut file_line_rows = Vec::new();
            let mut row = 0;
            let mut top_portion = render.top_line_portion;
            for file_line in render.top_line..render.file_lines {
                if let Some(line) = self.line_cache.get_or_create(&self.file, file_line, None) {
                    let line_height = line.height(file_width, self.wrapping_mode);
                    let visible_line_height = min(
                        line_height.saturating_sub(top_portion),
                        file_view_height - row,
                    );
                    for offset in 0..visible_line_height {
                        row_contents[row + offset] = RowContent::FileLinePortions {
                            line: file_line,
                            first_portion: top_portion + offset,
                            rows: 1,
                        };
                    }
                    file_line_rows.push((row, row + visible_line_height));
                    row += visible_line_height;
                } else {
                    file_line_rows.push((row, row));
                }
                top_portion = 0;
                if row >= file_view_height {
                    break;
                }
            }
            render.bottom_line = render.top_line + file_line_rows.len();
            render.file_line_rows = file_line_rows;
            for blank_row in row_contents.iter_mut().take(file_view_height).skip(row) {
                *blank_row = RowContent::Blank;
            }
        }

        // Update the ruler with the new position.
        self.ruler.set_position(
            render.top_line,
            render.left,
            if !self.following_end {
                Some(render.bottom_line)
            } else {
                None
            },
            self.wrapping_mode,
        );

        // Work out what else needs to be refreshed
        if pending_refresh != Refresh::All {
            // What needs to be refreshed because more of the file was loaded?
            if !file_loaded {
                let last_line = self.rendered.file_lines.saturating_sub(1);
                if let Some((start, end)) = render.file_line_rows(last_line) {
                    pending_refresh.add_range(start, end);
                }
            }
            if render.file_lines > self.rendered.file_lines {
                let start_line = max(self.rendered.file_lines, render.top_line);
                let end_line = min(render.file_lines, render.bottom_line);
                for file_line in start_line..end_line {
                    if let Some((start, end)) = render.file_line_rows(file_line) {
                        pending_refresh.add_range(start, end);
                    }
                }
            }

            // What needs to be refreshed because search has progressed?
            if let Some(search) = self.search.as_ref() {
                if render.searched_lines > self.rendered.searched_lines {
                    let start_line = max(render.top_line, self.rendered.searched_lines);
                    let end_line = min(render.bottom_line, render.searched_lines);
                    for line in search.matching_lines(start_line, end_line).into_iter() {
                        if let Some((start_row, end_row)) = render.file_line_rows(line) {
                            pending_refresh.add_range(start_row, end_row);
                        }
                    }
                }
            }

            // What needs to be refreshed because the overlay got smaller?
            if file_view_height > self.rendered.height - self.rendered.overlay_height {
                pending_refresh.add_range(
                    self.rendered.height - self.rendered.overlay_height,
                    file_view_height,
                );
            }

            // Which parts of the error file need to be refreshed because they moved?
            let bottom_row = render.height - render.progress_height;
            if !file_loaded && self.rendered.error_file_lines > 0 {
                pending_refresh.add_range(bottom_row - 1, bottom_row);
            }
            if self.rendered.error_file_lines != render.error_file_lines
                || self.rendered.progress_height != render.progress_height
                || self.rendered.error_file_last_line_portion != render.error_file_last_line_portion
            {
                pending_refresh.add_range(bottom_row - render.error_file_height, bottom_row);
            }

            // Did the ruler move or does it need updating?
            if let Some(ruler_row) = render.ruler_row {
                if self.rendered.ruler_row != Some(ruler_row)
                    || render.top_line != self.rendered.top_line
                    || render.bottom_line != self.rendered.bottom_line
                    || render.left != self.rendered.left
                {
                    pending_refresh.add_range(ruler_row, ruler_row + 1);
                }
            }

            // Did the prompt move?
            if let Some(prompt_row) = render.prompt_row {
                if self.rendered.prompt_row != Some(prompt_row) {
                    pending_refresh.add_range(prompt_row, prompt_row + 1);
                }
            }

            // Did the error message move?
            if let Some(error_row) = render.error_row {
                if self.rendered.error_row != Some(error_row) {
                    pending_refresh.add_range(error_row, error_row + 1);
                }
            }
        }

        if self.wrapping_mode == WrappingMode::GraphemeBoundary && !self.line_numbers {
            // In wrapped mode with line numbers off, render full lines at once
            // so that the terminal can handle wrapped lines properly.
            let mut first_row: Option<(usize, &mut RowContent)> = None;
            for (row, row_content) in row_contents.iter_mut().enumerate() {
                match row_content {
                    RowContent::FileLinePortions {
                        line: this_line,
                        first_portion: this_portion,
                        rows: _,
                    } => {
                        match first_row {
                            Some((
                                first_row,
                                &mut RowContent::FileLinePortions {
                                    line,
                                    first_portion,
                                    ref mut rows,
                                },
                            )) if *this_line == line && *this_portion == first_portion + *rows => {
                                *rows += 1;
                                *row_content = RowContent::Empty;
                                if pending_refresh.contains(row) {
                                    pending_refresh.add_range(first_row, first_row + 1);
                                }
                                continue;
                            }
                            _ => {}
                        }
                        first_row = Some((row, row_content));
                    }
                    _ => {
                        first_row = None;
                    }
                }
            }
        }

        // Render pending rows
        for (row, row_content) in row_contents.into_iter().enumerate() {
            if pending_refresh.contains(row) {
                match row_content {
                    RowContent::Empty => {}
                    RowContent::FileLinePortions {
                        line,
                        first_portion,
                        rows,
                    } => {
                        self.render_file_line(
                            &mut changes,
                            row,
                            line,
                            first_portion,
                            rows,
                            render.left,
                            render.width,
                        );
                    }
                    RowContent::Blank => {
                        self.render_blank_line(&mut changes, row);
                    }
                    RowContent::Error => {
                        self.render_error(&mut changes, row, render.width);
                    }
                    RowContent::Prompt => {
                        self.prompt
                            .as_mut()
                            .expect("prompt should be visible")
                            .render(&mut changes, row, render.width);
                    }
                    RowContent::Search => {
                        if let Some(search) = self.search.as_mut() {
                            search.render(&mut changes, row, render.width);
                        }
                    }
                    RowContent::Ruler => {
                        self.ruler.bar().render(&mut changes, row, render.width);
                    }
                    RowContent::ErrorFileLinePortion(line, portion) => {
                        self.render_error_file_line(&mut changes, row, line, portion, render.width);
                    }
                    RowContent::ProgressLine(line) => {
                        self.render_progress_line(&mut changes, row, line, render.width);
                    }
                }
            }
        }

        // Set the cursor to the right position and shape.
        if let Some(prompt) = self.prompt.as_ref() {
            changes.push(Change::CursorPosition {
                x: Position::Absolute(prompt.cursor_position()),
                y: Position::Absolute(
                    render
                        .prompt_row
                        .expect("prompt row should have been calculated"),
                ),
            });
            changes.push(Change::CursorVisibility(CursorVisibility::Visible));
        } else {
            changes.push(Change::CursorPosition {
                x: Position::Absolute(0),
                y: Position::Relative(0),
            });
            if self.config.show_cursor {
                changes.push(Change::CursorVisibility(CursorVisibility::Visible));
            }
        }

        // Restore attributes to default.
        changes.push(Change::AllAttributes(CellAttributes::default()));

        // Record what we've rendered.
        self.rendered = render;
        self.pending_refresh = Refresh::None;

        changes
    }

    /// Renders a line of the file on the screen.
    fn render_file_line(
        &mut self,
        changes: &mut Vec<Change>,
        row: usize,
        line_index: usize,
        first_portion: usize,
        rows: usize,
        left: usize,
        width: usize,
    ) {
        let line = match self.search {
            Some(ref search) if search.line_matches(line_index) => self
                .search_line_cache
                .get_or_create(&self.file, line_index, Some(search.regex())),
            _ => self.line_cache.get_or_create(&self.file, line_index, None),
        };

        let match_index = self
            .search
            .as_ref()
            .and_then(|search| search.current_match())
            .and_then(|(match_line_index, match_index)| {
                if match_line_index == line_index {
                    Some(match_index)
                } else {
                    None
                }
            });

        if let Some(line) = line {
            changes.push(Change::CursorPosition {
                x: Position::Absolute(0),
                y: Position::Absolute(row),
            });
            changes.push(Change::AllAttributes(CellAttributes::default()));

            let start = left;
            let mut end = left.saturating_add(width);
            if self.line_numbers {
                let lw = number_width(self.file.lines());
                if lw + 2 < width {
                    changes.push(Change::AllAttributes(
                        CellAttributes::default()
                            .set_foreground(AnsiColor::Black)
                            .set_background(AnsiColor::Silver)
                            .clone(),
                    ));
                    if first_portion == 0 {
                        changes.push(Change::Text(format!(" {:>1$} ", line_index + 1, lw)));
                    } else {
                        changes.push(Change::Text(" ".repeat(lw + 2)));
                    };
                    changes.push(Change::AllAttributes(CellAttributes::default()));
                    end -= lw + 2;
                }
            }
            if self.wrapping_mode == WrappingMode::Unwrapped {
                line.render(changes, start, end, match_index);
            } else {
                line.render_wrapped(
                    changes,
                    first_portion,
                    rows,
                    end - start,
                    self.wrapping_mode,
                    match_index,
                );
            }
        } else {
            self.render_blank_line(changes, row);
        }
    }

    fn render_blank_line(&self, changes: &mut Vec<Change>, row: usize) {
        changes.push(Change::CursorPosition {
            x: Position::Absolute(0),
            y: Position::Absolute(row),
        });
        changes.push(Change::AllAttributes(CellAttributes::default()));
        changes.push(Change::AllAttributes(
            CellAttributes::default()
                .set_foreground(AnsiColor::Navy)
                .set_intensity(Intensity::Bold)
                .clone(),
        ));
        changes.push(Change::Text("~".into()));
        changes.push(Change::ClearToEndOfLine(ColorAttribute::default()));
    }

    fn render_error_file_line(
        &mut self,
        changes: &mut Vec<Change>,
        row: usize,
        line_index: usize,
        portion: usize,
        width: usize,
    ) {
        if let Some(error_file) = self.error_file.as_ref() {
            changes.push(Change::CursorPosition {
                x: Position::Absolute(0),
                y: Position::Absolute(row),
            });
            changes.push(Change::AllAttributes(CellAttributes::default()));
            if let Some(line) = error_file.with_line(line_index, |line| Line::new(line_index, line))
            {
                line.render_wrapped(changes, portion, 1, width, WrappingMode::WordBoundary, None);
            } else {
                changes.push(Change::ClearToEndOfLine(ColorAttribute::default()));
            }
        }
    }

    fn render_progress_line(
        &mut self,
        changes: &mut Vec<Change>,
        row: usize,
        line_index: usize,
        width: usize,
    ) {
        if let Some(progress) = self.progress.as_ref() {
            changes.push(Change::CursorPosition {
                x: Position::Absolute(0),
                y: Position::Absolute(row),
            });
            changes.push(Change::AllAttributes(CellAttributes::default()));
            if let Some(line) = progress.with_line(line_index, |line| Line::new(line_index, line)) {
                line.render(changes, 0, width, None);
            } else {
                changes.push(Change::ClearToEndOfLine(ColorAttribute::default()));
            }
        }
    }

    /// Renders the error message at the bottom of the screen.
    fn render_error(&mut self, changes: &mut Vec<Change>, row: usize, _width: usize) {
        if let Some(error) = self.error.as_ref() {
            changes.push(Change::CursorPosition {
                x: Position::Absolute(0),
                y: Position::Absolute(row),
            });
            changes.push(Change::AllAttributes(
                CellAttributes::default()
                    .set_foreground(AnsiColor::Black)
                    .set_background(AnsiColor::Maroon)
                    .clone(),
            ));
            // TODO: truncate at width
            changes.push(Change::Text(format!("  {}  ", error)));
            changes.push(Change::AllAttributes(CellAttributes::default()));
            changes.push(Change::ClearToEndOfLine(ColorAttribute::default()));
        }
    }

    /// Refreshes the ruler on the next render.
    pub(crate) fn refresh_ruler(&mut self) {
        if let Some(ruler_row) = self.rendered.ruler_row {
            self.pending_refresh.add_range(ruler_row, ruler_row + 1);
        }
    }

    /// Refreshes the search bar on the next render.
    pub(crate) fn refresh_search_status(&mut self) {
        if let Some(search_row) = self.rendered.search_row {
            self.pending_refresh.add_range(search_row, search_row + 1);
        }
    }

    /// Refreshes the prompt on the next render.
    pub(crate) fn refresh_prompt(&mut self) {
        if let Some(prompt_row) = self.rendered.prompt_row {
            self.pending_refresh.add_range(prompt_row, prompt_row + 1);
        }
    }

    /// Refreshes the overlay on the next render.
    pub(crate) fn refresh_overlay(&mut self) {
        let start = self
            .rendered
            .height
            .saturating_sub(self.rendered.overlay_height);
        let end = self.rendered.height;
        self.pending_refresh.add_range(start, end);
    }

    /// Refreshes the progress section on the next render.
    pub(crate) fn refresh_progress(&mut self) {
        let start = self
            .rendered
            .height
            .saturating_sub(self.rendered.progress_height);
        let end = self.height;
        self.pending_refresh.add_range(start, end);
    }

    /// Refresh a file line.
    pub(crate) fn refresh_file_line(&mut self, file_line_index: usize) {
        if let Some((start_row, end_row)) = self.rendered.file_line_rows(file_line_index) {
            self.pending_refresh.add_range(start_row, end_row);
        }
    }

    /// Refresh the line with the current match (if any).
    pub(crate) fn refresh_matched_line(&mut self) {
        if let Some(ref search) = self.search {
            if let Some((line_index, _match_index)) = search.current_match() {
                self.refresh_file_line(line_index);
            }
        }
    }

    /// Refresh all lines with any matches.
    pub(crate) fn refresh_matched_lines(&mut self) {
        if let Some(ref search) = self.search {
            for line in search
                .matching_lines(self.rendered.top_line, self.rendered.bottom_line)
                .into_iter()
            {
                self.refresh_file_line(line);
            }
        }
    }

    /// Triggers a full refresh on the next render.
    pub(crate) fn refresh(&mut self) {
        self.pending_refresh = Refresh::All;
    }

    /// Scrolls to the given line number.
    pub(crate) fn scroll_to(&mut self, line: usize) {
        self.pending_absolute_scroll = Some(line);
        self.pending_relative_scroll = 0;
        self.following_end = false;
    }

    /// Scroll the screen `step` characters up.
    fn scroll_up(&mut self, step: usize) {
        self.pending_relative_scroll -= step as isize;
        self.following_end = false;
    }

    /// Scroll the screen `step` characters down.
    fn scroll_down(&mut self, step: usize) {
        self.pending_relative_scroll += step as isize;
        self.following_end = false;
    }

    /// Scroll the screen `step` characters to the left.
    fn scroll_left(&mut self, step: usize) {
        if self.wrapping_mode == WrappingMode::Unwrapped && self.left > 0 && step > 0 {
            self.left = self.left.saturating_sub(step);
            self.refresh();
        }
    }

    /// Scroll the screen `step` characters to the right.
    fn scroll_right(&mut self, step: usize) {
        if self.wrapping_mode == WrappingMode::Unwrapped && step != 0 {
            self.left = self.left.saturating_add(step);
            self.refresh();
        }
    }

    /// Scroll up (screen / n) * repeat lines.
    fn scroll_up_screen_fraction(&mut self, n: usize, repeat: usize) {
        if n != 0 {
            let lines = (self.rendered.height - self.rendered.overlay_height) / n;
            self.scroll_up(lines.saturating_mul(repeat));
        }
    }

    /// Scroll down (screen / n) * repeat lines.
    fn scroll_down_screen_fraction(&mut self, n: usize, repeat: usize) {
        if n != 0 {
            let lines = (self.rendered.height - self.rendered.overlay_height) / n;
            self.scroll_down(lines.saturating_mul(repeat));
        }
    }

    /// Scroll left (screen / n) * repeat columns.
    fn scroll_left_screen_fraction(&mut self, n: usize, repeat: usize) {
        if n != 0 {
            let columns = self.rendered.width / n;
            self.scroll_left(columns.saturating_mul(repeat));
        }
    }

    /// Scroll right (screen / n) * repeat columns.
    fn scroll_right_screen_fraction(&mut self, n: usize, repeat: usize) {
        if n != 0 {
            let columns = self.rendered.width / n;
            self.scroll_right(columns.saturating_mul(repeat));
        }
    }

    /// Dispatch an action to navigate the displayed file.
    pub(crate) fn dispatch_action(
        &mut self,
        action: Action,
        event_sender: &EventSender,
    ) -> DisplayAction {
        use Action::*;
        match action {
            Quit => return DisplayAction::Quit,
            Refresh => return DisplayAction::Refresh,
            Help => return DisplayAction::ShowHelp,
            Cancel => {
                if self.repeat_count.is_some() {
                    self.clear_repeat_count();
                } else {
                    self.error_file = None;
                    self.set_search(None);
                    self.error = None;
                    self.refresh();
                    return DisplayAction::ClearOverlay;
                }
            }
            PreviousFile => return DisplayAction::PreviousFile,
            NextFile => return DisplayAction::NextFile,
            ToggleRuler => {
                self.show_ruler = !self.show_ruler;
            }
            ScrollUpLines(n) => {
                let n = self.apply_repeat_count(n);
                self.scroll_up(n)
            }
            ScrollDownLines(n) => {
                let n = self.apply_repeat_count(n);
                self.scroll_down(n)
            }
            ScrollUpScreenFraction(n) => {
                let repeat = self.apply_repeat_count(1);
                self.scroll_up_screen_fraction(n, repeat)
            }
            ScrollDownScreenFraction(n) => {
                let repeat = self.apply_repeat_count(1);
                self.scroll_down_screen_fraction(n, repeat)
            }
            ScrollToTop | ScrollToBottom if self.repeat_count.is_some() => {
                if let Some(n) = self.repeat_count {
                    // Convert 1-based to 0-based line number.
                    self.scroll_to(n.max(1) - 1);
                }
            }
            ScrollToTop => self.scroll_to(0),
            ScrollToBottom => self.following_end = true,
            ScrollLeftColumns(n) => {
                let n = self.apply_repeat_count(n);
                self.scroll_left(n)
            }
            ScrollRightColumns(n) => {
                let n = self.apply_repeat_count(n);
                self.scroll_right(n)
            }
            ScrollLeftScreenFraction(n) => {
                let repeat = self.apply_repeat_count(1);
                self.scroll_left_screen_fraction(n, repeat)
            }
            ScrollRightScreenFraction(n) => {
                let repeat = self.apply_repeat_count(1);
                self.scroll_right_screen_fraction(n, repeat)
            }
            ToggleLineNumbers => {
                self.line_numbers = !self.line_numbers;
                return DisplayAction::Refresh;
            }
            ToggleLineWrapping => {
                self.wrapping_mode = self.wrapping_mode.next_mode();
                return DisplayAction::Refresh;
            }
            PromptGoToLine => self.prompt = Some(command::goto()),
            PromptSearchFromStart => {
                self.prompt = Some(command::search(SearchKind::First, event_sender.clone()))
            }
            PromptSearchForwards => {
                self.prompt = Some(command::search(
                    SearchKind::FirstAfter(self.rendered.top_line),
                    event_sender.clone(),
                ))
            }
            PromptSearchBackwards => {
                self.prompt = Some(command::search(
                    SearchKind::FirstBefore(self.rendered.bottom_line),
                    event_sender.clone(),
                ))
            }
            PreviousMatch => self.create_or_move_match(MatchMotion::Previous, event_sender.clone()),
            NextMatch => self.create_or_move_match(MatchMotion::Next, event_sender.clone()),
            PreviousMatchLine => {
                self.create_or_move_match(MatchMotion::PreviousLine, event_sender.clone())
            }
            NextMatchLine => self.create_or_move_match(MatchMotion::NextLine, event_sender.clone()),
            PreviousMatchScreen => {
                self.create_or_move_match(MatchMotion::PreviousScreen, event_sender.clone())
            }
            NextMatchScreen => {
                self.create_or_move_match(MatchMotion::NextScreen, event_sender.clone())
            }
            FirstMatch => self.create_or_move_match(MatchMotion::First, event_sender.clone()),
            LastMatch => self.create_or_move_match(MatchMotion::Last, event_sender.clone()),
            AppendDigitToRepeatCount(n) => self.append_digit_to_repeat_count(n),
        }
        if !matches!(action, AppendDigitToRepeatCount(_)) {
            self.clear_repeat_count();
        }
        DisplayAction::Render
    }

    /// Dispatch a keypress to navigate the displayed file.
    pub(crate) fn dispatch_key(
        &mut self,
        key: KeyEvent,
        event_sender: &EventSender,
    ) -> DisplayAction {
        if let Some(binding) = Self::key_candidates(key)
            .into_iter()
            .find_map(|key| self.keymap.get(key.modifiers, key.key))
        {
            match binding {
                Binding::Action(action) => {
                    let action = action.clone();
                    return self.dispatch_action(action, event_sender);
                }
                Binding::Custom(b) => {
                    b.run(self.file.index());
                    return DisplayAction::Render;
                }
                Binding::Unrecognized(_) => {}
            }
        }
        DisplayAction::Render
    }

    fn key_candidates(key: KeyEvent) -> Vec<KeyEvent> {
        use termwiz::input::{KeyCode, Modifiers};

        let mut candidates = vec![key.clone()];
        if matches!(key.key, KeyCode::Char(_)) && key.modifiers.contains(Modifiers::SHIFT) {
            let mut modifiers = key.modifiers;
            modifiers.remove(Modifiers::SHIFT);
            candidates.push(KeyEvent {
                key: key.key,
                modifiers,
            });
        }
        candidates
    }

    /// Append a digit to the repeat count.
    pub(crate) fn append_digit_to_repeat_count(&mut self, digit: usize) {
        assert!(digit < 10);
        let new_count = match self.repeat_count {
            None if digit > 0 => Some(digit),
            None => None,
            Some(count) => Some(count.saturating_mul(10).saturating_add(digit)),
        };
        self.ruler.set_repeat_count(new_count);
        self.refresh_ruler();
        self.repeat_count = new_count;
    }

    /// Clear the repeat count.
    pub(crate) fn clear_repeat_count(&mut self) {
        self.ruler.set_repeat_count(None);
        self.refresh_ruler();
        self.repeat_count = None;
    }

    /// Multiply `n` by the repeat count.
    pub(crate) fn apply_repeat_count(&self, n: usize) -> usize {
        self.repeat_count.unwrap_or(1).saturating_mul(n)
    }

    /// Set the search for this file.
    pub(crate) fn set_search(&mut self, search: Option<Search>) {
        self.search = search;
        self.search_line_cache.clear();
    }

    /// Set the error file for this file.
    pub(crate) fn set_error_file(&mut self, error_file: Option<File>) {
        self.error_file = error_file;
    }

    /// Set the progress indicator for this file.
    pub(crate) fn set_progress(&mut self, progress: Option<Progress>) {
        self.progress = progress;
    }

    /// Returns true if this screen is currently animating for any reason.
    pub(crate) fn animate(&self) -> bool {
        self.error_file.is_some()
            || (!self.file.loaded() && !self.file.paused())
            || self.following_end
            || self
                .search
                .as_ref().is_some_and(|search| !search.finished())
    }

    /// Dispatch an animation timeout, updating for the next animation frame.
    pub(crate) fn dispatch_animation(&mut self) -> DisplayAction {
        if !self.file.loaded() {
            self.refresh_ruler();
        }
        if self
            .search
            .as_ref()
            .is_some_and(|search| !search.finished())
        {
            self.refresh_overlay();
        }
        if let Some(ref error_file) = self.error_file {
            if error_file.lines() != self.rendered.error_file_lines {
                self.refresh_overlay();
            }
        }
        match &self.pending_refresh {
            Refresh::None => DisplayAction::None,
            _ => DisplayAction::Render,
        }
    }

    pub(crate) fn prompt(&mut self) -> &mut Option<Prompt> {
        &mut self.prompt
    }

    /// Clears the prompt from the screen.
    pub(crate) fn clear_prompt(&mut self) {
        // Refresh the prompt before we remove it, so that we know which line to refresh.
        self.refresh_prompt();
        self.prompt = None;
    }

    /// Called when a search finds its first match in order to scroll to that match.
    pub(crate) fn search_first_match(&mut self) -> DisplayAction {
        let current_match = self
            .search
            .as_ref()
            .and_then(|search| search.current_match());
        if let Some((line_index, _match_index)) = current_match {
            self.scroll_to(line_index);
            self.refresh_matched_lines();
            self.refresh_overlay();
            return DisplayAction::Render;
        }
        DisplayAction::None
    }

    /// Called when a search completes.
    #[allow(clippy::unnecessary_wraps)]
    pub(crate) fn search_finished(&mut self) -> DisplayAction {
        self.refresh_matched_lines();
        self.refresh_overlay();
        DisplayAction::Render
    }

    /// Move the currently selected match to a new match.
    pub(crate) fn move_match(&mut self, motion: MatchMotion) {
        self.refresh_matched_line();
        if let Some(ref mut search) = self.search {
            let scope = self.rendered.top_line..=self.rendered.bottom_line;
            search.move_match(motion, scope);
            if let Some((line_index, _match_index)) = search.current_match() {
                self.scroll_to(line_index);
            }
            self.refresh_matched_line();
            self.refresh_search_status();
        }
    }

    /// Like `move_match`, but create a new search from history based on the
    /// last pattern on demand.
    pub(crate) fn create_or_move_match(&mut self, motion: MatchMotion, event_sender: EventSender) {
        if self.search.is_some() {
            self.move_match(motion)
        } else {
            // Attempt to load search from history.
            if let Some(pattern) = prompt_history::peek_last("search") {
                if !pattern.is_empty() {
                    let kind = match motion {
                        MatchMotion::First => SearchKind::First,
                        MatchMotion::Last => SearchKind::FirstBefore(self.file.lines()),
                        MatchMotion::Next | MatchMotion::NextLine | MatchMotion::NextScreen => {
                            SearchKind::FirstAfter(self.rendered.top_line)
                        }
                        MatchMotion::Previous
                        | MatchMotion::PreviousLine
                        | MatchMotion::PreviousScreen => {
                            SearchKind::FirstBefore(self.rendered.bottom_line)
                        }
                    };
                    if let Ok(search) = Search::new(&self.file, &pattern, kind, event_sender) {
                        self.search = Some(search);
                        self.move_match(motion)
                    }
                }
            }
        }
    }

    pub(crate) fn flush_line_caches(&mut self) {
        self.line_cache.clear();
        self.search_line_cache.clear();
    }

    /// Load more lines from a stream.
    pub(crate) fn maybe_load_more(&mut self) {
        // Fetch 1 screen + config.read_ahead_lines.
        let needed_lines = self.rendered.bottom_line + self.height + self.config.read_ahead_lines;
        self.file.set_needed_lines(needed_lines);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use termwiz::input::{KeyCode, Modifiers};

    #[test]
    fn shifted_char_tries_exact_binding_before_unshifted_fallback() {
        let key = KeyEvent {
            key: KeyCode::Char('G'),
            modifiers: Modifiers::SHIFT,
        };

        assert_eq!(
            Screen::key_candidates(key)
                .into_iter()
                .map(|key| (key.modifiers, key.key))
                .collect::<Vec<_>>(),
            vec![
                (Modifiers::SHIFT, KeyCode::Char('G')),
                (Modifiers::NONE, KeyCode::Char('G')),
            ]
        );
    }

    #[test]
    fn shifted_non_char_has_no_unshifted_fallback() {
        let key = KeyEvent {
            key: KeyCode::UpArrow,
            modifiers: Modifiers::SHIFT,
        };

        assert_eq!(Screen::key_candidates(key).len(), 1);
    }
}
