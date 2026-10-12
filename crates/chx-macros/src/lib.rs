//! The proc macros behind `chx`: compile-time checked queries and the
//! `FromRow` derive.
//!
//! Do not depend on this crate directly. `chx-core` re-exports every macro,
//! and the generated code refers to `::chx::`, its library name.
//!
//! # How a query is checked
//!
//! The parameters a query takes are in its text, as `{name:Type}`, so they
//! need no server: the macro parses them and requires an argument for each,
//! of a Rust type that binds as the declared one. The columns a query returns
//! are what the server says they are, from `DESCRIBE TABLE (<query>)`, run
//! once at compile time against `CLICKHOUSE_URL`, or read from the `.chx`
//! cache that `chx prepare` writes.
//!
//! Which of the two is used, in order:
//!
//! 1. `CHX_OFFLINE=true`: the `.chx` cache only. A query with no entry is a
//!    compile error that says to run `chx prepare`.
//! 2. `CLICKHOUSE_URL` set, in the environment or the nearest `.env` at or
//!    above the crate: the server.
//! 3. Neither: the cache if it has the query, else an error naming both ways
//!    out.
//!
//! The cache is the `.chx` directory nearest the crate, searched upward, so a
//! workspace can keep one at its root.

mod derive;
mod describe;
mod expand;
mod input;
mod rust_type;

use proc_macro::TokenStream;

/// Checks a query at compile time and returns it as a `QueryAs` of an
/// anonymous record struct with one field per column.
///
/// ```ignore
/// let rows = chx::query!(
///     "SELECT id, venue FROM trades WHERE venue = {venue:String} LIMIT {n:UInt64}",
///     venue,
///     n = 10u64,
/// )
/// .fetch_all(&client)
/// .await?;
/// println!("{} {}", rows[0].id, rows[0].venue);
/// ```
///
/// Arguments are `name = expr`, or a bare `name` for a variable of the same
/// name, one per parameter in the SQL. The expression must bind as the
/// parameter's type: a `&str` or `String` for `String`, an `Option<u64>` for
/// `Nullable(UInt64)`, and so on; see `chx::BindAs`.
///
/// A column can be read as another type with an alias of the form
/// `AS "name: path::Type"`, where the type implements `chx::Decode`.
#[proc_macro]
pub fn query(input: TokenStream) -> TokenStream {
    expand::expand(input, expand::Mode::Record)
}

/// Like [`query!`], but reads each row into the named struct. Every column
/// must match a field by name and type, and every field must have a column.
///
/// ```ignore
/// struct Trade { id: u64, venue: String }
/// let trades = chx::query_as!(Trade, "SELECT id, venue FROM trades").fetch_all(&client).await?;
/// ```
///
/// The struct does not need to implement `FromRow`: the macro writes the
/// struct literal, so a field of the wrong type is a compile error.
#[proc_macro]
pub fn query_as(input: TokenStream) -> TokenStream {
    expand::expand(input, expand::Mode::As)
}

/// Like [`query!`], but reads the first column of each row.
#[proc_macro]
pub fn query_scalar(input: TokenStream) -> TokenStream {
    expand::expand(input, expand::Mode::Scalar)
}

/// [`query!`] with the SQL read from a file, relative to the crate's
/// `Cargo.toml`. The build tracks the file, so editing it recompiles.
#[proc_macro]
pub fn query_file(input: TokenStream) -> TokenStream {
    expand::expand(input, expand::Mode::File)
}

/// A query that is not checked against a database. Parameters are still
/// parsed from the SQL and bound by name; rows come back as `chx::Row`.
///
/// For a query no `DESCRIBE` can check: one with an `{name:Identifier}`
/// parameter, or one built at run time.
#[proc_macro]
pub fn query_unchecked(input: TokenStream) -> TokenStream {
    expand::expand(input, expand::Mode::Unchecked)
}

/// Reads a struct from a `chx::Row`, each field by its column name.
///
/// ```ignore
/// #[derive(chx::FromRow)]
/// struct Trade {
///     id: u64,
///     #[chx(rename = "px")]
///     price: chx::types::Decimal,
///     #[chx(default)]
///     note: Option<String>,
/// }
/// ```
///
/// `rename` reads another column; `default` uses `Default::default()` when
/// the column is missing. A tuple struct reads columns by position.
#[proc_macro_derive(FromRow, attributes(chx))]
pub fn derive_from_row(input: TokenStream) -> TokenStream {
    derive::from_row(input)
}
