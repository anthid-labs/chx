//! A minimal ClickHouse client over the HTTP interface.
//!
//! Four operations: run a statement, run a query and read its body as text,
//! run a query with parameters and read its body in a named format, and send
//! an `INSERT` with its data in the body. Everything typed is built on the
//! last two, in [`mod@crate::query`] and [`mod@crate::insert`]. Keeping the client
//! this small is what lets chx stay off the native protocol and work against
//! every deployment, ClickHouse Cloud included.

use chx_types::endpoint::Endpoint;
use reqwest::header::HeaderValue;

use crate::error::{Error, Result};

/// How long an unused connection stays in the pool.
///
/// ClickHouse closes an idle HTTP connection after its `keep_alive_timeout`,
/// which has been as low as three seconds by default. A pooled connection
/// older than that may already be closed by the server, and a request sent
/// on it fails without having run. Dropping idle connections sooner than the
/// server does costs a reconnect after a pause, and removes that failure.
const POOL_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// A connection to one ClickHouse server and database.
///
/// Cheap to clone: clones share one connection pool. Nothing is sent until
/// the first statement.
///
/// ```no_run
/// # async fn example() -> chx::Result<()> {
/// use chx::Client;
///
/// // Settings in the query string apply to every request.
/// let client = Client::from_url(
///     "https://reader:s3cret@clickhouse.internal:8443/analytics?max_execution_time=60",
/// )?;
///
/// client.execute("CREATE TABLE IF NOT EXISTS t (id UInt64) ENGINE = Memory").await?;
/// let version = client.query("SELECT version()").await?;
/// println!("ClickHouse {}", version.trim());
///
/// // The same server and credentials, another database.
/// let staging = client.with_database("staging");
/// assert_eq!(staging.database(), Some("staging"));
/// # Ok(())
/// # }
/// ```
///
/// For typed results and parameters, use [`chx::query`](crate::query()) and
/// its relatives; for loading data, [`Client::insert`].
#[derive(Debug, Clone)]
pub struct Client {
    http: reqwest::Client,
    endpoint: Endpoint,
}

