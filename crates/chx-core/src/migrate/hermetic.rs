//! Migration runs against the in-process fake ClickHouse.
//!
//! `plan` and `import_plan` are pure and tested beside their definitions.
//! These tests cover what sits around them and needs a server to answer:
//! where the history table is put, the lock, the apply loop and what it
//! records, and what happens when a statement or the cluster refuses. The
//! same behaviour is proven against a real server in `tests/single_node.rs`
//! and `tests/cluster.rs`; here it runs in milliseconds, and the fake can be
//! told to fail in ways a real server will not do on demand.

use std::time::Duration;

use tempfile::TempDir;

use super::history::{self, Placement};
use super::{Migrator, lock, unlock};
use crate::error::Error;
use crate::testing::{FakeClickHouse, HistoryRow};

/// A migrations directory holding `files`, as `(name, sql)`.
fn migrations(files: &[(&str, &str)]) -> TempDir {
    let dir = tempfile::tempdir().expect("temp dir");
    for (name, sql) in files {
        std::fs::write(dir.path().join(name), sql).expect("write migration");
    }
    dir
}

fn two_files() -> TempDir {
    migrations(&[
        (
            "1_create_trades.sql",
            "CREATE TABLE trades (id UInt64) ENGINE = Memory;\nCREATE TABLE fills (id UInt64) ENGINE = Memory;",
        ),
        (
            "2_add_venue.sql",
            "ALTER TABLE trades ADD COLUMN venue String",
        ),
    ])
}

#[tokio::test]
async fn a_run_applies_every_statement_in_order_and_records_each_one() {
    let server = FakeClickHouse::start();
    let dir = two_files();
    let migrator = Migrator::from_dir(dir.path()).unwrap();
    let mut reported = Vec::new();

    let applied = migrator
        .run(&server.client(), |done| reported.push(done.version))
        .await
        .unwrap();

    assert_eq!(reported, [1, 2]);
    assert_eq!(applied.len(), 2);
    assert_eq!(applied[0].description, "create trades");
    assert_eq!(applied[0].statements, 2);
    assert_eq!(applied[0].resumed_at, 0);

    let state = server.state();
    assert_eq!(
        state.executed,
        [
            "CREATE TABLE trades (id UInt64) ENGINE = Memory",
            "CREATE TABLE fills (id UInt64) ENGINE = Memory",
            "ALTER TABLE trades ADD COLUMN venue String",
        ]
    );
    // One history row per statement, the last of each migration complete.
    let progress: Vec<(i64, u32, bool)> = state
        .history
        .iter()
        .map(|row| (row.version, row.applied, row.success))
        .collect();
    assert_eq!(progress, [(1, 1, false), (1, 2, true), (2, 1, true)]);
    assert_eq!(state.history[1].checksum, migrator.migrations()[0].checksum);
    assert_eq!(state.history_engine.as_deref(), Some("ReplacingMergeTree"));
    assert_eq!(state.lock, None, "the lock is released after a run");
}

#[tokio::test]
async fn a_second_run_finds_nothing_to_do() {
    let server = FakeClickHouse::start();
    let dir = two_files();
    let migrator = Migrator::from_dir(dir.path()).unwrap();
    let client = server.client();

    migrator.run(&client, |_| {}).await.unwrap();
    let again = migrator.run(&client, |_| {}).await.unwrap();

    assert!(again.is_empty());
    assert_eq!(server.state().executed.len(), 3);
}

#[tokio::test]
async fn a_failed_statement_is_recorded_and_the_next_run_resumes_there() {
    let server = FakeClickHouse::start();
    let dir = two_files();
    let migrator = Migrator::from_dir(dir.path()).unwrap();
    let client = server.client();
    server.state().fail_containing = Some("fills".to_string());

    let err = migrator.run(&client, |_| {}).await.unwrap_err();

    match &err {
        Error::Statement {
            version,
            statement,
            total,
            source,
            ..
        } => {
            assert_eq!((*version, *statement, *total), (1, 2, 2));
            assert!(matches!(**source, Error::ClickHouse { code: Some(62), .. }));
        }
        other => panic!("unexpected error: {other}"),
    }
    {
        let state = server.state();
        assert_eq!(state.executed.len(), 1, "only the first statement ran");
        assert_eq!(state.history.len(), 1);
        assert!(!state.history[0].success);
        assert_eq!(state.lock, None, "a failed run still releases the lock");
    }

    server.state().fail_containing = None;
    let applied = migrator.run(&client, |_| {}).await.unwrap();

    assert_eq!(applied[0].version, 1);
    assert_eq!(applied[0].resumed_at, 1);
    let state = server.state();
    // The statement that already ran is not sent again.
    assert_eq!(
        state
            .executed
            .iter()
            .filter(|sql| sql.contains("CREATE TABLE trades"))
            .count(),
        1
    );
    assert_eq!(state.executed.len(), 3);
}

