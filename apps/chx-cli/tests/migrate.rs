//! The `chx migrate` commands, end to end, against a ClickHouse that lives in
//! this test process.
//!
//! `tests/cli.rs` covers what fails before a connection is made. This covers
//! what happens after one: the lines each command prints, the exit code, and
//! what a second run, a failed statement or a held lock looks like to the
//! person at the terminal. The server is the fake from `chx-core`'s own test
//! support, included here by path, so the suite needs no Docker and runs with
//! plain `cargo test`.
//!
//! What it proves is the command around the library: arguments reach the
//! right call and results reach stdout. That the library is right against a
//! real ClickHouse is the job of the live suites in `crates/chx-core/tests`.

#[allow(dead_code)]
#[path = "../../../crates/chx-core/src/testing.rs"]
mod testing;

use std::path::Path;
use std::process::{Command, Output};

use testing::FakeClickHouse;

const CHX: &str = env!("CARGO_BIN_EXE_chx");

/// Runs `chx` with `args` against `server`, from an empty directory so no
/// `.env` above the checkout is picked up.
fn chx(server: &FakeClickHouse, args: &[&str]) -> Output {
    let cwd = tempfile::tempdir().unwrap();
    Command::new(CHX)
        .current_dir(cwd.path())
        .args(args)
        .env("CLICKHOUSE_URL", format!("{}/analytics", server.fake.url))
        .env_remove("CLICKHOUSE_CLUSTER")
        .env_remove("CHX_LOCK_TIMEOUT")
        .env_remove("LOG_LEVEL")
        .env_remove("RUST_LOG")
        .output()
        .expect("run chx")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Two migrations, three statements.
fn migrations() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("1_create_trades.sql"),
        "CREATE TABLE trades (id UInt64) ENGINE = Memory;\nCREATE TABLE fills (id UInt64) ENGINE = Memory;",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("2_add_venue.sql"),
        "ALTER TABLE trades ADD COLUMN venue String",
    )
    .unwrap();
    dir
}

fn source(dir: &Path) -> &str {
    dir.to_str().unwrap()
}

#[test]
fn run_prints_a_line_per_migration_and_then_reports_up_to_date() {
    let server = FakeClickHouse::start();
    let dir = migrations();

    let first = chx(&server, &["migrate", "run", "--source", source(dir.path())]);

    assert!(first.status.success(), "{}", stderr(&first));
    let out = stdout(&first);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 2, "{out}");
    assert!(
        lines[0].starts_with("Applied 1/create trades (2 statements, "),
        "{out}"
    );
    assert!(
        lines[1].starts_with("Applied 2/add venue (1 statements, "),
        "{out}"
    );
    assert_eq!(server.state().executed.len(), 3);

    let second = chx(&server, &["migrate", "run", "--source", source(dir.path())]);

    assert!(second.status.success(), "{}", stderr(&second));
    assert_eq!(
        stdout(&second),
        format!("Up to date: 2 migrations in {}\n", dir.path().display())
    );
    assert_eq!(server.state().executed.len(), 3);
}

#[test]
fn a_failed_statement_exits_1_and_the_next_run_says_where_it_resumed() {
    let server = FakeClickHouse::start();
    let dir = migrations();
    server.state().fail_containing = Some("fills".to_string());

    let failed = chx(&server, &["migrate", "run", "--source", source(dir.path())]);

    assert_eq!(failed.status.code(), Some(1));
    assert_eq!(
        stdout(&failed),
        "",
        "nothing completed, so nothing is printed"
    );
    let err = stderr(&failed);
    assert!(
        err.contains("chx: migration 1 (create trades) failed at statement 2 of 2"),
        "{err}"
    );

    server.state().fail_containing = None;
    let resumed = chx(&server, &["migrate", "run", "--source", source(dir.path())]);

    assert!(resumed.status.success(), "{}", stderr(&resumed));
    let out = stdout(&resumed);
    assert!(
        out.contains("Applied 1/create trades (2 statements, resumed at statement 2, "),
        "{out}"
    );
}

