//! Manage the Display.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use scopeguard::guard;
use termwiz::caps::Capabilities as TermCapabilities;
use termwiz::cell::CellAttributes;
use termwiz::color::ColorAttribute;
use termwiz::input::{InputEvent, MouseButtons};
use termwiz::surface::change::Change;
use termwiz::surface::{CursorVisibility, Position};
use termwiz::terminal::Terminal;
use vec_map::VecMap;

use crate::action::Action;
use crate::command;
use crate::config::Config;
use crate::direct;
use crate::error::Error;
use crate::event::{Event, EventStream, UniqueInstance};
use crate::file::{File, FileIndex, FileInfo, LoadedFile};
use crate::help::help_text;
use crate::progress::Progress;
use crate::screen::Screen;
use crate::search::SearchKind;

/// How many lines to scroll for each click of the mouse wheel.
const WHEEL_SCROLL_LINES: usize = 3;

/// Capabilities of the terminal that we care about.
#[derive(Default)]
pub(crate) struct Capabilities {
    pub(crate) scroll_up: bool,
    pub(crate) scroll_down: bool,
}

impl Capabilities {
    fn new(term_caps: TermCapabilities) -> Capabilities {
        use terminfo::capability as cap;
        let mut caps = Capabilities::default();
        if let Some(db) = term_caps.terminfo_db() {
            if db.get::<cap::ChangeScrollRegion>().is_some() {
                caps.scroll_up = db.get::<cap::ParmIndex>().is_some()
                    || (db.get::<cap::CursorAddress>().is_some()
                        && db.get::<cap::ScrollForward>().is_some());
                caps.scroll_down = db.get::<cap::ParmRindex>().is_some()
                    || (db.get::<cap::CursorAddress>().is_some()
                        && db.get::<cap::ScrollReverse>().is_some());
            }
        }
        caps
    }
}

/// An action that affects the display.
pub(crate) enum DisplayAction {
    /// Do nothing.
    None,

    /// Run a function.  The function may return a new action to run next.
    Run(Box<dyn FnMut(&mut Screen) -> Result<DisplayAction, Error>>),

    /// Change the terminal.
    Change(Change),

    /// Render the parts of the screen that have changed.
    Render,

    /// Render the whole screen.
    Refresh,

    /// Render the prompt.
    RefreshPrompt,

    /// Move to the next file.
    NextFile,

    /// Move to the previous file.
    PreviousFile,

    /// Show the help screen.
    ShowHelp,

    /// Clear the overlay.
    ClearOverlay,

    /// Close the program.
    Quit,
}

/// Container for all screens.
struct Screens {
    /// The loaded files.
    screens: Vec<Screen>,

    /// An overlaid screen (e.g. the help screen).
    overlay: Option<Screen>,

    /// The currently active screen.
    current_index: FileIndex,

    /// The file index of the overlay.  While overlays aren't part of the
    /// screens vector, we still need a file index so that the file loader can
    /// report loading completion and the search thread can report search
    /// matches.  Use an index starting after the loaded files for this purpose.
    /// Each time a new overlay is added, this index is incremented, so that
    /// each overlay gets a unique index.
    overlay_index: FileIndex,
}

impl Screens {
    /// Create a new screens container for the given files.
    fn new(
        files: Vec<File>,
        mut error_files: VecMap<File>,
        progress: Option<Progress>,
        config: Arc<Config>,
    ) -> Result<Screens, Error> {
        let count = files.len();
        let mut screens = Vec::new();
        for file in files.into_iter() {
            let index = file.index();
            let mut screen = Screen::new(file, config.clone())?;
            screen.set_progress(progress.clone());
            screen.set_error_file(error_files.remove(index));
            screens.push(screen);
        }
        Ok(Screens {
            screens,
            overlay: None,
            current_index: 0,
            overlay_index: count,
        })
    }

    /// Get the current screen.
    fn current(&mut self) -> &mut Screen {
        if let Some(ref mut screen) = self.overlay {
            screen
        } else {
            &mut self.screens[self.current_index]
        }
    }

    /// True if the given index is the index of the currently visible screen.
    fn is_current_index(&self, index: FileIndex) -> bool {
        match self.overlay {
            Some(_) => index == self.overlay_index,
            None => index == self.current_index,
        }
    }

    /// Get the screen with the given index.
    fn get(&mut self, index: usize) -> Option<&mut Screen> {
        if index == self.overlay_index {
            self.overlay.as_mut()
        } else if index < self.screens.len() {
            Some(&mut self.screens[index])
        } else {
            None
        }
    }
}

