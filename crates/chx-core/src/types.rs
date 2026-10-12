//! ClickHouse types and the Rust values that have no standard counterpart.
//!
//! The type model itself lives in `chx-types`, shared with the macros, and is
//! re-exported here. [`Decimal`] and [`Bytes`] are the two value types chx
//! adds: a fixed-point number for `Decimal(P, S)` columns and a byte string
//! for a `String` that is not text.

use std::fmt;
use std::str::FromStr;

pub use chx_types::params::Param;
pub use chx_types::types::{ParseError, TupleElement, Type};

use crate::error::{Error, Result};

/// A fixed-point decimal: `mantissa / 10^scale`.
///
/// This is the wire representation of a ClickHouse `Decimal(P, S)` column,
/// not an arithmetic type. Convert to and from the decimal type the
/// application uses at the edge; a value read from a column carries the
/// column's scale, and a value written to one is rescaled to it, or refused
/// if that would lose digits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Decimal {
    mantissa: i128,
    scale: u8,
}

impl Decimal {
    pub const fn new(mantissa: i128, scale: u8) -> Self {
        Self { mantissa, scale }
    }

    pub const fn mantissa(&self) -> i128 {
        self.mantissa
    }

    pub const fn scale(&self) -> u8 {
        self.scale
    }

    /// The same number at another scale. Fails if digits would be lost
    /// moving to a smaller scale, or the mantissa would overflow moving to a
    /// larger one.
    pub fn rescale(&self, scale: u8) -> Result<Self> {
        if scale == self.scale {
            return Ok(*self);
        }
        let mantissa = if scale > self.scale {
            let factor = pow10(scale - self.scale)?;
            self.mantissa.checked_mul(factor).ok_or_else(|| {
                Error::Encode(format!("{self} does not fit in a Decimal of scale {scale}"))
            })?
        } else {
            let factor = pow10(self.scale - scale)?;
            if self.mantissa % factor != 0 {
                return Err(Error::Encode(format!(
                    "{self} has more than {scale} decimal places; rescaling would lose digits"
                )));
            }
            self.mantissa / factor
        };
        Ok(Self { mantissa, scale })
    }
}

fn pow10(exponent: u8) -> Result<i128> {
    10i128
        .checked_pow(u32::from(exponent))
        .ok_or_else(|| Error::Encode(format!("decimal scale {exponent} is too large")))
}

impl fmt::Display for Decimal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let digits = self.mantissa.unsigned_abs().to_string();
        let sign = if self.mantissa < 0 { "-" } else { "" };
        let scale = usize::from(self.scale);
        if scale == 0 {
            return write!(f, "{sign}{digits}");
        }
        let padded = format!("{digits:0>width$}", width = scale + 1);
        let (whole, fraction) = padded.split_at(padded.len() - scale);
        write!(f, "{sign}{whole}.{fraction}")
    }
}

/// Parses `-123.4500` as mantissa `-1234500` at scale 4. The scale is the
/// number of digits after the point, exactly as written.
impl FromStr for Decimal {
    type Err = Error;

    fn from_str(text: &str) -> Result<Self> {
        let invalid = || Error::Encode(format!("{text:?} is not a decimal number"));
        let (sign, unsigned) = match text.strip_prefix('-') {
            Some(rest) => (-1, rest),
            None => (1, text.strip_prefix('+').unwrap_or(text)),
        };
        let (whole, fraction) = unsigned.split_once('.').unwrap_or((unsigned, ""));
        if whole.is_empty() && fraction.is_empty() {
            return Err(invalid());
        }
        if !whole.chars().all(|c| c.is_ascii_digit()) || !fraction.chars().all(|c| c.is_ascii_digit())
        {
            return Err(invalid());
        }
        let scale = u8::try_from(fraction.len()).map_err(|_| invalid())?;
        let mantissa: i128 = format!("{whole}{fraction}").parse().map_err(|_| invalid())?;
        Ok(Self::new(sign * mantissa, scale))
    }
}

/// A `String` or `FixedString` column read as raw bytes.
///
/// ClickHouse strings are byte strings. `String` decodes the common case,
/// text, and refuses anything else; this is for columns that hold binary
/// data. For a `FixedString(N)`, `[u8; N]` is the stricter choice.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct Bytes(pub Vec<u8>);

impl From<Vec<u8>> for Bytes {
    fn from(bytes: Vec<u8>) -> Self {
        Bytes(bytes)
    }
}

impl From<Bytes> for Vec<u8> {
    fn from(bytes: Bytes) -> Self {
        bytes.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimal_prints_at_its_scale() {
        assert_eq!(Decimal::new(15000, 4).to_string(), "1.5000");
        assert_eq!(Decimal::new(-5, 2).to_string(), "-0.05");
        assert_eq!(Decimal::new(42, 0).to_string(), "42");
        assert_eq!(Decimal::new(0, 3).to_string(), "0.000");
    }

    #[test]
    fn decimal_parses_what_it_prints() {
        for text in ["1.5000", "-0.05", "42", "0.000", "123456789012345678901234567890.123456"] {
            assert_eq!(text.parse::<Decimal>().unwrap().to_string(), text);
        }
        assert_eq!("+1.5".parse::<Decimal>().unwrap(), Decimal::new(15, 1));
        assert!("abc".parse::<Decimal>().is_err());
        assert!("1.2.3".parse::<Decimal>().is_err());
        assert!(".".parse::<Decimal>().is_err());
    }

    #[test]
    fn rescaling_keeps_the_value_or_refuses() {
        let value = Decimal::new(15, 1);
        assert_eq!(value.rescale(4).unwrap(), Decimal::new(15000, 4));
        assert_eq!(Decimal::new(15000, 4).rescale(1).unwrap(), value);
        let err = Decimal::new(15001, 4).rescale(1).unwrap_err();
        assert!(err.to_string().contains("lose digits"), "{err}");
        assert!(Decimal::new(i128::MAX, 0).rescale(1).is_err());
    }
}
