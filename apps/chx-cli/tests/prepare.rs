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
//! The tests share that directory and run in parallel, on purpose. Each
//! `prepare` has to get its own answer from a directory another build is
//! using at the same time, with a different database behind it, and that is
//! the property a shared directory tests.
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

/// Checked queries in `chx-core`: those in `tests/macros.rs` and in
/// `examples/checked.rs`. One entry each in `crates/chx-core/.chx`.
const QUERIES: usize = 13;

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
    assert!(
        stdout.contains(&format!("is up to date ({QUERIES} queries)")),
        "{stdout}"
    );

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
        QUERIES
    );

    scratch.drop().await;
}

/// The failure this suite first found in CI. `prepare` can only learn a
/// query from a macro that runs, and a macro only runs when its crate
/// compiles. Here the crate is built first, in the same target directory and
/// with no database, so cargo has every reason to call it fresh.
///
/// `prepare --check` must recompile it anyway and find every query.
/// When it trusted the fresh artifacts it found none, reported every cache
/// entry as unused, and a plain `prepare` in that state would have deleted
/// them. Any second build in the directory sets this up: an editor's
/// background check, or the other test in this file running alongside.
#[tokio::test]
#[ignore = "needs Docker: cargo test -p chx-cli -- --ignored"]
async fn prepare_sees_every_query_in_a_crate_that_is_already_built() {
    compose_up(&["single"]);
    let scratch = Scratch::new(SINGLE, "prepare_fresh").await;
    for file in ["probe.sql", "probe_drift.sql"] {
        let sql = std::fs::read_to_string(core_dir().join("tests/fixture").join(file)).unwrap();
        scratch.client().execute(&sql).await.unwrap();
    }
    let url = format!("{SINGLE}/{}", scratch.name);
    let target = core_dir().join("../../target/prepare-test");

    let built = Command::new(env!("CARGO"))
        .args(["check", "--all-targets"])
        .current_dir(core_dir())
        .env("CARGO_TARGET_DIR", &target)
        .env_remove("CLICKHOUSE_URL")
        .env_remove("CHX_TMP")
        .output()
        .expect("run cargo");
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );

    let output = Command::new(env!("CARGO_BIN_EXE_chx"))
        .args(["prepare", "--check"])
        .current_dir(core_dir())
        .env("CLICKHOUSE_URL", &url)
        .env("CARGO_TARGET_DIR", &target)
        .output()
        .expect("run chx");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "prepare --check failed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains(&format!("is up to date ({QUERIES} queries)")),
        "{stdout}"
    );

    scratch.drop().await;
}
