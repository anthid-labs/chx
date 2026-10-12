//! `chx prepare --check` with real queries and a real database, the `single`
//! service in `docker/compose.yaml`.
//!
//! The `.chx` directory in `crates/chx-core` is checked in. These tests
//! create the fixture tables it was written from in a scratch database and
//! ask `prepare --check` whether the cache still matches what the macros
//! would write. That makes this suite the place a stale cache is caught.
//!
//! Each test runs the `chx` binary, which runs a full `cargo check` of
//! `chx-core`. It does so in a target directory of its own,
//! `target/prepare-test`, so it does not wait on the build lock of the
//! `cargo test` that is running it. The first run compiles the dependency
//! tree there and takes a minute or two; later runs take seconds.
//!
//! `tests/prepare_offline.rs` covers the orchestration around this, on a
//! throwaway crate, with no server.
//!
//! Ignored by default, because they start containers:
//!
//! ```bash
//! cargo test -p chx-cli --test prepare -- --ignored
//! ```

#[path = "../../../crates/chx-core/tests/common/mod.rs"]
mod common;

use std::path::Path;
use std::process::Command;

use common::{SINGLE, Scratch, compose_up};

fn core_dir() -> &'static Path {
    Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../crates/chx-core"
    ))
}

/// The guard on the checked-in cache. With the fixture tables in a scratch
/// database, `prepare --check` recompiles `chx-core` with the macros asking
/// that database, and compares what they learned with `crates/chx-core/.chx`.
///
/// It passes only if every query in `tests/macros.rs` has an entry, every
/// entry still describes the same columns, and no entry is left over. A query
/// edited without regenerating the cache fails here, in CI, rather than as a
/// confusing compile error for the next person.
#[tokio::test]
#[ignore = "needs Docker: cargo test -p chx-cli -- --ignored"]
async fn the_checked_in_cache_matches_the_fixture_schema() {
    compose_up(&["single"]);
    let scratch = Scratch::new(SINGLE, "prepare").await;
    for file in ["probe.sql", "probe_drift.sql"] {
        let sql = std::fs::read_to_string(core_dir().join("tests/fixture").join(file)).unwrap();
        scratch.client().execute(&sql).await.unwrap();
    }
    let url = format!("{SINGLE}/{}", scratch.name);

    let output = Command::new(env!("CARGO_BIN_EXE_chx"))
        .args(["prepare", "--check"])
        .current_dir(core_dir())
        .env("CLICKHOUSE_URL", &url)
        .env(
            "CARGO_TARGET_DIR",
            core_dir().join("../../target/prepare-test"),
        )
        .output()
        .expect("run chx");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "prepare --check failed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(stdout.contains("is up to date (8 queries)"), "{stdout}");

    scratch.drop().await;
}

/// The other direction: a database whose `probe` table lacks columns the
/// queries select. The macros fail to compile against it, and `prepare` has
/// to relay the server's error and exit 1, not write or accept a cache built
/// from a broken check. `--check` must also leave the directory untouched.
#[tokio::test]
#[ignore = "needs Docker: cargo test -p chx-cli -- --ignored"]
async fn a_schema_the_queries_do_not_fit_fails_the_check() {
    compose_up(&["single"]);
    let scratch = Scratch::new(SINGLE, "prepare_drift").await;
    // `probe` without `note`: every query that reads it fails to compile,
    // which is the error prepare must relay rather than write a cache for.
    scratch
        .client()
        .execute("CREATE TABLE probe (id UInt64, venue String) ENGINE = Memory")
        .await
        .unwrap();
    let url = format!("{SINGLE}/{}", scratch.name);

    let output = Command::new(env!("CARGO_BIN_EXE_chx"))
        .args(["prepare", "--check"])
        .current_dir(core_dir())
        .env("CLICKHOUSE_URL", &url)
        .env(
            "CARGO_TARGET_DIR",
            core_dir().join("../../target/prepare-test"),
        )
        .output()
        .expect("run chx");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("cargo check failed"), "{stderr}");
    assert!(
        stderr.contains("Missing columns") || stderr.contains("UNKNOWN_IDENTIFIER"),
        "{stderr}"
    );
    // Nothing was written: --check only reads.
    assert_eq!(
        std::fs::read_dir(core_dir().join(".chx")).unwrap().count(),
        8
    );

    scratch.drop().await;
}
