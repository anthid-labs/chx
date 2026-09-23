use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::format::{DefaultFields, Format, Full, Writer};
use tracing_subscriber::fmt::time::FormatTime;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// Targets that are noisy at `debug`/`trace` and rarely useful. Extend this as
/// dependencies are added rather than lowering the global filter.
const SILENCED_TARGETS: &[&str] = &["hyper", "h2", "reqwest", "rustls"];

#[derive(Debug, Clone)]
pub struct TelemetryProviderConfig {
    pub app_name: String,
    /// Fallback filter directive when `RUST_LOG` is unset.
    pub log_level: Option<String>,
    /// Where formatted events are written.
    pub sink: LogSink,
}

/// Which standard stream log lines go to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LogSink {
    #[default]
    Stdout,
    /// Keeps stdout for the command's own output, so the lines saying what was
    /// applied are not interleaved with log lines.
    Stderr,
}

/// Owns process-wide telemetry state.
///
/// Only the `fmt` layer is wired up today. OTLP exporters (logger bridge,
/// tracer layer, meter provider) slot in at the marked point below; the struct and [`TelemetryProvider::shutdown`]
/// exist so adding them does not change any call site.
#[derive(Debug, Clone)]
pub struct TelemetryProvider {
    pub app_name: String,
}

struct Rfc3339WithNanos;

impl FormatTime for Rfc3339WithNanos {
    fn format_time(&self, w: &mut Writer<'_>) -> fmt::Result {
        // Human readable RFC3339
        let now = chrono::Utc::now();
        let ts_rfc = now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);

        // Raw nanos since UNIX epoch
        let since_epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time went backwards");

        let nanos_total =
            since_epoch.as_secs() as u128 * 1_000_000_000u128 + since_epoch.subsec_nanos() as u128;

        // Print both, separated by a space
        write!(w, "{ts_rfc} nanos={nanos_total}")
    }
}

/// The formatting shared by both sinks.
fn fmt_layer<S>() -> tracing_subscriber::fmt::Layer<S, DefaultFields, Format<Full, Rfc3339WithNanos>>
where
    S: tracing::Subscriber,
{
    tracing_subscriber::fmt::layer()
        .with_target(true)
        .with_thread_ids(true)
        .with_thread_names(true)
        .with_line_number(true)
        // do not log every span event
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::NONE)
        .with_level(true)
        .with_timer(Rfc3339WithNanos)
}

impl TelemetryProvider {
    /// Fails only on an unparseable filter directive, which is operator input;
    /// everything else here is infallible.
    pub fn new(config: TelemetryProviderConfig) -> Result<Self, String> {
        let fallback = config.log_level.as_deref().unwrap_or("info");

        // `RUST_LOG` takes precedence, so an invalid value there is reported
        // rather than silently falling back to `log_level`.
        let mut env_filter = match std::env::var(EnvFilter::DEFAULT_ENV) {
            Ok(directives) => EnvFilter::try_new(&directives)
                .map_err(|err| format!("invalid RUST_LOG {directives:?}: {err}"))?,
            Err(_) => EnvFilter::try_new(fallback)
                .map_err(|err| format!("invalid log level {fallback:?}: {err}"))?,
        };

        for target in SILENCED_TARGETS {
            env_filter = env_filter.add_directive(
                format!("{target}=off")
                    .parse()
                    .expect("silenced target is a valid directive"),
            );
        }

        // Built twice rather than behind a boxed writer: `fmt::layer()` is
        // generic over its writer, so the two differ in type and there is no
        // one value to assign.
        let try_init = match config.sink {
            LogSink::Stdout => {
                let fmt_layer = fmt_layer().with_writer(std::io::stdout);

                // OTLP layers go here: build the resource from
                // `config.app_name`, then `.with(logger_layer)` and
                // `.with(tracer_layer)` before the fmt layer.
                tracing_subscriber::registry()
                    .with(fmt_layer)
                    .with(env_filter)
                    .try_init()
            }
            LogSink::Stderr => {
                let fmt_layer = fmt_layer().with_writer(std::io::stderr);

                tracing_subscriber::registry()
                    .with(fmt_layer)
                    .with(env_filter)
                    .try_init()
            }
        };

        // A second init in the same process is expected under `cargo test`,
        // where several tests may each build a provider.
        if let Err(err) = try_init {
            tracing::warn!("tracing sub init err: {err:?}");
        }

        Ok(Self {
            app_name: config.app_name,
        })
    }

    /// Flush and tear down exporters.
    ///
    /// A no-op while logging is stdout-only, but call it on the shutdown path
    /// so wiring OTLP in later does not lose the final batch of spans.
    pub async fn shutdown(&self) {
        tracing::debug!(app_name = %self.app_name, "telemetry shutdown");
    }
}
