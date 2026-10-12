//! The checked macros, compiled from the offline cache and run against one
//! real ClickHouse server, the `single` service in `docker/compose.yaml`.
//!
//! There are two halves to what a checked macro promises, and this file is
//! built so that each is proven separately:
//!
//! - **Compile time.** Every macro below compiles against `.chx` next to
//!   this crate, which `chx prepare` wrote from the tables in
//!   `tests/fixture`. The default `cargo test` builds this file with no
//!   database, so a hermetic build passing is itself the proof that the
//!   offline cache is enough. The repository's `.cargo/config.toml` sets
//!   `CHX_OFFLINE=true`, so a stray `CLICKHOUSE_URL` cannot change that.
//! - **Run time.** Each test creates the fixture tables in a scratch
//!   database and runs the query, which proves the code the macro generated
//!   binds, decodes and checks columns correctly against a real server.
//!
//! Ignored by default, because they start containers:
//!
//! ```bash
//! cargo test -p chx-core --test macros -- --ignored
//! ```
//!
//! # Changing a query here
//!
//! The cache is keyed by the SHA-256 of the query text, so any edit to a
//! query string, even whitespace, needs a new entry. With the compose server
//! up and the two files in `tests/fixture` applied to a database on it:
//!
//! ```text
//! cd crates/chx-core
//! CLICKHOUSE_URL=http://default:chx@127.0.0.1:58123/<db> cargo run -p chx-cli -- prepare
//! ```
//!
//! Check the changed `.chx` files in with the test. The same directory
//! holds the entries for `examples/checked.rs`, which is these macros again
//! as a program to read and run. The `chx-cli` live suite
//! runs `prepare --check` against this directory and fails on a stale one.
//!
//! | Test | Macro | Proves |
//! | --- | --- | --- |
//! | `query_returns_a_record_with_a_field_per_column` | `query!` | Field types follow column types |
//! | `query_as_fills_a_struct_literal` | `query_as!` | A plain struct is filled by name |
//! | `query_scalar_reads_the_first_column` | `query_scalar!` | A column needs no usable name |
//! | `query_file_reads_the_sql_from_disk` | `query_file!` | SQL from a file, comments and all |
//! | `query_unchecked_binds_what_describe_cannot_check` | `query_unchecked!` | `Identifier` parameters |
//! | `an_insert_is_checked_against_the_table_and_executed` | `query!` | `INSERT` with parameters |
//! | `a_column_alias_overrides_the_rust_type` | `query!` | `AS "name: Type"` |
//! | `a_result_that_drifted_since_prepare_is_refused` | `query!` | The run-time column check |
//! | `derive_from_row_reads_by_name_with_rename_and_default` | `FromRow` | `rename`, `default`, tuple structs |

mod common;

use chrono::{DateTime, TimeZone, Utc};
use chx::types::Decimal;
use chx::{Error, FromRow, Row, query, query_as, query_file, query_scalar, query_unchecked};

use common::{SINGLE, Scratch, compose_up};

/// A scratch database with the `probe` table from `tests/fixture` and two
/// rows in it: id 1 on NYSE with a note and two tags, id 2 on ARCA with
/// neither. The same table the cache was prepared against.
async fn fixture(test: &str) -> Scratch {
    compose_up(&["single"]);
    let scratch = Scratch::new(SINGLE, test).await;
    let client = scratch.client();
    client
        .execute(include_str!("fixture/probe.sql"))
        .await
        .unwrap();
    let at = Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap();
    let mut insert = client
        .insert(
            "probe",
            &["id", "venue", "px", "ts", "tags", "note", "kind"],
        )
        .await
        .unwrap();
    insert
        .write((
            1u64,
            "NYSE",
            Decimal::new(15, 1),
            at,
            &["a", "b"][..],
            Some("one"),
            "buy",
        ))
        .unwrap();
    insert
        .write((
            2u64,
            "ARCA",
            Decimal::new(-25, 1),
            at,
            Vec::<&str>::new(),
            None::<&str>,
            "sell",
        ))
        .unwrap();
    insert.end().await.unwrap();
    scratch
}

