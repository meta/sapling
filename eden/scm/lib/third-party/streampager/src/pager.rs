//! The pager.

use std::ffi::OsStr;
use std::io::Read;
use std::sync::Arc;

use termwiz::caps::ColorLevel;
use termwiz::caps::{Capabilities, ProbeHints};
use termwiz::terminal::{SystemTerminal, Terminal};
use vec_map::VecMap;

use crate::action::ActionSender;
use crate::bindings::Keymap;
use crate::config::{Config, InterfaceMode, KeymapConfig, WrappingMode};
use crate::control::Controller;
use crate::error::{Error, Result};
use crate::event::EventStream;
use crate::file::{ControlledFile, File, FileIndex, FileInfo, LoadedFile};
use crate::progress::Progress;

/// The main pager state.
pub struct Pager {
    /// The Terminal.
    term: SystemTerminal,

    /// The Terminal's capabilities.
    caps: Capabilities,

    /// Event Stream to process.
    events: EventStream,

    /// Files to load.
    files: Vec<File>,

    /// Error file mapping.  Maps file indices to the associated error files.
    error_files: VecMap<File>,

    /// Progress indicators to display.
    progress: Option<Progress>,

    /// Configuration.
    config: Config,
}

/// Determine terminal capabilities.
///
/// Mouse reporting is only enabled if the user asked for it, as it takes the
/// mouse away from the terminal and so stops click-and-drag text selection
/// from working.
fn termcaps(mouse: bool) -> Result<Capabilities> {
    // Get terminal capabilities from the environment. Enable TrueColor
    // support, which is backwards compatible with 16 or 256 colors.
    // Applications can still limit themselves to 16 or 256 colors if they
    // want.
    let hints = ProbeHints::new_from_env()
        .color_level(Some(ColorLevel::TrueColor))
        .mouse_reporting(Some(mouse));
    let caps = Capabilities::new_with_hints(hints).map_err(Error::Termwiz)?;
    if cfg!(unix) && caps.terminfo_db().is_none() {
        Err(Error::TerminfoDatabaseMissing)
    } else {
        Ok(caps)
    }
}

impl Pager {
    /// Build a `Pager` using the system terminal and config read from the
    /// config file and environment variables.
    pub fn new_using_system_terminal() -> Result<Self> {
        Self::new_using_system_terminal_with_config(Config::from_user_config())
    }

    /// Build a `Pager` using the system terminal and the given config
    pub fn new_using_system_terminal_with_config(config: Config) -> Result<Self> {
        Self::new_with_terminal_func_with_config(
            move |caps| SystemTerminal::new(caps).map_err(Error::Termwiz),
            config,
        )
    }

    /// Build a `Pager` using the system stdio and config read from the config
    /// file and environment variables.
    pub fn new_using_stdio() -> Result<Self> {
        Self::new_using_stdio_with_config(Config::from_user_config())
    }

    /// Build a `Pager` using the system stdio and the given config.
    pub fn new_using_stdio_with_config(config: Config) -> Result<Self> {
        Self::new_with_terminal_func_with_config(
            move |caps| SystemTerminal::new_from_stdio(caps).map_err(Error::Termwiz),
            config,
        )
    }

    #[cfg(unix)]
    /// Build a `Pager` using the specified terminal input and output and config
    /// read from the config file and environment variables.
    pub fn new_with_input_output(
        input: &impl std::os::unix::io::AsRawFd,
        output: &impl std::os::unix::io::AsRawFd,
    ) -> Result<Self> {
        Self::new_with_input_output_with_config(input, output, Config::from_user_config())
    }

    #[cfg(unix)]
    /// Build a `Pager` using the specified terminal input and output and the
    /// given config.
    pub fn new_with_input_output_with_config(
        input: &impl std::os::unix::io::AsRawFd,
        output: &impl std::os::unix::io::AsRawFd,
        config: Config,
    ) -> Result<Self> {
        Self::new_with_terminal_func_with_config(
            move |caps| SystemTerminal::new_with(caps, input, output).map_err(Error::Termwiz),
            config,
        )
    }

    #[cfg(windows)]
    /// Build a `Pager` using the specified terminal input and output and config
    /// read from the config file and environment variables.
    pub fn new_with_input_output(
        input: impl std::io::Read + termwiz::istty::IsTty + std::os::windows::io::AsRawHandle,
        output: impl std::io::Write + termwiz::istty::IsTty + std::os::windows::io::AsRawHandle,
    ) -> Result<Self> {
        Self::new_with_input_output_with_config(input, output, Config::from_user_config())
    }

    #[cfg(windows)]
    /// Build a `Pager` using the specified terminal input and output and the given config.
    pub fn new_with_input_output_with_config(
        input: impl std::io::Read + termwiz::istty::IsTty + std::os::windows::io::AsRawHandle,
        output: impl std::io::Write + termwiz::istty::IsTty + std::os::windows::io::AsRawHandle,
        config: Config,
    ) -> Result<Self> {
        Self::new_with_terminal_func_with_config(
            move |caps| SystemTerminal::new_with(caps, input, output).map_err(Error::Termwiz),
            config,
        )
    }

