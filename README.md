# chx

ClickHouse for Rust, in the shape of sqlx: queries checked at compile time,
typed rows over RowBinary, bulk inserts, and schema migrations built for the
way ClickHouse actually fails.

[![✅ CI](https://github.com/anthid-labs/chx/actions/workflows/ci.yml/badge.svg)](https://github.com/anthid-labs/chx/actions/workflows/ci.yml)
[![📦 Publish to crates.io](https://github.com/anthid-labs/chx/actions/workflows/publish.yml/badge.svg)](https://github.com/anthid-labs/chx/actions/workflows/publish.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

```rust
let trades = chx::query!(
    "SELECT id, venue, px FROM trades WHERE venue = {venue:String} LIMIT {n:UInt64}",
    venue,
    n = 10u64,
)
.fetch_all(&client)
.await?;
println!("{} at {}", trades[0].venue, trades[0].px);
```

The parameters come from the SQL, so a missing or mistyped argument is a
compile error. The columns come from the database, through a `DESCRIBE` at
compile time or the `.chx` cache that `chx prepare` writes, so `trades[0].px`
is a `Decimal` because the column is. See
[Compile-time checked queries](#compile-time-checked-queries).

```bash
cargo install chx-cli
export CLICKHOUSE_URL=http://default:@localhost:8123/analytics
chx migrate run
```

`chx migrate run` applies every `.sql` file in `./migrations` that has not run
yet, in version order. It records a SHA-256 of each file in the database, so a
file that changes after it ran is refused instead of silently disagreeing with
the schema.

## Why

ClickHouse speaks SQL, but not the SQL the Rust ecosystem's tools assume. It
has no prepared statements, so a driver cannot ask it what a query takes; it
declares parameter types in the query text instead. Its types carry their own
nullability, precision and scale, so a column is never ambiguous, but a value
cannot be written without knowing them. And its DDL is not transactional.
chx is built around each of those rather than around them.

**Queries.** Parameters are ClickHouse's own `{name:Type}`, substituted server
side. Rows travel as `RowBinaryWithNamesAndTypes`, whose header names and
types every column, so a row is decoded as the types it really has and a
result that drifted from what the code expects is refused rather than
misread. `query!` and friends check all of this when the crate compiles.

**Migrations.** ClickHouse DDL is not transactional. A migration that fails on its third
statement has already applied the first two, and a tool that records a
migration only when it finishes has no idea. The next run fails on the first
`CREATE TABLE`, and you work out by hand which half of the file is live.

chx is built around that:

- **Progress is recorded after every statement.** A failed migration resumes
  at the statement that failed, after you fix it.
- **The history is checked before anything runs.** An edited, deleted or
  out-of-order migration stops the run before a single statement is sent.
- **One run at a time.** Two deploys that start together apply each migration
  once.
- **Adopting an existing database is one command.** `chx migrate import`
  records the files a hand-built schema already has, without running them.
- **Clusters are supported where they can be.** The history and the lock
  reach every node with `ON CLUSTER`, a `Replicated` database or ClickHouse
  Cloud. Where they cannot, chx says so rather than pretending.

## Contents

- [Install](#install)
- [Quick start](#quick-start)
- [Queries](#queries)
- [Compile-time checked queries](#compile-time-checked-queries)
- [The offline cache](#the-offline-cache)
- [Bulk inserts](#bulk-inserts)
- [Types](#types)
- [Cookbook](#cookbook)
- [Coming from the clickhouse crate](#coming-from-the-clickhouse-crate)
- [Commands](#commands)
- [Migrations](#migrations)
- [What a run does](#what-a-run-does)
- [Partial migrations](#partial-migrations)
- [What a run refuses](#what-a-run-refuses)
- [Importing a database built by hand](#importing-a-database-built-by-hand)
- [One run at a time](#one-run-at-a-time)
- [Connection](#connection)
- [Clusters](#clusters)
- [The history table](#the-history-table)
- [As a library](#as-a-library)
- [Not done yet](#not-done-yet)
- [Testing](#testing)
- [Contributing](#contributing)
- [Support](#support)
- [License](#license)

## Install

```toml
[dependencies]
chx-core = "0.3"
```

```bash
cargo install chx-cli
```

The library is `chx-core`, used as `chx`, because `chx` on crates.io is an
unrelated hex editor. The command is `chx-cli`, installed as `chx`. Two more
crates sit underneath and are not depended on directly: `chx-types`, the type
model and cache format the library and the macros share, and `chx-macros`.

Features of `chx-core`, all on by default: `macros` for `query!` and
`#[derive(FromRow)]`, `chrono` for date and time columns, `uuid` for `UUID`.

Rust 1.88 or newer.

## Quick start

A table, some rows, and a typed query, start to finish. This is
[`examples/quickstart.rs`](crates/chx-core/examples/quickstart.rs) in short;
the full file runs against any ClickHouse:

```bash
docker compose -f docker/compose.yaml up -d --wait single
CLICKHOUSE_URL=http://default:chx@127.0.0.1:58123 cargo run -p chx-core --example quickstart
```

```sql
-- migrations/20260101000000_create_trades.sql
CREATE TABLE trades (
    id UInt64,
    venue LowCardinality(String),
    px Decimal(38, 18),
    ts DateTime64(9, 'UTC'),
    tags Array(String),
    note Nullable(String)
) ENGINE = MergeTree ORDER BY id
```

```rust
use chrono::{DateTime, Utc};
use chx::migrate::Migrator;
use chx::types::Decimal;
use chx::{Client, FromRow};

#[derive(Debug, FromRow)]
struct Trade {
    id: u64,
    venue: String,
    px: Decimal,
    ts: DateTime<Utc>,
    tags: Vec<String>,
    note: Option<String>,
}

#[tokio::main]
async fn main() -> chx::Result<()> {
    let client = Client::from_url("http://default:@localhost:8123/analytics")?;

    // Apply what has not run yet. A second call does nothing.
    Migrator::from_dir("migrations")?.run(&client, |_| {}).await?;

    // One request for the whole batch.
    let mut insert = client
        .insert("trades", &["id", "venue", "px", "ts", "tags", "note"])
        .await?;
    insert.write((1u64, "NYSE", "101.25".parse::<Decimal>()?, Utc::now(), vec!["open"], None::<&str>))?;
    insert.write((2u64, "ARCA", "101.50".parse::<Decimal>()?, Utc::now(), Vec::<&str>::new(), Some("late")))?;
    insert.end().await?;

    // Parameters are declared in the SQL and bound by name.
    let trades: Vec<Trade> = chx::query_as(
        "SELECT id, venue, px, ts, tags, note FROM trades WHERE venue = {venue:String}",
    )
    .bind("venue", "NYSE")
    .fetch_all(&client)
    .await?;

    println!("{trades:#?}");
    Ok(())
}
```

To have the query checked when the crate compiles, write it with `query_as!`
instead and run `chx prepare` once; see
[Compile-time checked queries](#compile-time-checked-queries).

## Queries

```rust
use chx::{Client, FromRow};

#[derive(FromRow)]
struct Trade {
    id: u64,
    venue: String,
    px: chx::types::Decimal,
    note: Option<String>,
}

let client = Client::from_url("http://default:@localhost:8123/analytics")?;

let trades: Vec<Trade> = chx::query_as(
    "SELECT id, venue, px, note FROM trades WHERE venue = {venue:String} AND ts > {since:DateTime64(9)}",
)
.bind("venue", "NYSE")
.bind("since", &since)
.fetch_all(&client)
.await?;

let count: u64 = chx::query_scalar("SELECT count() FROM trades")
    .fetch_one(&client)
    .await?;
```

- **Parameters are declared in the SQL** as `{name:Type}` and bound by name.
  The value is written in the text form the server reads parameters in and
  sent as `param_name`; the server does the substitution. Binding a name the
  query does not declare, a Rust type that cannot be written as the declared
  type, or leaving a parameter unbound fails before anything is sent.
- **Rows are `RowBinaryWithNamesAndTypes`.** The header carries every column's
  name and type, and each column is decoded as that type into the Rust type
  asked for. `Row::try_get("name")` or `try_get(index)` reads one column;
  `#[derive(FromRow)]` reads a struct by field name, with
  `#[chx(rename = "column")]` and `#[chx(default)]` for a column that may be
  missing. Tuples read columns by position.
- **`fetch_all`, `fetch_one`, `fetch_optional` and `execute`** do what they do
  in sqlx. The whole body is read before any row is decoded; there is no
  streaming yet.
- **Settings** go in the URL, as for migrations:
  `?max_execution_time=60`.

A query can also say which columns it expects:

```rust
chx::query("SELECT id, venue FROM trades")
    .expect_columns(&["id", "venue"])
```

A result with other columns fails with `Error::Schema` naming both lists. The
checked macros set this from what the query was compiled against.

## Compile-time checked queries

```rust
let trades = chx::query!(
    "SELECT id, venue, px FROM trades WHERE venue = {venue:String} LIMIT {n:UInt64}",
    venue,
    n = 10u64,
)
.fetch_all(&client)
.await?;
```

| Macro | Returns |
| --- | --- |
| `query!(sql, args..)` | Rows of an anonymous struct with a field per column. |
| `query_as!(Struct, sql, args..)` | Rows of `Struct`, filled as a struct literal, so every field needs a column of its type and every column a field. `Struct` need not implement `FromRow`. |
| `query_scalar!(sql, args..)` | The first column. |
| `query_file!("path.sql", args..)` | `query!` with the SQL in a file, relative to `Cargo.toml`. The file is tracked, so editing it recompiles. |
| `query_unchecked!(sql, args..)` | Parameters parsed and bound, nothing checked against a database. Rows come back as `Row`. |

Arguments are `name = expr`, or a bare `name` for a variable of that name.
There must be exactly one per parameter.

A macro call is a query, not a connection. It takes the same `chx::Client`
as everything else, at the point you fetch, so one client serves every
macro, every hand-written query and every insert in a program. While
compiling, the macro uses no client at all: it reads the `.chx` cache, or
makes one short request of its own to `CLICKHOUSE_URL`.

All five, in a program that runs:
[`examples/checked.rs`](crates/chx-core/examples/checked.rs).

```bash
docker compose -f docker/compose.yaml up -d --wait single
CLICKHOUSE_URL=http://default:chx@127.0.0.1:58123 cargo run -p chx-core --example checked
```

```text
query!        1 buy 101.250000000000000000 [] Some("opening print")
query!        2 sell 101.500000000000000000 [] None
query_as!     1 NYSE 101.250000000000000000 2026-10-12 15:39:12.931930001 UTC Some("opening print")
query_as!     2 NYSE 101.500000000000000000 2026-10-12 15:39:12.936562371 UTC None
query_scalar! 2 buys
query_file!   ids [3]
override      id 2 kind 2
unchecked     ids [1, 2, 3]
ok
```

What is checked, and where it comes from:

- **Parameters, from the text.** Each `{name:Type}` has one argument, and the
  argument's Rust type binds as the declared type: `&str` or `String` for a
  `String`, `Option<u64>` for `Nullable(UInt64)`, a slice or `Vec` for an
  `Array`, a `BTreeMap` or `HashMap` for a `Map`, a reference to any of
  them. A fixed-size array is not a list here, since `[u8; N]` is what
  `FixedString(N)` binds as; pass `&array[..]`. The rule is the `chx::BindAs` trait; a type of your own implements it
  alongside `Encode` to be accepted. No server is involved.
- **Columns, from the database.** A `SELECT` is described with
  `DESCRIBE TABLE (<query>)`, with placeholder values for the parameters, and
  each column becomes a field of the mapped Rust type (see [Types](#types)).
  An `INSERT INTO t (a, b)` checks that `t` has those columns. Any other
  statement is parsed by the server and nothing more.
- **At run time, the columns again.** The query carries the column list it
  was compiled against, and a result with different columns fails with
  `Error::Schema` before a row is read.

A column can be read as another type by aliasing it `"name: path::Type"`,
where the type implements `chx::Decode`:

```rust
chx::query!("SELECT kind AS \"kind: i8\" FROM trades")
```

Where the database comes from, in order:

1. `CHX_OFFLINE=true`: only the `.chx` cache. A query with no entry is a
   compile error that says to run `chx prepare`.
2. `CLICKHOUSE_URL`, from the environment or the nearest `.env` at or above
   the crate: the server. The schema the queries run against has to be
   applied there.
3. Neither: the cache if it has the query, else an error naming both ways
   out.

Two parameter types cannot be checked: `{t:Identifier}`, which has no
placeholder, and any type the model does not know. Use `query_unchecked!` for
those.

## The offline cache

```bash
chx prepare            # write .chx from every query! in the crate
chx prepare --check    # fail if .chx is stale; for CI
```

`.chx/query-<hash>.json` holds, per query, the text, the parameters parsed
from it and the columns the server described. The hash is plain SHA-256 of
the text, so `sha256sum` matches a file to its query. Check the directory in,
and builds need no database.

`chx prepare` runs `cargo check --all-targets` in the working directory with
the macros told to save what they learn, then makes `.chx` exactly that set:
stale files go, new and changed ones are written. Nothing is cleaned first:
each checked macro makes its crate depend on the variable `prepare` sets, so
cargo recompiles exactly the crates that hold a query, whatever state the
target directory is in and whatever else, an editor included, is building in
it.
Arguments after `--` replace the `cargo check` arguments, for example
`chx prepare -- --workspace`. It needs `CLICKHOUSE_URL`, from the
environment or `.env`, as `chx migrate` does.

The macros look for `.chx` in the crate's directory, then each directory
above it, so a workspace can keep one at its root by running `chx prepare`
there.

## Bulk inserts

```rust
let mut insert = client.insert("trades", &["id", "venue", "px"]).await?;
for trade in trades {
    insert.write((trade.id, &trade.venue, trade.px))?;
    if insert.buffered_bytes() > 8 << 20 {
        insert.flush().await?;
    }
}
let rows = insert.end().await?;
```

One `DESCRIBE TABLE` when the insert opens, so every value is encoded as the
column really is, then one `INSERT ... FORMAT RowBinary` request per
`flush`, however many rows it holds. A row is a tuple with one `Encode` value
per column, in the order the columns were named, up to sixteen. A row that
does not fit, such as a `u32` for a `UInt64` column or a `Decimal` with more
places than the column's scale, is refused by `write` and the batch is left
as it was.

A struct becomes a row by implementing `IntoRow`, usually by lending its
fields to a tuple:

```rust
use chx::{Column, IntoRow};

struct Trade {
    id: u64,
    venue: String,
    px: chx::types::Decimal,
}

impl IntoRow for Trade {
    fn encode_row(&self, columns: &[Column], buf: &mut Vec<u8>) -> chx::Result<()> {
        (self.id, &self.venue, self.px).encode_row(columns, buf)
    }
}

let mut insert = client.insert("trades", &["id", "venue", "px"]).await?;
for trade in &trades {
    insert.write(trade)?;
}
insert.end().await?;
```

If a `flush` fails, the rows in that batch are gone with the request, and
the error says how many. Whether ClickHouse stored some of them depends on
the failure, so a caller that must not lose or repeat rows should make the
insert idempotent, for example with a `ReplacingMergeTree` or
`insert_deduplication_token` in the URL's settings.

## Types

| ClickHouse | Rust | Notes |
| --- | --- | --- |
| `UInt8` to `UInt128`, `Int8` to `Int128` | `u8` to `u128`, `i8` to `i128` | Exact width; a `u32` does not read a `UInt64`. |
| `Float32`, `Float64` | `f32`, `f64` | |
| `Bool` | `bool` | |
| `String` | `String` | `chx::types::Bytes` for a string that is not UTF-8. |
| `FixedString(N)` | `[u8; N]` | `String` reads it too, with the zero padding stripped. A value longer than `N` is refused before it is sent. |
| `Date`, `Date32` | `chrono::NaiveDate` | |
| `DateTime`, `DateTime64(P)` | `chrono::DateTime<Utc>` | An instant; the column's display timezone does not move it. `NaiveDateTime` is taken as UTC. |
| `Decimal(P, S)`, P up to 38 | `chx::types::Decimal` | A mantissa and scale, rescaled to the column on write and refused if that loses digits. |
| `UUID` | `uuid::Uuid` | |
| `IPv4`, `IPv6` | `std::net::Ipv4Addr`, `Ipv6Addr` | |
| `Enum8`, `Enum16` | `String` | By name. `i8` or `i16` reads the number. |
| `Nullable(T)` | `Option<T>` | `Option<T>` also reads a plain `T`. |
| `LowCardinality(T)` | as `T` | Serialised as `T`. |
| `Array(T)` | `Vec<T>` | Slices write too. |
| `Map(K, V)` | `Vec<(K, V)>`, `BTreeMap`, `HashMap` | On the wire a map is an array of pairs. |
| `Tuple(A, B, ..)` | `(A, B, ..)` | Up to twelve elements. Named tuples read by position. |
| `UInt256`, `Int256`, `JSON`, `Dynamic`, `Variant`, `AggregateFunction` | none | Cast in the query, for example with `toString()`. |

The same mapping decides what a parameter binds as and what a `query!` field
is. `chx::Encode` and `chx::Decode` are the traits behind it, each taking the
ClickHouse type so a `DateTime64` knows its precision and a `Decimal` its
scale; implement them for a type of your own, as shown in the
[cookbook](#a-type-of-your-own).

## Cookbook

Each of these is checked somewhere: as a doc test in the crate, in
`examples/quickstart.rs`, or in the live suites.

### One value, one row, maybe a row

```rust
let count: u64 = chx::query_scalar("SELECT count() FROM trades")
    .fetch_one(&client)
    .await?;

let latest: Option<Trade> = chx::query_as("SELECT * FROM trades ORDER BY ts DESC LIMIT 1")
    .fetch_optional(&client)
    .await?;

// `fetch_one` on nothing is `Error::RowNotFound`.
let trade: Trade = chx::query_as("SELECT * FROM trades WHERE id = {id:UInt64}")
    .bind("id", &7u64)
    .fetch_one(&client)
    .await?;
```

### Tuples instead of a struct

```rust
let pairs: Vec<(String, u64)> =
    chx::query_as("SELECT venue, count() FROM trades GROUP BY venue")
        .fetch_all(&client)
        .await?;
```

### Rows without a type

```rust
let rows = chx::query("SELECT * FROM trades LIMIT 10").fetch_all(&client).await?;
for row in &rows {
    for column in row.columns() {
        println!("{}: {}", column.name, column.ty);
    }
    let id: u64 = row.try_get("id")?;
    let note: Option<String> = row.try_get("note")?;
}
```

### Renamed and optional columns

```rust
#[derive(chx::FromRow)]
struct VenueStats {
    venue: String,
    #[chx(rename = "n")]
    trades: u64,
    // Filled with `Default::default()` when the query does not select it.
    #[chx(default)]
    last_px: Option<chx::types::Decimal>,
}

let stats: Vec<VenueStats> =
    chx::query_as("SELECT venue, count() AS n FROM trades GROUP BY venue")
        .fetch_all(&client)
        .await?;
```

### Parameters of every shape

```rust
use std::collections::BTreeMap;

let rows = chx::query(
    "SELECT * FROM trades \
     WHERE venue IN {venues:Array(String)} \
       AND ts >= {since:DateTime64(9)} \
       AND day = {day:Date} \
       AND px > {floor:Decimal(38, 18)} \
       AND (note = {note:Nullable(String)} OR isNull({note:Nullable(String)})) \
       AND kind = {kind:Enum8('buy' = 1, 'sell' = 2)} \
       AND {weights:Map(String, UInt8)}[venue] > 0",
)
.bind("venues", &["NYSE", "ARCA"][..])      // a slice or a Vec
.bind("since", &since)                      // chrono::DateTime<Utc>
.bind("day", &day)                          // chrono::NaiveDate
.bind("floor", &"99.5".parse::<Decimal>()?) // rescaled to the declared scale
.bind("note", &None::<String>)              // NULL
.bind("kind", "sell")                       // an enum by name
.bind("weights", &BTreeMap::from([("NYSE", 1u8)]))
.fetch_all(&client)
.await?;
```

A parameter that appears twice, like `note` above, is bound once. Its two
declarations have to agree on the type.

### A table name as a parameter

```rust
let rows = chx::query("SELECT count() AS n FROM {table:Identifier}")
    .bind("table", "trades")
    .fetch_all(&client)
    .await?;
```

The server substitutes an `Identifier` as a name, not a string, so this is
not string interpolation. It is the one kind of query the checked macros
cannot check; from them, use `query_unchecked!`.

### Statements that return nothing

```rust
chx::query("ALTER TABLE trades DELETE WHERE ts < {cutoff:DateTime64(9)}")
    .bind("cutoff", &cutoff)
    .execute(&client)
    .await?;

// No parameters, no result: the client alone is enough.
client.execute("OPTIMIZE TABLE trades FINAL").await?;
```

Mutations wait until they are done on every replica, because every request
carries `mutations_sync=2`.

### Decimals

```rust
use chx::types::Decimal;

let px: Decimal = "101.2500".parse()?;      // mantissa 1012500, scale 4
assert_eq!(px.to_string(), "101.2500");
assert_eq!(px.rescale(2)?.to_string(), "101.25");
assert!(px.rescale(1).is_err());            // would lose a digit

// From your own decimal type: its mantissa and scale.
let px = Decimal::new(1_012_500, 4);
```

`Decimal` is the wire form, not an arithmetic type. A value read from a
`Decimal(38, 18)` column has scale 18; a value written to one is rescaled to
18, or refused if that would drop digits or overflow the column.

### Checked queries

These are the same calls [`examples/checked.rs`](crates/chx-core/examples/checked.rs)
runs.

```rust
// Fields come from the columns: `id: u64`, `venue: String`, `note: Option<String>`.
let rows = chx::query!(
    "SELECT id, venue, note FROM trades WHERE venue = {venue:String}",
    venue = "NYSE",
)
.fetch_all(&client)
.await?;

// Into your own struct, by field name. No derive needed.
struct Trade { id: u64, venue: String }
let trades = chx::query_as!(Trade, "SELECT id, venue FROM trades")
    .fetch_all(&client)
    .await?;

let count = chx::query_scalar!("SELECT count() FROM trades").fetch_one(&client).await?;

// SQL in a file, relative to Cargo.toml.
let rows = chx::query_file!("queries/trades_by_venue.sql", venue = "NYSE")
    .fetch_all(&client)
    .await?;

// An INSERT is checked against the table's columns.
chx::query!(
    "INSERT INTO trades (id, venue) VALUES ({id:UInt64}, {venue:String})",
    id = 7u64,
    venue = "IEX",
)
.execute(&client)
.await?;

// Read a column as another type: an enum as its number.
let row = chx::query!("SELECT kind AS \"kind: i8\" FROM trades LIMIT 1")
    .fetch_one(&client)
    .await?;
```

What a mistake looks like, at compile time. Each of these is a real message,
pointing at the query or the argument:

```text
error: no argument for parameter venue

error: the query has no parameter {vneue:...}

error: column "count()" is not a Rust identifier; alias it with AS

error: ClickHouse returned 404 Not Found: Code: 60. DB::Exception: Unknown table
       expression identifier 'tradez' in scope SELECT * FROM tradez. (UNKNOWN_TABLE)

error: ClickHouse returned 404 Not Found: Code: 47. DB::Exception: Unknown expression
       identifier `nmae` in scope SELECT nmae FROM system.tables. Maybe you meant:
       ['name']. (UNKNOWN_IDENTIFIER)

error[E0277]: the trait bound `u32: BindAs<std::string::String>` is not satisfied

error[E0308]: `?` operator has incompatible types
```

The last two are the compiler's own: an argument of the wrong type for its
parameter, and a `query_as!` struct whose field is not the type its column
maps to.

### A type of your own

```rust
use chx::decode::{Decode, Reader};
use chx::{BindAs, Encode, Result, Type};

/// A ticker symbol, stored as a String column.
struct Symbol(String);

impl Decode for Symbol {
    fn accepts(ty: &Type) -> bool {
        String::accepts(ty)
    }
    fn decode(reader: &mut Reader<'_>, ty: &Type) -> Result<Self> {
        String::decode(reader, ty).map(Symbol)
    }
}

impl Encode for Symbol {
    fn accepts(ty: &Type) -> bool {
        String::accepts(ty)
    }
    fn encode(&self, ty: &Type, buf: &mut Vec<u8>) -> Result<()> {
        self.0.encode(ty, buf)
    }
    fn encode_text(&self, ty: &Type, out: &mut String, nested: bool) -> Result<()> {
        self.0.encode_text(ty, out, nested)
    }
}

// Lets the checked macros take a Symbol where the SQL says String.
impl BindAs<String> for Symbol {}
```

`Symbol` now works as a struct field under `#[derive(FromRow)]`, as a value
in an insert row, as a `.bind` argument, and, with the alias
`AS "symbol: Symbol"`, as a field of a `query!` record.

### Acting on an error

```rust
use chx::Error;

match chx::query("SELECT * FROM trades").fetch_all(&client).await {
    Ok(rows) => println!("{} rows", rows.len()),
    // The exception code, never the message text, which changes by release.
    Err(Error::ClickHouse { code: Some(60), .. }) => eprintln!("run the migrations first"),
    Err(Error::Transport(err)) => eprintln!("ClickHouse is unreachable: {err}"),
    Err(Error::Schema(what)) => eprintln!("the schema changed since `chx prepare`: {what}"),
    Err(other) => return Err(other),
}
```

### Migrations from your own binary

```rust
use std::time::Duration;

use chx::migrate::Migrator;

let applied = Migrator::from_dir("migrations")?
    .on_cluster("prod")
    .lock_timeout(Duration::from_secs(60))
    .run(&client, |done| tracing::info!(version = done.version, "applied"))
    .await?;
```

### In CI

```yaml
- name: Queries are checked against the cache, never a database
  run: cargo build --locked
  env:
    CHX_OFFLINE: "true"

# Optional, with a ClickHouse service container and the schema applied:
- name: The cache is current
  run: |
    chx migrate run
    chx prepare --check
  env:
    CLICKHOUSE_URL: http://default:@localhost:8123/analytics
```

To make every build in a repository offline, whatever a contributor has
exported, set it once in `.cargo/config.toml`:

```toml
[env]
CHX_OFFLINE = "true"
```

`chx prepare` overrides it for the one build that is meant to connect.

## Coming from the clickhouse crate

chx does not take the [`clickhouse`](https://crates.io/crates/clickhouse)
crate's `Client`, and has no adapter for it: the macros generate calls into
chx's own query builder. The two do run side by side. Both speak HTTP to the
same server, so a service can build one of each from the same URL and move a
query at a time.

Moving a query changes three things.

```rust
// Before: the clickhouse crate.
#[derive(clickhouse::Row, serde::Deserialize)]
struct Trade {
    id: u64,
    venue: String,
    note: Option<String>,
}

let trades = client
    .query("SELECT ?fields FROM trades WHERE venue = ? AND id > ?")
    .bind("NYSE")
    .bind(100u64)
    .fetch_all::<Trade>()
    .await?;
```

```rust
// After: chx, checked when the crate compiles.
struct Trade {
    id: u64,
    venue: String,
    note: Option<String>,
}

let trades = chx::query_as!(
    Trade,
    "SELECT id, venue, note FROM trades WHERE venue = {venue:String} AND id > {min:UInt64}",
    venue = "NYSE",
    min = 100u64,
)
.fetch_all(&client)
.await?;
```

| | `clickhouse` crate | chx |
| --- | --- | --- |
| Parameters | `?`, by position, substituted into the SQL by the client. | `{name:Type}`, by name, sent beside the SQL and substituted by the server. |
| Columns | `?fields`, filled from the struct. | Written out. The macro checks them against the struct. |
| Row type | `#[derive(Row, Deserialize)]`, through serde. | No derive with `query_as!`; `#[derive(chx::FromRow)]`, read by name, with `chx::query_as`. |
| A query that does not fit its struct | Found at run time, when the query is fetched. | A compile error. A schema that drifted after the build is `Error::Schema` before a row is read. |
| Inserts | An insert typed by a serde row struct. | `client.insert("t", &[columns])`, rows as tuples or `IntoRow`, encoded against the table's real column types. |

Field types can change with the move. A `Decimal` column is
`chx::types::Decimal`, a mantissa and a scale taken from the column. A
`DateTime64` is a `chrono::DateTime<Utc>` with no serde adapter attribute
on the field, since the precision comes from the server.

This comparison is against `clickhouse` 0.15. Check its documentation for
what a later release does.

## Commands

| Command | What it does |
| --- | --- |
| `chx migrate run` | Apply every pending migration, in version order. |
| `chx migrate import` | Record migrations as applied without running them, for a database built by hand. |
| `chx migrate unlock` | Release the lock left by a run that died holding it. |
| `chx prepare` | Check every `query!` against the database and write `.chx`. `--check` fails if `.chx` is stale instead. |

Options, each with the environment variable it falls back to:

| Flag | Environment | Default | Used by |
| --- | --- | --- | --- |
| `--clickhouse-url` | `CLICKHOUSE_URL` | required | all |
| `--cluster` | `CLICKHOUSE_CLUSTER` | none | `migrate` |
| `--source` | | `migrations` | `run`, `import` |
| `--lock-timeout` | `CHX_LOCK_TIMEOUT` | `300` seconds | `run`, `import` |
| `--through` | | every file | `import` |
| `--check` | | off | `prepare` |
| `--log-level` | `LOG_LEVEL` | `warn` | all |

`RUST_LOG` takes precedence over `--log-level`. Logs go to stderr; stdout
carries one line per applied migration.

Exit code 0 is success and 1 is any failure, with the reason on stderr. A
malformed command line exits 2, from the argument parser.

## Migrations

```
migrations/
  20260923114500_create_trades.sql
  20260923120000_add_venue.sql
```

- Files are named `<version>_<description>.sql`. The version is an integer,
  and a timestamp avoids collisions between branches. The description comes
  from the name, with underscores as spaces.
- A file can hold several statements, separated by `;`. Semicolons inside
  string literals, quoted identifiers, `--` comments and `/* */` comments do
  not split. A fragment holding only comments is skipped, and an empty file is
  a valid migration.
- Other files in the directory, such as a README, are ignored.
- Refused: a `.sql` file with no version, two files with the same version, and
  `.up.sql` / `.down.sql` files. Running a down migration forward is the one
  mistake here that destroys data.

Statements are sent to ClickHouse exactly as written, comments included, so
they show up in `system.query_log` as you wrote them.

## What a run does

`chx migrate import` follows the same steps, except that step 5 records files
instead of running them. See
[Importing a database built by hand](#importing-a-database-built-by-hand).

1. **Reads the migrations directory.** A malformed directory is reported
   without connecting.
2. **Creates the history table** if it is missing. See [Clusters](#clusters)
   for where it lives.
3. **Takes the lock**, waiting for another run if one holds it.
4. **Reads the history and checks it** against the files. Any disagreement
   stops the run here.
5. **Applies what is left**, one statement at a time, writing a history row
   after each.
6. **Releases the lock**, whether the run succeeded or failed.

```
$ chx migrate run
Applied 20260923114500/create trades (2 statements, 41ms)
Applied 20260923120000/add venue (1 statements, 12ms)

$ chx migrate run
Up to date: 2 migrations in migrations
```

Every statement runs with three settings, each closing a way for ClickHouse
to report success before the change has actually happened:

| Setting | Why |
| --- | --- |
| `wait_end_of_query=1` | Without it the server can send a 200 and then fail partway through the body. |
| `async_insert=0` | An `INSERT`, including chx's own history rows, is stored before it returns. |
| `mutations_sync=2` | `ALTER ... UPDATE` and `ALTER ... DELETE` finish on every replica before the next statement starts. |

The last one means a migration that runs a large mutation takes as long as
that mutation. That is deliberate: the next statement, and the next
migration, should not run against half-rewritten data.

## Partial migrations

A migration that fails partway is recorded with the number of statements that
succeeded:

```
$ chx migrate run
chx: migration 3 (add venue) failed at statement 2 of 3: clickhouse returned 404: Code: 47. ...
```

Fix statement 2 in the file and run again. chx skips statement 1, which
already ran, and starts at statement 2:

```
$ chx migrate run
Applied 3/add venue (3 statements, resumed at statement 2, 18ms)
```

While a migration is incomplete, its checksum is not enforced, because fixing
the failed statement is the normal next step. The statements before it must
stay as they are: they are not run again, so editing one changes nothing in
the database and chx cannot tell. Once the migration completes, the fixed
file's checksum is the one it is held to.

## What a run refuses

All of these stop the run before any statement is sent:

| Situation | What to do |
| --- | --- |
| An applied file's checksum changed | Restore the file and put the change in a new migration. The schema is what the old file did. |
| An applied version has no file | Restore it. Nobody can rebuild the schema from the repository without it. |
| A new file is numbered below the latest applied one | Usually a branch merged late. Renumber it above the latest. |
| An incomplete migration is not the latest | The history table was changed outside chx. |
| An incomplete migration now has fewer statements than already ran | Restore the statements that ran. |

The stored checksum is plain `sha256sum` output, so it can be checked with no
tooling:

```bash
sha256sum migrations/20260923114500_create_trades.sql
```

## Importing a database built by hand

A database whose schema was applied by hand, by another tool, or by a history
table that got lost has no record chx can trust. `import` writes that record
from the files, without running any of them:

```
$ chx migrate import --through 20260923120000
Imported 20260923114500/create trades (2 statements, not run)
Imported 20260923120000/add venue (1 statements, not run)

$ chx migrate run
Applied 20260924090000/add fills (1 statements, 22ms)
```

Each imported row is what a completed run would have written: the file's
checksum, its statement count, and `success = true`. `execution_ms` is 0,
because nothing ran, which is also how an imported row can be told apart from
an applied one later.

- **It only adds rows.** A version already recorded with the same checksum is
  skipped, so running `import` twice is safe, and it fills gaps in a
  partial history.
- **It never overwrites.** A version recorded with a different checksum, or
  left incomplete by a failed run, stops the import before a single row is
  written. The history and the files disagree, and picking one is your call,
  not chx's. An incomplete migration is finished with `chx migrate run`.
- **`--through <version>`** records files up to and including that version and
  leaves later ones for `run` to apply. Without it, every file is recorded,
  including ones the database may not have yet.
- **It holds the same lock as `run`**, so it cannot race a deploy.

Nothing checks that the database really matches the files. That is your claim
to make, and a wrong one means `run` will skip a migration the schema never
had. Compare the schema against the files before importing.

## One run at a time

A run holds a lock from before it reads the history until after its last
statement. Two deploys that start together apply each migration once: the
second waits, then finds nothing to do.

```
$ chx migrate run
WARN waiting for another run to release the migration lock holder=chx 0.1.0 on ci-runner-7, pid 4121 since=2026-09-23 19:13:30
Up to date: 2 migrations in migrations
```

ClickHouse has no advisory locks and no unique constraints. The one thing it
guarantees only one caller wins is `CREATE TABLE` without `IF NOT EXISTS`, so
the lock is a table, `_chx_lock`, with the `Null` engine. Whoever created it
holds the lock. Its comment names the holder's host and process id, and
`system.tables` records when it was taken.

- **On a cluster** the lock is created `ON CLUSTER`. Every node applies
  distributed DDL in one queue order, so two runs racing through different
  nodes still get exactly one winner. A `Replicated` database and ClickHouse
  Cloud order DDL themselves, so there a plain create already reaches every
  node.
- **Waiting** is bounded by `--lock-timeout`, in seconds. `0` tries once.
  When it runs out, the run fails naming the holder.
- **A run that fails** still releases the lock. The history already says
  where to resume.
- **A run that is killed** leaves the lock behind. It is never expired on a
  timer: a run in the middle of a long mutation looks exactly like a dead one,
  and breaking its lock would let a second run start on top of it. Once you
  know the holder is gone:

  ```bash
  chx migrate unlock
  ```

  ```
  Released the lock held by chx 0.1.0 on ci-runner-7, pid 4121 since 2026-09-23 19:13:33 UTC
  ```

  Pass the same `--cluster` your runs use, so the lock is dropped on every
  node.

## Connection

`CLICKHOUSE_URL`, or `--clickhouse-url` to override it:

```
http[s]://user:password@host:port/database?setting=value
```

- **The HTTP interface** (8123, or 8443 with TLS), not the native protocol. It
  is the one every deployment exposes, ClickHouse Cloud included.
- **The path is the database.** Without one, the user's default database is
  used.
- **Credentials** are percent-decoded and sent as `X-ClickHouse-User` and
  `X-ClickHouse-Key` headers. They never appear in a request URL, an error
  message or a log line.
- **Query parameters** are passed through as ClickHouse settings, for example
  `?max_execution_time=600`.
- **`.env` is read, as sqlx-cli reads it.** The nearest `.env`, found by
  walking up from the working directory, fills in variables the process does
  not already have. An exported variable wins over the file, and
  `--clickhouse-url` wins over both. Check which `.env` a directory resolves
  to before running from it: the file is a common way to name a database
  without meaning to.

## Clusters

Every node has to see the same history, or a deploy that lands on another
node starts from the top. Where chx keeps its history table depends on the
setup:

| Setup | History table | Lock |
| --- | --- | --- |
| Single server | Local `ReplacingMergeTree` | Local |
| `--cluster <name>` | `ReplicatedReplacingMergeTree`, created `ON CLUSTER` at `/clickhouse/chx/{database}/_chx_migrations` | `ON CLUSTER` |
| `Replicated` database | `ReplicatedReplacingMergeTree` at the database's own path | Replicated by the database |
| ClickHouse Cloud | Shared by Cloud | Replicated by Cloud |

With `--cluster`, the history's Keeper path has no `{shard}`: it is one list
for the whole cluster, not one per shard. That makes `{replica}` a
cluster-wide name, so it has to be unique across shards. It is when it is the
host name, which is the common setup.

**`--cluster` is tried, not required.** If the cluster cannot hold a
replicated table (no Keeper, no `{replica}` macro, no cluster by that name),
chx warns and keeps the history on the node you connected to. A migration tool
that refused to run without Keeper would be refusing the common case.

**chx checks what the history actually reached.** After creating or finding a
replicated history table, it asks Keeper how many nodes hold a replica and
warns if that is fewer than the cluster has:

```
WARN _chx_migrations is replicated to 1 of 2 nodes; a run through a node outside that set will not see this history
```

The usual causes are a `{replica}` macro that repeats across shards, or a
`Replicated` database whose nodes carry different `{shard}` macros.
ClickHouse builds each table's Keeper path in a `Replicated` database from the
server's `{shard}` macro, and refuses a path that would span shards, so in that
layout each shard keeps its own history. Run chx through one shard.

**Reads catch up first.** A replicated history is read after
`SYSTEM SYNC REPLICA`, so a node that is behind fetches the latest rows before
chx decides what to run.

**The history table is decided once.** A table that already exists keeps the
engine it was created with, whatever flags a later run passes. Converting a
history table is not something to do implicitly.

**Your migrations run exactly as written.** `--cluster` only affects chx's own
tables. A table meant for every node needs its own `ON CLUSTER`:

```sql
CREATE TABLE trades ON CLUSTER prod (id UInt64) ENGINE = MergeTree ORDER BY id;
```

## The history table

`_chx_migrations`, in the target database. A new row is written after every
statement, and `ReplacingMergeTree(applied)` keeps the one with the most
statements applied. Read it with `FINAL`:

```sql
SELECT version, description, checksum, applied, statements, success, installed_at
FROM _chx_migrations FINAL
ORDER BY version
```

| Column | Meaning |
| --- | --- |
| `version` | From the file name. |
| `description` | From the file name. |
| `checksum` | Lowercase hex SHA-256 of the file's bytes. |
| `statements` | Statements in the file. |
| `applied` | Statements that have run, counted from the top. |
| `success` | Every statement ran. `false` means the next run resumes at `applied + 1`. |
| `execution_ms` | Time spent on the migration by the run that wrote the row. |
| `installed_at` | When the row was written, UTC. |

Do not edit it by hand. The history check catches some edits, such as an
incomplete migration that is not the latest, but not all of them.

## As a library

`chx-core` is the engine the command wraps. It installs no log subscriber and
picks no runtime: it emits `tracing` events and runs on whatever tokio runtime
its caller has.

```rust
use std::time::Duration;

use chx::client::Client;
use chx::migrate::Migrator;

let client = Client::from_url("http://default:@localhost:8123/analytics")?;
let migrator = Migrator::from_dir("migrations")?
    .on_cluster("prod")
    .lock_timeout(Duration::from_secs(60));

for applied in migrator.run(&client, |_| {}).await? {
    println!("applied {} {}", applied.version, applied.description);
}
```

`chx::migrate::plan` is the history check on its own, as a pure function, for
anything that wants to know what a run would do without running it.

## Not done yet

- No `info`, `add` or `revert` commands.
- No `$tag$ ... $tag$` heredoc strings in the statement splitter.
- No native protocol.
- No streaming reads: a result is read whole before the first row is decoded.
- No derive for insert rows; a row is a tuple, or a struct that implements
  `IntoRow`.
- No `JSON`, `Dynamic`, `Variant` or 256-bit types.
- A `.env` is read by the macros from the crate's directory upward, as the
  command reads it from the working directory. Check which one a crate
  resolves to.

## Testing

Three layers, each proving something the one before cannot.

| Layer | Command | Needs | Proves |
| --- | --- | --- | --- |
| Unit tests | `cargo test --workspace` | nothing | Every codec byte layout and text form, the type parser, the parameter scanner, the history planner, macro expansion, and the client, queries, inserts, migrations and the `chx` command against an in-process fake ClickHouse. |
| Live tests | `cargo test -p chx-core -p chx-cli -- --ignored` | Docker | That the same things hold against ClickHouse itself: a single server and a two-node cluster with Keeper. |
| Doc tests | part of `cargo test --workspace` | nothing | That the examples in the API documentation compile and, where they can, run. |

The unit suite runs in about two seconds and reaches the paths a real server
will not take on demand: a statement that fails partway, a cluster that
refuses, a lock that is released while another run waits, a body cut short.
The fake lives in `crates/chx-core/src/testing.rs` and recognises the
statements chx sends by a fragment of their text, so changing one of those
statements shows up as a test that no longer matches.

Line coverage of the hermetic suite, measured with
[`cargo-llvm-cov`](https://github.com/taiki-e/cargo-llvm-cov):

```bash
cargo llvm-cov --workspace --all-features
```

CI builds the same report as HTML on every run, along with a `cargo audit`
of `Cargo.lock`, and keeps both as artifacts named `coverage-html` and
`cargo-audit` on the run's summary page.

| Crate | Lines covered |
| --- | --- |
| `chx-types` | 99.4% |
| `chx-core` | 97.9% |
| `chx-macros` | 97.5% |
| `chx-cli` | 94.7% |
| Workspace | 97.9% |

Measured at 0.3.0, with no server. What is left is mostly what a test
cannot reach from inside the process: the proc-macro entry points, which
only the compiler calls, the branches for a feature that is switched off,
and invariant checks that would mean a bug in chx if they ever fired.

## Contributing

Issues and pull requests are welcome. For anything larger than a fix, open an
issue first: chx is deliberately small, and a change that fits the design
lands much faster than one that argues with it.

### Getting set up

Rust 1.88 or newer, and Docker for the live tests.

```bash
git clone https://github.com/anthid-labs/chx
cd chx
cargo test
```

The default suite needs no Docker and no network.

### Layout

| Path | What lives there |
| --- | --- |
| `crates/chx-types` | What the library and the macros agree on: the type model and parser, the `{name:Type}` grammar, the connection URL, the `.chx` file format. No I/O. |
| `crates/chx-macros` | `query!` and friends, `#[derive(FromRow)]`. Depends on `chx-types`, never on `chx-core`. |
| `crates/chx-core` | The library, `chx`. Queries, rows, codecs, inserts, migrations. Re-exports the macros. |
| `apps/chx-cli` | The `chx` binary: arguments, logging, `prepare`, and the exit code. |
| `docker/compose.yaml` | The ClickHouse servers the live tests run against. |

Inside the library, `migrate::source` reads files, `migrate::split` cuts them
into statements, `migrate::history` owns `_chx_migrations`, `migrate::lock`
owns `_chx_lock`, and `migrate::plan` checks one against the other. `plan` is
pure, so every refusal is unit tested without a server. `migrate::import_plan`
does the same for `import`.

For queries, `encode` and `decode` are the codecs, each driven by the
ClickHouse type; `row` cuts a `RowBinaryWithNamesAndTypes` body into rows and
columns; `query` binds parameters and fetches; `insert` batches rows. In the
macros, `describe` decides where a query's columns come from, `rust_type`
maps a column to a Rust type, and `expand` writes the code.

### Live tests

```bash
cargo test -p chx-core -- --ignored
cargo test -p chx-cli -- --ignored
```

They are `#[ignore]`d so the default suite stays hermetic, and they start the
compose stack themselves:

The live suites, and what each one is for:

| Suite | Against | Proves |
| --- | --- | --- |
| `chx-core` `single_node` | one server | Migrations: order, checksums, resume, the lock, import. |
| `chx-core` `cluster` | two nodes, one Keeper | One history and one lock for the whole cluster. |
| `chx-core` `query` | one server | Every type through RowBinary and through parameters, fetching, inserts. |
| `chx-core` `macros` | one server | The checked macros, compiled from the cache, run for real. |
| `chx-cli` `prepare` | one server | `prepare --check` against the checked-in cache. |

Every test has a doc comment saying what it proves, and every suite a header
saying how to run it alone.

`tests/macros.rs` in `chx-core` compiles against the `.chx` directory next to
it, which is checked in, and `.cargo/config.toml` sets `CHX_OFFLINE=true` so
no build in this repository connects by accident. After changing a query
there, regenerate the cache against a database holding the tables in
`tests/fixture`, with the compose server up:

```bash
cd crates/chx-core
CLICKHOUSE_URL=http://default:chx@127.0.0.1:58123/<database> cargo run -p chx-cli -- prepare
```

The `chx-cli` live suite runs `prepare --check` against that cache, so a
stale one fails there.

- `single`: one server on `127.0.0.1:58123`.
- `ch-1` and `ch-2`: cluster `chx` on `127.0.0.1:58124` and `58125`, two
  shards of one replica each, sharing one `keeper`. Two shards rather than two
  replicas, because a history shared across shards is the case that matters.

The ports are fixed and far from the defaults, so a live test can never be
pointed at a real server. Each test works in its own database. The stack is
left running so the next run starts in seconds; stop it with:

```bash
docker compose -f docker/compose.yaml down -v
```

### Before opening a pull request

```bash
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
cargo test -p chx-core -- --ignored
```

CI runs exactly these on every pull request, with warnings denied: the first
three in one job, and the integration tests in another against the same
compose stack.

### Releases

A release is a merge to main. Every pull request bumps `version` in the root
`Cargo.toml`, and the `chx-core` version in `[workspace.dependencies]` with it.
CI fails a pull request whose version is already tagged, or whose pin does not
match.

On merge, the publish workflow runs CI on that commit, then
`cargo publish --workspace`, which publishes `chx-core` before `chx-cli`, then
tags the commit `v<version>`.

### What a change is expected to carry

- **A test that would fail without it.** A new refusal belongs in `plan`, with
  a unit test. Anything that depends on how ClickHouse behaves gets a live
  test, because the server has surprised this project more than once.
- **Comments that say why, not what.** The reasoning behind a decision that
  looks arbitrary is the part that survives.
- **A note in the pull request for anything that changes the history table,
  the lock table, or the command line.** The history table lives in users'
  databases: add columns with defaults, never rename or retype one.

### Things to know

[`AGENTS.md`](AGENTS.md) has the conventions in full. The parts that are not
negotiable:

- **Nothing runs until the history check passes.**
- **Import only adds.** It never rewrites or removes a history row.
- **Progress is recorded per statement.** It is the reason the tool exists.
- **The lock is never expired on a timer.** `chx migrate unlock` is the only
  way a stale lock goes.
- **Cluster support is best effort, and says so.** A setup that cannot share
  the history falls back or warns. It never fails a run that would work on one
  node.
- **The checksum is plain SHA-256 of the file's bytes.** Users verify it with
  `sha256sum`, so the input is never normalised.
- **`.env` loads as sqlx-cli loads it**, never overriding the process environment.
- **Never use an em-dash or an en-dash** in code, comments or docs.

## Support

chx is free and Apache-2.0, and stays that way. If it saved you some trouble
and you feel like saying thanks,
[buy me a coffee](https://buymeacoffee.com/dallinwright).

## License

Licensed under the [Apache License, Version 2.0](LICENSE).

Contributions are accepted under the same license, per section 5 of the Apache
License: any contribution intentionally submitted for inclusion is licensed
Apache-2.0, with no additional terms.
