//! Splitting a migration file into statements.
//!
//! The HTTP interface takes one statement per request, and chx records progress
//! per statement, so a file has to be cut at its semicolons. Not at every
//! semicolon: one inside a string literal, a quoted identifier or a comment is
//! part of the statement.
//!
//! This is a lexer for exactly those four cases and nothing else. It does not
//! parse SQL, and a statement it cannot see the end of is sent whole for
//! ClickHouse to reject, which is a clearer failure than a guess.

/// Splits `sql` into statements, without their terminating semicolons.
///
/// A fragment holding only whitespace and comments is dropped, so a trailing
/// semicolon or a commented-out statement does not become an empty request.
/// Comments inside a kept statement are left in place: they reach the server
/// and show up in `system.query_log`, which is where someone reading a failure
/// will look.
pub fn statements(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut start = 0;
    // Whether the current fragment has anything other than whitespace and
    // comments in it.
    let mut substantive = false;
    let bytes = sql.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        match bytes[i] {
            quote @ (b'\'' | b'"' | b'`') => {
                substantive = true;
                i = skip_quoted(bytes, i, quote);
            }
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
            b';' => {
                if substantive {
                    out.push(sql[start..i].trim().to_string());
                }
                substantive = false;
                i += 1;
                start = i;
            }
            byte => {
                if !byte.is_ascii_whitespace() {
                    substantive = true;
                }
                i += 1;
            }
        }
    }

    if substantive {
        out.push(sql[start..].trim().to_string());
    }

    out
}

/// Returns the index just past the closing quote that matches the one at
/// `open`, or the end of input if it is never closed.
///
/// ClickHouse accepts both escape styles in all three quote kinds: a backslash
/// before any character, and a doubled quote.
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

    #[test]
    fn splits_on_semicolons() {
        assert_eq!(
            statements("CREATE TABLE a (x UInt8) ENGINE = Memory;\nDROP TABLE b;"),
            ["CREATE TABLE a (x UInt8) ENGINE = Memory", "DROP TABLE b"]
        );
    }

    #[test]
    fn keeps_a_final_statement_with_no_semicolon() {
        assert_eq!(statements("SELECT 1;\nSELECT 2"), ["SELECT 1", "SELECT 2"]);
    }

    #[test]
    fn semicolons_in_strings_and_identifiers_do_not_split() {
        assert_eq!(
            statements(
                r#"INSERT INTO t VALUES ('a;b'); ALTER TABLE `we;ird` ADD COLUMN "c;d" String"#
            ),
            [
                "INSERT INTO t VALUES ('a;b')",
                r#"ALTER TABLE `we;ird` ADD COLUMN "c;d" String"#
            ]
        );
    }

    #[test]
    fn both_escape_styles_stay_inside_the_string() {
        assert_eq!(
            statements(r"SELECT 'it''s; \'fine\'; ok'; SELECT 2"),
            [r"SELECT 'it''s; \'fine\'; ok'", "SELECT 2"]
        );
    }

    #[test]
    fn semicolons_in_comments_do_not_split() {
        assert_eq!(
            statements("SELECT 1 -- one; two\n; /* three; four */ SELECT 2;"),
            ["SELECT 1 -- one; two", "/* three; four */ SELECT 2"]
        );
    }

    #[test]
    fn comment_only_fragments_are_dropped() {
        assert_eq!(
            statements("-- header\n\nSELECT 1;\n-- SELECT 2;\n/* SELECT 3; */;\n  ;"),
            ["-- header\n\nSELECT 1"]
        );
    }

    #[test]
    fn an_empty_file_has_no_statements() {
        assert!(statements("").is_empty());
        assert!(statements("  \n-- nothing yet\n").is_empty());
    }

    #[test]
    fn an_unterminated_string_runs_to_the_end_rather_than_splitting() {
        assert_eq!(
            statements("SELECT 'oops; SELECT 2"),
            ["SELECT 'oops; SELECT 2"]
        );
    }

    #[test]
    fn multibyte_text_survives() {
        assert_eq!(
            statements("COMMENT COLUMN x 'prix en €; ok'; SELECT 1"),
            ["COMMENT COLUMN x 'prix en €; ok'", "SELECT 1"]
        );
    }
}
