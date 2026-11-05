/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::any::Any;
use std::collections::HashMap;
use std::io;
use std::mem;
use std::ops::Deref;
use std::ops::DerefMut;
use std::process::Command;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::Weak;
use std::thread::spawn;
use std::time::Duration;
use std::time::Instant;

pub use buf::BufIO;
use configmodel::Config;
use configmodel::ConfigExt;
use parking_lot::Mutex;
use parking_lot::MutexGuard;
use parking_lot::RwLock;
use pipe::PipeWriter;
use pipe::pipe;
use spawn_ext::CommandExt;
use streampager::Pager;
use streampager::action::Action;
use streampager::config::Config as PagerConfig;
use streampager::config::InterfaceMode;
use streampager::config::WrappingMode;
use term::DEFAULT_TERM_HEIGHT;
use term::DEFAULT_TERM_WIDTH;
use term::DumbTerm;
use term::DumbTty;
use term::Term;
pub use term::enable_compat_mode;
use term::make_real_term;
use termwiz::surface::Change;
use termwiz::surface::change::ChangeSequence;
pub use time_interval;
use time_interval::BlockedInterval;
use time_interval::TimeInterval;

mod buf;
mod impls;
mod term;

use crate::impls::WriterWithTty;

// IO is Clone, but care must be taken to drop the IO object normally
// to ensure things are cleaned up before the process exits.
#[derive(Clone)]
pub struct IO {
    inner: Arc<Inner>,
}

/// Implements `io::Read` on the input stream.
#[derive(Clone)]
pub struct IOInput(Weak<Inner>);

/// Implements `io::Write` on the output stream.
#[derive(Clone)]
pub struct IOOutput(Weak<Inner>);

/// Implements `io::Write` on the error stream.
#[derive(Clone)]
pub struct IOError(Weak<Inner>);

/// Provides a way to set progress, without requiring the `&IO` reference.
#[derive(Clone)]
pub struct IOProgress(Weak<Inner>);

struct Inner {
    io_state: Mutex<IOState>,
    // Track "blocked" time intervals.
    time_interval: TimeInterval,
    // Use a separate Mutex for quitting the pager without blocking.
    //
    // Note: wait_pager is in io_state so quit_pager during wait_pager
    // won't block.
    pager_quit_func: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

struct IOState {
    input: Box<dyn Read>,
    output: Box<dyn Write>,
    error: Option<Box<dyn Write>>,
    pager_progress: Option<Box<dyn Term + Send + Sync>>,
    // (output, error) before pager was activated
    // error is None if pager isn't handling the error stream
    pre_pager_io: Option<(Box<dyn Write>, Option<Option<Box<dyn Write>>>)>,

    term: Option<Box<dyn Term + Send + Sync>>,

    // Used to decide whether to render progress bars.
    output_on_new_line: bool,
    error_on_new_line: bool,
    // Whether progress is non-empty.
    progress_has_content: bool,
    // Whether progress (stderr) and stdout (is likely) sharing output.
    progress_conflict_with_output: bool,
    // Whether to redirect stdout writes to stderr. More useful for pager use-case.
    redirect_err_to_out: bool,

    // How many (nested) blocks want progress output disabled.
    progress_disabled: usize,

    // Function to wait for the pager to cleanup (restore terminal state).
    // Might block, unless `pager_quit_func` is called right before.
    pager_wait_func: Option<Box<dyn FnOnce(&mut IOState) + Send>>,
}

/// The "main" IO used by the process.
///
/// This global state makes it easier for Python bindings
/// (ex. "pyio") to obtain the IO state without needing
/// to pass the state across layers. This is similar to
/// `std::io::stdout` etc being globally accessible.
///
/// Use `IO::set_main()` to set the main IO, and `IO::main()`
/// to obtain the "main" `IO`.
static MAIN_IO_REF: LazyLock<RwLock<Option<Weak<Inner>>>> = LazyLock::new(Default::default);

fn colors_disabled_via_env() -> bool {
    hgplain::is_plain(Some("color")) || std::env::var("TERM").ok().as_deref() == Some("dumb")
}

pub trait IsTty {
    fn is_tty(&self) -> bool;

    fn is_stdin(&self) -> bool {
        false
    }
    fn is_stdout(&self) -> bool {
        false
    }
    fn is_stderr(&self) -> bool {
        false
    }

