//! Queries, parameters and bulk inserts against one real ClickHouse server,
//! the `single` service in `docker/compose.yaml`.
//!
//! Every value type goes to the server one way and comes back the other: as
//! RowBinary in an insert and as a parameter in a query, read back through
//! `RowBinaryWithNamesAndTypes`. What these tests prove is that the byte
//! layouts and text forms chx writes are the ones ClickHouse reads. The unit
//! tests beside each codec pin those layouts down byte by byte; only a real
//! server can say they are the right ones.
//!
//! Each test works in a scratch database of its own, created by
//! [`common::Scratch`] and dropped at the end, so tests run in parallel and
//! leave nothing behind. Tests that need no table use the server directly.
//!
//! Ignored by default, because they start containers:
//!
//! ```bash
//! cargo test -p chx-core --test query -- --ignored
//! ```
//!
//! | Test | Proves |
//! | --- | --- |
//! | `every_type_roundtrips_through_insert_and_select` | RowBinary encode and decode, for every mapped type |
//! | `parameters_are_read_as_their_declared_types` | The parameter text form, for every mapped type |
//! | `fetch_one_optional_and_scalar` | `fetch_*`, `query_scalar`, tuples, `Row::try_get` |
//! | `expected_columns_catch_a_drifted_result` | `Error::Schema` on a result that changed shape |
//! | `errors_from_the_server_and_before_it` | Which error each kind of failure produces |
//! | `inserts_flush_in_batches_and_refuse_a_misfit_row` | Batching, defaults, and refusals in `Insert` |
//! | `describe_table_reads_the_schema` | The schema an insert encodes against |

mod common;

use std::collections::BTreeMap;
use std::net::{Ipv4Addr, Ipv6Addr};

use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use chx::types::Decimal;
use chx::{Error, FromRow, query, query_as, query_scalar};
use uuid::Uuid;

use common::{SINGLE, Scratch, compose_up};

const PROBE: &str = "CREATE TABLE probe (
    id UInt64,
    venue LowCardinality(String),
    px Decimal(38, 18),
    ts DateTime64(9, 'UTC'),
    day Date,
    tags Array(String),
    note Nullable(String),
    flags Map(String, UInt8),
    pair Tuple(UInt8, String),
    kind Enum8('buy' = 1, 'sell' = 2),
    ok Bool,
    uid UUID,
    ip4 IPv4,
    ip6 IPv6,
    fixed FixedString(4),
    big Int128
) ENGINE = MergeTree ORDER BY id";

const COLUMNS: [&str; 16] = [
    "id", "venue", "px", "ts", "day", "tags", "note", "flags", "pair", "kind", "ok", "uid", "ip4",
    "ip6", "fixed", "big",
];

#[derive(Debug, Clone, PartialEq, FromRow)]
struct Probe {
    id: u64,
    venue: String,
    px: Decimal,
    ts: DateTime<Utc>,
    day: NaiveDate,
    tags: Vec<String>,
    note: Option<String>,
    flags: BTreeMap<String, u8>,
    pair: (u8, String),
    kind: String,
    ok: bool,
    uid: Uuid,
    ip4: Ipv4Addr,
    ip6: Ipv6Addr,
    fixed: [u8; 4],
    big: i128,
}

fn probes() -> Vec<Probe> {
    vec![
        Probe {
            id: 1,
            venue: "NYSE".to_string(),
            px: Decimal::new(123_456_789_012_345_678_901_234_567, 18),
            ts: Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap()
                + chrono::Duration::nanoseconds(123_456_789),
            day: NaiveDate::from_ymd_opt(2026, 1, 2).unwrap(),
            tags: vec![
                "a\tb".to_string(),
                "it's".to_string(),
                "back\\slash".to_string(),
            ],
            note: Some("line\nbreak".to_string()),
            flags: BTreeMap::from([("x".to_string(), 1), ("y".to_string(), 2)]),
            pair: (7, "seven".to_string()),
            kind: "sell".to_string(),
            ok: true,
            uid: "0264ef6b-85d9-4be7-b403-bb5c73e04fcb".parse().unwrap(),
            ip4: Ipv4Addr::new(10, 1, 2, 3),
            ip6: "2001:db8::1".parse().unwrap(),
            fixed: *b"ab\0\0",
            big: -170_141_183_460_469_231_731_687_303_715_884_105_728,
        },
        Probe {
            id: 2,
            venue: "ARCA".to_string(),
            px: Decimal::new(-5, 18),
            ts: Utc.timestamp_opt(0, 0).unwrap(),
            day: NaiveDate::from_ymd_opt(1970, 1, 1).unwrap(),
            tags: vec![],
            note: None,
            flags: BTreeMap::new(),
            pair: (0, String::new()),
            kind: "buy".to_string(),
            ok: false,
            uid: Uuid::nil(),
            ip4: Ipv4Addr::UNSPECIFIED,
            ip6: Ipv6Addr::UNSPECIFIED,
            fixed: *b"wxyz",
            big: 170_141_183_460_469_231_731_687_303_715_884_105_727,
        },
    ]
}

