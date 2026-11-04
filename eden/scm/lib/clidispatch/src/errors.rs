/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::borrow::Cow;

use cliparser::parser::ParseError;
use configmodel::Config;
use configmodel::ConfigExt;
use thiserror::Error;
#[cfg(feature = "eden")]
use thrift_types::edenfs_clients as eden_clients;

/// A network failure the network doctor could explain. Displays as the
/// diagnosis text so it prints like any other error.
#[derive(Debug, Error)]
#[error("{message}")]
pub struct DiagnosedNetworkError {
    pub diagnosis: network_doctor::Diagnosis,
    message: String,
}

/// Sampling metadata shared with Python's `error.py` and the telemetry wrapper.
pub fn error_category(err: &anyhow::Error) -> Option<&'static str> {
    if err.is::<RepoRequired>() || err.is::<ParseError>() || err.is::<MalformedConfigOption>() {
        Some("usage")
    } else if err
        .downcast_ref::<DiagnosedNetworkError>()
        .is_some_and(|err| err.diagnosis.is_cert_problem())
    {
        Some("certificate")
    } else {
        None
    }
}

#[derive(Debug, Error)]
#[error("cannot decode arguments")]
pub struct NonUTF8Arguments;

pub use cliparser::errors::InvalidArguments;

#[derive(Debug, Error)]
// This error message isn't user facing yet, so let's just say "sl".
#[error("unknown command '{0}'\n(use 'sl help' to get help)")]
pub struct UnknownCommand(pub String);

/// Explicitly fallback to Python code path.
///
/// Ideally this does not exist.
#[derive(Debug, Error)]
#[error("Feature not supported in Rust implementation; falling back to Python due to: {0}")]
pub struct FallbackToPython(pub String);

#[derive(Debug, Error)]
#[error("")]
pub struct FailedFallbackToPython;

#[derive(Debug, Error)]
#[error(
    "'{0}' is not inside a repository, but this command requires a repository!\n(use 'cd' to go to a directory inside a repository and try again)"
)]
pub struct RepoRequired(pub String);

#[derive(Debug, Error)]
#[error("missing command name\n(use 'sl help' to get help)")]
pub struct CommandRequired;

#[derive(Debug, Error)]
#[error("malformed --config option: '{0}' (use --config section.name=value)")]
pub struct MalformedConfigOption(pub String);

#[derive(Debug)]
pub struct Abort(pub Cow<'static, str>);

impl std::error::Error for Abort {}

impl std::fmt::Display for Abort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", identity::default().punch(&self.0))
    }
}

/// Print an error suitable for end-user consumption.
///
/// This function adds `hg:` or `abort:` to error messages.
pub fn print_error(
    err: &anyhow::Error,
    io: &crate::io::IO,
    config: Option<&dyn configmodel::Config>,
    traceback: bool,
) {
    let cli_name = identity::cli_name();
    if let Some(e) = err.downcast_ref::<types::errors::PermissionDenied>() {
        if let Some(config) = config {
            let msg = crate::acl::format_permission_denied_error(e, config);
            let _ = io.write_err(format!("abort: {msg}\n"));
        } else {
            let _ = io.write_err(format!("abort: {e}\n"));
        }
        return;
    }
    if err.downcast_ref::<configloader::Error>().is_some() {
        let _ = io.write_err(format!("{cli_name}: parse error: {err:?}\n"));
    } else if err.downcast_ref::<configloader::Errors>().is_some() {
        let _ = io.write_err(format!("{cli_name}: parse errors: {err:?}\n"));
    } else if let Some(ParseError::AmbiguousCommand {
        command_name: _,
        possibilities,
    }) = err.downcast_ref::<ParseError>()
    {
        let _ = io.write_err(format!("{cli_name}: {err}:\n"));
        for possibility in possibilities {
            // UX: Colorize the output once `io` can output colors.
            let _ = io.write_err(format!("     {possibility}\n"));
        }
    } else {
        #[cfg(feature = "eden")]
        {
            if let Some(eden_clients::errors::GetScmStatusV2Error::ex(e)) = err.downcast_ref() {
                let _ = io.write_err(format!("abort: {}\n", e.message));
                let _ = io.flush();
                return;
            }
        }

        // Ideally we'd identify expected errors and unexpected errors and print the full {:?}
        // output for unexpected errors. Today we can't make that distinction though, so for now we
        // print it in the user-friendly way.
        if traceback {
            let _ = io.write_err(format!("abort: {err:?}\n"));
        } else {
            let _ = io.write_err(format!("abort: {err:#}\n"));
        }
    }
}

