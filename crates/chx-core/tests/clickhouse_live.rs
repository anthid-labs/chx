//! Migrations against a real ClickHouse server.
//!
//! Skipped unless `CHX_TEST_CLICKHOUSE_URL` names one. Deliberately not
//! `CLICKHOUSE_URL`: on a developer machine that variable is very often a
//! tunnel to a production server, and a test suite that read the obvious name
//! would create databases there the first time somebody ran it.
//!
//! ```bash
//! docker run -d --rm -p 58123:8123 -e CLICKHOUSE_PASSWORD=chx clickhouse/clickhouse-server
//! CHX_TEST_CLICKHOUSE_URL=http://default:chx@127.0.0.1:58123 cargo test -p chx-core --test clickhouse_live
//! ```
//!
//! Each test creates its own database and drops it at the end, so they run in
//! parallel against one server without seeing each other.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use chx::client::Client;
use chx::error::Error;
use chx::migrate::Migrator;

/// The server, or `None` to skip.
fn server() -> Option<Client> {
    let url = std::env::var("CHX_TEST_CLICKHOUSE_URL")
        .ok()
        .filter(|url| !url.trim().is_empty())?;

    Some(Client::from_url(&url).expect("CHX_TEST_CLICKHOUSE_URL parses"))
}

/// A fresh database on the test server, dropped when the guard goes.
struct Scratch {
    server: Client,
    name: String,
    client: Client,
}

impl Scratch {
    async fn new(server: Client, test: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let name = format!("chx_test_{test}_{nanos}");

        server
            .execute(&format!("CREATE DATABASE {name}"))
            .await
            .expect("create scratch database");

        let client = server.with_database(&name);
        Self {
            server,
            name,
            client,
        }
    }

    async fn drop(self) {
        self.server
            .execute(&format!("DROP DATABASE IF EXISTS {}", self.name))
            .await
            .expect("drop scratch database");
    }

    async fn scalar(&self, sql: &str) -> String {
        self.client.query(sql).await.unwrap().trim().to_string()
    }
}

fn write(dir: &Path, name: &str, body: &str) {
    std::fs::write(dir.join(name), body).unwrap();
}

macro_rules! require_server {
    () => {
        match server() {
            Some(server) => server,
            None => {
                eprintln!("skipped: CHX_TEST_CLICKHOUSE_URL is not set");
                return;
            }
        }
    };
}

#[tokio::test]
async fn applies_in_order_then_is_a_no_op_then_appends() {
    let scratch = Scratch::new(require_server!(), "append").await;
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "1_create_trades.sql",
        "CREATE TABLE trades (id UInt64, px Decimal(18, 6)) ENGINE = MergeTree ORDER BY id;\n\
         -- a comment; with a semicolon\n\
         INSERT INTO trades VALUES (1, 1.5);",
    );
    write(
        dir.path(),
        "2_add_venue.sql",
        "ALTER TABLE trades ADD COLUMN venue String DEFAULT 'x;y'",
    );

    let first = Migrator::from_dir(dir.path())
        .unwrap()
        .run(&scratch.client, |_| {})
        .await
        .unwrap();
    let versions: Vec<_> = first.iter().map(|a| a.version).collect();
    assert_eq!(versions, [1, 2]);
    assert_eq!(first[0].statements, 2);
    assert_eq!(scratch.scalar("SELECT venue FROM trades").await, "x;y");

    let second = Migrator::from_dir(dir.path())
        .unwrap()
        .run(&scratch.client, |_| {})
        .await
        .unwrap();
    assert!(second.is_empty(), "second run applied {second:?}");

    write(
        dir.path(),
        "3_more.sql",
        "INSERT INTO trades (id, px) VALUES (2, 2)",
    );
    let third = Migrator::from_dir(dir.path())
        .unwrap()
        .run(&scratch.client, |_| {})
        .await
        .unwrap();
    assert_eq!(third.iter().map(|a| a.version).collect::<Vec<_>>(), [3]);
    assert_eq!(scratch.scalar("SELECT count() FROM trades").await, "2");

    scratch.drop().await;
}

