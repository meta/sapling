/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::io::IsTerminal;
use std::str::FromStr;

use anyhow::Result;
use cached_config::ConfigStore;
use clap::ArgAction;
use clap::Args;
use clap::ValueEnum;
use clap::builder::PossibleValuesParser;
use fbinit::FacebookInit;
use observability::ObservabilityContext;
use panichandler::Fate;
use tracing::Event;
use tracing::Subscriber;
use tracing_glog::FormatLevelChars;
use tracing_glog::Glog;
use tracing_glog::GlogFields;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::Layer;
use tracing_subscriber::filter;
use tracing_subscriber::filter::Directive;
use tracing_subscriber::fmt::FmtContext;
use tracing_subscriber::fmt::FormatEvent;
use tracing_subscriber::fmt::FormatFields;
use tracing_subscriber::fmt::FormattedFields;
use tracing_subscriber::fmt::format;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::LookupSpan;

const LOG_LEVEL_NAMES: [&str; 6] = ["OFF", "ERROR", "WARN", "INFO", "DEBUG", "TRACE"];
const DEFAULT_TRACING_LEVEL: filter::LevelFilter = filter::LevelFilter::INFO;

/// Command line arguments for tracing/logging configuration
#[derive(Args, Debug)]
pub struct LoggingArgs {
    /// Configure tracing to output in test format
    #[clap(long)]
    pub tracing_test_format: bool,

    /// Print debug output
    ///
    /// Equivalent to --log-level=DEBUG.
    #[clap(long, global = true)]
    pub debug: bool,

    /// Log level to use
    #[clap(long, conflicts_with = "debug", value_parser = PossibleValuesParser::new(&LOG_LEVEL_NAMES), global = true, overrides_with = "log_level")]
    pub log_level: Option<String>,

    /// Log level to use for C++ logging
    #[clap(long, conflicts_with = "debug", value_parser = PossibleValuesParser::new(&LOG_LEVEL_NAMES), global = true, overrides_with = "cxx_log_level")]
    pub cxx_log_level: Option<String>,

    /// Deprecated: Logview category (no longer used, accepted for compatibility)
    #[clap(long, hide = true)]
    pub logview_category: Option<String>,

    /// Fate of the process when a panic happens
    #[clap(long, value_enum, default_value_t=PanicFate::Abort)]
    pub panic_fate: PanicFate,

    /// Whether to instantiate ObservabilityContext::Dynamic, which reads
    /// logging levels from configerator. Overwrites --log-level or --debug
    // For compatibility with existing usage, this arg takes value,
    // for example `--with-dynamic-observability=true`.
    #[clap(long, default_value_t = false, value_name = "BOOL", action = ArgAction::Set)]
    pub with_dynamic_observability: bool,
}

#[derive(ValueEnum, Clone, Copy, Debug)]
#[clap(rename_all = "lower")]
pub enum PanicFate {
    None,
    Continue,
    Exit,
    Abort,
}

// Override trace and debug levels use to use `V` as this is what
// is expected by the log parser.  See: https://fburl.com/code/qeburjh0
const GLOG_FORMAT_LEVEL_CHARS: FormatLevelChars = FormatLevelChars {
    trace: "V",
    debug: "V",
    ..FormatLevelChars::const_default()
};

impl LoggingArgs {
    fn setup_panic_handler(&self) {
        // Set the panic handler up here. Not really relevant to logger other than it emits output
        // when things go wrong. This writes directly to stderr as coredumper expects.
        // TODO: separate the panic handler out from logging
        let fate = match self.panic_fate {
            PanicFate::None => None,
            PanicFate::Continue => Some(Fate::Continue),
            PanicFate::Exit => Some(Fate::Exit(101)),
            PanicFate::Abort => Some(Fate::Abort),
        };
        if let Some(fate) = fate {
            panichandler::set_panichandler(fate);
        }
    }

