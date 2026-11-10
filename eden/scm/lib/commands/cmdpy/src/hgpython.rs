/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::env;
use std::path::Path;
use std::sync::Arc;
use std::sync::OnceLock;

use clidispatch::command::CommandDefinition;
use clidispatch::command::CommandTable;
use clidispatch::io::IO;
use configmodel::Config;
use cpython::*;
use cpython_ext::PythonKeepAlive;
use cpython_ext::ResultPyErrExt;
use cpython_ext::format_py_error;
use pyio::WrappedIO;
use pyio::wrap_pyio;
use tracing::debug_span;
use tracing::info_span;

use crate::python::py_init_threads;
use crate::python::py_initialize;
use crate::python::py_is_initialized;
use crate::python::py_main;

const HGPYENTRYPOINT_MOD: &str = "sapling";
/// Python interpreter that bridges to Rust commands and bindings.
pub struct HgPython {
    keep_alive: Option<PythonKeepAlive>,
}

/// Configuration for Rust commands used by Python.
/// This needs to be manually configured to avoid cyclic dependency.
// The PartialEq implementation for RustCommandConfig compares function pointers by address.
// This is intentional and safe in this context.
#[allow(unpredictable_function_pointer_comparisons)]
#[derive(Clone, Copy, Eq, PartialEq, Debug)]
pub struct RustCommandConfig {
    /// How to obtain the command table.
    pub table: fn() -> CommandTable,
    /// How to run a command and return an exit code.
    pub run_command: fn(Vec<String>, &IO) -> i32,
}

static RUST_COMMAND_CONFIG: OnceLock<RustCommandConfig> = OnceLock::new();

impl RustCommandConfig {
    /// Register the `RustCommandConfig` so `HgPython` can use it.
    /// Must be called before the first `HgPython` initialization.
    /// If called multiple times, must provide the same functions.
    /// Otherwise the program will panic.
    pub fn register(self) {
        let orig_config = RUST_COMMAND_CONFIG.get_or_init(|| self);
        assert_eq!(
            orig_config, &self,
            "bug: cannot register different RustCommandConfigs"
        );
    }

    fn get() -> &'static Self {
        RUST_COMMAND_CONFIG
            .get()
            .expect("bug: RustCommandConfig must be registered before Python initialization")
    }
}

impl HgPython {
    pub fn new(args: &[String]) -> HgPython {
        let py_initialized_by_us = !py_is_initialized();
        let keep_alive = if py_initialized_by_us {
            let keep_alive = PythonKeepAlive::new();
            Self::setup_python(args);
            Some(keep_alive.enable_py_finalize_on_drop(true))
        } else {
            None
        };
        HgPython { keep_alive }
    }

    pub(crate) fn is_python_initialized_by_us(&self) -> bool {
        self.keep_alive.is_some()
    }

    fn setup_python(args: &[String]) {
        let span = info_span!("Initialize Python");
        let _guard = span.enter();
        let args = Self::prepare_args(args);

        let home = Self::sapling_python_home();

        py_initialize(&args, home.as_ref());

        py_init_threads();

        let gil = Python::acquire_gil();
        let py = gil.python();

        // Putting the module in sys.modules makes it importable.
        let sys = py.import("sys").unwrap();

        // If this fails, it's a fatal error.
        let name = "bindings";
        let bindings_module = PyModule::new(py, name).unwrap();
        prepare_builtin_modules(py, &bindings_module).unwrap();

        let sys_modules = PyDict::extract(py, &sys.get(py, "modules").unwrap()).unwrap();
        sys_modules.set_item(py, name, bindings_module).unwrap();
        Self::update_meta_path(py, home, &sys);
    }

    fn sapling_python_home() -> Option<String> {
        if let Ok(v) = std::env::var("SAPLING_PYTHON_HOME") {
            if !v.is_empty() && Path::new(&v).is_dir() {
                Some(v)
            } else {
                None
            }
        } else {
            infer_python_home()
        }
    }

