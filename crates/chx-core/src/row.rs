//! Rows of a `RowBinaryWithNamesAndTypes` result.
//!
//! The header names and types every column. A body is cut into rows, and a
//! row into columns, by walking those types once; the bytes are only turned
//! into values when a column is read, as the type the caller asks for. That
//! is what lets a [`FromRow`] read columns by name in any order from a format
//! that has no random access of its own.

use std::sync::Arc;

use crate::decode::{Decode, Reader, skip};
use crate::error::{Error, Result};
use crate::types::Type;

/// One column of a result set, as the server described it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    /// The column's name or alias in the result.
    pub name: String,
    /// The column's type.
    pub ty: Type,
}

/// One row. Cheap to clone: it shares the body and the column list.
#[derive(Debug, Clone)]
pub struct Row {
    columns: Arc<[Column]>,
    body: Arc<Vec<u8>>,
    /// Byte offsets into `body`, one per column plus the end.
    offsets: Vec<usize>,
}

impl Row {
    /// The columns of the result this row belongs to, in order.
    pub fn columns(&self) -> &[Column] {
        &self.columns
    }

    /// The number of columns.
    pub fn len(&self) -> usize {
        self.columns.len()
    }

    /// Whether the row has no columns.
    pub fn is_empty(&self) -> bool {
        self.columns.is_empty()
    }

    /// Reads one column as `T`, by name or by index.
    ///
    /// Fails if the column is missing, if `T` does not read the column's
    /// type, or if the value does not decode.
    ///
    /// ```no_run
    /// # async fn example(client: &chx::Client) -> chx::Result<()> {
    /// let row = chx::query("SELECT id, note FROM trades LIMIT 1").fetch_one(client).await?;
    /// let id: u64 = row.try_get("id")?;
    /// let note: Option<String> = row.try_get(1)?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn try_get<T: Decode, I: ColumnIndex>(&self, index: I) -> Result<T> {
        let i = index.index(&self.columns)?;
        let column = &self.columns[i];
        if !T::accepts(&column.ty) {
            return Err(Error::Decode(format!(
                "column {:?} is {}, which cannot be read as {}",
                column.name,
                column.ty,
                std::any::type_name::<T>()
            )));
        }
        let bytes = &self.body[self.offsets[i]..self.offsets[i + 1]];
        let mut reader = Reader::new(bytes);
        let value = T::decode(&mut reader, &column.ty)
            .map_err(|err| Error::Decode(format!("column {:?}: {err}", column.name)))?;
        if !reader.is_empty() {
            return Err(Error::Internal(format!(
                "column {:?} decoded short: {} of {} bytes read",
                column.name,
                reader.position(),
                bytes.len()
            )));
        }
        Ok(value)
    }
}

/// How a column is addressed in [`Row::try_get`]: a position or a name.
pub trait ColumnIndex {
    /// The position of the column this refers to, or
    /// [`Error::ColumnNotFound`].
    fn index(&self, columns: &[Column]) -> Result<usize>;
}

impl ColumnIndex for usize {
    fn index(&self, columns: &[Column]) -> Result<usize> {
        if *self < columns.len() {
            Ok(*self)
        } else {
            Err(Error::ColumnNotFound(format!(
                "index {self} in a row of {} columns",
                columns.len()
            )))
        }
    }
}

impl ColumnIndex for &str {
    fn index(&self, columns: &[Column]) -> Result<usize> {
        columns
            .iter()
            .position(|column| column.name == *self)
            .ok_or_else(|| Error::ColumnNotFound((*self).to_string()))
    }
}

/// A type built from one row. Derive it with `#[derive(FromRow)]`, which reads
/// each field by its column name.
///
/// Written by hand, it is one [`Row::try_get`] per field:
///
/// ```
/// use chx::{FromRow, Result, Row};
///
/// struct Trade {
///     id: u64,
///     venue: String,
/// }
///
/// impl FromRow for Trade {
///     fn from_row(row: &Row) -> Result<Self> {
///         Ok(Trade {
///             id: row.try_get("id")?,
///             venue: row.try_get("venue")?,
///         })
///     }
/// }
/// ```
///
/// Tuples of up to twelve [`Decode`] types implement it by position, and
/// [`Row`] implements it as itself.
pub trait FromRow: Sized {
    /// Builds `Self` from `row`.
    fn from_row(row: &Row) -> Result<Self>;
}

impl FromRow for Row {
    fn from_row(row: &Row) -> Result<Self> {
        Ok(row.clone())
    }
}

