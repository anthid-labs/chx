# chx-macros

The proc macros behind [chx](https://github.com/anthid-labs/chx): `query!`,
`query_as!`, `query_scalar!`, `query_file!`, `query_unchecked!` and
`#[derive(FromRow)]`.

Do not depend on this crate directly. `chx-core` re-exports every macro, and
the code they generate refers to `chx-core`'s types.
