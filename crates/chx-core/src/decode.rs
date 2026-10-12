//! Reading values out of RowBinary.
//!
//! RowBinary has no per-row or per-value framing: a value's length is known
//! only from its type. So every decoder takes the column's [`Type`], and a
//! column is skipped the same way, by walking its type. Both come from the
//! `RowBinaryWithNamesAndTypes` header, which is why that is the only format
//! chx reads.

use std::any::type_name;
use std::collections::{BTreeMap, HashMap};
use std::hash::Hash;
use std::net::{Ipv4Addr, Ipv6Addr};

use crate::error::{Error, Result};
use crate::types::{Bytes, Decimal, Type};

/// A cursor over RowBinary bytes.
#[derive(Debug, Clone)]
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    /// A cursor at the start of `buf`.
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// Bytes read so far.
    pub fn position(&self) -> usize {
        self.pos
    }

    /// Bytes not yet read.
    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    /// Whether every byte has been read.
    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    /// The next `n` bytes, or an error if fewer are left.
    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).filter(|end| *end <= self.buf.len());
        match end {
            Some(end) => {
                let slice = &self.buf[self.pos..end];
                self.pos = end;
                Ok(slice)
            }
            None => Err(Error::Decode(format!(
                "body ended early: wanted {n} bytes at offset {}, {} left",
                self.pos,
                self.remaining()
            ))),
        }
    }

    /// The next byte.
    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.bytes(1)?[0])
    }

    /// The next `N` bytes as an array, which is what `from_le_bytes` takes.
    pub fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.bytes(N)?);
        Ok(out)
    }

    /// Unsigned LEB128, as every length in RowBinary is.
    pub fn varint(&mut self) -> Result<u64> {
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let byte = self.u8()?;
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(Error::Decode("varint longer than 64 bits".to_string()))
    }

    /// A varint used as a length. Refused if it claims more than the bytes
    /// left, so a corrupt length cannot ask for a huge allocation.
    pub fn length(&mut self) -> Result<usize> {
        let length = self.varint()?;
        usize::try_from(length)
            .ok()
            .filter(|length| *length <= self.remaining())
            .ok_or_else(|| {
                Error::Decode(format!(
                    "length {length} at offset {} exceeds the {} bytes left",
                    self.pos,
                    self.remaining()
                ))
            })
    }

    /// A length-prefixed byte string, as a `String` column is written.
    pub fn string_bytes(&mut self) -> Result<&'a [u8]> {
        let length = self.length()?;
        self.bytes(length)
    }

    /// A length-prefixed string that must be UTF-8.
    pub fn string(&mut self) -> Result<String> {
        text(self.string_bytes()?)
    }
}

fn text(bytes: &[u8]) -> Result<String> {
    String::from_utf8(bytes.to_vec())
        .map_err(|_| Error::Decode("string is not UTF-8; read it as chx::types::Bytes".to_string()))
}

/// A Rust type that can be read from a RowBinary column.
///
/// `accepts` says which ClickHouse types this Rust type reads, and is what a
/// schema check consults. `decode` is only called with a type `accepts`
/// said yes to, and may still fail on the value, such as an enum number with
/// no name.
///
/// # Implementing it
///
/// A newtype over a `String` column that must hold a known symbol:
///
/// ```
/// use chx::decode::{Decode, Reader};
/// use chx::{Error, Result, Type};
///
/// struct Symbol(String);
///
/// impl Decode for Symbol {
///     fn accepts(ty: &Type) -> bool {
///         String::accepts(ty)
///     }
///
///     fn decode(reader: &mut Reader<'_>, ty: &Type) -> Result<Self> {
///         let text = String::decode(reader, ty)?;
///         if text.is_empty() {
///             return Err(Error::Decode("empty symbol".into()));
///         }
///         Ok(Symbol(text))
///     }
/// }
///
/// // "AAPL" as a String column: a length byte, then the bytes.
/// let mut reader = Reader::new(b"\x04AAPL");
/// let symbol = Symbol::decode(&mut reader, &Type::String).unwrap();
/// assert_eq!(symbol.0, "AAPL");
/// ```
pub trait Decode: Sized {
    /// Whether a column of type `ty` can be read as `Self`. Look through
    /// `LowCardinality` with [`Type::unwrapped`].
    fn accepts(ty: &Type) -> bool;

