/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::io::Write;
use std::path::Path;
use std::process::Command;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use edenfs_commands::is_command_enabled_in_rust;
#[cfg(fbcode_build)]
use edenfs_telemetry::cli_usage::CliUsageSample;
#[cfg(fbcode_build)]
use edenfs_telemetry::send_edenfs_cli_usage;
#[cfg(fbcode_build)]
use edenfs_telemetry::telemetry_disabled;
#[cfg(windows)]
use edenfs_utils::execute_par;
#[cfg(windows)]
use edenfs_utils::strip_unc_prefix;
use fail::fail_point;
use fbinit::FacebookInit;
use testutil::failpoint;
use tracing_subscriber::filter::EnvFilter;

#[cfg(fbcode_build)]
use crate::error_report::ErrorReport;

#[cfg(fbcode_build)]
mod error_report;

#[cfg(not(fbcode_build))]
// For non-fbcode builds, CliUsageSample is not defined. Let's give it a dummy
// value so we can pass CliUsageSample through wrapper_main() and fallback().
struct CliUsageSample;

/// Value used in Python to indicate a command failed to parse
pub const PYTHON_EDENFSCTL_EX_USAGE: i32 = 64;

// Initialize OpenSSL before curl's `.init_array` constructor so OpenSSL does
// not register an atexit cleanup that frees its state under HTTP threads still
// running at exit. Prioritized constructors run before unprioritized ones.
#[cfg(target_os = "linux")]
#[used]
#[unsafe(link_section = ".init_array.00101")]
static INIT_OPENSSL: extern "C" fn() = init_openssl;

#[cfg(target_os = "linux")]
extern "C" fn init_openssl() {
    http_client::init_openssl();
}

fn python_fallback() -> Result<Command> {
    if let Ok(args) = std::env::var("EDENFSCTL_REAL") {
        // We might get a command starting with python.exe here instead of a simple path.
        let mut parts = args.split_ascii_whitespace();
        let binary = parts
            .next()
            .ok_or_else(|| anyhow!("invalid fallback environment variable: {args:?}"))?;
        let mut cmd = Command::new(binary);
        #[cfg(windows)]
        if binary.ends_with(".par") {
            cmd = execute_par(binary.into())?;
        }
        cmd.args(parts);
        tracing::debug!("Using binary set by EDENFSCTL_REAL {:?}", &cmd);
        return Ok(cmd);
    }

    let binary = std::env::current_exe().context("unable to retrieve path to the executable")?;
    let binary =
        std::fs::canonicalize(binary).context("unable to canonicalize path to the executable")?;
    #[cfg(windows)]
    let binary = strip_unc_prefix(binary);
    let libexec = binary.parent().ok_or_else(|| {
        anyhow!(
            "unable to retrieve parent directory to '{}'",
            binary.display()
        )
    })?;

    let executable = libexec.join(if cfg!(windows) {
        "edenfsctl.real.exe"
    } else {
        "edenfsctl.real"
    });
    tracing::debug!("trying {:?}", executable);
    if executable.exists() {
        return Ok(Command::new(executable));
    }

    // On Windows we are shipping the Python edenfsctl as PAR file that is not executable by itself
    #[cfg(windows)]
    {
        let par = libexec.join("edenfsctl.real.par");
        tracing::debug!("trying {:?}", par);

        if par.exists() {
            return execute_par(par);
        }
    }

    Err(anyhow!("unable to locate fallback binary"))
}

