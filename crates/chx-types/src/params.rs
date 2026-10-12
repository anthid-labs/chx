//! Query parameters: `{name:Type}` in the SQL text.
//!
//! ClickHouse substitutes parameters server side, from `param_<name>` on the
//! request, and the type is declared in the query itself. That makes the
//! parameter list a property of the text alone: no server is needed to know
//! what a query takes, only to know what it returns.

use std::fmt;

use crate::types::{ParseError, Type};

/// One parameter the query declares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Param {
    /// The name between the brace and the colon. Values are sent to the
    /// server as `param_<name>`.
    pub name: String,
    /// The type after the colon, which the server parses the value as.
    pub ty: Type,
}

/// A query whose parameters cannot be used as written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParamError {
    /// A `{name:...}` whose type does not parse.
    Type {
        /// The parameter's name.
        name: String,
        /// Why its type did not parse.
        source: ParseError,
    },
    /// The same name declared twice with different types.
    Conflict {
        /// The parameter's name.
        name: String,
        /// The type at its first appearance.
        first: Type,
        /// The different type at a later one.
        second: Type,
    },
}

impl fmt::Display for ParamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParamError::Type { name, source } => {
                write!(f, "parameter {{{name}:...}}: {source}")
            }
            ParamError::Conflict {
                name,
                first,
                second,
            } => write!(
                f,
                "parameter {name} is declared as both {first} and {second}"
            ),
        }
    }
}

impl std::error::Error for ParamError {}

/// Finds every `{name:Type}` in `sql`, in order of first appearance, skipping
/// string literals, quoted identifiers and comments.
///
/// A brace that is not followed by `name:` is not a parameter; ClickHouse has
/// map literals such as `{'k': 1}`, and they are left alone. A name declared
/// twice with the same type is one parameter.
///
/// ```
/// let params = chx_types::params::parse(
///     "SELECT * FROM t WHERE venue = {venue:String} AND ts > {since:DateTime64(9)} -- {not:UInt8}",
/// )
/// .unwrap();
///
/// let names: Vec<_> = params.iter().map(|p| p.name.as_str()).collect();
/// assert_eq!(names, ["venue", "since"]);
/// assert_eq!(params[1].ty.to_string(), "DateTime64(9)");
/// ```
pub fn parse(sql: &str) -> Result<Vec<Param>, ParamError> {
    let bytes = sql.as_bytes();
    let mut params: Vec<Param> = Vec::new();
    let mut i = 0;

    while i < bytes.len() {
        match bytes[i] {
            quote @ (b'\'' | b'"' | b'`') => i = skip_quoted(bytes, i, quote),
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                i = bytes[i..]
                    .iter()
                    .position(|&b| b == b'\n')
                    .map_or(bytes.len(), |offset| i + offset + 1);
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i = bytes[i + 2..]
                    .windows(2)
                    .position(|pair| pair == b"*/")
                    .map_or(bytes.len(), |offset| i + 2 + offset + 2);
            }
            b'{' => match parameter_at(sql, i) {
                Some((name, type_text, end)) => {
                    let ty = Type::parse(type_text).map_err(|source| ParamError::Type {
                        name: name.to_string(),
                        source,
                    })?;
                    match params.iter().find(|param| param.name == name) {
                        Some(existing) if existing.ty != ty => {
                            return Err(ParamError::Conflict {
                                name: name.to_string(),
                                first: existing.ty.clone(),
                                second: ty,
                            });
                        }
                        Some(_) => {}
                        None => params.push(Param {
                            name: name.to_string(),
                            ty,
                        }),
                    }
                    i = end;
                }
                None => i += 1,
            },
            _ => i += 1,
        }
    }

    Ok(params)
}

/// If the brace at `open` starts a `{name:Type}`, returns the name, the type
/// text and the index just past the closing brace.
fn parameter_at(sql: &str, open: usize) -> Option<(&str, &str, usize)> {
    let bytes = sql.as_bytes();
    let mut i = open + 1;
    while bytes.get(i).is_some_and(|b| b.is_ascii_whitespace()) {
        i += 1;
    }
    let name_start = i;
    if !bytes
        .get(i)
        .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_')
    {
        return None;
    }
    while bytes
        .get(i)
        .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
    {
        i += 1;
    }
    let name = &sql[name_start..i];
    while bytes.get(i).is_some_and(|b| b.is_ascii_whitespace()) {
        i += 1;
    }
    if bytes.get(i) != Some(&b':') {
        return None;
    }
    i += 1;

    // The type runs to the first `}` outside parentheses and strings, so
    // `{d:DateTime64(3, 'UTC')}` is one parameter.
    let type_start = i;
    let mut depth = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' => i = skip_quoted(bytes, i, b'\''),
            b'(' => {
                depth += 1;
                i += 1;
            }
            b')' => {
                depth = depth.checked_sub(1)?;
                i += 1;
            }
            b'}' if depth == 0 => {
                let type_text = sql[type_start..i].trim();
                if type_text.is_empty() {
                    return None;
                }
                return Some((name, type_text, i + 1));
            }
            b'{' => return None,
            _ => i += 1,
        }
    }
    None
}