    fn update_meta_path(py: Python, home: Option<String>, sys: &PyModule) {
        if let Some(dir) = home.as_ref() {
            // Append the Python home to sys.path.
            tracing::debug!(
                "Python modules will be imported from filesystem {} (SAPLING_PYTHON_HOME)",
                dir
            );

            // NB: This has no effect for "debugpython" on Python 3.10 (see py_initialize).
            let sys_path = PyList::extract(py, &sys.get(py, "path").unwrap()).unwrap();
            sys_path.append(py, PyString::new(py, dir).into_object());
        }

        let meta_path_finder = pymodules::BindingsModuleFinder::new(py, home).unwrap();
        let meta_path = PyList::extract(py, &sys.get(py, "meta_path").unwrap()).unwrap();
        meta_path.insert(py, 0, meta_path_finder.into_object());
    }

    fn prepare_args(args: &[String]) -> Vec<String> {
        // Replace args[0] with the absolute current_exe path. This workarounds
        // an issue in libpython sys.path handling.
        //
        // More context: Usually, argv[0] is either:
        // - a relative path to the executable, like "hg", or "./hg". It can be
        //   translated to an absolute path using the PATH environment variable
        //   and the current workdir.
        // - an absolute path to the executable, like "/bin/hg".
        //
        // When running as local build, we expect libpython to add the
        // "executable path" to sys.path. However, libpython seems pretty dumb
        // if argv[0] is a relative path, and it's not in the current workdir
        // (in other words, libpython seems to ignore PATH). Therefore, give
        // it some hint by passing the absolute path resolved by the Rust stdlib.
        Some(
            env::current_exe()
                .unwrap()
                .into_os_string()
                .into_string()
                .unwrap(),
        )
        .into_iter()
        .chain(args.iter().skip(1).cloned())
        .collect()
    }

    fn run_hg_py(
        &self,
        py: Python<'_>,
        args: &[String],
        io: &clidispatch::io::IO,
        config: &Arc<dyn Config>,
        skip_pre_hooks: bool,
    ) -> PyResult<()> {
        let entry_point_mod =
            info_span!("import sapling").in_scope(|| py.import(HGPYENTRYPOINT_MOD))?;
        let call_args = {
            let fin = io.with_input(|i| read_to_py_object(py, i));
            let fout = io.with_output(|o| write_to_py_object(py, o));
            let ferr = io.with_error(|e| match e {
                None => fout.clone_ref(py),
                Some(error) => write_to_py_object(py, error),
            });
            let context = context::CoreContext::new(config.clone(), io.clone(), args.to_vec());
            let context = pycontext::context::create_instance(py, context).unwrap();
            (args, fin, fout, ferr, context, skip_pre_hooks).to_py_object(py)
        };
        entry_point_mod.call(py, "run", call_args, None)?;
        Ok(())
    }

    /// Run an hg command defined in Python.
    pub fn run_hg(
        &self,
        args: &[String],
        orig_args: &[String],
        io: &clidispatch::io::IO,
        config: &Arc<dyn Config>,
        skip_pre_hooks: bool,
    ) -> i32 {
        let gil = Python::acquire_gil();
        let py = gil.python();
        let result = (|| {
            let sys = py.import("sys")?;
            let saved_argv = sys.get(py, "argv")?;
            sys.add(py, "argv", Self::prepare_args(orig_args))?;
            let result = self.run_hg_py(py, args, io, config, skip_pre_hooks);
            // A nested command must leave its caller's arguments intact.
            sys.add(py, "argv", saved_argv)?;
            result
        })();
        match result {
            // The code below considers the following exit scenarios:
            // - `PyResult` is `Ok`. This means that the Python code returned
            //    successfully, without calling `sys.exit` or raising an
            //    uncaught exception
            // - `PyResult` is a `PyErr(SystemExit)`. This means that the Python
            //    code called `sys.exit`.
            //    - The expected case is that the `SystemExit` instance contains
            //      a `code` attribute, which is the desired exit code.
            //    - If it does not, we fail hard with code 255.
            // - `PyResult` is a `PyErr(UncaughtException)`. Something went wrong.
            //    Python's behavior in this case is to print a traceback and to
            //    return 1.
            Err(mut err) => {
                if let Ok(system_exit) = err.instance(py).extract::<exc::SystemExit>(py) {
                    match system_exit.as_object().getattr(py, "code") {
                        Ok(code) => code.extract::<i32>(py).unwrap(),
                        Err(_) => 255,
                    }
                } else {
                    let message = format_py_error(py, &err).unwrap_or_else(|err| {
                        format!("unknown python exception {:?} {:?}", err.ptype, err.pvalue)
                    });
                    let _ = io.write_err(message);
                    1
                }
            }
            Ok(()) => 0,
        }
    }