/// Start displaying files.
pub(crate) fn start(
    mut term: impl Terminal,
    term_caps: TermCapabilities,
    mut events: EventStream,
    files: Vec<File>,
    error_files: VecMap<File>,
    progress: Option<Progress>,
    config: Config,
) -> Result<(), Error> {
    // Defer enabling raw mode until we need it. It has some undesirable side
    // effects for direct mode such as disabling terminal echo and consuming all
    // pending terminal input (e.g. user types next terminal command before the
    // current command has finished).
    let mut in_raw_mode = false;
    if config.startup_poll_input {
        // We need raw mode to poll for user input during direct mode.
        term.set_raw_mode().map_err(Error::Termwiz)?;
        in_raw_mode = true;
    }

    let outcome = {
        // Only take the first output and error. This emulates the behavior that
        // the main pager can only display one stream at a time.
        let output_files = &files[0..1.min(files.len())];
        let error_files = match error_files.iter().next() {
            None => Vec::new(),
            Some((_i, file)) => vec![file.clone()],
        };
        direct::direct(
            &mut term,
            output_files,
            &error_files[..],
            progress.as_ref(),
            &mut events,
            config.interface_mode,
            config.startup_poll_input,
        )?
    };
    match outcome {
        direct::Outcome::RenderComplete | direct::Outcome::Interrupted => return Ok(()),
        direct::Outcome::RenderIncomplete(rows) => {
            // Push the rendered output up to the top of the screen, so that
            // when we start rendering full screen we don't overwrite output
            // from earlier commands.  In direct mode the bottom line held the
            // cursor, so we must subtract that line, too, otherwise we will
            // scroll up too far.
            let size = term.get_screen_size().map_err(Error::Termwiz)?;
            let scroll_count = size.rows.saturating_sub(rows).saturating_sub(1);
            if scroll_count > 0 {
                term.render(&[Change::Text("\n".repeat(scroll_count))])
                    .map_err(Error::Termwiz)?;
            }
        }
        direct::Outcome::RenderNothing => term.enter_alternate_screen().map_err(Error::Termwiz)?,
    };

    // We certainly need raw mode for fullscreen.
    if !in_raw_mode {
        term.set_raw_mode().map_err(Error::Termwiz)?;
    }

    let overlay_height = AtomicUsize::new(0);
    let mut term = guard(term, |mut term| {
        // Clean up when exiting.  Most of this should be achieved by exiting
        // the alternate screen, but just in case it isn't, move to the
        // bottom of the screen and reset all attributes.
        //
        // Errors are ignored: the terminal may already be gone (for example
        // after a hangup), and a panic here aborts the process because the
        // terminal's own Drop panics on the same dead terminal while
        // unwinding.
        let Ok(size) = term.get_screen_size() else {
            return;
        };
        let overlay_height = overlay_height.load(Ordering::SeqCst);
        let scroll_count = 1usize.saturating_sub(overlay_height);
        let _ = term.render(&[
            Change::CursorVisibility(CursorVisibility::Visible),
            Change::AllAttributes(CellAttributes::default()),
            Change::ScrollRegionUp {
                first_row: 0,
                region_size: size.rows,
                scroll_count,
            },
            Change::CursorPosition {
                x: Position::Absolute(0),
                y: Position::Absolute(size.rows.saturating_sub(overlay_height + scroll_count)),
            },
            Change::ClearToEndOfScreen(ColorAttribute::default()),
        ]);
    });
    let config = Arc::new(config);
    let caps = Capabilities::new(term_caps);
    let mut screens = Screens::new(files, error_files, progress, config.clone())?;
    let event_sender = events.sender();
    let render_unique = UniqueInstance::new();
    let refresh_unique = UniqueInstance::new();
    {
        let screen = screens.current();
        let size = term.get_screen_size().map_err(Error::Termwiz)?;
        screen.resize(size.cols, size.rows);
        screen.maybe_load_more();
        term.render(&screen.render(&caps)).map_err(Error::Termwiz)?;
    }
    loop {
        // Listen for an event or input.  If we are animating, put a timeout on the wait.
        let timeout = if screens.current().animate() {
            Some(Duration::from_millis(100))
        } else {
            None
        };
        let event = events.get(&mut *term, timeout)?;

        // Dispatch the event and receive an action to take.
        let mut action = {
            let screen = screens.current();
            screen.maybe_load_more();

            match event {
                None => screen.dispatch_animation(),
                Some(Event::Render) => {
                    term.render(&screen.render(&caps)).map_err(Error::Termwiz)?;
                    DisplayAction::None
                }
                Some(Event::Input(InputEvent::Resized { .. })) => {
                    let size = term.get_screen_size().map_err(Error::Termwiz)?;
                    screen.resize(size.cols, size.rows);
                    term.render(&screen.render(&caps)).map_err(Error::Termwiz)?;
                    DisplayAction::None
                }
                Some(Event::Refresh) => {
                    let size = term.get_screen_size().map_err(Error::Termwiz)?;
                    screen.resize(size.cols, size.rows);
                    screen.refresh();
                    term.render(&screen.render(&caps)).map_err(Error::Termwiz)?;
                    DisplayAction::None
                }
                Some(Event::Progress) => {
                    screen.refresh_progress();
                    term.render(&screen.render(&caps)).map_err(Error::Termwiz)?;
                    DisplayAction::None
                }
                Some(Event::Action(action)) => screen.dispatch_action(action, &event_sender),
                Some(Event::Input(InputEvent::Key(key))) => {
                    let width = screen.width();
                    if let Some(prompt) = screen.prompt() {
                        prompt.dispatch_key(key, width)
                    } else {
                        screen.dispatch_key(key, &event_sender)
                    }
                }
                Some(Event::Input(InputEvent::Paste(ref text))) => {
                    let width = screen.width();
                    screen
                        .prompt()
                        .get_or_insert_with(|| {
                            // Assume the user wanted to search for what they're pasting.
                            command::search(SearchKind::First, event_sender.clone())
                        })
                        .paste(text, width)
                }
                // Only the vertical wheel scrolls; other mouse events are
                // ignored. Check the config here rather than relying on mouse
                // events not arriving, since Windows always puts the console in
                // mouse-input mode.
                Some(Event::Input(InputEvent::Mouse(ref mouse)))
                    if config.mouse
                        && mouse.mouse_buttons.contains(MouseButtons::VERT_WHEEL) =>
                {
                    let action = if mouse.mouse_buttons.contains(MouseButtons::WHEEL_POSITIVE) {
                        Action::ScrollUpLines(WHEEL_SCROLL_LINES)
                    } else {
                        Action::ScrollDownLines(WHEEL_SCROLL_LINES)
                    };
                    screen.dispatch_action(action, &event_sender)
                }
                Some(Event::Loaded(index)) if screens.is_current_index(index) => {
                    DisplayAction::Refresh
                }
                #[cfg(feature = "load_file")]
                Some(Event::Appending(index)) if screens.is_current_index(index) => {
                    DisplayAction::Refresh
                }
                Some(Event::Reloading(index)) => {
                    if let Some(screen) = screens.get(index) {
                        screen.flush_line_caches();
                    }
                    if screens.is_current_index(index) {
                        DisplayAction::Refresh
                    } else {
                        DisplayAction::None
                    }
                }
                Some(Event::SearchFirstMatch(index)) => {
                    if let Some(screen) = screens.get(index) {
                        screen.search_first_match()
                    } else {
                        DisplayAction::None
                    }
                }
                Some(Event::SearchFinished(index)) => {
                    if let Some(screen) = screens.get(index) {
                        screen.search_finished()
                    } else {
                        DisplayAction::None
                    }
                }
                _ => DisplayAction::None,
            }
        };

        // Process the action.  We may get new actions in return from the action.
        loop {
            match std::mem::replace(&mut action, DisplayAction::None) {
                DisplayAction::None => break,
                DisplayAction::Run(mut f) => action = f(screens.current())?,
                DisplayAction::Change(c) => {
                    term.render(&[c]).map_err(Error::Termwiz)?;
                }
                DisplayAction::Render => event_sender.send_unique(Event::Render, &render_unique)?,
                DisplayAction::Refresh => {
                    event_sender.send_unique(Event::Refresh, &refresh_unique)?
                }
                DisplayAction::RefreshPrompt => {
                    screens.current().refresh_prompt();
                    event_sender.send_unique(Event::Render, &render_unique)?;
                }
                DisplayAction::NextFile => {
                    screens.overlay = None;
                    if screens.current_index < screens.screens.len() - 1 {
                        screens.current_index += 1;
                        let screen = screens.current();
                        let size = term.get_screen_size().map_err(Error::Termwiz)?;
                        screen.resize(size.cols, size.rows);
                        screen.refresh();
                        term.render(&screen.render(&caps)).map_err(Error::Termwiz)?;
                    }
                }
                DisplayAction::PreviousFile => {
                    screens.overlay = None;
                    if screens.current_index > 0 {
                        screens.current_index -= 1;
                        let screen = screens.current();
                        let size = term.get_screen_size().map_err(Error::Termwiz)?;
                        screen.resize(size.cols, size.rows);
                        screen.refresh();
                        term.render(&screen.render(&caps)).map_err(Error::Termwiz)?;
                    }
                }
                DisplayAction::ShowHelp => {
                    let overlay_index = screens.overlay_index + 1;
                    let screen = screens.current();
                    let mut screen = Screen::new(
                        LoadedFile::new_static(
                            overlay_index,
                            "HELP",
                            help_text(screen.keymap())?.into_bytes(),
                            event_sender.clone(),
                        )
                        .into(),
                        config.clone(),
                    )?;
                    let size = term.get_screen_size().map_err(Error::Termwiz)?;
                    screen.resize(size.cols, size.rows);
                    screen.refresh();
                    term.render(&screen.render(&caps)).map_err(Error::Termwiz)?;
                    screens.overlay = Some(screen);
                    screens.overlay_index = overlay_index;
                }
                DisplayAction::ClearOverlay => {
                    screens.overlay = None;
                    let screen = screens.current();
                    let size = term.get_screen_size().map_err(Error::Termwiz)?;
                    screen.resize(size.cols, size.rows);
                    screen.refresh();
                    term.render(&screen.render(&caps)).map_err(Error::Termwiz)?;
                }
                DisplayAction::Quit => {
                    let screen = screens.current();
                    overlay_height.store(screen.overlay_height(), Ordering::SeqCst);
                    return Ok(());
                }
            }
        }
    }
}