    fn setup_tracing(&self, fb: FacebookInit) -> Result<()> {
        self.setup_panic_handler();

        let default_level = if self.debug {
            Some(filter::LevelFilter::DEBUG)
        } else {
            match &self.log_level {
                Some(log_level_str) => Some(filter::LevelFilter::from_str(log_level_str)?),
                None => None,
            }
        };

        #[cfg(fbcode_build)]
        if let Some(log_level_str) = &self.cxx_log_level {
            let level = filter::LevelFilter::from_str(log_level_str)?;
            crate::glog::set_glog_log_level(fb, level.into())?;
        } else {
            crate::glog::set_glog_log_level(
                fb,
                default_level.unwrap_or(DEFAULT_TRACING_LEVEL).into(),
            )?;
        }
        #[cfg(not(fbcode_build))]
        let _ = fb;

        // Make sure noisy dependencies don't pollute the logs
        let mut builtins: Vec<Directive> = vec![
            "fb303_core::server=WARN".parse()?,
            "overload_protection::capacity=WARN".parse()?,
            "hyper::proto=WARN".parse()?,
            "runtime=WARN".parse()?,
            "tokio=WARN".parse()?,
            "edenapi::client=WARN".parse()?,
            // Artillery annotations are trace data, not log lines. The target
            // filter below is the Artillery layer's own; this is the log one.
            format!("{}=OFF", artillery_http_ext::TRACING_TARGET).parse()?,
        ];

        if self.tracing_test_format {
            // Suppress startup info logs from mononoke_configs in test mode.
            // These are useful in production but noise in integration tests
            // where every foreground CLI command would emit them.
            builtins.push("mononoke_configs=WARN".parse()?);

            // Suppress futures_watchdog "Slow poll()" warnings in test mode.
            // The 500ms default poll threshold is borderline for blobstore and
            // repo construction on loaded devservers/ondemand hosts, so these
            // fire nondeterministically and break `.t` output comparisons.
            builtins.push("futures_watchdog=ERROR".parse()?);
        }

        let filter = match std::env::var("RUST_LOG") {
            Ok(env) if !env.is_empty() => {
                // EnvFilter doesn't offer an API that lets us merge our own directives with the ones from
                // the environment; it's either/or. Let's just concatenate them manually.
                let directives = builtins
                    .iter()
                    .map(|d| d.to_string())
                    .collect::<Vec<_>>()
                    .join(",");
                let all = {
                    // The order of precedence is: built-in, environment, command-line
                    let mut all = vec![directives, env];
                    if let Some(default_level) = default_level {
                        all.push(default_level.to_string());
                    }
                    all
                }
                .join(",");

                EnvFilter::builder()
                    .with_default_directive(DEFAULT_TRACING_LEVEL.into())
                    .parse(all)
            }
            _ => Ok({
                // The order of precedence is: built-in, command-line
                if let Some(default_level) = default_level {
                    builtins.push(default_level.into());
                }

                builtins.into_iter().fold(
                    EnvFilter::builder()
                        .with_default_directive(DEFAULT_TRACING_LEVEL.into())
                        .parse("")?,
                    |filter, directive| filter.add_directive(directive),
                )
            }),
        }?;

        // Called per branch rather than hoisted: the branches build different
        // subscriber types, and the layer is generic over that type.
        if self.tracing_test_format {
            let event_format = TestFormatter {};

            let log_layer = tracing_subscriber::fmt::layer()
                .event_format(event_format)
                .with_writer(std::io::stderr)
                .with_ansi(false)
                .with_filter(filter);

            let subscriber = tracing_subscriber::registry()
                .with(log_layer)
                .with(artillery_http_ext::tracing_layer(fb));
            tracing::subscriber::set_global_default(subscriber)?;
        } else {
            let event_format = Glog::default()
                .with_timer(tracing_glog::LocalTime::default())
                .with_format_level_chars(&GLOG_FORMAT_LEVEL_CHARS)
                .with_span_names(false)
                .with_target(true);
            let log_layer = tracing_subscriber::fmt::layer()
                .event_format(event_format)
                .fmt_fields(GlogFields::default())
                .with_writer(std::io::stderr)
                .with_ansi(std::io::stderr().is_terminal())
                .with_filter(filter);

            let subscriber = tracing_subscriber::registry()
                .with(log_layer)
                .with(artillery_http_ext::tracing_layer(fb));
            tracing::subscriber::set_global_default(subscriber)?;
        }

        // Configure legacy logging (at ERROR or above) to go to tracing.
        let stdlog_level_filter = log::LevelFilter::Error;
        tracing_log::LogTracer::builder()
            .with_max_level(stdlog_level_filter)
            .init()?;
        log::set_max_level(stdlog_level_filter);

        Ok(())
    }

    pub fn create_observability_context(
        &self,
        config_store: &ConfigStore,
    ) -> Result<ObservabilityContext> {
        if self.with_dynamic_observability {
            Ok(ObservabilityContext::new(config_store)?)
        } else {
            Ok(ObservabilityContext::new_static(config_store))
        }
    }

    pub fn setup_logging(&self, fb: FacebookInit) -> Result<()> {
        self.setup_tracing(fb)
    }
}

pub struct TestFormatter;

impl<S, N> FormatEvent<S, N> for TestFormatter
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: format::Writer<'_>,
        event: &Event<'_>,
    ) -> std::fmt::Result {
        write!(&mut writer, "[{}] ", event.metadata().level())?;

        if let Some(scope) = ctx.event_scope() {
            for span in scope.from_root() {
                // Ignore well-known spans with per-request data that is not useful in tests.
                if span.name() == "request_info" {
                    continue;
                }

                write!(writer, "[{}", span.name())?;

                let ext = span.extensions();
                if let Some(fields) = &ext.get::<FormattedFields<N>>() {
                    if !fields.is_empty() {
                        write!(writer, "{{{fields}}}")?;
                    }
                }

                write!(writer, "] ")?;
            }
        }

        ctx.field_format().format_fields(writer.by_ref(), event)?;

        writeln!(writer)
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use mononoke_macros::mononoke;

    use super::*;

    #[derive(Parser, Debug)]
    struct TestCli {
        #[clap(flatten)]
        logging: LoggingArgs,
    }

    #[mononoke::test]
    fn repeated_log_level_last_wins() {
        let cli = TestCli::try_parse_from(["test", "--log-level", "INFO", "--log-level", "OFF"])
            .expect("repeated --log-level should parse");
        assert_eq!(cli.logging.log_level.as_deref(), Some("OFF"));
    }

    #[mononoke::test]
    fn repeated_cxx_log_level_last_wins() {
        let cli =
            TestCli::try_parse_from(["test", "--cxx-log-level", "INFO", "--cxx-log-level", "OFF"])
                .expect("repeated --cxx-log-level should parse");
        assert_eq!(cli.logging.cxx_log_level.as_deref(), Some("OFF"));
    }
}
