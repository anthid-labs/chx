//! The one error type, and the `Result` alias over it.
//!
//! A caller usually matches on a few variants and passes the rest through:
//!
//! ```no_run
//! # async fn example(client: &chx::Client) -> chx::Result<()> {
//! use chx::Error;
//!
//! match chx::query("SELECT id FROM trades WHERE id = {id:UInt64}")
//!     .bind("id", &7u64)
//!     .fetch_one(client)
//!     .await
//! {
//!     Ok(row) => println!("{}", row.try_get::<u64, _>("id")?),
//!     Err(Error::RowNotFound) => println!("no such trade"),
//!     // 60 is UNKNOWN_TABLE. Match the code, never the message text.
//!     Err(Error::ClickHouse { code: Some(60), .. }) => println!("run the migrations first"),
//!     Err(other) => return Err(other),
//! }
//! # Ok(())
//! # }
//! ```

use std::io;

use thiserror::Error as ThisError;

/// A `Result` whose error is [`Error`].
pub type Result<T> = std::result::Result<T, Error>;

/// Errors surfaced by chx.
///
/// Variants are the categories a caller can act on, not a mirror of every
/// underlying failure. The split that matters most is between
/// [`Error::History`], where the files and the database disagree and nothing
/// was run, and [`Error::Statement`], where a migration was running and
/// stopped partway.
#[derive(Debug, ThisError)]
pub enum Error {
    /// A path that was expected to exist does not.
    #[error("not found: {0}")]
    NotFound(String),

    /// The connection URL is missing or malformed.
    #[error("invalid database url: {0}")]
    Url(String),

    /// The migrations directory holds a file chx cannot use: a name with no
    /// version, a duplicate version, a down migration.
    #[error("invalid migration source: {0}")]
    Source(String),

    /// The recorded history disagrees with the files on disk: a checksum
    /// changed, a file that ran is gone, or a new file is numbered below one
    /// that already ran.
    ///
    /// Always raised before any statement executes. Refusing is the point: a
    /// tool that runs anyway leaves a schema nobody can reconstruct from the
    /// repository.
    #[error("migration history: {0}")]
    History(String),

    /// ClickHouse answered, and the answer was an error.
    #[error("clickhouse returned {status}: {message}")]
    ClickHouse {
        /// The HTTP status of the response.
        status: u16,
        /// The exception code from `X-ClickHouse-Exception-Code`, for callers
        /// that act on one failure and pass the rest through. Matching on the
        /// message text would break with the next server release.
        code: Option<u32>,
        /// The response body: the server's own error text.
        message: String,
    },

    /// Another run held the migration lock for longer than this one would
    /// wait. Nothing was run.
    #[error(
        "another run holds the migration lock ({holder}, since {since} UTC). If that run is \
         gone, release it with `chx migrate unlock`."
    )]
    Locked {
        /// Who holds it: the tool version, host and process id.
        holder: String,
        /// When it was taken, UTC.
        since: String,
    },

    /// ClickHouse could not be reached, or the connection broke mid-request.
    #[error("clickhouse unreachable: {0}")]
    Transport(#[source] reqwest::Error),

    /// A statement in a migration failed. Everything before it is applied and
    /// recorded, so the next run resumes at this statement.
    #[error(
        "migration {version} ({description}) failed at statement {statement} of {total}: {source}"
    )]
    Statement {
        /// The migration's version, from its file name.
        version: i64,
        /// The migration's description, from its file name.
        description: String,
        /// One-based, because it is read by a person counting statements in
        /// the file.
        statement: usize,
        /// Statements in the file.
        total: usize,
        /// What the statement failed with.
        #[source]
        source: Box<Error>,
    },

    /// A Rust value could not be encoded for the ClickHouse type it was bound
    /// or inserted as: the types do not match, or the value does not fit.
    #[error("encode: {0}")]
    Encode(String),

    /// A column could not be read as the Rust type asked for, or the body
    /// was not the RowBinary the server was asked for.
    #[error("decode: {0}")]
    Decode(String),

    /// A query parameter was bound that the query does not declare, or one it
    /// declares was never bound. Raised before anything is sent.
    #[error("query parameter: {0}")]
    Parameter(String),

    /// The result set is not the one the code was compiled against: a column
    /// was renamed, moved, added or dropped since `chx prepare` ran.
    #[error("result columns differ from what this code expects: {0}")]
    Schema(String),

    /// `fetch_one` on a query that returned nothing.
    #[error("no rows returned by a query that expected one")]
    RowNotFound,

    /// A row has no column by that name or at that index.
    #[error("column not found: {0}")]
    ColumnNotFound(String),

    /// The server named a type the type model could not parse.
    #[error("type: {0}")]
    Type(String),

    /// An I/O failure with no more specific category. The source is preserved
    /// so the raw `io::ErrorKind` stays reachable.
    #[error("io error: {0}")]
    Io(#[source] io::Error),

    /// An invariant was violated. Reaching this is a bug in chx.
    #[error("internal error: {0}")]
    Internal(String),
}

