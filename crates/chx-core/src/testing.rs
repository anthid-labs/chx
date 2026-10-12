//! Test support: a ClickHouse that lives in the test process.
//!
//! Two layers. [`Fake`] is a bare HTTP server on a loopback port that hands
//! every request to a closure, for tests of the client, queries and inserts
//! that need to see exactly what was sent and choose exactly what comes
//! back. [`FakeClickHouse`] is that server with enough of ClickHouse behind
//! it to run migrations: a history table, the lock, a cluster that can be
//! made to refuse, and a statement that can be made to fail.
//!
//! Neither parses SQL. They recognise the handful of statements chx itself
//! sends by a fragment of their text, which is the point: a change to one of
//! those statements shows up here as a test that no longer matches.
//!
//! The live suites in `tests/` are what prove chx against a real server.
//! These prove the logic around it, in milliseconds and without Docker.
//!
//! Paths here are spelled `chx::`, not `crate::`, because the command line's
//! tests include this same file with `#[path]` to drive the `chx` binary
//! against the fake. It is test support in both places and ships in neither.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex, MutexGuard};

use chx::client::Client;

/// One request the fake received.
#[derive(Debug, Clone)]
pub struct Request {
    /// The URL's query string, decoded.
    pub query: Vec<(String, String)>,
    /// Header names lowercased.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    /// The first value of a URL parameter.
    pub fn param(&self, name: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    /// The body as text: the SQL, for anything but an insert.
    pub fn sql(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// What the fake answers with.
#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    /// Sent as `X-ClickHouse-Exception-Code`.
    pub code: Option<u32>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn ok(body: impl Into<Vec<u8>>) -> Self {
        Self {
            status: 200,
            code: None,
            body: body.into(),
        }
    }

    pub fn empty() -> Self {
        Self::ok(Vec::new())
    }

    /// A server exception, as ClickHouse reports one: a 500 with the code in
    /// a header and the message in the body.
    pub fn exception(code: u32, message: &str) -> Self {
        Self {
            status: 500,
            code: Some(code),
            body: format!("Code: {code}. DB::Exception: {message}").into_bytes(),
        }
    }
}

/// An HTTP server on a loopback port, answering with a closure.
pub struct Fake {
    pub url: String,
    requests: Arc<Mutex<Vec<Request>>>,
}

impl Fake {
    /// Starts the server. The thread serving it lives until the test process
    /// exits.
    pub fn start(handler: impl Fn(&Request) -> Response + Send + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a loopback port");
        let url = format!("http://{}", listener.local_addr().expect("local address"));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&requests);

        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let Some(request) = read_request(&mut stream) else {
                    continue;
                };
                let response = handler(&request);
                seen.lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(request);
                write_response(&mut stream, &response);
            }
        });

        Self { url, requests }
    }

    /// A server that answers every request the same way.
    pub fn always(response: Response) -> Self {
        Self::start(move |_| response.clone())
    }

    /// A client for this server, as `user` with `password`, in `database`.
    pub fn client(&self) -> Client {
        let url = self.url.replacen("http://", "http://tester:s3cret@", 1);
        Client::from_url(&format!("{url}/analytics")).expect("fake url parses")
    }

    /// Every request received so far, oldest first.
    pub fn requests(&self) -> Vec<Request> {
        self.requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub fn last(&self) -> Request {
        self.requests().pop().expect("at least one request")
    }
}

fn read_request(stream: &mut TcpStream) -> Option<Request> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(end) = buf.windows(4).position(|window| window == b"\r\n\r\n") {
            break end;
        }
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..read]);
    };

    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let target = lines.next()?.split(' ').nth(1)?.to_string();
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.trim().to_ascii_lowercase(), value.trim().to_string()))
        .collect();
    let length: usize = headers
        .iter()
        .find(|(key, _)| key == "content-length")
        .and_then(|(_, value)| value.parse().ok())
        .unwrap_or(0);

    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < length {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }

    let query = target
        .split_once('?')
        .map(|(_, query)| {
            url::form_urlencoded::parse(query.as_bytes())
                .map(|(key, value)| (key.into_owned(), value.into_owned()))
                .collect()
        })
        .unwrap_or_default();

    Some(Request {
        query,
        headers,
        body,
    })
}