    /// Reads one value of type `ty` from `reader`, consuming exactly its
    /// bytes.
    fn decode(reader: &mut Reader<'_>, ty: &Type) -> Result<Self>;
}

fn mismatch<T>(ty: &Type) -> Error {
    Error::Decode(format!("{ty} cannot be read as {}", type_name::<T>()))
}

macro_rules! numbers {
    ($($rust:ty => $($variant:pat_param)|+,)*) => {$(
        impl Decode for $rust {
            fn accepts(ty: &Type) -> bool {
                matches!(ty.unwrapped(), $($variant)|+)
            }

            fn decode(reader: &mut Reader<'_>, ty: &Type) -> Result<Self> {
                match ty.unwrapped() {
                    $($variant)|+ => Ok(<$rust>::from_le_bytes(reader.array()?)),
                    other => Err(mismatch::<$rust>(other)),
                }
            }
        }
    )*};
}

numbers! {
    u8 => Type::UInt8,
    u16 => Type::UInt16,
    u32 => Type::UInt32,
    u64 => Type::UInt64,
    u128 => Type::UInt128,
    i8 => Type::Int8 | Type::Enum8(_),
    i16 => Type::Int16 | Type::Enum16(_),
    i32 => Type::Int32,
    i64 => Type::Int64,
    i128 => Type::Int128,
    f32 => Type::Float32,
    f64 => Type::Float64,
}

impl Decode for bool {
    fn accepts(ty: &Type) -> bool {
        matches!(ty.unwrapped(), Type::Bool)
    }

    fn decode(reader: &mut Reader<'_>, ty: &Type) -> Result<Self> {
        match ty.unwrapped() {
            Type::Bool => Ok(reader.u8()? != 0),
            other => Err(mismatch::<bool>(other)),
        }
    }
}

impl Decode for Decimal {
    fn accepts(ty: &Type) -> bool {
        matches!(ty.unwrapped(), Type::Decimal { precision, .. } if *precision <= 38)
    }

    fn decode(reader: &mut Reader<'_>, ty: &Type) -> Result<Self> {
        match ty.unwrapped() {
            Type::Decimal { precision, scale } => {
                let mantissa = match precision {
                    0..=9 => i128::from(i32::from_le_bytes(reader.array()?)),
                    10..=18 => i128::from(i64::from_le_bytes(reader.array()?)),
                    19..=38 => i128::from_le_bytes(reader.array()?),
                    _ => return Err(mismatch::<Decimal>(ty)),
                };
                Ok(Decimal::new(mantissa, *scale))
            }
            other => Err(mismatch::<Decimal>(other)),
        }
    }
}

/// Text, including the name of an enum value. A `FixedString` loses its zero
/// padding, since that is what the padding is for.
impl Decode for String {
    fn accepts(ty: &Type) -> bool {
        matches!(
            ty.unwrapped(),
            Type::String | Type::FixedString(_) | Type::Enum8(_) | Type::Enum16(_)
        )
    }

    fn decode(reader: &mut Reader<'_>, ty: &Type) -> Result<Self> {
        match ty.unwrapped() {
            Type::String => reader.string(),
            Type::FixedString(n) => {
                let bytes = reader.bytes(*n)?;
                let end = bytes
                    .iter()
                    .rposition(|byte| *byte != 0)
                    .map_or(0, |last| last + 1);
                text(&bytes[..end])
            }
            Type::Enum8(variants) => {
                let value = i8::from_le_bytes(reader.array()?);
                enum_name(
                    variants.iter().map(|(name, v)| (name, i64::from(*v))),
                    value.into(),
                    ty,
                )
            }
            Type::Enum16(variants) => {
                let value = i16::from_le_bytes(reader.array()?);
                enum_name(
                    variants.iter().map(|(name, v)| (name, i64::from(*v))),
                    value.into(),
                    ty,
                )
            }
            other => Err(mismatch::<String>(other)),
        }
    }
}

fn enum_name<'a>(
    variants: impl Iterator<Item = (&'a String, i64)>,
    value: i64,
    ty: &Type,
) -> Result<String> {
    variants
        .into_iter()
        .find(|(_, v)| *v == value)
        .map(|(name, _)| name.clone())
        .ok_or_else(|| Error::Decode(format!("{ty} has no variant numbered {value}")))
}

impl Decode for Bytes {
    fn accepts(ty: &Type) -> bool {
        matches!(ty.unwrapped(), Type::String | Type::FixedString(_))
    }

    fn decode(reader: &mut Reader<'_>, ty: &Type) -> Result<Self> {
        match ty.unwrapped() {
            Type::String => Ok(Bytes(reader.string_bytes()?.to_vec())),
            Type::FixedString(n) => Ok(Bytes(reader.bytes(*n)?.to_vec())),
            other => Err(mismatch::<Bytes>(other)),
        }
    }
}

impl<const N: usize> Decode for [u8; N] {
    fn accepts(ty: &Type) -> bool {
        matches!(ty.unwrapped(), Type::FixedString(n) if *n == N)
    }