#[test]
fn import_records_without_running_and_run_applies_the_rest() {
    let server = FakeClickHouse::start();
    let dir = migrations();
    let import = |through: &[&str]| {
        let mut args = vec!["migrate", "import", "--source", source(dir.path())];
        args.extend_from_slice(through);
        chx(&server, &args)
    };

    let first = import(&["--through", "1"]);

    assert!(first.status.success(), "{}", stderr(&first));
    assert_eq!(
        stdout(&first),
        "Imported 1/create trades (2 statements, not run)\n"
    );
    assert!(server.state().executed.is_empty());

    assert_eq!(
        stdout(&import(&["--through", "1"])),
        "Nothing to import: every file up to 1 is already recorded\n"
    );

    let run = chx(&server, &["migrate", "run", "--source", source(dir.path())]);
    assert!(
        stdout(&run).starts_with("Applied 2/add venue"),
        "{}",
        stdout(&run)
    );

    assert_eq!(
        stdout(&import(&[])),
        "Nothing to import: every file is already recorded\n"
    );
}

#[test]
fn a_held_lock_fails_the_run_and_unlock_names_who_held_it() {
    let server = FakeClickHouse::start();
    let dir = migrations();
    server.state().lock = Some("chx 0.1.0 on ci-runner-7, pid 4121".to_string());

    let blocked = chx(
        &server,
        &[
            "migrate",
            "run",
            "--source",
            source(dir.path()),
            "--lock-timeout",
            "0",
        ],
    );

    assert_eq!(blocked.status.code(), Some(1));
    let err = stderr(&blocked);
    assert!(err.contains("chx 0.1.0 on ci-runner-7, pid 4121"), "{err}");
    assert!(err.contains("chx migrate unlock"), "{err}");
    assert!(server.state().executed.is_empty());

    let unlock = chx(&server, &["migrate", "unlock"]);

    assert!(unlock.status.success(), "{}", stderr(&unlock));
    assert_eq!(
        stdout(&unlock),
        "Released the lock held by chx 0.1.0 on ci-runner-7, pid 4121 since 2026-10-12 14:00:00 UTC\n"
    );
    assert_eq!(
        stdout(&chx(&server, &["migrate", "unlock"])),
        "No lock was held\n"
    );
}

#[test]
fn the_cluster_flag_reaches_the_history_table_and_the_lock() {
    let server = FakeClickHouse::start();
    let dir = migrations();
    {
        let mut state = server.state();
        state.nodes = 2;
        state.replicas = 1;
    }

    let output = chx(
        &server,
        &[
            "migrate",
            "run",
            "--source",
            source(dir.path()),
            "--cluster",
            "prod",
        ],
    );

    assert!(output.status.success(), "{}", stderr(&output));
    let state = server.state();
    assert!(
        state
            .log
            .iter()
            .any(|sql| sql.contains("_chx_migrations ON CLUSTER `prod`"))
    );
    assert!(
        state
            .log
            .iter()
            .any(|sql| sql.contains("_chx_lock ON CLUSTER `prod`"))
    );
    // One of two nodes holds the history, and the run says so.
    assert!(
        stderr(&output).contains("replicated to 1 of 2 nodes"),
        "{}",
        stderr(&output)
    );
}

/// Logs go to stderr and only what was applied goes to stdout, so a script
/// can read one without the other.
#[test]
fn log_level_info_adds_timestamped_lines_to_stderr_only() {
    let server = FakeClickHouse::start();
    let dir = migrations();
    server.state().lock = Some("another run".to_string());
    server.state().release_after_holder_reads = Some(1);

    let output = chx(
        &server,
        &[
            "--log-level",
            "info",
            "migrate",
            "run",
            "--source",
            source(dir.path()),
        ],
    );

    assert!(output.status.success(), "{}", stderr(&output));
    let err = stderr(&output);
    assert!(err.contains("waiting for another run"), "{err}");
    assert!(err.contains("nanos="), "{err}");
    assert_eq!(stdout(&output).lines().count(), 2);
}
