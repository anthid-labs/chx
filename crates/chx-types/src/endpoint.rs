//! The connection URL, taken apart.
//!
//! `http[s]://user:password@host:port/database?setting=value`. Credentials
//! travel as headers rather than in the request URL, because a URL ends up in
//! proxy logs and ClickHouse's own `query_log`, and a header does not. So the
//! URL is split once, here, into an endpoint that is safe to print and the
//! credentials that are not.

use std::fmt;

use percent_encoding::percent_decode_str;
use url::Url;

/// Settings sent with every request.
///
/// Each one closes a way for a statement to report success before its effect
/// is real, which for a migration tool means recording a step as done that is
/// not:
///
/// - `wait_end_of_query` buffers the response server side. Without it,
///   ClickHouse can send a 200 header and then fail partway through the body,
///   and the status code says nothing. For a query, it is what turns an error
///   in the middle of a result set into an error status instead of a
///   truncated body.
/// - `async_insert=0` makes an `INSERT` durable before it returns, whatever
///   the server or user profile defaults to.
/// - `mutations_sync=2` makes `ALTER ... UPDATE` and `ALTER ... DELETE` wait
///   for the mutation on every replica. A backfill in one migration followed
///   by a statement that depends on it would otherwise race.
pub const REQUEST_SETTINGS: &[(&str, &str)] = &[
    ("wait_end_of_query", "1"),
    ("async_insert", "0"),
    ("mutations_sync", "2"),
];

/// A malformed connection URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrlError(pub String);

impl fmt::Display for UrlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for UrlError {}

/// One ClickHouse server and database, with the credentials held apart from
/// the URL.
#[derive(Debug, Clone)]
pub struct Endpoint {
    /// The server's HTTP endpoint with the database and settings in the query
    /// string. Never carries credentials, so it is safe to appear in a log
    /// line or an error.
    url: Url,
    user: Option<String>,
    password: Option<String>,
    database: Option<String>,
}

impl Endpoint {
    /// Parses `http[s]://[user[:password]@]host[:port][/database][?setting=value]`.
    ///
    /// The path, when present, is the database. Any query parameters are
    /// passed through as ClickHouse settings.
    pub fn parse(raw: &str) -> Result<Self, UrlError> {
        let parsed = Url::parse(raw).map_err(|err| UrlError(err.to_string()))?;

        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(UrlError(format!(
                "scheme {:?} is not supported; chx speaks the HTTP interface (port 8123 or 8443), \
                 not the native protocol",
                parsed.scheme()
            )));
        }

        if parsed.host_str().is_none() {
            return Err(UrlError("no host".to_string()));
        }

        let decode = |value: &str| -> Result<String, UrlError> {
            percent_decode_str(value)
                .decode_utf8()
                .map(|decoded| decoded.into_owned())
                .map_err(|err| UrlError(format!("credentials are not valid UTF-8: {err}")))
        };

        let user = match parsed.username() {
            "" => None,
            user => Some(decode(user)?),
        };
        let password = parsed.password().map(decode).transpose()?;

        let database = match parsed.path().trim_matches('/') {
            "" => None,
            path if path.contains('/') => {
                return Err(UrlError(format!(
                    "path {path:?} has more than one segment; it should be just the database name"
                )));
            }
            path => Some(decode(path)?),
        };

        let mut url = parsed.clone();
        url.set_username("")
            .and_then(|()| url.set_password(None))
            .map_err(|()| UrlError("could not strip credentials from url".to_string()))?;
        url.set_path("/");
        {
            let mut query = url.query_pairs_mut();
            if let Some(database) = &database {
                query.append_pair("database", database);
            }
            for (key, value) in REQUEST_SETTINGS {
                query.append_pair(key, value);
            }
        }

        Ok(Self {
            url,
            user,
            password,
            database,
        })
    }

    /// The request URL: scheme, host, port, database and settings. No
    /// credentials.
    pub fn url(&self) -> &Url {
        &self.url
    }

    pub fn user(&self) -> Option<&str> {
        self.user.as_deref()
    }

    pub fn password(&self) -> Option<&str> {
        self.password.as_deref()
    }

    /// The database statements run in, or `None` for the user's default.
    pub fn database(&self) -> Option<&str> {
        self.database.as_deref()
    }

    /// The same server with a different database. Credentials and settings
    /// carry over.
    pub fn with_database(&self, database: &str) -> Self {
        let mut url = self.url.clone();
        let pairs: Vec<(String, String)> = url
            .query_pairs()
            .filter(|(key, _)| key != "database")
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        url.query_pairs_mut()
            .clear()
            .append_pair("database", database)
            .extend_pairs(pairs);

        Self {
            url,
            database: Some(database.to_string()),
            ..self.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_move_out_of_the_url() {
        let endpoint = Endpoint::parse("http://reader:p%40ss@localhost:8123/analytics").unwrap();

        assert_eq!(endpoint.user(), Some("reader"));
        assert_eq!(endpoint.password(), Some("p@ss"));
        assert_eq!(endpoint.database(), Some("analytics"));
        assert_eq!(endpoint.url().username(), "");
        assert_eq!(endpoint.url().password(), None);
        assert!(!endpoint.url().as_str().contains("p%40ss"));
    }

    #[test]
    fn path_becomes_the_database_parameter() {
        let endpoint = Endpoint::parse("http://localhost:8123/analytics").unwrap();

        assert_eq!(endpoint.url().path(), "/");
        assert!(
            endpoint
                .url()
                .query_pairs()
                .any(|(key, value)| key == "database" && value == "analytics")
        );
    }

    #[test]
    fn no_path_means_the_default_database() {
        let endpoint = Endpoint::parse("http://localhost:8123").unwrap();

        assert_eq!(endpoint.database(), None);
        assert!(
            !endpoint
                .url()
                .query_pairs()
                .any(|(key, _)| key == "database")
        );
    }

    #[test]
    fn user_settings_are_kept() {
        let endpoint = Endpoint::parse("https://host:8443/db?max_execution_time=60").unwrap();

        assert!(
            endpoint
                .url()
                .query_pairs()
                .any(|(key, value)| key == "max_execution_time" && value == "60")
        );
    }

    #[test]
    fn native_protocol_urls_are_refused_with_the_reason() {
        let err = Endpoint::parse("clickhouse://localhost:9000/db").unwrap_err();

        assert!(err.to_string().contains("HTTP interface"), "{err}");
    }

    #[test]
    fn nested_paths_are_refused() {
        assert!(Endpoint::parse("http://localhost:8123/a/b").is_err());
    }

    #[test]
    fn with_database_replaces_rather_than_appends() {
        let endpoint = Endpoint::parse("http://localhost:8123/one")
            .unwrap()
            .with_database("two");

        let databases: Vec<_> = endpoint
            .url()
            .query_pairs()
            .filter(|(key, _)| key == "database")
            .map(|(_, value)| value.into_owned())
            .collect();

        assert_eq!(databases, ["two"]);
        assert_eq!(endpoint.database(), Some("two"));
    }
}
