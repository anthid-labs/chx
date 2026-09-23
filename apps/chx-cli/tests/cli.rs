//! End to end tests for the `chx` binary.
//!
//! These run the real command as a child process, so they cover argument
//! parsing, the exit code, and what reaches stdout and stderr. Everything here
//! is hermetic: every case fails before a connection is attempted, or points
//! at a port nothing listens on.

use std::process::{Command, Output};

const CHX: &str = env!("CARGO_BIN_EXE_chx");

fn chx(args: &[&str], database_url: Option<&str>) -> Output {
    let mut command = Command::new(CHX);
    command
        .args(args)
        .env_remove("CLICKHOUSE_URL")
        .env_remove("CLICKHOUSE_CLUSTER");
    if let Some(url) = database_url {
        command.env("CLICKHOUSE_URL", url);
    }
    command.output().expect("run chx")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn a_missing_url_is_a_usage_error() {
    let output = chx(&["migrate", "run"], None);

    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("--clickhouse-url"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn a_missing_directory_fails_before_connecting() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("nope");

    let output = chx(
        &["migrate", "run", "--source", source.to_str().unwrap()],
        Some("http://127.0.0.1:1/db"),
    );

    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("migrations directory"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn an_unreachable_server_is_exit_one_and_names_the_cause() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("1_init.sql"), "SELECT 1").unwrap();

    let output = chx(
        &["migrate", "run", "--source", dir.path().to_str().unwrap()],
        Some("http://127.0.0.1:1/db"),
    );

    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("unreachable"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn the_password_never_reaches_the_output() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("1_init.sql"), "SELECT 1").unwrap();

    let output = chx(
        &["migrate", "run", "--source", dir.path().to_str().unwrap()],
        Some("http://default:hunter2@127.0.0.1:1/db"),
    );

    assert!(!stderr(&output).contains("hunter2"), "{}", stderr(&output));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("hunter2"));
}