async fn probe_table(test: &str) -> Scratch {
    compose_up(&["single"]);
    let scratch = Scratch::new(SINGLE, test).await;
    scratch.client().execute(PROBE).await.unwrap();
    scratch
}

async fn fill(scratch: &Scratch) {
    let mut insert = scratch.client().insert("probe", &COLUMNS).await.unwrap();
    for p in probes() {
        insert
            .write((
                p.id, &p.venue, p.px, p.ts, p.day, &p.tags, &p.note, &p.flags, &p.pair, &p.kind,
                p.ok, p.uid, p.ip4, p.ip6, p.fixed, p.big,
            ))
            .unwrap();
    }
    assert_eq!(insert.end().await.unwrap(), 2);
}

/// The widest proof in the suite: one table with a column of every type chx
/// maps, two rows chosen for their edges (extreme integers, a negative
/// decimal, the epoch, an empty array and map, a NULL, strings with tabs,
/// quotes, backslashes and newlines), written with `Client::insert` and read
/// back with `query_as` into a `#[derive(FromRow)]` struct.
///
/// Equality of what went in and what came out means the RowBinary encoder and
/// decoder agree with the server on every layout, not only with each other:
/// the server parsed the bytes into real columns in between.
#[tokio::test]
#[ignore = "needs Docker: cargo test -p chx-core -- --ignored"]
async fn every_type_roundtrips_through_insert_and_select() {
    let scratch = probe_table("roundtrip").await;
    fill(&scratch).await;

    let rows: Vec<Probe> = query_as("SELECT * FROM probe ORDER BY id")
        .fetch_all(scratch.client())
        .await
        .unwrap();
    assert_eq!(rows, probes());

    scratch.drop().await;
}