    fn new_with_terminal_func_with_config(
        create_term: impl FnOnce(Capabilities) -> Result<SystemTerminal>,
        config: Config,
    ) -> Result<Self> {
        let caps = termcaps(config.mouse)?;
        let term = create_term(caps.clone())?;

        let events = EventStream::new(term.waker());
        let files = Vec::new();
        let error_files = VecMap::new();
        let progress = None;

        Ok(Self {
            term,
            caps,
            events,
            files,
            error_files,
            progress,
            config,
        })
    }

    /// Add a stream to be paged.
    pub fn add_stream(
        &mut self,
        stream: impl Read + Send + 'static,
        title: &str,
    ) -> Result<FileIndex> {
        let index = self.files.len();
        let event_sender = self.events.sender();
        let file = LoadedFile::new_streamed(index, stream, title, event_sender);
        self.files.push(file.into());
        Ok(index)
    }

    /// Attach an error stream to the previously added output stream.
    pub fn add_error_stream(
        &mut self,
        stream: impl Read + Send + 'static,
        title: &str,
    ) -> Result<FileIndex> {
        let index = self.files.len();
        let event_sender = self.events.sender();
        let file = LoadedFile::new_streamed(index, stream, title, event_sender);
        if let Some(out_file) = self.files.last() {
            self.error_files
                .insert(out_file.index(), file.clone().into());
        }
        self.files.push(file.into());
        Ok(index)
    }

    #[cfg(feature = "load_file")]
    /// Attach a file from disk.
    pub fn add_file(&mut self, filename: &OsStr) -> Result<FileIndex> {
        let index = self.files.len();
        let event_sender = self.events.sender();
        let file = LoadedFile::new_file(index, filename, event_sender)?;
        self.files.push(file.into());
        Ok(index)
    }

    /// Attach a controlled file.
    pub fn add_controlled_file(&mut self, controller: &Controller) -> Result<FileIndex> {
        let index = self.files.len();
        let event_sender = self.events.sender();
        let file = ControlledFile::new(controller, index, event_sender);
        self.files.push(file.into());
        Ok(index)
    }

    /// Attach the output and error streams from a subprocess.
    ///
    /// Returns the file index for each stream.
    pub fn add_subprocess<I, S>(
        &mut self,
        command: &OsStr,
        args: I,
        title: &str,
    ) -> Result<(FileIndex, FileIndex)>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let index = self.files.len();
        let event_sender = self.events.sender();
        let (out_file, err_file) =
            LoadedFile::new_command(index, command, args, title, event_sender)?;
        self.error_files.insert(index, err_file.clone().into());
        self.files.push(out_file.into());
        self.files.push(err_file.into());
        Ok((index, index + 1))
    }

    /// Set the progress stream.
    pub fn set_progress_stream(&mut self, stream: impl Read + Send + 'static) {
        let event_sender = self.events.sender();
        self.progress = Some(Progress::new(stream, event_sender));
    }

    /// Set when to use full screen mode. See [`InterfaceMode`] for details.
    pub fn set_interface_mode(&mut self, value: impl Into<InterfaceMode>) {
        self.config.interface_mode = value.into();
    }

    /// Set whether scrolling can past end of file.
    pub fn set_scroll_past_eof(&mut self, value: bool) {
        self.config.scroll_past_eof = value;
    }

    /// Set how many lines to read ahead.
    pub fn set_read_ahead_lines(&mut self, lines: usize) {
        self.config.read_ahead_lines = lines;
    }

    /// Set whether to poll input during start-up (delayed or direct mode).
    pub fn set_startup_poll_input(&mut self, poll_input: bool) {
        self.config.startup_poll_input = poll_input;
    }

    /// Set whether to show the ruler by default.
    pub fn set_show_ruler(&mut self, show_ruler: bool) {
        self.config.show_ruler = show_ruler;
    }

    /// Set default wrapping mode. See [`WrappingMode`] for details.
    pub fn set_wrapping_mode(&mut self, value: impl Into<WrappingMode>) {
        self.config.wrapping_mode = value.into();
    }

    /// Set keymap name.
    pub fn set_keymap_name(&mut self, keymap: impl Into<String>) {
        self.config.keymap = KeymapConfig::Name(keymap.into());
    }

    /// Set keymap.
    pub fn set_keymap(&mut self, keymap: Keymap) {
        self.config.keymap = KeymapConfig::Keymap(Arc::new(keymap));
    }

    /// Create an action sender which can be used to send `Action`s to this pager.
    pub fn action_sender(&self) -> ActionSender {
        self.events.action_sender()
    }

    /// Run Stream Pager.
    pub fn run(self) -> Result<()> {
        crate::display::start(
            self.term,
            self.caps,
            self.events,
            self.files,
            self.error_files,
            self.progress,
            self.config,
        )
    }
}