macro_rules! tuples {
    ($(($($name:ident $index:tt),+);)*) => {$(
        /// Columns by position.
        impl<$($name: Decode),+> FromRow for ($($name,)+) {
            fn from_row(row: &Row) -> Result<Self> {
                Ok(($(row.try_get::<$name, _>($index)?,)+))
            }
        }
    )*};
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

/// The header of a `RowBinaryWithNamesAndTypes` body: a count, the names,
/// then the types.
pub fn read_header(reader: &mut Reader<'_>) -> Result<Vec<Column>> {
    let count = reader.length()?;
    let mut names = Vec::with_capacity(count);
    for _ in 0..count {
        names.push(reader.string()?);
    }
    let mut columns = Vec::with_capacity(count);
    for name in names {
        let text = reader.string()?;
        let ty =
            Type::parse(&text).map_err(|err| Error::Type(format!("column {name:?}: {err}")))?;
        columns.push(Column { name, ty });
    }
    Ok(columns)
}

/// A decoded body: its columns and its rows.
#[derive(Debug, Clone)]
pub struct RowSet {
    /// The columns from the header, present even when there are no rows.
    pub columns: Arc<[Column]>,
    /// The rows, in the order the server sent them.
    pub rows: Vec<Row>,
}

/// Cuts a whole `RowBinaryWithNamesAndTypes` body into rows.
pub fn decode_body(body: Vec<u8>) -> Result<RowSet> {
    let mut reader = Reader::new(&body);
    let columns: Arc<[Column]> = read_header(&mut reader)?.into();
    let mut row_offsets = Vec::new();
    while !reader.is_empty() {
        let mut offsets = Vec::with_capacity(columns.len() + 1);
        offsets.push(reader.position());
        for column in columns.iter() {
            skip(&mut reader, &column.ty)
                .map_err(|err| Error::Decode(format!("column {:?}: {err}", column.name)))?;
            offsets.push(reader.position());
        }
        row_offsets.push(offsets);
    }
    let body = Arc::new(body);
    let rows = row_offsets
        .into_iter()
        .map(|offsets| Row {
            columns: Arc::clone(&columns),
            body: Arc::clone(&body),
            offsets,
        })
        .collect();
    Ok(RowSet { columns, rows })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two columns, `id UInt8` and `name String`, with two rows.
    fn body() -> Vec<u8> {
        let mut body = vec![2];
        for name in ["id", "name"] {
            body.push(name.len() as u8);
            body.extend_from_slice(name.as_bytes());
        }
        for ty in ["UInt8", "String"] {
            body.push(ty.len() as u8);
            body.extend_from_slice(ty.as_bytes());
        }
        body.extend_from_slice(b"\x01\x03one");
        body.extend_from_slice(b"\x02\x03two");
        body
    }

    #[test]
    fn a_body_is_cut_into_rows_and_columns() {
        let set = decode_body(body()).unwrap();
        assert_eq!(set.columns.len(), 2);
        assert_eq!(set.columns[1].name, "name");
        assert_eq!(set.columns[1].ty, Type::String);
        assert_eq!(set.rows.len(), 2);

        let second = &set.rows[1];
        assert_eq!(second.try_get::<u8, _>("id").unwrap(), 2);
        assert_eq!(second.try_get::<String, _>(1).unwrap(), "two");
        assert_eq!(
            <(u8, String)>::from_row(second).unwrap(),
            (2, "two".to_string())
        );
    }

    #[test]
    fn an_empty_result_has_columns_and_no_rows() {
        let set = decode_body(body()[..body().len() - 10].to_vec()).unwrap();
        assert_eq!(set.columns.len(), 2);
        assert!(set.rows.is_empty());
    }

    #[test]
    fn misreads_are_named() {
        let set = decode_body(body()).unwrap();
        let row = &set.rows[0];

        let err = row.try_get::<String, _>("id").unwrap_err();
        assert!(err.to_string().contains("\"id\" is UInt8"), "{err}");
        let err = row.try_get::<u8, _>("missing").unwrap_err();
        assert!(matches!(err, Error::ColumnNotFound(_)), "{err}");
        let err = row.try_get::<u8, _>(5).unwrap_err();
        assert!(matches!(err, Error::ColumnNotFound(_)), "{err}");
    }

    #[test]
    fn a_truncated_row_is_an_error() {
        let mut truncated = body();
        truncated.pop();
        let err = decode_body(truncated).unwrap_err();
        assert!(err.to_string().contains("\"name\""), "{err}");
    }

    #[test]
    fn an_unparseable_type_in_the_header_is_a_type_error() {
        let mut body = vec![1, 1, b'x', 7];
        body.extend_from_slice(b"UInt8()");
        let err = decode_body(body).unwrap_err();
        assert!(matches!(err, Error::Type(_)), "{err}");
    }

    #[test]
    fn a_row_knows_its_shape_and_reads_as_itself() {
        let set = decode_body(body()).unwrap();
        let row = &set.rows[0];

        assert_eq!(row.len(), 2);
        assert!(!row.is_empty());
        assert_eq!(row.columns()[0].name, "id");
        let same = Row::from_row(row).unwrap();
        assert_eq!(same.try_get::<u8, _>("id").unwrap(), 1);

        let empty = decode_body(vec![0]).unwrap();
        assert!(empty.columns.is_empty());
        assert!(empty.rows.is_empty());
    }

    #[test]
    fn a_value_that_does_not_decode_names_its_column() {
        // `kind Enum8('a' = 1)` holding the number 9, which has no name.
        let mut body = vec![1, 4];
        body.extend_from_slice(b"kind");
        let ty = "Enum8('a' = 1)";
        body.push(ty.len() as u8);
        body.extend_from_slice(ty.as_bytes());
        body.push(9);

        let set = decode_body(body).unwrap();
        let err = set.rows[0].try_get::<String, _>("kind").unwrap_err();

        assert!(err.to_string().contains("column \"kind\""), "{err}");
        assert!(err.to_string().contains("no variant numbered 9"), "{err}");
        // The number itself is still readable.
        assert_eq!(set.rows[0].try_get::<i8, _>("kind").unwrap(), 9);
    }

    #[test]
    fn a_tuple_wider_than_the_row_is_a_missing_column() {
        let set = decode_body(body()).unwrap();
        let err = <(u8, String, u8)>::from_row(&set.rows[0]).unwrap_err();
        assert!(matches!(err, Error::ColumnNotFound(_)), "{err}");
    }

    #[test]
    fn a_header_that_is_cut_short_is_an_error() {
        assert!(decode_body(vec![]).is_err());
        assert!(decode_body(vec![1]).is_err());
        assert!(decode_body(vec![1, 2, b'i', b'd']).is_err());
    }
}
