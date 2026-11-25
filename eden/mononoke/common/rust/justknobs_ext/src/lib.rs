/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Mononoke-local facade over `justknobs_stub` whose three top-level
//! functions (`eval`, `get`, `get_as`) panic on failure instead of
//! returning `Result`. The Mononoke style rule (see
//! `eden/mononoke/AGENTS.md`) is that a missing or misspelled
//! JustKnob is a programming error; we want it to surface loudly at
//! the call site instead of being silently masked by `unwrap_or`.
//!
//! Everything else (the test helpers, the cached-config init, the
//! `JustKnobsCombinedImpl` struct) is re-exported unchanged from the
//! stub so Mononoke `use` paths keep working.

pub mod overrides;

pub use justknobs_stub::JustKnobsCombinedImpl;
pub use justknobs_stub::cached_config;
pub use justknobs_stub::init_cached_config_just_knobs;
pub use justknobs_stub::init_cached_config_just_knobs_worker;
pub use justknobs_stub::test_helpers;

/// Evaluate a Boolean knob. Panics if the read fails (e.g. the JK doesn't
/// exist or is misspelled).
pub fn eval(name: &str, hash_val: Option<&str>, switch_val: Option<&str>) -> bool {
    if let Some(overridden) = overrides::eval(name) {
        return overridden;
    }
    justknobs_stub::eval(name, hash_val, switch_val)
        .unwrap_or_else(|e| panic!("JustKnobs eval failed for {name}: {e:#}"))
}

/// Evaluate a numeric knob. Panics if the read fails.
pub fn get(name: &str, switch_val: Option<&str>) -> i64 {
    if let Some(overridden) = overrides::get(name) {
        return overridden;
    }
    justknobs_stub::get(name, switch_val)
        .unwrap_or_else(|e| panic!("JustKnobs get failed for {name}: {e:#}"))
}

/// Evaluate a numeric knob converted to `T`. Panics if the read or the
/// `i64 -> T` conversion fails.
pub fn get_as<T>(name: &str, switch_val: Option<&str>) -> T
where
    T: TryFrom<i64>,
    <T as TryFrom<i64>>::Error: std::error::Error + Send + Sync + 'static,
{
    if let Some(overridden) = overrides::get(name) {
        return overridden
            .try_into()
            .unwrap_or_else(|e| panic!("JustKnobs get_as failed for {name}: {e:#}"));
    }
    justknobs_stub::get_as(name, switch_val)
        .unwrap_or_else(|e| panic!("JustKnobs get_as failed for {name}: {e:#}"))
}
