//! Prompts for input.

use std::char;
use std::fmt::Write;

use termwiz::cell::{AttributeChange, CellAttributes};
use termwiz::color::{AnsiColor, ColorAttribute};
use termwiz::input::{KeyCode, KeyEvent};
use termwiz::surface::change::Change;
use termwiz::surface::Position;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::display::DisplayAction;
use crate::error::Error;
use crate::prompt_history::PromptHistory;
use crate::screen::Screen;
use crate::util;

type PromptRunFn = dyn FnMut(&mut Screen, &str) -> Result<DisplayAction, Error>;

/// A prompt for input from the user.
pub(crate) struct Prompt {
    /// The text of the prompt to display to the user.
    prompt: String,

    /// The current prompt history,
    history: PromptHistory,

    /// The closure to run when the user presses Return.  Will only be called once.
    run: Option<Box<PromptRunFn>>,
}

pub(crate) struct PromptState {
    /// The value the user is typing in.
    value: Vec<char>,

    /// The offset within the value that we are displaying from.
    offset: usize,

    /// The cursor position within the value.
    position: usize,
}

impl PromptState {
    pub(crate) fn new() -> PromptState {
        PromptState {
            value: Vec::new(),
            offset: 0,
            position: 0,
        }
    }

    pub(crate) fn load(data: &str) -> PromptState {
        let mut value = Vec::new();
        let mut iter = data.chars();
        while let Some(c) = iter.next() {
            if c == '\\' {
                if let Some(c) = iter.next() {
                    if c == 'x' {
                        if let (Some(c1), Some(c2)) = (iter.next(), iter.next()) {
                            let hex: String = [c1, c2].iter().collect();
                            if let Some(c) =
                                u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32)
                            {
                                value.push(c);
                            }
                        }
                    } else {
                        value.push(c);
                    }
                }
            } else {
                value.push(c);
            }
        }
        let position = value.len();
        PromptState {
            value,
            offset: 0,
            position,
        }
    }

    pub(crate) fn save(&self) -> String {
        let mut data = String::new();
        for &c in self.value.iter() {
            if c == '\\' {
                data.push_str("\\\\");
            } else if c < ' ' || c == '\x7f' {
                write!(data, "\\x{:02X}", c as u8).expect("writes to strings can't fail")
            } else {
                data.push(c);
            }
        }
        data
    }

    /// Returns the column for the cursor.
    pub(crate) fn cursor_position(&self) -> usize {
        let mut position = 0;
        for c in self.value[self.offset..self.position].iter() {
            position += render_width(*c);
        }
        position
    }

    /// Clamp the offset to values appropriate for the length of the value and
    /// the current cursor position.  Keeps at least 4 characters visible to the
    /// left and right of the value if possible.
    fn clamp_offset(&mut self, width: usize) {
        if self.offset > self.position {
            self.offset = self.position;
        }
        while self.cursor_position() < 5 && self.offset > 0 {
            self.offset -= 1;
        }
        while self.cursor_position() > width - 5 && self.offset < self.position {
            self.offset += 1;
        }
    }

    /// Renders the prompt onto the terminal.
    fn render(&mut self, changes: &mut Vec<Change>, mut position: usize, width: usize) {
        let mut start = self.offset;
        let mut end = self.offset;
        while end < self.value.len() {
            let c = self.value[end];
            if let Some(render) = special_render(self.value[end]) {
                if end > start {
                    let value: String = self.value[start..end].iter().collect();
                    changes.push(Change::Text(value));
                }
                let render = util::truncate_string(render, 0, width - position);
                position += render.width();
                changes.push(Change::Attribute(AttributeChange::Reverse(true)));
                changes.push(Change::Text(render));
                changes.push(Change::Attribute(AttributeChange::Reverse(false)));
                start = end + 1;
                // Control characters can't compose, so stop if we hit the end.
                if position >= width {
                    break;
                }
            } else {
                let w = c.width().unwrap_or(0);
                if position + w > width {
                    // This character would take us past the end, so stop.
                    break;
                }
                position += w;
            }
            end += 1;
        }
        if end > start {
            let value: String = self.value[start..end].iter().collect();
            changes.push(Change::Text(value));
        }
        if position < width {
            changes.push(Change::ClearToEndOfLine(ColorAttribute::default()));
        }
    }

    /// Insert a character at the current position.
    fn insert_char(&mut self, c: char, width: usize) -> DisplayAction {
        self.value.insert(self.position, c);
        self.position += 1;
        if self.position == self.value.len() && self.cursor_position() < width - 5 {
            DisplayAction::Change(Change::Text(c.to_string()))
        } else {
            DisplayAction::RefreshPrompt
        }
    }

    fn insert_str(&mut self, s: &str) -> DisplayAction {
        let old_len = self.value.len();
        self.value.splice(self.position..self.position, s.chars());
        self.position += self.value.len() - old_len;
        DisplayAction::RefreshPrompt
    }

    /// Delete previous character.
    fn delete_prev_char(&mut self) -> DisplayAction {
        if self.position > 0 {
            self.value.remove(self.position - 1);
            self.position -= 1;
            DisplayAction::RefreshPrompt
        } else {
            DisplayAction::None
        }
    }

    /// Delete next character.
    fn delete_next_char(&mut self) -> DisplayAction {
        if self.position < self.value.len() {
            self.value.remove(self.position);
            DisplayAction::RefreshPrompt
        } else {
            DisplayAction::None
        }
    }

    /// Delete previous word.
    fn delete_prev_word(&mut self) -> DisplayAction {
        let dest = move_word_backwards(self.value.as_slice(), self.position);
        if dest != self.position {
            self.value.splice(dest..self.position, None);
            self.position = dest;
            DisplayAction::RefreshPrompt
        } else {
            DisplayAction::None
        }
    }

    /// Delete next word.
    fn delete_next_word(&mut self) -> DisplayAction {
        let dest = move_word_forwards(self.value.as_slice(), self.position);
        if dest != self.position {
            self.value.splice(self.position..dest, None);
            DisplayAction::RefreshPrompt
        } else {
            DisplayAction::None
        }
    }

    /// Move right one character.
    fn move_next_char(&mut self) -> DisplayAction {
        if self.position < self.value.len() {
            self.position += 1;
            while self.position < self.value.len() {
                let w = render_width(self.value[self.position]);
                if w != 0 {
                    break;
                }
                self.position += 1;
            }
            DisplayAction::RefreshPrompt
        } else {
            DisplayAction::None
        }
    }

    /// Move left one character.
    fn move_prev_char(&mut self) -> DisplayAction {
        if self.position > 0 {
            while self.position > 0 {
                self.position -= 1;
                let w = render_width(self.value[self.position]);
                if w != 0 {
                    break;
                }
            }
            DisplayAction::RefreshPrompt
        } else {
            DisplayAction::None
        }
    }

    /// Move right one word.
    fn move_next_word(&mut self) -> DisplayAction {
        let dest = move_word_forwards(self.value.as_slice(), self.position);
        if dest != self.position {
            self.position = dest;
            DisplayAction::RefreshPrompt
        } else {
            DisplayAction::None
        }
    }

    /// Move left one word.
    fn move_prev_word(&mut self) -> DisplayAction {
        let dest = move_word_backwards(self.value.as_slice(), self.position);
        if dest != self.position {
            self.position = dest;
            DisplayAction::RefreshPrompt
        } else {
            DisplayAction::None
        }
    }

    /// Delete to end of line.
    fn delete_to_end(&mut self) -> DisplayAction {
        if self.position < self.value.len() {
            self.value.splice(self.position.., None);
            DisplayAction::RefreshPrompt
        } else {
            DisplayAction::None
        }
    }

    /// Delete to start of line.
    fn delete_to_start(&mut self) -> DisplayAction {
        if self.position > 0 {
            self.value.splice(..self.position, None);
            self.position = 0;
            DisplayAction::RefreshPrompt
        } else {
            DisplayAction::None
        }
    }

    /// Move to end of line.
    fn move_to_end(&mut self) -> DisplayAction {
        self.position = self.value.len();
        DisplayAction::RefreshPrompt
    }

    /// Move to beginning of line.
    fn move_to_start(&mut self) -> DisplayAction {
        self.position = 0;
        DisplayAction::RefreshPrompt
    }

    /// Transpose characters.
    fn transpose_chars(&mut self) -> DisplayAction {
        if self.position > 0 && self.value.len() > 1 {
            if self.position < self.value.len() {
                self.position += 1;
            }
            self.value.swap(self.position - 2, self.position - 1);
            DisplayAction::RefreshPrompt
        } else {
            DisplayAction::None
        }
    }
}

