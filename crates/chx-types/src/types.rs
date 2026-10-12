//! The ClickHouse type model.
//!
//! Types are parsed from the text ClickHouse itself prints, in `DESCRIBE`
//! output and in the header of `RowBinaryWithNamesAndTypes`, and printed back
//! in the same canonical form. Both directions matter: a cached describe is
//! stored as text, and a schema check compares what the server sent against
//! what the code was compiled with.
//!
//! The model covers what RowBinary can carry and what a query parameter can
//! name. Anything else parses as [`Type::Other`], which keeps its text so an
//! error can show it, and is refused by every codec.

use std::fmt;
use std::str::FromStr;

/// A ClickHouse data type.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Type {
    UInt8,
    UInt16,
    UInt32,
    UInt64,
    UInt128,
    UInt256,
    Int8,
    Int16,
    Int32,
    Int64,
    Int128,
    Int256,
    Float32,
    Float64,
    Bool,
    String,
    FixedString(usize),
    Date,
    Date32,
    DateTime {
        timezone: Option<String>,
    },
    DateTime64 {
        precision: u8,
        timezone: Option<String>,
    },
    /// `Decimal32(S)`, `Decimal64(S)`, `Decimal128(S)` and `Decimal256(S)`
    /// normalise to this, with the precision they imply, because that is how
    /// the server describes them.
    Decimal {
        precision: u8,
        scale: u8,
    },
    Uuid,
    IPv4,
    IPv6,
    Enum8(Vec<(String, i8)>),
    Enum16(Vec<(String, i16)>),
    Nullable(Box<Type>),
    /// Serialised exactly as its inner type in RowBinary, so every codec
    /// looks through it. See [`Type::unwrapped`].
    LowCardinality(Box<Type>),
    Array(Box<Type>),
    /// On the wire, a `Map(K, V)` is an `Array(Tuple(K, V))`.
    Map(Box<Type>, Box<Type>),
    Tuple(Vec<TupleElement>),
    /// Serialised as its inner type, like `LowCardinality`.
    SimpleAggregateFunction {
        function: String,
        inner: Box<Type>,
    },
    /// The type of `NULL` on its own, as in `Nullable(Nothing)`.
    Nothing,
    /// Only valid in a query parameter, `{table:Identifier}`. It has no
    /// serialisation, and no `DESCRIBE` can check a query that uses one.
    Identifier,
    /// A type this model does not cover, kept as the server printed it.
    Other(String),
}

/// One element of a `Tuple`, named or positional.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TupleElement {
    pub name: Option<String>,
    pub ty: Type,
}

/// A type string that could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub text: String,
    /// Byte offset into `text`.
    pub position: usize,
    pub message: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "cannot parse type {:?} at byte {}: {}",
            self.text, self.position, self.message
        )
    }
}

impl std::error::Error for ParseError {}

impl Type {
    /// Parses a type as ClickHouse prints it: `Array(Nullable(UInt64))`,
    /// `DateTime64(9, 'UTC')`, `Enum8('a' = 1, 'b' = 2)`.
    pub fn parse(text: &str) -> Result<Type, ParseError> {
        let mut parser = Parser { text, pos: 0 };
        let ty = parser.parse_type()?;
        parser.skip_ws();
        if parser.pos != text.len() {
            return Err(parser.error("unexpected text after the type"));
        }
        Ok(ty)
    }

    /// The type as it is serialised: `LowCardinality(T)` and
    /// `SimpleAggregateFunction(f, T)` are both written as `T`.
    pub fn unwrapped(&self) -> &Type {
        match self {
            Type::LowCardinality(inner) | Type::SimpleAggregateFunction { inner, .. } => {
                inner.unwrapped()
            }
            other => other,
        }
    }

    /// Whether a value of this type can be `NULL`.
    pub fn is_nullable(&self) -> bool {
        matches!(self.unwrapped(), Type::Nullable(_))
    }