    /// Setup ad-hoc tracing with `pattern` about modules.
    /// See `sapling/traceimport.py` for details.
    ///
    /// Call this before `run_python`, or `run_hg`.
    ///
    /// This is merely to provide convenience.  The user can achieve the same
    /// effect via `run_python`, then import the modules and calling methods
    /// manually.
    pub fn setup_tracing(&mut self, pattern: String) -> PyResult<()> {
        let gil = Python::acquire_gil();
        let py = gil.python();
        let traceimport = py.import("sapling.traceimport")?;
        traceimport.call(py, "enable", (pattern,), None)?;
        Ok(())
    }

    /// Run the Python interpreter.
    pub fn run_python(&mut self, args: &[String], io: &clidispatch::io::IO) -> u8 {
        let args = Self::prepare_args(args);
        if self.is_python_initialized_by_us() {
            let keep_alive = self.keep_alive.take();
            // Command flags like `--help` and `--version` go through a code path that
            // *partially* finalizes the Python runtime. They called `_PyRuntime_Finalize` [1]
            // but not `Py_Finalize`. There is no public API like `Py_IsInitialized` to detect
            // this situation. Calling `Py_Finalize` by `keep_alive::drop` will segfault.
            // So let's forget the `keep_alive`.
            // [1]: https://github.com/python/cpython/commit/f5f336a819a3d881bb217bf8f9b5cacba03a4e45
            std::mem::forget(keep_alive);
            py_main(&args)
        } else {
            // If Python is not initialized by us, it's expected that this
            // function does not call Py_Finalize.
            //
            // If we call Py_Main, users like the Python testutil will crash
            // because Py_Main calls Py_Finalize.
            // Avoid that by just returning an error code.
            let _ = io.write_err("error: Py_Main cannot be used in this context\n");
            1
        }
    }
}

fn read_to_py_object(py: Python, reader: &dyn clidispatch::io::Read) -> PyObject {
    let any = reader.as_any();
    if any.downcast_ref::<std::io::Stdin>().is_some() {
        // The Python code accepts None, and will use its default input stream.
        py.None()
    } else if let Some(obj) = any.downcast_ref::<WrappedIO>() {
        obj.obj.clone_ref(py)
    } else {
        unimplemented!(
            "converting non-stdio Read ({}) from Rust to Python is not implemented",
            reader.type_name()
        )
    }
}

fn write_to_py_object(py: Python, writer: &dyn clidispatch::io::Write) -> PyObject {
    let any = writer.as_any();
    if any.downcast_ref::<std::io::Stdout>().is_some()
        || any.downcast_ref::<std::io::Stderr>().is_some()
    {
        py.None()
    } else if let Some(obj) = any.downcast_ref::<WrappedIO>() {
        obj.obj.clone_ref(py)
    } else {
        unimplemented!(
            "converting non-stdio Write ({}) from Rust to Python is not implemented",
            writer.type_name()
        )
    }
}

fn python_command_aliases(def: &CommandDefinition) -> String {
    def.aliases()
        .split('|')
        .map(|alias| alias.strip_prefix("legacy:").unwrap_or(alias))
        .collect::<Vec<_>>()
        .join("|")
}