impl Prompt {
    /// Create a new prompt.
    pub(crate) fn new(ident: impl Into<String>, prompt: &str, run: Box<PromptRunFn>) -> Prompt {
        Prompt {
            prompt: prompt.to_string(),
            history: PromptHistory::open(ident),
            run: Some(run),
        }
    }

    fn state(&self) -> &PromptState {
        self.history.state()
    }

    fn state_mut(&mut self) -> &mut PromptState {
        self.history.state_mut()
    }

    /// Returns the column for the cursor.
    pub(crate) fn cursor_position(&self) -> usize {
        self.prompt.width() + 4 + self.state().cursor_position()
    }

    /// Renders the prompt onto the terminal.
    pub(crate) fn render(&mut self, changes: &mut Vec<Change>, row: usize, width: usize) {
        changes.push(Change::CursorPosition {
            x: Position::Absolute(0),
            y: Position::Absolute(row),
        });
        changes.push(Change::AllAttributes(
            CellAttributes::default()
                .set_foreground(AnsiColor::Black)
                .set_background(AnsiColor::Silver)
                .clone(),
        ));
        changes.push(Change::Text(format!("  {} ", self.prompt)));
        changes.push(Change::AllAttributes(CellAttributes::default()));
        changes.push(Change::Text(" ".into()));
        let offset = self.prompt.width() + 4;
        self.state_mut().render(changes, offset, width);
    }

