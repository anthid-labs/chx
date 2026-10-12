//! Running a query: parameters in, rows out.
//!
//! [`query`] takes SQL with `{name:Type}` parameters, binds values to them by
//! name, and fetches the result as [`Row`]s, as a type that is [`FromRow`],
//! or as a single column. Parameters are checked against the SQL before
//! anything is sent: a name the query does not declare, a Rust type that
//! cannot be written as the declared type, or a parameter left unbound is an
//! error here, not a server round trip.
//!
//! The checked macros build on this: `query!` is [`query_as`] with the
//! result struct generated and [`Query::expect_columns`] set from the
//! compile-time describe.

use std::marker::PhantomData;

use chx_types::params::{self, Param};

use crate::client::Client;
use crate::decode::Decode;
use crate::encode::Encode;
use crate::error::{Error, Result};
use crate::row::{FromRow, Row, RowSet, decode_body};

const FORMAT: &str = "RowBinaryWithNamesAndTypes";

/// A query with its parameters, read as untyped [`Row`]s.
///
/// ```no_run
/// # async fn example(client: &chx::Client) -> chx::Result<()> {
/// let rows = chx::query("SELECT id, venue FROM trades WHERE id > {min:UInt64}")
///     .bind("min", &100u64)
///     .fetch_all(client)
///     .await?;
/// for row in &rows {
///     let id: u64 = row.try_get("id")?;
///     let venue: String = row.try_get("venue")?;
///     println!("{id} {venue}");
/// }
///
/// // A statement that returns nothing.
/// chx::query("ALTER TABLE trades DELETE WHERE id = {id:UInt64}")
///     .bind("id", &7u64)
///     .execute(client)
///     .await?;
/// # Ok(())
/// # }
/// ```
pub fn query(sql: impl Into<String>) -> Query {
    Query::new(sql)
}

/// A query whose rows are read as `O`: a `#[derive(FromRow)]` struct, or a
/// tuple by position.
///
/// ```no_run
/// # async fn example(client: &chx::Client) -> chx::Result<()> {
/// #[derive(chx::FromRow)]
/// struct Trade {
///     id: u64,
///     venue: String,
/// }
///
/// let trades: Vec<Trade> = chx::query_as("SELECT id, venue FROM trades WHERE venue = {v:String}")
///     .bind("v", "NYSE")
///     .fetch_all(client)
///     .await?;
///
/// let pairs: Vec<(u64, String)> = chx::query_as("SELECT id, venue FROM trades")
///     .fetch_all(client)
///     .await?;
/// # Ok(())
/// # }
/// ```
pub fn query_as<O: FromRow>(sql: impl Into<String>) -> QueryAs<O> {
    Query::new(sql).try_map(O::from_row)
}

/// A query whose first column is read as `T`.
///
/// ```no_run
/// # async fn example(client: &chx::Client) -> chx::Result<()> {
/// let count: u64 = chx::query_scalar("SELECT count() FROM trades")
///     .fetch_one(client)
///     .await?;
/// let venues: Vec<String> = chx::query_scalar("SELECT DISTINCT venue FROM trades")
///     .fetch_all(client)
///     .await?;
/// # Ok(())
/// # }
/// ```
pub fn query_scalar<T: Decode>(sql: impl Into<String>) -> QueryScalar<T> {
    Query::new(sql).try_map(|row: &Row| row.try_get::<T, _>(0))
}

/// SQL with its parameters bound.
#[derive(Debug)]
pub struct Query {
    sql: String,
    declared: Vec<Param>,
    /// `(name, text)`, in binding order.
    bound: Vec<(String, String)>,
    expected: Option<&'static [&'static str]>,
    /// The first thing that went wrong while building. Binding is infallible
    /// so it chains; the error surfaces when the query runs.
    error: Option<Error>,
}

impl Query {
    /// Parses the parameters `sql` declares. A malformed `{name:Type}` is
    /// reported when the query runs.
    pub fn new(sql: impl Into<String>) -> Self {
        let sql = sql.into();
        let (declared, error) = match params::parse(&sql) {
            Ok(declared) => (declared, None),
            Err(err) => (Vec::new(), Some(err.into())),
        };
        Self {
            sql,
            declared,
            bound: Vec::new(),
            expected: None,
            error,
        }
    }

