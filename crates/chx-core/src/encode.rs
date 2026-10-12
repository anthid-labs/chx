//! Writing values for ClickHouse, two ways.
//!
//! A value goes to the server either in a RowBinary body, for an `INSERT`, or
//! as the text of a query parameter, for a `{name:Type}` in the SQL. Both are
//! driven by the ClickHouse type the value is written as, which is why
//! [`Encode`] takes one: a `DateTime64` needs its precision and a `Decimal`
//! its scale before a single byte can be produced.
//!
//! The text form is the one the server reads parameters in. At the top level
//! that is the escaped TSV form: a string is bare, with `\t`, `\n` and `\\`
//! escaped, and NULL is `\N`. Inside an array, map or tuple it is the literal
//! form: strings and dates quoted, NULL spelled out.

use std::collections::{BTreeMap, HashMap};
use std::net::{Ipv4Addr, Ipv6Addr};

use crate::decode::pair_type;
use crate::error::{Error, Result};
use crate::types::{Bytes, Decimal, Type};

/// A Rust type that can be written as a ClickHouse value.
///
/// Implemented for the integer and float types, `bool`, `str` and `String`,
/// [`Decimal`], [`Bytes`], `[u8; N]`, `Option<T>`, slices and `Vec<T>`,
/// `BTreeMap` and `HashMap`, tuples of up to twelve elements, the `std::net`
/// address types, and, with their features, `uuid::Uuid` and the `chrono`
/// date and time types. A reference to any of them encodes as the value.
///
/// Both forms of one value:
///
/// ```
/// use chx::{Encode, Type};
///
/// let ty = Type::parse("Array(Nullable(String))").unwrap();
/// let value = vec![Some("it's"), None];
///
/// // RowBinary, as an INSERT sends it: a length, then flag and value each.
/// let mut bytes = Vec::new();
/// value.encode(&ty, &mut bytes).unwrap();
/// assert_eq!(bytes, b"\x02\x00\x04it's\x01");
///
/// // Text, as a `{name:Array(Nullable(String))}` parameter is sent.
/// let mut text = String::new();
/// value.encode_text(&ty, &mut text, false).unwrap();
/// assert_eq!(text, r"['it\'s', NULL]");
/// ```
///
/// # Implementing it
///
/// A newtype usually delegates to the type it wraps. Add [`BindAs`] to let
/// the checked macros accept it as an argument:
///
/// ```
/// use chx::{BindAs, Encode, Result, Type};
///
/// struct Symbol(String);
///
/// impl Encode for Symbol {
///     fn accepts(ty: &Type) -> bool {
///         String::accepts(ty)
///     }
///
///     fn encode(&self, ty: &Type, buf: &mut Vec<u8>) -> Result<()> {
///         self.0.encode(ty, buf)
///     }
///
///     fn encode_text(&self, ty: &Type, out: &mut String, nested: bool) -> Result<()> {
///         self.0.encode_text(ty, out, nested)
///     }
/// }
///
/// // `query!("... {symbol:String}", symbol = Symbol(..))` now compiles.
/// impl BindAs<String> for Symbol {}
///
/// let query = chx::query("SELECT * FROM trades WHERE symbol = {symbol:String}")
///     .bind("symbol", &Symbol("AAPL".into()));
/// assert_eq!(query.params()[0].name, "symbol");
/// ```
pub trait Encode {
    /// Which ClickHouse types this Rust type can be written as.
    fn accepts(ty: &Type) -> bool;

    /// Appends the RowBinary form of `self` as a `ty`.
    fn encode(&self, ty: &Type, buf: &mut Vec<u8>) -> Result<()>;

    /// Appends the text form of `self` as a `ty`. `nested` is whether the
    /// value sits inside an array, map or tuple.
    fn encode_text(&self, ty: &Type, out: &mut String, nested: bool) -> Result<()>;

    /// The `key:value` text of a map entry. Only a two-element tuple has one;
    /// it is how an `Array(Tuple(K, V))` in Rust writes a `Map(K, V)`.
    fn encode_text_pair(&self, _key: &Type, _value: &Type, _out: &mut String) -> Result<()> {
        Err(Error::Encode(format!(
            "{} is not a key-value pair",
            std::any::type_name::<Self>()
        )))
    }
}

impl<T: Encode + ?Sized> Encode for &T {
    fn accepts(ty: &Type) -> bool {
        T::accepts(ty)
    }

    fn encode(&self, ty: &Type, buf: &mut Vec<u8>) -> Result<()> {
        (**self).encode(ty, buf)
    }

    fn encode_text(&self, ty: &Type, out: &mut String, nested: bool) -> Result<()> {
        (**self).encode_text(ty, out, nested)
    }

    fn encode_text_pair(&self, key: &Type, value: &Type, out: &mut String) -> Result<()> {
        (**self).encode_text_pair(key, value, out)
    }
}

fn mismatch<T: ?Sized>(ty: &Type) -> Error {
    Error::Encode(format!(
        "{} cannot be written as {ty}",
        std::any::type_name::<T>()
    ))
}

/// Unsigned LEB128, as every length in RowBinary is.
pub fn write_varint(buf: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        buf.push((value as u8) | 0x80);
        value >>= 7;
    }
    buf.push(value as u8);
}

/// A string in the text form: bare and TSV-escaped at the top level, a
/// quoted literal when nested.
pub fn write_text(out: &mut String, text: &str, nested: bool) {
    if nested {
        out.push('\'');
    }
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '\0' => out.push_str("\\0"),
            '\'' if nested => out.push_str("\\'"),
            c => out.push(c),
        }
    }
    if nested {
        out.push('\'');
    }
}

fn write_string_bytes(bytes: &[u8], ty: &Type, buf: &mut Vec<u8>, what: &str) -> Result<()> {
    match ty.unwrapped() {
        Type::String => {
            write_varint(buf, bytes.len() as u64);
            buf.extend_from_slice(bytes);
            Ok(())
        }
        Type::FixedString(n) if bytes.len() <= *n => {
            buf.extend_from_slice(bytes);
            buf.resize(buf.len() + (n - bytes.len()), 0);
            Ok(())
        }
        Type::FixedString(n) => Err(Error::Encode(format!(
            "{what} of {} bytes does not fit in FixedString({n})",
            bytes.len()
        ))),
        other => Err(Error::Encode(format!(
            "{what} cannot be written as {other}"
        ))),
    }
}

/// A value longer than its `FixedString(N)` is refused here rather than by
/// the server, which is the same rule the binary form applies.
fn check_fixed_length(length: usize, ty: &Type) -> Result<()> {
    match ty.unwrapped() {
        Type::FixedString(n) if length > *n => Err(Error::Encode(format!(
            "{length} bytes do not fit in FixedString({n})"
        ))),
        _ => Ok(()),
    }
}

fn enum_value<'a>(
    variants: impl Iterator<Item = (&'a String, i64)>,
    name: &str,
    ty: &Type,
) -> Result<i64> {
    variants
        .into_iter()
        .find(|(variant, _)| variant.as_str() == name)
        .map(|(_, value)| value)
        .ok_or_else(|| Error::Encode(format!("{ty} has no variant named {name:?}")))
}

fn enum_name<'a>(
    variants: impl Iterator<Item = (&'a String, i64)>,
    value: i64,
    ty: &Type,
) -> Result<&'a str> {
    variants
        .into_iter()
        .find(|(_, v)| *v == value)
        .map(|(name, _)| name.as_str())
        .ok_or_else(|| Error::Encode(format!("{ty} has no variant numbered {value}")))
}

