//! Migrations against one real ClickHouse server, the `single` service in
//! `docker/compose.yaml`.
//!
//! Ignored by default, because they start containers:
//!
//! ```bash
//! cargo test -p chx-core --test single_node -- --ignored
//! ```

mod common;

use std::time::Duration;

use chx::error::Error;
use chx::migrate::{self, Migrator, Placement, lock};

use common::{SINGLE, Scratch, compose_up, write};

async fn scratch(test: &str) -> Scratch {
    compose_up(&["single"]);
    Scratch::new(SINGLE, test).await
}

#[tokio::test]
#[ignore = "needs Docker: cargo test -p chx-core -- --ignored"]
async fn applies_in_order_then_is_a_no_op_then_appends() {
    let scratch = scratch("append").await;
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
        .run(scratch.client(), |_| {})
        .await
        .unwrap();
    let versions: Vec<_> = first.iter().map(|a| a.version).collect();
    assert_eq!(versions, [1, 2]);
    assert_eq!(first[0].statements, 2);
    assert_eq!(scratch.scalar(0, "SELECT venue FROM trades").await, "x;y");

    let second = Migrator::from_dir(dir.path())
        .unwrap()
        .run(scratch.client(), |_| {})
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
        .run(scratch.client(), |_| {})
        .await
        .unwrap();
    assert_eq!(third.iter().map(|a| a.version).collect::<Vec<_>>(), [3]);
    assert_eq!(scratch.scalar(0, "SELECT count() FROM trades").await, "2");
    assert_eq!(scratch.history_engine(0).await, "ReplacingMergeTree");

    scratch.drop().await;
}

#[tokio::test]
#[ignore = "needs Docker: cargo test -p chx-core -- --ignored"]
async fn an_edited_applied_migration_is_refused_before_anything_runs() {
    let scratch = scratch("edited").await;
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "1_init.sql",
        "CREATE TABLE t (x UInt8) ENGINE = MergeTree ORDER BY x",
    );
    Migrator::from_dir(dir.path())
        .unwrap()
        .run(scratch.client(), |_| {})
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
        .run(scratch.client(), |_| {})
        .await
        .unwrap_err();

    assert!(matches!(err, Error::History(_)), "{err}");
    assert_eq!(scratch.scalar(0, "EXISTS TABLE u").await, "0");

    scratch.drop().await;
}

#[tokio::test]
#[ignore = "needs Docker: cargo test -p chx-core -- --ignored"]
async fn a_failed_migration_resumes_at_the_statement_that_failed() {
    let scratch = scratch("resume").await;
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "1_three_tables.sql",
        "CREATE TABLE a (x UInt8) ENGINE = MergeTree ORDER BY x;\n\
         CREATE TABLE b (x UInt8) ENGINE = MergeTree ORDER BY nope;\n\
         CREATE TABLE c (x UInt8) ENGINE = MergeTree ORDER BY x;",
    );
    write(dir.path(), "2_after.sql", "INSERT INTO a VALUES (1)");

    let err = Migrator::from_dir(dir.path())
        .unwrap()
        .run(scratch.client(), |_| {})
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
    assert_eq!(scratch.scalar(0, "EXISTS TABLE a").await, "1");
    assert_eq!(
        scratch
            .scalar(
                0,
                "SELECT applied, success FROM _chx_migrations FINAL WHERE version = 1"
            )
            .await,
        "1\tfalse"
    );

    // Fix the statement that failed. The one before it already ran, and
    // running it again would fail on `a` existing, which is the bug this
    // whole tool exists for.
    write(
        dir.path(),
        "1_three_tables.sql",
        "CREATE TABLE a (x UInt8) ENGINE = MergeTree ORDER BY x;\n\
         CREATE TABLE b (x UInt8) ENGINE = MergeTree ORDER BY x;\n\
         CREATE TABLE c (x UInt8) ENGINE = MergeTree ORDER BY x;",
    );
    let applied = Migrator::from_dir(dir.path())
        .unwrap()
        .run(scratch.client(), |_| {})
        .await
        .unwrap();

    assert_eq!(
        applied.iter().map(|a| a.version).collect::<Vec<_>>(),
        [1, 2]
    );
    assert_eq!(applied[0].resumed_at, 1);
    assert_eq!(scratch.scalar(0, "EXISTS TABLE c").await, "1");
    assert_eq!(
        scratch
            .scalar(
                0,
                "SELECT applied, success FROM _chx_migrations FINAL WHERE version = 1"
            )
            .await,
        "3\ttrue"
    );

    // Now complete, so the fixed file is the one the checksum holds it to.
    let again = Migrator::from_dir(dir.path())
        .unwrap()
        .run(scratch.client(), |_| {})
        .await
        .unwrap();
    assert!(again.is_empty());

    scratch.drop().await;
}

