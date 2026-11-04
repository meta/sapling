/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#![allow(non_camel_case_types)]

use cpython::*;
use pyconfigloader::config;

pub fn init_module(py: Python, package: &str) -> PyResult<PyModule> {
    let name = [package, "doctor"].join(".");
    let m = PyModule::new(py, &name)?;
    m.add(
        py,
        "diagnose_network",
        py_fn!(py, diagnose_network(config: &config)),
    )?;
    Ok(m)
}

/// Returns `(treatment, details, is_cert_problem)` when a problem is found.
fn diagnose_network(py: Python, config: &config) -> PyResult<Option<(String, String, bool)>> {
    let config = &config.get_cfg(py);
    match network_doctor::Doctor::new().diagnose(config) {
        Ok(()) => Ok(None),
        Err(d) => Ok(Some((
            d.treatment(config),
            format!("{d}"),
            d.is_cert_problem(),
        ))),
    }
}