    /// The SQL as given.
    pub fn sql(&self) -> &str {
        &self.sql
    }

    /// The parameters the SQL declares, in order of first appearance.
    pub fn params(&self) -> &[Param] {
        &self.declared
    }

    /// Binds `value` to the `{name:Type}` parameter.
    ///
    /// The value is written to text now, as the declared type. A name the
    /// query does not declare, or a value that cannot be written as its type,
    /// fails the query when it runs.
    ///
    /// ```no_run
    /// # async fn example(client: &chx::Client) -> chx::Result<()> {
    /// use std::collections::BTreeMap;
    ///
    /// let rows = chx::query(
    ///     "SELECT * FROM trades \
    ///      WHERE venue IN {venues:Array(String)} \
    ///        AND note = {note:Nullable(String)} \
    ///        AND tags = {tags:Map(String, UInt8)}",
    /// )
    /// .bind("venues", &["NYSE", "ARCA"][..])
    /// .bind("note", &None::<String>)
    /// .bind("tags", &BTreeMap::from([("hot", 1u8)]))
    /// .fetch_all(client)
    /// .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn bind<T: Encode + ?Sized>(mut self, name: &str, value: &T) -> Self {
        if self.error.is_some() {
            return self;
        }
        let Some(param) = self.declared.iter().find(|param| param.name == name) else {
            self.error = Some(Error::Parameter(format!(
                "the query declares no parameter named {name:?}"
            )));
            return self;
        };
        if self.bound.iter().any(|(bound, _)| bound == name) {
            self.error = Some(Error::Parameter(format!("{name:?} is bound twice")));
            return self;
        }
        if !T::accepts(&param.ty) {
            self.error = Some(Error::Encode(format!(
                "parameter {name:?} is {}, which {} cannot be written as",
                param.ty,
                std::any::type_name::<T>()
            )));
            return self;
        }
        let mut text = String::new();
        match value.encode_text(&param.ty, &mut text, false) {
            Ok(()) => self.bound.push((name.to_string(), text)),
            Err(err) => {
                self.error = Some(Error::Encode(format!("parameter {name:?}: {err}")));
            }
        }
        self
    }

    /// Requires the result set to have exactly these columns, in this order,
    /// before any row is read. The checked macros set it from the columns the
    /// query was compiled against, so a schema that drifted since `chx
    /// prepare` fails with [`Error::Schema`] rather than a misread column.
    pub fn expect_columns(mut self, names: &'static [&'static str]) -> Self {
        self.expected = Some(names);
        self
    }

    /// Reads each row with `map`.
    pub fn try_map<O, F: FnMut(&Row) -> Result<O>>(self, map: F) -> Map<O, F> {
        Map {
            query: self,
            map,
            output: PhantomData,
        }
    }

    fn check_bound(&self) -> Result<()> {
        if let Some(error) = &self.error {
            return Err(Error::Parameter(error.to_string()));
        }
        let missing: Vec<&str> = self
            .declared
            .iter()
            .filter(|param| !self.bound.iter().any(|(name, _)| *name == param.name))
            .map(|param| param.name.as_str())
            .collect();
        if !missing.is_empty() {
            return Err(Error::Parameter(format!(
                "not bound: {}",
                missing.join(", ")
            )));
        }
        Ok(())
    }

    /// Runs the query and discards any result.
    pub async fn execute(self, client: &Client) -> Result<()> {
        self.check_bound()?;
        client.query_bytes(&self.sql, &self.bound, None).await?;
        Ok(())
    }

    async fn run(self, client: &Client) -> Result<RowSet> {
        self.check_bound()?;
        let body = client
            .query_bytes(&self.sql, &self.bound, Some(FORMAT))
            .await?;
        let set = decode_body(body)?;
        if let Some(expected) = self.expected {
            check_columns(expected, &set)?;
        }
        Ok(set)
    }

    /// Runs the query and returns every row.
    pub async fn fetch_all(self, client: &Client) -> Result<Vec<Row>> {
        Ok(self.run(client).await?.rows)
    }

    /// Runs the query and returns the first row, or `None` if there is none.
    /// Later rows are discarded; add a `LIMIT 1` to avoid fetching them.
    pub async fn fetch_optional(self, client: &Client) -> Result<Option<Row>> {
        Ok(self.run(client).await?.rows.into_iter().next())
    }

    /// Runs the query and returns the first row, or [`Error::RowNotFound`].
    pub async fn fetch_one(self, client: &Client) -> Result<Row> {
        self.fetch_optional(client).await?.ok_or(Error::RowNotFound)
    }
}