/// Get the traceback from anyhow and upload it
pub fn upload_traceback(err: &anyhow::Error, start_time_epoch_ms: u64) {
    if !tracing::enabled!(target: "errortrace", tracing::Level::INFO)
        || !tracing::enabled!(target: "errortracekey", tracing::Level::INFO)
    {
        return;
    }
    let hostname = match hostname::get_hostname() {
        Ok(s) => s,
        Err(_) => {
            return;
        }
    };
    let pid = std::process::id();
    let trace_key = format!(
        "flat/errortrace-{}-{}-{}{}",
        hostname,
        pid,
        start_time_epoch_ms / 1000,
        (start_time_epoch_ms % 1000) * 1000, // this is microseconds on python
    );
    let prefix = "abort: ";
    let traceback = format!("{prefix}{err:?}\n");
    let tk = trace_key.as_str();
    tracing::info!(target: "errortracekey", errortracekey=tk);
    tracing::info!(target: "errortrace", key=tk, payload=traceback);
    let start = prefix.len();
    let end = traceback.len().min(start + 500);
    let error_prefix = &traceback[start..end];
    tracing::info!(target: "error_prefix", error_prefix=error_prefix);
}

/// Optionally transform an error into something more friendly to the user.
pub fn triage_error(
    config: &dyn Config,
    cmd_err: anyhow::Error,
    command_name: Option<&str>,
) -> anyhow::Error {
    if types::errors::is_network_error(&cmd_err)
        && config
            .get_or_default("experimental", "network-doctor")
            .unwrap_or(false)
    {
        match network_doctor::Doctor::new().diagnose(config) {
            Ok(()) => cmd_err,
            Err(diagnosis) =>
            // TODO: colorize diagnosis, vary output by verbose/quiet
            {
                let message = format!(
                    "command failed due to network error\n\n{}\n\nDetails:\n\n{:?}\n\nOriginal error:\n\n{:?}\n",
                    diagnosis.treatment(config),
                    diagnosis,
                    cmd_err
                );
                DiagnosedNetworkError { diagnosis, message }.into()
            }
        }
    } else {
        if let Some(command_name) = command_name {
            if cmd_err.is::<FallbackToPython>()
                && config
                    .get_or_default::<Vec<String>>("commands", "force-rust")
                    .is_ok_and(|config| config.contains(&command_name.to_string()))
            {
                return anyhow::Error::new(FailedFallbackToPython);
            }
        }
        cmd_err
    }
}

#[cfg(all(test, feature = "eden"))]
mod tests {
    use std::io::Cursor;

    #[cfg(feature = "eden")]
    use thrift_types::edenfs as eden;

    use super::*;

    #[test]
    fn test_status_error_msg() {
        // Construct error and parameters
        let error_msg = "cannot compute status while a checkout is currently in progress";
        let expected_error = format!("abort: {error_msg}\n");

        let error: anyhow::Error = eden_clients::errors::GetScmStatusV2Error::ex(eden::EdenError {
            message: error_msg.to_string(),
            errorCode: Some(255),
            errorType: eden::EdenErrorType::CHECKOUT_IN_PROGRESS,
            ..Default::default()
        })
        .into();

        let tin = Cursor::new(Vec::new());
        let tout = Cursor::new(Vec::new());
        let terr = Cursor::new(Vec::new());
        let io = crate::io::IO::new(tin, tout, Some(terr));

        // Call print_error with error and in-memory IO stream
        print_error(&error, &io, None, false);

        // Make sure error message is formatted correctly.
        io.with_error(|e| {
            if let Some(actual_error_wrapped) = e {
                let any = actual_error_wrapped.as_any();
                if let Some(c) = any.downcast_ref::<std::io::Cursor<Vec<u8>>>() {
                    let actual_error = c.clone().into_inner();
                    assert_eq!(String::from_utf8(actual_error).unwrap(), expected_error);
                }
            }
        });
    }
}

#[cfg(test)]
mod error_category_tests {
    use super::*;

    #[test]
    fn test_error_category() {
        let repo_required: anyhow::Error = RepoRequired("/tmp".to_string()).into();
        assert_eq!(error_category(&repo_required), Some("usage"));

        let bad_option: anyhow::Error = ParseError::OptionNotRecognized {
            option_name: "--bogus".to_string(),
        }
        .into();
        assert_eq!(error_category(&bad_option), Some("usage"));

        let unexpected = anyhow::anyhow!("something broke");
        assert_eq!(error_category(&unexpected), None);
    }
}