    /// Whether this connection is capable of colors, ignoring whether
    /// the user has enabled colors.
    fn can_color(&self) -> bool {
        self.is_tty() && !colors_disabled_via_env()
    }

    fn pager_active(&self) -> bool {
        false
    }
}

pub trait Read: io::Read + IsTty + Any + Send + Sync {
    fn as_any(&self) -> &dyn Any;

    fn type_name(&self) -> &'static str {
        std::any::type_name::<Self>()
    }
}

pub trait Write: io::Write + IsTty + Any + Send + Sync {
    fn as_any(&self) -> &dyn Any;

    fn type_name(&self) -> &'static str {
        std::any::type_name::<Self>()
    }
}

impl<T: io::Read + IsTty + Any + Send + Sync> Read for T {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl<T: io::Write + IsTty + Any + Send + Sync> Write for T {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

// Read from input.
impl io::Read for IOInput {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let inner = match Weak::upgrade(&self.0) {
            Some(inner) => inner,
            None => return Ok(0),
        };
        let mut inner = inner.locked_with_blocked_interval();
        inner.input.read(buf)
    }
}

// Write to error.
impl io::Write for IOError {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let inner = match Weak::upgrade(&self.0) {
            Some(inner) => inner,
            None => return Ok(buf.len()),
        };
        let mut inner = inner.locked_with_blocked_interval();
        if inner.redirect_err_to_out {
            inner.clear_progress_for_output()?;
            inner.output_on_new_line = buf.ends_with(b"\n");
            return inner.output.write(buf);
        }
        inner.clear_progress_for_error()?;
        inner.error_on_new_line = buf.ends_with(b"\n");
        if let Some(error) = inner.error.as_mut() {
            error.write(buf)
        } else {
            Ok(buf.len())
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        let inner = match Weak::upgrade(&self.0) {
            Some(inner) => inner,
            None => return Ok(()),
        };
        let mut inner = inner.locked_with_blocked_interval();
        if let Some(error) = inner.error.as_mut() {
            error.flush()?;
        }
        Ok(())
    }
}

// Write to output.
impl io::Write for IOOutput {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let inner = match Weak::upgrade(&self.0) {
            Some(inner) => inner,
            None => return Ok(buf.len()),
        };
        let mut inner = inner.locked_with_blocked_interval();
        inner.clear_progress_for_output()?;
        inner.output_on_new_line = buf.ends_with(b"\n");
        inner.output.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        let inner = match Weak::upgrade(&self.0) {
            Some(inner) => inner,
            None => return Ok(()),
        };
        let mut inner = inner.locked_with_blocked_interval();
        inner.output.flush()
    }
}

impl IOProgress {
    /// Set progress to the given text.
    pub fn set(&self, changes: &[Change]) -> io::Result<()> {
        let inner = match Weak::upgrade(&self.0) {
            Some(inner) => inner,
            None => return Ok(()),
        };
        let mut inner = inner.io_state.lock();
        inner.set_progress(changes)
    }

    pub fn term_size(&self) -> (usize, usize) {
        if let Some(inner) = Weak::upgrade(&self.0) {
            inner.locked_with_blocked_interval().term_size()
        } else {
            (DEFAULT_TERM_WIDTH, DEFAULT_TERM_HEIGHT)
        }
    }
}

impl IO {
    pub fn with_input<R>(&self, f: impl FnOnce(&mut dyn Read) -> R) -> R {
        f(self.inner.locked_with_blocked_interval().input.as_mut())
    }

    pub fn with_output<R>(&self, f: impl FnOnce(&dyn Write) -> R) -> R {
        f(self.inner.locked_with_blocked_interval().output.as_ref())
    }

    pub fn with_error<R>(&self, f: impl FnOnce(Option<&dyn Write>) -> R) -> R {
        f(self.inner.locked_with_blocked_interval().error.as_deref())
    }

    pub fn time_interval(&self) -> &TimeInterval {
        &self.inner.time_interval
    }

    /// Returns a clonable value that impls [`io::Write`] to `error` stream.
    /// The output is associated with the `IO` so if the `IO` starts a pager,
    /// the error stream will be properly redirected to the pager.
    ///
    /// If this IO is dropped, the IOError stream will be redirected to null.
    pub fn error(&self) -> IOError {
        IOError(Arc::downgrade(&self.inner))
    }