macro_rules! integers {
    ($($rust:ty => $variant:pat_param,)*) => {$(
        impl Encode for $rust {
            fn accepts(ty: &Type) -> bool {
                matches!(ty.unwrapped(), $variant)
            }

            fn encode(&self, ty: &Type, buf: &mut Vec<u8>) -> Result<()> {
                match ty.unwrapped() {
                    $variant => {
                        buf.extend_from_slice(&self.to_le_bytes());
                        Ok(())
                    }
                    other => Err(mismatch::<$rust>(other)),
                }
            }

            fn encode_text(&self, ty: &Type, out: &mut String, _nested: bool) -> Result<()> {
                match ty.unwrapped() {
                    $variant => {
                        out.push_str(&self.to_string());
                        Ok(())
                    }
                    other => Err(mismatch::<$rust>(other)),
                }
            }
        }
    )*};
}

integers! {
    u8 => Type::UInt8,
    u16 => Type::UInt16,
    u32 => Type::UInt32,
    u64 => Type::UInt64,
    u128 => Type::UInt128,
    i32 => Type::Int32,
    i64 => Type::Int64,
    i128 => Type::Int128,
}

/// `Int8`, or an `Enum8` by number.
impl Encode for i8 {
    fn accepts(ty: &Type) -> bool {
        matches!(ty.unwrapped(), Type::Int8 | Type::Enum8(_))
    }

    fn encode(&self, ty: &Type, buf: &mut Vec<u8>) -> Result<()> {
        match ty.unwrapped() {
            Type::Int8 => {}
            Type::Enum8(variants) => {
                enum_name(
                    variants.iter().map(|(n, v)| (n, i64::from(*v))),
                    i64::from(*self),
                    ty,
                )?;
            }
            other => return Err(mismatch::<i8>(other)),
        }
        buf.push(*self as u8);
        Ok(())
    }

    fn encode_text(&self, ty: &Type, out: &mut String, nested: bool) -> Result<()> {
        match ty.unwrapped() {
            Type::Int8 => out.push_str(&self.to_string()),
            Type::Enum8(variants) => {
                let name = enum_name(
                    variants.iter().map(|(n, v)| (n, i64::from(*v))),
                    i64::from(*self),
                    ty,
                )?;
                write_text(out, name, nested);
            }
            other => return Err(mismatch::<i8>(other)),
        }
        Ok(())
    }
}

/// `Int16`, or an `Enum16` by number.
impl Encode for i16 {
    fn accepts(ty: &Type) -> bool {
        matches!(ty.unwrapped(), Type::Int16 | Type::Enum16(_))
    }

    fn encode(&self, ty: &Type, buf: &mut Vec<u8>) -> Result<()> {
        match ty.unwrapped() {
            Type::Int16 => {}
            Type::Enum16(variants) => {
                enum_name(
                    variants.iter().map(|(n, v)| (n, i64::from(*v))),
                    i64::from(*self),
                    ty,
                )?;
            }
            other => return Err(mismatch::<i16>(other)),
        }
        buf.extend_from_slice(&self.to_le_bytes());
        Ok(())
    }

    fn encode_text(&self, ty: &Type, out: &mut String, nested: bool) -> Result<()> {
        match ty.unwrapped() {
            Type::Int16 => out.push_str(&self.to_string()),
            Type::Enum16(variants) => {
                let name = enum_name(
                    variants.iter().map(|(n, v)| (n, i64::from(*v))),
                    i64::from(*self),
                    ty,
                )?;
                write_text(out, name, nested);
            }
            other => return Err(mismatch::<i16>(other)),
        }
        Ok(())
    }
}

macro_rules! floats {
    ($($rust:ty => $variant:pat_param,)*) => {$(
        impl Encode for $rust {
            fn accepts(ty: &Type) -> bool {
                matches!(ty.unwrapped(), $variant)
            }

            fn encode(&self, ty: &Type, buf: &mut Vec<u8>) -> Result<()> {
                match ty.unwrapped() {
                    $variant => {
                        buf.extend_from_slice(&self.to_le_bytes());
                        Ok(())
                    }
                    other => Err(mismatch::<$rust>(other)),
                }
            }

            fn encode_text(&self, ty: &Type, out: &mut String, _nested: bool) -> Result<()> {
                match ty.unwrapped() {
                    $variant => {
                        // The server reads these spellings; Rust's `NaN` it does not.
                        if self.is_nan() {
                            out.push_str("nan");
                        } else if self.is_infinite() {
                            out.push_str(if *self > 0.0 { "inf" } else { "-inf" });
                        } else {
                            out.push_str(&self.to_string());
                        }
                        Ok(())
                    }
                    other => Err(mismatch::<$rust>(other)),
                }
            }
        }
    )*};
}

floats! {
    f32 => Type::Float32,
    f64 => Type::Float64,
}

impl Encode for bool {
    fn accepts(ty: &Type) -> bool {
        matches!(ty.unwrapped(), Type::Bool)
    }

    fn encode(&self, ty: &Type, buf: &mut Vec<u8>) -> Result<()> {
        match ty.unwrapped() {
            Type::Bool => {
                buf.push(u8::from(*self));
                Ok(())
            }
            other => Err(mismatch::<bool>(other)),
        }
    }

    fn encode_text(&self, ty: &Type, out: &mut String, _nested: bool) -> Result<()> {
        match ty.unwrapped() {
            Type::Bool => {
                out.push_str(if *self { "true" } else { "false" });
                Ok(())
            }
            other => Err(mismatch::<bool>(other)),
        }
    }
}

/// Rescaled to the column's scale; refused if that loses digits or the
/// mantissa does not fit the column's precision.
impl Encode for Decimal {
    fn accepts(ty: &Type) -> bool {
        matches!(ty.unwrapped(), Type::Decimal { precision, .. } if *precision <= 38)
    }

    fn encode(&self, ty: &Type, buf: &mut Vec<u8>) -> Result<()> {
        let Type::Decimal { precision, scale } = ty.unwrapped() else {
            return Err(mismatch::<Decimal>(ty));
        };
        let mantissa = self.rescale(*scale)?.mantissa();
        let limit = 10i128.checked_pow(u32::from(*precision));
        if limit.is_some_and(|limit| mantissa.unsigned_abs() >= limit.unsigned_abs()) {
            return Err(Error::Encode(format!("{self} does not fit in {ty}")));
        }
        match precision {
            0..=9 => buf.extend_from_slice(&(mantissa as i32).to_le_bytes()),
            10..=18 => buf.extend_from_slice(&(mantissa as i64).to_le_bytes()),
            19..=38 => buf.extend_from_slice(&mantissa.to_le_bytes()),
            _ => return Err(mismatch::<Decimal>(ty)),
        }
        Ok(())
    }

    fn encode_text(&self, ty: &Type, out: &mut String, _nested: bool) -> Result<()> {
        match ty.unwrapped() {
            // The same limit as the binary form: a mantissa is an i128.
            Type::Decimal { precision, scale } if *precision <= 38 => {
                out.push_str(&self.rescale(*scale)?.to_string());
                Ok(())
            }
            other => Err(mismatch::<Decimal>(other)),
        }
    }
}

/// Text: a `String`, a `FixedString` it fits in, an enum by name, or an
/// `Identifier` parameter.
impl Encode for str {
    fn accepts(ty: &Type) -> bool {
        matches!(
            ty.unwrapped(),
            Type::String
                | Type::FixedString(_)
                | Type::Enum8(_)
                | Type::Enum16(_)
                | Type::Identifier
        )
    }

