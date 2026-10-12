//! `#[derive(FromRow)]`.

use proc_macro2::TokenStream;
use quote::quote;
use syn::spanned::Spanned;
use syn::{Data, DeriveInput, Fields, LitStr};

pub fn from_row(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    let input = syn::parse_macro_input!(input as DeriveInput);
    match generate(input) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

fn generate(input: DeriveInput) -> syn::Result<TokenStream> {
    let name = &input.ident;
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();

    let Data::Struct(data) = &input.data else {
        return Err(syn::Error::new(
            input.span(),
            "FromRow can only be derived for a struct",
        ));
    };

    let body = match &data.fields {
        Fields::Named(fields) => {
            let reads = fields
                .named
                .iter()
                .map(|field| {
                    let ident = field.ident.as_ref().expect("named");
                    let attrs = Attrs::parse(&field.attrs)?;
                    let column = attrs.rename.unwrap_or_else(|| ident.to_string());
                    let column = LitStr::new(&column, ident.span());
                    let read = if attrs.default {
                        quote! {
                            match row.try_get::<_, &str>(#column) {
                                ::std::result::Result::Ok(value) => value,
                                ::std::result::Result::Err(::chx::Error::ColumnNotFound(_)) => {
                                    ::std::default::Default::default()
                                }
                                ::std::result::Result::Err(err) => return ::std::result::Result::Err(err),
                            }
                        }
                    } else {
                        quote!(row.try_get::<_, &str>(#column)?)
                    };
                    Ok(quote!(#ident: #read))
                })
                .collect::<syn::Result<Vec<_>>>()?;
            quote!(#name { #(#reads,)* })
        }
        Fields::Unnamed(fields) => {
            let reads =
                (0..fields.unnamed.len()).map(|index| quote!(row.try_get::<_, usize>(#index)?));
            quote!(#name(#(#reads,)*))
        }
        Fields::Unit => quote!(#name),
    };

    Ok(quote! {
        #[automatically_derived]
        impl #impl_generics ::chx::FromRow for #name #ty_generics #where_clause {
            fn from_row(row: &::chx::Row) -> ::chx::Result<Self> {
                ::std::result::Result::Ok(#body)
            }
        }
    })
}

#[derive(Default)]
struct Attrs {
    rename: Option<String>,
    default: bool,
}

impl Attrs {
    /// `#[chx(rename = "column")]` and `#[chx(default)]`.
    fn parse(attrs: &[syn::Attribute]) -> syn::Result<Self> {
        let mut out = Attrs::default();
        for attr in attrs.iter().filter(|attr| attr.path().is_ident("chx")) {
            attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("rename") {
                    let value: LitStr = meta.value()?.parse()?;
                    out.rename = Some(value.value());
                    Ok(())
                } else if meta.path.is_ident("default") {
                    out.default = true;
                    Ok(())
                } else {
                    Err(meta.error("expected `rename = \"...\"` or `default`"))
                }
            })?;
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expand(input: TokenStream) -> Result<String, String> {
        let input = syn::parse2::<DeriveInput>(input).map_err(|err| err.to_string())?;
        let tokens = generate(input).map_err(|err| err.to_string())?;
        syn::parse2::<syn::ItemImpl>(tokens.clone())
            .map_err(|err| format!("not an impl: {err}"))?;
        Ok(tokens.to_string().replace([' ', '\n'], ""))
    }

    #[test]
    fn named_fields_are_read_by_column_name() {
        let code = expand(quote! {
            struct Trade {
                id: u64,
                venue: String,
            }
        })
        .unwrap();

        assert!(code.contains("impl::chx::FromRowforTrade"), "{code}");
        assert!(code.contains("id:row.try_get::<_,&str>(\"id\")?"), "{code}");
        assert!(
            code.contains("venue:row.try_get::<_,&str>(\"venue\")?"),
            "{code}"
        );
    }

    #[test]
    fn rename_reads_another_column_and_default_tolerates_a_missing_one() {
        let code = expand(quote! {
            struct Trade {
                #[chx(rename = "px")]
                price: u64,
                #[chx(default)]
                note: Option<String>,
                #[chx(rename = "v", default)]
                venue: String,
            }
        })
        .unwrap();

        assert!(
            code.contains("price:row.try_get::<_,&str>(\"px\")?"),
            "{code}"
        );
        assert!(
            code.contains("note:matchrow.try_get::<_,&str>(\"note\")"),
            "{code}"
        );
        assert!(
            code.contains("::chx::Error::ColumnNotFound(_))=>{::std::default::Default::default()}"),
            "{code}"
        );
        assert!(
            code.contains("venue:matchrow.try_get::<_,&str>(\"v\")"),
            "{code}"
        );
    }

    #[test]
    fn tuple_structs_read_by_position_and_unit_structs_read_nothing() {
        let code = expand(quote!(
            struct Pair(u64, String);
        ))
        .unwrap();
        assert!(
            code.contains("Pair(row.try_get::<_,usize>(0usize)?,row.try_get::<_,usize>(1usize)?,)"),
            "{code}"
        );

        let code = expand(quote!(
            struct Nothing;
        ))
        .unwrap();
        assert!(
            code.contains("::std::result::Result::Ok(Nothing)"),
            "{code}"
        );
    }

    #[test]
    fn generics_are_carried_onto_the_impl() {
        let code = expand(quote! {
            struct Wrapper<T: ::chx::Decode> where T: Clone {
                value: T,
            }
        })
        .unwrap();

        assert!(
            code.contains("impl<T:::chx::Decode>::chx::FromRowforWrapper<T>whereT:Clone"),
            "{code}"
        );
    }

    #[test]
    fn enums_and_unknown_attributes_are_refused() {
        let err = expand(quote!(
            enum Side {
                Buy,
                Sell,
            }
        ))
        .unwrap_err();
        assert_eq!(err, "FromRow can only be derived for a struct");

        let err = expand(quote! {
            struct Trade {
                #[chx(skip)]
                id: u64,
            }
        })
        .unwrap_err();
        assert!(
            err.contains("expected `rename = \"...\"` or `default`"),
            "{err}"
        );
    }

    #[test]
    fn attributes_of_other_macros_are_ignored() {
        let code = expand(quote! {
            struct Trade {
                #[serde(rename = "ID")]
                id: u64,
            }
        })
        .unwrap();
        assert!(code.contains("row.try_get::<_,&str>(\"id\")?"), "{code}");
    }
}
