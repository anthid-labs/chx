# chx

ClickHouse schema migrations, in the shape of `sqlx migrate`, built for the
way ClickHouse actually fails.

[![✅ CI](https://github.com/anthid-labs/chx/actions/workflows/ci.yml/badge.svg)](https://github.com/anthid-labs/chx/actions/workflows/ci.yml)
[![📦 Publish to crates.io](https://github.com/anthid-labs/chx/actions/workflows/publish.yml/badge.svg)](https://github.com/anthid-labs/chx/actions/workflows/publish.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

```bash
cargo install chx-cli
export CLICKHOUSE_URL=http://default:@localhost:8123/analytics
chx migrate run
```

`chx migrate run` applies every `.sql` file in `./migrations` that has not run
yet, in version order. It records a SHA-256 of each file in the database, so a
file that changes after it ran is refused instead of silently disagreeing with
the schema.

## Why

ClickHouse DDL is not transactional. A migration that fails on its third
statement has already applied the first two, and a tool that records a
migration only when it finishes has no idea. The next run fails on the first
`CREATE TABLE`, and you work out by hand which half of the file is live.

chx is built around that:

- **Progress is recorded after every statement.** A failed migration resumes
  at the statement that failed, after you fix it.
- **The history is checked before anything runs.** An edited, deleted or
  out-of-order migration stops the run before a single statement is sent.
- **One run at a time.** Two deploys that start together apply each migration
  once.
- **Adopting an existing database is one command.** `chx migrate import`
  records the files a hand-built schema already has, without running them.
- **Clusters are supported where they can be.** The history and the lock
  reach every node with `ON CLUSTER`, a `Replicated` database or ClickHouse
  Cloud. Where they cannot, chx says so rather than pretending.

## Contents

- [Install](#install)
- [Commands](#commands)
- [Migrations](#migrations)
- [What a run does](#what-a-run-does)
- [Partial migrations](#partial-migrations)
- [What a run refuses](#what-a-run-refuses)
- [Importing a database built by hand](#importing-a-database-built-by-hand)
- [One run at a time](#one-run-at-a-time)
- [Connection](#connection)
- [Clusters](#clusters)
- [The history table](#the-history-table)
- [As a library](#as-a-library)
- [Not done yet](#not-done-yet)
- [Contributing](#contributing)
- [Support](#support)
- [License](#license)

## Install

```bash
cargo install chx-cli
```

The package is `chx-cli` and the command it installs is `chx`. The library is
`chx-core`, because `chx` on crates.io is an unrelated hex editor.

Rust 1.88 or newer.

## Commands

| Command | What it does |
| --- | --- |
| `chx migrate run` | Apply every pending migration, in version order. |
| `chx migrate import` | Record migrations as applied without running them, for a database built by hand. |
| `chx migrate unlock` | Release the lock left by a run that died holding it. |

Options, each with the environment variable it falls back to:

| Flag | Environment | Default | Used by |
| --- | --- | --- | --- |
| `--clickhouse-url` | `CLICKHOUSE_URL` | required | all |
| `--cluster` | `CLICKHOUSE_CLUSTER` | none | all |
| `--source` | | `migrations` | `run`, `import` |
| `--lock-timeout` | `CHX_LOCK_TIMEOUT` | `300` seconds | `run`, `import` |
| `--through` | | every file | `import` |
| `--log-level` | `LOG_LEVEL` | `warn` | all |

`all` means every `chx migrate` command.

`RUST_LOG` takes precedence over `--log-level`. Logs go to stderr; stdout
carries one line per applied migration.

Exit code 0 is success and 1 is any failure, with the reason on stderr. A
malformed command line exits 2, from the argument parser.

## Migrations

```
migrations/
  20260923114500_create_trades.sql
  20260923120000_add_venue.sql
```

- Files are named `<version>_<description>.sql`. The version is an integer,
  and a timestamp avoids collisions between branches. The description comes
  from the name, with underscores as spaces.
- A file can hold several statements, separated by `;`. Semicolons inside
  string literals, quoted identifiers, `--` comments and `/* */` comments do
  not split. A fragment holding only comments is skipped, and an empty file is
  a valid migration.
- Other files in the directory, such as a README, are ignored.
- Refused: a `.sql` file with no version, two files with the same version, and
  `.up.sql` / `.down.sql` files. Running a down migration forward is the one
  mistake here that destroys data.

Statements are sent to ClickHouse exactly as written, comments included, so
they show up in `system.query_log` as you wrote them.

## What a run does

`chx migrate import` follows the same steps, except that step 5 records files
instead of running them. See
[Importing a database built by hand](#importing-a-database-built-by-hand).

1. **Reads the migrations directory.** A malformed directory is reported
   without connecting.
2. **Creates the history table** if it is missing. See [Clusters](#clusters)
   for where it lives.
3. **Takes the lock**, waiting for another run if one holds it.
4. **Reads the history and checks it** against the files. Any disagreement
   stops the run here.
5. **Applies what is left**, one statement at a time, writing a history row
   after each.
6. **Releases the lock**, whether the run succeeded or failed.

```
$ chx migrate run
Applied 20260923114500/create trades (2 statements, 41ms)
Applied 20260923120000/add venue (1 statements, 12ms)

$ chx migrate run
Up to date: 2 migrations in migrations
```

Every statement runs with three settings, each closing a way for ClickHouse
to report success before the change has actually happened:

| Setting | Why |
| --- | --- |
| `wait_end_of_query=1` | Without it the server can send a 200 and then fail partway through the body. |
| `async_insert=0` | An `INSERT`, including chx's own history rows, is stored before it returns. |
| `mutations_sync=2` | `ALTER ... UPDATE` and `ALTER ... DELETE` finish on every replica before the next statement starts. |

The last one means a migration that runs a large mutation takes as long as
that mutation. That is deliberate: the next statement, and the next
migration, should not run against half-rewritten data.

## Partial migrations

A migration that fails partway is recorded with the number of statements that
succeeded:

```
$ chx migrate run
chx: migration 3 (add venue) failed at statement 2 of 3: clickhouse returned 404: Code: 47. ...
```

Fix statement 2 in the file and run again. chx skips statement 1, which
already ran, and starts at statement 2:

```
$ chx migrate run
Applied 3/add venue (3 statements, resumed at statement 2, 18ms)
```

While a migration is incomplete, its checksum is not enforced, because fixing
the failed statement is the normal next step. The statements before it must
stay as they are: they are not run again, so editing one changes nothing in
the database and chx cannot tell. Once the migration completes, the fixed
file's checksum is the one it is held to.

## What a run refuses

All of these stop the run before any statement is sent:

| Situation | What to do |
| --- | --- |
| An applied file's checksum changed | Restore the file and put the change in a new migration. The schema is what the old file did. |
| An applied version has no file | Restore it. Nobody can rebuild the schema from the repository without it. |
| A new file is numbered below the latest applied one | Usually a branch merged late. Renumber it above the latest. |
| An incomplete migration is not the latest | The history table was changed outside chx. |
| An incomplete migration now has fewer statements than already ran | Restore the statements that ran. |

The stored checksum is plain `sha256sum` output, so it can be checked with no
tooling:

```bash
sha256sum migrations/20260923114500_create_trades.sql
```

## Importing a database built by hand

A database whose schema was applied by hand, by another tool, or by a history
table that got lost has no record chx can trust. `import` writes that record
from the files, without running any of them:

```
$ chx migrate import --through 20260923120000
Imported 20260923114500/create trades (2 statements, not run)
Imported 20260923120000/add venue (1 statements, not run)

$ chx migrate run
Applied 20260924090000/add fills (1 statements, 22ms)
```

Each imported row is what a completed run would have written: the file's
checksum, its statement count, and `success = true`. `execution_ms` is 0,
because nothing ran, which is also how an imported row can be told apart from
an applied one later.

- **It only adds rows.** A version already recorded with the same checksum is
  skipped, so running `import` twice is safe, and it fills gaps in a
  partial history.
- **It never overwrites.** A version recorded with a different checksum, or
  left incomplete by a failed run, stops the import before a single row is
  written. The history and the files disagree, and picking one is your call,
  not chx's. An incomplete migration is finished with `chx migrate run`.
- **`--through <version>`** records files up to and including that version and
  leaves later ones for `run` to apply. Without it, every file is recorded,
  including ones the database may not have yet.
- **It holds the same lock as `run`**, so it cannot race a deploy.

Nothing checks that the database really matches the files. That is your claim
to make, and a wrong one means `run` will skip a migration the schema never
had. Compare the schema against the files before importing.

## One run at a time

A run holds a lock from before it reads the history until after its last
statement. Two deploys that start together apply each migration once: the
second waits, then finds nothing to do.

```
$ chx migrate run
WARN waiting for another run to release the migration lock holder=chx 0.1.0 on ci-runner-7, pid 4121 since=2026-09-23 19:13:30
Up to date: 2 migrations in migrations
```

ClickHouse has no advisory locks and no unique constraints. The one thing it
guarantees only one caller wins is `CREATE TABLE` without `IF NOT EXISTS`, so
the lock is a table, `_chx_lock`, with the `Null` engine. Whoever created it
holds the lock. Its comment names the holder's host and process id, and
`system.tables` records when it was taken.

- **On a cluster** the lock is created `ON CLUSTER`. Every node applies
  distributed DDL in one queue order, so two runs racing through different
  nodes still get exactly one winner. A `Replicated` database and ClickHouse
  Cloud order DDL themselves, so there a plain create already reaches every
  node.
- **Waiting** is bounded by `--lock-timeout`, in seconds. `0` tries once.
  When it runs out, the run fails naming the holder.
- **A run that fails** still releases the lock. The history already says
  where to resume.
- **A run that is killed** leaves the lock behind. It is never expired on a
  timer: a run in the middle of a long mutation looks exactly like a dead one,
  and breaking its lock would let a second run start on top of it. Once you
  know the holder is gone:

  ```bash
  chx migrate unlock
  ```

  ```
  Released the lock held by chx 0.1.0 on ci-runner-7, pid 4121 since 2026-09-23 19:13:33 UTC
  ```

  Pass the same `--cluster` your runs use, so the lock is dropped on every
  node.

## Connection

`CLICKHOUSE_URL`, or `--clickhouse-url` to override it:

```
http[s]://user:password@host:port/database?setting=value
```

- **The HTTP interface** (8123, or 8443 with TLS), not the native protocol. It
  is the one every deployment exposes, ClickHouse Cloud included.
- **The path is the database.** Without one, the user's default database is
  used.
- **Credentials** are percent-decoded and sent as `X-ClickHouse-User` and
  `X-ClickHouse-Key` headers. They never appear in a request URL, an error
  message or a log line.
- **Query parameters** are passed through as ClickHouse settings, for example
  `?max_execution_time=600`.
- **`.env` files are not read.** A `.env` picked up by walking up from the
  working directory is a common way to run a migration against a database
  nobody meant to name. The URL comes from the flag or the process
  environment, and nowhere else.

## Clusters

Every node has to see the same history, or a deploy that lands on another
node starts from the top. Where chx keeps its history table depends on the
setup:

| Setup | History table | Lock |
| --- | --- | --- |
| Single server | Local `ReplacingMergeTree` | Local |
| `--cluster <name>` | `ReplicatedReplacingMergeTree`, created `ON CLUSTER` at `/clickhouse/chx/{database}/_chx_migrations` | `ON CLUSTER` |
| `Replicated` database | `ReplicatedReplacingMergeTree` at the database's own path | Replicated by the database |
| ClickHouse Cloud | Shared by Cloud | Replicated by Cloud |

With `--cluster`, the history's Keeper path has no `{shard}`: it is one list
for the whole cluster, not one per shard. That makes `{replica}` a
cluster-wide name, so it has to be unique across shards. It is when it is the
host name, which is the common setup.

**`--cluster` is tried, not required.** If the cluster cannot hold a
replicated table (no Keeper, no `{replica}` macro, no cluster by that name),
chx warns and keeps the history on the node you connected to. A migration tool
that refused to run without Keeper would be refusing the common case.

**chx checks what the history actually reached.** After creating or finding a
replicated history table, it asks Keeper how many nodes hold a replica and
warns if that is fewer than the cluster has:

```
WARN _chx_migrations is replicated to 1 of 2 nodes; a run through a node outside that set will not see this history
```

The usual causes are a `{replica}` macro that repeats across shards, or a
`Replicated` database whose nodes carry different `{shard}` macros.
ClickHouse builds each table's Keeper path in a `Replicated` database from the
server's `{shard}` macro, and refuses a path that would span shards, so in that
layout each shard keeps its own history. Run chx through one shard.

**Reads catch up first.** A replicated history is read after
`SYSTEM SYNC REPLICA`, so a node that is behind fetches the latest rows before
chx decides what to run.

**The history table is decided once.** A table that already exists keeps the
engine it was created with, whatever flags a later run passes. Converting a
history table is not something to do implicitly.

**Your migrations run exactly as written.** `--cluster` only affects chx's own
tables. A table meant for every node needs its own `ON CLUSTER`:

```sql
CREATE TABLE trades ON CLUSTER prod (id UInt64) ENGINE = MergeTree ORDER BY id;
```

## The history table

`_chx_migrations`, in the target database. A new row is written after every
statement, and `ReplacingMergeTree(applied)` keeps the one with the most
statements applied. Read it with `FINAL`:

```sql
SELECT version, description, checksum, applied, statements, success, installed_at
FROM _chx_migrations FINAL
ORDER BY version
```

| Column | Meaning |
| --- | --- |
| `version` | From the file name. |
| `description` | From the file name. |
| `checksum` | Lowercase hex SHA-256 of the file's bytes. |
| `statements` | Statements in the file. |
| `applied` | Statements that have run, counted from the top. |
| `success` | Every statement ran. `false` means the next run resumes at `applied + 1`. |
| `execution_ms` | Time spent on the migration by the run that wrote the row. |
| `installed_at` | When the row was written, UTC. |

Do not edit it by hand. The history check catches some edits, such as an
incomplete migration that is not the latest, but not all of them.

## As a library

`chx-core` is the engine the command wraps. It installs no log subscriber and
picks no runtime: it emits `tracing` events and runs on whatever tokio runtime
its caller has.

```rust
use std::time::Duration;

use chx::client::Client;
use chx::migrate::Migrator;

let client = Client::from_url("http://default:@localhost:8123/analytics")?;
let migrator = Migrator::from_dir("migrations")?
    .on_cluster("prod")
    .lock_timeout(Duration::from_secs(60));

for applied in migrator.run(&client, |_| {}).await? {
    println!("applied {} {}", applied.version, applied.description);
}
```

`chx::migrate::plan` is the history check on its own, as a pure function, for
anything that wants to know what a run would do without running it.

## Not done yet

- No `info`, `add` or `revert` commands.
- No `$$` heredoc strings in the statement splitter.
- No native protocol.

## Contributing

Issues and pull requests are welcome. For anything larger than a fix, open an
issue first: chx is deliberately small, and a change that fits the design
lands much faster than one that argues with it.

### Getting set up

Rust 1.88 or newer, and Docker for the live tests.

```bash
git clone https://github.com/anthid-labs/chx
cd chx
cargo test
```

The default suite needs no Docker and no network.

### Layout

| Path | What lives there |
| --- | --- |
| `crates/chx-core` | The engine. Anything that reads, checks or runs a migration. |
| `apps/chx-cli` | The `chx` binary: arguments, logging and the exit code. |
| `docker/compose.yaml` | The ClickHouse servers the live tests run against. |

Inside the library, `migrate::source` reads files, `migrate::split` cuts them
into statements, `migrate::history` owns `_chx_migrations`, `migrate::lock`
owns `_chx_lock`, and `migrate::plan` checks one against the other. `plan` is
pure, so every refusal is unit tested without a server. `migrate::import_plan`
does the same for `import`.

### Live tests

```bash
cargo test -p chx-core -- --ignored
```

They are `#[ignore]`d so the default suite stays hermetic, and they start the
compose stack themselves:

- `single`: one server on `127.0.0.1:58123`.
- `ch-1` and `ch-2`: cluster `chx` on `127.0.0.1:58124` and `58125`, two
  shards of one replica each, sharing one `keeper`. Two shards rather than two
  replicas, because a history shared across shards is the case that matters.

The ports are fixed and far from the defaults, so a live test can never be
pointed at a real server. Each test works in its own database. The stack is
left running so the next run starts in seconds; stop it with:

```bash
docker compose -f docker/compose.yaml down -v
```

### Before opening a pull request

```bash
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
cargo test -p chx-core -- --ignored
```

CI runs exactly these on every pull request, with warnings denied: the first
three in one job, and the integration tests in another against the same
compose stack.

### Releases

A release is a pushed tag. Bump `version` in the root `Cargo.toml`, and the
`chx-core` version in `[workspace.dependencies]` with it, then:

```bash
git tag v0.2.0
git push origin v0.2.0
```

The publish workflow runs CI on that commit, then `cargo publish --workspace`,
which publishes `chx-core` before `chx-cli`.

### What a change is expected to carry

- **A test that would fail without it.** A new refusal belongs in `plan`, with
  a unit test. Anything that depends on how ClickHouse behaves gets a live
  test, because the server has surprised this project more than once.
- **Comments that say why, not what.** The reasoning behind a decision that
  looks arbitrary is the part that survives.
- **A note in the pull request for anything that changes the history table,
  the lock table, or the command line.** The history table lives in users'
  databases: add columns with defaults, never rename or retype one.

### Things to know

[`AGENTS.md`](AGENTS.md) has the conventions in full. The parts that are not
negotiable:

- **Nothing runs until the history check passes.**
- **Import only adds.** It never rewrites or removes a history row.
- **Progress is recorded per statement.** It is the reason the tool exists.
- **The lock is never expired on a timer.** `chx migrate unlock` is the only
  way a stale lock goes.
- **Cluster support is best effort, and says so.** A setup that cannot share
  the history falls back or warns. It never fails a run that would work on one
  node.
- **The checksum is plain SHA-256 of the file's bytes.** Users verify it with
  `sha256sum`, so the input is never normalised.
- **No `.env` loading.**
- **Never use an em-dash or an en-dash** in code, comments or docs.

## Support

chx is free and Apache-2.0, and stays that way. If it saved you some trouble
and you feel like saying thanks,
[buy me a coffee](https://buymeacoffee.com/dallinwright).

## License

Licensed under the [Apache License, Version 2.0](LICENSE).

Contributions are accepted under the same license, per section 5 of the Apache
License: any contribution intentionally submitted for inclusion is licensed
Apache-2.0, with no additional terms.