    fn encode(&self, ty: &Type, buf: &mut Vec<u8>) -> Result<()> {
        match ty.unwrapped() {
            Type::Enum8(variants) => {
                let value = enum_value(variants.iter().map(|(n, v)| (n, i64::from(*v))), self, ty)?;
                buf.push(value as i8 as u8);
                Ok(())
            }
            Type::Enum16(variants) => {
                let value = enum_value(variants.iter().map(|(n, v)| (n, i64::from(*v))), self, ty)?;
                buf.extend_from_slice(&(value as i16).to_le_bytes());
                Ok(())
            }
            Type::Identifier => Err(Error::Encode(
                "an Identifier is only valid as a query parameter".to_string(),
            )),
            _ => write_string_bytes(self.as_bytes(), ty, buf, "string"),
        }
    }

    fn encode_text(&self, ty: &Type, out: &mut String, nested: bool) -> Result<()> {
        match ty.unwrapped() {
            Type::String | Type::FixedString(_) => {
                check_fixed_length(self.len(), ty)?;
                write_text(out, self, nested);
            }
            Type::Enum8(variants) => {
                enum_value(variants.iter().map(|(n, v)| (n, i64::from(*v))), self, ty)?;
                write_text(out, self, nested);
            }
            Type::Enum16(variants) => {
                enum_value(variants.iter().map(|(n, v)| (n, i64::from(*v))), self, ty)?;
                write_text(out, self, nested);
            }
            Type::Identifier if !nested => out.push_str(self),
            other => return Err(mismatch::<str>(other)),
        }
        Ok(())
    }
}

impl Encode for String {
    fn accepts(ty: &Type) -> bool {
        str::accepts(ty)
    }

    fn encode(&self, ty: &Type, buf: &mut Vec<u8>) -> Result<()> {
        self.as_str().encode(ty, buf)
    }

    fn encode_text(&self, ty: &Type, out: &mut String, nested: bool) -> Result<()> {
        self.as_str().encode_text(ty, out, nested)
    }
}

fn encode_bytes_text(bytes: &[u8], ty: &Type, out: &mut String, nested: bool) -> Result<()> {
    check_fixed_length(bytes.len(), ty)?;
    match ty.unwrapped() {
        Type::String | Type::FixedString(_) => match std::str::from_utf8(bytes) {
            Ok(text) => {
                write_text(out, text, nested);
                Ok(())
            }
            Err(_) => Err(Error::Encode(
                "binary data cannot be passed as a query parameter; insert it with RowBinary"
                    .into(),
            )),
        },
        other => Err(Error::Encode(format!("bytes cannot be written as {other}"))),
    }
}

impl Encode for Bytes {
    fn accepts(ty: &Type) -> bool {
        matches!(ty.unwrapped(), Type::String | Type::FixedString(_))
    }

    fn encode(&self, ty: &Type, buf: &mut Vec<u8>) -> Result<()> {
        write_string_bytes(&self.0, ty, buf, "byte string")
    }

    fn encode_text(&self, ty: &Type, out: &mut String, nested: bool) -> Result<()> {
        encode_bytes_text(&self.0, ty, out, nested)
    }
}

impl<const N: usize> Encode for [u8; N] {
    fn accepts(ty: &Type) -> bool {
        matches!(ty.unwrapped(), Type::String)
            || matches!(ty.unwrapped(), Type::FixedString(n) if *n == N)
    }

    fn encode(&self, ty: &Type, buf: &mut Vec<u8>) -> Result<()> {
        write_string_bytes(self, ty, buf, "byte array")
    }

    fn encode_text(&self, ty: &Type, out: &mut String, nested: bool) -> Result<()> {
        encode_bytes_text(self, ty, out, nested)
    }
}

/// `Nullable(T)`, or a plain `T` when the value is `Some`.
impl<T: Encode> Encode for Option<T> {
    fn accepts(ty: &Type) -> bool {
        match ty.unwrapped() {
            Type::Nullable(inner) => T::accepts(inner),
            other => T::accepts(other),
        }
    }

    fn encode(&self, ty: &Type, buf: &mut Vec<u8>) -> Result<()> {
        match (ty.unwrapped(), self) {
            (Type::Nullable(_), None) => {
                buf.push(1);
                Ok(())
            }
            (Type::Nullable(inner), Some(value)) => {
                buf.push(0);
                value.encode(inner, buf)
            }
            (_, Some(value)) => value.encode(ty, buf),
            (other, None) => Err(Error::Encode(format!("NULL cannot be written as {other}"))),
        }
    }

    fn encode_text(&self, ty: &Type, out: &mut String, nested: bool) -> Result<()> {
        match (ty.unwrapped(), self) {
            (Type::Nullable(_), None) => {
                out.push_str(if nested { "NULL" } else { "\\N" });
                Ok(())
            }
            (Type::Nullable(inner), Some(value)) => value.encode_text(inner, out, nested),
            (_, Some(value)) => value.encode_text(ty, out, nested),
            (other, None) => Err(Error::Encode(format!("NULL cannot be written as {other}"))),
        }
    }
}

/// `Array(T)`, or a `Map(K, V)` written from its pairs.
impl<T: Encode> Encode for [T] {
    fn accepts(ty: &Type) -> bool {
        match ty.unwrapped() {
            Type::Array(inner) => T::accepts(inner),
            Type::Map(key, value) => T::accepts(&pair_type(key, value)),
            _ => false,
        }
    }

    fn encode(&self, ty: &Type, buf: &mut Vec<u8>) -> Result<()> {
        let element = match ty.unwrapped() {
            Type::Array(inner) => (**inner).clone(),
            Type::Map(key, value) => pair_type(key, value),
            other => return Err(mismatch::<[T]>(other)),
        };
        // Checked here and not only by the caller, because an empty slice
        // would otherwise write as an array of anything.
        if !T::accepts(&element) {
            return Err(mismatch::<[T]>(ty));
        }
        write_varint(buf, self.len() as u64);
        for item in self {
            item.encode(&element, buf)?;
        }
        Ok(())
    }

    fn encode_text(&self, ty: &Type, out: &mut String, _nested: bool) -> Result<()> {
        if !Self::accepts(ty) {
            return Err(mismatch::<[T]>(ty));
        }
        match ty.unwrapped() {
            Type::Array(inner) => {
                out.push('[');
                for (i, item) in self.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    item.encode_text(inner, out, true)?;
                }
                out.push(']');
                Ok(())
            }
            Type::Map(key, value) => {
                out.push('{');
                for (i, item) in self.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    item.encode_text_pair(key, value, out)?;
                }
                out.push('}');
                Ok(())
            }
            other => Err(mismatch::<[T]>(other)),
        }
    }
}

impl<T: Encode> Encode for Vec<T> {
    fn accepts(ty: &Type) -> bool {
        <[T]>::accepts(ty)
    }

    fn encode(&self, ty: &Type, buf: &mut Vec<u8>) -> Result<()> {
        self.as_slice().encode(ty, buf)
    }

    fn encode_text(&self, ty: &Type, out: &mut String, nested: bool) -> Result<()> {
        self.as_slice().encode_text(ty, out, nested)
    }
}

