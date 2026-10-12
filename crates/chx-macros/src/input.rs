//! What the macros take: `[Path,] "sql" [, name = expr | name]*`.

use syn::parse::{Parse, ParseStream};
use syn::{Expr, Ident, LitStr, Path, Token};

pub struct Input {
    /// The struct `query_as!` reads into.
    pub record: Option<Path>,
    pub sql: LitStr,
    pub args: Vec<Arg>,
}

pub struct Arg {
    pub name: Ident,
    pub value: Expr,
}

impl Input {
    pub fn parse(input: ParseStream<'_>, with_record: bool) -> syn::Result<Self> {
        let record = if with_record {
            let path = input.parse::<Path>()?;
            input.parse::<Token![,]>()?;
            Some(path)
        } else {
            None
        };
        let sql = input.parse::<LitStr>()?;
        let mut args = Vec::new();
        while !input.is_empty() {
            input.parse::<Token![,]>()?;
            if input.is_empty() {
                break;
            }
            let name = input.parse::<Ident>()?;
            let value = if input.peek(Token![=]) {
                input.parse::<Token![=]>()?;
                input.parse::<Expr>()?
            } else {
                // `query!(sql, venue)` binds the variable `venue` to the
                // parameter `venue`, as format strings capture.
                syn::parse_quote!(#name)
            };
            args.push(Arg { name, value });
        }
        Ok(Self { record, sql, args })
    }
}

pub struct Plain(pub Input);

impl Parse for Plain {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        Input::parse(input, false).map(Plain)
    }
}

pub struct WithRecord(pub Input);

impl Parse for WithRecord {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        Input::parse(input, true).map(WithRecord)
    }
}

#[cfg(test)]
mod tests {
    use quote::{ToTokens, quote};

    use super::*;

    #[test]
    fn sql_then_named_and_bare_arguments() {
        let input = syn::parse2::<Plain>(quote!("SELECT 1", a = 1 + 2, b, c = name.len(),))
            .unwrap()
            .0;

        assert!(input.record.is_none());
        assert_eq!(input.sql.value(), "SELECT 1");
        let args: Vec<(String, String)> = input
            .args
            .iter()
            .map(|arg| {
                (
                    arg.name.to_string(),
                    arg.value.to_token_stream().to_string().replace(' ', ""),
                )
            })
            .collect();
        assert_eq!(
            args,
            [
                ("a".to_string(), "1+2".to_string()),
                ("b".to_string(), "b".to_string()),
                ("c".to_string(), "name.len()".to_string()),
            ]
        );
    }

    #[test]
    fn query_as_takes_a_path_first() {
        let input = syn::parse2::<WithRecord>(quote!(models::Trade, "SELECT 1"))
            .unwrap()
            .0;
        let record = input.record.unwrap().to_token_stream().to_string();
        assert_eq!(record.replace(' ', ""), "models::Trade");
        assert!(input.args.is_empty());
    }

    #[test]
    fn what_is_not_a_query_is_refused() {
        assert!(syn::parse2::<Plain>(quote!()).is_err());
        assert!(syn::parse2::<Plain>(quote!(sql_variable)).is_err());
        assert!(syn::parse2::<Plain>(quote!("SELECT 1", 42)).is_err());
        assert!(syn::parse2::<Plain>(quote!("SELECT 1", a = )).is_err());
        assert!(syn::parse2::<WithRecord>(quote!("SELECT 1")).is_err());
        assert!(syn::parse2::<WithRecord>(quote!(Trade "SELECT 1")).is_err());
    }
}
