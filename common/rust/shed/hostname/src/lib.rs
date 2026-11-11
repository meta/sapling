/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

#![deny(warnings, missing_docs, clippy::all, rustdoc::broken_intra_doc_links)]

//! Crate that wraps the OSS hostname and FB internal libraries to provide
//! hostname resolution

use anyhow::Result;

/// Returns hostname as reported by the system
#[allow(clippy::collapsible_if)]
pub fn get_hostname() -> Result<String> {
    if let Ok(aws) = std::env::var("AWS_REGION") {
        if !aws.is_empty() {
            if let Ok(hostname) = std::env::var("HOSTNAME") {
                // we are running in AWS and probably in EKS, we can trust the HOSTNAME env var
                return Ok(hostname);
            }
        }
    }

    #[cfg(any(target_os = "macos", not(any(conda_build, fbcode_build))))]
    {
        Ok(::real_hostname::get()?.to_string_lossy().into_owned())
    }

    #[cfg(all(not(target_os = "macos"), any(conda_build, fbcode_build)))]
    {
        fbwhoami::FbWhoAmI::get()?
            .name
            .clone()
            .ok_or_else(|| ::anyhow::Error::msg("No hostname in fbwhoami"))
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use std::process::Command;

    use super::get_hostname;

    #[test]
    fn macos_hostname_matches_system_hostname() {
        let output = Command::new("/bin/hostname")
            .output()
            .expect("read macOS hostname");
        assert!(output.status.success());
        let expected = String::from_utf8(output.stdout).expect("hostname is UTF-8");
        assert_ne!(expected.trim(), "");
        assert_eq!(
            get_hostname().expect("resolve hostname on macOS"),
            expected.trim(),
        );
    }
}
