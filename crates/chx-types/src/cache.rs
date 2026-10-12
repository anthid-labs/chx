//! The offline query cache: `.chx/query-<hash>.json`.
//!
//! One file per checked query, holding what the macros learned from the
//! server: the parameters the text declares and the columns a `DESCRIBE`
//! returned. With the directory checked in, the macros compile against it and
//! a build needs no database. `chx prepare` writes it and
//! `chx prepare --check` fails when it is stale.
//!
//! The shape follows `.sqlx`, so a reader of one recognises the other, and
//! the hash is plain SHA-256 of the query text, so a file can be matched to
//! its query with `sha256sum` and nothing else.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The directory the cache lives in, relative to a crate or workspace root.
pub const DIR: &str = ".chx";

/// Everything the macros need to compile one query without a server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryData {
    /// Always `ClickHouse`. Present so a file is recognisable on its own.
    pub db_name: String,
    /// The SQL exactly as written in the macro, whitespace included.
    pub query: String,
    /// What the query takes and returns.
    pub describe: Describe,
    /// SHA-256 of `query`, lowercase hex.
    pub hash: String,
}

/// The parameters and columns of one query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Describe {
    /// From the query text, in order of first appearance.
    pub parameters: Vec<Field>,
    /// From the server, in result order. Empty for a statement that returns
    /// nothing.
    pub columns: Vec<Field>,
}

/// A named, typed slot: a parameter or a column. The type is kept as text,
/// in the canonical form the type model prints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Field {
    /// The parameter or column name.
    pub name: String,
    /// The ClickHouse type, as text. Stored under the JSON key `type`.
    #[serde(rename = "type")]
    pub ty: String,
}

/// A cache file that could not be used.
#[derive(Debug)]
pub enum LoadError {
    /// The file or directory could not be read.
    Io(io::Error),
    /// The file is not the JSON this module writes.
    Json(serde_json::Error),
    /// The file's `hash` is not the hash of its `query`: it was edited by
    /// hand, or written by something else.
    Hash {
        /// The file that failed the check.
        path: PathBuf,
        /// The hash its `query` really has.
        expected: String,
    },
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoadError::Io(err) => write!(f, "{err}"),
            LoadError::Json(err) => write!(f, "not a chx query file: {err}"),
            LoadError::Hash { path, expected } => write!(
                f,
                "{} does not hash to its own query ({expected}); it was edited or is not a chx file",
                path.display()
            ),
        }
    }
}

impl std::error::Error for LoadError {}

impl From<io::Error> for LoadError {
    fn from(err: io::Error) -> Self {
        LoadError::Io(err)
    }
}

/// Lowercase hex SHA-256 of the query text, exactly as written in the macro.
///
/// ```
/// // printf 'SELECT 1' | sha256sum
/// assert_eq!(
///     chx_types::cache::hash("SELECT 1"),
///     "e004ebd5b5532a4b85984a62f8ad48a81aa3460c1ca07701f386135d72cdecf5"
/// );
/// ```
pub fn hash(query: &str) -> String {
    let digest = Sha256::digest(query.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The file a query is cached in, within the cache directory.
pub fn file_name(hash: &str) -> String {
    format!("query-{hash}.json")
}

impl QueryData {
    /// Cache data for `query`, with its hash computed.
    ///
    /// ```
    /// use chx_types::cache::{Describe, Field, QueryData};
    ///
    /// let data = QueryData::new(
    ///     "SELECT 1 AS one",
    ///     Describe {
    ///         parameters: vec![],
    ///         columns: vec![Field { name: "one".into(), ty: "UInt8".into() }],
    ///     },
    /// );
    /// assert_eq!(data.file_name(), format!("query-{}.json", data.hash));
    /// ```
    pub fn new(query: &str, describe: Describe) -> Self {
        Self {
            db_name: "ClickHouse".to_string(),
            query: query.to_string(),
            describe,
            hash: hash(query),
        }
    }

    /// The file this data is stored in: `query-<hash>.json`.
    pub fn file_name(&self) -> String {
        file_name(&self.hash)
    }

    /// Writes the file into `dir`, creating the directory if needed. Returns
    /// the path written.
    pub fn save_in(&self, dir: &Path) -> io::Result<PathBuf> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(self.file_name());
        let mut json = serde_json::to_string_pretty(self).map_err(io::Error::other)?;
        json.push('\n');
        std::fs::write(&path, json)?;
        Ok(path)
    }

    /// Reads one cache file, refusing it if its hash does not match its
    /// query.
    pub fn load(path: &Path) -> Result<Self, LoadError> {
        let text = std::fs::read_to_string(path)?;
        let data: QueryData = serde_json::from_str(&text).map_err(LoadError::Json)?;
        let expected = hash(&data.query);
        if data.hash != expected {
            return Err(LoadError::Hash {
                path: path.to_path_buf(),
                expected,
            });
        }
        Ok(data)
    }

    /// The cached data for `query` in `dir`, or `None` if there is none.
    pub fn find(dir: &Path, query: &str) -> Result<Option<Self>, LoadError> {
        let path = dir.join(file_name(&hash(query)));
        if !path.is_file() {
            return Ok(None);
        }
        Self::load(&path).map(Some)
    }
}

/// Every `query-*.json` in `dir`, sorted by file name. A missing directory is
/// an empty cache.
pub fn list(dir: &Path) -> Result<Vec<QueryData>, LoadError> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err.into()),
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("query-") && name.ends_with(".json"))
        })
        .collect();
    paths.sort();
    paths.iter().map(|path| QueryData::load(path)).collect()
}