    /// A value of this type in the text form a query parameter takes, for a
    /// `DESCRIBE` that only needs the parameters to be set. `None` for a type
    /// that has no literal, such as `Identifier`.
    pub fn placeholder(&self) -> Option<String> {
        self.placeholder_literal(false)
    }

    /// `nested` is whether the value sits inside an array, map or tuple,
    /// where strings and dates are quoted and NULL is spelled out. At the top
    /// level the server reads the parameter in the escaped text format, where
    /// a string is bare and NULL is `\N`.
    fn placeholder_literal(&self, nested: bool) -> Option<String> {
        let quote = |s: &str| {
            if nested {
                format!("'{s}'")
            } else {
                s.to_string()
            }
        };
        let value = match self {
            Type::UInt8
            | Type::UInt16
            | Type::UInt32
            | Type::UInt64
            | Type::UInt128
            | Type::UInt256
            | Type::Int8
            | Type::Int16
            | Type::Int32
            | Type::Int64
            | Type::Int128
            | Type::Int256
            | Type::Float32
            | Type::Float64
            | Type::Decimal { .. } => "0".to_string(),
            Type::Bool => "false".to_string(),
            Type::String | Type::FixedString(_) => quote(""),
            Type::Date | Type::Date32 => quote("1970-01-01"),
            Type::DateTime { .. } | Type::DateTime64 { .. } => quote("1970-01-01 00:00:00"),
            Type::Uuid => quote("00000000-0000-0000-0000-000000000000"),
            Type::IPv4 => quote("0.0.0.0"),
            Type::IPv6 => quote("::"),
            Type::Enum8(variants) => quote(&variants.first()?.0),
            Type::Enum16(variants) => quote(&variants.first()?.0),
            Type::Nullable(_) => {
                if nested {
                    "NULL".to_string()
                } else {
                    "\\N".to_string()
                }
            }
            Type::LowCardinality(inner) | Type::SimpleAggregateFunction { inner, .. } => {
                return inner.placeholder_literal(nested);
            }
            Type::Array(_) => "[]".to_string(),
            Type::Map(_, _) => "{}".to_string(),
            Type::Tuple(elements) => {
                let parts = elements
                    .iter()
                    .map(|element| element.ty.placeholder_literal(true))
                    .collect::<Option<Vec<_>>>()?;
                format!("({})", parts.join(", "))
            }
            Type::Nothing | Type::Identifier | Type::Other(_) => return None,
        };
        Some(value)
    }
}

impl FromStr for Type {
    type Err = ParseError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Type::parse(text)
    }
}