fn write_response(stream: &mut TcpStream, response: &Response) {
    let mut head = format!(
        "HTTP/1.1 {} X\r\nContent-Length: {}\r\nConnection: close\r\n",
        response.status,
        response.body.len()
    );
    if let Some(code) = response.code {
        head.push_str(&format!("X-ClickHouse-Exception-Code: {code}\r\n"));
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(&response.body);
    let _ = stream.flush();
}

/// A `RowBinaryWithNamesAndTypes` body: the header for `columns`, as
/// `(name, type)`, followed by `rows`, which the caller has already encoded.
pub fn row_binary(columns: &[(&str, &str)], rows: &[u8]) -> Vec<u8> {
    let mut body = vec![columns.len() as u8];
    for (name, _) in columns {
        body.push(name.len() as u8);
        body.extend_from_slice(name.as_bytes());
    }
    for (_, ty) in columns {
        body.push(ty.len() as u8);
        body.extend_from_slice(ty.as_bytes());
    }
    body.extend_from_slice(rows);
    body
}

/// One row of the fake's `_chx_migrations`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryRow {
    pub version: i64,
    pub description: String,
    pub checksum: String,
    pub statements: u32,
    pub applied: u32,
    pub success: bool,
    pub execution_ms: u64,
}

/// What the fake ClickHouse holds, and how it is told to misbehave.
#[derive(Debug, Default)]
pub struct State {
    /// The engine of the database, as `system.databases` reports it.
    /// `Atomic` unless a test sets `Replicated` or `Shared`.
    pub database_engine: String,
    /// The engine `_chx_migrations` was created with. `None` until it is.
    pub history_engine: Option<String>,
    /// Every row inserted into the history, in order. Reads return the
    /// latest per version, as `FINAL` would.
    pub history: Vec<HistoryRow>,
    /// The lock table's comment while the lock is held.
    pub lock: Option<String>,
    /// Statements from migration files, in the order they ran.
    pub executed: Vec<String>,
    /// Every statement received, chx's own included.
    pub log: Vec<String>,
    /// A migration statement containing this text fails with a syntax error.
    pub fail_containing: Option<String>,
    /// Whether `ON CLUSTER` DDL is refused, as on a server with no Keeper.
    pub refuse_on_cluster: bool,
    /// What `system.clusters` and `system.replicas` report for the history
    /// table: nodes in the cluster, and replicas holding the table.
    pub nodes: u64,
    pub replicas: u64,
    /// Release the lock by itself after this many reads of its holder, as a
    /// run that finishes while another waits.
    pub release_after_holder_reads: Option<u32>,
}

/// A [`Fake`] that behaves like ClickHouse for everything a migration run
/// sends.
pub struct FakeClickHouse {
    pub fake: Fake,
    state: Arc<Mutex<State>>,
}

impl FakeClickHouse {
    pub fn start() -> Self {
        let state = Arc::new(Mutex::new(State {
            database_engine: "Atomic".to_string(),
            ..State::default()
        }));
        let shared = Arc::clone(&state);
        let fake = Fake::start(move |request| {
            let mut state = shared
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            answer(&mut state, &request.sql())
        });
        Self { fake, state }
    }

    pub fn client(&self) -> Client {
        self.fake.client()
    }