#[tokio::test]
#[ignore = "needs Docker: cargo test -p chx-core -- --ignored"]
async fn an_empty_migration_is_recorded() {
    let scratch = scratch("empty").await;
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "1_placeholder.sql", "-- nothing yet\n");

    Migrator::from_dir(dir.path())
        .unwrap()
        .run(scratch.client(), |_| {})
        .await
        .unwrap();

    assert_eq!(
        scratch
            .scalar(0, "SELECT statements, success FROM _chx_migrations FINAL")
            .await,
        "0\ttrue"
    );

    scratch.drop().await;
}

/// This server has no Keeper and no cluster by that name, so `ON CLUSTER` is
/// refused and the history stays local rather than the run failing.
#[tokio::test]
#[ignore = "needs Docker: cargo test -p chx-core -- --ignored"]
async fn an_unusable_cluster_falls_back_to_a_local_history() {
    let scratch = scratch("nocluster").await;
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "1_init.sql", "SELECT 1");

    let applied = Migrator::from_dir(dir.path())
        .unwrap()
        .on_cluster("chx_no_such_cluster")
        .run(scratch.client(), |_| {})
        .await
        .unwrap();

    assert_eq!(applied.len(), 1);
    assert_eq!(scratch.history_engine(0).await, "ReplacingMergeTree");

    scratch.drop().await;
}

#[tokio::test]
#[ignore = "needs Docker: cargo test -p chx-core -- --ignored"]
async fn a_held_lock_stops_a_run_until_it_is_released() {
    let scratch = scratch("locked").await;
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "1_init.sql",
        "CREATE TABLE t (x UInt8) ENGINE = MergeTree ORDER BY x",
    );

    // Taken and never released: a run that died holding it.
    let _dead = lock::acquire(scratch.client(), &Placement::Local, Duration::ZERO)
        .await
        .unwrap();

    let err = Migrator::from_dir(dir.path())
        .unwrap()
        .lock_timeout(Duration::from_secs(1))
        .run(scratch.client(), |_| {})
        .await
        .unwrap_err();
    match &err {
        Error::Locked { holder, .. } => {
            assert!(
                holder.contains(&format!("pid {}", std::process::id())),
                "{holder}"
            )
        }
        other => panic!("expected a lock error, got {other}"),
    }
    assert_eq!(scratch.scalar(0, "EXISTS TABLE t").await, "0");

    let released = migrate::unlock(scratch.client(), None).await.unwrap();
    assert!(released.is_some());
    assert!(
        migrate::unlock(scratch.client(), None)
            .await
            .unwrap()
            .is_none()
    );

    let applied = Migrator::from_dir(dir.path())
        .unwrap()
        .run(scratch.client(), |_| {})
        .await
        .unwrap();
    assert_eq!(applied.len(), 1);

    scratch.drop().await;
}

#[tokio::test]
#[ignore = "needs Docker: cargo test -p chx-core -- --ignored"]
async fn concurrent_runs_apply_each_migration_once() {
    let scratch = scratch("race").await;
    let dir = tempfile::tempdir().unwrap();
    // The sleep keeps the first run inside the lock long enough that the
    // second is certain to arrive while it is held.
    write(
        dir.path(),
        "1_slow.sql",
        "CREATE TABLE t (x UInt8) ENGINE = MergeTree ORDER BY x;\nSELECT sleep(1);",
    );
    let one = Migrator::from_dir(dir.path()).unwrap();
    let two = Migrator::from_dir(dir.path()).unwrap();

    let (first, second) = tokio::join!(
        one.run(scratch.client(), |_| {}),
        two.run(scratch.client(), |_| {})
    );

    let mut counts = [first.unwrap().len(), second.unwrap().len()];
    counts.sort();
    assert_eq!(counts, [0, 1]);
    assert_eq!(
        scratch
            .scalar(0, "SELECT count() FROM _chx_migrations FINAL")
            .await,
        "1"
    );

    scratch.drop().await;
}