/// Every parameter type, sent as `param_<name>` text and echoed back by the
/// server as a column of that type.
///
/// This is the proof of the text form, which the unit tests can only assert
/// against themselves: that a bare string survives TSV escaping, that strings
/// inside an array are quoted and escaped the way the server unquotes them,
/// that `\N` is NULL at the top level, and that a timestamp sent as a Unix
/// number lands on the same instant whatever display zone the type names.
#[tokio::test]
#[ignore = "needs Docker: cargo test -p chx-core -- --ignored"]
async fn parameters_are_read_as_their_declared_types() {
    compose_up(&["single"]);
    let client = common::client(SINGLE);

    #[derive(Debug, PartialEq, FromRow)]
    struct Echo {
        s: String,
        n: Option<u8>,
        a: Vec<String>,
        m: Vec<(String, u8)>,
        t: (u8, String),
        d: DateTime<Utc>,
        dt: DateTime<Utc>,
        day: NaiveDate,
        dec: Decimal,
        u: Uuid,
        ip: Ipv4Addr,
        b: bool,
        e: String,
        i: i128,
        fs: String,
        nn: Option<String>,
    }

    let at =
        Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap() + chrono::Duration::milliseconds(678);
    let uid = Uuid::new_v4();
    let echo: Echo = query_as(
        "SELECT {s:String} AS s, {n:Nullable(UInt8)} AS n, {a:Array(String)} AS a, \
         {m:Map(String, UInt8)} AS m, {t:Tuple(UInt8, String)} AS t, {d:DateTime64(3)} AS d, \
         {dt:DateTime('America/New_York')} AS dt, {day:Date} AS day, {dec:Decimal(18, 4)} AS dec, \
         {u:UUID} AS u, {ip:IPv4} AS ip, {b:Bool} AS b, {e:Enum8('a' = 1, 'b' = 2)} AS e, \
         {i:Int128} AS i, {fs:FixedString(4)} AS fs, {nn:Nullable(String)} AS nn",
    )
    .bind("s", "tab\tquote'back\\slash\nnewline")
    .bind("n", &Some(7u8))
    .bind("a", &["x'y", "z\\w", "tab\tbed"][..])
    .bind("m", &BTreeMap::from([("k", 1u8), ("it's", 2u8)]))
    .bind("t", &(1u8, "one"))
    .bind("d", &at)
    .bind("dt", &at)
    .bind("day", &NaiveDate::from_ymd_opt(2026, 1, 2).unwrap())
    .bind("dec", &Decimal::new(15, 1))
    .bind("u", &uid)
    .bind("ip", &Ipv4Addr::new(1, 2, 3, 4))
    .bind("b", &true)
    .bind("e", "b")
    .bind("i", &i128::MIN)
    .bind("fs", "ab")
    .bind("nn", &None::<String>)
    .fetch_one(&client)
    .await
    .unwrap();

    assert_eq!(
        echo,
        Echo {
            s: "tab\tquote'back\\slash\nnewline".to_string(),
            n: Some(7),
            a: vec![
                "x'y".to_string(),
                "z\\w".to_string(),
                "tab\tbed".to_string()
            ],
            // In the map's own order, which for a BTreeMap is sorted.
            m: vec![("it's".to_string(), 2), ("k".to_string(), 1)],
            t: (1, "one".to_string()),
            d: at,
            // The display zone of the column does not move the instant.
            dt: Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap(),
            day: NaiveDate::from_ymd_opt(2026, 1, 2).unwrap(),
            dec: Decimal::new(15000, 4),
            u: uid,
            ip: Ipv4Addr::new(1, 2, 3, 4),
            b: true,
            e: "b".to_string(),
            i: i128::MIN,
            fs: "ab".to_string(),
            nn: None,
        }
    );
}

/// The fetch methods against real rows: `query_scalar` for one value and for a
/// column, `fetch_optional` returning `None`, `fetch_one` failing with
/// `RowNotFound`, tuples by position, and `Row::try_get` by name with the
/// column types the server reported.
#[tokio::test]
#[ignore = "needs Docker: cargo test -p chx-core -- --ignored"]
async fn fetch_one_optional_and_scalar() {
    let scratch = probe_table("fetch").await;
    fill(&scratch).await;
    let client = scratch.client();

    let count: u64 = query_scalar("SELECT count() FROM probe")
        .fetch_one(client)
        .await
        .unwrap();
    assert_eq!(count, 2);

    let venues: Vec<String> = query_scalar("SELECT venue FROM probe ORDER BY id")
        .fetch_all(client)
        .await
        .unwrap();
    assert_eq!(venues, ["NYSE", "ARCA"]);

    let none: Option<Probe> = query_as("SELECT * FROM probe WHERE id = {id:UInt64}")
        .bind("id", &99u64)
        .fetch_optional(client)
        .await
        .unwrap();
    assert!(none.is_none());

    let err = query("SELECT * FROM probe WHERE id = {id:UInt64}")
        .bind("id", &99u64)
        .fetch_one(client)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::RowNotFound), "{err}");

    let one: (u64, String) = query_as("SELECT id, venue FROM probe WHERE venue = {v:String}")
        .bind("v", "ARCA")
        .fetch_one(client)
        .await
        .unwrap();
    assert_eq!(one, (2, "ARCA".to_string()));

    let row = query("SELECT id, note FROM probe WHERE id = {id:UInt64}")
        .bind("id", &1u64)
        .fetch_one(client)
        .await
        .unwrap();
    assert_eq!(row.try_get::<u64, _>("id").unwrap(), 1);
    assert_eq!(
        row.try_get::<Option<String>, _>("note").unwrap().as_deref(),
        Some("line\nbreak")
    );
    assert_eq!(row.columns()[1].ty.to_string(), "Nullable(String)");

    scratch.drop().await;
}

