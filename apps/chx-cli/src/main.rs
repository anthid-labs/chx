//! The `chx` command.
//!
//! Thin by design: everything that reads, checks or runs a migration lives in
//! the [`chx`] library, and this crate is the argument parsing, the logging
//! setup and the exit code around it.

mod cli;
mod telemetry;

use chx::error::{Error, Result};
use clap::Parser;

use crate::cli::Cli;
use crate::telemetry::{LogSink, setup_telemetry_client_to};

/// Returning `Result` from `main` would print the error's `Debug` form, because
/// that is what `Termination` uses. Handling it here prints `Display` instead,
/// which is what the messages were written for.
#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("chx: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();

    // Logs go to stderr so stdout carries only what was applied. The default
    // is `warn` rather than `info`: a migration run is read by a person, and a
    // timestamped line per statement buries the one line per migration that
    // matters. `--log-level info` brings them back.
    let telemetry = setup_telemetry_client_to(
        env!("CARGO_PKG_NAME"),
        Some(cli.log_level.as_deref().unwrap_or("warn")),
        LogSink::Stderr,
    )
    .map_err(Error::Internal)?;

    let outcome = cli.run().await;

    telemetry.shutdown().await;

    outcome
}