    /// Returns a clonable value that impls [`io::Read`] from `input` stream.
    /// The output is associated with the `IO` so if the `IO` starts a pager,
    /// the error stream will be properly redirected to the pager.
    ///
    /// If this IO is dropped, the IOInput stream will be redirected to null.
    pub fn input(&self) -> IOInput {
        IOInput(Arc::downgrade(&self.inner))
    }

    /// Returns a clonable value that impls [`io::Write`] to `output` stream.
    /// The output is associated with the `IO` so if the `IO` starts a pager,
    /// the error stream will be properly redirected to the pager.
    ///
    /// If this IO is dropped, the IOError stream will be redirected to null.
    pub fn output(&self) -> IOOutput {
        IOOutput(Arc::downgrade(&self.inner))
    }

    /// Returns a clonable value that provides a way to set progress text.
    pub fn progress(&self) -> IOProgress {
        IOProgress(Arc::downgrade(&self.inner))
    }

    pub fn new<IS, OS, ES>(input: IS, output: OS, error: Option<ES>) -> Self
    where
        IS: Read + 'static,
        OS: Write + 'static,
        ES: Write + 'static,
    {
        let progress_conflict_with_output = match &error {
            None => false, // No progress bar.
            Some(e) => e.is_tty() && output.is_tty(),
        };

        let inner = Inner {
            io_state: Mutex::new(IOState {
                input: Box::new(input),
                output: Box::new(output),
                error: error.map(|e| Box::new(e) as Box<dyn Write>),
                pager_progress: None,
                pre_pager_io: None,
                term: None,
                progress_conflict_with_output,
                output_on_new_line: true,
                error_on_new_line: true,
                progress_has_content: false,
                progress_disabled: 0,
                redirect_err_to_out: false,
                pager_wait_func: None,
            }),
            time_interval: TimeInterval::from_now(),
            pager_quit_func: Default::default(),
        };

        Self {
            inner: Arc::new(inner),
        }
    }

    /// Creates an "null" IO that writes to nowhere and reads empty.
    pub fn null() -> Self {
        let empty = io::empty();
        Self::new(empty, EmptyWrite, None::<EmptyWrite>)
    }

    /// Wait for the pager to exit, and restore outputs to stdio.
    /// Might block if the pager is waiting for the user to exit.
    pub fn wait_pager(&self) -> io::Result<()> {
        let mut inner = self.inner.locked_with_blocked_interval();
        inner.flush()?;

        // Restore pre-pager output/error streams.
        if let Some((output, error)) = inner.pre_pager_io.take() {
            inner.output = output;
            if let Some(error) = error {
                inner.error = error;
            }
        }

        inner.redirect_err_to_out = false;
        inner.pager_progress = None;

        // This might block but shouldn't block quit_pager.
        inner.wait_pager();

        // pager_quit_func is no longer needed.
        let _ = self.inner.pager_quit_func.lock().take();

        Ok(())
    }

    /// Quit the pager now and wait for it to complete cleanup.
    /// Does not restore `input`, `output`, `error`, should only
    /// be used before exiting.
    pub fn quit_pager(&self) {
        let mut lock = self.inner.pager_quit_func.lock();
        let mut func = None;
        mem::swap(&mut func, &mut lock);
        if let Some(func) = func {
            drop(lock);
            func();
        }
    }

    pub fn write(&self, data: impl AsRef<[u8]>) -> io::Result<()> {
        let data = data.as_ref();
        let mut inner = self.inner.locked_with_blocked_interval();
        inner.clear_progress_for_output()?;
        inner.output_on_new_line = data.ends_with(b"\n");
        inner.output.write_all(data)?;
        Ok(())
    }

    pub fn write_err(&self, data: impl AsRef<[u8]>) -> io::Result<()> {
        let data = data.as_ref();
        let mut inner = self.inner.locked_with_blocked_interval();
        if inner.redirect_err_to_out {
            inner.clear_progress_for_output()?;
            inner.output_on_new_line = data.ends_with(b"\n");
            inner.output.write_all(data)?;
            return Ok(());
        }
        inner.clear_progress_for_error()?;
        inner.error_on_new_line = data.ends_with(b"\n");
        if let Some(ref mut error) = inner.error {
            error.write_all(data)?;
        }
        Ok(())
    }