/// Mirrors the statement splitter: a backslash escapes the next byte, and a
/// doubled quote is a literal quote.
fn skip_quoted(bytes: &[u8], open: usize, quote: u8) -> usize {
    let mut i = open + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            byte if byte == quote => {
                if bytes.get(i + 1) == Some(&quote) {
                    i += 2;
                } else {
                    return i + 1;
                }
            }
            _ => i += 1,
        }
    }
    bytes.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(sql: &str) -> Vec<String> {
        parse(sql)
            .unwrap()
            .into_iter()
            .map(|param| format!("{}:{}", param.name, param.ty))
            .collect()
    }

    #[test]
    fn finds_parameters_in_order_of_appearance() {
        assert_eq!(
            names("SELECT * FROM t WHERE venue = {v:String} AND ts > {since:DateTime64(9, 'UTC')}"),
            ["v:String", "since:DateTime64(9, 'UTC')"]
        );
    }

    #[test]
    fn a_repeated_parameter_is_one_parameter() {
        assert_eq!(
            names("SELECT {n:UInt64} AS a, {n:UInt64} AS b"),
            ["n:UInt64"]
        );
    }

    #[test]
    fn a_repeated_name_with_another_type_is_refused() {
        let err = parse("SELECT {n:UInt64}, {n:String}").unwrap_err();
        assert!(matches!(err, ParamError::Conflict { .. }), "{err}");
    }

    #[test]
    fn braces_in_strings_identifiers_and_comments_are_not_parameters() {
        assert!(
            names(
                "SELECT '{x:String}', \"{y:UInt8}\", `{z:UInt8}` -- {c:UInt8}\n/* {d:UInt8} */ FROM t"
            )
            .is_empty()
        );
    }

    #[test]
    fn map_literals_and_stray_braces_are_left_alone() {
        assert!(names("SELECT {'k': 1}, {}, { 1 }").is_empty());
        assert_eq!(names("SELECT {'k': {v:UInt8}}"), ["v:UInt8"]);
    }

    #[test]
    fn whitespace_around_the_name_and_type_is_allowed() {
        assert_eq!(names("SELECT { n : UInt64 }"), ["n:UInt64"]);
    }

    #[test]
    fn a_bad_type_is_an_error_not_a_literal() {
        let err = parse("SELECT {n:Array()}").unwrap_err();
        assert!(matches!(err, ParamError::Type { .. }), "{err}");
        assert!(err.to_string().contains("Array()"), "{err}");
        // An unknown name is a type this model does not know, not a syntax
        // error: the server decides whether it exists.
        assert_eq!(names("SELECT {n:UInt7}"), ["n:UInt7"]);
    }

    #[test]
    fn identifier_parameters_parse() {
        assert_eq!(names("SELECT id FROM {t:Identifier}"), ["t:Identifier"]);
    }

    #[test]
    fn errors_print_the_parameter_they_are_about() {
        let conflict = parse("SELECT {n:UInt64}, {n:String}").unwrap_err();
        assert_eq!(
            conflict.to_string(),
            "parameter n is declared as both UInt64 and String"
        );
        let bad = parse("SELECT {n:Array()}").unwrap_err();
        assert!(
            bad.to_string()
                .starts_with("parameter {n:...}: cannot parse type"),
            "{bad}"
        );
    }

    #[test]
    fn braces_that_only_look_like_parameters_are_left_alone() {
        // No colon, an empty type, a nested brace, unbalanced parentheses, or
        // never closed: none of these is a parameter, and none is an error.
        for sql in [
            "SELECT {name}",
            "SELECT {name:}",
            "SELECT {name: {inner}}",
            "SELECT {name:Array)}",
            "SELECT {name:String",
            "SELECT {1:String}",
            "SELECT {",
        ] {
            assert!(names(sql).is_empty(), "{sql}");
        }
    }

    #[test]
    fn unterminated_quotes_and_comments_end_the_scan_quietly() {
        assert!(names("SELECT 'never closed {a:UInt8}").is_empty());
        assert!(names("SELECT 1 /* never closed {a:UInt8}").is_empty());
        assert!(names("SELECT 1 -- trailing {a:UInt8}").is_empty());
        assert_eq!(names("SELECT 'it''s', 'a\\'b', {a:UInt8}"), ["a:UInt8"]);
        assert_eq!(names("SELECT {e:Enum8('}' = 1)}"), ["e:Enum8('}' = 1)"]);
    }
}