#[tokio::test]
async fn an_edited_applied_migration_is_refused_before_anything_runs() {
    let scratch = Scratch::new(require_server!(), "edited").await;
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "1_init.sql",
        "CREATE TABLE t (x UInt8) ENGINE = MergeTree ORDER BY x",
    );
    Migrator::from_dir(dir.path())
        .unwrap()
        .run(&scratch.client, |_| {})
        .await
        .unwrap();

    write(
        dir.path(),
        "1_init.sql",
        "CREATE TABLE t (x UInt16) ENGINE = MergeTree ORDER BY x",
    );
    write(
        dir.path(),
        "2_next.sql",
        "CREATE TABLE u (x UInt8) ENGINE = MergeTree ORDER BY x",
    );
    let err = Migrator::from_dir(dir.path())
        .unwrap()
        .run(&scratch.client, |_| {})
        .await
        .unwrap_err();

    assert!(matches!(err, Error::History(_)), "{err}");
    assert_eq!(scratch.scalar("EXISTS TABLE u").await, "0");

    scratch.drop().await;
}

#[tokio::test]
async fn a_failed_migration_resumes_at_the_statement_that_failed() {
    let scratch = Scratch::new(require_server!(), "resume").await;
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "1_two_tables.sql",
        "CREATE TABLE a (x UInt8) ENGINE = MergeTree ORDER BY x;\n\
         CREATE TABLE b (x UInt8) ENGINE = MergeTree ORDER BY nope;\n\
         CREATE TABLE c (x UInt8) ENGINE = MergeTree ORDER BY x;",
    );
    write(dir.path(), "2_after.sql", "INSERT INTO a VALUES (1)");

    let err = Migrator::from_dir(dir.path())
        .unwrap()
        .run(&scratch.client, |_| {})
        .await
        .unwrap_err();
    match &err {
        Error::Statement {
            version,
            statement,
            total,
            ..
        } => assert_eq!((*version, *statement, *total), (1, 2, 3)),
        other => panic!("expected a statement error, got {other}"),
    }
    assert_eq!(scratch.scalar("EXISTS TABLE a").await, "1");
    assert_eq!(
        scratch
            .scalar("SELECT applied, success FROM _chx_migrations FINAL WHERE version = 1")
            .await,
        "1\tfalse"
    );

    // Fix the statement that failed. The one before it already ran, and
    // running it again would fail on `a` existing, which is the bug this
    // whole tool exists for.
    write(
        dir.path(),
        "1_two_tables.sql",
        "CREATE TABLE a (x UInt8) ENGINE = MergeTree ORDER BY x;\n\
         CREATE TABLE b (x UInt8) ENGINE = MergeTree ORDER BY x;\n\
         CREATE TABLE c (x UInt8) ENGINE = MergeTree ORDER BY x;",
    );
    let applied = Migrator::from_dir(dir.path())
        .unwrap()
        .run(&scratch.client, |_| {})
        .await
        .unwrap();

    assert_eq!(
        applied.iter().map(|a| a.version).collect::<Vec<_>>(),
        [1, 2]
    );
    assert_eq!(applied[0].resumed_at, 1);
    assert_eq!(scratch.scalar("EXISTS TABLE c").await, "1");
    assert_eq!(
        scratch
            .scalar("SELECT applied, success FROM _chx_migrations FINAL WHERE version = 1")
            .await,
        "3\ttrue"
    );

    // Now complete, so the fixed file is the one the checksum holds it to.
    let again = Migrator::from_dir(dir.path())
        .unwrap()
        .run(&scratch.client, |_| {})
        .await
        .unwrap();
    assert!(again.is_empty());

    scratch.drop().await;
}

#[tokio::test]
async fn an_empty_migration_is_recorded() {
    let scratch = Scratch::new(require_server!(), "empty").await;
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "1_placeholder.sql", "-- nothing yet\n");

    Migrator::from_dir(dir.path())
        .unwrap()
        .run(&scratch.client, |_| {})
        .await
        .unwrap();

    assert_eq!(
        scratch
            .scalar("SELECT statements, success FROM _chx_migrations FINAL")
            .await,
        "0\ttrue"
    );

    scratch.drop().await;
}
