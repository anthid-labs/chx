//! The Rust type a column of a given ClickHouse type is read as, and the one
//! a parameter of that type is bound as.
//!
//! One mapping for both, which is what makes `query_as!` work: the macro
//! decodes each column as this type and assigns it to the struct's field,
//! so the field has to be this type. The mapping is the default; a column
//! alias `"name: path::Type"` overrides it.

use chx_types::types::Type;
use proc_macro2::TokenStream;
use quote::quote;

pub fn rust_type(ty: &Type) -> Result<TokenStream, String> {
    let tokens = match ty {
        Type::UInt8 => quote!(u8),
        Type::UInt16 => quote!(u16),
        Type::UInt32 => quote!(u32),
        Type::UInt64 => quote!(u64),
        Type::UInt128 => quote!(u128),
        Type::Int8 => quote!(i8),
        Type::Int16 => quote!(i16),
        Type::Int32 => quote!(i32),
        Type::Int64 => quote!(i64),
        Type::Int128 => quote!(i128),
        Type::UInt256 | Type::Int256 => {
            return Err(format!(
                "{ty} has no Rust type; cast it, for example with toString()"
            ));
        }
        Type::Float32 => quote!(f32),
        Type::Float64 => quote!(f64),
        Type::Bool => quote!(bool),
        Type::String | Type::Enum8(_) | Type::Enum16(_) => quote!(::std::string::String),
        Type::FixedString(n) => quote!([u8; #n]),
        Type::Date | Type::Date32 => {
            if !cfg!(feature = "chrono") {
                return Err(format!("{ty} needs the `chrono` feature of chx-core"));
            }
            quote!(::chx::__private::chrono::NaiveDate)
        }
        Type::DateTime { .. } | Type::DateTime64 { .. } => {
            if !cfg!(feature = "chrono") {
                return Err(format!("{ty} needs the `chrono` feature of chx-core"));
            }
            quote!(::chx::__private::chrono::DateTime<::chx::__private::chrono::Utc>)
        }
        Type::Decimal { precision, .. } if *precision <= 38 => quote!(::chx::types::Decimal),
        Type::Decimal { .. } => {
            return Err(format!(
                "{ty} is wider than 128 bits; cast it to Decimal128 or a string"
            ));
        }
        Type::Uuid => {
            if !cfg!(feature = "uuid") {
                return Err(format!("{ty} needs the `uuid` feature of chx-core"));
            }
            quote!(::chx::__private::uuid::Uuid)
        }
        Type::IPv4 => quote!(::std::net::Ipv4Addr),
        Type::IPv6 => quote!(::std::net::Ipv6Addr),
        Type::Nullable(inner) => {
            let inner = rust_type(inner)?;
            quote!(::std::option::Option<#inner>)
        }
        Type::LowCardinality(inner) | Type::SimpleAggregateFunction { inner, .. } => {
            rust_type(inner)?
        }
        Type::Array(inner) => {
            let inner = rust_type(inner)?;
            quote!(::std::vec::Vec<#inner>)
        }
        Type::Map(key, value) => {
            let key = rust_type(key)?;
            let value = rust_type(value)?;
            quote!(::std::vec::Vec<(#key, #value)>)
        }
        Type::Tuple(elements) => {
            let elements = elements
                .iter()
                .map(|element| rust_type(&element.ty))
                .collect::<Result<Vec<_>, _>>()?;
            quote!((#(#elements,)*))
        }
        Type::Nothing => {
            return Err(
                "a bare NULL has no type; cast it, for example with CAST(NULL AS Nullable(String))"
                    .to_string(),
            );
        }
        Type::Identifier => {
            return Err(
                "an Identifier parameter cannot be checked; use query_unchecked!".to_string(),
            );
        }
        Type::Other(raw) => {
            return Err(format!(
                "{raw} is not a type chx can read; cast it, for example with toString()"
            ));
        }
    };
    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(text: &str) -> String {
        rust_type(&Type::parse(text).unwrap())
            .unwrap_or_else(|err| panic!("{err}"))
            .to_string()
            .replace(' ', "")
    }

    #[test]
    fn scalars_and_wrappers() {
        assert_eq!(map("UInt64"), "u64");
        assert_eq!(
            map("Nullable(String)"),
            "::std::option::Option<::std::string::String>"
        );
        assert_eq!(map("LowCardinality(String)"), "::std::string::String");
        assert_eq!(map("Array(UInt8)"), "::std::vec::Vec<u8>");
        assert_eq!(
            map("Map(String, UInt8)"),
            "::std::vec::Vec<(::std::string::String,u8)>"
        );
        assert_eq!(map("Tuple(UInt8, String)"), "(u8,::std::string::String,)");
        assert_eq!(map("FixedString(16)"), "[u8;16usize]");
        assert_eq!(map("Decimal(38, 18)"), "::chx::types::Decimal");
        assert_eq!(map("Enum8('a' = 1)"), "::std::string::String");
    }

    #[test]
    fn every_scalar_maps_to_its_exact_width() {
        for (clickhouse, rust) in [
            ("UInt8", "u8"),
            ("UInt16", "u16"),
            ("UInt32", "u32"),
            ("UInt128", "u128"),
            ("Int8", "i8"),
            ("Int16", "i16"),
            ("Int32", "i32"),
            ("Int64", "i64"),
            ("Int128", "i128"),
            ("Float32", "f32"),
            ("Float64", "f64"),
            ("Bool", "bool"),
            ("IPv4", "::std::net::Ipv4Addr"),
            ("IPv6", "::std::net::Ipv6Addr"),
            ("Enum16('x' = 300)", "::std::string::String"),
            ("SimpleAggregateFunction(sum, UInt64)", "u64"),
            ("Decimal(9, 2)", "::chx::types::Decimal"),
        ] {
            assert_eq!(map(clickhouse), rust, "{clickhouse}");
        }
    }

    #[cfg(feature = "chrono")]
    #[test]
    fn dates_and_times_map_to_chrono() {
        assert_eq!(map("Date"), "::chx::__private::chrono::NaiveDate");
        assert_eq!(map("Date32"), "::chx::__private::chrono::NaiveDate");
        let instant = "::chx::__private::chrono::DateTime<::chx::__private::chrono::Utc>";
        assert_eq!(map("DateTime('UTC')"), instant);
        assert_eq!(map("DateTime64(9)"), instant);
        assert_eq!(
            map("Array(Nullable(DateTime64(3)))"),
            format!("::std::vec::Vec<::std::option::Option<{instant}>>")
        );
    }

    #[cfg(not(feature = "chrono"))]
    #[test]
    fn dates_and_times_name_the_feature_they_need() {
        for text in ["Date", "DateTime", "DateTime64(3)"] {
            let err = rust_type(&Type::parse(text).unwrap()).unwrap_err();
            assert!(err.contains("`chrono` feature"), "{err}");
        }
    }

    #[cfg(feature = "uuid")]
    #[test]
    fn uuids_map_to_uuid() {
        assert_eq!(map("UUID"), "::chx::__private::uuid::Uuid");
    }

    #[cfg(not(feature = "uuid"))]
    #[test]
    fn uuids_name_the_feature_they_need() {
        let err = rust_type(&Type::Uuid).unwrap_err();
        assert!(err.contains("`uuid` feature"), "{err}");
    }

    #[test]
    fn an_unmappable_type_inside_a_wrapper_is_still_refused() {
        for text in [
            "Nullable(UInt256)",
            "Array(JSON)",
            "Map(String, Dynamic)",
            "Map(UInt256, String)",
            "Tuple(UInt8, Int256)",
            "LowCardinality(Nothing)",
        ] {
            assert!(rust_type(&Type::parse(text).unwrap()).is_err(), "{text}");
        }
    }

    #[test]
    fn types_without_a_rust_type_say_what_to_do() {
        for text in [
            "UInt256",
            "Dynamic",
            "Identifier",
            "Nothing",
            "Decimal(76, 0)",
        ] {
            let err = rust_type(&Type::parse(text).unwrap()).unwrap_err();
            assert!(!err.is_empty(), "{text}");
        }
    }
}