    pub fn set_progress(&self, changes: &[Change]) -> io::Result<()> {
        let mut inner = self.inner.locked_with_blocked_interval();
        inner.set_progress(changes)
    }

    pub fn set_progress_str(&self, data: &str) -> io::Result<()> {
        self.set_progress(&[data.into()])
    }

    pub fn flush(&self) -> io::Result<()> {
        let mut inner = self.inner.locked_with_blocked_interval();
        inner.flush()
    }

    pub fn stdio() -> Self {
        let progress_conflict_with_output = io::stderr().is_tty() && io::stdout().is_tty();
        let inner = Inner {
            io_state: Mutex::new(IOState {
                input: Box::new(io::stdin()),
                output: Box::new(io::stdout()),
                error: Some(Box::new(io::stderr())),
                pager_progress: None,
                pre_pager_io: None,
                term: None,
                progress_conflict_with_output,
                progress_has_content: false,
                progress_disabled: 0,
                output_on_new_line: true,
                error_on_new_line: true,
                redirect_err_to_out: false,
                pager_wait_func: None,
            }),
            time_interval: TimeInterval::from_now(),
            pager_quit_func: Default::default(),
        };
        Self {
            inner: Arc::new(inner),
        }
    }

    pub fn setup_term(&mut self) -> Result<(), termwiz::Error> {
        let mut inner = self.inner.io_state.lock();

        if std::env::var_os("TESTTMP").is_some() {
            // Use dumb terminal with static width/height for tests.
            inner.term = Some(Box::new(DumbTerm::new(DumbTty::new(Box::new(
                io::stderr(),
            )))?));
        } else {
            inner.term = Some(make_real_term()?);
        }

        Ok(())
    }

    /// Attempt to reset terminal back to a good state.
    pub fn reset_term(&self) -> Result<(), io::Error> {
        let mut inner = self.inner.io_state.lock();
        match &mut inner.term {
            Some(term) => term.reset(),
            None => Ok(()),
        }
    }

