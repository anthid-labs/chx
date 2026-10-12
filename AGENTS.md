# chx Agent Guide

## Scope

ClickHouse for Rust in the shape of sqlx: compile-time checked queries, typed
rows over RowBinary, bulk inserts, and schema migrations. It is deliberately
small. Make the smallest cohesive change that fits the layout below, and do not
scaffold commands or crates the current task does not need. Every crate is
published to crates.io under Apache-2.0: a new one needs a description,
keywords, a README and a `[workspace.dependencies]` pin at the workspace
version.

## Architecture

- `crates/chx-types` is what the library and the macros agree on: the type
  model and parser (`types`), the `{name:Type}` parameter grammar (`params`),
  the connection URL (`endpoint`) and the `.chx` cache format (`cache`). No
  I/O, no server.
- `crates/chx-macros` is the proc-macro crate: `query!`, `query_as!`,
  `query_scalar!`, `query_file!`, `query_unchecked!`, `#[derive(FromRow)]`.
  It depends on `chx-types` and must never depend on `chx-core`, which
  re-exports it. Generated code refers to `::chx::`.
- `crates/chx-core` is the library (library name `chx`; `chx` on crates.io is
  an unrelated hex editor, hence the package name). `encode` and `decode` are
  the codecs, `row` cuts a `RowBinaryWithNamesAndTypes` body into rows,
  `query` binds parameters and fetches, `insert` batches rows, `migrate` is
  the migration engine, `client` is the HTTP client underneath all of it.
- `apps/chx-cli` builds the `chx` binary: argument parsing, logging setup,
  `prepare`, and the exit code. Anything that reads, checks or runs a
  migration or a query belongs in the library.
- Workspace members are globbed. A new crate under `apps/` or `crates/` joins
  without a root `Cargo.toml` edit, but needs a `[workspace.dependencies]`
  line to be depended on.

Inside `migrate`: `source` reads files, `split` cuts them into statements,
`history` owns the `_chx_migrations` table, `lock` owns `_chx_lock`, and
`plan` checks the history against the files (`import_plan` does the same for
`chx migrate import`). `plan` is pure, so every refusal is unit tested
without a server.

Inside the macros: `describe` decides where a query's columns come from
(cache, server, or an error naming both), `rust_type` maps a ClickHouse type
to the Rust type a column is read as and a parameter is bound as, and
`expand` writes the code.

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
- **`.env` loads as sqlx-cli loads it.** `dotenvy::dotenv()` before argument
  parsing, so every `env` fallback sees it. It never overrides the process
  environment, and only the binary loads it: the library takes a URL.
- **Cluster support is best effort, and says so.** Where the history table
  lives is decided in `migrate::history::ensure_table`. A setup that cannot
  share it falls back or warns; it never fails a run that would work on one
  node.
- **Codecs take the ClickHouse type.** `Encode` and `Decode` are given the
  column's `Type`, because a `DateTime64` cannot be written without its
  precision or a `Decimal` without its scale. `accepts` is the compatibility
  rule a schema check consults; `decode` and `encode` assume it said yes.
  `LowCardinality` and `SimpleAggregateFunction` are looked through with
  `Type::unwrapped`, since RowBinary serialises them as their inner type.
- **Parameters are server side.** `{name:Type}` in the SQL, sent as
  `param_name` in the text form the server reads: escaped TSV at the top
  level, quoted literals inside arrays, maps and tuples. Never substitute
  into the SQL text client side.
- **A query fails before it is sent when it can.** An undeclared, unbound or
  mistyped parameter is an error at bind or fetch time, not a server round
  trip. The macros move the same checks to compile time; the runtime check
  stays, since `query()` is the same builder.
- **The macros check the result at run time too.** `expect_columns` carries
  the columns a `query!` was compiled against, and a result with different
  columns is `Error::Schema`, never a misread column.
- **`crates/chx-core/.chx` is generated.** It is the cache `tests/macros.rs`
  compiles against, written by `chx prepare` from the tables in
  `tests/fixture`. Change a query in that file, regenerate the cache (see the
  README), and check both in. Never edit a cache file by hand; its hash is
  checked.
- **The library never panics on data.** A short body, a bad type string or a
  value that does not fit is an `Error`. `unwrap` and `expect` belong in
  tests.

## Verification

```bash
cargo fmt --check
cargo clippy -p <package> --all-targets --all-features -- -D warnings
cargo test -p <package>
```

```bash
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
cargo llvm-cov --workspace --all-features   # if cargo-llvm-cov is installed
```

Every library crate has `#![warn(missing_docs)]`, and CI builds the docs
with warnings denied: a new public item needs a doc comment, and a type or
trait a caller is meant to use needs an example that runs as a doc test.

The default suite is hermetic, and it is where behaviour is pinned down.
`crates/chx-core/src/testing.rs` is an in-process fake ClickHouse: `Fake`
answers HTTP with a closure, and `FakeClickHouse` holds a history table and
a lock and can be told to fail a statement or refuse a cluster. A change to
the client, queries, inserts or migrations gets a unit test against it, and
a change to the `chx` command gets one in `apps/chx-cli/tests/migrate.rs`,
which includes the same file by path. Line coverage of the hermetic suite
is about 98%; do not land a change that lowers it without saying why.

The live suites are what prove the same things against ClickHouse itself.
Every test in them has a doc comment saying what it proves, and every suite
a header with the command to run it alone. Keep both when adding one. They
are `#[ignore]`d and start `docker/compose.yaml` themselves on fixed ports
58123 to 58125:

```bash
cargo test -p chx-core -- --ignored
cargo test -p chx-cli -- --ignored
```

The `chx-core` live suites cover migrations (`single_node`, `cluster`), the
query layer (`query`) and the macros (`macros`); the `chx-cli` one runs
`prepare --check` against the checked-in cache, in a target directory of its
own. Do not claim a live test passed when it was ignored.

## Writing style

- Never use an em-dash or an en-dash.
- Simple, direct language. Short sentences.
- Comments say why, not what.
