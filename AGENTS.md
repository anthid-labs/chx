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
into statements, `migrate::history` owns the `_chx_migrations` table, and
`migrate::plan` checks the history against the files. `plan` is pure, so every
refusal is unit tested without a server.

## Rules

- **Nothing runs until the history check passes.** A new refusal goes in
  `plan`, not in the apply loop.
- **Progress is recorded per statement.** ClickHouse DDL is not
  transactional, and this is the whole reason the tool exists.
- **The history table is a compatibility surface.** It lives in users'
  databases. Add columns with defaults; never rename or retype one.
- **The checksum is plain SHA-256 of the file bytes.** Users verify it with
  `sha256sum`. Do not normalise the input.
- **No `.env` loading.** The URL comes from the flag or the environment.

## Verification

```bash
cargo fmt --check
cargo clippy -p <package> --all-targets --all-features -- -D warnings
cargo test -p <package>
```

The default suite is hermetic. The live suite needs a scratch server and is
skipped without `CHX_TEST_CLICKHOUSE_URL`. Never point that at a real
database, and do not claim a live test passed when it was skipped.

```bash
docker run -d --rm -p 58123:8123 -e CLICKHOUSE_PASSWORD=chx clickhouse/clickhouse-server
CHX_TEST_CLICKHOUSE_URL=http://default:chx@127.0.0.1:58123 cargo test -p chx-core --test clickhouse_live
```

## Writing style

- Never use an em-dash or an en-dash.
- Simple, direct language. Short sentences.
- Comments say why, not what.