    /// The state, to arrange a test or to assert on afterwards.
    pub fn state(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn answer(state: &mut State, sql: &str) -> Response {
    state.log.push(sql.to_string());
    let history = chx::migrate::history::TABLE;
    let lock = chx::migrate::lock::TABLE;

    if sql.contains("FROM system.databases") {
        return Response::ok(format!("{}\n", state.database_engine));
    }
    if sql.contains("SELECT engine FROM system.tables") {
        return Response::ok(
            state
                .history_engine
                .as_ref()
                .map(|engine| format!("{engine}\n"))
                .unwrap_or_default(),
        );
    }
    if sql.starts_with(&format!("CREATE TABLE IF NOT EXISTS {history}")) {
        if sql.contains("ON CLUSTER") && state.refuse_on_cluster {
            return Response::exception(170, "Requested cluster 'prod' not found");
        }
        let engine = if sql.contains("ENGINE = Replicated") {
            "ReplicatedReplacingMergeTree"
        } else if state.database_engine == "Shared" {
            "SharedReplacingMergeTree"
        } else {
            "ReplacingMergeTree"
        };
        state.history_engine = Some(engine.to_string());
        return Response::empty();
    }
    if sql.contains("FROM system.clusters") {
        return Response::ok(format!("{}\t{}\n", state.nodes, state.replicas));
    }
    if sql.starts_with("SYSTEM SYNC REPLICA") {
        return Response::empty();
    }
    if sql.contains(&format!("FROM {history} FINAL")) {
        return Response::ok(final_rows(&state.history));
    }
    if sql.starts_with(&format!("INSERT INTO {history}")) {
        return match parse_history_insert(sql) {
            Some(row) => {
                state.history.push(row);
                Response::empty()
            }
            None => Response::exception(62, "the fake could not parse this history insert"),
        };
    }
    if sql.starts_with(&format!("CREATE TABLE {lock}")) {
        if sql.contains("ON CLUSTER") && state.refuse_on_cluster {
            return Response::exception(170, "Requested cluster 'prod' not found");
        }
        if state.lock.is_some() {
            return Response::exception(57, "Table analytics._chx_lock already exists");
        }
        let comment = sql
            .split_once("COMMENT ")
            .map(|(_, literal)| unquote(literal.trim()))
            .unwrap_or_default();
        state.lock = Some(comment);
        return Response::empty();
    }
    if sql.contains("SELECT comment, metadata_modification_time") {
        let body = state
            .lock
            .as_ref()
            .map(|comment| format!("{comment}\t2026-10-12 14:00:00\n"))
            .unwrap_or_default();
        if let Some(left) = state.release_after_holder_reads.as_mut() {
            *left = left.saturating_sub(1);
            if *left == 0 {
                state.lock = None;
                state.release_after_holder_reads = None;
            }
        }
        return Response::ok(body);
    }
    if sql.starts_with(&format!("DROP TABLE IF EXISTS {lock}")) {
        state.lock = None;
        return Response::empty();
    }

    // Anything else came from a migration file.
    if state
        .fail_containing
        .as_ref()
        .is_some_and(|marker| sql.contains(marker.as_str()))
    {
        return Response::exception(62, "Syntax error: failed at position 1");
    }
    state.executed.push(sql.to_string());
    Response::empty()
}

/// The latest row per version, by statements applied, as `FINAL` over
/// `ReplacingMergeTree(applied)` returns, in the column order chx selects.
fn final_rows(history: &[HistoryRow]) -> String {
    let mut latest: std::collections::BTreeMap<i64, &HistoryRow> =
        std::collections::BTreeMap::new();
    for row in history {
        match latest.get(&row.version) {
            Some(existing) if existing.applied > row.applied => {}
            _ => {
                latest.insert(row.version, row);
            }
        }
    }
    latest
        .values()
        .map(|row| {
            format!(
                "{}\t{}\t{}\t{}\t{}\t{}\n",
                row.version,
                row.checksum,
                row.statements,
                row.applied,
                row.success,
                row.description
            )
        })
        .collect()
}

/// `INSERT INTO _chx_migrations (...) VALUES (v, 'desc', 'sum', n, n, bool, ms)`.
fn parse_history_insert(sql: &str) -> Option<HistoryRow> {
    let values = sql.split_once("VALUES (")?.1.strip_suffix(')')?;
    let fields = split_values(values);
    let [
        version,
        description,
        checksum,
        statements,
        applied,
        success,
        ms,
    ] = fields.as_slice()
    else {
        return None;
    };
    Some(HistoryRow {
        version: version.parse().ok()?,
        description: unquote(description),
        checksum: unquote(checksum),
        statements: statements.parse().ok()?,
        applied: applied.parse().ok()?,
        success: success.parse().ok()?,
        execution_ms: ms.parse().ok()?,
    })
}

/// Splits on commas that are outside single-quoted literals.
fn split_values(values: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut chars = values.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' if quoted => {
                current.push(c);
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            '\'' => {
                quoted = !quoted;
                current.push(c);
            }
            ',' if !quoted => {
                fields.push(current.trim().to_string());
                current.clear();
            }
            c => current.push(c),
        }
    }
    fields.push(current.trim().to_string());
    fields
}

/// The value of a single-quoted literal, as `history::quote` wrote it.
fn unquote(literal: &str) -> String {
    let inner = literal
        .strip_prefix('\'')
        .and_then(|rest| rest.strip_suffix('\''))
        .unwrap_or(literal);
    let mut out = String::new();
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(next) = chars.next() {
                out.push(next);
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_inserts_parse_back_to_what_was_quoted() {
        let row = parse_history_insert(
            "INSERT INTO _chx_migrations (version, description, checksum, statements, applied, \
             success, execution_ms) VALUES (3, 'it\\'s, a test', 'abc', 2, 1, false, 17)",
        )
        .unwrap();
        assert_eq!(
            row,
            HistoryRow {
                version: 3,
                description: "it's, a test".to_string(),
                checksum: "abc".to_string(),
                statements: 2,
                applied: 1,
                success: false,
                execution_ms: 17,
            }
        );
        assert!(parse_history_insert("INSERT INTO _chx_migrations VALUES (1)").is_none());
    }

    #[test]
    fn final_keeps_the_row_with_the_most_statements_applied() {
        let row = |applied, success| HistoryRow {
            version: 1,
            description: "d".to_string(),
            checksum: "c".to_string(),
            statements: 2,
            applied,
            success,
            execution_ms: 0,
        };
        assert_eq!(
            final_rows(&[row(1, false), row(2, true), row(1, false)]),
            "1\tc\t2\t2\ttrue\td\n"
        );
    }

    #[test]
    fn row_binary_header_is_count_names_types() {
        assert_eq!(
            row_binary(&[("a", "UInt8")], &[7]),
            [1, 1, b'a', 5, b'U', b'I', b'n', b't', b'8', 7]
        );
    }
}
