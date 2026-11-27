/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use mononoke_types::audit::ValidationOutcome;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ValidationSummary {
    pub passed: u64,
    pub failed: u64,
    pub not_applicable: u64,
}

impl ValidationSummary {
    pub(crate) fn record(&mut self, outcome: &ValidationOutcome) {
        match outcome {
            ValidationOutcome::Passed { .. } => self.passed += 1,
            ValidationOutcome::Failed { .. } => self.failed += 1,
            ValidationOutcome::NotApplicable { .. } => self.not_applicable += 1,
        }
    }
}
