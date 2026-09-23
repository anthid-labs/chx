//! ClickHouse schema migrations.
//!
//! A directory of numbered SQL files, applied in order, with a checksum of each
//! one recorded in the database so a file that changes after it ran is refused
//! rather than silently diverging from what the schema actually is.
//!
//! For the command line tool, install `chx-cli`. This crate is the engine
//! underneath it.
//!
//! # Why ClickHouse needs its own tool
//!
//! ClickHouse has no transactional DDL. A migration of five statements that
//! fails on the third leaves the first two applied, and a tool that records a
//! migration only once it completes has no idea they ran. The next attempt
//! fails on the first `CREATE TABLE`, and the operator is left working out by
//! hand which half of the file is live.
//!
//! chx records progress after every statement instead. A failed migration
//! stays in the history as incomplete, with the number of statements that
//! landed, and the next run resumes it at the statement that failed. See
//! [`migrate`].
//!
//! # Getting started
//!
//! ```no_run
//! # async fn example() -> chx::error::Result<()> {
//! use chx::client::Client;
//! use chx::migrate::Migrator;
//!
//! let client = Client::from_url("http://default:@localhost:8123/analytics")?;
//! let migrator = Migrator::from_dir("migrations")?;
//!
//! for applied in migrator.run(&client, |_| {}).await? {
//!     println!("applied {} {}", applied.version, applied.description);
//! }
//! # Ok(())
//! # }
//! ```

pub mod client;
pub mod error;
pub mod migrate;