impl fmt::Display for Type {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Type::UInt8 => f.write_str("UInt8"),
            Type::UInt16 => f.write_str("UInt16"),
            Type::UInt32 => f.write_str("UInt32"),
            Type::UInt64 => f.write_str("UInt64"),
            Type::UInt128 => f.write_str("UInt128"),
            Type::UInt256 => f.write_str("UInt256"),
            Type::Int8 => f.write_str("Int8"),
            Type::Int16 => f.write_str("Int16"),
            Type::Int32 => f.write_str("Int32"),
            Type::Int64 => f.write_str("Int64"),
            Type::Int128 => f.write_str("Int128"),
            Type::Int256 => f.write_str("Int256"),
            Type::Float32 => f.write_str("Float32"),
            Type::Float64 => f.write_str("Float64"),
            Type::Bool => f.write_str("Bool"),
            Type::String => f.write_str("String"),
            Type::FixedString(n) => write!(f, "FixedString({n})"),
            Type::Date => f.write_str("Date"),
            Type::Date32 => f.write_str("Date32"),
            Type::DateTime { timezone: None } => f.write_str("DateTime"),
            Type::DateTime { timezone: Some(tz) } => write!(f, "DateTime({})", Quoted(tz)),
            Type::DateTime64 {
                precision,
                timezone: None,
            } => write!(f, "DateTime64({precision})"),
            Type::DateTime64 {
                precision,
                timezone: Some(tz),
            } => write!(f, "DateTime64({precision}, {})", Quoted(tz)),
            Type::Decimal { precision, scale } => write!(f, "Decimal({precision}, {scale})"),
            Type::Uuid => f.write_str("UUID"),
            Type::IPv4 => f.write_str("IPv4"),
            Type::IPv6 => f.write_str("IPv6"),
            Type::Enum8(variants) => {
                f.write_str("Enum8(")?;
                write_variants(f, variants.iter().map(|(name, value)| (name, *value as i64)))?;
                f.write_str(")")
            }
            Type::Enum16(variants) => {
                f.write_str("Enum16(")?;
                write_variants(f, variants.iter().map(|(name, value)| (name, *value as i64)))?;
                f.write_str(")")
            }
            Type::Nullable(inner) => write!(f, "Nullable({inner})"),
            Type::LowCardinality(inner) => write!(f, "LowCardinality({inner})"),
            Type::Array(inner) => write!(f, "Array({inner})"),
            Type::Map(key, value) => write!(f, "Map({key}, {value})"),
            Type::Tuple(elements) => {
                f.write_str("Tuple(")?;
                for (i, element) in elements.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    if let Some(name) = &element.name {
                        if is_identifier(name) {
                            write!(f, "{name} ")?;
                        } else {
                            write!(f, "`{}` ", name.replace('`', "\\`"))?;
                        }
                    }
                    write!(f, "{}", element.ty)?;
                }
                f.write_str(")")
            }
            Type::SimpleAggregateFunction { function, inner } => {
                write!(f, "SimpleAggregateFunction({function}, {inner})")
            }
            Type::Nothing => f.write_str("Nothing"),
            Type::Identifier => f.write_str("Identifier"),
            Type::Other(raw) => f.write_str(raw),
        }
    }
}

fn write_variants<'a>(
    f: &mut fmt::Formatter<'_>,
    variants: impl Iterator<Item = (&'a String, i64)>,
) -> fmt::Result {
    for (i, (name, value)) in variants.enumerate() {
        if i > 0 {
            f.write_str(", ")?;
        }
        write!(f, "{} = {value}", Quoted(name))?;
    }
    Ok(())
}

/// A single-quoted string literal with ClickHouse escaping.
struct Quoted<'a>(&'a str);

impl fmt::Display for Quoted<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("'")?;
        for c in self.0.chars() {
            match c {
                '\'' => f.write_str("\\'")?,
                '\\' => f.write_str("\\\\")?,
                c => write!(f, "{c}")?,
            }
        }
        f.write_str("'")
    }
}

fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// A cursor over the type text. Types are small, so this reads bytes directly
/// rather than tokenising first; the one place that needs lookahead is a tuple
/// element, which may or may not start with a name.
struct Parser<'a> {
    text: &'a str,
    pos: usize,
}

impl<'a> Parser<'a> {
    fn error(&self, message: impl Into<String>) -> ParseError {
        ParseError {
            text: self.text.to_string(),
            position: self.pos,
            message: message.into(),
        }
    }

    fn peek(&self) -> Option<u8> {
        self.text.as_bytes().get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while self.peek().is_some_and(|b| b.is_ascii_whitespace()) {
            self.pos += 1;
        }
    }

    fn eat(&mut self, byte: u8) -> bool {
        self.skip_ws();
        if self.peek() == Some(byte) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, byte: u8) -> Result<(), ParseError> {
        if self.eat(byte) {
            Ok(())
        } else {
            Err(self.error(format!("expected {:?}", byte as char)))
        }
    }

