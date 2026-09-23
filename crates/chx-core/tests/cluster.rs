//! Migrations against a two-node cluster, `ch-1` and `ch-2` in
//! `docker/compose.yaml`: two shards of one replica each, sharing one Keeper.
//!
//! What these prove is that the history is one list for the whole cluster. A
//! run through either node sees what a run through the other one did, which
//! is what stops a deploy that lands on a different node from starting over.
//!
//! Ignored by default, because they start containers:
//!
//! ```bash
//! cargo test -p chx-core --test cluster -- --ignored
//! ```

mod common;

use std::time::Duration;

use chx::error::Error;
use chx::migrate::{self, Migrator, Placement, lock};

use common::{CH_1, CH_2, CLUSTER, Scratch, compose_up, write};

const NODES: [&str; 2] = [CH_1, CH_2];

fn up() {
    compose_up(&["keeper", "ch-1", "ch-2"]);
}

#[tokio::test]
#[ignore = "needs Docker: cargo test -p chx-core -- --ignored"]
async fn history_written_through_one_node_is_seen_through_the_other() {
    up();
    let scratch = Scratch::on_cluster(&NODES, "shared").await;
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "1_trades.sql",
        "CREATE TABLE trades ON CLUSTER chx (id UInt64) ENGINE = MergeTree ORDER BY id",
    );

    let first = Migrator::from_dir(dir.path())
        .unwrap()
        .on_cluster(CLUSTER)
        .run(&scratch.on[0], |_| {})
        .await
        .unwrap();
    assert_eq!(first.len(), 1);

    for node in 0..2 {
        assert_eq!(
            scratch.history_engine(node).await,
            "ReplicatedReplacingMergeTree",
            "node {node}"
        );
        assert_eq!(
            scratch.scalar(node, "EXISTS TABLE trades").await,
            "1",
            "node {node}"
        );
    }

    // Through the other node: nothing to do, and a new file is appended.
    let second = Migrator::from_dir(dir.path())
        .unwrap()
        .on_cluster(CLUSTER)
        .run(&scratch.on[1], |_| {})
        .await
        .unwrap();
    assert!(second.is_empty(), "ch-2 re-ran {second:?}");

    write(
        dir.path(),
        "2_venue.sql",
        "ALTER TABLE trades ON CLUSTER chx ADD COLUMN venue String",
    );
    let third = Migrator::from_dir(dir.path())
        .unwrap()
        .on_cluster(CLUSTER)
        .run(&scratch.on[1], |_| {})
        .await
        .unwrap();
    assert_eq!(third.iter().map(|a| a.version).collect::<Vec<_>>(), [2]);

    let back_on_first = Migrator::from_dir(dir.path())
        .unwrap()
        .on_cluster(CLUSTER)
        .run(&scratch.on[0], |_| {})
        .await
        .unwrap();
    assert!(back_on_first.is_empty(), "ch-1 re-ran {back_on_first:?}");

    scratch.drop().await;
}

#[tokio::test]
#[ignore = "needs Docker: cargo test -p chx-core -- --ignored"]
async fn a_migration_that_failed_on_one_node_resumes_on_the_other() {
    up();
    let scratch = Scratch::on_cluster(&NODES, "resume").await;
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "1_two_tables.sql",
        "CREATE TABLE a ON CLUSTER chx (x UInt8) ENGINE = MergeTree ORDER BY x;\n\
         CREATE TABLE b ON CLUSTER chx (x UInt8) ENGINE = MergeTree ORDER BY nope;",
    );

    let err = Migrator::from_dir(dir.path())
        .unwrap()
        .on_cluster(CLUSTER)
        .run(&scratch.on[0], |_| {})
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::Statement { statement: 2, .. }),
        "{err}"
    );

    write(
        dir.path(),
        "1_two_tables.sql",
        "CREATE TABLE a ON CLUSTER chx (x UInt8) ENGINE = MergeTree ORDER BY x;\n\
         CREATE TABLE b ON CLUSTER chx (x UInt8) ENGINE = MergeTree ORDER BY x;",
    );
    let applied = Migrator::from_dir(dir.path())
        .unwrap()
        .on_cluster(CLUSTER)
        .run(&scratch.on[1], |_| {})
        .await
        .unwrap();

    assert_eq!(applied.len(), 1);
    assert_eq!(applied[0].resumed_at, 1);
    for node in 0..2 {
        assert_eq!(
            scratch.scalar(node, "EXISTS TABLE b").await,
            "1",
            "node {node}"
        );
    }

    scratch.drop().await;
}