/// Maps the `io::ErrorKind`s that callers branch on into their own variants and
/// keeps the rest as [`Error::Io`], so a caller can match on `NotFound` without
/// reaching through to the source.
impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        match error.kind() {
            io::ErrorKind::NotFound => Error::NotFound(error.to_string()),
            _ => Error::Io(error),
        }
    }
}

impl From<chx_types::endpoint::UrlError> for Error {
    fn from(error: chx_types::endpoint::UrlError) -> Self {
        Error::Url(error.0)
    }
}

impl From<chx_types::types::ParseError> for Error {
    fn from(error: chx_types::types::ParseError) -> Self {
        Error::Type(error.to_string())
    }
}

impl From<chx_types::params::ParamError> for Error {
    fn from(error: chx_types::params::ParamError) -> Self {
        Error::Parameter(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_not_found_maps_to_not_found() {
        let err = Error::from(io::Error::new(io::ErrorKind::NotFound, "no such file"));

        assert!(matches!(err, Error::NotFound(_)));
    }

    #[test]
    fn statement_errors_name_the_migration_and_position() {
        let err = Error::Statement {
            version: 3,
            description: "create trades".to_string(),
            statement: 2,
            total: 4,
            source: Box::new(Error::ClickHouse {
                status: 500,
                code: Some(62),
                message: "Code: 62. Syntax error".to_string(),
            }),
        };

        assert_eq!(
            err.to_string(),
            "migration 3 (create trades) failed at statement 2 of 4: \
             clickhouse returned 500: Code: 62. Syntax error"
        );
    }

    #[test]
    fn other_io_errors_keep_their_source() {
        let err = Error::from(io::Error::new(io::ErrorKind::PermissionDenied, "denied"));

        assert!(matches!(err, Error::Io(_)), "{err}");
        assert!(std::error::Error::source(&err).is_some());
    }

    #[test]
    fn errors_from_the_shared_crate_keep_their_category() {
        let url = chx_types::endpoint::Endpoint::parse("ftp://host").unwrap_err();
        assert!(matches!(Error::from(url), Error::Url(_)));

        let ty = chx_types::Type::parse("Array(").unwrap_err();
        assert!(matches!(Error::from(ty), Error::Type(_)));

        let param = chx_types::params::parse("SELECT {a:UInt8}, {a:String}").unwrap_err();
        let err = Error::from(param);
        assert!(matches!(err, Error::Parameter(_)), "{err}");
        assert!(err.to_string().contains("both UInt8 and String"), "{err}");
    }

    #[test]
    fn a_held_lock_says_how_to_release_it() {
        let err = Error::Locked {
            holder: "chx 0.3.0 on ci, pid 1".to_string(),
            since: "2026-10-12 14:00:00".to_string(),
        };
        let text = err.to_string();
        assert!(text.contains("chx 0.3.0 on ci, pid 1"), "{text}");
        assert!(text.contains("chx migrate unlock"), "{text}");
    }
}
