//! Code generation for the query macros.
//!
//! Every macro ends in a call into `chx::query`, the same builder a caller
//! writes by hand. What the macro adds is known at compile time: the record
//! struct a `query!` reads into, the struct literal a `query_as!` fills, the
//! column list the result is checked against, and the `bind_as` wrapper on
//! each argument that makes a wrong Rust type a compile error.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use chx_types::params::{self, Param};
use proc_macro2::{Span, TokenStream};
use quote::{ToTokens, quote};
use syn::{Ident, LitStr};

use crate::describe::{self, Column};
use crate::input::{Arg, Input, Plain, WithRecord};
use crate::rust_type::rust_type;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Record,
    As,
    Scalar,
    File,
    Unchecked,
}

pub fn expand(input: proc_macro::TokenStream, mode: Mode) -> proc_macro::TokenStream {
    let parsed = if mode == Mode::As {
        syn::parse::<WithRecord>(input).map(|p| p.0)
    } else {
        syn::parse::<Plain>(input).map(|p| p.0)
    };
    let input = match parsed {
        Ok(input) => input,
        Err(err) => return err.to_compile_error().into(),
    };
    let manifest_dir = std::env::var_os("CARGO_MANIFEST_DIR").map(PathBuf::from);
    match generate(input, mode, manifest_dir.as_deref(), &describe::columns) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

/// How the columns of a query are found. The real one asks the server or the
/// cache; tests pass their own.
type Describe<'a> = &'a dyn Fn(&str, &[Param]) -> Result<Vec<Column>, String>;

/// The expansion of one macro call. `manifest_dir` is where `query_file!`
/// paths are relative to.
fn generate(
    input: Input,
    mode: Mode,
    manifest_dir: Option<&Path>,
    describe: Describe<'_>,
) -> syn::Result<TokenStream> {
    let span = input.sql.span();
    let (sql, tracked) = match mode {
        Mode::File => {
            let (sql, path) = read_file(&input.sql, manifest_dir)?;
            let path = path.to_string_lossy().into_owned();
            // `include_str!` of the same file, so cargo recompiles when it
            // changes.
            (
                sql,
                quote!(
                    const _: &str = include_str!(#path);
                ),
            )
        }
        _ => (input.sql.value(), TokenStream::new()),
    };

    let params = params::parse(&sql).map_err(|err| syn::Error::new(span, err.to_string()))?;
    check_args(&params, &input.args, span)?;
    let binds = binds(&params, &input.args, mode == Mode::Unchecked)?;
    let sql_lit = LitStr::new(&sql, span);

    if mode == Mode::Unchecked {
        return Ok(quote! {{
            #tracked
            ::chx::query(#sql_lit) #(#binds)*
        }});
    }

    let columns = describe(&sql, &params).map_err(|err| syn::Error::new(span, err))?;
    let names: Vec<LitStr> = columns
        .iter()
        .map(|column| LitStr::new(&column.name, span))
        .collect();
    let fields = if mode == Mode::Scalar {
        Vec::new()
    } else {
        fields(&columns, span)?
    };

    let body = match mode {
        Mode::Record | Mode::File => {
            let idents = fields.iter().map(|field| &field.ident);
            let types = fields.iter().map(|field| &field.ty);
            let reads = fields.iter().map(|field| field.read());
            quote! {
                #[derive(Debug, Clone, PartialEq)]
                struct Record {
                    #(pub #idents: #types,)*
                }
                impl ::chx::FromRow for Record {
                    fn from_row(row: &::chx::Row) -> ::chx::Result<Self> {
                        ::std::result::Result::Ok(Record { #(#reads,)* })
                    }
                }
                ::chx::query_as::<Record>(#sql_lit)
                    .expect_columns(&[#(#names,)*])
                    #(#binds)*
            }
        }
        Mode::As => {
            let record = input.record.expect("query_as! parsed a path");
            let reads = fields.iter().map(|field| field.read());
            quote! {
                ::chx::query(#sql_lit)
                    .expect_columns(&[#(#names,)*])
                    #(#binds)*
                    .try_map(|row: &::chx::Row| -> ::chx::Result<#record> {
                        ::std::result::Result::Ok(#record { #(#reads,)* })
                    })
            }
        }
        Mode::Scalar => {
            // By position, so the column needs no name a struct could use.
            let first = columns
                .first()
                .ok_or_else(|| syn::Error::new(span, "the query returns no columns"))?;
            let ty = column_type(first, span)?;
            quote! {
                ::chx::query(#sql_lit)
                    .expect_columns(&[#(#names,)*])
                    #(#binds)*
                    .try_map(|row: &::chx::Row| -> ::chx::Result<#ty> {
                        row.try_get::<#ty, usize>(0usize)
                    })
            }
        }
        Mode::Unchecked => unreachable!(),
    };

    // `option_env!` is expanded by rustc in the caller's crate, which records
    // the variable in that crate's dependency info, and cargo rebuilds the
    // crate when it changes. That is what makes `chx prepare` sound: it runs
    // the build with a fresh `CHX_TMP`, so every crate holding a checked
    // query is recompiled and its macros run, however fresh cargo thought it
    // was and whoever built it last. `CLICKHOUSE_URL` is deliberately not
    // tracked this way: its value would be written, password and all, into
    // the target directory.
    Ok(quote! {{
        #tracked
        const _: (::core::option::Option<&str>, ::core::option::Option<&str>) = (
            ::core::option_env!("CHX_TMP"),
            ::core::option_env!("CHX_OFFLINE"),
        );
        #body
    }})
}

fn read_file(lit: &LitStr, manifest_dir: Option<&Path>) -> syn::Result<(String, PathBuf)> {
    let manifest_dir =
        manifest_dir.ok_or_else(|| syn::Error::new(lit.span(), "CARGO_MANIFEST_DIR is not set"))?;
    let path = manifest_dir.join(lit.value());
    let sql = std::fs::read_to_string(&path).map_err(|err| {
        syn::Error::new(lit.span(), format!("cannot read {}: {err}", path.display()))
    })?;
    Ok((sql, path))
}

/// Every parameter has exactly one argument, and every argument a parameter.
fn check_args(params: &[Param], args: &[Arg], span: Span) -> syn::Result<()> {
    let mut seen = BTreeSet::new();
    for arg in args {
        let name = arg.name.to_string();
        if !seen.insert(name.clone()) {
            return Err(syn::Error::new(
                arg.name.span(),
                format!("{name} is given twice"),
            ));
        }
        if !params.iter().any(|param| param.name == name) {
            return Err(syn::Error::new(
                arg.name.span(),
                format!("the query has no parameter {{{name}:...}}"),
            ));
        }
    }
    let missing: Vec<&str> = params
        .iter()
        .filter(|param| !seen.contains(&param.name))
        .map(|param| param.name.as_str())
        .collect();
    if !missing.is_empty() {
        return Err(syn::Error::new(
            span,
            format!("no argument for parameter {}", missing.join(", ")),
        ));
    }
    Ok(())
}

/// `.bind("name", bind_as::<Target, _>(&expr))` for each argument, which is
/// the compile-time type check on the parameter. Unchecked queries bind
/// plainly when the type has no Rust mapping.
fn binds(params: &[Param], args: &[Arg], unchecked: bool) -> syn::Result<Vec<TokenStream>> {
    args.iter()
        .map(|arg| {
            let name = arg.name.to_string();
            let param = params
                .iter()
                .find(|param| param.name == name)
                .expect("checked above");
            let name_lit = LitStr::new(&name, arg.name.span());
            let value = &arg.value;
            match rust_type(&param.ty) {
                Ok(target) => Ok(quote! {
                    .bind(#name_lit, ::chx::__private::bind_as::<#target, _>(&#value))
                }),
                Err(_) if unchecked => Ok(quote!(.bind(#name_lit, &#value))),
                Err(err) => Err(syn::Error::new(
                    arg.name.span(),
                    format!("parameter {{{name}:{}}}: {err}", param.ty),
                )),
            }
        })
        .collect()
}

struct Field {
    index: usize,
    ident: Ident,
    ty: TokenStream,
}

impl Field {
    fn read(&self) -> TokenStream {
        let Field { index, ident, ty } = self;
        quote!(#ident: row.try_get::<#ty, usize>(#index)?)
    }
}

/// A column's name and, if it was aliased `"name: path::Type"`, the type to
/// read it as instead of the mapping.
fn split_alias(column: &Column) -> (&str, Option<&str>) {
    match column.name.split_once(':') {
        Some((name, ty)) => (name.trim(), Some(ty.trim())),
        None => (column.name.as_str(), None),
    }
}

/// The Rust type a column is read as: the alias override, else the mapping.
fn column_type(column: &Column, span: Span) -> syn::Result<TokenStream> {
    let (name, override_ty) = split_alias(column);
    match override_ty {
        Some(text) => syn::parse_str::<syn::Type>(text)
            .map(|ty| ty.into_token_stream())
            .map_err(|err| syn::Error::new(span, format!("column {name:?}: {err}"))),
        None => rust_type(&column.ty)
            .map_err(|err| syn::Error::new(span, format!("column {name:?}: {err}"))),
    }
}

/// A field per column. The name is the column's, which has to be a Rust
/// identifier.
fn fields(columns: &[Column], span: Span) -> syn::Result<Vec<Field>> {
    let mut seen = BTreeSet::new();
    columns
        .iter()
        .enumerate()
        .map(|(index, column)| {
            let (name, _) = split_alias(column);
            if !seen.insert(name.to_string()) {
                return Err(syn::Error::new(
                    span,
                    format!("column {name:?} appears twice; alias one of them with AS"),
                ));
            }
            let ident = syn::parse_str::<Ident>(name).map_err(|_| {
                syn::Error::new(
                    span,
                    format!("column {name:?} is not a Rust identifier; alias it with AS"),
                )
            })?;
            let ty = column_type(column, span)?;
            Ok(Field { index, ident, ty })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use chx_types::types::Type;

    use super::*;

    fn column(name: &str, ty: &str) -> Column {
        Column {
            name: name.to_string(),
            ty: Type::parse(ty).unwrap(),
        }
    }

    /// Expands `tokens` as `mode` would, with `columns` standing in for the
    /// database, and returns the code with whitespace removed.
    fn expand_with(
        tokens: TokenStream,
        mode: Mode,
        columns: Vec<Column>,
    ) -> Result<String, String> {
        let input = if mode == Mode::As {
            syn::parse2::<WithRecord>(tokens).map(|p| p.0)
        } else {
            syn::parse2::<Plain>(tokens).map(|p| p.0)
        }
        .map_err(|err| err.to_string())?;
        let describe = move |_: &str, _: &[Param]| Ok(columns.clone());
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let tokens =
            generate(input, mode, Some(manifest_dir), &describe).map_err(|err| err.to_string())?;
        // Whatever is generated has to be an expression: the macros are used
        // in expression position.
        syn::parse2::<syn::Expr>(tokens.clone())
            .map_err(|err| format!("not an expression: {err}"))?;
        Ok(tokens.to_string().replace([' ', '\n'], ""))
    }

    #[test]
    fn query_generates_a_record_with_a_field_per_column() {
        let code = expand_with(
            quote!("SELECT id, note FROM t WHERE v = {v:String}", v = venue),
            Mode::Record,
            vec![column("id", "UInt64"), column("note", "Nullable(String)")],
        )
        .unwrap();

        assert!(
            code.contains(
                "structRecord{pubid:u64,pubnote:::std::option::Option<::std::string::String>,}"
            ),
            "{code}"
        );
        assert!(
            code.contains("id:row.try_get::<u64,usize>(0usize)?"),
            "{code}"
        );
        assert!(
            code.contains("::chx::query_as::<Record>(\"SELECTid,noteFROMtWHEREv={v:String}\")"),
            "{code}"
        );
        assert!(
            code.contains(".expect_columns(&[\"id\",\"note\",])"),
            "{code}"
        );
        assert!(
            code.contains(
                ".bind(\"v\",::chx::__private::bind_as::<::std::string::String,_>(&venue))"
            ),
            "{code}"
        );
    }

    #[test]
    fn a_bare_argument_binds_the_variable_of_that_name() {
        let code = expand_with(
            quote!("SELECT {n:UInt64} AS n", n),
            Mode::Record,
            vec![column("n", "UInt64")],
        )
        .unwrap();

        assert!(
            code.contains(".bind(\"n\",::chx::__private::bind_as::<u64,_>(&n))"),
            "{code}"
        );
    }

    #[test]
    fn query_as_fills_the_named_struct_as_a_literal() {
        let code = expand_with(
            quote!(crate::Trade, "SELECT id, venue FROM t"),
            Mode::As,
            vec![
                column("id", "UInt64"),
                column("venue", "LowCardinality(String)"),
            ],
        )
        .unwrap();

        assert!(code.contains("->::chx::Result<crate::Trade>"), "{code}");
        assert!(code.contains("crate::Trade{id:row.try_get::<u64,usize>(0usize)?,venue:row.try_get::<::std::string::String,usize>(1usize)?,}"), "{code}");
        assert!(!code.contains("structRecord"), "{code}");
    }

    #[test]
    fn query_scalar_reads_the_first_column_whatever_it_is_called() {
        let code = expand_with(
            quote!("SELECT count() FROM t"),
            Mode::Scalar,
            vec![column("count()", "UInt64")],
        )
        .unwrap();

        assert!(code.contains("row.try_get::<u64,usize>(0usize)"), "{code}");
        assert!(code.contains(".expect_columns(&[\"count()\",])"), "{code}");

        let err =
            expand_with(quote!("ALTER TABLE t DELETE WHERE 1"), Mode::Scalar, vec![]).unwrap_err();
        assert_eq!(err, "the query returns no columns");
    }

    #[test]
    fn query_unchecked_binds_without_describing() {
        let input = syn::parse2::<Plain>(quote!(
            "SELECT id FROM {t:Identifier} WHERE id = {id:UInt64}",
            t = table,
            id = 1u64
        ))
        .unwrap()
        .0;
        let never = |_: &str, _: &[Param]| -> Result<Vec<Column>, String> {
            panic!("an unchecked query must not be described")
        };

        let code = generate(input, Mode::Unchecked, None, &never)
            .unwrap()
            .to_string()
            .replace(' ', "");

        // No Rust type maps to Identifier, so it binds plainly; the typed
        // parameter keeps its check.
        assert!(code.contains(".bind(\"t\",&table)"), "{code}");
        assert!(
            code.contains(".bind(\"id\",::chx::__private::bind_as::<u64,_>(&1u64))"),
            "{code}"
        );
        assert!(code.starts_with("{::chx::query("), "{code}");
    }

    #[test]
    fn query_file_reads_the_sql_and_tracks_the_file() {
        let code = expand_with(
            quote!("../chx-core/tests/sql/probe_by_venue.sql", venue = "ARCA"),
            Mode::File,
            vec![column("id", "UInt64"), column("note", "Nullable(String)")],
        )
        .unwrap();

        assert!(code.contains("const_:&str=include_str!("), "{code}");
        assert!(code.contains("probe_by_venue.sql"), "{code}");
        assert!(code.contains("WHEREvenue={venue:String}"), "{code}");

        let err = expand_with(quote!("no/such/file.sql"), Mode::File, vec![]).unwrap_err();
        assert!(err.contains("cannot read"), "{err}");
    }

    #[test]
    fn query_file_needs_a_crate_to_be_relative_to() {
        let input = syn::parse2::<Plain>(quote!("q.sql")).unwrap().0;
        let describe = |_: &str, _: &[Param]| Ok(Vec::new());
        let err = generate(input, Mode::File, None, &describe).unwrap_err();
        assert!(err.to_string().contains("CARGO_MANIFEST_DIR"), "{err}");
    }

    #[test]
    fn a_column_alias_overrides_the_mapped_type() {
        let code = expand_with(
            quote!("SELECT kind AS \"kind: i8\", px AS \"px: my::Money\" FROM t"),
            Mode::Record,
            vec![
                column("kind: i8", "Enum8('a' = 1)"),
                column("px: my::Money", "Decimal(38, 18)"),
            ],
        )
        .unwrap();

        assert!(code.contains("pubkind:i8,pubpx:my::Money,"), "{code}");
        // The check at run time is against the name the server really sends.
        assert!(
            code.contains(".expect_columns(&[\"kind:i8\",\"px:my::Money\",])"),
            "{code}"
        );

        let err = expand_with(
            quote!("SELECT 1"),
            Mode::Record,
            vec![column("x: not a type", "UInt8")],
        )
        .unwrap_err();
        assert!(err.contains("column \"x\""), "{err}");
    }

    #[test]
    fn arguments_and_parameters_must_match_one_to_one() {
        let columns = || vec![column("x", "UInt8")];
        let err = |tokens| expand_with(tokens, Mode::Record, columns()).unwrap_err();

        assert_eq!(
            err(quote!("SELECT {a:UInt8} AS x, {b:UInt8}", a = 1u8)),
            "no argument for parameter b"
        );
        assert_eq!(
            err(quote!("SELECT {a:UInt8} AS x", a = 1u8, extra = 2u8)),
            "the query has no parameter {extra:...}"
        );
        assert_eq!(
            err(quote!("SELECT {a:UInt8} AS x", a = 1u8, a = 2u8)),
            "a is given twice"
        );
    }

    #[test]
    fn columns_that_cannot_be_fields_are_refused_with_the_fix() {
        let err = |columns| expand_with(quote!("SELECT 1"), Mode::Record, columns).unwrap_err();

        assert_eq!(
            err(vec![column("count()", "UInt64")]),
            "column \"count()\" is not a Rust identifier; alias it with AS"
        );
        assert_eq!(
            err(vec![column("id", "UInt64"), column("id", "UInt64")]),
            "column \"id\" appears twice; alias one of them with AS"
        );
        assert!(err(vec![column("j", "JSON")]).contains("toString()"));
    }

    #[test]
    fn a_parameter_with_no_rust_type_is_refused_in_a_checked_query() {
        let err = expand_with(
            quote!("SELECT id FROM {t:Identifier}", t = "x"),
            Mode::Record,
            vec![column("id", "UInt64")],
        )
        .unwrap_err();
        assert!(err.contains("{t:Identifier}"), "{err}");
        assert!(err.contains("query_unchecked!"), "{err}");
    }

    #[test]
    fn a_describe_failure_and_a_bad_parameter_are_compile_errors() {
        let input = syn::parse2::<Plain>(quote!("SELECT * FROM missing"))
            .unwrap()
            .0;
        let failing = |_: &str, _: &[Param]| Err("Unknown table missing".to_string());
        let err = generate(input, Mode::Record, None, &failing).unwrap_err();
        assert_eq!(err.to_string(), "Unknown table missing");

        let err =
            expand_with(quote!("SELECT {a:Array()}", a = 1u8), Mode::Record, vec![]).unwrap_err();
        assert!(err.contains("parameter {a:...}"), "{err}");
    }

    #[test]
    fn malformed_macro_input_is_a_parse_error() {
        assert!(expand_with(quote!(42), Mode::Record, vec![]).is_err());
        assert!(expand_with(quote!("SELECT 1"), Mode::As, vec![]).is_err());
        assert!(expand_with(quote!("SELECT 1" extra), Mode::Record, vec![]).is_err());
        // A trailing comma is fine.
        assert!(
            expand_with(
                quote!("SELECT 1 AS x",),
                Mode::Record,
                vec![column("x", "UInt8")]
            )
            .is_ok()
        );
    }

    /// The mechanism `chx prepare` relies on to make the macros run again.
    #[test]
    fn a_checked_query_ties_its_crate_to_the_prepare_variables() {
        for (tokens, mode) in [
            (quote!("SELECT 1 AS x"), Mode::Record),
            (quote!("SELECT 1 AS x"), Mode::Scalar),
            (quote!(T, "SELECT 1 AS x"), Mode::As),
        ] {
            let code = expand_with(tokens, mode, vec![column("x", "UInt8")]).unwrap();
            assert!(code.contains("::core::option_env!(\"CHX_TMP\")"), "{code}");
            assert!(
                code.contains("::core::option_env!(\"CHX_OFFLINE\")"),
                "{code}"
            );
            // The URL carries a password and must never reach the target directory.
            assert!(!code.contains("CLICKHOUSE_URL"), "{code}");
        }
    }
}
