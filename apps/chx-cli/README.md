# chx-cli

Installs the `chx` command.

```bash
cargo install chx-cli
export CLICKHOUSE_URL=http://default:@localhost:8123/analytics
chx migrate run       # apply pending migrations
chx prepare           # write .chx, the offline cache for query! and friends
chx prepare --check   # fail if .chx is stale; for CI
```

See the [repository README](https://github.com/anthid-labs/chx).
