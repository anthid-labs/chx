# chx-types

The pieces of [chx](https://github.com/anthid-labs/chx) that the library and
its proc macros must agree on: the ClickHouse type model and parser, the
`{name:Type}` query parameter grammar, the connection URL, and the
`.chx/query-<hash>.json` cache format.

A proc-macro crate cannot depend on the crate that re-exports it, so this
sits underneath both `chx-macros` and `chx-core`. Depend on `chx-core`, which
re-exports what is useful here.