impl Client {
    /// Connects to `http[s]://[user[:password]@]host[:port][/database][?setting=value]`.
    ///
    /// The path, when present, is the database. Any query parameters are
    /// passed through as ClickHouse settings. Nothing is sent until the first
    /// statement: this only parses.
    pub fn from_url(raw: &str) -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .pool_idle_timeout(POOL_IDLE_TIMEOUT)
                .build()
                .map_err(Error::Transport)?,
            endpoint: Endpoint::parse(raw)?,
        })
    }

    /// The database statements run in, or `None` for the user's default.
    pub fn database(&self) -> Option<&str> {
        self.endpoint.database()
    }

    /// The same server with a different database. Credentials and settings
    /// carry over.
    pub fn with_database(&self, database: &str) -> Self {
        Self {
            http: self.http.clone(),
            endpoint: self.endpoint.with_database(database),
        }
    }

    /// Runs one statement and discards any result.
    pub async fn execute(&self, sql: &str) -> Result<()> {
        self.query_bytes(sql, &[], None).await.map(|_| ())
    }

    /// Runs one query and returns its body as text. The caller chooses the
    /// format in the SQL.
    pub async fn query(&self, sql: &str) -> Result<String> {
        let body = self.query_bytes(sql, &[], None).await?;
        String::from_utf8(body).map_err(|err| Error::Decode(format!("body is not UTF-8: {err}")))
    }

    /// Runs `sql` with query parameters bound and returns the raw body.
    ///
    /// `params` are `(name, value)` pairs for the `{name:Type}` placeholders
    /// in the SQL, with the value in the text form the server reads them in;
    /// [`Encode::encode_text`](crate::encode::Encode::encode_text) produces
    /// it. `format` sets `default_format`, which applies when the SQL has no
    /// `FORMAT` clause of its own.
    pub async fn query_bytes(
        &self,
        sql: &str,
        params: &[(String, String)],
        format: Option<&str>,
    ) -> Result<Vec<u8>> {
        let mut url = self.endpoint.url().clone();
        {
            let mut query = url.query_pairs_mut();
            for (name, value) in params {
                query.append_pair(&format!("param_{name}"), value);
            }
            if let Some(format) = format {
                query.append_pair("default_format", format);
            }
        }
        let request = self.http.post(url).body(sql.to_string());
        self.send(request).await
    }

    /// Sends `sql`, an `INSERT ... FORMAT <binary format>`, with `body` as
    /// the data. The statement goes in the URL, which is how the HTTP
    /// interface tells the two apart.
    pub async fn insert_bytes(&self, sql: &str, body: Vec<u8>) -> Result<()> {
        let mut url = self.endpoint.url().clone();
        url.query_pairs_mut().append_pair("query", sql);
        let request = self.http.post(url).body(body);
        self.send(request).await.map(|_| ())
    }

    async fn send(&self, mut request: reqwest::RequestBuilder) -> Result<Vec<u8>> {
        // Headers rather than basic auth or URL parameters, because a URL ends
        // up in proxy logs and ClickHouse's own query_log, and a header does
        // not.
        if let Some(user) = self.endpoint.user() {
            request = request.header("X-ClickHouse-User", header(user)?);
        }
        if let Some(password) = self.endpoint.password() {
            let mut value = header(password)?;
            value.set_sensitive(true);
            request = request.header("X-ClickHouse-Key", value);
        }

        let response = request.send().await.map_err(Error::Transport)?;
        let status = response.status();
        let code = response
            .headers()
            .get("X-ClickHouse-Exception-Code")
            .map(|value| value.to_str().ok().and_then(|code| code.parse().ok()));
        let body = response.bytes().await.map_err(Error::Transport)?;

        if !status.is_success() || code.is_some() {
            return Err(Error::ClickHouse {
                status: status.as_u16(),
                code: code.flatten(),
                message: String::from_utf8_lossy(&body).trim().to_string(),
            });
        }

        Ok(body.to_vec())
    }
}

