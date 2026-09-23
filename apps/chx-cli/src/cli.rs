//! The `chx` command line.
//!
//! One command so far: `chx migrate run`. Nothing in this module decides
//! anything about a migration. It parses arguments, calls
//! [`Migrator`](chx::migrate::Migrator), and prints the result.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use chx::client::Client;
use chx::error::Result;
use chx::migrate::{Applied, Migrator};

#[derive(Debug, Parser)]
#[command(name = "chx", version, about = "ClickHouse schema migrations")]
pub struct Cli {
    /// Log filter. `RUST_LOG` takes precedence when set.
    #[arg(long, global = true, env = "LOG_LEVEL")]
    pub log_level: Option<String>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Work with the migrations directory.
    #[command(subcommand)]
    Migrate(Migrate),
}

#[derive(Debug, Subcommand)]
pub enum Migrate {
    /// Apply every pending migration, in version order.
    Run(Connection),
}

#[derive(Debug, Args)]
pub struct Connection {
    /// `http[s]://user:password@host:port/database`.
    ///
    /// Read from the environment only, never from a `.env` file. A `.env`
    /// found by walking up from the working directory is a common way to run
    /// a migration against a database nobody meant to name.
    #[arg(long, env = "CLICKHOUSE_URL", hide_env_values = true)]
    pub database_url: String,

    /// Directory holding `<version>_<description>.sql` files.
    #[arg(long, default_value = "migrations")]
    pub source: PathBuf,
}

impl Cli {
    pub async fn run(self) -> Result<()> {
        match self.command {
            Command::Migrate(Migrate::Run(connection)) => migrate_run(connection).await,
        }
    }
}

async fn migrate_run(connection: Connection) -> Result<()> {
    // Files first: a malformed directory is reported without ever connecting.
    let migrator = Migrator::from_dir(&connection.source)?;
    let client = Client::from_url(&connection.database_url)?;

    let applied = migrator.run(&client, print_applied).await?;

    if applied.is_empty() {
        println!(
            "Up to date: {} migrations in {}",
            migrator.migrations().len(),
            connection.source.display()
        );
    }

    Ok(())
}

fn print_applied(applied: &Applied) {
    let resumed = match applied.resumed_at {
        0 => String::new(),
        n => format!(", resumed at statement {}", n + 1),
    };

    println!(
        "Applied {}/{} ({} statements{resumed}, {}ms)",
        applied.version,
        applied.description,
        applied.statements,
        applied.elapsed.as_millis()
    );
}
