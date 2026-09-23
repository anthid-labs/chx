//! Reading migrations from a directory.
//!
//! A migration is a file named `<version>_<description>.sql`, where the version
//! is an integer. Anything that sorts is fine, but a timestamp such as
//! `20260923114500` avoids collisions between branches.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::error::{Error, Result};

/// One migration file, read and checksummed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Migration {
    pub version: i64,
    /// From the file name, with underscores as spaces.
    pub description: String,
    pub path: PathBuf,
    pub sql: String,
    /// Lowercase hex SHA-256 of the file's bytes, exactly as `sha256sum`
    /// prints it. Nothing is normalised first: a change to line endings or
    /// trailing whitespace is a change to the file, and the history says so.
    pub checksum: String,
}

/// Reads every `.sql` file in `dir`, sorted by version.
///
/// Other files are ignored, so a README can live next to the migrations. A
/// `.sql` file whose name has no version is refused rather than skipped: a
/// migration that silently never runs is worse than one that fails loudly.
pub fn read_dir(dir: &Path) -> Result<Vec<Migration>> {
    let entries = std::fs::read_dir(dir).map_err(|err| match err.kind() {
        std::io::ErrorKind::NotFound => {
            Error::NotFound(format!("migrations directory {}", dir.display()))
        }
        _ => Error::Io(err),
    })?;

    let mut by_version: BTreeMap<i64, Migration> = BTreeMap::new();

    for entry in entries {
        let path = entry?.path();

        if !path.is_file() || path.extension().is_none_or(|ext| ext != "sql") {
            continue;
        }

        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_string)
            .ok_or_else(|| Error::Source(format!("{} is not valid UTF-8", path.display())))?;

        let (version, description) = parse_name(&name)?;
        let bytes = std::fs::read(&path)?;
        let sql = String::from_utf8(bytes)
            .map_err(|_| Error::Source(format!("{name} is not valid UTF-8")))?;
        let checksum = checksum(sql.as_bytes());

        let migration = Migration {
            version,
            description,
            path,
            sql,
            checksum,
        };

        if let Some(existing) = by_version.insert(version, migration) {
            return Err(Error::Source(format!(
                "version {version} is used by both {} and {name}",
                existing.path.display()
            )));
        }
    }

    Ok(by_version.into_values().collect())
}

/// Lowercase hex SHA-256.
pub fn checksum(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn parse_name(name: &str) -> Result<(i64, String)> {
    // Refused rather than treated as a forward migration, because running a
    // down file forward is the one mistake here that destroys data.
    if name.ends_with(".down.sql") || name.ends_with(".up.sql") {
        return Err(Error::Source(format!(
            "{name}: reversible migrations are not supported; use a plain <version>_<description>.sql"
        )));
    }

    let stem = name.trim_end_matches(".sql");
    let (version, description) = stem.split_once('_').unwrap_or((stem, ""));

    if version.is_empty() || !version.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Error::Source(format!(
            "{name}: expected <version>_<description>.sql with an integer version"
        )));
    }

    let version = version
        .parse()
        .map_err(|_| Error::Source(format!("{name}: version does not fit in an i64")))?;

    Ok((version, description.replace('_', " ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, body: &str) {
        std::fs::write(dir.join(name), body).unwrap();
    }

    #[test]
    fn reads_in_version_order_not_name_order() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "10_later.sql", "SELECT 10");
        write(dir.path(), "9_earlier.sql", "SELECT 9");

        let versions: Vec<_> = read_dir(dir.path())
            .unwrap()
            .into_iter()
            .map(|m| m.version)
            .collect();

        assert_eq!(versions, [9, 10]);
    }

    #[test]
    fn description_comes_from_the_name() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "20260923114500_create_trades_table.sql", "");

        let migration = &read_dir(dir.path()).unwrap()[0];

        assert_eq!(migration.version, 20260923114500);
        assert_eq!(migration.description, "create trades table");
    }

    #[test]
    fn checksum_matches_sha256sum() {
        // `printf 'SELECT 1;\n' | sha256sum`
        assert_eq!(
            checksum(b"SELECT 1;\n"),
            "b4e0497804e46e0a0b0b8c31975b062152d551bac49c3c2e80932567b4085dcd"
        );
    }

    #[test]
    fn other_files_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "README.md", "notes");
        write(dir.path(), "1_init.sql", "SELECT 1");

        assert_eq!(read_dir(dir.path()).unwrap().len(), 1);
    }

    #[test]
    fn unversioned_sql_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "init.sql", "SELECT 1");

        assert!(matches!(read_dir(dir.path()), Err(Error::Source(_))));
    }

    #[test]
    fn duplicate_versions_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "1_a.sql", "SELECT 1");
        write(dir.path(), "01_b.sql", "SELECT 1");

        let err = read_dir(dir.path()).unwrap_err();

        assert!(err.to_string().contains("version 1"), "{err}");
    }

    #[test]
    fn down_migrations_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "1_init.down.sql", "DROP TABLE t");

        assert!(matches!(read_dir(dir.path()), Err(Error::Source(_))));
    }

    #[test]
    fn a_missing_directory_is_not_found() {
        assert!(matches!(
            read_dir(Path::new("/nonexistent/chx/migrations")),
            Err(Error::NotFound(_))
        ));
    }
}