macro_rules! maps {
    ($($map:ident;)*) => {$(
        impl<K: Encode, V: Encode> Encode for $map<K, V> {
            fn accepts(ty: &Type) -> bool {
                matches!(ty.unwrapped(), Type::Map(key, value) if K::accepts(key) && V::accepts(value))
            }

            fn encode(&self, ty: &Type, buf: &mut Vec<u8>) -> Result<()> {
                let Type::Map(key, value) = ty.unwrapped() else {
                    return Err(mismatch::<Self>(ty));
                };
                if !Self::accepts(ty) {
                    return Err(mismatch::<Self>(ty));
                }
                write_varint(buf, self.len() as u64);
                for (k, v) in self {
                    k.encode(key, buf)?;
                    v.encode(value, buf)?;
                }
                Ok(())
            }

            fn encode_text(&self, ty: &Type, out: &mut String, _nested: bool) -> Result<()> {
                let Type::Map(key, value) = ty.unwrapped() else {
                    return Err(mismatch::<Self>(ty));
                };
                if !Self::accepts(ty) {
                    return Err(mismatch::<Self>(ty));
                }
                out.push('{');
                for (i, (k, v)) in self.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    k.encode_text(key, out, true)?;
                    out.push(':');
                    v.encode_text(value, out, true)?;
                }
                out.push('}');
                Ok(())
            }
        }
    )*};
}

maps! {
    BTreeMap;
    HashMap;
}

macro_rules! tuples {
    ($(($($name:ident $index:tt),+);)*) => {$(
        impl<$($name: Encode),+> Encode for ($($name,)+) {
            fn accepts(ty: &Type) -> bool {
                match ty.unwrapped() {
                    Type::Tuple(elements) => {
                        elements.len() == tuples!(@count $($name)+)
                            $(&& $name::accepts(&elements[$index].ty))+
                    }
                    _ => false,
                }
            }

            fn encode(&self, ty: &Type, buf: &mut Vec<u8>) -> Result<()> {
                match ty.unwrapped() {
                    Type::Tuple(elements) if elements.len() == tuples!(@count $($name)+) => {
                        $(self.$index.encode(&elements[$index].ty, buf)?;)+
                        Ok(())
                    }
                    other => Err(mismatch::<Self>(other)),
                }
            }

            fn encode_text(&self, ty: &Type, out: &mut String, _nested: bool) -> Result<()> {
                match ty.unwrapped() {
                    Type::Tuple(elements) if elements.len() == tuples!(@count $($name)+) => {
                        out.push('(');
                        $(
                            if $index > 0 {
                                out.push_str(", ");
                            }
                            self.$index.encode_text(&elements[$index].ty, out, true)?;
                        )+
                        out.push(')');
                        Ok(())
                    }
                    other => Err(mismatch::<Self>(other)),
                }
            }
        }
    )*};
    (@count $($name:ident)+) => { [$(tuples!(@one $name)),+].len() };
    (@one $name:ident) => { () };
}

tuples! {
    (A 0);
    (A 0, B 1, C 2);
    (A 0, B 1, C 2, D 3);
    (A 0, B 1, C 2, D 3, E 4);
    (A 0, B 1, C 2, D 3, E 4, F 5);
    (A 0, B 1, C 2, D 3, E 4, F 5, G 6);
    (A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7);
    (A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8);
    (A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8, J 9);
    (A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8, J 9, K 10);
    (A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8, J 9, K 10, L 11);
}

/// The one tuple that is also a map entry.
impl<K: Encode, V: Encode> Encode for (K, V) {
    fn accepts(ty: &Type) -> bool {
        match ty.unwrapped() {
            Type::Tuple(elements) => {
                elements.len() == 2 && K::accepts(&elements[0].ty) && V::accepts(&elements[1].ty)
            }
            _ => false,
        }
    }

    fn encode(&self, ty: &Type, buf: &mut Vec<u8>) -> Result<()> {
        match ty.unwrapped() {
            Type::Tuple(elements) if elements.len() == 2 => {
                self.0.encode(&elements[0].ty, buf)?;
                self.1.encode(&elements[1].ty, buf)
            }
            other => Err(mismatch::<Self>(other)),
        }
    }

    fn encode_text(&self, ty: &Type, out: &mut String, _nested: bool) -> Result<()> {
        match ty.unwrapped() {
            Type::Tuple(elements) if elements.len() == 2 => {
                out.push('(');
                self.0.encode_text(&elements[0].ty, out, true)?;
                out.push_str(", ");
                self.1.encode_text(&elements[1].ty, out, true)?;
                out.push(')');
                Ok(())
            }
            other => Err(mismatch::<Self>(other)),
        }
    }

    fn encode_text_pair(&self, key: &Type, value: &Type, out: &mut String) -> Result<()> {
        self.0.encode_text(key, out, true)?;
        out.push(':');
        self.1.encode_text(value, out, true)
    }
}

impl Encode for Ipv4Addr {
    fn accepts(ty: &Type) -> bool {
        matches!(ty.unwrapped(), Type::IPv4)
    }

    fn encode(&self, ty: &Type, buf: &mut Vec<u8>) -> Result<()> {
        match ty.unwrapped() {
            Type::IPv4 => {
                buf.extend_from_slice(&u32::from(*self).to_le_bytes());
                Ok(())
            }
            other => Err(mismatch::<Ipv4Addr>(other)),
        }
    }

    fn encode_text(&self, ty: &Type, out: &mut String, nested: bool) -> Result<()> {
        match ty.unwrapped() {
            Type::IPv4 => {
                write_text(out, &self.to_string(), nested);
                Ok(())
            }
            other => Err(mismatch::<Ipv4Addr>(other)),
        }
    }
}

impl Encode for Ipv6Addr {
    fn accepts(ty: &Type) -> bool {
        matches!(ty.unwrapped(), Type::IPv6)
    }

    fn encode(&self, ty: &Type, buf: &mut Vec<u8>) -> Result<()> {
        match ty.unwrapped() {
            Type::IPv6 => {
                buf.extend_from_slice(&self.octets());
                Ok(())
            }
            other => Err(mismatch::<Ipv6Addr>(other)),
        }
    }

    fn encode_text(&self, ty: &Type, out: &mut String, nested: bool) -> Result<()> {
        match ty.unwrapped() {
            Type::IPv6 => {
                write_text(out, &self.to_string(), nested);
                Ok(())
            }
            other => Err(mismatch::<Ipv6Addr>(other)),
        }
    }
}

#[cfg(feature = "uuid")]
impl Encode for uuid::Uuid {
    fn accepts(ty: &Type) -> bool {
        matches!(ty.unwrapped(), Type::Uuid)
    }

    fn encode(&self, ty: &Type, buf: &mut Vec<u8>) -> Result<()> {
        match ty.unwrapped() {
            Type::Uuid => {
                let (high, low) = self.as_u64_pair();
                buf.extend_from_slice(&high.to_le_bytes());
                buf.extend_from_slice(&low.to_le_bytes());
                Ok(())
            }
            other => Err(mismatch::<uuid::Uuid>(other)),
        }
    }

    fn encode_text(&self, ty: &Type, out: &mut String, nested: bool) -> Result<()> {
        match ty.unwrapped() {
            Type::Uuid => {
                write_text(out, &self.hyphenated().to_string(), nested);
                Ok(())
            }
            other => Err(mismatch::<uuid::Uuid>(other)),
        }
    }
}

#[cfg(feature = "chrono")]
mod chrono_impls {
    use chrono::{DateTime, Datelike, NaiveDate, NaiveDateTime, Utc};

    use super::*;
    use crate::decode::EPOCH_DAYS_FROM_CE;

    fn days(date: &NaiveDate) -> i64 {
        i64::from(date.num_days_from_ce()) - i64::from(EPOCH_DAYS_FROM_CE)
    }

    impl Encode for NaiveDate {
        fn accepts(ty: &Type) -> bool {
            matches!(ty.unwrapped(), Type::Date | Type::Date32)
        }