#[tokio::test]
async fn a_changed_file_is_refused_before_the_lock_is_given_up_or_anything_runs() {
    let server = FakeClickHouse::start();
    let dir = two_files();
    let client = server.client();
    Migrator::from_dir(dir.path())
        .unwrap()
        .run(&client, |_| {})
        .await
        .unwrap();
    let before = server.state().executed.len();

    std::fs::write(dir.path().join("2_add_venue.sql"), "SELECT 'edited'").unwrap();
    let err = Migrator::from_dir(dir.path())
        .unwrap()
        .run(&client, |_| {})
        .await
        .unwrap_err();

    assert!(matches!(err, Error::History(_)), "{err}");
    let state = server.state();
    assert_eq!(state.executed.len(), before);
    assert_eq!(state.lock, None);
}

#[tokio::test]
async fn import_records_files_without_running_them() {
    let server = FakeClickHouse::start();
    let dir = two_files();
    let migrator = Migrator::from_dir(dir.path()).unwrap();
    let client = server.client();

    let imported = migrator.import(&client, Some(1)).await.unwrap();

    assert_eq!(imported.len(), 1);
    assert_eq!(imported[0].version, 1);
    assert_eq!(imported[0].statements, 2);
    {
        let state = server.state();
        assert!(state.executed.is_empty(), "import runs nothing");
        assert_eq!(
            state.history,
            [HistoryRow {
                version: 1,
                description: "create trades".to_string(),
                checksum: migrator.migrations()[0].checksum.clone(),
                statements: 2,
                applied: 2,
                success: true,
                // Zero is how an imported row is told from an applied one.
                execution_ms: 0,
            }]
        );
        assert_eq!(state.lock, None);
    }

    // Importing again adds nothing, and `run` applies only what is left.
    assert!(migrator.import(&client, Some(1)).await.unwrap().is_empty());
    let applied = migrator.run(&client, |_| {}).await.unwrap();
    assert_eq!(applied.len(), 1);
    assert_eq!(applied[0].version, 2);
}

#[tokio::test]
async fn import_without_a_bound_records_every_file() {
    let server = FakeClickHouse::start();
    let dir = two_files();

    let imported = Migrator::from_dir(dir.path())
        .unwrap()
        .import(&server.client(), None)
        .await
        .unwrap();

    assert_eq!(imported.len(), 2);
    assert_eq!(server.state().history.len(), 2);
}

#[tokio::test]
async fn import_refuses_an_incomplete_migration_and_releases_the_lock() {
    let server = FakeClickHouse::start();
    let dir = two_files();
    let migrator = Migrator::from_dir(dir.path()).unwrap();
    let client = server.client();
    server.state().fail_containing = Some("fills".to_string());
    migrator.run(&client, |_| {}).await.unwrap_err();

    let err = migrator.import(&client, None).await.unwrap_err();

    assert!(matches!(err, Error::History(_)), "{err}");
    assert!(err.to_string().contains("incomplete"), "{err}");
    assert_eq!(server.state().lock, None);
}

#[tokio::test]
async fn a_held_lock_fails_a_run_that_will_not_wait_and_names_the_holder() {
    let server = FakeClickHouse::start();
    let dir = two_files();
    server.state().lock = Some("chx 0.1.0 on ci-runner-7, pid 4121".to_string());

    let err = Migrator::from_dir(dir.path())
        .unwrap()
        .lock_timeout(Duration::ZERO)
        .run(&server.client(), |_| {})
        .await
        .unwrap_err();

    match err {
        Error::Locked { holder, since } => {
            assert_eq!(holder, "chx 0.1.0 on ci-runner-7, pid 4121");
            assert_eq!(since, "2026-10-12 14:00:00");
        }
        other => panic!("unexpected error: {other}"),
    }
    let state = server.state();
    assert!(state.executed.is_empty());
    assert!(state.lock.is_some(), "someone else's lock is left alone");
}

