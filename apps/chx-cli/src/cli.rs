//! The `chx` command line.
//!
//! Four commands: `chx migrate run`; `chx migrate import`, for a database
//! built by hand from the same files; `chx migrate unlock`, for the lock a
//! dead run left behind; and `chx prepare`, which writes the `.chx` cache
//! the query macros compile against. Nothing in this module decides anything
//! about a migration or a query. It parses arguments, calls the library, and
//! prints the result.

use std::path::PathBuf;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};

use chx::client::Client;
use chx::error::Result;
use chx::migrate::{self, Applied, DEFAULT_LOCK_TIMEOUT, Imported, Migrator};

use crate::prepare;

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

    /// Check every `query!` in the crate against the database and write the
    /// results to `.chx`, so later builds need no database.
    ///
    /// Runs `cargo check` with the macros told to save what they learn, then
    /// replaces `.chx/query-*.json` with the result. Arguments after `--`
    /// go to `cargo check`, for example `-- --workspace`.
    Prepare(PrepareArgs),
}

#[derive(Debug, Args)]
pub struct PrepareArgs {
    /// `http[s]://user:password@host:port/database`, with the schema the
    /// queries run against already applied.
    #[arg(long, env = "CLICKHOUSE_URL", hide_env_values = true)]
    pub clickhouse_url: String,

    /// Fail if `.chx` is not exactly what the queries would write now,
    /// instead of writing it. For CI.
    #[arg(long)]
    pub check: bool,

    /// Arguments passed through to `cargo check`.
    #[arg(last = true)]
    pub cargo_args: Vec<String>,
}

#[derive(Debug, Subcommand)]
pub enum Migrate {
    /// Apply every pending migration, in version order.
    Run(RunArgs),

    /// Record migrations as applied without running them.
    ///
    /// For a database whose schema was built by hand from these same files.
    /// Only adds rows: a version already recorded with the same checksum is
    /// skipped, and one recorded differently stops the import before anything
    /// is written.
    Import(ImportArgs),

    /// Release the lock left by a run that died holding it.
    ///
    /// Only once that run is known to be gone: releasing the lock of a live
    /// run lets a second one start on top of it.
    Unlock(Connection),
}

#[derive(Debug, Args)]
pub struct RunArgs {
    #[command(flatten)]
    pub connection: Connection,

    /// Directory holding `<version>_<description>.sql` files.
    #[arg(long, default_value = "migrations")]
    pub source: PathBuf,

    /// Seconds to wait for another run to release the lock. 0 tries once.
    #[arg(long, env = "CHX_LOCK_TIMEOUT", default_value_t = DEFAULT_LOCK_TIMEOUT.as_secs())]
    pub lock_timeout: u64,
}

#[derive(Debug, Args)]
pub struct ImportArgs {
    #[command(flatten)]
    pub run: RunArgs,

    /// Import up to and including this version, and leave later files for
    /// `chx migrate run` to apply. Every file when omitted.
    #[arg(long)]
    pub through: Option<i64>,
}

#[derive(Debug, Args)]
pub struct Connection {
    /// `http[s]://user:password@host:port/database`.
    ///
    /// From this flag, else `CLICKHOUSE_URL` in the environment, else the
    /// nearest `.env` above the working directory, as sqlx-cli resolves it.
    #[arg(long, env = "CLICKHOUSE_URL", hide_env_values = true)]
    pub clickhouse_url: String,

    /// Cluster to keep the history table on, so every node sees one history.
    ///
    /// Tried, not required: if the cluster cannot hold a replicated table
    /// (no Keeper, no `{replica}` macro, no such cluster) chx warns and keeps
    /// the history on the connected server. Not needed for a `Replicated`
    /// database or ClickHouse Cloud, which replicate it already.
    #[arg(long, env = "CLICKHOUSE_CLUSTER")]
    pub cluster: Option<String>,
}

impl Cli {
    pub async fn run(self) -> Result<()> {
        match self.command {
            Command::Migrate(Migrate::Run(args)) => migrate_run(args).await,
            Command::Migrate(Migrate::Import(args)) => migrate_import(args).await,
            Command::Migrate(Migrate::Unlock(connection)) => migrate_unlock(connection).await,
            Command::Prepare(args) => prepare::run(args),
        }
    }
}

/// Files first: a malformed directory is reported without ever connecting.
fn migrator(args: &RunArgs) -> Result<(Migrator, Client)> {
    let mut migrator =
        Migrator::from_dir(&args.source)?.lock_timeout(Duration::from_secs(args.lock_timeout));
    if let Some(cluster) = &args.connection.cluster {
        migrator = migrator.on_cluster(cluster);
    }
    let client = Client::from_url(&args.connection.clickhouse_url)?;

    Ok((migrator, client))
}

async fn migrate_run(args: RunArgs) -> Result<()> {
    let (migrator, client) = migrator(&args)?;

    let applied = migrator.run(&client, print_applied).await?;

    if applied.is_empty() {
        println!(
            "Up to date: {} migrations in {}",
            migrator.migrations().len(),
            args.source.display()
        );
    }

    Ok(())
}

async fn migrate_import(args: ImportArgs) -> Result<()> {
    let (migrator, client) = migrator(&args.run)?;

    let imported = migrator.import(&client, args.through).await?;

    for Imported {
        version,
        description,
        statements,
    } in &imported
    {
        println!("Imported {version}/{description} ({statements} statements, not run)");
    }
    if imported.is_empty() {
        match args.through {
            Some(through) => {
                println!("Nothing to import: every file up to {through} is already recorded")
            }
            None => println!("Nothing to import: every file is already recorded"),
        }
    }

    Ok(())
}

async fn migrate_unlock(connection: Connection) -> Result<()> {
    let client = Client::from_url(&connection.clickhouse_url)?;

    match migrate::unlock(&client, connection.cluster.as_deref()).await? {
        Some(holder) => println!(
            "Released the lock held by {} since {} UTC",
            holder.description, holder.since
        ),
        None => println!("No lock was held"),
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
