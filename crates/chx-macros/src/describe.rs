//! Where the columns of a query come from at compile time.
//!
//! In order:
//!
//! 1. `CHX_OFFLINE=true`: the `.chx` cache only. No cache entry is an error
//!    that says to run `chx prepare`.
//! 2. `CLICKHOUSE_URL` set, from the environment or the nearest `.env` above
//!    the crate: the server. The result is also saved to `CHX_TMP` when that
//!    is set, which is how `chx prepare` collects what every macro learned.
//! 3. Neither: the cache, if it has the query, else an error naming both
//!    ways out.
//!
//! The cache is the `.chx` directory nearest the crate, searched upward, so a
//! workspace can keep one at its root.

use std::path::{Path, PathBuf};

use chx_types::cache::{self, Describe, Field, QueryData};
use chx_types::endpoint::Endpoint;
use chx_types::params::Param;
use chx_types::types::Type;

/// One column of a query's result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    pub name: String,
    pub ty: Type,
}

/// Everything outside the query that decides where its columns come from.
/// Read from the environment by [`Source::from_env`], and built by hand in
/// tests.
pub struct Source {
    /// The crate being compiled. The cache is searched for from here upward.
    pub manifest_dir: PathBuf,
    /// `CHX_OFFLINE`: never connect.
    pub offline: bool,
    /// `CLICKHOUSE_URL`, when set and not empty.
    pub url: Option<String>,
    /// `CHX_TMP`: where `chx prepare` collects what the macros learn.
    pub save_to: Option<PathBuf>,
}

impl Source {
    pub fn from_env() -> Result<Self, String> {
        let manifest_dir = PathBuf::from(
            std::env::var("CARGO_MANIFEST_DIR").map_err(|_| "CARGO_MANIFEST_DIR is not set")?,
        );
        let offline = std::env::var("CHX_OFFLINE")
            .map(|value| matches!(value.as_str(), "true" | "1"))
            .unwrap_or(false);
        // Offline means the `.env` is not consulted either: it exists to
        // name a database, and this build must not find one.
        if !offline {
            load_dotenv(&manifest_dir);
        }
        let url = if offline {
            None
        } else {
            std::env::var("CLICKHOUSE_URL")
                .ok()
                .filter(|url| !url.is_empty())
        };
        Ok(Self {
            manifest_dir,
            offline,
            url,
            save_to: std::env::var_os("CHX_TMP").map(PathBuf::from),
        })
    }
}

/// The columns `sql` returns, from the server or the cache as the
/// environment says.
pub fn columns(sql: &str, params: &[Param]) -> Result<Vec<Column>, String> {
    resolve(sql, params, &Source::from_env()?)
}

/// The columns `sql` returns, from `source`.
pub fn resolve(sql: &str, params: &[Param], source: &Source) -> Result<Vec<Column>, String> {
    match &source.url {
        Some(url) => {
            let columns = online(url, sql, params)?;
            if let Some(dir) = &source.save_to {
                let fields = |names: Vec<(String, String)>| {
                    names
                        .into_iter()
                        .map(|(name, ty)| Field { name, ty })
                        .collect()
                };
                let data = QueryData::new(
                    sql,
                    Describe {
                        parameters: fields(
                            params
                                .iter()
                                .map(|param| (param.name.clone(), param.ty.to_string()))
                                .collect(),
                        ),
                        columns: fields(
                            columns
                                .iter()
                                .map(|column| (column.name.clone(), column.ty.to_string()))
                                .collect(),
                        ),
                    },
                );
                data.save_in(dir)
                    .map_err(|err| format!("cannot write {}: {err}", dir.display()))?;
            }
            Ok(columns)
        }
        None => {
            let dir = cache::find_dir(&source.manifest_dir);
            let cached = match &dir {
                Some(dir) => QueryData::find(dir, sql).map_err(|err| err.to_string())?,
                None => None,
            };
            match cached {
                Some(data) => data
                    .describe
                    .columns
                    .into_iter()
                    .map(|field| {
                        Ok(Column {
                            ty: Type::parse(&field.ty).map_err(|err| err.to_string())?,
                            name: field.name,
                        })
                    })
                    .collect(),
                None if source.offline => Err(format!(
                    "CHX_OFFLINE is set and {} has no entry for this query; run `chx prepare` \
                     with CLICKHOUSE_URL set to a database with the schema applied",
                    dir.map_or(".chx".to_string(), |dir| dir.display().to_string())
                )),
                None => Err(
                    "set CLICKHOUSE_URL to check this query against a database, or run \
                     `chx prepare` to cache the check in .chx"
                        .to_string(),
                ),
            }
        }
    }
}