#[cfg_attr(
    not(fbcode_build),
    expect(unused_variables, reason = "only fbcode builds log telemetry")
)]
fn fallback(reason: Option<&clap::Error>, telemetry_sample: &mut CliUsageSample) -> Result<i32> {
    if std::env::var("EDENFS_LOG").is_ok() {
        setup_logging();
    }

    if let Some(reason) = reason {
        tracing::debug!(%reason, "falling back to Python");
    }

    let mut cmd = python_fallback()?;
    // skip arg0
    cmd.args(std::env::args().skip(1));

    // Users have PYTHONHOME and PYTHONPATH variables
    // that break the python version of edenfsctl since it will fail to
    // import modules. So, let's strip the PYTHONHOME and PYTHONPATH variables.
    cmd.env_remove("PYTHONHOME");
    cmd.env_remove("PYTHONPATH");
    #[cfg(fbcode_build)]
    cmd.envs(art_cli_lite_rs::outgoing_env("edenfsctl.real"));

    // Python cannot tell a dev build from a release one, so hand it our
    // decision instead of letting it log where we would not.
    #[cfg(fbcode_build)]
    let error_report = if telemetry_disabled() {
        cmd.env("EDENFS_NO_TELEMETRY", "1");
        None
    } else {
        ErrorReport::attach(&mut cmd)
            .inspect_err(|error| tracing::debug!(?error, "no error report for Python"))
            .ok()
    };

    #[cfg(fbcode_build)]
    telemetry_sample.set_rust_command(false);

    tracing::debug!("Falling back to {:?}", cmd);

    // Create a subprocess to run Python edenfsctl
    let status = cmd
        .status()
        .with_context(|| format!("failed to execute: {cmd:?}"))?;
    let code = status.code().unwrap_or(1);

    #[cfg(fbcode_build)]
    if code != 0
        && let Some(error_report) = error_report
    {
        error_report.record(telemetry_sample);
    }

    Ok(code)
}

/// Setup tracing logging. If we are in development mode, we use the fancier logger, otherwise a
/// simple logger for production use. Logs will be printined to stderr when `--debug` flag is
/// passed.
fn setup_logging() {
    let subscriber = tracing_subscriber::fmt();
    #[cfg(debug_assertions)]
    let subscriber = subscriber.pretty();
    let subscriber = subscriber.with_env_filter(EnvFilter::from_env("EDENFS_LOG"));

    if let Err(e) = subscriber.try_init() {
        eprintln!("Unable to initialize logger. Logging will be disabled. Cause: {e:?}");
    }
}

fn rust_main(cmd: edenfs_commands::MainCommand) -> Result<i32> {
    if cmd.debug {
        setup_logging();
    }
    cmd.run()
}

/// This function takes care of the fallback logic, hijack supported subcommand
/// to Rust implementation and forward the rest to Python.
#[allow(unused_variables)]
fn wrapper_main(telemetry_sample: &mut CliUsageSample) -> Result<i32> {
    if std::env::var("EDENFSCTL_ONLY_RUST").is_ok() {
        let cmd = edenfs_commands::MainCommand::try_parse();
        match cmd {
            Ok(cmd) => {
                #[cfg(fbcode_build)]
                {
                    // mark the command is triggered as a Rust command
                    telemetry_sample.set_rust_command(true);
                }
                rust_main(cmd)
            }
            // We failed to parse the command due to unknown argument/flag.
            // Print the clap-generated error message (which includes helpful usage info)
            // and exit with the same exit code that Python exits with for parse failures.
            Err(e) if e.kind() == clap::error::ErrorKind::UnknownArgument => {
                e.print().ok();
                exit_process(PYTHON_EDENFSCTL_EX_USAGE)
            }
            // Some other error occurred during parsing. Let's exit like normal
            // since we can't confirm it was due to an invalid command/arg.
            Err(e) => {
                e.print().ok();
                exit_process(e.exit_code())
            }
        }
    } else if std::env::var("EDENFSCTL_SKIP_RUST").is_ok() {
        fallback(None, telemetry_sample)
    } else {
        match edenfs_commands::MainCommand::try_parse() {
            // The command is defined in Rust, but check whether it's "enabled"
            // for Rust or else fall back to Python.
            Ok(cmd) => {
                if cmd.is_enabled() {
                    #[cfg(fbcode_build)]
                    {
                        // mark the command is triggered as a Rust command
                        telemetry_sample.set_rust_command(true);
                    }
                    rust_main(cmd)
                } else {
                    match fallback(None, telemetry_sample) {
                        // If the Python version of edenfsctl exited with a
                        // parse error, we should see if the Rust version
                        // exists. This helps prevent cases where rollouts
                        // are not working correctly.
                        Ok(PYTHON_EDENFSCTL_EX_USAGE) => {
                            #[cfg(fbcode_build)]
                            {
                                // We expected to use Python but we were forced
                                // to fall back to Rust. Something is wrong.
                                telemetry_sample.set_rust_fallback(true);
                                // mark the command is triggered as a Rust command
                                telemetry_sample.set_rust_command(true);
                            }
                            eprintln!(
                                "Failed to find Python implementation; falling back to Rust."
                            );
                            rust_main(cmd)
                        }
                        res => {
                            #[cfg(fbcode_build)]
                            telemetry_sample.set_rust_fallback(false);
                            res
                        }
                    }
                }
            }
            // If the command is defined in Rust, then --help will cause
            // try_parse() to return a DisplayHelp error.  In that case, we
            // should check whether the Rust version of the command is "enabled"
            // to decide whether to print Rust or Python help.
            //
            // If the command isn't defined in Rust then try_parse will fail
            // UnknownArgument (whether or not --help was requested) and we
            // should fall back to Python.
            //
            // Otherwise, we have encountered a different error in rust and should
            // display the rust error. We still return the python error code 64 to differentiate from
            // edenfsctl errors(2)
            Err(e) => {
                if e.kind() == clap::error::ErrorKind::DisplayHelp
                    || e.kind() == clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                {
                    if should_use_rust_help(std::env::args(), &None, &None).unwrap_or(false) {
                        e.print().ok();
                        exit_process(e.exit_code())
                    } else {
                        fallback(Some(&e), telemetry_sample)
                    }
                } else if e.kind() == clap::error::ErrorKind::UnknownArgument
                    || e.kind() == clap::error::ErrorKind::InvalidSubcommand
                {
                    // Failed to parse the command. We should try to fallback to Python.
                    fallback(Some(&e), telemetry_sample)
                } else {
                    // Rust command exists, but encountered a different parsing error. Print the error
                    e.print().ok();
                    Ok(PYTHON_EDENFSCTL_EX_USAGE)
                }
            }
        }
    }
}