/// `query!` over every kind of column in the fixture. The `let` bindings with
/// explicit types are the assertion that matters: they only compile if the
/// macro gave each field the Rust type its column maps to, including
/// `Option` for the `Nullable` one and `String` for the enum.
#[tokio::test]
#[ignore = "needs Docker: cargo test -p chx-core -- --ignored"]
async fn query_returns_a_record_with_a_field_per_column() {
    let scratch = fixture("record").await;
    let venue = "NYSE";

    let rows = query!(
        "SELECT id, venue, px, ts, tags, note, kind FROM probe WHERE venue = {venue:String} ORDER BY id",
        venue
    )
    .fetch_all(scratch.client())
    .await
    .unwrap();

    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    let id: u64 = row.id;
    let px: Decimal = row.px;
    let ts: DateTime<Utc> = row.ts;
    let tags: &Vec<String> = &row.tags;
    let note: &Option<String> = &row.note;
    assert_eq!(id, 1);
    assert_eq!(row.venue, "NYSE");
    assert_eq!(px, Decimal::new(1_500_000_000_000_000_000, 18));
    assert_eq!(ts, Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap());
    assert_eq!(tags, &["a", "b"]);
    assert_eq!(note.as_deref(), Some("one"));
    assert_eq!(row.kind, "buy");

    scratch.drop().await;
}

/// `query_as!` into a plain struct that does not implement `FromRow`. The
/// macro writes the struct literal itself, so this compiling shows the fields
/// were matched to columns by name and type.
#[tokio::test]
#[ignore = "needs Docker: cargo test -p chx-core -- --ignored"]
async fn query_as_fills_a_struct_literal() {
    let scratch = fixture("as").await;

    #[derive(Debug, PartialEq)]
    struct Trade {
        id: u64,
        venue: String,
        note: Option<String>,
    }

    let trades = query_as!(
        Trade,
        "SELECT id, venue, note FROM probe WHERE id >= {min:UInt64} ORDER BY id",
        min = 1u64
    )
    .fetch_all(scratch.client())
    .await
    .unwrap();

    assert_eq!(
        trades,
        [
            Trade {
                id: 1,
                venue: "NYSE".to_string(),
                note: Some("one".to_string())
            },
            Trade {
                id: 2,
                venue: "ARCA".to_string(),
                note: None
            },
        ]
    );

    scratch.drop().await;
}

/// `query_scalar!` on `count()`, a column whose name is not a Rust identifier.
/// A scalar is read by position, so no alias is needed.
#[tokio::test]
#[ignore = "needs Docker: cargo test -p chx-core -- --ignored"]
async fn query_scalar_reads_the_first_column() {
    let scratch = fixture("scalar").await;

    let count = query_scalar!(
        "SELECT count() FROM probe WHERE id > {min:UInt64}",
        min = 0u64
    )
    .fetch_one(scratch.client())
    .await
    .unwrap();
    let count: u64 = count;
    assert_eq!(count, 2);

    scratch.drop().await;
}

/// `query_file!` with the SQL in `tests/sql/probe_by_venue.sql`, which starts
/// with a comment. The macro has to see past it to recognise a `SELECT`.
#[tokio::test]
#[ignore = "needs Docker: cargo test -p chx-core -- --ignored"]
async fn query_file_reads_the_sql_from_disk() {
    let scratch = fixture("file").await;

    let rows = query_file!("tests/sql/probe_by_venue.sql", venue = "ARCA")
        .fetch_all(scratch.client())
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, 2);
    assert_eq!(rows[0].note, None);

    scratch.drop().await;
}

/// `query_unchecked!` with an `{t:Identifier}` parameter, the one kind of
/// query no `DESCRIBE` can check, since there is no placeholder for a table
/// name. Parameters are still bound by name and the query still runs.
#[tokio::test]
#[ignore = "needs Docker: cargo test -p chx-core -- --ignored"]
async fn query_unchecked_binds_what_describe_cannot_check() {
    let scratch = fixture("unchecked").await;

    let rows: Vec<Row> = query_unchecked!("SELECT id FROM {t:Identifier} ORDER BY id", t = "probe")
        .fetch_all(scratch.client())
        .await
        .unwrap();
    let ids: Vec<u64> = rows
        .iter()
        .map(|row| row.try_get::<u64, _>("id").unwrap())
        .collect();
    assert_eq!(ids, [1, 2]);

    scratch.drop().await;
}