#[tokio::test]
#[ignore = "needs Docker: cargo test -p chx-core -- --ignored"]
async fn import_records_a_hand_built_database_without_running_anything() {
    let scratch = scratch("import").await;
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "1_trades.sql",
        "CREATE TABLE trades (id UInt64) ENGINE = MergeTree ORDER BY id;\n\
         INSERT INTO trades VALUES (1);",
    );
    write(
        dir.path(),
        "2_venues.sql",
        "CREATE TABLE venues (id UInt64) ENGINE = MergeTree ORDER BY id",
    );

    // Built by hand, and the insert was never done.
    scratch
        .client()
        .execute("CREATE TABLE trades (id UInt64) ENGINE = MergeTree ORDER BY id")
        .await
        .unwrap();

    let imported = Migrator::from_dir(dir.path())
        .unwrap()
        .import(scratch.client(), Some(1))
        .await
        .unwrap();
    assert_eq!(imported.iter().map(|i| i.version).collect::<Vec<_>>(), [1]);
    assert_eq!(imported[0].statements, 2);

    // Recorded as a complete run would, and nothing in the file ran.
    assert_eq!(
        scratch
            .scalar(
                0,
                "SELECT applied, statements, success, execution_ms \
                 FROM _chx_migrations FINAL WHERE version = 1"
            )
            .await,
        "2\t2\ttrue\t0"
    );
    assert_eq!(scratch.scalar(0, "SELECT count() FROM trades").await, "0");

    // Again: nothing new to record.
    let again = Migrator::from_dir(dir.path())
        .unwrap()
        .import(scratch.client(), Some(1))
        .await
        .unwrap();
    assert!(again.is_empty());

    // `run` treats it as applied, and applies only what is after it.
    let applied = Migrator::from_dir(dir.path())
        .unwrap()
        .run(scratch.client(), |_| {})
        .await
        .unwrap();
    assert_eq!(applied.iter().map(|a| a.version).collect::<Vec<_>>(), [2]);
    assert_eq!(scratch.scalar(0, "EXISTS TABLE venues").await, "1");
    assert_eq!(scratch.scalar(0, "EXISTS TABLE _chx_lock").await, "0");

    scratch.drop().await;
}

#[tokio::test]
#[ignore = "needs Docker: cargo test -p chx-core -- --ignored"]
async fn import_never_overwrites_recorded_history() {
    let scratch = scratch("importkeep").await;
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "1_init.sql",
        "CREATE TABLE t (x UInt8) ENGINE = MergeTree ORDER BY x",
    );
    Migrator::from_dir(dir.path())
        .unwrap()
        .run(scratch.client(), |_| {})
        .await
        .unwrap();
    let before = scratch
        .scalar(
            0,
            "SELECT checksum FROM _chx_migrations FINAL WHERE version = 1",
        )
        .await;

    write(
        dir.path(),
        "1_init.sql",
        "CREATE TABLE t (x UInt16) ENGINE = MergeTree ORDER BY x",
    );
    write(dir.path(), "2_next.sql", "SELECT 1");
    let err = Migrator::from_dir(dir.path())
        .unwrap()
        .import(scratch.client(), None)
        .await
        .unwrap_err();

    assert!(matches!(err, Error::History(_)), "{err}");
    // All or nothing: the conflict on 1 means 2 was not recorded either.
    assert_eq!(
        scratch
            .scalar(0, "SELECT groupArray(checksum) FROM _chx_migrations FINAL")
            .await,
        format!("['{before}']")
    );

    scratch.drop().await;
}
