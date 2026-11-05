/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::sync::LazyLock;
use std::sync::Mutex;

use anyhow::Result;
use factory::FunctionSignature;
use hook::PythonHookSig;

use crate::HgPython;

/// Register a function to run python hooks.
pub(crate) fn init() {
    factory::register_function::<PythonHookSig>(initialize_python_and_run_python_hook);
}

/// Maybe call `Py_Finalize`.
pub(crate) fn deinit() {
    let _ = PYTHON.lock().unwrap().take();
}

static PYTHON: LazyLock<Mutex<Option<HgPython>>> = LazyLock::new(|| Mutex::new(None));

fn initialize_python_and_run_python_hook(
    input: <PythonHookSig as FunctionSignature>::In,
) -> Result<i8> {
    // Initialize the Python interpreter on demand.
    let interp = HgPython::new(&[]);

    // Prepare input.
    let (repo, spec, hook_name, kwargs) = input;
    let result = pyhook::run_python_hook(repo, spec, hook_name, kwargs);

    // Do not `Py_Finalize` immediately. Keep it in `PYTHON` to run other hooks.
    // No need to wait for the lock. Any thread doing this has a same effect.
    if let Ok(mut locked) = PYTHON.try_lock() {
        // Note: do not run Python logic while holding this lock. Otherwise it
        // might deadlock with the Python GIL in theory.
        // Keep only the `HgPython` that initialized Python. Dropping it
        // finalizes Python, and the others own nothing.
        if interp.is_python_initialized_by_us() {
            *locked = Some(interp);
        }
    }

    result
}