/// The clock is paused, so the one-second poll costs nothing. The fake gives
/// the lock up after it has been asked who holds it twice.
#[tokio::test(start_paused = true)]
async fn a_run_waits_for_the_lock_and_proceeds_when_it_is_released() {
    let server = FakeClickHouse::start();
    let dir = two_files();
    {
        let mut state = server.state();
        state.lock = Some("another run".to_string());
        state.release_after_holder_reads = Some(2);
    }

    let applied = Migrator::from_dir(dir.path())
        .unwrap()
        .lock_timeout(Duration::from_secs(60))
        .run(&server.client(), |_| {})
        .await
        .unwrap();

    assert_eq!(applied.len(), 2);
    assert_eq!(server.state().lock, None);
}

#[tokio::test(start_paused = true)]
async fn a_run_gives_up_on_the_lock_when_the_wait_runs_out() {
    let server = FakeClickHouse::start();
    let dir = two_files();
    server.state().lock = Some("a run that never ends".to_string());

    let err = Migrator::from_dir(dir.path())
        .unwrap()
        .lock_timeout(Duration::from_secs(3))
        .run(&server.client(), |_| {})
        .await
        .unwrap_err();

    assert!(matches!(err, Error::Locked { .. }), "{err}");
    assert!(err.to_string().contains("chx migrate unlock"), "{err}");
}

#[tokio::test]
async fn the_lock_is_taken_with_a_description_of_this_process() {
    let server = FakeClickHouse::start();
    let client = server.client();

    let held = lock::acquire(&client, &Placement::Local, Duration::ZERO)
        .await
        .unwrap();

    let holder = lock::holder(&client).await.unwrap().expect("lock is held");
    assert!(holder.description.starts_with("chx "), "{holder:?}");
    assert!(
        holder
            .description
            .contains(&format!("pid {}", std::process::id())),
        "{holder:?}"
    );
    held.release().await.unwrap();
    assert_eq!(lock::holder(&client).await.unwrap(), None);
}

#[tokio::test]
async fn unlock_reports_who_held_the_lock_or_that_nobody_did() {
    let server = FakeClickHouse::start();
    let client = server.client();

    assert_eq!(unlock(&client, None).await.unwrap(), None);

    server.state().lock = Some("chx 0.1.0 on ci-runner-7, pid 4121".to_string());
    let holder = unlock(&client, None).await.unwrap().expect("a holder");

    assert_eq!(holder.description, "chx 0.1.0 on ci-runner-7, pid 4121");
    assert_eq!(server.state().lock, None);
}

#[tokio::test]
async fn an_error_other_than_already_exists_fails_the_lock_at_once() {
    let server = FakeClickHouse::start();
    server.state().refuse_on_cluster = true;

    let err = lock::acquire(
        &server.client(),
        &Placement::OnCluster("prod".to_string()),
        Duration::from_secs(60),
    )
    .await
    .unwrap_err();

    assert!(
        matches!(
            err,
            Error::ClickHouse {
                code: Some(170),
                ..
            }
        ),
        "{err}"
    );
}

#[tokio::test]
async fn a_cluster_gets_a_replicated_history_and_an_on_cluster_lock() {
    let server = FakeClickHouse::start();
    let dir = two_files();
    {
        let mut state = server.state();
        state.nodes = 2;
        state.replicas = 2;
    }

    Migrator::from_dir(dir.path())
        .unwrap()
        .on_cluster("prod")
        .run(&server.client(), |_| {})
        .await
        .unwrap();

    let state = server.state();
    assert_eq!(
        state.history_engine.as_deref(),
        Some("ReplicatedReplacingMergeTree")
    );
    let sent = |fragment: &str| state.log.iter().any(|sql| sql.contains(fragment));
    assert!(sent(
        "CREATE TABLE IF NOT EXISTS _chx_migrations ON CLUSTER `prod`"
    ));
    assert!(sent("CREATE TABLE _chx_lock ON CLUSTER `prod`"));
    assert!(sent(
        "DROP TABLE IF EXISTS _chx_lock ON CLUSTER `prod` SYNC"
    ));
    // A replicated history is caught up before it is read.
    assert!(sent("SYSTEM SYNC REPLICA _chx_migrations"));
    // Statements from the files run exactly as written.
    assert!(state.executed.iter().all(|sql| !sql.contains("ON CLUSTER")));
}