    fn decode(reader: &mut Reader<'_>, ty: &Type) -> Result<Self> {
        match ty.unwrapped() {
            Type::FixedString(n) if *n == N => reader.array(),
            other => Err(mismatch::<[u8; N]>(other)),
        }
    }
}

/// `Nullable(T)`, and also a plain `T`, which is then always `Some`.
impl<T: Decode> Decode for Option<T> {
    fn accepts(ty: &Type) -> bool {
        match ty.unwrapped() {
            Type::Nullable(inner) => T::accepts(inner) || **inner == Type::Nothing,
            other => T::accepts(other),
        }
    }

    fn decode(reader: &mut Reader<'_>, ty: &Type) -> Result<Self> {
        match ty.unwrapped() {
            Type::Nullable(inner) => {
                if reader.u8()? != 0 {
                    return Ok(None);
                }
                if **inner == Type::Nothing {
                    return Err(Error::Decode(
                        "a Nullable(Nothing) value that is not NULL".into(),
                    ));
                }
                T::decode(reader, inner).map(Some)
            }
            other => T::decode(reader, other).map(Some),
        }
    }
}

/// `Array(T)`, and `Map(K, V)` read as its wire form, an array of pairs.
impl<T: Decode> Decode for Vec<T> {
    fn accepts(ty: &Type) -> bool {
        match ty.unwrapped() {
            Type::Array(inner) => T::accepts(inner),
            Type::Map(key, value) => T::accepts(&pair_type(key, value)),
            _ => false,
        }
    }