/// The nearest `.chx` directory at or above `start`: the crate's own, else
/// the workspace's.
pub fn find_dir(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .map(|dir| dir.join(DIR))
        .find(|candidate| candidate.is_dir())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> QueryData {
        QueryData::new(
            "SELECT number FROM system.numbers LIMIT {n:UInt64}",
            Describe {
                parameters: vec![Field {
                    name: "n".to_string(),
                    ty: "UInt64".to_string(),
                }],
                columns: vec![Field {
                    name: "number".to_string(),
                    ty: "UInt64".to_string(),
                }],
            },
        )
    }

    #[test]
    fn hash_is_plain_sha256_of_the_text() {
        // printf 'SELECT 1' | sha256sum
        assert_eq!(
            hash("SELECT 1"),
            "e004ebd5b5532a4b85984a62f8ad48a81aa3460c1ca07701f386135d72cdecf5"
        );
    }

    #[test]
    fn saves_and_loads_the_same_data() {
        let dir = std::env::temp_dir().join(format!("chx-cache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let data = sample();
        let path = data.save_in(&dir).unwrap();
        assert_eq!(
            path.file_name().unwrap().to_str().unwrap(),
            format!("query-{}.json", data.hash)
        );
        assert_eq!(QueryData::load(&path).unwrap(), data);
        assert_eq!(
            QueryData::find(&dir, &data.query).unwrap(),
            Some(data.clone())
        );
        assert_eq!(QueryData::find(&dir, "SELECT 2").unwrap(), None);
        assert_eq!(list(&dir).unwrap(), vec![data]);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_json_shape_names_the_type_field_type() {
        let json = serde_json::to_value(sample()).unwrap();
        assert_eq!(json["db_name"], "ClickHouse");
        assert_eq!(json["describe"]["columns"][0]["type"], "UInt64");
        assert_eq!(json["describe"]["parameters"][0]["name"], "n");
    }

    #[test]
    fn an_edited_file_is_refused() {
        let dir = std::env::temp_dir().join(format!("chx-cache-edit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let mut data = sample();
        data.query.push_str(" -- edited after hashing");
        let path = data.save_in(&dir).unwrap();
        let err = QueryData::load(&path).unwrap_err();
        assert!(matches!(err, LoadError::Hash { .. }), "{err}");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_missing_directory_is_an_empty_cache() {
        assert!(list(Path::new("/nonexistent/chx")).unwrap().is_empty());
        assert_eq!(find_dir(Path::new("/nonexistent/chx")), None);
    }

    #[test]
    fn load_errors_say_what_is_wrong_with_the_file() {
        let dir = std::env::temp_dir().join(format!("chx-cache-errors-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let missing = QueryData::load(&dir.join("query-none.json")).unwrap_err();
        assert!(matches!(missing, LoadError::Io(_)), "{missing}");
        assert!(!missing.to_string().is_empty());

        let not_json = dir.join("query-bad.json");
        std::fs::write(&not_json, "not json").unwrap();
        let err = QueryData::load(&not_json).unwrap_err();
        assert!(matches!(err, LoadError::Json(_)), "{err}");
        assert!(err.to_string().starts_with("not a chx query file"), "{err}");
        // One unreadable file fails the whole listing: a cache is all or nothing.
        assert!(list(&dir).is_err());
        assert!(std::error::Error::source(&err).is_none());

        let mut edited = sample();
        edited.hash = "0".repeat(64);
        let path = edited.save_in(&dir).unwrap();
        let err = QueryData::load(&path).unwrap_err();
        assert!(
            err.to_string().contains("does not hash to its own query"),
            "{err}"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn listing_ignores_files_that_are_not_cache_entries() {
        let dir = std::env::temp_dir().join(format!("chx-cache-list-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        sample().save_in(&dir).unwrap();
        std::fs::write(dir.join("README.md"), "not a query").unwrap();
        std::fs::write(dir.join("query-notes.txt"), "not json").unwrap();

        assert_eq!(list(&dir).unwrap(), vec![sample()]);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_cache_directory_is_found_in_the_crate_or_above_it() {
        let root = std::env::temp_dir().join(format!("chx-cache-find-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let nested = root.join("crates/app/src");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir(root.join(DIR)).unwrap();

        assert_eq!(find_dir(&nested), Some(root.join(DIR)));

        // The crate's own wins over the workspace's.
        std::fs::create_dir(root.join("crates/app").join(DIR)).unwrap();
        assert_eq!(find_dir(&nested), Some(root.join("crates/app").join(DIR)));

        std::fs::remove_dir_all(&root).unwrap();
    }
}
