# chx-core

The engine behind the `chx` command: ClickHouse schema migrations with
checksummed, per-statement history, so a migration that fails partway resumes
at the statement that failed.

For the command line tool, install [`chx-cli`](https://crates.io/crates/chx-cli).
See the [repository README](https://github.com/anthid-labs/chx) for the full
behaviour.
