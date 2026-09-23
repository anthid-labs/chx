//! The history table: what has run, with which checksum, and how far it got.

use crate::client::Client;
use crate::error::{Error, Result};

/// The table chx keeps its history in, inside the target database.
pub const TABLE: &str = "_chx_migrations";

/// One row per migration, and progress is written as a new row after every
/// statement rather than an update.
///
/// `ReplacingMergeTree(applied)` keeps the row with the most statements
/// applied, and on a tie the one inserted last, which is the latest attempt.
/// Reads use `FINAL`, so the answer does not depend on whether a background
/// merge has run yet. The table stays tiny, so `FINAL` costs nothing.
///
/// `statements` is recorded so an incomplete row says how far it had left to
/// go without anyone opening the file.
const CREATE_TABLE: &str = "CREATE TABLE IF NOT EXISTS _chx_migrations (
    version Int64,
    description String,
    checksum String,
    statements UInt32,
    applied UInt32,
    success Bool,
    execution_ms UInt64,
    installed_at DateTime64(3, 'UTC') DEFAULT now64(3)
)
ENGINE = ReplacingMergeTree(applied)
ORDER BY version";

/// The latest row for one version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub version: i64,
    pub description: String,
    pub checksum: String,
    pub statements: u32,
    /// How many statements, counted from the top of the file, have run.
    pub applied: u32,
    /// Every statement ran. `false` means the migration stopped partway and
    /// the next run resumes it at statement `applied + 1`.
    pub success: bool,
}

pub async fn ensure_table(client: &Client) -> Result<()> {
    client.execute(CREATE_TABLE).await
}

/// Every recorded version, in version order.
pub async fn read(client: &Client) -> Result<Vec<Record>> {
    let body = client
        .query(
            "SELECT version, checksum, statements, applied, success, description \
             FROM _chx_migrations FINAL ORDER BY version FORMAT TabSeparated",
        )
        .await?;

    body.lines()
        .filter(|line| !line.is_empty())
        .map(parse_row)
        .collect()
}

/// Appends a progress row. `execution_ms` is the time spent on this migration
/// in the current run only.
pub async fn record(client: &Client, record: &Record, execution_ms: u64) -> Result<()> {
    let sql = format!(
        "INSERT INTO _chx_migrations \
         (version, description, checksum, statements, applied, success, execution_ms) \
         VALUES ({}, {}, {}, {}, {}, {}, {})",
        record.version,
        quote(&record.description),
        quote(&record.checksum),
        record.statements,
        record.applied,
        record.success,
        execution_ms,
    );

    client.execute(&sql).await
}

fn parse_row(line: &str) -> Result<Record> {
    let bad = || Error::Internal(format!("unreadable row in {TABLE}: {line:?}"));
    let mut fields = line.splitn(6, '\t');
    let mut next = || fields.next().ok_or_else(bad);

    let version = next()?.parse().map_err(|_| bad())?;
    let checksum = next()?.to_string();
    let statements = next()?.parse().map_err(|_| bad())?;
    let applied = next()?.parse().map_err(|_| bad())?;
    let success = match next()? {
        "true" => true,
        "false" => false,
        _ => return Err(bad()),
    };
    let description = unescape(next()?);

    Ok(Record {
        version,
        description,
        checksum,
        statements,
        applied,
        success,
    })
}

/// A single-quoted ClickHouse string literal.
fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// Reverses the escaping `TabSeparated` applies to a string field.
fn unescape(field: &str) -> String {
    let mut out = String::with_capacity(field.len());
    let mut chars = field.chars();

    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('0') => out.push('\0'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_tab_separated_row() {
        let row = parse_row("7\tabc123\t4\t2\tfalse\tcreate trades").unwrap();

        assert_eq!(
            row,
            Record {
                version: 7,
                description: "create trades".to_string(),
                checksum: "abc123".to_string(),
                statements: 4,
                applied: 2,
                success: false,
            }
        );
    }

    #[test]
    fn an_escaped_description_round_trips() {
        let row = parse_row("1\tx\t1\t1\ttrue\tit\\'s a\\ttab").unwrap();

        assert_eq!(row.description, "it's a\ttab");
    }

    #[test]
    fn an_empty_description_is_fine() {
        let row = parse_row("1\tx\t0\t0\ttrue\t").unwrap();

        assert_eq!(row.description, "");
    }

    #[test]
    fn a_short_row_is_an_error_not_a_default() {
        assert!(parse_row("1\tx\t0").is_err());
    }

    #[test]
    fn quoting_escapes_quotes_and_backslashes() {
        assert_eq!(quote(r"it's a \ path"), r"'it\'s a \\ path'");
    }
}