/// The nearest `.env` at or above the crate, loaded without overriding the
/// process environment. The same walk the `chx` command does from the
/// working directory, here from the crate instead, since rustc's working
/// directory is wherever cargo was run.
fn load_dotenv(manifest_dir: &Path) {
    if let Some(path) = manifest_dir
        .ancestors()
        .map(|dir| dir.join(".env"))
        .find(|candidate| candidate.is_file())
    {
        let _ = dotenvy::from_path(&path);
    }
}

/// What kind of statement this is, which decides how the server is asked
/// about it.
enum Kind {
    /// `DESCRIBE TABLE (<sql>)` names its columns.
    Select,
    /// `DESCRIBE TABLE <table>` checks the columns named; nothing comes back.
    Insert { table: String, columns: Vec<String> },
    /// Parsed by the server, nothing more.
    Other,
}

/// The statement without its leading comments and whitespace.
fn strip_leading_comments(sql: &str) -> &str {
    let mut rest = sql.trim_start();
    loop {
        if let Some(after) = rest.strip_prefix("--") {
            rest = after
                .split_once('\n')
                .map_or("", |(_, rest)| rest)
                .trim_start();
        } else if let Some(after) = rest.strip_prefix("/*") {
            rest = after
                .split_once("*/")
                .map_or("", |(_, rest)| rest)
                .trim_start();
        } else {
            return rest;
        }
    }
}

fn kind(sql: &str) -> Kind {
    let sql = strip_leading_comments(sql);
    let words: Vec<&str> = sql
        .split(|c: char| c.is_whitespace() || c == '(' || c == ')' || c == ',')
        .filter(|word| !word.is_empty())
        .collect();
    match words
        .first()
        .map(|word| word.to_ascii_uppercase())
        .as_deref()
    {
        Some("SELECT" | "WITH") => Kind::Select,
        Some("INSERT") if words.get(1).is_some_and(|w| w.eq_ignore_ascii_case("INTO")) => {
            let Some(table) = words.get(2) else {
                return Kind::Other;
            };
            // `INSERT INTO t (a, b) VALUES` or `INSERT INTO t VALUES`: the
            // column list, if any, runs from after the table to the next
            // keyword.
            let rest = sql
                .split_once(table)
                .map(|(_, rest)| rest.trim_start())
                .unwrap_or("");
            let columns = match rest.strip_prefix('(') {
                Some(list) => list
                    .split(')')
                    .next()
                    .unwrap_or("")
                    .split(',')
                    .map(|column| column.trim().trim_matches('`').to_string())
                    .filter(|column| !column.is_empty())
                    .collect(),
                None => Vec::new(),
            };
            Kind::Insert {
                table: table.to_string(),
                columns,
            }
        }
        _ => Kind::Other,
    }
}