fn header(value: &str) -> Result<HeaderValue> {
    HeaderValue::from_str(value)
        .map_err(|_| Error::Url("credentials contain characters not allowed in a header".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bad_url_is_reported_as_a_url_error() {
        let err = Client::from_url("clickhouse://localhost:9000/db").unwrap_err();

        assert!(matches!(err, Error::Url(_)), "{err}");
        assert!(err.to_string().contains("HTTP interface"), "{err}");
    }

    #[test]
    fn with_database_changes_only_the_database() {
        let client = Client::from_url("http://reader:secret@localhost:8123/one")
            .unwrap()
            .with_database("two");

        assert_eq!(client.database(), Some("two"));
        assert_eq!(client.endpoint.user(), Some("reader"));
    }

    use crate::testing::{Fake, Response};

    #[tokio::test]
    async fn credentials_travel_as_headers_and_never_in_the_url() {
        let fake = Fake::always(Response::ok("1\n"));

        assert_eq!(fake.client().query("SELECT 1").await.unwrap(), "1\n");

        let request = fake.last();
        assert_eq!(request.header("x-clickhouse-user"), Some("tester"));
        assert_eq!(request.header("x-clickhouse-key"), Some("s3cret"));
        assert!(request.query.iter().all(|(_, value)| value != "s3cret"));
        assert_eq!(request.sql(), "SELECT 1");
    }

    #[tokio::test]
    async fn every_request_carries_the_database_and_the_safety_settings() {
        let fake = Fake::always(Response::empty());

        fake.client().execute("DROP TABLE t").await.unwrap();

        let request = fake.last();
        assert_eq!(request.param("database"), Some("analytics"));
        assert_eq!(request.param("wait_end_of_query"), Some("1"));
        assert_eq!(request.param("async_insert"), Some("0"));
        assert_eq!(request.param("mutations_sync"), Some("2"));
    }

    #[tokio::test]
    async fn a_client_without_credentials_sends_no_auth_headers() {
        let fake = Fake::always(Response::empty());
        let client = Client::from_url(&fake.url).unwrap();

        client.execute("SELECT 1").await.unwrap();

        let request = fake.last();
        assert_eq!(request.header("x-clickhouse-user"), None);
        assert_eq!(request.header("x-clickhouse-key"), None);
        assert_eq!(request.param("database"), None);
        assert_eq!(client.database(), None);
    }

    #[tokio::test]
    async fn parameters_and_format_go_in_the_url() {
        let fake = Fake::always(Response::empty());

        fake.client()
            .query_bytes(
                "SELECT {a:String}",
                &[("a".to_string(), "x y&z".to_string())],
                Some("RowBinaryWithNamesAndTypes"),
            )
            .await
            .unwrap();

        let request = fake.last();
        assert_eq!(request.param("param_a"), Some("x y&z"));
        assert_eq!(
            request.param("default_format"),
            Some("RowBinaryWithNamesAndTypes")
        );
        assert_eq!(request.sql(), "SELECT {a:String}");
    }

    #[tokio::test]
    async fn an_insert_sends_the_statement_in_the_url_and_the_rows_in_the_body() {
        let fake = Fake::always(Response::empty());

        fake.client()
            .insert_bytes("INSERT INTO t (a) FORMAT RowBinary", vec![1, 2, 3])
            .await
            .unwrap();

        let request = fake.last();
        assert_eq!(
            request.param("query"),
            Some("INSERT INTO t (a) FORMAT RowBinary")
        );
        assert_eq!(request.body, [1, 2, 3]);
    }

    #[tokio::test]
    async fn a_server_exception_keeps_its_status_code_and_message() {
        let fake = Fake::always(Response::exception(60, "Table analytics.t does not exist"));

        let err = fake.client().execute("SELECT * FROM t").await.unwrap_err();

        match err {
            Error::ClickHouse {
                status,
                code,
                message,
            } => {
                assert_eq!(status, 500);
                assert_eq!(code, Some(60));
                assert!(message.contains("does not exist"), "{message}");
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    /// With `wait_end_of_query` off, or behind some proxies, ClickHouse can
    /// send a 200 and still report an exception in the header.
    #[tokio::test]
    async fn an_exception_header_on_a_200_is_still_an_error() {
        let fake = Fake::always(Response {
            status: 200,
            code: Some(241),
            body: b"Memory limit exceeded".to_vec(),
        });

        let err = fake.client().execute("SELECT 1").await.unwrap_err();

        assert!(
            matches!(
                err,
                Error::ClickHouse {
                    status: 200,
                    code: Some(241),
                    ..
                }
            ),
            "{err}"
        );
    }

    #[tokio::test]
    async fn an_error_status_without_a_code_header_is_an_error_with_no_code() {
        let fake = Fake::always(Response {
            status: 502,
            code: None,
            body: b"bad gateway".to_vec(),
        });

        let err = fake.client().execute("SELECT 1").await.unwrap_err();

        assert!(
            matches!(
                err,
                Error::ClickHouse {
                    status: 502,
                    code: None,
                    ..
                }
            ),
            "{err}"
        );
    }

    #[tokio::test]
    async fn an_unreachable_server_is_a_transport_error() {
        // Bound and dropped, so nothing listens on the port.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let client = Client::from_url(&format!("http://127.0.0.1:{port}")).unwrap();

        let err = client.execute("SELECT 1").await.unwrap_err();

        assert!(matches!(err, Error::Transport(_)), "{err}");
    }

    #[tokio::test]
    async fn a_text_query_refuses_a_body_that_is_not_utf8() {
        let fake = Fake::always(Response::ok(vec![0xff, 0xfe]));

        let err = fake.client().query("SELECT 1").await.unwrap_err();

        assert!(matches!(err, Error::Decode(_)), "{err}");
    }

    #[test]
    fn credentials_that_cannot_be_a_header_are_refused() {
        assert!(header("line\nbreak").is_err());
        assert!(header("plain").is_ok());
    }
}