fn should_use_rust_help<T>(
    args: T,
    etc_eden_dir_override: &Option<&Path>,
    experimental_commands_override: &Option<Vec<&str>>,
) -> Result<bool>
where
    T: Iterator<Item = String>,
{
    // This is gross, but clap v3 doesn't let us make --help a normal bool flag.
    // This means we can't successfully parse a command when --help is
    // requested.
    // But we know that if this function is called, the subcommand requested is
    // defined in Rust. So we can just manually parse the args by skipping any
    // options provided for 'edenfsctl' until we find the subcommand name.
    let mut subcommand_name = None;
    let mut skipping = false;
    for arg in args.skip(1) {
        if [
            "--config-dir".to_string(),
            "--etc-eden-dir".to_string(),
            "--home-dir".to_string(),
        ]
        .contains(&arg)
        {
            // handle skipping global option pair names
            skipping = true;
            continue;
        } else if skipping {
            // handle skipping global option pair values
            skipping = false;
            continue;
        } else if arg.starts_with("-") {
            // handle skipping global option flags, e.g. --version, -v, --debug
            continue;
        } else {
            subcommand_name = Some(arg);
            break;
        }
    }
    match subcommand_name {
        Some(name) => Ok(is_command_enabled_in_rust(
            &name,
            &etc_eden_dir_override.map(Path::to_owned),
            experimental_commands_override,
        )),
        None => Ok(false), // we are safe by always falling back to Python
    }
}

fn finish() {
    #[cfg(fbcode_build)]
    art_cli_lite_rs::finish_process();
}

/// Leaves the process without the exit-time teardown that `exit(3)` runs.
///
/// The atexit handlers of this binary destroy the C++ singletons (folly), whose
/// shutdown timer aborts when the host cannot create a thread, and whose
/// teardown races with the threads this CLI never joins: the use-case config
/// refresh in `edenfs_client::use_case` and the fbagent connection. Telemetry
/// and tracing are flushed before this point, so nothing is lost by skipping
/// the handlers.
fn exit_process(code: i32) -> ! {
    finish();
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    // SAFETY: both are plain C calls with no pointer arguments other than the
    // NULL that tells `fflush` to flush every open stream; `_exit` does not
    // return, so no Rust code observes the state left behind.
    unsafe {
        libc::fflush(std::ptr::null_mut());
        libc::_exit(code)
    }
}

