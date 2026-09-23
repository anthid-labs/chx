use std::io;

use thiserror::Error as ThisError;

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
        status: u16,
        /// The exception code from `X-ClickHouse-Exception-Code`, for callers
        /// that act on one failure and pass the rest through. Matching on the
        /// message text would break with the next server release.
        code: Option<u32>,
        message: String,
    },

    /// Another run held the migration lock for longer than this one would
    /// wait. Nothing was run.
    #[error(
        "another run holds the migration lock ({holder}, since {since} UTC). If that run is \
         gone, release it with `chx migrate unlock`."
    )]
    Locked { holder: String, since: String },

    /// ClickHouse could not be reached, or the connection broke mid-request.
    #[error("clickhouse unreachable: {0}")]
    Transport(#[source] reqwest::Error),

    /// A statement in a migration failed. Everything before it is applied and
    /// recorded, so the next run resumes at this statement.
    #[error(
        "migration {version} ({description}) failed at statement {statement} of {total}: {source}"
    )]
    Statement {
        version: i64,
        description: String,
        /// One-based, because it is read by a person counting statements in
        /// the file.
        statement: usize,
        total: usize,
        #[source]
        source: Box<Error>,
    },

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
}