#[tokio::test]
async fn a_cluster_that_refuses_falls_back_to_a_local_history() {
    let server = FakeClickHouse::start();
    let dir = two_files();
    server.state().refuse_on_cluster = true;

    let applied = Migrator::from_dir(dir.path())
        .unwrap()
        .on_cluster("prod")
        .run(&server.client(), |_| {})
        .await
        .unwrap();

    assert_eq!(applied.len(), 2);
    let state = server.state();
    assert_eq!(state.history_engine.as_deref(), Some("ReplacingMergeTree"));
    // With a local history the lock is local too.
    assert!(
        state
            .log
            .iter()
            .filter(|sql| sql.contains("_chx_lock"))
            .all(|sql| !sql.contains("ON CLUSTER"))
    );
}

#[tokio::test]
async fn placement_follows_the_database_and_the_table_that_exists() {
    // A plain server, no cluster asked for.
    let server = FakeClickHouse::start();
    assert_eq!(
        history::ensure_table(&server.client(), None).await.unwrap(),
        Placement::Local
    );

    // An existing local table stays local whatever a later run asks for.
    assert_eq!(
        history::ensure_table(&server.client(), Some("prod"))
            .await
            .unwrap(),
        Placement::Local
    );

    // A `Replicated` database replicates the DDL itself.
    let replicated = FakeClickHouse::start();
    {
        let mut state = replicated.state();
        state.database_engine = "Replicated".to_string();
        state.nodes = 3;
        state.replicas = 1;
    }
    assert_eq!(
        history::ensure_table(&replicated.client(), None)
            .await
            .unwrap(),
        Placement::Replicated
    );
    assert!(
        replicated
            .state()
            .log
            .iter()
            .any(|sql| sql.contains("ENGINE = ReplicatedReplacingMergeTree(applied)"))
    );

    // ClickHouse Cloud shares a plain engine by itself.
    let cloud = FakeClickHouse::start();
    cloud.state().database_engine = "Shared".to_string();
    assert_eq!(
        history::ensure_table(&cloud.client(), Some("ignored"))
            .await
            .unwrap(),
        Placement::Replicated
    );

    // A replicated table found by a run that was not told the cluster.
    let orphan = FakeClickHouse::start();
    orphan.state().history_engine = Some("ReplicatedReplacingMergeTree".to_string());
    assert_eq!(
        history::ensure_table(&orphan.client(), None).await.unwrap(),
        Placement::Replicated
    );
}

#[tokio::test]
async fn history_rows_read_back_as_the_latest_per_version() {
    let server = FakeClickHouse::start();
    let client = server.client();
    let placement = history::ensure_table(&client, None).await.unwrap();
    let mut record = history::Record {
        version: 7,
        description: "it's quoted".to_string(),
        checksum: "abc".to_string(),
        statements: 2,
        applied: 1,
        success: false,
    };

    history::record(&client, &record, 5).await.unwrap();
    record.applied = 2;
    record.success = true;
    history::record(&client, &record, 9).await.unwrap();

    assert_eq!(history::read(&client, &placement).await.unwrap(), [record]);
}

#[tokio::test]
async fn an_unreadable_history_row_is_an_internal_error() {
    let fake = crate::testing::Fake::start(|request| {
        if request.sql().contains("FINAL") {
            crate::testing::Response::ok("not\ta\trow\n")
        } else {
            crate::testing::Response::empty()
        }
    });

    let err = history::read(&fake.client(), &Placement::Local)
        .await
        .unwrap_err();

    assert!(matches!(err, Error::Internal(_)), "{err}");
}
