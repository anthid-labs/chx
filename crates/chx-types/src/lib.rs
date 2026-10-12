//! What chx and its macros agree on.
//!
//! A proc-macro crate cannot depend on the crate that re-exports it, so
//! anything `chx-macros` needs at compile time and `chx-core` needs at run
//! time lives here: the [type model](types), the [parameter grammar](params),
//! the [connection URL](endpoint) and the [offline cache](cache). Nothing in
//! this crate talks to a server.

#![warn(missing_docs)]

pub mod cache;
pub mod endpoint;
pub mod params;
pub mod types;

pub use types::Type;
