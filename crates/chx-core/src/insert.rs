//! Bulk inserts: `INSERT ... FORMAT RowBinary` with the rows in the body.
//!
//! One request per batch, however many rows, and the server parses nothing:
//! RowBinary is the column types laid end to end. Those types come from a
//! `DESCRIBE TABLE` when the insert is opened, so each value is encoded as
//! the column really is, and a value that does not fit its column is refused
//! on `write`, before anything is sent.

use crate::client::Client;
use crate::encode::Encode;
use crate::error::{Error, Result};
use crate::query::query_as;
use crate::row::Column;

/// The columns of a table, from `DESCRIBE TABLE`, in table order.
pub async fn describe_table(client: &Client, table: &str) -> Result<Vec<Column>> {
    let rows: Vec<(String, String)> = query_as(format!("DESCRIBE TABLE {table}"))
        .fetch_all(client)
        .await?;
    rows.into_iter()
        .map(|(name, ty)| {
            Ok(Column {
                ty: ty
                    .parse()
                    .map_err(|err| Error::Type(format!("{table}.{name}: {err}")))?,
                name,
            })
        })
        .collect()
}

impl Client {
    /// Opens a bulk insert into `columns` of `table`. One `DESCRIBE TABLE`
    /// round trip; nothing is inserted until [`Insert::flush`] or
    /// [`Insert::end`].
    ///
    /// ```no_run
    /// # async fn example(client: &chx::Client) -> chx::Result<()> {
    /// use chx::types::Decimal;
    ///
    /// let mut insert = client.insert("trades", &["id", "venue", "px", "note"]).await?;
    /// for id in 0..100_000u64 {
    ///     // One value per column, in the order named above.
    ///     insert.write((id, "NYSE", Decimal::new(101_25, 2), None::<&str>))?;
    ///     // Bound the memory a long stream holds.
    ///     if insert.buffered_bytes() > 8 << 20 {
    ///         insert.flush().await?;
    ///     }
    /// }
    /// let rows = insert.end().await?;
    /// assert_eq!(rows, 100_000);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn insert(&self, table: &str, columns: &[&str]) -> Result<Insert> {
        let described = describe_table(self, table).await?;
        let columns = columns
            .iter()
            .map(|name| {
                described
                    .iter()
                    .find(|column| column.name == *name)
                    .cloned()
                    .ok_or_else(|| Error::ColumnNotFound(format!("{table} has no column {name:?}")))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Insert {
            client: self.clone(),
            table: table.to_string(),
            columns,
            buf: Vec::new(),
            buffered: 0,
            sent: 0,
        })
    }
}

/// A batch of rows on their way to one table.
#[derive(Debug)]
pub struct Insert {
    client: Client,
    table: String,
    columns: Vec<Column>,
    buf: Vec<u8>,
    buffered: usize,
    sent: usize,
}

impl Insert {
    /// The columns being written, in the order rows must supply them.
    pub fn columns(&self) -> &[Column] {
        &self.columns
    }

    /// Rows written and not yet sent.
    pub fn buffered(&self) -> usize {
        self.buffered
    }

    /// Bytes written and not yet sent. The usual trigger for a
    /// [`flush`](Self::flush) in a long stream.
    pub fn buffered_bytes(&self) -> usize {
        self.buf.len()
    }

    /// Encodes one row into the batch. A row that does not fit its columns
    /// leaves the batch as it was.
    pub fn write<R: IntoRow>(&mut self, row: R) -> Result<()> {
        let start = self.buf.len();
        match row.encode_row(&self.columns, &mut self.buf) {
            Ok(()) => {
                self.buffered += 1;
                Ok(())
            }
            Err(err) => {
                self.buf.truncate(start);
                Err(err)
            }
        }
    }