    fn identifier(&mut self) -> Result<&'a str, ParseError> {
        self.skip_ws();
        let start = self.pos;
        while self
            .peek()
            .is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            self.pos += 1;
        }
        if start == self.pos {
            return Err(self.error("expected a type name"));
        }
        Ok(&self.text[start..self.pos])
    }

    fn integer(&mut self) -> Result<i64, ParseError> {
        self.skip_ws();
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        while self.peek().is_some_and(|b| b.is_ascii_digit()) {
            self.pos += 1;
        }
        self.text[start..self.pos]
            .parse()
            .map_err(|_| self.error("expected an integer"))
    }

    fn small(&mut self, what: &str, max: i64) -> Result<u8, ParseError> {
        let value = self.integer()?;
        if (0..=max).contains(&value) {
            Ok(value as u8)
        } else {
            Err(self.error(format!("{what} must be between 0 and {max}")))
        }
    }

    /// A `'...'` literal. Both escape styles ClickHouse accepts: a backslash
    /// before any character, and a doubled quote.
    fn string(&mut self) -> Result<String, ParseError> {
        self.expect(b'\'')?;
        let bytes = self.text.as_bytes();
        let mut out = Vec::new();
        loop {
            match bytes.get(self.pos) {
                None => return Err(self.error("unterminated string")),
                Some(b'\\') => {
                    let escaped = *bytes
                        .get(self.pos + 1)
                        .ok_or_else(|| self.error("unterminated string"))?;
                    out.push(match escaped {
                        b'n' => b'\n',
                        b't' => b'\t',
                        b'r' => b'\r',
                        b'0' => 0,
                        other => other,
                    });
                    self.pos += 2;
                }
                Some(b'\'') if bytes.get(self.pos + 1) == Some(&b'\'') => {
                    out.push(b'\'');
                    self.pos += 2;
                }
                Some(b'\'') => {
                    self.pos += 1;
                    break;
                }
                Some(&byte) => {
                    out.push(byte);
                    self.pos += 1;
                }
            }
        }
        String::from_utf8(out).map_err(|_| self.error("string is not valid UTF-8"))
    }

    fn quoted_identifier(&mut self) -> Result<String, ParseError> {
        self.expect(b'`')?;
        let start = self.pos;
        let rest = &self.text[start..];
        let end = rest
            .find('`')
            .ok_or_else(|| self.error("unterminated quoted name"))?;
        self.pos = start + end + 1;
        Ok(rest[..end].to_string())
    }

    fn parse_type(&mut self) -> Result<Type, ParseError> {
        self.skip_ws();
        let start = self.pos;
        let name = self.identifier()?;

        let simple = match name {
            "UInt8" => Some(Type::UInt8),
            "UInt16" => Some(Type::UInt16),
            "UInt32" => Some(Type::UInt32),
            "UInt64" => Some(Type::UInt64),
            "UInt128" => Some(Type::UInt128),
            "UInt256" => Some(Type::UInt256),
            "Int8" => Some(Type::Int8),
            "Int16" => Some(Type::Int16),
            "Int32" => Some(Type::Int32),
            "Int64" => Some(Type::Int64),
            "Int128" => Some(Type::Int128),
            "Int256" => Some(Type::Int256),
            "Float32" => Some(Type::Float32),
            "Float64" => Some(Type::Float64),
            "Bool" => Some(Type::Bool),
            "String" => Some(Type::String),
            "Date" => Some(Type::Date),
            "Date32" => Some(Type::Date32),
            "UUID" => Some(Type::Uuid),
            "IPv4" => Some(Type::IPv4),
            "IPv6" => Some(Type::IPv6),
            "Nothing" => Some(Type::Nothing),
            "Identifier" => Some(Type::Identifier),
            _ => None,
        };
        if let Some(ty) = simple {
            self.skip_ws();
            if self.peek() == Some(b'(') {
                return Err(self.error(format!("{name} takes no arguments")));
            }
            return Ok(ty);
        }

        match name {
            "FixedString" => {
                self.expect(b'(')?;
                let n = self.integer()?;
                if n <= 0 {
                    return Err(self.error("FixedString length must be positive"));
                }
                self.expect(b')')?;
                Ok(Type::FixedString(n as usize))
            }
            "DateTime" => {
                let mut timezone = None;
                if self.eat(b'(') {
                    self.skip_ws();
                    if self.peek() != Some(b')') {
                        timezone = Some(self.string()?);
                    }
                    self.expect(b')')?;
                }
                Ok(Type::DateTime { timezone })
            }
            "DateTime64" => {
                self.expect(b'(')?;
                let precision = self.small("DateTime64 precision", 9)?;
                let mut timezone = None;
                if self.eat(b',') {
                    timezone = Some(self.string()?);
                }
                self.expect(b')')?;
                Ok(Type::DateTime64 {
                    precision,
                    timezone,
                })
            }
            "Decimal" => {
                self.expect(b'(')?;
                let precision = self.small("Decimal precision", 76)?;
                self.expect(b',')?;
                let scale = self.small("Decimal scale", 76)?;
                self.expect(b')')?;
                if scale > precision {
                    return Err(self.error("Decimal scale cannot exceed its precision"));
                }
                Ok(Type::Decimal { precision, scale })
            }
            "Decimal32" | "Decimal64" | "Decimal128" | "Decimal256" => {
                let precision = match name {
                    "Decimal32" => 9,
                    "Decimal64" => 18,
                    "Decimal128" => 38,
                    _ => 76,
                };
                self.expect(b'(')?;
                let scale = self.small("Decimal scale", precision as i64)?;
                self.expect(b')')?;
                Ok(Type::Decimal { precision, scale })
            }
            "Enum" | "Enum8" | "Enum16" => self.parse_enum(name),
            "Nullable" | "LowCardinality" | "Array" => {
                self.expect(b'(')?;
                let inner = Box::new(self.parse_type()?);
                self.expect(b')')?;
                Ok(match name {
                    "Nullable" => Type::Nullable(inner),
                    "LowCardinality" => Type::LowCardinality(inner),
                    _ => Type::Array(inner),
                })
            }
            "Map" => {
                self.expect(b'(')?;
                let key = Box::new(self.parse_type()?);
                self.expect(b',')?;
                let value = Box::new(self.parse_type()?);
                self.expect(b')')?;
                Ok(Type::Map(key, value))
            }
            "Tuple" => {
                self.expect(b'(')?;
                let mut elements = Vec::new();
                loop {
                    elements.push(self.parse_tuple_element()?);
                    if !self.eat(b',') {
                        break;
                    }
                }
                self.expect(b')')?;
                Ok(Type::Tuple(elements))
            }
            "SimpleAggregateFunction" => {
                self.expect(b'(')?;
                let function = self.identifier()?.to_string();
                self.expect(b',')?;
                let inner = Box::new(self.parse_type()?);
                self.expect(b')')?;
                Ok(Type::SimpleAggregateFunction { function, inner })
            }
            _ => {
                self.skip_ws();
                if self.peek() == Some(b'(') {
                    self.skip_balanced()?;
                }
                Ok(Type::Other(self.text[start..self.pos].trim().to_string()))
            }
        }
    }

    fn parse_enum(&mut self, name: &str) -> Result<Type, ParseError> {
        self.expect(b'(')?;
        let mut variants: Vec<(String, i64)> = Vec::new();
        loop {
            let variant = self.string()?;
            let value = if self.eat(b'=') {
                self.integer()?
            } else {
                // Unnumbered variants count from 1, as in a CREATE TABLE.
                variants.last().map_or(1, |(_, last)| last + 1)
            };
            variants.push((variant, value));
            if !self.eat(b',') {
                break;
            }
        }
        self.expect(b')')?;

        let fits_i8 = variants
            .iter()
            .all(|(_, value)| i8::try_from(*value).is_ok());
        let fits_i16 = variants
            .iter()
            .all(|(_, value)| i16::try_from(*value).is_ok());
        match name {
            "Enum8" | "Enum" if fits_i8 => Ok(Type::Enum8(
                variants
                    .into_iter()
                    .map(|(name, value)| (name, value as i8))
                    .collect(),
            )),
            "Enum8" => Err(self.error("Enum8 value does not fit in 8 bits")),
            _ if fits_i16 => Ok(Type::Enum16(
                variants
                    .into_iter()
                    .map(|(name, value)| (name, value as i16))
                    .collect(),
            )),
            _ => Err(self.error("Enum16 value does not fit in 16 bits")),
        }
    }

    /// `name Type` or `Type`. The name is whatever comes before a second word
    /// on the same level, so `a UInt8` is named and `Array(UInt8)` is not.
    fn parse_tuple_element(&mut self) -> Result<TupleElement, ParseError> {
        self.skip_ws();
        if self.peek() == Some(b'`') {
            let name = self.quoted_identifier()?;
            return Ok(TupleElement {
                name: Some(name),
                ty: self.parse_type()?,
            });
        }

        let start = self.pos;
        let word = self.identifier()?;
        self.skip_ws();
        let named = self
            .peek()
            .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_' || b == b'`');
        if named {
            return Ok(TupleElement {
                name: Some(word.to_string()),
                ty: self.parse_type()?,
            });
        }

        self.pos = start;
        Ok(TupleElement {
            name: None,
            ty: self.parse_type()?,
        })
    }

    /// Skips a parenthesised argument list of a type this model does not
    /// know, so the whole thing can be kept as text.
    fn skip_balanced(&mut self) -> Result<(), ParseError> {
        let bytes = self.text.as_bytes();
        let mut depth = 0usize;
        while let Some(&byte) = bytes.get(self.pos) {
            match byte {
                b'\'' => {
                    self.string()?;
                    continue;
                }
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        self.pos += 1;
                        return Ok(());
                    }
                }
                _ => {}
            }
            self.pos += 1;
        }
        Err(self.error("unbalanced parentheses"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(text: &str) -> Type {
        let ty = Type::parse(text).unwrap_or_else(|err| panic!("{err}"));
        assert_eq!(ty.to_string(), text, "display is not canonical");
        ty
    }

    #[test]
    fn scalars_parse_and_print_as_themselves() {
        for text in [
            "UInt8", "UInt256", "Int128", "Float64", "Bool", "String", "Date", "Date32", "UUID",
            "IPv4", "IPv6", "Nothing",
        ] {
            roundtrip(text);
        }
    }

    #[test]
    fn parameterised_types_nest() {
        assert_eq!(
            roundtrip("Array(Nullable(UInt64))"),
            Type::Array(Box::new(Type::Nullable(Box::new(Type::UInt64))))
        );
        assert_eq!(
            roundtrip("Map(String, Array(LowCardinality(String)))"),
            Type::Map(
                Box::new(Type::String),
                Box::new(Type::Array(Box::new(Type::LowCardinality(Box::new(
                    Type::String
                )))))
            )
        );
        roundtrip("Tuple(UInt8, String)");
        roundtrip("Tuple(a UInt8, b Array(String))");
        roundtrip("SimpleAggregateFunction(sum, UInt64)");
    }

    #[test]
    fn tuple_names_that_are_not_identifiers_are_quoted() {
        let ty = roundtrip("Tuple(`my col` UInt8)");
        assert_eq!(
            ty,
            Type::Tuple(vec![TupleElement {
                name: Some("my col".to_string()),
                ty: Type::UInt8
            }])
        );
    }

    #[test]
    fn time_types_keep_precision_and_timezone() {
        assert_eq!(
            roundtrip("DateTime64(9, 'UTC')"),
            Type::DateTime64 {
                precision: 9,
                timezone: Some("UTC".to_string())
            }
        );
        roundtrip("DateTime64(3)");
        roundtrip("DateTime('America/New_York')");
        roundtrip("DateTime");
        assert!(Type::parse("DateTime64(10)").is_err());
    }

    #[test]
    fn decimal_aliases_normalise_to_the_described_form() {
        assert_eq!(
            Type::parse("Decimal64(4)").unwrap(),
            Type::Decimal {
                precision: 18,
                scale: 4
            }
        );
        assert_eq!(
            Type::parse("Decimal128(18)").unwrap().to_string(),
            "Decimal(38, 18)"
        );
        roundtrip("Decimal(38, 18)");
        assert!(Type::parse("Decimal(4, 5)").is_err());
    }

    #[test]
    fn enums_keep_names_values_and_width() {
        assert_eq!(
            roundtrip("Enum8('a' = 1, 'b' = 2)"),
            Type::Enum8(vec![("a".to_string(), 1), ("b".to_string(), 2)])
        );
        roundtrip("Enum16('x' = 300)");
        // Unsized Enum picks the narrowest width, and unnumbered variants count
        // from one, as CREATE TABLE does.
        assert_eq!(
            Type::parse("Enum('a', 'b')").unwrap(),
            Type::Enum8(vec![("a".to_string(), 1), ("b".to_string(), 2)])
        );
        assert_eq!(
            Type::parse("Enum('a' = 1000)").unwrap(),
            Type::Enum16(vec![("a".to_string(), 1000)])
        );
        assert!(Type::parse("Enum8('a' = 200)").is_err());
    }

    #[test]
    fn strings_in_types_accept_both_escape_styles() {
        assert_eq!(
            Type::parse(r"Enum8('it\'s' = 1, 'a''b' = 2)").unwrap(),
            Type::Enum8(vec![("it's".to_string(), 1), ("a'b".to_string(), 2)])
        );
        assert_eq!(
            Type::Enum8(vec![("it's".to_string(), 1)]).to_string(),
            r"Enum8('it\'s' = 1)"
        );
    }

    #[test]
    fn unknown_types_are_kept_as_text() {
        assert_eq!(
            Type::parse("JSON(max_dynamic_paths=16, a.b UInt32)").unwrap(),
            Type::Other("JSON(max_dynamic_paths=16, a.b UInt32)".to_string())
        );
        assert_eq!(
            Type::parse("Array(Variant(UInt8, String))").unwrap(),
            Type::Array(Box::new(Type::Other("Variant(UInt8, String)".to_string())))
        );
        assert_eq!(Type::parse("Dynamic").unwrap(), Type::Other("Dynamic".to_string()));
    }

    #[test]
    fn malformed_types_are_refused_with_a_position() {
        let err = Type::parse("Array(UInt8").unwrap_err();
        assert_eq!(err.position, 11);
        assert!(Type::parse("String(1)").is_err());
        assert!(Type::parse("UInt8 extra").is_err());
        assert!(Type::parse("").is_err());
    }

    #[test]
    fn unwrapped_sees_through_storage_wrappers() {
        let ty = Type::parse("LowCardinality(Nullable(String))").unwrap();
        assert_eq!(ty.unwrapped(), &Type::Nullable(Box::new(Type::String)));
        assert!(ty.is_nullable());
        assert!(!Type::parse("Array(Nullable(String))").unwrap().is_nullable());
    }

    #[test]
    fn placeholders_follow_the_parameter_text_format() {
        let placeholder = |text: &str| Type::parse(text).unwrap().placeholder();
        assert_eq!(placeholder("UInt64").as_deref(), Some("0"));
        assert_eq!(placeholder("String").as_deref(), Some(""));
        assert_eq!(placeholder("Nullable(String)").as_deref(), Some("\\N"));
        assert_eq!(placeholder("Array(String)").as_deref(), Some("[]"));
        assert_eq!(
            placeholder("Tuple(UInt8, String, Nullable(Date))").as_deref(),
            Some("(0, '', NULL)")
        );
        assert_eq!(placeholder("Enum8('a' = 1)").as_deref(), Some("a"));
        assert_eq!(placeholder("LowCardinality(String)").as_deref(), Some(""));
        assert_eq!(placeholder("Identifier"), None);
        assert_eq!(placeholder("Dynamic"), None);
    }
}
