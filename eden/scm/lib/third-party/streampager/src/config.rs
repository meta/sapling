//! Configuration that affects Pager behaviors.

use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;

use crate::bindings::Keymap;
use crate::error::Result;

/// Specify what interface to use.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(from = "&str")]
pub enum InterfaceMode {
    /// The full screen terminal interface.
    ///
    /// Support text search and other operations.
    ///
    /// Use the alternate screen. The pager UI will disappear completely at
    /// exit (except for terminals without alternate screen support).
    ///
    /// Similar to external command `less` without flags. This is the default.
    FullScreen,

    /// The minimal interface. Output goes to the terminal directly.
    ///
    /// Does not support text search or other fancy operations.
    ///
    /// Does not use the alternate screen. Content will be kept in the terminal
    /// at exit.
    ///
    /// Error messages and progress messages are printed after
    /// outputs.
    ///
    /// Similar to shell command `cat` without buffering.
    Direct,

    /// Hybrid: `Direct` first, `FullScreen` next.
    ///
    /// `Direct` is used initially. When content exceeds one screen, switch to the
    /// `FullScreen` interface.
    ///
    /// Unlike `FullScreen` or `Delayed`, skip initializing the alternate
    /// screen. This is because the initial `Direct` might have "polluted"
    /// the terminal.
    ///
    /// Similar to external command `less -F -X`.
    Hybrid,

    /// Wait to decide.
    ///
    /// If output completes in the delayed time, and is within one screen, print
    /// the output and exit. Otherwise, enter the `FullScreen` interface.
    ///
    /// Unlike `Hybrid`, output is buffered in memory. So the terminal is not
    /// "polluted" and the alternate screen is used for the `FullScreen`
    /// interface.
    ///
    /// If duration is set to infinite, similar to external command `less -F`.
    /// If duration is set to 0, similar to `FullScreen`.
    Delayed(Duration),
}

impl Default for InterfaceMode {
    fn default() -> Self {
        Self::FullScreen
    }
}

impl From<&str> for InterfaceMode {
    fn from(value: &str) -> InterfaceMode {
        match value.to_lowercase().as_ref() {
            "full" | "fullscreen" | "" => InterfaceMode::FullScreen,
            "direct" => InterfaceMode::Direct,
            "hybrid" => InterfaceMode::Hybrid,
            s if s.starts_with("delayed") => {
                let duration = s.rsplit(':').next().unwrap_or("inf");
                let duration = if duration.ends_with("ms") {
                    // ex. delayed:100ms
                    Duration::from_millis(duration.trim_end_matches("ms").parse().unwrap_or(0))
                } else {
                    // ex. delayed:1s, delayed:1, delayed
                    Duration::from_secs(duration.trim_end_matches('s').parse().unwrap_or(1 << 30))
                };
                InterfaceMode::Delayed(duration)
            }
            _ => InterfaceMode::default(),
        }
    }
}

/// Specify the default line wrapping mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
pub enum WrappingMode {
    /// Lines are not wrapped.
    #[serde(rename = "none")]
    Unwrapped,
    /// Lines are wrapped on grapheme boundaries.
    #[serde(rename = "line")]
    GraphemeBoundary,
    /// Lines are wrapped on word boundaries.
    #[serde(rename = "word")]
    WordBoundary,
}

impl WrappingMode {
    pub(crate) fn next_mode(self) -> WrappingMode {
        match self {
            WrappingMode::Unwrapped => WrappingMode::GraphemeBoundary,
            WrappingMode::GraphemeBoundary => WrappingMode::WordBoundary,
            WrappingMode::WordBoundary => WrappingMode::Unwrapped,
        }
    }
}

impl Default for WrappingMode {
    fn default() -> Self {
        Self::Unwrapped
    }
}

/// Keymap Configuration
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(from = "&str")]
pub enum KeymapConfig {
    /// A keymap name to be loaded.
    Name(String),

    /// An already-loaded keymap.
    Keymap(Arc<Keymap>),
}

impl KeymapConfig {
    pub(crate) fn load(&self) -> Result<Arc<Keymap>> {
        match self {
            Self::Name(name) => Ok(Arc::new(crate::keymaps::load(name)?)),
            Self::Keymap(keymap) => Ok(keymap.clone()),
        }
    }
}

impl Default for KeymapConfig {
    fn default() -> Self {
        Self::Name(String::from("default"))
    }
}

impl From<&str> for KeymapConfig {
    fn from(value: &str) -> Self {
        Self::Name(String::from(value))
    }
}

/// A group of configurations.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Specify when to use fullscreen.
    pub interface_mode: InterfaceMode,

    /// Specify whether scrolling down can past end of file.
    pub scroll_past_eof: bool,

    /// Specify how many lines to read ahead.
    pub read_ahead_lines: usize,

    /// Specify whether to poll input during start-up (delayed or direct mode).
    pub startup_poll_input: bool,

    /// Specify whether to show the ruler by default.
    pub show_ruler: bool,

    /// Specify whether to show the cursor by default.
    pub show_cursor: bool,

    /// Specify default wrapping move.
    pub wrapping_mode: WrappingMode,

    /// Specify whether to handle the mouse, so the scroll wheel scrolls.
    pub mouse: bool,

    /// Specify the name of the default key map.
    pub keymap: KeymapConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            interface_mode: Default::default(),
            scroll_past_eof: true,
            read_ahead_lines: crate::file::DEFAULT_NEEDED_LINES,
            startup_poll_input: false,
            show_ruler: true,
            // See issue #52. With cursor hidden, scrolling is flaky in VSCode terminal.
            show_cursor: std::env::var("TERM_PROGRAM").ok().as_deref() == Some("vscode"),
            wrapping_mode: Default::default(),
            mouse: false,
            keymap: Default::default(),
        }
    }
}

impl Config {
    /// Create [`Config`] from the user's default config file.
    pub fn from_config_file() -> Self {
        #[cfg(feature = "toml_config")]
        if let Some(mut path) = dirs::config_dir() {
            path.push("streampager");
            path.push("streampager.toml");
            if let Ok(config) = std::fs::read_to_string(&path) {
                match toml::from_str(&config) {
                    Ok(config) => return config,
                    Err(e) => eprintln!(
                        "streampager: failed to parse config at {:?}, using defaults: {}",
                        path, e
                    ),
                }
            }
        }
        Self::default()
    }

    /// Modify [`Config`] using environment variables.
    pub fn with_env(mut self) -> Self {
        use std::env::var;
        if let Ok(s) = var("SP_INTERFACE_MODE") {
            self.interface_mode = InterfaceMode::from(s.as_ref());
        }
        if let Ok(s) = var("SP_SCROLL_PAST_EOF") {
            if let Some(b) = parse_bool(&s) {
                self.scroll_past_eof = b;
            }
        }
        if let Ok(s) = var("SP_READ_AHEAD_LINES") {
            if let Ok(n) = s.parse::<usize>() {
                self.read_ahead_lines = n;
            }
        }
        if let Ok(s) = var("SP_MOUSE") {
            if let Some(b) = parse_bool(&s) {
                self.mouse = b;
            }
        }
        self
    }

    pub(crate) fn from_user_config() -> Self {
        Self::from_config_file().with_env()
    }
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_ref() {
        "1" | "yes" | "true" | "on" | "always" => Some(true),
        "0" | "no" | "false" | "off" | "never" => Some(false),
        _ => None,
    }
}
