//! The whole library in one file: migrate, insert, query.
//!
//! Start a ClickHouse to run it against, for example the one the tests use:
//!
//! ```bash
//! docker compose -f docker/compose.yaml up -d --wait single
//! CLICKHOUSE_URL=http://default:chx@127.0.0.1:58123 cargo run -p chx-core --example quickstart
//! ```
//!
//! It creates a database named `chx_quickstart`, works in it, and drops it.
//! Nothing here uses the checked macros, so it compiles with no database and
//! no `.chx` cache; see `tests/macros.rs` for those.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use chx::migrate::Migrator;
use chx::types::Decimal;
use chx::{Client, Error, FromRow, Result};

/// One row of `trades`, read by column name.
#[derive(Debug, FromRow)]
struct Trade {
    id: u64,
    venue: String,
    /// `Decimal(38, 18)` in the table: a mantissa and a scale.
    px: Decimal,
    ts: DateTime<Utc>,
    tags: Vec<String>,
    /// `Nullable(String)`.
    note: Option<String>,
}

/// A row of an aggregate, with a column renamed to a field.
#[derive(Debug, FromRow)]
struct VenueCount {
    venue: String,
    #[chx(rename = "n")]
    trades: u64,
}

const DATABASE: &str = "chx_quickstart";

#[tokio::main]
async fn main() -> Result<()> {
    let url = std::env::var("CLICKHOUSE_URL").map_err(|_| {
        Error::Url("set CLICKHOUSE_URL, for example http://default:@localhost:8123".into())
    })?;
    let server = Client::from_url(&url)?;

    server
        .execute(&format!("CREATE DATABASE IF NOT EXISTS {DATABASE}"))
        .await?;
    let client = server.with_database(DATABASE);

    let outcome = run(&client).await;

    server
        .execute(&format!("DROP DATABASE IF EXISTS {DATABASE} SYNC"))
        .await?;
    outcome
}

async fn run(client: &Client) -> Result<()> {
    // 1. Migrations. A directory of numbered files, applied once each.
    let migrations = tempfile::tempdir()?;
    std::fs::write(
        migrations.path().join("20260101000000_create_trades.sql"),
        "CREATE TABLE trades (
             id UInt64,
             venue LowCardinality(String),
             px Decimal(38, 18),
             ts DateTime64(9, 'UTC'),
             tags Array(String),
             note Nullable(String)
         ) ENGINE = MergeTree ORDER BY id",
    )?;
    let migrator = Migrator::from_dir(migrations.path())?;
    migrator
        .run(client, |done| {
            println!("applied {} {}", done.version, done.description)
        })
        .await?;
    // A second run has nothing to do.
    assert!(migrator.run(client, |_| {}).await?.is_empty());

    // 2. A bulk insert: one request, rows as RowBinary.
    let now = Utc::now();
    let mut insert = client
        .insert("trades", &["id", "venue", "px", "ts", "tags", "note"])
        .await?;
    for id in 1..=1000u64 {
        let venue = if id % 3 == 0 { "ARCA" } else { "NYSE" };
        let px: Decimal = format!("{}.25", 100 + id % 7).parse()?;
        let tags = if id % 2 == 0 { vec!["even"] } else { vec![] };
        let note = (id == 1).then_some("first");
        insert.write((id, venue, px, now, tags, note))?;
    }
    println!("inserted {} rows", insert.end().await?);

    // 3. Typed rows, with parameters declared in the SQL and bound by name.
    let trades: Vec<Trade> = chx::query_as(
        "SELECT id, venue, px, ts, tags, note FROM trades \
         WHERE venue = {venue:String} AND id <= {max:UInt64} ORDER BY id",
    )
    .bind("venue", "NYSE")
    .bind("max", &5u64)
    .fetch_all(client)
    .await?;
    for trade in &trades {
        println!(
            "{} {} {} {} {:?} {:?}",
            trade.id, trade.venue, trade.px, trade.ts, trade.tags, trade.note
        );
    }
    assert_eq!(trades.len(), 4);
    assert_eq!(trades[0].note.as_deref(), Some("first"));

    // 4. One value, an aggregate, and a row that may not exist.
    let count: u64 = chx::query_scalar("SELECT count() FROM trades")
        .fetch_one(client)
        .await?;
    assert_eq!(count, 1000);

    let by_venue: Vec<VenueCount> =
        chx::query_as("SELECT venue, count() AS n FROM trades GROUP BY venue ORDER BY venue")
            .fetch_all(client)
            .await?;
    for row in &by_venue {
        println!("{}: {} trades", row.venue, row.trades);
    }

    let missing: Option<Trade> = chx::query_as("SELECT * FROM trades WHERE id = {id:UInt64}")
        .bind("id", &0u64)
        .fetch_optional(client)
        .await?;
    assert!(missing.is_none());

    // 5. Collections and NULL as parameters.
    let tagged: Vec<(u64, Vec<String>)> = chx::query_as(
        "SELECT id, tags FROM trades \
         WHERE hasAny(tags, {tags:Array(String)}) AND venue IN {venues:Array(String)} \
           AND isNull({nothing:Nullable(String)}) AND {weights:Map(String, UInt8)}['a'] = 1 \
         ORDER BY id LIMIT 3",
    )
    .bind("tags", &["even"][..])
    .bind("venues", &vec!["NYSE", "ARCA"])
    .bind("nothing", &None::<String>)
    .bind("weights", &BTreeMap::from([("a", 1u8)]))
    .fetch_all(client)
    .await?;
    assert_eq!(tagged.len(), 3);

    // 6. Errors a caller acts on are variants, not message text.
    match chx::query("SELECT * FROM no_such_table")
        .fetch_all(client)
        .await
    {
        // 60 is UNKNOWN_TABLE.
        Err(Error::ClickHouse { code: Some(60), .. }) => println!("no such table, as expected"),
        other => panic!("expected an unknown table error, got {other:?}"),
    }
    // A parameter that was never bound is refused before anything is sent.
    let unbound = chx::query("SELECT {id:UInt64}").fetch_all(client).await;
    assert!(matches!(unbound, Err(Error::Parameter(_))));

    println!("ok");
    Ok(())
}
