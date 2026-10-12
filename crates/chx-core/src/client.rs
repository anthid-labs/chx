//! A minimal ClickHouse client over the HTTP interface.
//!
//! Four operations: run a statement, run a query and read its body as text,
//! run a query with parameters and read its body in a named format, and send
//! an `INSERT` with its data in the body. Everything typed is built on the
//! last two, in [`crate::query`] and [`crate::insert`]. Keeping the client
//! this small is what lets chx stay off the native protocol and work against
//! every deployment, ClickHouse Cloud included.

use chx_types::endpoint::Endpoint;
use reqwest::header::HeaderValue;

use crate::error::{Error, Result};

/// A connection to one ClickHouse server and database.
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
            http: reqwest::Client::new(),
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
}