    /// Sends the buffered rows in one request and returns how many there
    /// were. Nothing is sent for an empty batch.
    pub async fn flush(&mut self) -> Result<usize> {
        if self.buffered == 0 {
            return Ok(0);
        }
        let sql = self.statement();
        let body = std::mem::take(&mut self.buf);
        let rows = std::mem::take(&mut self.buffered);
        match self.client.insert_bytes(&sql, body).await {
            Ok(()) => {
                self.sent += rows;
                Ok(rows)
            }
            Err(err) => {
                // The rows are gone with the body. Say how many, since the
                // caller is the one who can resend them.
                Err(Error::Encode(format!(
                    "insert of {rows} rows into {} failed: {err}",
                    self.table
                )))
            }
        }
    }

    /// Flushes what is left and returns the total rows sent by this insert.
    pub async fn end(mut self) -> Result<usize> {
        self.flush().await?;
        Ok(self.sent)
    }

    fn statement(&self) -> String {
        let columns: Vec<String> = self
            .columns
            .iter()
            .map(|column| format!("`{}`", column.name.replace('`', "\\`")))
            .collect();
        format!(
            "INSERT INTO {} ({}) FORMAT RowBinary",
            self.table,
            columns.join(", ")
        )
    }
}

/// One row of an insert: a value per column, in column order. Tuples of
/// up to sixteen [`Encode`] values implement it, and so does a reference to
/// one.
///
/// Implement it for a struct to write it directly:
///
/// ```
/// use chx::{Column, Encode, IntoRow, Result};
///
/// struct Trade {
///     id: u64,
///     venue: String,
/// }
///
/// impl IntoRow for Trade {
///     fn encode_row(&self, columns: &[Column], buf: &mut Vec<u8>) -> Result<()> {
///         // Borrow the fields as a tuple and let it do the checking.
///         (self.id, &self.venue).encode_row(columns, buf)
///     }
/// }
/// # let _ = <u64 as Encode>::accepts;
/// ```
pub trait IntoRow {
    /// Appends the RowBinary form of this row, one value per entry of
    /// `columns`, in order.
    fn encode_row(&self, columns: &[Column], buf: &mut Vec<u8>) -> Result<()>;
}

impl<R: IntoRow + ?Sized> IntoRow for &R {
    fn encode_row(&self, columns: &[Column], buf: &mut Vec<u8>) -> Result<()> {
        (**self).encode_row(columns, buf)
    }
}

fn encode_column<T: Encode + ?Sized>(value: &T, column: &Column, buf: &mut Vec<u8>) -> Result<()> {
    if !T::accepts(&column.ty) {
        return Err(Error::Encode(format!(
            "column {:?} is {}, which {} cannot be written as",
            column.name,
            column.ty,
            std::any::type_name::<T>()
        )));
    }
    value
        .encode(&column.ty, buf)
        .map_err(|err| Error::Encode(format!("column {:?}: {err}", column.name)))
}