    fn decode(reader: &mut Reader<'_>, ty: &Type) -> Result<Self> {
        let element = match ty.unwrapped() {
            Type::Array(inner) => (**inner).clone(),
            Type::Map(key, value) => pair_type(key, value),
            other => return Err(mismatch::<Vec<T>>(other)),
        };
        // Checked here and not only by the caller, because an empty array
        // would otherwise read as an array of anything.
        if !T::accepts(&element) {
            return Err(mismatch::<Vec<T>>(ty));
        }
        let length = reader.length()?;
        let mut out = Vec::with_capacity(length);
        for _ in 0..length {
            out.push(T::decode(reader, &element)?);
        }
        Ok(out)
    }
}

/// The element type a `Map(K, V)` is serialised as.
pub(crate) fn pair_type(key: &Type, value: &Type) -> Type {
    Type::Tuple(vec![
        crate::types::TupleElement {
            name: None,
            ty: key.clone(),
        },
        crate::types::TupleElement {
            name: None,
            ty: value.clone(),
        },
    ])
}

macro_rules! maps {
    ($($map:ident $(+ $bound:ident)*;)*) => {$(
        impl<K: Decode $(+ $bound)*, V: Decode> Decode for $map<K, V> {
            fn accepts(ty: &Type) -> bool {
                matches!(ty.unwrapped(), Type::Map(key, value) if K::accepts(key) && V::accepts(value))
            }

            fn decode(reader: &mut Reader<'_>, ty: &Type) -> Result<Self> {
                match ty.unwrapped() {
                    Type::Map(key, value) if K::accepts(key) && V::accepts(value) => {
                        let length = reader.length()?;
                        let mut out = $map::new();
                        for _ in 0..length {
                            out.insert(K::decode(reader, key)?, V::decode(reader, value)?);
                        }
                        Ok(out)
                    }
                    other => Err(mismatch::<$map<K, V>>(other)),
                }
            }
        }
    )*};
}

maps! {
    BTreeMap + Ord;
    HashMap + Eq + Hash;
}

macro_rules! tuples {
    ($(($($name:ident $index:tt),+);)*) => {$(
        impl<$($name: Decode),+> Decode for ($($name,)+) {
            fn accepts(ty: &Type) -> bool {
                match ty.unwrapped() {
                    Type::Tuple(elements) => {
                        elements.len() == tuples!(@count $($name)+)
                            $(&& $name::accepts(&elements[$index].ty))+
                    }
                    _ => false,
                }
            }

            fn decode(reader: &mut Reader<'_>, ty: &Type) -> Result<Self> {
                match ty.unwrapped() {
                    Type::Tuple(elements) if elements.len() == tuples!(@count $($name)+) => {
                        Ok(($($name::decode(reader, &elements[$index].ty)?,)+))
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
    (A 0, B 1);
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

impl Decode for Ipv4Addr {
    fn accepts(ty: &Type) -> bool {
        matches!(ty.unwrapped(), Type::IPv4)
    }

    fn decode(reader: &mut Reader<'_>, ty: &Type) -> Result<Self> {
        match ty.unwrapped() {
            // Little-endian, like every number in RowBinary: 1.2.3.4 is
            // 0x01020304 on the wire as 04 03 02 01.
            Type::IPv4 => Ok(Ipv4Addr::from(u32::from_le_bytes(reader.array()?))),
            other => Err(mismatch::<Ipv4Addr>(other)),
        }
    }
}

impl Decode for Ipv6Addr {
    fn accepts(ty: &Type) -> bool {
        matches!(ty.unwrapped(), Type::IPv6)
    }

    fn decode(reader: &mut Reader<'_>, ty: &Type) -> Result<Self> {
        match ty.unwrapped() {
            Type::IPv6 => Ok(Ipv6Addr::from(reader.array::<16>()?)),
            other => Err(mismatch::<Ipv6Addr>(other)),
        }
    }
}

#[cfg(feature = "uuid")]
impl Decode for uuid::Uuid {
    fn accepts(ty: &Type) -> bool {
        matches!(ty.unwrapped(), Type::Uuid)
    }

    fn decode(reader: &mut Reader<'_>, ty: &Type) -> Result<Self> {
        match ty.unwrapped() {
            // Two little-endian halves, most significant first. Not the
            // RFC byte order, which is why this is not `from_bytes`.
            Type::Uuid => {
                let high = u64::from_le_bytes(reader.array()?);
                let low = u64::from_le_bytes(reader.array()?);
                Ok(uuid::Uuid::from_u64_pair(high, low))
            }
            other => Err(mismatch::<uuid::Uuid>(other)),
        }
    }
}

#[cfg(feature = "chrono")]
mod chrono_impls {
    use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};

    use super::*;

    /// Days from 0001-01-01 to 1970-01-01, which is how chrono counts.
    pub(crate) const EPOCH_DAYS_FROM_CE: i32 = 719_163;

    pub(crate) fn date_from_days(days: i32) -> Result<NaiveDate> {
        days.checked_add(EPOCH_DAYS_FROM_CE)
            .and_then(NaiveDate::from_num_days_from_ce_opt)
            .ok_or_else(|| Error::Decode(format!("day {days} is out of range")))
    }

    pub(crate) fn datetime_from_ticks(ticks: i64, precision: u8) -> Result<DateTime<Utc>> {
        let unit = 10i64.pow(u32::from(precision));
        let seconds = ticks.div_euclid(unit);
        let fraction = ticks.rem_euclid(unit);
        let nanos = fraction * 10i64.pow(9 - u32::from(precision));
        DateTime::from_timestamp(seconds, nanos as u32).ok_or_else(|| {
            Error::Decode(format!(
                "timestamp {ticks} at precision {precision} is out of range"
            ))
        })
    }

    impl Decode for NaiveDate {
        fn accepts(ty: &Type) -> bool {
            matches!(ty.unwrapped(), Type::Date | Type::Date32)
        }

        fn decode(reader: &mut Reader<'_>, ty: &Type) -> Result<Self> {
            match ty.unwrapped() {
                Type::Date => date_from_days(i32::from(u16::from_le_bytes(reader.array()?))),
                Type::Date32 => date_from_days(i32::from_le_bytes(reader.array()?)),
                other => Err(mismatch::<NaiveDate>(other)),
            }
        }
    }

    /// A ClickHouse `DateTime` is seconds since the epoch whatever its
    /// display timezone, so it is always a UTC instant here.
    impl Decode for DateTime<Utc> {
        fn accepts(ty: &Type) -> bool {
            matches!(
                ty.unwrapped(),
                Type::DateTime { .. } | Type::DateTime64 { .. }
            )
        }

        fn decode(reader: &mut Reader<'_>, ty: &Type) -> Result<Self> {
            match ty.unwrapped() {
                Type::DateTime { .. } => {
                    datetime_from_ticks(i64::from(u32::from_le_bytes(reader.array()?)), 0)
                }
                Type::DateTime64 { precision, .. } => {
                    datetime_from_ticks(i64::from_le_bytes(reader.array()?), *precision)
                }
                other => Err(mismatch::<DateTime<Utc>>(other)),
            }
        }
    }

    impl Decode for NaiveDateTime {
        fn accepts(ty: &Type) -> bool {
            <DateTime<Utc>>::accepts(ty)
        }

        fn decode(reader: &mut Reader<'_>, ty: &Type) -> Result<Self> {
            <DateTime<Utc>>::decode(reader, ty).map(|value| value.naive_utc())
        }
    }
}

#[cfg(feature = "chrono")]
pub(crate) use chrono_impls::EPOCH_DAYS_FROM_CE;

/// Advances past one value of `ty` without building it. This is how a row is
/// cut into columns before any of them is read.
pub fn skip(reader: &mut Reader<'_>, ty: &Type) -> Result<()> {
    let fixed = match ty.unwrapped() {
        // Nothing only occurs inside Nullable, where the flag byte is all
        // there is.
        Type::Nothing => Some(0),
        Type::UInt8 | Type::Int8 | Type::Bool | Type::Enum8(_) => Some(1),
        Type::UInt16 | Type::Int16 | Type::Date | Type::Enum16(_) => Some(2),
        Type::UInt32
        | Type::Int32
        | Type::Float32
        | Type::Date32
        | Type::DateTime { .. }
        | Type::IPv4 => Some(4),
        Type::UInt64 | Type::Int64 | Type::Float64 | Type::DateTime64 { .. } => Some(8),
        Type::UInt128 | Type::Int128 | Type::Uuid | Type::IPv6 => Some(16),
        Type::UInt256 | Type::Int256 => Some(32),
        Type::Decimal { precision, .. } => Some(match precision {
            0..=9 => 4,
            10..=18 => 8,
            19..=38 => 16,
            _ => 32,
        }),
        Type::FixedString(n) => Some(*n),
        Type::String => {
            reader.string_bytes()?;
            None
        }
        Type::Nullable(inner) => {
            if reader.u8()? == 0 {
                skip(reader, inner)?;
            }
            None
        }
        Type::Array(inner) => {
            for _ in 0..reader.length()? {
                skip(reader, inner)?;
            }
            None
        }
        Type::Map(key, value) => {
            for _ in 0..reader.length()? {
                skip(reader, key)?;
                skip(reader, value)?;
            }
            None
        }
        Type::Tuple(elements) => {
            for element in elements {
                skip(reader, &element.ty)?;
            }
            None
        }
        Type::LowCardinality(_) | Type::SimpleAggregateFunction { .. } => {
            unreachable!("unwrapped")
        }
        Type::Identifier | Type::Other(_) => {
            return Err(Error::Decode(format!(
                "cannot read a column of type {ty}; cast it in the query, for example with toString()"
            )));
        }
    };
    if let Some(n) = fixed {
        reader.bytes(n)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ty(text: &str) -> Type {
        Type::parse(text).unwrap()
    }

    fn decode<T: Decode>(bytes: &[u8], text: &str) -> T {
        let t = ty(text);
        assert!(T::accepts(&t), "{} should accept {t}", type_name::<T>());
        let mut reader = Reader::new(bytes);
        let value = T::decode(&mut reader, &t).unwrap_or_else(|err| panic!("{err}"));
        assert!(reader.is_empty(), "{} bytes left over", reader.remaining());
        value
    }

    #[test]
    fn varints_are_leb128() {
        assert_eq!(Reader::new(&[0x7f]).varint().unwrap(), 127);
        assert_eq!(Reader::new(&[0x80, 0x01]).varint().unwrap(), 128);
        assert_eq!(Reader::new(&[0xe5, 0x8e, 0x26]).varint().unwrap(), 624_485);
        assert!(Reader::new(&[0x80]).varint().is_err());
    }

    #[test]
    fn numbers_are_little_endian() {
        assert_eq!(decode::<u16>(&[0x34, 0x12], "UInt16"), 0x1234);
        assert_eq!(decode::<i32>(&[0xff, 0xff, 0xff, 0xff], "Int32"), -1);
        assert_eq!(decode::<f64>(&1.5f64.to_le_bytes(), "Float64"), 1.5);
        assert_eq!(decode::<i128>(&(-7i128).to_le_bytes(), "Int128"), -7);
        assert!(decode::<bool>(&[1], "Bool"));
    }

    #[test]
    fn strings_are_length_prefixed_and_fixed_strings_lose_padding() {
        assert_eq!(decode::<String>(b"\x02ab", "String"), "ab");
        assert_eq!(decode::<String>(b"ab\0\0", "FixedString(4)"), "ab");
        assert_eq!(decode::<[u8; 4]>(b"ab\0\0", "FixedString(4)"), *b"ab\0\0");
        assert_eq!(
            decode::<Bytes>(b"\x02\xff\x00", "String"),
            Bytes(vec![0xff, 0])
        );
        assert!(!<[u8; 3]>::accepts(&ty("FixedString(4)")));
        let err = String::decode(&mut Reader::new(b"\x01\xff"), &ty("String")).unwrap_err();
        assert!(err.to_string().contains("Bytes"), "{err}");
    }

    #[test]
    fn enums_read_as_their_name_or_their_number() {
        let enum8 = "Enum8('a' = 1, 'b' = 2)";
        assert_eq!(decode::<String>(&[2], enum8), "b");
        assert_eq!(decode::<i8>(&[2], enum8), 2);
        assert!(String::decode(&mut Reader::new(&[3]), &ty(enum8)).is_err());
        assert_eq!(decode::<String>(&[0x2c, 0x01], "Enum16('x' = 300)"), "x");
    }

    #[test]
    fn decimals_carry_the_column_scale() {
        assert_eq!(
            decode::<Decimal>(&15000i64.to_le_bytes(), "Decimal(18, 4)"),
            Decimal::new(15000, 4)
        );
        assert_eq!(
            decode::<Decimal>(&(-5i32).to_le_bytes(), "Decimal(9, 2)"),
            Decimal::new(-5, 2)
        );
        assert_eq!(
            decode::<Decimal>(&1i128.to_le_bytes(), "Decimal(38, 18)"),
            Decimal::new(1, 18)
        );
        assert!(!Decimal::accepts(&ty("Decimal(76, 0)")));
    }

    #[test]
    fn nullable_is_a_flag_byte() {
        assert_eq!(decode::<Option<u8>>(&[0, 7], "Nullable(UInt8)"), Some(7));
        assert_eq!(decode::<Option<u8>>(&[1], "Nullable(UInt8)"), None);
        assert_eq!(decode::<Option<u8>>(&[7], "UInt8"), Some(7));
        assert_eq!(decode::<Option<u8>>(&[1], "Nullable(Nothing)"), None);
    }

    #[test]
    fn arrays_maps_and_tuples_nest() {
        assert_eq!(
            decode::<Vec<Option<u8>>>(&[2, 0, 1, 1], "Array(Nullable(UInt8))"),
            vec![Some(1), None]
        );
        assert_eq!(
            decode::<Vec<(String, u8)>>(b"\x01\x01k\x01", "Map(String, UInt8)"),
            vec![("k".to_string(), 1)]
        );
        assert_eq!(
            decode::<BTreeMap<String, u8>>(b"\x01\x01k\x01", "Map(String, UInt8)"),
            BTreeMap::from([("k".to_string(), 1)])
        );
        assert_eq!(
            decode::<(u8, String)>(b"\x01\x01x", "Tuple(UInt8, String)"),
            (1, "x".to_string())
        );
        assert_eq!(
            decode::<(u8, String)>(b"\x01\x01x", "Tuple(a UInt8, b String)"),
            (1, "x".to_string())
        );
        assert!(!<(u8,)>::accepts(&ty("Tuple(UInt8, String)")));
    }

    #[test]
    fn low_cardinality_is_read_as_its_inner_type() {
        assert_eq!(decode::<String>(b"\x02ab", "LowCardinality(String)"), "ab");
        assert_eq!(
            decode::<Option<String>>(&[1], "LowCardinality(Nullable(String))"),
            None
        );
        assert_eq!(
            decode::<u64>(&3u64.to_le_bytes(), "SimpleAggregateFunction(sum, UInt64)"),
            3
        );
    }

    #[test]
    fn addresses() {
        assert_eq!(
            decode::<Ipv4Addr>(&[4, 3, 2, 1], "IPv4"),
            Ipv4Addr::new(1, 2, 3, 4)
        );
        let mut v6 = [0u8; 16];
        v6[15] = 1;
        assert_eq!(decode::<Ipv6Addr>(&v6, "IPv6"), Ipv6Addr::LOCALHOST);
    }

    #[cfg(feature = "uuid")]
    #[test]
    fn uuids_are_two_little_endian_halves() {
        // Captured from the server for 01234567-89ab-cdef-0123-456789abcdef.
        let wire = [
            0xef, 0xcd, 0xab, 0x89, 0x67, 0x45, 0x23, 0x01, 0xef, 0xcd, 0xab, 0x89, 0x67, 0x45,
            0x23, 0x01,
        ];
        assert_eq!(
            decode::<uuid::Uuid>(&wire, "UUID").to_string(),
            "01234567-89ab-cdef-0123-456789abcdef"
        );
    }

    #[cfg(feature = "chrono")]
    #[test]
    fn dates_and_times_count_from_the_epoch() {
        use chrono::{NaiveDate, TimeZone, Utc};

        assert_eq!(
            decode::<NaiveDate>(&20455u16.to_le_bytes(), "Date"),
            NaiveDate::from_ymd_opt(2026, 1, 2).unwrap()
        );
        assert_eq!(
            decode::<NaiveDate>(&(-3653i32).to_le_bytes(), "Date32"),
            NaiveDate::from_ymd_opt(1960, 1, 1).unwrap()
        );
        assert_eq!(
            decode::<chrono::DateTime<Utc>>(&1_767_323_045u32.to_le_bytes(), "DateTime('UTC')"),
            Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap()
        );
        let dt64 = decode::<chrono::DateTime<Utc>>(
            &1_767_323_045_678i64.to_le_bytes(),
            "DateTime64(3, 'UTC')",
        );
        assert_eq!(dt64.timestamp_millis(), 1_767_323_045_678);
        // Negative ticks: before the epoch, the fraction still counts upward.
        let before = decode::<chrono::DateTime<Utc>>(&(-1i64).to_le_bytes(), "DateTime64(3)");
        assert_eq!(before.timestamp_millis(), -1);
    }

    #[test]
    fn skip_walks_every_type() {
        let cases: &[(&str, &[u8])] = &[
            ("UInt8", &[1]),
            ("Decimal(38, 18)", &[0; 16]),
            ("String", b"\x02ab"),
            ("FixedString(3)", b"abc"),
            ("Nullable(UInt16)", &[0, 1, 2]),
            ("Nullable(UInt16)", &[1]),
            ("Array(String)", b"\x02\x01a\x01b"),
            ("Map(String, UInt8)", b"\x01\x01k\x01"),
            ("Tuple(UInt8, Array(UInt8))", &[1, 2, 3, 4]),
            ("LowCardinality(String)", b"\x01a"),
            ("Nullable(Nothing)", &[1]),
        ];
        for (text, bytes) in cases {
            let mut reader = Reader::new(bytes);
            skip(&mut reader, &ty(text)).unwrap_or_else(|err| panic!("{text}: {err}"));
            assert!(
                reader.is_empty(),
                "{text}: {} bytes left",
                reader.remaining()
            );
        }
        let err = skip(&mut Reader::new(&[0; 8]), &ty("Dynamic")).unwrap_err();
        assert!(err.to_string().contains("toString"), "{err}");
    }

    #[test]
    fn a_short_body_is_an_error_not_a_panic() {
        assert!(u64::decode(&mut Reader::new(&[1, 2]), &ty("UInt64")).is_err());
        assert!(String::decode(&mut Reader::new(&[5, b'a']), &ty("String")).is_err());
        let mut reader = Reader::new(&[0xff, 0xff, 0xff, 0xff, 0x0f]);
        assert!(reader.length().is_err());
    }

    /// `accepts` says no, and `decode` on that type is an error rather than
    /// a misread.
    fn refuses<T: Decode>(text: &str) {
        let t = ty(text);
        assert!(!T::accepts(&t), "{} accepts {t}", type_name::<T>());
        assert!(T::decode(&mut Reader::new(&[0; 64]), &t).is_err(), "{t}");
    }

    #[test]
    fn every_type_refuses_a_column_it_cannot_read() {
        refuses::<u8>("UInt16");
        refuses::<u64>("Int64");
        refuses::<i8>("UInt8");
        refuses::<i16>("Enum8('a' = 1)");
        refuses::<f32>("Float64");
        refuses::<bool>("UInt8");
        refuses::<Decimal>("Float64");
        refuses::<Decimal>("Decimal(76, 2)");
        refuses::<String>("UInt8");
        refuses::<Bytes>("UInt8");
        refuses::<[u8; 4]>("FixedString(3)");
        refuses::<[u8; 4]>("String");
        refuses::<Option<u8>>("Nullable(String)");
        refuses::<Vec<u8>>("UInt8");
        refuses::<Vec<u8>>("Array(String)");
        refuses::<BTreeMap<String, u8>>("Array(String)");
        refuses::<HashMap<String, u8>>("Map(UInt8, UInt8)");
        refuses::<(u8, u8)>("Tuple(UInt8)");
        refuses::<(u8, u8)>("UInt8");
        refuses::<Ipv4Addr>("IPv6");
        refuses::<Ipv6Addr>("IPv4");
    }

    #[test]
    fn every_integer_width_reads_its_bytes() {
        assert_eq!(decode::<u8>(&[7], "UInt8"), 7);
        assert_eq!(decode::<u32>(&7u32.to_le_bytes(), "UInt32"), 7);
        assert_eq!(decode::<u64>(&7u64.to_le_bytes(), "UInt64"), 7);
        assert_eq!(decode::<u128>(&7u128.to_le_bytes(), "UInt128"), 7);
        assert_eq!(decode::<i8>(&[0xf9], "Int8"), -7);
        assert_eq!(decode::<i16>(&(-7i16).to_le_bytes(), "Int16"), -7);
        assert_eq!(decode::<i64>(&(-7i64).to_le_bytes(), "Int64"), -7);
        assert_eq!(decode::<f32>(&1.5f32.to_le_bytes(), "Float32"), 1.5);
        assert_eq!(
            decode::<i16>(&300i16.to_le_bytes(), "Enum16('x' = 300)"),
            300
        );
        assert!(!decode::<bool>(&[0], "Bool"));
    }

    #[test]
    fn an_enum16_number_with_no_name_is_an_error() {
        let err = String::decode(&mut Reader::new(&[1, 0]), &ty("Enum16('x' = 300)")).unwrap_err();
        assert!(err.to_string().contains("no variant numbered 1"), "{err}");
    }

    #[test]
    fn fixed_strings_read_as_bytes_keep_their_padding() {
        assert_eq!(
            decode::<Bytes>(b"ab\0\0", "FixedString(4)"),
            Bytes(b"ab\0\0".to_vec())
        );
        // All padding is an empty string, not an error.
        assert_eq!(decode::<String>(b"\0\0", "FixedString(2)"), "");
    }

    #[test]
    fn a_nullable_nothing_that_claims_a_value_is_an_error() {
        let err =
            Option::<u8>::decode(&mut Reader::new(&[0]), &ty("Nullable(Nothing)")).unwrap_err();
        assert!(err.to_string().contains("Nullable(Nothing)"), "{err}");
    }

    #[test]
    fn hash_maps_and_longer_tuples_read() {
        assert_eq!(
            decode::<HashMap<String, u8>>(b"\x01\x01k\x01", "Map(String, UInt8)"),
            HashMap::from([("k".to_string(), 1)])
        );
        assert_eq!(decode::<(u8,)>(&[7], "Tuple(UInt8)"), (7,));
        assert_eq!(
            decode::<(u8, u8, u8, u8)>(&[1, 2, 3, 4], "Tuple(UInt8, UInt8, UInt8, UInt8)"),
            (1, 2, 3, 4)
        );
    }

    #[cfg(feature = "uuid")]
    #[test]
    fn a_uuid_refuses_what_it_is_not() {
        refuses::<uuid::Uuid>("String");
    }

    #[cfg(feature = "chrono")]
    #[test]
    fn dates_and_times_refuse_what_they_are_not_and_what_is_out_of_range() {
        use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};

        refuses::<NaiveDate>("DateTime");
        refuses::<DateTime<Utc>>("Date");
        refuses::<NaiveDateTime>("String");
        assert!(
            NaiveDate::decode(&mut Reader::new(&i32::MAX.to_le_bytes()), &ty("Date32")).is_err()
        );
        assert!(
            DateTime::<Utc>::decode(
                &mut Reader::new(&i64::MAX.to_le_bytes()),
                &ty("DateTime64(0)")
            )
            .is_err()
        );
        let naive = decode::<NaiveDateTime>(&0u32.to_le_bytes(), "DateTime");
        assert_eq!(naive.and_utc().timestamp(), 0);
    }

    #[test]
    fn skip_covers_every_fixed_width_and_refuses_what_it_cannot_size() {
        let widths: &[(&str, usize)] = &[
            ("Int8", 1),
            ("Bool", 1),
            ("Enum8('a' = 1)", 1),
            ("UInt16", 2),
            ("Date", 2),
            ("Enum16('a' = 1)", 2),
            ("Int32", 4),
            ("Float32", 4),
            ("Date32", 4),
            ("DateTime", 4),
            ("IPv4", 4),
            ("Decimal(9, 2)", 4),
            ("Int64", 8),
            ("Float64", 8),
            ("DateTime64(3)", 8),
            ("Decimal(18, 2)", 8),
            ("UInt128", 16),
            ("UUID", 16),
            ("IPv6", 16),
            ("UInt256", 32),
            ("Int256", 32),
            ("Decimal(76, 2)", 32),
            ("SimpleAggregateFunction(max, UInt16)", 2),
        ];
        for (text, width) in widths {
            let bytes = vec![0u8; *width];
            let mut reader = Reader::new(&bytes);
            skip(&mut reader, &ty(text)).unwrap_or_else(|err| panic!("{text}: {err}"));
            assert!(reader.is_empty(), "{text} is not {width} bytes");
        }
        assert!(skip(&mut Reader::new(&[]), &ty("Identifier")).is_err());
        assert!(skip(&mut Reader::new(&[1]), &ty("UInt64")).is_err());
    }

    #[test]
    fn a_reader_reports_where_it_is() {
        let mut reader = Reader::new(&[1, 2, 3]);
        assert_eq!((reader.position(), reader.remaining()), (0, 3));
        assert_eq!(reader.u8().unwrap(), 1);
        assert_eq!(reader.array::<2>().unwrap(), [2, 3]);
        assert_eq!((reader.position(), reader.remaining()), (3, 0));
        assert!(reader.is_empty());
        assert!(reader.u8().is_err());
        // Eleven continuation bytes is more than a u64 can hold.
        assert!(Reader::new(&[0x80; 11]).varint().is_err());
    }

    #[test]
    fn an_empty_collection_is_still_refused_for_the_wrong_element_type() {
        // One zero byte is a valid empty array or map of anything.
        assert!(Vec::<u8>::decode(&mut Reader::new(&[0]), &ty("Array(String)")).is_err());
        assert!(
            BTreeMap::<u8, u8>::decode(&mut Reader::new(&[0]), &ty("Map(String, UInt8)")).is_err()
        );
        assert!(
            Vec::<(u8, u8)>::decode(&mut Reader::new(&[0]), &ty("Map(String, UInt8)")).is_err()
        );
    }
}