/// No cluster setting: the `Replicated` database engine replicates the
/// history table by itself.
///
/// Only through one node. In this compose file the nodes carry different
/// `{shard}` macros, and a `Replicated` database builds each table's Keeper
/// path from that macro, so ch-2 keeps a history of its own. chx warns about
/// that case rather than hiding it; this checks the part that does work.
#[tokio::test]
#[ignore = "needs Docker: cargo test -p chx-core -- --ignored"]
async fn a_replicated_database_gets_a_replicated_history_without_a_cluster_setting() {
    up();
    let scratch = Scratch::replicated(&NODES, "repdb").await;
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "1_init.sql",
        "CREATE TABLE t (x UInt8) ENGINE = MergeTree ORDER BY x",
    );

    let first = Migrator::from_dir(dir.path())
        .unwrap()
        .run(&scratch.on[0], |_| {})
        .await
        .unwrap();
    assert_eq!(first.len(), 1);

    // DDL in the migration reached the other node through the database.
    assert_eq!(scratch.scalar(1, "EXISTS TABLE t").await, "1");

    let second = Migrator::from_dir(dir.path())
        .unwrap()
        .run(&scratch.on[0], |_| {})
        .await
        .unwrap();
    assert!(second.is_empty(), "re-ran {second:?}");
    assert_eq!(
        scratch.history_engine(0).await,
        "ReplicatedReplacingMergeTree"
    );

    scratch.drop().await;
}

#[tokio::test]
#[ignore = "needs Docker: cargo test -p chx-core -- --ignored"]
async fn a_lock_taken_through_one_node_holds_on_the_other() {
    up();
    let scratch = Scratch::on_cluster(&NODES, "lock").await;
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "1_init.sql",
        "CREATE TABLE t ON CLUSTER chx (x UInt8) ENGINE = MergeTree ORDER BY x",
    );

    let _dead = lock::acquire(
        &scratch.on[0],
        &Placement::OnCluster(CLUSTER.to_string()),
        Duration::ZERO,
    )
    .await
    .unwrap();
    assert_eq!(scratch.scalar(1, "EXISTS TABLE _chx_lock").await, "1");

    let err = Migrator::from_dir(dir.path())
        .unwrap()
        .on_cluster(CLUSTER)
        .lock_timeout(Duration::from_secs(1))
        .run(&scratch.on[1], |_| {})
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Locked { .. }), "{err}");

    // Released through the other node, and gone from both.
    let released = migrate::unlock(&scratch.on[1], Some(CLUSTER))
        .await
        .unwrap();
    assert!(released.is_some());
    for node in 0..2 {
        assert_eq!(
            scratch.scalar(node, "EXISTS TABLE _chx_lock").await,
            "0",
            "node {node}"
        );
    }

    scratch.drop().await;
}

#[tokio::test]
#[ignore = "needs Docker: cargo test -p chx-core -- --ignored"]
async fn concurrent_runs_through_both_nodes_apply_each_migration_once() {
    up();
    let scratch = Scratch::on_cluster(&NODES, "race").await;
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "1_slow.sql",
        "CREATE TABLE t ON CLUSTER chx (x UInt8) ENGINE = MergeTree ORDER BY x;\n\
         SELECT sleep(1);",
    );
    let one = Migrator::from_dir(dir.path()).unwrap().on_cluster(CLUSTER);
    let two = Migrator::from_dir(dir.path()).unwrap().on_cluster(CLUSTER);

    let (first, second) = tokio::join!(
        one.run(&scratch.on[0], |_| {}),
        two.run(&scratch.on[1], |_| {})
    );

    let mut counts = [first.unwrap().len(), second.unwrap().len()];
    counts.sort();
    assert_eq!(counts, [0, 1]);

    scratch.drop().await;
}

#[tokio::test]
#[ignore = "needs Docker: cargo test -p chx-core -- --ignored"]
async fn an_import_through_one_node_is_seen_through_the_other() {
    up();
    let scratch = Scratch::on_cluster(&NODES, "import").await;
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "1_init.sql",
        "CREATE TABLE t ON CLUSTER chx (x UInt8) ENGINE = MergeTree ORDER BY x",
    );
    scratch.on[0]
        .execute("CREATE TABLE t ON CLUSTER chx (x UInt8) ENGINE = MergeTree ORDER BY x")
        .await
        .unwrap();

    let imported = Migrator::from_dir(dir.path())
        .unwrap()
        .on_cluster(CLUSTER)
        .import(&scratch.on[0], None)
        .await
        .unwrap();
    assert_eq!(imported.len(), 1);

    let applied = Migrator::from_dir(dir.path())
        .unwrap()
        .on_cluster(CLUSTER)
        .run(&scratch.on[1], |_| {})
        .await
        .unwrap();
    assert!(applied.is_empty(), "ch-2 ran {applied:?}");

    scratch.drop().await;
}
