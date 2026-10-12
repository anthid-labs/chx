//! ClickHouse for Rust, over the HTTP interface.
//!
//! Three things, each usable without the others:
//!
//! - **Queries.** [`query()`], [`query_as()`] and [`query_scalar()`] bind values to
//!   the `{name:Type}` parameters a query declares and read the result as
//!   typed rows over `RowBinaryWithNamesAndTypes`. With the `macros` feature,
//!   [`query!`], [`query_as!`], [`query_scalar!`] and [`query_file!`] check
//!   the SQL against a database at compile time, or against the `.chx`
//!   cache that `chx prepare` writes, and [`query_unchecked!`] skips the
//!   check.
//! - **Bulk inserts.** [`Client::insert`] opens an `INSERT ... FORMAT
//!   RowBinary`, [`Insert::write`](insert::Insert::write) encodes a row into
//!   it, and one request carries the batch.
//! - **Migrations.** [`migrate`] applies a directory of numbered SQL files
//!   with a checksum of each recorded in the database, built around the fact
//!   that ClickHouse DDL is not transactional. The `chx` command wraps it.
//!
//! For the command line tool, install `chx-cli`. This crate is the engine
//! underneath it.
//!
//! # Queries
//!
//! ```no_run
//! # async fn example() -> chx::error::Result<()> {
//! use chx::{Client, FromRow};
//!
//! #[derive(FromRow)]
//! struct Trade {
//!     id: u64,
//!     venue: String,
//!     px: chx::types::Decimal,
//! }
//!
//! let client = Client::from_url("http://default:@localhost:8123/analytics")?;
//! let trades: Vec<Trade> = chx::query_as("SELECT id, venue, px FROM trades WHERE venue = {v:String}")
//!     .bind("v", "NYSE")
//!     .fetch_all(&client)
//!     .await?;
//! # Ok(())
//! # }
//! ```
//!
//! With `CLICKHOUSE_URL` set, or a `.chx` directory from `chx prepare`, the
//! same query checks its columns and parameters when it compiles:
//!
//! ```ignore
//! let trades = chx::query!("SELECT id, venue, px FROM trades WHERE venue = {v:String}", v = venue)
//!     .fetch_all(&client)
//!     .await?;
//! println!("{}", trades[0].venue);
//! ```
//!
//! # Why ClickHouse needs its own migration tool
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

#![warn(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]

// The macros generate paths that start `::chx::`. This makes that name mean
// this crate from inside it too, so its own tests and doc examples can use
// them.
extern crate self as chx;

pub mod client;
pub mod decode;
pub mod encode;
pub mod error;
pub mod insert;
pub mod migrate;
pub mod query;
pub mod row;
pub mod types;

#[cfg(test)]
#[allow(missing_docs)]
pub(crate) mod testing;

pub use client::Client;
pub use decode::Decode;
pub use encode::{BindAs, Encode};
pub use error::{Error, Result};
pub use insert::{Insert, IntoRow};
pub use query::{Query, QueryAs, QueryScalar, query, query_as, query_scalar};
pub use row::{Column, FromRow, Row};
pub use types::{Decimal, Type};

#[cfg(feature = "macros")]
pub use chx_macros::{FromRow, query, query_as, query_file, query_scalar, query_unchecked};

/// Re-exports for the code the macros generate. Not part of the public API.
#[doc(hidden)]
pub mod __private {
    #[cfg(feature = "chrono")]
    pub use chrono;
    #[cfg(feature = "uuid")]
    pub use uuid;

    /// Returns its argument, having required it to bind as `Target`. The
    /// generated code wraps every macro argument in this.
    pub fn bind_as<Target: ?Sized, V: ?Sized + crate::BindAs<Target>>(value: &V) -> &V {
        value
    }
}

#[cfg(all(test, feature = "macros"))]
mod tests {
    use crate::testing::{Fake, Response, row_binary};

    /// `query_unchecked!` needs no database to compile, so it can be
    /// exercised here: the arguments are bound by name through `bind_as`.
    #[tokio::test]
    async fn an_unchecked_macro_binds_its_arguments_by_name() {
        let fake = Fake::always(Response::ok(row_binary(
            &[("n", "UInt64")],
            &7u64.to_le_bytes(),
        )));
        let limit = 1u64;

        let rows = crate::query_unchecked!(
            "SELECT n FROM {t:Identifier} WHERE v = {venue:String} LIMIT {limit:UInt64}",
            t = "numbers",
            venue = String::from("NYSE"),
            limit,
        )
        .fetch_all(&fake.client())
        .await
        .unwrap();

        assert_eq!(rows[0].try_get::<u64, _>("n").unwrap(), 7);
        let request = fake.last();
        assert_eq!(request.param("param_t"), Some("numbers"));
        assert_eq!(request.param("param_venue"), Some("NYSE"));
        assert_eq!(request.param("param_limit"), Some("1"));
    }

    #[derive(Debug, PartialEq, crate::FromRow)]
    struct Named {
        n: u64,
        #[chx(rename = "label")]
        name: String,
        #[chx(default)]
        missing: Option<u8>,
    }

    #[derive(Debug, PartialEq, crate::FromRow)]
    struct Positional(u64, String);

    #[tokio::test]
    async fn derived_from_row_reads_by_name_and_by_position() {
        let mut row = 7u64.to_le_bytes().to_vec();
        row.extend_from_slice(b"\x03abc");
        let fake = Fake::always(Response::ok(row_binary(
            &[("n", "UInt64"), ("label", "String")],
            &row,
        )));
        let client = fake.client();

        let named: Named = crate::query_as("SELECT 1")
            .fetch_one(&client)
            .await
            .unwrap();
        assert_eq!(
            named,
            Named {
                n: 7,
                name: "abc".to_string(),
                missing: None
            }
        );

        let positional: Positional = crate::query_as("SELECT 1")
            .fetch_one(&client)
            .await
            .unwrap();
        assert_eq!(positional, Positional(7, "abc".to_string()));
    }

    /// `default` covers a missing column, not a column of the wrong type.
    #[tokio::test]
    async fn a_defaulted_field_still_fails_on_a_type_it_cannot_read() {
        #[derive(Debug, crate::FromRow)]
        struct Strict {
            #[chx(default)]
            #[allow(dead_code)]
            n: Option<String>,
        }
        let fake = Fake::always(Response::ok(row_binary(
            &[("n", "UInt64")],
            &7u64.to_le_bytes(),
        )));

        let err = crate::query_as::<Strict>("SELECT 1")
            .fetch_one(&fake.client())
            .await
            .unwrap_err();

        assert!(matches!(err, crate::Error::Decode(_)), "{err}");
    }
}