#[cfg(fbcode_build)]
fn trace() {
    art_cli_lite_rs::trace_cli_for_process("cli.edenfsctl");
    art_cli_lite_rs_thrift::install_thrift_interceptor();
    http_client::Request::on_new_request(|request| {
        for (name, value) in art_cli_lite_rs::outgoing_http(request.ctx().url().as_str()) {
            request.set_header(name, value);
        }
    });
}

#[fbinit::main]
fn main(_fb: FacebookInit) {
    #[cfg(fbcode_build)]
    trace();
    // NOTE: if you are considering passing `FacebookInit` down, you may want to check
    // [`fbinit::expect_init`].
    #[cfg(fbcode_build)]
    let mut sample = CliUsageSample::build();

    #[cfg(not(fbcode_build))]
    let mut sample = CliUsageSample;

    let scenario = failpoint::setup_fail_points();

    fail_point!("edenfsctl:main");

    let code = match wrapper_main(&mut sample) {
        Ok(code) => Ok(code),
        Err(e) => {
            #[cfg(fbcode_build)]
            sample.set_exception(&e);
            Err(e)
        }
    };

    if let Some(scenario) = scenario {
        failpoint::teardown_fail_points(scenario);
    }

    #[cfg(fbcode_build)]
    {
        sample.set_exit_code(*code.as_ref().unwrap_or(&1));
        send_edenfs_cli_usage(sample.sample);
    }

    match code {
        Ok(code) => exit_process(code),
        Err(e) => {
            // The text Rust prints for an `Err` returned from `main`.
            eprintln!("Error: {e:?}");
            exit_process(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::io::Write;

    use anyhow::Result;
    use tempfile::TempDir;

    use super::should_use_rust_help;

    macro_rules! args {
        ( $( $x:expr ),* ) => (
            {
                let mut v = Vec::new();
                $(
                    v.push($x.to_string());
                )*
                v.into_iter()
            }
        );
    }

    #[test]
    fn test_should_use_rust_help() -> Result<()> {
        assert!(should_use_rust_help(
            args!["eden.exe", "minitop"],
            &None,
            &None
        )?);
        {
            let dir = TempDir::new()?;
            assert!(!should_use_rust_help(
                args!["eden.exe", "debug"],
                &Some(dir.path()),
                &Some(vec!["debug"])
            )?,);
            assert!(!should_use_rust_help(
                args![
                    "eden.exe",
                    "--config-dir",
                    "/home/scm/local/eden-dev-state",
                    "debug"
                ],
                &Some(dir.path()),
                &Some(vec!["debug"])
            )?,);
            assert!(!should_use_rust_help(
                args!["eden.exe", "debug"],
                &Some(dir.path()),
                &Some(vec!["debug"])
            )?,);
            assert!(should_use_rust_help(
                args!["eden.exe", "--debug", "debug"],
                &Some(dir.path()),
                &None
            )?,);
            assert!(!should_use_rust_help(
                args!["eden.exe", "--debug", "debug"],
                &Some(dir.path()),
                &Some(vec!["debug"])
            )?,);
        }
        {
            let dir = TempDir::new()?;
            let rollout_path = dir.path().join("edenfsctl_rollout.json");
            let mut rollout_file = File::create(rollout_path)?;
            writeln!(rollout_file, r#"{{"debug": true}}"#)?;

            assert!(should_use_rust_help(
                args!["eden.exe", "debug"],
                &Some(dir.path()),
                &Some(vec!["debug"])
            )?,);
        }
        Ok(())
    }

    #[cfg(fbcode_build)]
    const AGENT_CERT: &str = "-----BEGIN CERTIFICATE-----\nMIIDSzCCAjOgAwIBAgIUBZH3MYYDAtH3rw4GEoicCwbFgyowDQYJKoZIhvcNAQEL\nBQAwGDEWMBQGA1UEAwwNY2xpX2xpdGVfdGVzdDAgFw0yNjA5MDEwNTI2MDNaGA8y\nMTI2MDgwODA1MjYwM1owGDEWMBQGA1UEAwwNY2xpX2xpdGVfdGVzdDCCASIwDQYJ\nKoZIhvcNAQEBBQADggEPADCCAQoCggEBAKXL9A3z58/dnw9duMi1pjARHnDzMRRJ\n1dFIGDMqJVN8O0ngpQdl2DM4dN1i32JArfIquwULAC1izthntgFo0uXNSNpRaclj\n2oggdKXIwsWYKyK99q2dBi77kq5yVTLArBRVWrz1Q5/EMfyGDsxP/lsJ30Zy5tRo\nKXACiJ/Vgo8pScQ/8cBKvNtUkcNQCUwRqNMfj0gLNZHtu1H5O6h0ps3OTZVGrpyl\nzGO2QQAjt0dV1uxRcwtMhwfl11xjRo8vjFXCNmPtdsN00uUoyR0nfto1ZJ0zNCxB\nSnC0fnO0+wIIjL/U77IYEtVRb1s8pWfYuL87D77VLKAG00hN9kAQ74sCAwEAAaOB\nijCBhzBmBgNVHREEXzBdhkFtaWQ6Ly9QUk9EL1VTRVIvMTIzND9mbGF2b3I9eCZh\nZ2VudC5pZD1BR0VOVCUzYWNsYXVkZV9jb2RlJnRpZXI9eYYYbWlkOi8vUFJPRC9N\nQUNISU5FL2hvc3QwMB0GA1UdDgQWBBTWPEQZLYG+v7eXfhis6aMrM+bJHTANBgkq\nhkiG9w0BAQsFAAOCAQEApL49k77CDcILMCZd+aY2W70PG4OkbexS0ovBg5Zfok43\nGMWkG612VKBcJOpymvs2sscvdVZNpb/szTNMBtlKsVaS9vpr5fGzm+34OH+dUZ7o\nApz+0yPCYeHLvErrmXf9zWZJ8usFzipWgqFcFNpeyQ/7p02p89/fVnKrzWIYarZl\n/+6w9wq4XM0+SkapyRdW0Q+LqpvVIBQJUIOgY+pD1OMPJCIpuEwh5QJYc8C2Dae2\n8c+HXC0sz0TCyWiriU3ErKL9BQObu/wrDYbRt8YWOryHp0Zvx7M8p9JxuOFoEgJE\nox+ZgOXJBgPJwkSmw7K5ugovFBuStJFUB5jaM2FYrQ==\n-----END CERTIFICATE-----\n";

    #[cfg(fbcode_build)]
    fn http_headers_sent() -> Result<Vec<(String, String)>> {
        use std::io::BufRead;
        use std::io::BufReader;
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0")?;
        let url = format!("http://{}/", listener.local_addr()?);
        let server = std::thread::spawn(move || -> Result<Vec<(String, String)>> {
            let (stream, _) = listener.accept()?;
            let mut reader = BufReader::new(stream.try_clone()?);
            let mut headers = Vec::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line)?;
                let line = line.trim_end();
                if line.is_empty() {
                    break;
                }
                if let Some((name, value)) = line.split_once(": ") {
                    headers.push((name.to_lowercase(), value.to_owned()));
                }
            }
            (&stream).write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")?;
            Ok(headers)
        });
        super::trace();
        http_client::HttpClient::new().get(url.parse()?).send()?;
        server.join().expect("server thread should not panic")
    }

    #[cfg(fbcode_build)]
    fn agent_ids(cp2: &str, apc: &str) -> (Option<String>, Option<String>) {
        use base64::Engine;

        let decode = |value: &str| {
            base64::engine::general_purpose::STANDARD_NO_PAD
                .decode(
                    value
                        .trim_end_matches('=')
                        .replace('-', "+")
                        .replace('_', "/"),
                )
                .expect("carrier should be base64")
        };
        let cp2: baggage::Baggage =
            fbthrift::compact_protocol::deserialize(decode(cp2)).expect("cp2 should be a Baggage");
        let apc: context_prop_const::ContextpropCore::ArtilleryPropagatedContext =
            fbthrift::compact_protocol::deserialize(decode(apc))
                .expect("the V1 carrier should be an ArtilleryPropagatedContext");
        (cp2.agent_id, apc.agent_id)
    }

    #[cfg(fbcode_build)]
    #[fbinit::test]
    fn egress_carries_the_agents_trace_only_when_traced(_fb: fbinit::FacebookInit) -> Result<()> {
        const MODE: &str = "EDENFSCTL_EGRESS_TEST_MODE";
        if let Ok(mode) = std::env::var(MODE) {
            let headers = http_headers_sent()?;
            let header = |name: &str| {
                headers
                    .iter()
                    .find(|(key, _)| key == name)
                    .map(|(_, value)| value.clone())
            };
            let handle = edenfs_core::fsutil::handle_command(std::path::Path::new("mount"));
            let env = |name: &str| {
                handle
                    .get_envs()
                    .find(|(key, _)| *key == name)
                    .and_then(|(_, value)| value)
                    .map(|value| value.to_string_lossy().into_owned())
            };
            let expected = (
                Some("AGENT:claude_code".to_owned()),
                Some("AGENT:claude_code".to_owned()),
            );
            for (egress, cp2, apc) in [
                (
                    "http",
                    header("cp2"),
                    header("propagated-artillery-trace-ids"),
                ),
                (
                    "handle.exe",
                    env("CONTEXTPROP_BAGGAGE"),
                    env("ARTILLERY_CONTEXT"),
                ),
            ] {
                match (mode == "traced", cp2, apc) {
                    (true, Some(cp2), Some(apc)) => {
                        assert_eq!(agent_ids(&cp2, &apc), expected, "{egress} names the agent")
                    }
                    (false, None, None) => {}
                    (traced, cp2, apc) => {
                        panic!("{mode} {egress}: traced={traced} cp2={cp2:?} apc={apc:?}")
                    }
                }
            }
            return Ok(());
        }

        let dir = TempDir::new()?;
        let cert = dir.path().join("cert.pem");
        std::fs::write(&cert, AGENT_CERT)?;
        let control = |name: &str, overrides: &str| -> Result<std::path::PathBuf> {
            let path = dir.path().join(format!("{name}.json"));
            std::fs::write(
                &path,
                format!(
                    r#"{{"transports": ["none"], "observe_sample_rate": 0, "killed": false, "overrides": {{"edenfsctl": {{{overrides}}}}}}}"#
                ),
            )?;
            Ok(path)
        };
        let cases = [
            ("traced", Some(control("traced", r#""sample_rate": 1.0"#)?)),
            ("human", None),
            ("killed", Some(control("killed", r#""killed": true"#)?)),
            (
                "sampled_out",
                Some(control("sampled_out", r#""sample_rate": 0.0"#)?),
            ),
        ];
        for (mode, control) in cases {
            let mut child = std::process::Command::new(std::env::current_exe()?);
            child
                .args([
                    "--exact",
                    "tests::egress_carries_the_agents_trace_only_when_traced",
                ])
                .env(MODE, mode)
                .env("EDENFS_NO_TELEMETRY", "1");
            for name in [
                "TRACEPARENT",
                "CONTEXTPROP_BAGGAGE",
                "ARTILLERY_CONTEXT",
                "CODING_AGENT_METADATA",
                "META_3PAI_INVOCATION_ID",
                "AGENT",
                "CONFUCIUS_IN_NAMESPACE",
                "CLAUDE_CODE_CURRENT_SESSION_ID",
                "CC_TRACE_DIR",
                "ARTILLERY_KILLSWITCH_PATH",
                "THRIFT_TLS_CL_CERT_PATH",
            ] {
                child.env_remove(name);
            }
            if let Some(control) = control {
                child
                    .env("ARTILLERY_KILLSWITCH_PATH", control)
                    .env("THRIFT_TLS_CL_CERT_PATH", &cert)
                    .env("CODING_AGENT_METADATA", "id=claude_code")
                    .env(
                        "TRACEPARENT",
                        "00-5ad4f2c7a3e14b8d1f2e3d4c5b6a7940-b7ad6b7169203331-01",
                    );
            }
            let status = child.status()?;
            assert!(status.success(), "{mode} run: {status:?}");
        }
        Ok(())
    }
}