        fn encode(&self, ty: &Type, buf: &mut Vec<u8>) -> Result<()> {
            let out_of_range = || Error::Encode(format!("{self} is out of range for {ty}"));
            match ty.unwrapped() {
                Type::Date => {
                    let days = u16::try_from(days(self)).map_err(|_| out_of_range())?;
                    buf.extend_from_slice(&days.to_le_bytes());
                }
                Type::Date32 => {
                    let days = i32::try_from(days(self)).map_err(|_| out_of_range())?;
                    buf.extend_from_slice(&days.to_le_bytes());
                }
                other => return Err(mismatch::<NaiveDate>(other)),
            }
            Ok(())
        }

        fn encode_text(&self, ty: &Type, out: &mut String, nested: bool) -> Result<()> {
            match ty.unwrapped() {
                Type::Date | Type::Date32 => {
                    write_text(out, &self.format("%Y-%m-%d").to_string(), nested);
                    Ok(())
                }
                other => Err(mismatch::<NaiveDate>(other)),
            }
        }
    }

    /// Ticks since the epoch at the column's precision.
    fn ticks(value: &DateTime<Utc>, precision: u8) -> Result<i64> {
        let unit = 10i64.pow(u32::from(precision));
        let fraction =
            i64::from(value.timestamp_subsec_nanos()) / 10i64.pow(9 - u32::from(precision));
        value
            .timestamp()
            .checked_mul(unit)
            .and_then(|seconds| seconds.checked_add(fraction))
            .ok_or_else(|| {
                Error::Encode(format!("{value} is out of range at precision {precision}"))
            })
    }

    /// Written as a Unix timestamp, so the column's display timezone cannot
    /// shift it: the server reads a bare number as seconds since the epoch
    /// whatever zone the column shows.
    impl Encode for DateTime<Utc> {
        fn accepts(ty: &Type) -> bool {
            matches!(
                ty.unwrapped(),
                Type::DateTime { .. } | Type::DateTime64 { .. }
            )
        }

        fn encode(&self, ty: &Type, buf: &mut Vec<u8>) -> Result<()> {
            match ty.unwrapped() {
                Type::DateTime { .. } => {
                    let seconds = u32::try_from(self.timestamp())
                        .map_err(|_| Error::Encode(format!("{self} is out of range for {ty}")))?;
                    buf.extend_from_slice(&seconds.to_le_bytes());
                }
                Type::DateTime64 { precision, .. } => {
                    buf.extend_from_slice(&ticks(self, *precision)?.to_le_bytes());
                }
                other => return Err(mismatch::<DateTime<Utc>>(other)),
            }
            Ok(())
        }

        fn encode_text(&self, ty: &Type, out: &mut String, nested: bool) -> Result<()> {
            let text = match ty.unwrapped() {
                Type::DateTime { .. } => self.timestamp().to_string(),
                Type::DateTime64 { precision: 0, .. } => self.timestamp().to_string(),
                Type::DateTime64 { precision, .. } => {
                    let unit = 10i64.pow(u32::from(*precision));
                    let ticks = ticks(self, *precision)?;
                    format!(
                        "{}.{:0width$}",
                        ticks.div_euclid(unit),
                        ticks.rem_euclid(unit),
                        width = usize::from(*precision)
                    )
                }
                other => return Err(mismatch::<DateTime<Utc>>(other)),
            };
            write_text(out, &text, nested);
            Ok(())
        }
    }

    /// Taken as UTC.
    impl Encode for NaiveDateTime {
        fn accepts(ty: &Type) -> bool {
            <DateTime<Utc>>::accepts(ty)
        }

        fn encode(&self, ty: &Type, buf: &mut Vec<u8>) -> Result<()> {
            self.and_utc().encode(ty, buf)
        }

        fn encode_text(&self, ty: &Type, out: &mut String, nested: bool) -> Result<()> {
            self.and_utc().encode_text(ty, out, nested)
        }
    }
}

/// The argument types a checked macro accepts for a parameter whose natural
/// Rust type is `Target`.
///
/// `query!` maps each `{name:Type}` to one Rust type, such as `String` for a
/// `String` parameter or `Option<u64>` for `Nullable(UInt64)`, and requires
/// the argument to implement `BindAs<ThatType>`. The implementations here let
/// a `&str` stand in for a `String`, a slice for a `Vec`, a `BTreeMap` for a
/// list of pairs, and a reference for anything. A type of your own that
/// should bind as, say, a `Decimal` implements both `Encode` and
/// `BindAs<Decimal>`.
///
/// This is a compile-time check only; [`Encode`] does the work.
pub trait BindAs<Target: ?Sized> {}

impl<T: ?Sized + BindAs<U>, U: ?Sized> BindAs<U> for &T {}

macro_rules! bind_as_self {
    ($($ty:ty),* $(,)?) => {$(impl BindAs<$ty> for $ty {})*};
}

bind_as_self!(
    u8, u16, u32, u64, u128, i8, i16, i32, i64, i128, f32, f64, bool, String, Decimal, Bytes,
    Ipv4Addr, Ipv6Addr,
);

impl BindAs<String> for str {}
impl BindAs<String> for Bytes {}
impl<const N: usize> BindAs<[u8; N]> for [u8; N] {}
impl<const N: usize> BindAs<[u8; N]> for str {}
impl<const N: usize> BindAs<[u8; N]> for String {}
impl<T: BindAs<U>, U> BindAs<Option<U>> for Option<T> {}
impl<T: BindAs<U>, U> BindAs<Vec<U>> for Vec<T> {}
impl<T: BindAs<U>, U> BindAs<Vec<U>> for [T] {}
impl<K: BindAs<K2>, V: BindAs<V2>, K2, V2> BindAs<Vec<(K2, V2)>> for BTreeMap<K, V> {}
impl<K: BindAs<K2>, V: BindAs<V2>, K2, V2> BindAs<Vec<(K2, V2)>> for HashMap<K, V> {}

macro_rules! bind_as_tuples {
    ($(($($name:ident $target:ident),+);)*) => {$(
        impl<$($name: BindAs<$target>, $target),+> BindAs<($($target,)+)> for ($($name,)+) {}
    )*};
}

bind_as_tuples! {
    (A A2);
    (A A2, B B2);
    (A A2, B B2, C C2);
    (A A2, B B2, C C2, D D2);
    (A A2, B B2, C C2, D D2, E E2);
    (A A2, B B2, C C2, D D2, E E2, F F2);
    (A A2, B B2, C C2, D D2, E E2, F F2, G G2);
    (A A2, B B2, C C2, D D2, E E2, F F2, G G2, H H2);
}

#[cfg(feature = "uuid")]
impl BindAs<uuid::Uuid> for uuid::Uuid {}