    /// Obtain the main IO.
    ///
    /// The main IO must be set via `set_main` and is still alive.
    /// Otherwise, this function will return an error.
    pub fn main() -> io::Result<Self> {
        let opt_main_io = MAIN_IO_REF.read();
        let main_io = match opt_main_io.as_ref() {
            Some(io) => io,
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::NotConnected,
                    "IO::main() is not available (call set_main first)",
                ));
            }
        };

        if let Some(inner) = Weak::upgrade(main_io) {
            Ok(Self { inner })
        } else {
            Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "IO::main() is not available (dropped)",
            ))
        }
    }

    /// Report whether self is the current main IO.
    ///
    /// This can be used to minimize race condition windows when running
    /// commands in threads.
    pub fn is_main(&self) -> bool {
        Self::main().is_ok_and(|main| Arc::ptr_eq(&main.inner, &self.inner))
    }

    /// Set the current IO as the main IO.
    ///
    /// Note: If the current IO gets dropped then the main IO will be dropped
    /// too and [`IO::main`] will return an error.
    pub fn set_main(&self) {
        let mut main_io_ref = MAIN_IO_REF.write();
        *main_io_ref = Some(Arc::downgrade(&self.inner));
    }

    /// Check if the pager is active.
    pub fn is_pager_active(&self) -> bool {
        let state = self.inner.io_state.lock();
        state.is_pager_active()
    }

    pub fn start_pager(&self, config: &dyn Config) -> io::Result<()> {
        match config.get("pager", "pager").as_deref() {
            None | Some("internal:streampager") => self.start_streampager(config),
            Some(pager) => self.start_custom_pager(config, pager),
        }
    }

    pub fn start_custom_pager(&self, config: &dyn Config, pager_cmd: &str) -> io::Result<()> {
        let mut inner = self.inner.locked_with_blocked_interval();
        if inner.is_pager_active() {
            return Ok(());
        }

        // use inner.disable_progress to prevent double locking
        inner.disable_progress(true)?;

        // Reproduce tweakdefault.py's logic to improve experience when people have PAGER=less set.
        let pager_cmd = match pager_cmd.trim() {
            "less" => "less -FRQX",
            other => other,
        };

        let is_shell = "|&;<>()$`\\\"' \t\n*?[#~=%"
            .chars()
            .any(|c| pager_cmd.contains(c));

        let mut command = if is_shell {
            Command::new_shell(pager_cmd)
        } else {
            Command::new(pager_cmd)
        };

        // config ENV for pager
        // keep rcutil::defaultpagerenv behavior
        let mut pager_envs: HashMap<&str, String> =
            [("LESS", String::from("FRX")), ("LV", String::from("-c"))]
                .into_iter()
                .filter(|(k, _)| std::env::var(k).is_err())
                .collect();
        if let Some(encoding) = config.get("pager", "encoding") {
            pager_envs.insert("LESSCHARSET", String::from(encoding.as_ref()));
        }

        let spawn_result = command
            .stdin(Stdio::piped())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .envs(&pager_envs)
            .spawn();

        let mut child = match spawn_result {
            Err(e) => match e.kind() {
                std::io::ErrorKind::NotFound => {
                    if let Some(err) = inner.error.as_mut() {
                        writeln!(err, "missing pager command '{pager_cmd}', skipping pager")?;
                    }

                    return Ok(());
                }
                _ => return Err(e),
            },
            Ok(child) => child,
        };

        let mut output_to_pager =
            WriterWithTty::new(Box::new(child.stdin.take().unwrap()), inner.output.is_tty());
        output_to_pager.pretend_stdout = inner.output.is_stdout();
        let output_to_pager = Box::new(output_to_pager);

        let pre_pager_output = std::mem::replace(&mut inner.output, output_to_pager);

        let err_is_tty = inner
            .error
            .as_ref()
            .map_or_else(|| inner.output.is_tty(), |e| e.is_tty());

        let mut pre_pager_error = None;

        // handle pager.stderr
        let send_err_to_pager = err_is_tty && config.get_or_default("pager", "stderr").unwrap();
        if send_err_to_pager {
            inner.redirect_err_to_out = true;
            pre_pager_error = Some(std::mem::take(&mut inner.error));
        }

        inner.pre_pager_io = Some((pre_pager_output, pre_pager_error));

        inner.flush()?;

        let child = Arc::new(Mutex::new(child));
        let child_copy = child.clone();

        // wait for the pager to finish and restore the progress state
        inner.pager_wait_func = Some(Box::new(move |io_state| {
            // currently wait_pager() reset inner's io to hard coded values.

            let _ = child.lock().wait();
            let _ = io_state.disable_progress(false);
        }));

        // kill the child process when quitting
        self.inner.pager_quit_func.lock().replace(Box::new(move || {
            let _ = child_copy.lock().kill();
        }));

        Ok(())
    }

    /// Starts a pager.
    ///
    /// It is recommended to run [`IO::flush`] and [`IO::wait_pager`] before exiting.
    pub fn start_streampager(&self, config: &dyn Config) -> io::Result<()> {
        let mut inner = self.inner.locked_with_blocked_interval();
        if inner.is_pager_active() {
            return Ok(());
        }

        inner.set_progress(&[])?;

        let mut pager_config = PagerConfig::from_config_file().with_env();
        pager_config.mouse = config.must_get("pager", "mouse").unwrap_or(false);

        let mut pager = Pager::new_using_system_terminal_with_config(pager_config.clone())
            .or_else(|_| Pager::new_using_stdio_with_config(pager_config))
            .map_err(io::Error::other)?;

        // Configure the pager.
        // The Hybrid mode is similar to "-FX" from "less".
        let mut interface_mode = InterfaceMode::Hybrid;
        // Similar to "less" behavior - lines are wrapped and copy-paste preserves long lines.
        let mut wrapping_mode = WrappingMode::GraphemeBoundary;
        if let Some(mode_str) = config.get("pager", "interface") {
            let mode = InterfaceMode::from(mode_str.as_ref());
            interface_mode = mode;
        }
        if let Ok(Some(wrapping_mode_str)) = config.get_opt::<String>("pager", "wrapping-mode") {
            match wrapping_mode_str.to_lowercase().as_str() {
                "word" => wrapping_mode = WrappingMode::WordBoundary,
                "unwrapped" => wrapping_mode = WrappingMode::Unwrapped,
                _ => {}
            }
        }
        pager.set_wrapping_mode(wrapping_mode);
        // Similar to "less" default.
        pager.set_scroll_past_eof(config.must_get("pager", "scroll-past-eof").unwrap_or(false));
        pager.set_interface_mode(interface_mode);
        // Setting this to "false" will preserve pending terminal input in
        // "direct" mode at the cost of disabling streampager user input
        // handling.
        pager.set_startup_poll_input(
            config
                .must_get("pager", "startup-poll-input")
                .unwrap_or(false),
        );

        let (out_read, out_write) = pipe();
        let (err_read, err_write) = pipe();
        let (prg_read, prg_write) = pipe();

        let out_is_tty = inner.output.is_tty();
        let out_is_stdout = inner.output.is_stdout();
        let err_is_tty = inner
            .error
            .as_ref()
            .map_or_else(|| out_is_tty, |e| e.is_tty());

        inner.flush()?;

        let mut output_to_pager = WriterWithTty::new(Box::new(out_write), out_is_tty);
        output_to_pager.pretend_stdout = out_is_stdout;

        let pre_pager_output = std::mem::replace(&mut inner.output, Box::new(output_to_pager));

        pager
            .add_stream(out_read, "")
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;

        let mut pre_pager_error = None;

        // Only use the pager for error stream if error stream is a tty.
        // This makes `hg 2>foo` works as expected.
        if err_is_tty {
            pre_pager_error = Some(
                inner.error.replace(
                    Box::new(WriterWithTty::new(Box::new(err_write), err_is_tty)) as Box<dyn Write>,
                ),
            );
            let separate =
                config.get_opt::<bool>("pager", "separate-stderr").ok() == Some(Some(true));
            inner.redirect_err_to_out = !separate;
            if separate {
                pager
                    .add_error_stream(err_read, "stderr")
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
            }
        }

        inner.pre_pager_io = Some((pre_pager_output, pre_pager_error));

        let mut pager_term =
            DumbTerm::new(DumbTty::new(Box::new(prg_write))).map_err(io::Error::other)?;
        pager_term.set_separator(0x0C);
        inner.pager_progress = Some(Box::new(pager_term));
        pager.set_progress_stream(prg_read);

        let pager_action_sender = pager.action_sender();
        let pager_thread_handler = Arc::new(Mutex::new(Some(spawn(|| {
            let _ = pager.run();
        }))));

        let pager_thread_handler_wait = pager_thread_handler.clone();
        inner.pager_wait_func = Some(Box::new(move |_| {
            if let Some(handler) = pager_thread_handler_wait.lock().take() {
                let _ = handler.join();
            }
        }));

        self.inner.pager_quit_func.lock().replace(Box::new(move || {
            let _ = pager_action_sender.send(Action::Quit);

            if let Some(handler) = pager_thread_handler.lock().take() {
                let start = Instant::now();

                // Wait up to 10ms for streampager to shut down.
                while !handler.is_finished() && start.elapsed() < Duration::from_millis(10) {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        }));

        Ok(())
    }

    /// Disable progress rendering.
    /// - `disable_progress(true)` disables progress rendering. It can be nested.
    /// - `disable_progress(false)` cancels out a `disable_progress(true)`.
    ///   If all `disable_progress(true)` are canceled out, restore the progress
    ///   rendering.
    pub fn disable_progress(&self, disabled: bool) -> io::Result<()> {
        let mut inner = self.inner.locked_with_blocked_interval();
        inner.disable_progress(disabled)
    }

    pub fn set_progress_pipe_writer(&self, progress: Option<PipeWriter>) -> io::Result<()> {
        let mut inner = self.inner.locked_with_blocked_interval();
        inner.pager_progress = match progress {
            Some(progress) => {
                let mut pager_term =
                    DumbTerm::new(DumbTty::new(Box::new(progress))).map_err(io::Error::other)?;
                pager_term.set_separator(0x0C);
                Some(Box::new(pager_term))
            }
            None => None,
        };
        Ok(())
    }
}

struct LockedIOState<'a> {
    _interval: BlockedInterval,
    inner: MutexGuard<'a, IOState>,
}

