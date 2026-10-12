# chx-core

ClickHouse for Rust, used as `chx`: queries checked at compile time with
`query!`, typed rows over `RowBinaryWithNamesAndTypes`, bulk inserts as
`INSERT ... FORMAT RowBinary`, and schema migrations with checksummed,
per-statement history, so a migration that fails partway resumes at the
statement that failed.

```rust
let trades = chx::query!(
    "SELECT id, venue, px FROM trades WHERE venue = {venue:String}",
    venue,
)
.fetch_all(&client)
.await?;
```

For the command line tool, install [`chx-cli`](https://crates.io/crates/chx-cli).
See the [repository README](https://github.com/anthid-labs/chx) for the full
behaviour.