macro_rules! tuples {
    ($(($($name:ident $index:tt),+);)*) => {$(
        impl<$($name: Encode),+> IntoRow for ($($name,)+) {
            fn encode_row(&self, columns: &[Column], buf: &mut Vec<u8>) -> Result<()> {
                let width = tuples!(@count $($name)+);
                if columns.len() != width {
                    return Err(Error::Encode(format!(
                        "row has {width} values for {} columns",
                        columns.len()
                    )));
                }
                $(encode_column(&self.$index, &columns[$index], buf)?;)+
                Ok(())
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
    (A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8, J 9, K 10, L 11, M 12);
    (A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8, J 9, K 10, L 11, M 12, N 13);
    (A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8, J 9, K 10, L 11, M 12, N 13, O 14);
    (A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8, J 9, K 10, L 11, M 12, N 13, O 14, P 15);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Type;

    fn columns() -> Vec<Column> {
        vec![
            Column {
                name: "id".to_string(),
                ty: Type::UInt64,
            },
            Column {
                name: "name".to_string(),
                ty: Type::String,
            },
        ]
    }

    #[test]
    fn a_tuple_row_is_the_columns_laid_end_to_end() {
        let mut buf = Vec::new();
        (7u64, "ab").encode_row(&columns(), &mut buf).unwrap();
        let mut expected = 7u64.to_le_bytes().to_vec();
        expected.extend_from_slice(b"\x02ab");
        assert_eq!(buf, expected);
    }

    #[test]
    fn a_misfitting_row_names_the_column() {
        let mut buf = Vec::new();
        let err = (7u32, "ab").encode_row(&columns(), &mut buf).unwrap_err();
        assert!(err.to_string().contains("\"id\" is UInt64"), "{err}");
        let err = (7u64,).encode_row(&columns(), &mut buf).unwrap_err();
        assert!(err.to_string().contains("1 values for 2 columns"), "{err}");
    }

    #[test]
    fn the_statement_quotes_columns() {
        let insert = Insert {
            client: Client::from_url("http://localhost:8123/db").unwrap(),
            table: "db.trades".to_string(),
            columns: columns(),
            buf: Vec::new(),
            buffered: 0,
            sent: 0,
        };
        assert_eq!(
            insert.statement(),
            "INSERT INTO db.trades (`id`, `name`) FORMAT RowBinary"
        );
    }

    #[test]
    fn a_failed_write_leaves_the_batch_intact() {
        let mut insert = Insert {
            client: Client::from_url("http://localhost:8123/db").unwrap(),
            table: "t".to_string(),
            columns: columns(),
            buf: Vec::new(),
            buffered: 0,
            sent: 0,
        };
        insert.write((1u64, "a")).unwrap();
        let before = insert.buffered_bytes();
        assert!(insert.write((2u64, 5u8)).is_err());
        assert_eq!(insert.buffered_bytes(), before);
        assert_eq!(insert.buffered(), 1);
    }

    use crate::testing::{Fake, Response, row_binary};

    /// Answers `DESCRIBE TABLE` with `id UInt64, name String` and accepts
    /// anything else.
    fn table() -> Fake {
        Fake::start(|request| {
            if request.sql().starts_with("DESCRIBE TABLE") {
                Response::ok(row_binary(
                    &[("name", "String"), ("type", "String")],
                    b"\x02id\x06UInt64\x04name\x06String",
                ))
            } else {
                Response::empty()
            }
        })
    }

    #[tokio::test]
    async fn describe_table_reads_names_and_types() {
        let fake = table();

        let columns = describe_table(&fake.client(), "trades").await.unwrap();

        assert_eq!(columns, columns_fixture());
        assert_eq!(fake.last().sql(), "DESCRIBE TABLE trades");
    }

    fn columns_fixture() -> Vec<Column> {
        columns()
    }

    #[tokio::test]
    async fn describe_table_refuses_a_type_it_cannot_parse() {
        let fake = Fake::always(Response::ok(row_binary(
            &[("name", "String"), ("type", "String")],
            b"\x02id\x07Array()",
        )));

        let err = describe_table(&fake.client(), "trades").await.unwrap_err();

        assert!(matches!(err, Error::Type(_)), "{err}");
        assert!(err.to_string().contains("trades.id"), "{err}");
    }

    #[tokio::test]
    async fn an_insert_takes_columns_in_the_order_asked_for() {
        let fake = table();

        let insert = fake
            .client()
            .insert("trades", &["name", "id"])
            .await
            .unwrap();

        let names: Vec<_> = insert.columns().iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["name", "id"]);
    }

    #[tokio::test]
    async fn an_unknown_column_is_refused_when_the_insert_opens() {
        let fake = table();

        let err = fake
            .client()
            .insert("trades", &["id", "nope"])
            .await
            .unwrap_err();

        assert!(matches!(err, Error::ColumnNotFound(_)), "{err}");
        assert!(err.to_string().contains("\"nope\""), "{err}");
    }

    #[tokio::test]
    async fn flush_sends_one_request_per_batch_and_end_counts_them_all() {
        let fake = table();
        let mut insert = fake
            .client()
            .insert("trades", &["id", "name"])
            .await
            .unwrap();

        insert.write((1u64, "a")).unwrap();
        // A borrowed row, as when writing from a slice of rows.
        let second = (2u64, "b");
        let borrowed = &second;
        insert.write(borrowed).unwrap();
        assert_eq!(insert.buffered(), 2);
        assert_eq!(insert.flush().await.unwrap(), 2);
        assert_eq!(insert.buffered(), 0);
        assert_eq!(insert.buffered_bytes(), 0);

        let request = fake.last();
        assert_eq!(
            request.param("query"),
            Some("INSERT INTO trades (`id`, `name`) FORMAT RowBinary")
        );
        let mut expected = 1u64.to_le_bytes().to_vec();
        expected.extend_from_slice(b"\x01a");
        expected.extend_from_slice(&2u64.to_le_bytes());
        expected.extend_from_slice(b"\x01b");
        assert_eq!(request.body, expected);

        insert.write((3u64, "c")).unwrap();
        assert_eq!(insert.end().await.unwrap(), 3);
        // DESCRIBE, the first batch, the second batch.
        assert_eq!(fake.requests().len(), 3);
    }

    #[tokio::test]
    async fn an_empty_batch_sends_nothing() {
        let fake = table();
        let mut insert = fake.client().insert("trades", &["id"]).await.unwrap();

        assert_eq!(insert.flush().await.unwrap(), 0);
        assert_eq!(insert.end().await.unwrap(), 0);

        // Only the DESCRIBE.
        assert_eq!(fake.requests().len(), 1);
    }

    #[tokio::test]
    async fn a_refused_batch_says_how_many_rows_were_lost() {
        let fake = Fake::start(|request| {
            if request.sql().starts_with("DESCRIBE TABLE") {
                Response::ok(row_binary(
                    &[("name", "String"), ("type", "String")],
                    b"\x02id\x06UInt64",
                ))
            } else {
                Response::exception(252, "Too many parts")
            }
        });
        let mut insert = fake.client().insert("trades", &["id"]).await.unwrap();
        insert.write((1u64,)).unwrap();
        insert.write((2u64,)).unwrap();

        let err = insert.flush().await.unwrap_err();

        assert!(err.to_string().contains("2 rows into trades"), "{err}");
        assert!(err.to_string().contains("Too many parts"), "{err}");
        assert_eq!(insert.buffered(), 0);
    }

    #[test]
    fn a_value_that_does_not_fit_names_the_column() {
        let columns = vec![Column {
            name: "px".to_string(),
            ty: Type::parse("Decimal(9, 2)").unwrap(),
        }];
        let err = (crate::types::Decimal::new(15001, 4),)
            .encode_row(&columns, &mut Vec::new())
            .unwrap_err();
        assert!(err.to_string().contains("column \"px\""), "{err}");
    }

    #[test]
    fn rows_of_every_width_up_to_sixteen_encode() {
        let column = |i: usize| Column {
            name: format!("c{i}"),
            ty: Type::UInt8,
        };
        let columns: Vec<Column> = (0..16).map(column).collect();
        let mut buf = Vec::new();
        (1u8,).encode_row(&columns[..1], &mut buf).unwrap();
        (1u8, 2u8, 3u8).encode_row(&columns[..3], &mut buf).unwrap();
        (1u8, 2u8, 3u8, 4u8, 5u8, 6u8, 7u8, 8u8)
            .encode_row(&columns[..8], &mut buf)
            .unwrap();
        (
            1u8, 2u8, 3u8, 4u8, 5u8, 6u8, 7u8, 8u8, 9u8, 10u8, 11u8, 12u8, 13u8, 14u8, 15u8, 16u8,
        )
            .encode_row(&columns, &mut buf)
            .unwrap();
        assert_eq!(buf.len(), 1 + 3 + 8 + 16);
    }
}