fn online(url: &str, sql: &str, params: &[Param]) -> Result<Vec<Column>, String> {
    let endpoint = Endpoint::parse(url).map_err(|err| format!("CLICKHOUSE_URL: {err}"))?;
    match kind(sql) {
        Kind::Select => {
            let placeholders = params
                .iter()
                .map(|param| {
                    param.ty.placeholder().map(|value| (param.name.clone(), value)).ok_or_else(|| {
                        format!(
                            "parameter {{{}:{}}} cannot be checked at compile time; use query_unchecked!",
                            param.name, param.ty
                        )
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            let body = request(
                &endpoint,
                &format!("DESCRIBE TABLE ({sql}) FORMAT TSVRaw"),
                &placeholders,
            )?;
            parse_describe(&body)
        }
        Kind::Insert { table, columns } => {
            let body = request(
                &endpoint,
                &format!("DESCRIBE TABLE {table} FORMAT TSVRaw"),
                &[],
            )?;
            let described = parse_describe(&body)?;
            for column in &columns {
                if !described.iter().any(|c| c.name == *column) {
                    return Err(format!("{table} has no column {column:?}"));
                }
            }
            Ok(Vec::new())
        }
        Kind::Other => {
            request(&endpoint, &format!("EXPLAIN AST {sql}"), &[])?;
            Ok(Vec::new())
        }
    }
}

fn parse_describe(body: &str) -> Result<Vec<Column>, String> {
    body.lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            let mut fields = line.split('\t');
            let name = fields.next().unwrap_or_default().to_string();
            let ty = fields
                .next()
                .ok_or_else(|| format!("unexpected DESCRIBE line {line:?}"))?;
            Ok(Column {
                name,
                ty: Type::parse(ty).map_err(|err| err.to_string())?,
            })
        })
        .collect()
}

fn request(endpoint: &Endpoint, sql: &str, params: &[(String, String)]) -> Result<String, String> {
    let mut url = endpoint.url().clone();
    for (name, value) in params {
        url.query_pairs_mut()
            .append_pair(&format!("param_{name}"), value);
    }
    let client = reqwest::blocking::Client::new();
    let mut request = client.post(url).body(sql.to_string());
    if let Some(user) = endpoint.user() {
        request = request.header("X-ClickHouse-User", user);
    }
    if let Some(password) = endpoint.password() {
        request = request.header("X-ClickHouse-Key", password);
    }
    let response = request
        .send()
        .map_err(|err| format!("cannot reach ClickHouse at {}: {err}", endpoint.url()))?;
    let status = response.status();
    let failed = !status.is_success()
        || response
            .headers()
            .contains_key("X-ClickHouse-Exception-Code");
    let body = response.text().map_err(|err| err.to_string())?;
    if failed {
        return Err(format!("ClickHouse returned {status}: {}", body.trim()));
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    use super::*;

    /// A one-thread HTTP server that answers every request with `answer`,
    /// given the request's target and body, and remembers what it was sent.
    struct Fake {
        url: String,
        seen: Arc<Mutex<Vec<(String, String)>>>,
    }

    impl Fake {
        fn start(answer: impl Fn(&str, &str) -> (u16, String) + Send + 'static) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://reader:pw@{}/db", listener.local_addr().unwrap());
            let seen = Arc::new(Mutex::new(Vec::new()));
            let log = Arc::clone(&seen);
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { continue };
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    let head_end = loop {
                        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            break end;
                        }
                        match stream.read(&mut chunk) {
                            Ok(0) | Err(_) => break 0,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    };
                    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
                    let target = head.split(' ').nth(1).unwrap_or_default().to_string();
                    let length: usize = head
                        .lines()
                        .filter_map(|line| line.split_once(':'))
                        .find(|(key, _)| key.eq_ignore_ascii_case("content-length"))
                        .and_then(|(_, value)| value.trim().parse().ok())
                        .unwrap_or(0);
                    let mut body = buf[(head_end + 4).min(buf.len())..].to_vec();
                    while body.len() < length {
                        match stream.read(&mut chunk) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => body.extend_from_slice(&chunk[..n]),
                        }
                    }
                    let body = String::from_utf8_lossy(&body).into_owned();
                    let (status, response) = answer(&target, &body);
                    log.lock().unwrap().push((target, body));
                    let _ = write!(
                        stream,
                        "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                        response.len()
                    );
                }
            });
            Self { url, seen }
        }

        fn seen(&self) -> Vec<(String, String)> {
            self.seen.lock().unwrap().clone()
        }
    }

    fn params(sql: &str) -> Vec<Param> {
        chx_types::params::parse(sql).unwrap()
    }

    fn source(dir: &Path) -> Source {
        Source {
            manifest_dir: dir.to_path_buf(),
            offline: false,
            url: None,
            save_to: None,
        }
    }

    fn names(columns: &[Column]) -> Vec<String> {
        columns
            .iter()
            .map(|column| format!("{} {}", column.name, column.ty))
            .collect()
    }

    #[test]
    fn statement_kinds() {
        assert!(matches!(kind("SELECT 1"), Kind::Select));
        assert!(matches!(
            kind("-- a comment\n/* another */ SELECT 1"),
            Kind::Select
        ));
        assert!(matches!(
            kind("  with x as (select 1) select * from x"),
            Kind::Select
        ));
        assert!(matches!(kind("ALTER TABLE t DROP COLUMN a"), Kind::Other));
        assert!(matches!(kind(""), Kind::Other));
        assert!(matches!(kind("INSERT INTO"), Kind::Other));
        match kind("INSERT INTO db.t (a, `b`) VALUES ({a:UInt8}, {b:String})") {
            Kind::Insert { table, columns } => {
                assert_eq!(table, "db.t");
                assert_eq!(columns, ["a", "b"]);
            }
            _ => panic!("not an insert"),
        }
        match kind("INSERT INTO t SELECT 1") {
            Kind::Insert { columns, .. } => assert!(columns.is_empty()),
            _ => panic!("not an insert"),
        }
    }

    #[test]
    fn leading_comments_are_not_the_statement() {
        assert_eq!(strip_leading_comments("  -- x\n  SELECT 1"), "SELECT 1");
        assert_eq!(strip_leading_comments("/* x */SELECT 1"), "SELECT 1");
        assert_eq!(strip_leading_comments("-- only a comment"), "");
        assert_eq!(strip_leading_comments("/* never closed"), "");
    }

    #[test]
    fn describe_output_is_name_tab_type() {
        let columns =
            parse_describe("id\tUInt64\t\t\t\t\t\nts\tDateTime64(9, 'UTC')\t\t\t\t\t\n").unwrap();
        assert_eq!(names(&columns), ["id UInt64", "ts DateTime64(9, 'UTC')"]);
        assert!(parse_describe("broken").is_err());
        assert!(parse_describe("x\tArray()").is_err());
    }

    #[test]
    fn a_select_is_described_with_placeholders_for_its_parameters() {
        let fake =
            Fake::start(|_, _| (200, "id\tUInt64\t\t\t\t\t\nnote\tNullable(String)\n".into()));
        let sql = "SELECT id, note FROM t WHERE id > {min:UInt64} AND v = {v:String}";

        let columns = online(&fake.url, sql, &params(sql)).unwrap();

        assert_eq!(names(&columns), ["id UInt64", "note Nullable(String)"]);
        let (target, body) = fake.seen().remove(0);
        assert_eq!(body, format!("DESCRIBE TABLE ({sql}) FORMAT TSVRaw"));
        assert!(target.contains("param_min=0"), "{target}");
        assert!(target.contains("param_v="), "{target}");
        assert!(target.contains("database=db"), "{target}");
        // Credentials are headers, never part of the URL.
        assert!(!target.contains("pw"), "{target}");
    }

    #[test]
    fn a_parameter_with_no_placeholder_cannot_be_checked() {
        let sql = "SELECT id FROM {t:Identifier}";
        let err = online("http://127.0.0.1:1/db", sql, &params(sql)).unwrap_err();
        assert!(err.contains("query_unchecked!"), "{err}");
    }

    #[test]
    fn an_insert_is_checked_against_the_columns_of_its_table() {
        let fake = Fake::start(|_, _| (200, "id\tUInt64\nvenue\tString\n".into()));

        let ok = "INSERT INTO trades (id, venue) VALUES ({id:UInt64}, {v:String})";
        assert!(online(&fake.url, ok, &params(ok)).unwrap().is_empty());
        assert_eq!(fake.seen()[0].1, "DESCRIBE TABLE trades FORMAT TSVRaw");

        let bad = "INSERT INTO trades (id, nope) VALUES ({id:UInt64}, {v:String})";
        let err = online(&fake.url, bad, &params(bad)).unwrap_err();
        assert_eq!(err, "trades has no column \"nope\"");
    }

    #[test]
    fn any_other_statement_is_only_parsed() {
        let fake = Fake::start(|_, _| (200, String::new()));

        let columns = online(&fake.url, "ALTER TABLE t DROP COLUMN a", &[]).unwrap();

        assert!(columns.is_empty());
        assert_eq!(fake.seen()[0].1, "EXPLAIN AST ALTER TABLE t DROP COLUMN a");
    }

    #[test]
    fn a_server_error_is_reported_with_its_text() {
        let fake = Fake::start(|_, _| (404, "Code: 60. Unknown table t".into()));

        let err = online(&fake.url, "SELECT * FROM t", &[]).unwrap_err();

        assert!(err.contains("404"), "{err}");
        assert!(err.contains("Unknown table t"), "{err}");
    }

    #[test]
    fn an_unreachable_or_malformed_url_is_reported() {
        let err = online("clickhouse://localhost:9000", "SELECT 1", &[]).unwrap_err();
        assert!(err.starts_with("CLICKHOUSE_URL:"), "{err}");

        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let err = online(&format!("http://127.0.0.1:{port}"), "SELECT 1", &[]).unwrap_err();
        assert!(err.contains("cannot reach ClickHouse"), "{err}");
    }

    #[test]
    fn with_a_url_the_server_answers_and_the_result_is_saved_for_prepare() {
        let fake = Fake::start(|_, _| (200, "n\tUInt64\n".into()));
        let crate_dir = tempfile::tempdir().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let sql = "SELECT number AS n FROM system.numbers LIMIT {limit:UInt64}";
        let source = Source {
            url: Some(fake.url.clone()),
            save_to: Some(tmp.path().to_path_buf()),
            ..source(crate_dir.path())
        };

        let columns = resolve(sql, &params(sql), &source).unwrap();

        assert_eq!(names(&columns), ["n UInt64"]);
        let saved = QueryData::find(tmp.path(), sql).unwrap().expect("saved");
        assert_eq!(saved.describe.parameters[0].name, "limit");
        assert_eq!(saved.describe.parameters[0].ty, "UInt64");
        assert_eq!(saved.describe.columns[0].ty, "UInt64");
    }

    #[test]
    fn without_a_url_the_cache_answers_from_the_crate_or_above_it() {
        let workspace = tempfile::tempdir().unwrap();
        let crate_dir = workspace.path().join("crates/app");
        std::fs::create_dir_all(&crate_dir).unwrap();
        let sql = "SELECT 1 AS one";
        QueryData::new(
            sql,
            Describe {
                parameters: vec![],
                columns: vec![Field {
                    name: "one".to_string(),
                    ty: "UInt8".to_string(),
                }],
            },
        )
        .save_in(&workspace.path().join(cache::DIR))
        .unwrap();

        let columns = resolve(sql, &[], &source(&crate_dir)).unwrap();
        assert_eq!(names(&columns), ["one UInt8"]);

        // The same, with connecting forbidden.
        let offline = Source {
            offline: true,
            ..source(&crate_dir)
        };
        assert_eq!(names(&resolve(sql, &[], &offline).unwrap()), ["one UInt8"]);
    }

    #[test]
    fn a_query_that_is_nowhere_says_how_to_fix_it() {
        let dir = tempfile::tempdir().unwrap();

        let err = resolve("SELECT 1", &[], &source(dir.path())).unwrap_err();
        assert!(err.contains("CLICKHOUSE_URL"), "{err}");
        assert!(err.contains("chx prepare"), "{err}");

        std::fs::create_dir(dir.path().join(cache::DIR)).unwrap();
        let offline = Source {
            offline: true,
            ..source(dir.path())
        };
        let err = resolve("SELECT 1", &[], &offline).unwrap_err();
        assert!(err.contains("CHX_OFFLINE is set"), "{err}");
        assert!(err.contains(".chx has no entry"), "{err}");
    }

    #[test]
    fn a_cache_entry_with_an_unparseable_type_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        QueryData::new(
            "SELECT 1",
            Describe {
                parameters: vec![],
                columns: vec![Field {
                    name: "x".to_string(),
                    ty: "Array()".to_string(),
                }],
            },
        )
        .save_in(&dir.path().join(cache::DIR))
        .unwrap();

        assert!(resolve("SELECT 1", &[], &source(dir.path())).is_err());
    }

    #[test]
    fn the_nearest_dotenv_is_found_above_the_crate() {
        let workspace = tempfile::tempdir().unwrap();
        let crate_dir = workspace.path().join("crates/app");
        std::fs::create_dir_all(&crate_dir).unwrap();
        std::fs::write(
            workspace.path().join(".env"),
            "CHX_MACROS_DOTENV_TEST=from-file\n",
        )
        .unwrap();

        load_dotenv(&crate_dir);

        assert_eq!(
            std::env::var("CHX_MACROS_DOTENV_TEST").as_deref(),
            Ok("from-file")
        );
    }
}