impl Deref for LockedIOState<'_> {
    type Target = IOState;
    fn deref(&self) -> &Self::Target {
        self.inner.deref()
    }
}

impl DerefMut for LockedIOState<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.inner.deref_mut()
    }
}

impl Inner {
    /// Lock, and track the interval as "blocking".
    fn locked_with_blocked_interval(&self) -> LockedIOState<'_> {
        let interval = self.time_interval.scoped_blocked_interval("stdio".into());
        let inner = self.io_state.lock();
        LockedIOState {
            _interval: interval,
            inner,
        }
    }
}

impl IOState {
    pub(crate) fn flush(&mut self) -> io::Result<()> {
        self.output.flush()?;
        if let Some(ref mut error) = self.error {
            error.flush()?;
        }
        Ok(())
    }

    /// Clear the progress (temporarily) for other output.
    fn clear_progress_for_error(&mut self) -> io::Result<()> {
        if self.progress_has_content && self.pager_progress.is_none() {
            self.progress_has_content = false;
            if let Some(ref mut term) = self.term {
                write_term_progress(term, &[]).map_err(io::Error::other)?;
            }
        }
        Ok(())
    }

    /// Clear the progress (temporarily) if it ("stderr") conflicts with "stdout" output.
    fn clear_progress_for_output(&mut self) -> io::Result<()> {
        // If self.progress is set. Progress writes to streampager, and does not need clear.
        if self.progress_conflict_with_output && self.pager_progress.is_none() {
            self.clear_progress_for_error()?;
        }
        Ok(())
    }

