# chx Agent Guide

## Scope

A ClickHouse migration tool in the shape of `sqlx migrate`. It is deliberately
small. Make the smallest cohesive change that fits the layout below, and do not
scaffold commands or crates the current task does not need.

## Architecture

- `crates/chx-core` is the engine (library name `chx`). `chx` on crates.io is
  an unrelated hex editor, hence the package name.
- `apps/chx-cli` builds the `chx` binary: argument parsing, logging setup and
  the exit code. Anything that reads, checks or runs a migration belongs in
  the library.
- Workspace members are globbed. A new crate under `apps/` or `crates/` joins
  without a root `Cargo.toml` edit.

Inside the library: `migrate::source` reads files, `migrate::split` cuts them
into statements, `migrate::history` owns the `_chx_migrations` table,
`migrate::lock` owns `_chx_lock`, and
`migrate::plan` checks the history against the files (`import_plan` does the
same for `chx migrate import`). `plan` is pure, so every
refusal is unit tested without a server.

## Rules

- **Nothing runs until the history check passes.** A new refusal goes in
  `plan`, not in the apply loop.
- **Import only adds.** `import_plan` refuses rather than rewriting a row
  that disagrees with its file; `import` never updates or deletes history.
- **One run at a time.** `migrate::lock` is a `_chx_lock` table created
  without `IF NOT EXISTS`. Never expire it on a timer: a long mutation looks
  like a dead run. `chx migrate unlock` is the only way a stale lock goes.
- **Progress is recorded per statement.** ClickHouse DDL is not
  transactional, and this is the whole reason the tool exists.
- **The history table is a compatibility surface.** It lives in users'
  databases. Add columns with defaults; never rename or retype one.
- **The checksum is plain SHA-256 of the file bytes.** Users verify it with
  `sha256sum`. Do not normalise the input.
- **No `.env` loading.** The URL comes from `CLICKHOUSE_URL` or
  `--clickhouse-url`.
- **Cluster support is best effort, and says so.** Where the history table
  lives is decided in `migrate::history::ensure_table`. A setup that cannot
  share it falls back or warns; it never fails a run that would work on one
  node.

## Verification

```bash
cargo fmt --check
cargo clippy -p <package> --all-targets --all-features -- -D warnings
cargo test -p <package>
```

The default suite is hermetic. The live suites in `crates/chx-core/tests`
(`single_node`, `cluster`) are `#[ignore]`d and start `docker/compose.yaml`
themselves on fixed ports 58123 to 58125:

```bash
cargo test -p chx-core -- --ignored
```

Do not claim a live test passed when it was ignored.

## Writing style

- Never use an em-dash or an en-dash.
- Simple, direct language. Short sentences.
- Comments say why, not what.