    /// Dispatch a key press to the prompt.
    pub(crate) fn dispatch_key(&mut self, key: KeyEvent, width: usize) -> DisplayAction {
        use termwiz::input::{KeyCode::*, Modifiers};
        const CTRL: Modifiers = Modifiers::CTRL;
        const NONE: Modifiers = Modifiers::NONE;
        const ALT: Modifiers = Modifiers::ALT;
        // Strip SHIFT from Char keys (modifyOtherKeys=2 sends shifted char + SHIFT).
        let mut key = key;
        if key.modifiers.contains(Modifiers::SHIFT) {
            if let KeyCode::Char(_) = key.key {
                key.modifiers.remove(Modifiers::SHIFT);
            }
        }
        let value_width = width - self.prompt.width() - 4;
        let action = match (key.modifiers, key.key) {
            (NONE, Enter) | (CTRL, Char('j')) | (CTRL, Char('m')) => {
                // Finish.
                let _ = self.history.save();
                let mut run = self.run.take();
                let value: String = self.state().value[..].iter().collect();
                return DisplayAction::Run(Box::new(move |screen: &mut Screen| {
                    screen.clear_prompt();
                    if let Some(ref mut run) = run {
                        run(screen, &value)
                    } else {
                        Ok(DisplayAction::Render)
                    }
                }));
            }
            (NONE, Escape) | (CTRL, Char('c')) => {
                // Cancel.
                return DisplayAction::Run(Box::new(|screen: &mut Screen| {
                    screen.clear_prompt();
                    Ok(DisplayAction::Render)
                }));
            }
            (NONE, Char(c)) => self.state_mut().insert_char(c, value_width),
            (NONE, Backspace) | (CTRL, Char('h')) => self.state_mut().delete_prev_char(),
            (NONE, Delete) | (CTRL, Char('d')) => self.state_mut().delete_next_char(),
            (CTRL, Char('w')) | (ALT, Backspace) => self.state_mut().delete_prev_word(),
            (ALT, Char('d')) => self.state_mut().delete_next_word(),
            (NONE, RightArrow) | (CTRL, Char('f')) => self.state_mut().move_next_char(),
            (NONE, LeftArrow) | (CTRL, Char('b')) => self.state_mut().move_prev_char(),
            (CTRL, RightArrow) | (ALT, Char('f')) => self.state_mut().move_next_word(),
            (CTRL, LeftArrow) | (ALT, Char('b')) => self.state_mut().move_prev_word(),
            (CTRL, Char('k')) => self.state_mut().delete_to_end(),
            (CTRL, Char('u')) => self.state_mut().delete_to_start(),
            (NONE, End) | (CTRL, Char('e')) => self.state_mut().move_to_end(),
            (NONE, Home) | (CTRL, Char('a')) => self.state_mut().move_to_start(),
            (CTRL, Char('t')) => self.state_mut().transpose_chars(),
            (NONE, UpArrow) => self.history.previous(),
            (NONE, DownArrow) => self.history.next(),
            _ => return DisplayAction::None,
        };
        self.state_mut().clamp_offset(value_width);
        action
    }

    /// Paste some text into the prompt.
    pub(crate) fn paste(&mut self, text: &str, width: usize) -> DisplayAction {
        let value_width = width - self.prompt.width() - 4;
        let action = self.state_mut().insert_str(text);
        self.state_mut().clamp_offset(value_width);
        action
    }
}

fn move_word_forwards(value: &[char], mut position: usize) -> usize {
    let len = value.len();
    while position < len && value[position].is_whitespace() {
        position += 1;
    }
    while position < len && !value[position].is_whitespace() {
        position += 1;
    }
    position
}

fn move_word_backwards(value: &[char], mut position: usize) -> usize {
    while position > 0 {
        position -= 1;
        if !value[position].is_whitespace() {
            break;
        }
    }
    while position > 0 {
        if value[position].is_whitespace() {
            position += 1;
            break;
        }
        position -= 1;
    }
    position
}

/// Determine the rendering width for a character.
fn render_width(c: char) -> usize {
    if c < ' ' || c == '\x7F' {
        // Render as <XX>
        4
    } else {
        c.width().unwrap_or(8)

    }
}

/// Determine the special rendering for a character, if any.
fn special_render(c: char) -> Option<String> {
    if c < ' ' || c == '\x7F' {
        Some(format!("<{:02X}>", c as u8))
    } else if c.width().is_none() {
        Some(format!("<U+{:04X}>", c as u32))
    } else {
        None
    }
}