    fn term_size(&mut self) -> (usize, usize) {
        if let Some(ref mut term) = self.term {
            if let Ok((cols, rows)) = term.size() {
                return (cols, rows);
            }
        }

        (DEFAULT_TERM_WIDTH, DEFAULT_TERM_HEIGHT)
    }

    fn disable_progress(&mut self, disabled: bool) -> io::Result<()> {
        if disabled {
            self.progress_disabled += 1;
            self.set_progress(&[])?;
        } else {
            if self.progress_disabled == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "disable_progress(false) called without matching disable_progress(true)",
                ));
            }
            self.progress_disabled -= 1;
        }
        Ok(())
    }

    fn set_progress(&mut self, mut changes: &[Change]) -> io::Result<()> {
        let inner = self;
        if inner.progress_disabled > 0 {
            changes = &[];
        }

        if let Some(ref mut progress) = inner.pager_progress {
            write_term_progress(progress, changes).map_err(io::Error::other)?;
        } else {
            if !inner.output_on_new_line || !inner.error_on_new_line {
                // There is a line that hasn't ended.
                // Not suitable to render progress bars.
                changes = &[];
            }

            // Fast path: empty progress, unchanged.
            if changes.is_empty() && !inner.progress_has_content {
                return Ok(());
            }

            // Flush pending output if it might conflict with progress.
            if inner.progress_conflict_with_output {
                inner.output.flush()?;
            }

            if let Some(ref mut term) = inner.term {
                inner.progress_has_content = !changes.is_empty();
                write_term_progress(term, changes).map_err(io::Error::other)?;
            }
        }
        Ok(())
    }

    /// Wait for the pager to exit and cleanup (restore terminal).
    fn wait_pager(&mut self) {
        let mut func = None;
        mem::swap(&mut func, &mut self.pager_wait_func);
        if let Some(func) = func {
            func(self);
        }
    }

    fn is_pager_active(&self) -> bool {
        self.pager_wait_func.is_some()
    }
}

/// Write data to the progress area by clearing everything after
/// cursor, writing data, then moving cursor back.
fn write_term_progress(
    term: &mut Box<dyn Term + Send + Sync>,
    changes: &[Change],
) -> Result<(), termwiz::Error> {
    let (cols, rows) = term.size()?;
    let mut change_seq = ChangeSequence::new(rows, cols);
    change_seq.add(Change::ClearToEndOfScreen(Default::default()));
    change_seq.add_changes(changes.to_vec());
    change_seq.move_to((0, 0));
    term.render(&change_seq.consume())?;

    Ok(())
}

impl Drop for IOState {
    fn drop(&mut self) {
        let _ = self.disable_progress(true);
        let _ = self.flush();
        // Drop the output and error. This sends EOF to pager.
        self.output = Box::<Vec<u8>>::default();
        self.error = None;
        self.pager_progress = None;
        self.wait_pager();
    }
}

// Workaround `io::Empty` not implementing `io::Write` before Rust 1.73
struct EmptyWrite;
impl io::Write for EmptyWrite {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl IsTty for EmptyWrite {
    fn is_tty(&self) -> bool {
        false
    }
}