#[cfg(feature = "chrono")]
mod chrono_bind {
    use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};

    use super::BindAs;

    impl BindAs<NaiveDate> for NaiveDate {}
    impl BindAs<DateTime<Utc>> for DateTime<Utc> {}
    impl BindAs<DateTime<Utc>> for NaiveDateTime {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ty(text: &str) -> Type {
        Type::parse(text).unwrap()
    }

    fn binary<T: Encode + ?Sized>(value: &T, text: &str) -> Vec<u8> {
        let t = ty(text);
        assert!(T::accepts(&t), "should accept {t}");
        let mut buf = Vec::new();
        value
            .encode(&t, &mut buf)
            .unwrap_or_else(|err| panic!("{err}"));
        buf
    }

    fn param<T: Encode + ?Sized>(value: &T, text: &str) -> String {
        let mut out = String::new();
        value
            .encode_text(&ty(text), &mut out, false)
            .unwrap_or_else(|err| panic!("{err}"));
        out
    }

    #[test]
    fn varints_are_leb128() {
        let mut buf = Vec::new();
        write_varint(&mut buf, 624_485);
        assert_eq!(buf, [0xe5, 0x8e, 0x26]);
    }

    #[test]
    fn numbers_are_little_endian_and_strict() {
        assert_eq!(binary(&0x1234u16, "UInt16"), [0x34, 0x12]);
        assert_eq!(binary(&-1i32, "Int32"), [0xff; 4]);
        assert!(!u32::accepts(&ty("UInt64")));
        assert_eq!(param(&7u64, "UInt64"), "7");
        assert_eq!(param(&f64::NAN, "Float64"), "nan");
        assert_eq!(param(&f64::NEG_INFINITY, "Float64"), "-inf");
        assert_eq!(param(&true, "Bool"), "true");
        assert_eq!(binary(&true, "Bool"), [1]);
    }

    #[test]
    fn strings_quote_only_when_nested() {
        assert_eq!(binary("ab", "String"), b"\x02ab");
        assert_eq!(binary("ab", "FixedString(4)"), b"ab\0\0");
        assert!(
            "abcde"
                .encode(&ty("FixedString(4)"), &mut Vec::new())
                .is_err()
        );
        assert_eq!(param("a\tb\\c'd\ne", "String"), "a\\tb\\\\c'd\\ne");
        let mut out = String::new();
        "it's".encode_text(&ty("String"), &mut out, true).unwrap();
        assert_eq!(out, r"'it\'s'");
        assert_eq!(param("ab", "LowCardinality(String)"), "ab");
    }

    #[test]
    fn enums_are_written_by_name_or_number() {
        let enum8 = "Enum8('a' = 1, 'b' = 2)";
        assert_eq!(binary("b", enum8), [2]);
        assert_eq!(binary(&2i8, enum8), [2]);
        assert_eq!(param("b", enum8), "b");
        assert_eq!(param(&2i8, enum8), "b");
        assert!("c".encode(&ty(enum8), &mut Vec::new()).is_err());
        assert!(3i8.encode(&ty(enum8), &mut Vec::new()).is_err());
        assert_eq!(binary("x", "Enum16('x' = 300)"), [0x2c, 0x01]);
    }

    #[test]
    fn identifiers_are_text_only() {
        assert_eq!(param("trades", "Identifier"), "trades");
        assert!("trades".encode(&ty("Identifier"), &mut Vec::new()).is_err());
    }

    #[test]
    fn decimals_rescale_to_the_column() {
        assert_eq!(
            binary(&Decimal::new(15, 1), "Decimal(18, 4)"),
            15000i64.to_le_bytes()
        );
        assert_eq!(
            binary(&Decimal::new(15, 1), "Decimal(9, 2)"),
            150i32.to_le_bytes()
        );
        assert_eq!(
            binary(&Decimal::new(1, 0), "Decimal(38, 18)"),
            10i128.pow(18).to_le_bytes()
        );
        assert_eq!(
            param(&Decimal::new(15, 1), "Decimal(38, 18)"),
            "1.500000000000000000"
        );
        assert!(
            Decimal::new(15001, 4)
                .encode(&ty("Decimal(9, 2)"), &mut Vec::new())
                .is_err()
        );
        assert!(
            Decimal::new(10_000_000_000, 0)
                .encode(&ty("Decimal(9, 0)"), &mut Vec::new())
                .is_err()
        );
    }

    #[test]
    fn nullable_is_a_flag_byte_and_a_backslash_n() {
        assert_eq!(binary(&Some(7u8), "Nullable(UInt8)"), [0, 7]);
        assert_eq!(binary(&None::<u8>, "Nullable(UInt8)"), [1]);
        assert_eq!(binary(&Some(7u8), "UInt8"), [7]);
        assert!(None::<u8>.encode(&ty("UInt8"), &mut Vec::new()).is_err());
        assert_eq!(param(&None::<String>, "Nullable(String)"), "\\N");
        assert_eq!(
            param(&vec![Some(1u8), None], "Array(Nullable(UInt8))"),
            "[1, NULL]"
        );
    }

    #[test]
    fn collections_nest() {
        assert_eq!(binary(&["a", "b"][..], "Array(String)"), b"\x02\x01a\x01b");
        assert_eq!(
            param(&vec!["x'y", "z\\w"], "Array(String)"),
            r"['x\'y', 'z\\w']"
        );
        assert_eq!(
            binary(&BTreeMap::from([("k", 1u8)]), "Map(String, UInt8)"),
            b"\x01\x01k\x01"
        );
        assert_eq!(
            param(&BTreeMap::from([("k", 1u8)]), "Map(String, UInt8)"),
            "{'k':1}"
        );
        assert_eq!(param(&vec![("k", 1u8)], "Map(String, UInt8)"), "{'k':1}");
        assert_eq!(
            binary(&vec![("k", 1u8)], "Map(String, UInt8)"),
            b"\x01\x01k\x01"
        );
        assert_eq!(binary(&(1u8, "x"), "Tuple(UInt8, String)"), b"\x01\x01x");
        assert_eq!(param(&(1u8, "x"), "Tuple(UInt8, String)"), "(1, 'x')");
        assert_eq!(
            param(&(1u8, "x", 2u8), "Tuple(UInt8, String, UInt8)"),
            "(1, 'x', 2)"
        );
        assert!(!<(u8, u8)>::accepts(&ty("Tuple(UInt8)")));
    }

    #[test]
    fn addresses() {
        assert_eq!(binary(&Ipv4Addr::new(1, 2, 3, 4), "IPv4"), [4, 3, 2, 1]);
        assert_eq!(binary(&Ipv6Addr::LOCALHOST, "IPv6")[15], 1);
        assert_eq!(
            param(&vec![Ipv4Addr::new(1, 2, 3, 4)], "Array(IPv4)"),
            "['1.2.3.4']"
        );
    }

    #[cfg(feature = "uuid")]
    #[test]
    fn uuids_roundtrip_through_the_wire_order() {
        let id: uuid::Uuid = "01234567-89ab-cdef-0123-456789abcdef".parse().unwrap();
        let wire = binary(&id, "UUID");
        let decoded = <uuid::Uuid as crate::decode::Decode>::decode(
            &mut crate::decode::Reader::new(&wire),
            &ty("UUID"),
        )
        .unwrap();
        assert_eq!(decoded, id);
        assert_eq!(param(&id, "UUID"), "01234567-89ab-cdef-0123-456789abcdef");
    }

    #[cfg(feature = "chrono")]
    #[test]
    fn times_are_epoch_based_in_both_forms() {
        use chrono::{NaiveDate, TimeZone, Utc};

        let date = NaiveDate::from_ymd_opt(2026, 1, 2).unwrap();
        assert_eq!(binary(&date, "Date"), 20455u16.to_le_bytes());
        assert_eq!(binary(&date, "Date32"), 20455i32.to_le_bytes());
        assert_eq!(param(&date, "Date"), "2026-01-02");
        assert_eq!(param(&vec![date], "Array(Date)"), "['2026-01-02']");
        assert!(
            NaiveDate::from_ymd_opt(1960, 1, 1)
                .unwrap()
                .encode(&ty("Date"), &mut Vec::new())
                .is_err()
        );

        let at = Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap()
            + chrono::Duration::milliseconds(678);
        assert_eq!(binary(&at, "DateTime"), 1_767_323_045u32.to_le_bytes());
        assert_eq!(
            binary(&at, "DateTime64(3, 'UTC')"),
            1_767_323_045_678i64.to_le_bytes()
        );
        assert_eq!(param(&at, "DateTime('America/New_York')"), "1767323045");
        assert_eq!(param(&at, "DateTime64(3)"), "1767323045.678");
        assert_eq!(param(&at, "DateTime64(0)"), "1767323045");
        assert_eq!(
            param(&vec![at], "Array(DateTime64(3))"),
            "['1767323045.678']"
        );
        let before = Utc.timestamp_millis_opt(-1500).unwrap();
        assert_eq!(param(&before, "DateTime64(3)"), "-2.500");
    }

    #[test]
    fn bind_as_accepts_borrowed_and_owned_shapes() {
        fn check<Target: ?Sized, V: BindAs<Target> + ?Sized>(_: &V) {}
        let owned = String::from("x");
        check::<String, _>(&owned);
        check::<String, _>(&&owned);
        check::<String, _>("literal");
        check::<Vec<String>, _>(&["a", "b"][..]);
        check::<Vec<String>, _>(&vec![owned.clone()]);
        check::<Option<u64>, _>(&Some(1u64));
        check::<Vec<(String, u8)>, _>(&BTreeMap::from([("k", 1u8)]));
        check::<(u8, String), _>(&(1u8, "x"));
        check::<[u8; 4], _>("abcd");
    }

    /// Both forms refuse a ClickHouse type the Rust type cannot be written
    /// as, and `accepts` agrees with them.
    fn refuses<T: Encode + ?Sized>(value: &T, text: &str) {
        let t = ty(text);
        assert!(
            !T::accepts(&t),
            "{} accepts {t}",
            std::any::type_name::<T>()
        );
        assert!(value.encode(&t, &mut Vec::new()).is_err(), "binary as {t}");
        assert!(
            value.encode_text(&t, &mut String::new(), false).is_err(),
            "text as {t}"
        );
    }

    #[test]
    fn every_scalar_refuses_a_type_it_is_not() {
        refuses(&1u8, "UInt16");
        refuses(&1u16, "UInt8");
        refuses(&1u32, "Int32");
        refuses(&1u64, "String");
        refuses(&1u128, "UInt64");
        refuses(&1i8, "UInt8");
        refuses(&1i16, "Enum8('a' = 1)");
        refuses(&1i32, "Int64");
        refuses(&1i64, "Int32");
        refuses(&1i128, "Decimal(38, 0)");
        refuses(&1f32, "Float64");
        refuses(&1f64, "Float32");
        refuses(&true, "UInt8");
        refuses("text", "UInt8");
        refuses(&String::from("text"), "Array(String)");
        refuses(&Decimal::new(1, 0), "Float64");
        refuses(&Decimal::new(1, 0), "Decimal(76, 0)");
        refuses(&Bytes(vec![1]), "UInt8");
        refuses(b"abcd", "FixedString(3)");
        refuses(&Ipv4Addr::LOCALHOST, "IPv6");
        refuses(&Ipv6Addr::LOCALHOST, "IPv4");
        refuses(&Some(1u8), "Nullable(String)");
        refuses(&vec![1u8], "Array(String)");
        refuses(&vec![1u8], "UInt8");
        refuses(&BTreeMap::from([(1u8, 1u8)]), "Array(UInt8)");
        refuses(&HashMap::from([(1u8, 1u8)]), "Map(String, UInt8)");
        refuses(&(1u8,), "Tuple(UInt8, UInt8)");
        refuses(&(1u8, 2u8), "Tuple(UInt8, String)");
        refuses(&(1u8, 2u8), "UInt8");
        refuses(&(1u8, 2u8, 3u8), "Tuple(UInt8, UInt8)");
        refuses(&(1u8, 2u8, 3u8), "String");
    }

    #[test]
    fn every_integer_width_writes_its_bytes_and_its_digits() {
        assert_eq!(binary(&7u8, "UInt8"), [7]);
        assert_eq!(binary(&7u32, "UInt32"), 7u32.to_le_bytes());
        assert_eq!(binary(&7u64, "UInt64"), 7u64.to_le_bytes());
        assert_eq!(binary(&7u128, "UInt128"), 7u128.to_le_bytes());
        assert_eq!(binary(&-7i8, "Int8"), [0xf9]);
        assert_eq!(binary(&-7i16, "Int16"), (-7i16).to_le_bytes());
        assert_eq!(binary(&-7i64, "Int64"), (-7i64).to_le_bytes());
        assert_eq!(binary(&-7i128, "Int128"), (-7i128).to_le_bytes());
        assert_eq!(binary(&1.5f32, "Float32"), 1.5f32.to_le_bytes());
        assert_eq!(binary(&1.5f64, "Float64"), 1.5f64.to_le_bytes());
        assert_eq!(param(&-7i8, "Int8"), "-7");
        assert_eq!(param(&-7i16, "Int16"), "-7");
        assert_eq!(param(&u128::MAX, "UInt128"), u128::MAX.to_string());
        assert_eq!(param(&1.5f32, "Float32"), "1.5");
        assert_eq!(param(&f64::INFINITY, "Float64"), "inf");
        assert_eq!(param(&false, "Bool"), "false");
    }

    #[test]
    fn enum16_is_written_by_name_or_number() {
        let enum16 = "Enum16('x' = 300, 'y' = -2)";
        assert_eq!(binary(&300i16, enum16), 300i16.to_le_bytes());
        assert_eq!(binary("y", enum16), (-2i16).to_le_bytes());
        assert_eq!(param(&300i16, enum16), "x");
        assert_eq!(param("y", enum16), "y");
        assert!(1i16.encode(&ty(enum16), &mut Vec::new()).is_err());
        assert!(
            1i16.encode_text(&ty(enum16), &mut String::new(), false)
                .is_err()
        );
        assert!("z".encode(&ty(enum16), &mut Vec::new()).is_err());
        assert!(
            "z".encode_text(&ty(enum16), &mut String::new(), false)
                .is_err()
        );
        // An Enum8 number with no name is refused in the text form too.
        assert!(
            9i8.encode_text(&ty("Enum8('a' = 1)"), &mut String::new(), false)
                .is_err()
        );
        assert!(
            "z".encode_text(&ty("Enum8('a' = 1)"), &mut String::new(), false)
                .is_err()
        );
    }

    #[test]
    fn a_reference_encodes_as_what_it_points_to() {
        let value = 7u64;
        let reference = &value;
        assert_eq!(binary(&reference, "UInt64"), 7u64.to_le_bytes());
        assert_eq!(param(&reference, "UInt64"), "7");
        assert!(<&u64>::accepts(&ty("UInt64")));

        // And a reference to a pair is still a map entry.
        let pair = ("k", 1u8);
        let mut out = String::new();
        <&(&str, u8) as Encode>::encode_text_pair(&&pair, &ty("String"), &ty("UInt8"), &mut out)
            .unwrap();
        assert_eq!(out, "'k':1");
    }

    #[test]
    fn only_a_pair_is_a_map_entry() {
        let err = 1u8
            .encode_text_pair(&ty("String"), &ty("UInt8"), &mut String::new())
            .unwrap_err();
        assert!(err.to_string().contains("not a key-value pair"), "{err}");
        // So a list of scalars cannot be written as a map.
        assert!(
            vec![1u8]
                .encode_text(&ty("Map(UInt8, UInt8)"), &mut String::new(), false)
                .is_err()
        );
    }

    #[test]
    fn owned_strings_and_control_characters() {
        let owned = String::from("a\rb\0c");
        assert_eq!(binary(&owned, "String"), b"\x05a\rb\0c");
        assert_eq!(param(&owned, "String"), "a\\rb\\0c");
        assert_eq!(param(&owned, "FixedString(8)"), "a\\rb\\0c");
        assert!(String::accepts(&ty("Identifier")));
        // An identifier is never quoted, so it cannot sit inside an array.
        assert!(
            "t".encode_text(&ty("Identifier"), &mut String::new(), true)
                .is_err()
        );
    }

    #[test]
    fn byte_strings_are_binary_in_rowbinary_and_text_only_as_parameters() {
        let binary_data = Bytes(vec![0xff, 0x00]);
        assert_eq!(binary(&binary_data, "String"), [2, 0xff, 0x00]);
        assert_eq!(binary(&binary_data, "FixedString(3)"), [0xff, 0x00, 0x00]);
        assert!(
            binary_data
                .encode(&ty("FixedString(1)"), &mut Vec::new())
                .is_err()
        );
        let err = binary_data
            .encode_text(&ty("String"), &mut String::new(), false)
            .unwrap_err();
        assert!(err.to_string().contains("RowBinary"), "{err}");
        assert_eq!(param(&Bytes(b"ok".to_vec()), "String"), "ok");
        assert!(
            Bytes(vec![1])
                .encode_text(&ty("UInt8"), &mut String::new(), false)
                .is_err()
        );

        assert_eq!(binary(b"abcd", "FixedString(4)"), b"abcd");
        assert_eq!(binary(b"abcd", "String"), b"\x04abcd");
        assert_eq!(param(b"abcd", "FixedString(4)"), "abcd");
    }

    #[test]
    fn options_write_through_to_a_plain_column_when_some() {
        assert_eq!(param(&Some("x"), "String"), "x");
        assert_eq!(param(&Some(7u8), "Nullable(UInt8)"), "7");
        assert!(
            None::<u8>
                .encode_text(&ty("UInt8"), &mut String::new(), false)
                .is_err()
        );
        let mut nested = String::new();
        None::<u8>
            .encode_text(&ty("Nullable(UInt8)"), &mut nested, true)
            .unwrap();
        assert_eq!(nested, "NULL");
    }

    #[test]
    fn hash_maps_and_owned_vectors_write_as_maps_and_arrays() {
        let map = HashMap::from([("k".to_string(), 1u8)]);
        assert_eq!(binary(&map, "Map(String, UInt8)"), b"\x01\x01k\x01");
        assert_eq!(param(&map, "Map(String, UInt8)"), "{'k':1}");
        assert_eq!(
            param(
                &BTreeMap::from([(1u8, "a"), (2u8, "b")]),
                "Map(UInt8, String)"
            ),
            "{1:'a', 2:'b'}"
        );
        assert_eq!(binary(&vec![1u8, 2], "Array(UInt8)"), [2, 1, 2]);
        assert_eq!(param(&vec![1u8, 2], "Array(UInt8)"), "[1, 2]");
        assert_eq!(
            param(&vec![("a", 1u8), ("b", 2u8)], "Map(String, UInt8)"),
            "{'a':1, 'b':2}"
        );
        assert_eq!(
            param(&vec![vec![1u8], vec![]], "Array(Array(UInt8))"),
            "[[1], []]"
        );
    }

    #[test]
    fn tuples_of_every_shape_write_in_order() {
        assert_eq!(binary(&(7u8,), "Tuple(UInt8)"), [7]);
        assert_eq!(param(&(7u8,), "Tuple(UInt8)"), "(7)");
        assert_eq!(
            binary(&(1u8, "x", 2u8), "Tuple(UInt8, String, UInt8)"),
            b"\x01\x01x\x02"
        );
        assert_eq!(
            param(&(1u8, 2u8, 3u8, 4u8), "Tuple(UInt8, UInt8, UInt8, UInt8)"),
            "(1, 2, 3, 4)"
        );
        assert_eq!(
            param(&(1u8, None::<u8>), "Tuple(a UInt8, b Nullable(UInt8))"),
            "(1, NULL)"
        );
    }

    #[test]
    fn addresses_as_parameters() {
        assert_eq!(param(&Ipv4Addr::new(1, 2, 3, 4), "IPv4"), "1.2.3.4");
        assert_eq!(param(&Ipv6Addr::LOCALHOST, "IPv6"), "::1");
        assert_eq!(binary(&Ipv6Addr::LOCALHOST, "IPv6").len(), 16);
    }

    #[cfg(feature = "uuid")]
    #[test]
    fn a_uuid_refuses_what_it_is_not() {
        refuses(&uuid::Uuid::nil(), "String");
        assert_eq!(
            param(&vec![uuid::Uuid::nil()], "Array(UUID)"),
            "['00000000-0000-0000-0000-000000000000']"
        );
    }

    #[cfg(feature = "chrono")]
    #[test]
    fn dates_and_times_refuse_what_they_are_not_and_what_is_out_of_range() {
        use chrono::{NaiveDate, TimeZone, Utc};

        let date = NaiveDate::from_ymd_opt(2026, 1, 2).unwrap();
        let at = Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap();
        refuses(&date, "DateTime");
        refuses(&at, "Date");
        refuses(&at.naive_utc(), "String");

        // A DateTime is an unsigned 32-bit count of seconds.
        let before_epoch = Utc.timestamp_opt(-1, 0).unwrap();
        assert!(
            before_epoch
                .encode(&ty("DateTime"), &mut Vec::new())
                .is_err()
        );
        assert_eq!(
            binary(&before_epoch, "DateTime64(3)"),
            (-1000i64).to_le_bytes()
        );
        // The far future overflows nanosecond ticks.
        let far = Utc.with_ymd_and_hms(9999, 1, 1, 0, 0, 0).unwrap();
        assert!(far.encode(&ty("DateTime64(9)"), &mut Vec::new()).is_err());
        assert!(
            far.encode_text(&ty("DateTime64(9)"), &mut String::new(), false)
                .is_err()
        );
        // A date far outside Date32 is refused rather than wrapped.
        let ancient = NaiveDate::from_ymd_opt(-200_000, 1, 1).unwrap();
        assert!(
            ancient
                .encode(&ty("Date").clone(), &mut Vec::new())
                .is_err()
        );

        // A naive time is taken as UTC, in both forms.
        assert_eq!(binary(&at.naive_utc(), "DateTime"), binary(&at, "DateTime"));
        assert_eq!(param(&at.naive_utc(), "DateTime"), param(&at, "DateTime"));
        assert_eq!(param(&date, "Date32"), "2026-01-02");
    }

    #[test]
    fn a_decimal_parameter_is_refused_when_rescaling_loses_digits() {
        assert!(
            Decimal::new(15001, 4)
                .encode_text(&ty("Decimal(9, 2)"), &mut String::new(), false)
                .is_err()
        );
        assert!(
            Decimal::new(1, 0)
                .encode_text(&ty("String"), &mut String::new(), false)
                .is_err()
        );
    }

    #[test]
    fn an_empty_collection_is_still_refused_for_the_wrong_element_type() {
        refuses(&Vec::<u8>::new(), "Array(String)");
        refuses(&BTreeMap::<u8, u8>::new(), "Map(String, UInt8)");
        refuses(&Vec::<(u8, u8)>::new(), "Map(String, UInt8)");
    }

    #[test]
    fn a_parameter_too_long_for_its_fixed_string_is_refused_before_it_is_sent() {
        let too_long = |value: &dyn Fn(&mut String) -> Result<()>| {
            let err = value(&mut String::new()).unwrap_err();
            assert!(err.to_string().contains("FixedString(4)"), "{err}");
        };
        let t = ty("FixedString(4)");
        too_long(&|out| "abcde".encode_text(&t, out, false));
        too_long(&|out| Bytes(b"abcde".to_vec()).encode_text(&t, out, false));
        assert_eq!(param("abcd", "FixedString(4)"), "abcd");
    }
}