fn check_columns(expected: &[&str], set: &RowSet) -> Result<()> {
    let actual: Vec<&str> = set.columns.iter().map(|c| c.name.as_str()).collect();
    if actual != expected {
        return Err(Error::Schema(format!(
            "compiled against [{}], the server returned [{}]",
            expected.join(", "),
            actual.join(", ")
        )));
    }
    Ok(())
}

/// A [`Query`] whose rows are read with a function.
pub struct Map<O, F> {
    query: Query,
    map: F,
    output: PhantomData<fn() -> O>,
}

/// A query read as a [`FromRow`] type.
pub type QueryAs<O> = Map<O, fn(&Row) -> Result<O>>;

/// A query read as its first column.
pub type QueryScalar<T> = Map<T, fn(&Row) -> Result<T>>;

impl<O, F: FnMut(&Row) -> Result<O>> Map<O, F> {
    /// See [`Query::bind`].
    pub fn bind<T: Encode + ?Sized>(mut self, name: &str, value: &T) -> Self {
        self.query = self.query.bind(name, value);
        self
    }

    /// See [`Query::expect_columns`].
    pub fn expect_columns(mut self, names: &'static [&'static str]) -> Self {
        self.query = self.query.expect_columns(names);
        self
    }

    /// The SQL as given.
    pub fn sql(&self) -> &str {
        self.query.sql()
    }

    /// Runs the query and discards any result. For a `query!` that is an
    /// `INSERT` or DDL.
    pub async fn execute(self, client: &Client) -> Result<()> {
        self.query.execute(client).await
    }

    /// Runs the query and returns every row, mapped.
    pub async fn fetch_all(mut self, client: &Client) -> Result<Vec<O>> {
        let set = self.query.run(client).await?;
        set.rows.iter().map(&mut self.map).collect()
    }

    /// Runs the query and returns the first row, mapped, or `None`.
    pub async fn fetch_optional(mut self, client: &Client) -> Result<Option<O>> {
        let set = self.query.run(client).await?;
        set.rows.first().map(&mut self.map).transpose()
    }

    /// Runs the query and returns the first row, mapped, or
    /// [`Error::RowNotFound`].
    pub async fn fetch_one(self, client: &Client) -> Result<O> {
        self.fetch_optional(client).await?.ok_or(Error::RowNotFound)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parameters_come_from_the_sql() {
        let q = query("SELECT * FROM t WHERE a = {a:UInt64} AND b = {b:String}");
        let names: Vec<_> = q.params().iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["a", "b"]);
    }

    #[test]
    fn binding_writes_the_text_form() {
        let q = query("SELECT {a:UInt64}, {b:Array(String)}, {c:Nullable(String)}")
            .bind("a", &7u64)
            .bind("b", &["x"][..])
            .bind("c", &None::<String>);
        assert!(q.error.is_none(), "{:?}", q.error);
        assert_eq!(
            q.bound,
            [
                ("a".to_string(), "7".to_string()),
                ("b".to_string(), "['x']".to_string()),
                ("c".to_string(), "\\N".to_string()),
            ]
        );
        assert!(q.check_bound().is_ok());
    }

    #[test]
    fn an_undeclared_name_fails_before_running() {
        let q = query("SELECT {a:UInt64}").bind("b", &1u64);
        let err = q.check_bound().unwrap_err();
        assert!(matches!(err, Error::Parameter(_)), "{err}");
        assert!(err.to_string().contains("\"b\""), "{err}");
    }

    #[test]
    fn a_wrong_type_fails_before_running() {
        let q = query("SELECT {a:UInt64}").bind("a", "seven");
        let err = q.check_bound().unwrap_err();
        assert!(err.to_string().contains("UInt64"), "{err}");
    }

    #[test]
    fn an_unbound_parameter_fails_before_running() {
        let q = query("SELECT {a:UInt64}, {b:UInt64}").bind("a", &1u64);
        let err = q.check_bound().unwrap_err();
        assert!(err.to_string().contains("not bound: b"), "{err}");
    }

    #[test]
    fn binding_twice_is_refused() {
        let q = query("SELECT {a:UInt64}").bind("a", &1u64).bind("a", &2u64);
        assert!(q.check_bound().is_err());
    }

    #[test]
    fn a_malformed_parameter_type_is_reported() {
        let q = query("SELECT {a:Array()}");
        assert!(q.check_bound().is_err());
    }

    use crate::testing::{Fake, Response, row_binary};

    /// `id UInt8, name String` with the rows (1, "one") and (2, "two").
    fn two_rows() -> Response {
        Response::ok(row_binary(
            &[("id", "UInt8"), ("name", "String")],
            b"\x01\x03one\x02\x03two",
        ))
    }

    fn no_rows() -> Response {
        Response::ok(row_binary(&[("id", "UInt8"), ("name", "String")], b""))
    }

    #[tokio::test]
    async fn fetch_all_asks_for_rowbinary_and_sends_the_bound_parameters() {
        let fake = Fake::always(two_rows());

        let rows = query("SELECT id, name FROM t WHERE id > {min:UInt8}")
            .bind("min", &0u8)
            .fetch_all(&fake.client())
            .await
            .unwrap();

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].try_get::<String, _>("name").unwrap(), "two");
        let request = fake.last();
        assert_eq!(request.param("param_min"), Some("0"));
        assert_eq!(
            request.param("default_format"),
            Some("RowBinaryWithNamesAndTypes")
        );
    }

    #[tokio::test]
    async fn fetch_one_and_optional_take_the_first_row() {
        let fake = Fake::always(two_rows());
        let client = fake.client();

        let one = query("SELECT 1").fetch_one(&client).await.unwrap();
        assert_eq!(one.try_get::<u8, _>(0).unwrap(), 1);
        let optional = query("SELECT 1").fetch_optional(&client).await.unwrap();
        assert!(optional.is_some());
    }

    #[tokio::test]
    async fn an_empty_result_is_none_or_row_not_found() {
        let fake = Fake::always(no_rows());
        let client = fake.client();

        assert!(
            query("SELECT 1")
                .fetch_all(&client)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            query("SELECT 1")
                .fetch_optional(&client)
                .await
                .unwrap()
                .is_none()
        );
        let err = query("SELECT 1").fetch_one(&client).await.unwrap_err();
        assert!(matches!(err, Error::RowNotFound), "{err}");
    }

    #[tokio::test]
    async fn query_as_reads_tuples_and_query_scalar_the_first_column() {
        let fake = Fake::always(two_rows());
        let client = fake.client();

        let pairs: Vec<(u8, String)> = query_as("SELECT id, name FROM t")
            .fetch_all(&client)
            .await
            .unwrap();
        assert_eq!(pairs, [(1, "one".to_string()), (2, "two".to_string())]);

        let first: (u8, String) = query_as("SELECT id, name FROM t")
            .fetch_one(&client)
            .await
            .unwrap();
        assert_eq!(first.0, 1);

        let ids: Vec<u8> = query_scalar("SELECT id FROM t")
            .fetch_all(&client)
            .await
            .unwrap();
        assert_eq!(ids, [1, 2]);

        let id: Option<u8> = query_scalar("SELECT id FROM t")
            .fetch_optional(&client)
            .await
            .unwrap();
        assert_eq!(id, Some(1));
    }

    #[tokio::test]
    async fn a_mapped_query_on_no_rows_is_none_or_row_not_found() {
        let fake = Fake::always(no_rows());
        let client = fake.client();

        let none: Option<(u8, String)> =
            query_as("SELECT 1").fetch_optional(&client).await.unwrap();
        assert!(none.is_none());
        let err = query_scalar::<u8>("SELECT 1")
            .fetch_one(&client)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::RowNotFound), "{err}");
    }

    #[tokio::test]
    async fn a_mapped_query_binds_checks_columns_and_executes() {
        let fake = Fake::always(two_rows());
        let client = fake.client();

        let mapped = query_as::<(u8, String)>("SELECT id, name FROM t WHERE id = {id:UInt8}")
            .bind("id", &1u8)
            .expect_columns(&["id", "name"]);
        assert_eq!(mapped.sql(), "SELECT id, name FROM t WHERE id = {id:UInt8}");
        assert_eq!(mapped.fetch_all(&client).await.unwrap().len(), 2);
        assert_eq!(fake.last().param("param_id"), Some("1"));

        query_as::<(u8, String)>("INSERT INTO t VALUES ({id:UInt8}, 'x')")
            .bind("id", &3u8)
            .execute(&client)
            .await
            .unwrap();
        // `execute` asks for no format: there is nothing to read.
        assert_eq!(fake.last().param("default_format"), None);
    }

    #[tokio::test]
    async fn a_mapping_error_names_the_column() {
        let fake = Fake::always(two_rows());

        let err = query_as::<(String, String)>("SELECT id, name FROM t")
            .fetch_all(&fake.client())
            .await
            .unwrap_err();

        assert!(err.to_string().contains("\"id\" is UInt8"), "{err}");
    }

    #[tokio::test]
    async fn expected_columns_are_checked_before_any_row_is_read() {
        let fake = Fake::always(two_rows());
        let client = fake.client();

        query("SELECT id, name FROM t")
            .expect_columns(&["id", "name"])
            .fetch_all(&client)
            .await
            .unwrap();

        for wrong in [&["id"][..], &["name", "id"], &["id", "name", "extra"]] {
            // A `'static` list is what the macros pass; leak one for the test.
            let wrong: &'static [&'static str] = Vec::leak(wrong.to_vec());
            let err = query("SELECT id, name FROM t")
                .expect_columns(wrong)
                .fetch_all(&client)
                .await
                .unwrap_err();
            assert!(matches!(err, Error::Schema(_)), "{err}");
            assert!(err.to_string().contains("[id, name]"), "{err}");
        }
    }

    #[tokio::test]
    async fn execute_sends_the_statement_and_its_parameters() {
        let fake = Fake::always(Response::empty());

        query("ALTER TABLE t DELETE WHERE id = {id:UInt64}")
            .bind("id", &7u64)
            .execute(&fake.client())
            .await
            .unwrap();

        let request = fake.last();
        assert_eq!(request.sql(), "ALTER TABLE t DELETE WHERE id = {id:UInt64}");
        assert_eq!(request.param("param_id"), Some("7"));
    }

    #[tokio::test]
    async fn nothing_is_sent_for_a_query_that_is_wrong_before_it_runs() {
        let fake = Fake::always(two_rows());
        let client = fake.client();

        assert!(query("SELECT {a:UInt8}").fetch_all(&client).await.is_err());
        assert!(query("SELECT {a:UInt8}").execute(&client).await.is_err());
        assert!(
            query("SELECT {a:UInt8}")
                .bind("a", "text")
                .fetch_one(&client)
                .await
                .is_err()
        );

        assert!(fake.requests().is_empty());
    }

    #[tokio::test]
    async fn a_server_error_and_a_malformed_body_are_passed_through() {
        let failing = Fake::always(Response::exception(60, "no such table"));
        let err = query("SELECT 1")
            .fetch_all(&failing.client())
            .await
            .unwrap_err();
        assert!(
            matches!(err, Error::ClickHouse { code: Some(60), .. }),
            "{err}"
        );

        let garbage = Fake::always(Response::ok(vec![2, 1, b'a']));
        let err = query("SELECT 1")
            .fetch_all(&garbage.client())
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Decode(_)), "{err}");
    }

    #[test]
    fn a_value_that_fails_to_encode_names_the_parameter() {
        let q = query("SELECT {e:Enum8('a' = 1)}").bind("e", "missing");
        let err = q.check_bound().unwrap_err();
        assert!(err.to_string().contains("\"e\""), "{err}");
        assert!(err.to_string().contains("missing"), "{err}");
    }

    #[test]
    fn a_failed_bind_stops_later_binds_from_hiding_it() {
        let q = query("SELECT {a:UInt8}, {b:UInt8}")
            .bind("nope", &1u8)
            .bind("a", &1u8)
            .bind("b", &2u8);
        let err = q.check_bound().unwrap_err();
        assert!(err.to_string().contains("\"nope\""), "{err}");
        assert_eq!(q.sql(), "SELECT {a:UInt8}, {b:UInt8}");
    }
}