/// `expect_columns` is the run-time half of the compile-time check. The same
/// query with one alias changed must fail with `Error::Schema` naming the
/// column the server actually sent, before any row is decoded.
#[tokio::test]
#[ignore = "needs Docker: cargo test -p chx-core -- --ignored"]
async fn expected_columns_catch_a_drifted_result() {
    compose_up(&["single"]);
    let client = common::client(SINGLE);

    let ok = query("SELECT 1 AS a, 2 AS b")
        .expect_columns(&["a", "b"])
        .fetch_one(&client)
        .await;
    assert!(ok.is_ok());

    let err = query("SELECT 1 AS a, 2 AS renamed")
        .expect_columns(&["a", "b"])
        .fetch_one(&client)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Schema(_)), "{err}");
    assert!(err.to_string().contains("renamed"), "{err}");
}

/// The three places a query can fail, each with its own error: the server
/// refusing it (`ClickHouse`, with the exception code, here 60 for an unknown
/// table), chx refusing to send it (`Parameter`, for one left unbound), and a
/// column read as a type it is not (`Decode`, naming the real type).
#[tokio::test]
#[ignore = "needs Docker: cargo test -p chx-core -- --ignored"]
async fn errors_from_the_server_and_before_it() {
    compose_up(&["single"]);
    let client = common::client(SINGLE);

    let err = query("SELECT * FROM no_such_table")
        .fetch_all(&client)
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::ClickHouse { code: Some(60), .. }),
        "{err}"
    );

    let err = query("SELECT {a:UInt64}")
        .fetch_all(&client)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Parameter(_)), "{err}");

    let row = query("SELECT 1 AS a").fetch_one(&client).await.unwrap();
    let err = row.try_get::<String, _>("a").unwrap_err();
    assert!(err.to_string().contains("UInt8"), "{err}");
}

/// An insert over several flushes: each `flush` is one request, a row of the
/// wrong type is refused by `write` without disturbing the batch, columns left
/// out of the insert take their table defaults, a column the table lacks is
/// refused when the insert opens, and an insert with no rows sends nothing.
#[tokio::test]
#[ignore = "needs Docker: cargo test -p chx-core -- --ignored"]
async fn inserts_flush_in_batches_and_refuse_a_misfit_row() {
    let scratch = probe_table("insert").await;
    let client = scratch.client();

    let mut insert = client.insert("probe", &["id", "venue"]).await.unwrap();
    assert_eq!(insert.columns()[1].ty.to_string(), "LowCardinality(String)");
    for id in 1u64..=3 {
        insert.write((id, "A")).unwrap();
    }
    assert_eq!(insert.flush().await.unwrap(), 3);
    assert_eq!(insert.buffered(), 0);
    insert.write((4u64, "B")).unwrap();
    let err = insert.write(("five", "B")).unwrap_err();
    assert!(err.to_string().contains("\"id\" is UInt64"), "{err}");
    assert_eq!(insert.buffered(), 1);
    assert_eq!(insert.end().await.unwrap(), 4);

    let ids: Vec<u64> = query_scalar("SELECT id FROM probe ORDER BY id")
        .fetch_all(client)
        .await
        .unwrap();
    assert_eq!(ids, [1, 2, 3, 4]);
    // Columns not written take their defaults.
    let px: Decimal = query_scalar("SELECT px FROM probe WHERE id = 1")
        .fetch_one(client)
        .await
        .unwrap();
    assert_eq!(px, Decimal::new(0, 18));

    let err = client.insert("probe", &["id", "nope"]).await.unwrap_err();
    assert!(matches!(err, Error::ColumnNotFound(_)), "{err}");

    let empty = client.insert("probe", &["id"]).await.unwrap();
    assert_eq!(empty.end().await.unwrap(), 0);

    scratch.drop().await;
}

/// `describe_table` is what an insert encodes against, so it has to return the
/// table's columns in table order with types the model parses, parameters and
/// all.
#[tokio::test]
#[ignore = "needs Docker: cargo test -p chx-core -- --ignored"]
async fn describe_table_reads_the_schema() {
    let scratch = probe_table("describe").await;
    let columns = chx::insert::describe_table(scratch.client(), "probe")
        .await
        .unwrap();
    let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, COLUMNS);
    assert_eq!(columns[3].ty.to_string(), "DateTime64(9, 'UTC')");
    scratch.drop().await;
}
