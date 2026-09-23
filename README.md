# chx

ClickHouse schema migrations, in the shape of `sqlx migrate`.

```bash
cargo install chx-cli
CLICKHOUSE_URL=http://default:@localhost:8123/analytics chx migrate run
```

`chx migrate run` applies every `.sql` file in `./migrations` that has not run
yet, in version order, and records a SHA-256 of each file in a
`_chx_migrations` table in the target database.

## Why

ClickHouse DDL is not transactional. A migration that fails on its third
statement has already applied the first two, and a tool that records a
migration only when it finishes does not know that. The next run fails on the
first `CREATE TABLE`, and you work out by hand which half of the file is live.

chx writes a history row after **every statement**. A failed migration stays
recorded as incomplete with the count of statements that landed, and the next
run starts at the one that failed. Fix that statement in place and run again.

## Migrations

```
migrations/
  20260923114500_create_trades.sql
  20260923120000_add_venue.sql
```

- Named `<version>_<description>.sql`. The version is an integer, and a
  timestamp avoids collisions between branches.
- Several statements per file, separated by `;`. Semicolons inside strings,
  quoted identifiers and comments are handled.
- Other files in the directory are ignored. A `.sql` file without a version,
  a duplicate version, or a `.up.sql` / `.down.sql` file is refused.

## What a run refuses

All of these stop the run before any statement is sent:

| Situation | Why |
| --- | --- |
| An applied file's checksum changed | The schema is what the old file did. Put the change in a new migration. |
| An applied version has no file | Nobody can rebuild the schema from the repository. |
| A new file is numbered below the latest applied one | Usually a branch merged late. Renumber it above the latest. |

The stored checksum is plain `sha256sum` output, so you can check it by hand:

```sql
SELECT version, checksum, applied, statements, success
FROM _chx_migrations FINAL ORDER BY version
```

## Connection

`--database-url` or `CLICKHOUSE_URL`:

```
http[s]://user:password@host:port/database?setting=value
```

- The HTTP interface (8123 / 8443), not the native protocol.
- Credentials are sent as `X-ClickHouse-User` / `X-ClickHouse-Key` headers,
  never in the URL.
- Query parameters are passed through as ClickHouse settings.
- chx does **not** read `.env` files. The URL comes from the flag or the
  process environment only.

Every statement runs with `wait_end_of_query=1`, `async_insert=0` and
`mutations_sync=2`, so a statement is not counted as done until its effect is.

## Not done yet

- No lock. Do not run two `chx migrate run` at once against one database.
- No `ON CLUSTER` awareness. The history table is local to the node you
  connect to.
- No `info`, `add` or `revert` commands.
- No `$$` heredoc strings in the statement splitter.

## License

Apache-2.0
