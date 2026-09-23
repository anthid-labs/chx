//! Process-wide logging setup.
//!
//! Lives in the CLI rather than the library because it is a property of a
//! *process*: it installs a global subscriber, which is a decision only the
//! binary at the top of the stack gets to make. A library that did this would
//! fight whatever its host application had already set up. [`chx`] emits
//! `tracing` events and leaves collecting them to the caller.

pub mod provider;

pub use provider::{LogSink, TelemetryProvider, TelemetryProviderConfig};

/// Builds the process-wide telemetry provider, choosing where the logs go.
///
/// `log_level` is the fallback filter used when `RUST_LOG` is not set; pass
/// `None` to fall back to `info`. Returns the operator-facing message when
/// either filter is unparseable.
pub fn setup_telemetry_client_to(
    app_name: &str,
    log_level: Option<&str>,
    sink: LogSink,
) -> Result<TelemetryProvider, String> {
    let config = TelemetryProviderConfig {
        app_name: app_name.to_string(),
        log_level: log_level.map(str::to_string),
        sink,
    };

    TelemetryProvider::new(config)
}
