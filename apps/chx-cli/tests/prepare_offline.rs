//! `chx prepare` on a crate with no checked queries, which needs no server.
//!
//! `prepare` is mostly orchestration: find the workspace, clean it, run
//! `cargo check` with the macros told where to save, and reconcile `.chx`
//! with what came back. All of that can be exercised on a throwaway crate
//! that has no dependencies and no `query!`, so nothing is described and
//! nothing connects; `CLICKHOUSE_URL` only has to be present. The real
//! thing, with queries and a database, is `tests/prepare.rs`.
//!
//! Each test builds its crate in a temporary directory and runs the `chx`
//! binary inside it. Cargo is invoked for real, offline, which is why these
//! take about a second each.

use std::path::Path;
use std::process::{Command, Output};

use chx_types::cache::{Describe, Field, QueryData};

const CHX: &str = env!("CARGO_BIN_EXE_chx");

/// A crate named `scratch` with `body` as its `src/lib.rs`.
fn scratch_crate(body: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname = \"scratch\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n[workspace]\n",
    )
    .unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/lib.rs"), body).unwrap();
    dir
}

fn prepare(dir: &Path, args: &[&str]) -> Output {
    Command::new(CHX)
        .current_dir(dir)
        .arg("prepare")
        .args(args)
        // Never contacted: this crate has no query to describe.
        .env("CLICKHOUSE_URL", "http://127.0.0.1:1/unused")
        .env("CARGO_TARGET_DIR", dir.join("target"))
        .env_remove("CHX_OFFLINE")
        .output()
        .expect("run chx")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// A cache file for a query the crate does not contain.
fn stale_entry(dir: &Path) -> std::path::PathBuf {
    QueryData::new(
        "SELECT 1 AS removed",
        Describe {
            parameters: vec![],
            columns: vec![Field {
                name: "removed".to_string(),
                ty: "UInt8".to_string(),
            }],
        },
    )
    .save_in(&dir.join(".chx"))
    .unwrap()
}

#[test]
fn a_crate_with_no_queries_prepares_to_an_empty_cache() {
    let dir = scratch_crate("pub fn answer() -> u8 { 42 }\n");

    let output = prepare(dir.path(), &[]);

    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "0 queries in .chx\n");
}

#[test]
fn prepare_removes_the_entry_of_a_query_that_is_gone() {
    let dir = scratch_crate("pub fn answer() -> u8 { 42 }\n");
    let stale = stale_entry(dir.path());

    let output = prepare(dir.path(), &[]);

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(!stale.exists(), "the stale entry is still there");
}

#[test]
fn check_passes_on_a_cache_that_matches_and_changes_nothing() {
    let dir = scratch_crate("pub fn answer() -> u8 { 42 }\n");

    let output = prepare(dir.path(), &["--check"]);

    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), ".chx is up to date (0 queries)\n");
    assert!(!dir.path().join(".chx").exists(), "--check wrote a cache");
}

#[test]
fn check_fails_on_a_stale_entry_and_leaves_it_in_place() {
    let dir = scratch_crate("pub fn answer() -> u8 { 42 }\n");
    let stale = stale_entry(dir.path());

    let output = prepare(dir.path(), &["--check"]);

    assert_eq!(output.status.code(), Some(1));
    let err = stderr(&output);
    assert!(
        err.contains(".chx is out of date; run `chx prepare`"),
        "{err}"
    );
    assert!(err.contains("no longer used: query-"), "{err}");
    assert!(stale.exists(), "--check removed a file");
}

#[test]
fn a_crate_that_does_not_compile_fails_prepare_with_the_compiler_error() {
    let dir = scratch_crate("pub fn broken() -> u8 { \"not a number\" }\n");

    let output = prepare(dir.path(), &[]);

    assert_eq!(output.status.code(), Some(1));
    let err = stderr(&output);
    assert!(err.contains("mismatched types"), "{err}");
    assert!(
        err.contains("chx: internal error: cargo check failed"),
        "{err}"
    );
    assert!(!dir.path().join(".chx").exists());
}

#[test]
fn arguments_after_the_dashes_replace_the_cargo_check_arguments() {
    // `--lib` skips the test target, which is the only thing that is broken.
    let dir = scratch_crate(
        "pub fn answer() -> u8 { 42 }\n#[cfg(test)]\nmod tests { fn broken() -> u8 { \"no\" } }\n",
    );

    let default = prepare(dir.path(), &[]);
    assert_eq!(default.status.code(), Some(1), "--all-targets checks tests");

    let lib_only = prepare(dir.path(), &["--", "--lib"]);
    assert!(lib_only.status.success(), "{}", stderr(&lib_only));
}

#[test]
fn outside_a_cargo_project_prepare_fails_with_cargos_reason() {
    let dir = tempfile::tempdir().unwrap();

    let output = prepare(dir.path(), &[]);

    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("cargo metadata failed"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn prepare_needs_a_url() {
    let dir = scratch_crate("");
    let output = Command::new(CHX)
        .current_dir(dir.path())
        .arg("prepare")
        .env_remove("CLICKHOUSE_URL")
        .output()
        .expect("run chx");

    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr(&output).contains("--clickhouse-url"),
        "{}",
        stderr(&output)
    );
}
