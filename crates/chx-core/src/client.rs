//! A minimal ClickHouse client over the HTTP interface.
//!
//! Two operations: run a statement, and run a query and read its body as text.
//! That is all a migration tool needs, and keeping it that small is what lets
//! chx avoid a client crate whose type mapping it would never use.

use percent_encoding::percent_decode_str;
use reqwest::header::HeaderValue;
use url::Url;

use crate::error::{Error, Result};

/// Settings sent with every request.
///
/// Each one closes a way for a statement to report success before its effect
/// is real, which for a migration tool means recording a step as done that is
/// not:
///
/// - `wait_end_of_query` buffers the response server side. Without it,
///   ClickHouse can send a 200 header and then fail partway through the body,
///   and the status code says nothing.
/// - `async_insert=0` makes an `INSERT`, including chx's own history rows,
///   durable before it returns, whatever the server or user profile defaults
///   to.
/// - `mutations_sync=2` makes `ALTER ... UPDATE` and `ALTER ... DELETE` wait
///   for the mutation on every replica. A backfill in one migration followed
///   by a statement that depends on it would otherwise race.
const REQUEST_SETTINGS: &[(&str, &str)] = &[
    ("wait_end_of_query", "1"),
    ("async_insert", "0"),
    ("mutations_sync", "2"),
];

/// A connection to one ClickHouse server and database.
#[derive(Debug, Clone)]
pub struct Client {
    http: reqwest::Client,
    /// The server's HTTP endpoint with the database and settings in the query
    /// string. Never carries credentials: those travel as headers, so the URL
    /// is safe to appear in a log line or an error.
    endpoint: Url,
    user: Option<String>,
    password: Option<String>,
    database: Option<String>,
}

impl Client {
    /// Connects to `http[s]://[user[:password]@]host[:port][/database][?setting=value]`.
    ///
    /// The path, when present, is the database. Any query parameters are
    /// passed through as ClickHouse settings. Nothing is sent until the first
    /// statement: this only parses.
    pub fn from_url(raw: &str) -> Result<Self> {
        let parsed = Url::parse(raw).map_err(|err| Error::Url(err.to_string()))?;

        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(Error::Url(format!(
                "scheme {:?} is not supported; chx speaks the HTTP interface (port 8123 or 8443), \
                 not the native protocol",
                parsed.scheme()
            )));
        }

        if parsed.host_str().is_none() {
            return Err(Error::Url("no host".to_string()));
        }

        let decode = |value: &str| -> Result<String> {
            percent_decode_str(value)
                .decode_utf8()
                .map(|decoded| decoded.into_owned())
                .map_err(|err| Error::Url(format!("credentials are not valid UTF-8: {err}")))
        };

        let user = match parsed.username() {
            "" => None,
            user => Some(decode(user)?),
        };
        let password = parsed.password().map(decode).transpose()?;

        let database = match parsed.path().trim_matches('/') {
            "" => None,
            path if path.contains('/') => {
                return Err(Error::Url(format!(
                    "path {path:?} has more than one segment; it should be just the database name"
                )));
            }
            path => Some(decode(path)?),
        };

        let mut endpoint = parsed.clone();
        endpoint
            .set_username("")
            .and_then(|()| endpoint.set_password(None))
            .map_err(|()| Error::Internal("could not strip credentials from url".to_string()))?;
        endpoint.set_path("/");
        {
            let mut query = endpoint.query_pairs_mut();
            if let Some(database) = &database {
                query.append_pair("database", database);
            }
            for (key, value) in REQUEST_SETTINGS {
                query.append_pair(key, value);
            }
        }

        Ok(Self {
            http: reqwest::Client::new(),
            endpoint,
            user,
            password,
            database,
        })
    }

    /// The database statements run in, or `None` for the user's default.
    pub fn database(&self) -> Option<&str> {
        self.database.as_deref()
    }

    /// The same server with a different database. Credentials and settings
    /// carry over.
    pub fn with_database(&self, database: &str) -> Self {
        let mut endpoint = self.endpoint.clone();
        let pairs: Vec<(String, String)> = endpoint
            .query_pairs()
            .filter(|(key, _)| key != "database")
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        endpoint
            .query_pairs_mut()
            .clear()
            .append_pair("database", database)
            .extend_pairs(pairs);

        Self {
            endpoint,
            database: Some(database.to_string()),
            ..self.clone()
        }
    }

    /// Runs one statement and discards any result.
    pub async fn execute(&self, sql: &str) -> Result<()> {
        self.send(sql).await.map(|_| ())
    }

    /// Runs one query and returns its body. The caller chooses the format in
    /// the SQL.
    pub async fn query(&self, sql: &str) -> Result<String> {
        self.send(sql).await
    }

    async fn send(&self, sql: &str) -> Result<String> {
        let mut request = self.http.post(self.endpoint.clone()).body(sql.to_string());

        // Headers rather than basic auth or URL parameters, because a URL ends
        // up in proxy logs and ClickHouse's own query_log, and a header does
        // not.
        if let Some(user) = &self.user {
            request = request.header("X-ClickHouse-User", header(user)?);
        }
        if let Some(password) = &self.password {
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
        let body = response.text().await.map_err(Error::Transport)?;

        if !status.is_success() || code.is_some() {
            return Err(Error::ClickHouse {
                status: status.as_u16(),
                code: code.flatten(),
                message: body.trim().to_string(),
            });
        }

        Ok(body)
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
    fn credentials_move_out_of_the_url() {
        let client = Client::from_url("http://reader:p%40ss@localhost:8123/analytics").unwrap();

        assert_eq!(client.user.as_deref(), Some("reader"));
        assert_eq!(client.password.as_deref(), Some("p@ss"));
        assert_eq!(client.database(), Some("analytics"));
        assert_eq!(client.endpoint.username(), "");
        assert_eq!(client.endpoint.password(), None);
        assert!(!client.endpoint.as_str().contains("p%40ss"));
    }

    #[test]
    fn path_becomes_the_database_parameter() {
        let client = Client::from_url("http://localhost:8123/analytics").unwrap();

        assert_eq!(client.endpoint.path(), "/");
        assert!(
            client
                .endpoint
                .query_pairs()
                .any(|(key, value)| key == "database" && value == "analytics")
        );
    }

    #[test]
    fn no_path_means_the_default_database() {
        let client = Client::from_url("http://localhost:8123").unwrap();

        assert_eq!(client.database(), None);
        assert!(
            !client
                .endpoint
                .query_pairs()
                .any(|(key, _)| key == "database")
        );
    }

    #[test]
    fn user_settings_are_kept() {
        let client = Client::from_url("https://host:8443/db?max_execution_time=60").unwrap();

        assert!(
            client
                .endpoint
                .query_pairs()
                .any(|(key, value)| key == "max_execution_time" && value == "60")
        );
    }

    #[test]
    fn native_protocol_urls_are_refused_with_the_reason() {
        let err = Client::from_url("clickhouse://localhost:9000/db").unwrap_err();

        assert!(err.to_string().contains("HTTP interface"), "{err}");
    }

    #[test]
    fn nested_paths_are_refused() {
        assert!(Client::from_url("http://localhost:8123/a/b").is_err());
    }

    #[test]
    fn with_database_replaces_rather_than_appends() {
        let client = Client::from_url("http://localhost:8123/one")
            .unwrap()
            .with_database("two");

        let databases: Vec<_> = client
            .endpoint
            .query_pairs()
            .filter(|(key, _)| key == "database")
            .map(|(_, value)| value.into_owned())
            .collect();

        assert_eq!(databases, ["two"]);
        assert_eq!(client.database(), Some("two"));
    }
}