fn init_bindings_commands(py: Python, package: &str) -> PyResult<PyModule> {
    // Called by "-t.py" tests.
    fn run_py(
        py: Python,
        args: Vec<String>,
        fin: Option<PyObject>,
        fout: Option<PyObject>,
        ferr: Option<PyObject>,
    ) -> PyResult<i32> {
        let run_command = RustCommandConfig::get().run_command;
        if let (Some(fin), Some(fout), Some(ferr)) = (fin, fout, ferr) {
            let fin = wrap_pyio(py, fin);
            let fout = wrap_pyio(py, fout);
            let ferr = wrap_pyio(py, ferr);
            let old_io = IO::main();
            let io = IO::new(fin, fout, Some(ferr));
            io.set_main();
            let result = Ok(py.allow_threads(|| (run_command)(args, &io)));
            if let (Ok(old_io), true) = (old_io, io.is_main()) {
                old_io.set_main();
            }
            result
        } else {
            // Reuse the main IO.
            let io = IO::main().map_pyerr(py)?;
            Ok(py.allow_threads(|| (run_command)(args, &io)))
        }
    }

    fn table_py(py: Python) -> PyResult<PyDict> {
        let table = (RustCommandConfig::get().table)();
        let py_table: PyDict = PyDict::new(py);
        for def in table.values() {
            let doc = def.doc().to_string();

            let aliases = python_command_aliases(def);

            if let Some(synopsis) = def.synopsis().map(|s| s.to_string()) {
                py_table.set_item(py, aliases, (doc, def.flags(), synopsis))?;
            } else {
                py_table.set_item(py, aliases, (doc, def.flags()))?;
            }
        }
        Ok(py_table)
    }

    let name = [package, "commands"].join(".");
    let m = PyModule::new(py, &name)?;
    m.add(
        py,
        "run",
        py_fn!(
            py,
            run_py(
                args: Vec<String>,
                fin: Option<PyObject> = None,
                fout: Option<PyObject> = None,
                ferr: Option<PyObject> = None,
            )
        ),
    )?;
    m.add(py, "table", py_fn!(py, table_py()))?;
    Ok(m)
}

/// Prepare builtin modules. Namely, `bindings`.
///
/// This makes sure the bindings use the same dependencies as the main
/// executable. For example, the global instance in `blackbox` is the
/// same, so if the Rust code logs to blackbox, the Python code can read
/// them out via bindings.
///
/// This is more difficult if the bindings project is compiled as a separate
/// Python extension, because `blackbox` will be compiled separately, and
/// the global instance might be different.
pub fn prepare_builtin_modules(py: Python<'_>, module: &PyModule) -> PyResult<()> {
    let span = debug_span!("Initialize bindings");
    let _guard = span.enter();

    // Prepare `bindings.command`. This cannot be done in the bindings
    // crate because it forms a circular dependency.
    module.add(
        py,
        "commands",
        init_bindings_commands(py, module.name(py)?)?,
    )?;
    bindings::populate_module(py, module)?;
    Ok(())
}

fn infer_python_home() -> Option<String> {
    let exe_path = match std::env::current_exe() {
        Ok(path) => path,
        _ => return None,
    };

    if cfg!(unix) && (exe_path.starts_with("/usr/") || exe_path.starts_with("/opt/")) {
        // Unlikely an in-repo path. Skip repo discovery.
        return None;
    }

    // resolve symbolic links
    let exe_path = exe_path.canonicalize().ok()?;

    // Try to locate the repo root and check the known "home" path.
    let prefix = if cfg!(feature = "fb") {
        // fbsource
        "fbcode/eden/scm"
    } else {
        // github: facebook/sapling
        "eden/scm"
    };
    let mut path: &Path = exe_path.as_path();
    while let Some(parent) = path.parent() {
        path = parent;
        if path.join(".hg").is_dir() || path.join(".sl").is_dir() {
            let maybe_home = path.join(prefix);
            if maybe_home.is_dir() {
                tracing::debug!("Discovered SAPLING_PYTHON_HOME at {}", maybe_home.display());
                return Some(maybe_home.display().to_string());
            }
            break;
        }
    }

    None
}