/// `query!` on an `INSERT ... VALUES` with parameters. At compile time it was
/// checked that `probe` has the three columns named; here it runs with
/// `execute` and the row is counted.
#[tokio::test]
#[ignore = "needs Docker: cargo test -p chx-core -- --ignored"]
async fn an_insert_is_checked_against_the_table_and_executed() {
    let scratch = fixture("insert").await;
    let client = scratch.client();

    query!(
        "INSERT INTO probe (id, venue, kind) VALUES ({id:UInt64}, {venue:String}, {kind:Enum8('buy' = 1, 'sell' = 2)})",
        id = 3u64,
        venue = "IEX",
        kind = "sell"
    )
    .execute(client)
    .await
    .unwrap();

    let count: u64 = query_scalar!("SELECT count() FROM probe")
        .fetch_one(client)
        .await
        .unwrap();
    assert_eq!(count, 3);

    scratch.drop().await;
}

/// The `AS "name: Type"` alias: an enum read as its number, and a decimal
/// named by its full path. The override is in the column name the server
/// returns, so this also shows the run-time column check accepts it.
#[tokio::test]
#[ignore = "needs Docker: cargo test -p chx-core -- --ignored"]
async fn a_column_alias_overrides_the_rust_type() {
    let scratch = fixture("override").await;

    let row = query!(
        "SELECT kind AS \"kind: i8\", px AS \"px: chx::types::Decimal\" FROM probe WHERE id = {id:UInt64}",
        id = 2u64
    )
    .fetch_one(scratch.client())
    .await
    .unwrap();
    let kind: i8 = row.kind;
    assert_eq!(kind, 2);
    assert_eq!(row.px, Decimal::new(-2_500_000_000_000_000_000, 18));

    scratch.drop().await;
}

/// The reason the macros carry their column list to run time. The cache says
/// `SELECT * FROM probe_drift` returns `(a, b)`; the table created here has
/// `(a, c)`. Without the check the second column would be read into a field
/// called `b`. With it, the query fails with `Error::Schema` naming both.
#[tokio::test]
#[ignore = "needs Docker: cargo test -p chx-core -- --ignored"]
async fn a_result_that_drifted_since_prepare_is_refused() {
    compose_up(&["single"]);
    let scratch = Scratch::new(SINGLE, "drift").await;
    let client = scratch.client();

    // The cache has this query returning (a, b). The table here says otherwise.
    client
        .execute("CREATE TABLE probe_drift (a UInt8, c UInt8) ENGINE = Memory")
        .await
        .unwrap();
    client
        .execute("INSERT INTO probe_drift VALUES (1, 2)")
        .await
        .unwrap();

    let err = query!("SELECT * FROM probe_drift")
        .fetch_all(client)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Schema(_)), "{err}");
    assert!(err.to_string().contains("[a, b]"), "{err}");
    assert!(err.to_string().contains("[a, c]"), "{err}");

    scratch.drop().await;
}

/// `#[derive(FromRow)]` against a real result: a field renamed to another
/// column, a defaulted field whose column the query does not select, and a
/// tuple struct read by position.
#[tokio::test]
#[ignore = "needs Docker: cargo test -p chx-core -- --ignored"]
async fn derive_from_row_reads_by_name_with_rename_and_default() {
    let scratch = fixture("derive").await;

    #[derive(Debug, PartialEq, FromRow)]
    struct Named {
        id: u64,
        #[chx(rename = "venue")]
        market: String,
        #[chx(default)]
        absent: Option<u64>,
    }

    #[derive(Debug, PartialEq, FromRow)]
    struct Positional(u64, String);

    let named: Named = query_as("SELECT id, venue FROM probe WHERE id = 1")
        .fetch_one(scratch.client())
        .await
        .unwrap();
    assert_eq!(
        named,
        Named {
            id: 1,
            market: "NYSE".to_string(),
            absent: None
        }
    );

    let positional: Positional = query_as("SELECT id, venue FROM probe WHERE id = 2")
        .fetch_one(scratch.client())
        .await
        .unwrap();
    assert_eq!(positional, Positional(2, "ARCA".to_string()));

    scratch.drop().await;
}
