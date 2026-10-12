//! The compile-time checked macros, all five, in a program you can run.
//!
//! ```bash
//! docker compose -f docker/compose.yaml up -d --wait single
//! CLICKHOUSE_URL=http://default:chx@127.0.0.1:58123 cargo run -p chx-core --example checked
//! ```
//!
//! # What was checked, and when
//!
//! Every query below was checked when this file compiled: that each
//! `{name:Type}` has an argument of a Rust type that binds as it, and that
//! the columns are the ones the fields are read from. The compiler did that
//! against the `.chx` directory next to this crate, not a database, which is
//! why `cargo build` works here with no server. `CLICKHOUSE_URL` above is
//! only read by `main`, at run time.
//!
//! The macros never see the [`Client`]. A macro call is a query value; the
//! client is what you hand it when you fetch. One client serves every macro,
//! every hand-written query and every insert in a program.
//!
//! The table is `probe`, from `tests/fixture/probe.sql`, because that is the
//! schema the checked-in cache was prepared against. In your own crate the
//! queries name your tables, and `chx prepare` writes your cache.
//!
//! # Changing a query here
//!
//! The cache is keyed by the exact query text. After editing one, regenerate
//! it as `tests/macros.rs` describes, or the build fails with a message that
//! says to run `chx prepare`.

use chrono::{DateTime, Utc};
use chx::types::Decimal;
use chx::{Client, Error, Result, Row};

const DATABASE: &str = "chx_checked_example";

/// Filled by `query_as!` as a struct literal. It needs no derive, and a
/// field whose type does not match its column is a compile error.
#[derive(Debug)]
struct Trade {
    id: u64,
    venue: String,
    px: Decimal,
    ts: DateTime<Utc>,
    note: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let url = std::env::var("CLICKHOUSE_URL").map_err(|_| {
        Error::Url("set CLICKHOUSE_URL, for example http://default:@localhost:8123".into())
    })?;
    let server = Client::from_url(&url)?;

    server
        .execute(&format!("CREATE DATABASE IF NOT EXISTS {DATABASE}"))
        .await?;
    // The one client everything below is given.
    let client = server.with_database(DATABASE);

    let outcome = run(&client).await;

    server
        .execute(&format!("DROP DATABASE IF EXISTS {DATABASE} SYNC"))
        .await?;
    outcome
}

async fn run(client: &Client) -> Result<()> {
    client
        .execute(include_str!("../tests/fixture/probe.sql"))
        .await?;

    // An INSERT through `query!`. Checked at compile time: `probe` has the
    // columns named, and each argument binds as its parameter's type.
    let rows = [
        (1u64, "NYSE", "101.25", "buy", Some("opening print")),
        (2, "NYSE", "101.50", "sell", None),
        (3, "ARCA", "99.75", "buy", None),
    ];
    for (id, venue, px, kind, note) in rows {
        let px: Decimal = px.parse()?;
        chx::query!(
            "INSERT INTO probe (id, venue, px, ts, kind, note) VALUES ({id:UInt64}, {venue:String}, {px:Decimal(38, 18)}, {ts:DateTime64(9)}, {kind:Enum8('buy' = 1, 'sell' = 2)}, {note:Nullable(String)})",
            id,
            venue,
            px,
            ts = Utc::now(),
            kind,
            note,
        )
        .execute(client)
        .await?;
    }

    // `query!`: an anonymous record with a field per column. The types come
    // from the table: `id: u64`, `px: Decimal`, `tags: Vec<String>`,
    // `note: Option<String>`, and the enum as its name.
    let venue = "NYSE";
    let records = chx::query!(
        "SELECT id, px, tags, note, kind FROM probe WHERE venue = {venue:String} ORDER BY id",
        venue,
    )
    .fetch_all(client)
    .await?;
    for record in &records {
        println!(
            "query!        {} {} {} {:?} {:?}",
            record.id, record.kind, record.px, record.tags, record.note
        );
    }
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].note.as_deref(), Some("opening print"));

    // `query_as!`: the same check, into a struct of your own.
    let trades = chx::query_as!(
        Trade,
        "SELECT id, venue, px, ts, note FROM probe WHERE px >= {floor:Decimal(38, 18)} ORDER BY id",
        floor = "100".parse::<Decimal>()?,
    )
    .fetch_all(client)
    .await?;
    for trade in &trades {
        println!(
            "query_as!     {} {} {} {} {:?}",
            trade.id, trade.venue, trade.px, trade.ts, trade.note
        );
    }
    assert_eq!(trades.len(), 2);

    // `query_scalar!`: the first column, whatever it is called.
    let count = chx::query_scalar!(
        "SELECT count() FROM probe WHERE kind = {kind:Enum8('buy' = 1, 'sell' = 2)}",
        kind = "buy",
    )
    .fetch_one(client)
    .await?;
    println!("query_scalar! {count} buys");
    assert_eq!(count, 2);

    // `query_file!`: the SQL lives in a file, relative to Cargo.toml.
    let from_file = chx::query_file!("tests/sql/probe_by_venue.sql", venue = "ARCA")
        .fetch_all(client)
        .await?;
    println!(
        "query_file!   ids {:?}",
        from_file.iter().map(|r| r.id).collect::<Vec<_>>()
    );
    assert_eq!(from_file.len(), 1);

    // A column read as another type: the enum as its number.
    let numbered = chx::query!(
        "SELECT id, kind AS \"kind: i8\" FROM probe WHERE id = {id:UInt64}",
        id = 2u64,
    )
    .fetch_one(client)
    .await?;
    println!("override      id {} kind {}", numbered.id, numbered.kind);
    assert_eq!(numbered.kind, 2);

    // `query_unchecked!`: for what no DESCRIBE can check, here a table name
    // as a parameter. Still bound by name; rows come back untyped.
    let untyped: Vec<Row> = chx::query_unchecked!(
        "SELECT id FROM {table:Identifier} ORDER BY id",
        table = "probe",
    )
    .fetch_all(client)
    .await?;
    let ids: Vec<u64> = untyped
        .iter()
        .map(|row| row.try_get("id"))
        .collect::<Result<_>>()?;
    println!("unchecked     ids {ids:?}");
    assert_eq!(ids, [1, 2, 3]);

    println!("ok");
    Ok(())
}
