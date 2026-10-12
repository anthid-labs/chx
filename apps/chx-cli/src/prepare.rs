//! `chx prepare`: the offline cache for compile-time checked queries.
//!
//! The macros learn a query's columns from the database when the crate
//! compiles. This runs that compile with `CHX_TMP` set, so every macro also
//! writes what it learned to one directory, then makes `.chx` in the working
//! directory exactly that set of files. A query that was removed loses its
//! file; one that changed gets a new one, since the file is named by the hash
//! of its text.
//!
//! The crate's own artifacts are cleaned first. A macro only runs when its
//! crate compiles, and an up-to-date crate would not, so the cache would miss
//! every query cargo saw no reason to revisit.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use chx::error::{Error, Result};
use chx_types::cache::{self, QueryData};

use crate::cli::PrepareArgs;

pub fn run(args: PrepareArgs) -> Result<()> {
    let members = workspace_members()?;
    let tmp = tempfile::tempdir()?;

    clean(&members)?;
    check(&args, tmp.path())?;

    let fresh: BTreeMap<String, QueryData> = cache::list(tmp.path())
        .map_err(|err| Error::Internal(format!("reading what the macros wrote: {err}")))?
        .into_iter()
        .map(|data| (data.hash.clone(), data))
        .collect();

    let dir = PathBuf::from(cache::DIR);
    if args.check {
        return compare(&dir, &fresh);
    }
    write(&dir, &fresh)?;
    println!("{} queries in {}", fresh.len(), dir.display());
    Ok(())
}

fn cargo() -> Command {
    Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
}

/// The packages in the workspace the working directory belongs to.
fn workspace_members() -> Result<Vec<String>> {
    let output = cargo()
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .output()?;
    if !output.status.success() {
        return Err(Error::Internal(format!(
            "cargo metadata failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    let metadata: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|err| Error::Internal(format!("cargo metadata: {err}")))?;
    Ok(metadata["packages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|package| package["name"].as_str().map(str::to_string))
        .collect())
}

fn clean(members: &[String]) -> Result<()> {
    let mut command = cargo();
    command.arg("clean");
    for member in members {
        command.args(["-p", member]);
    }
    let status = command.status()?;
    if !status.success() {
        return Err(Error::Internal("cargo clean failed".to_string()));
    }
    Ok(())
}

fn check(args: &PrepareArgs, tmp: &Path) -> Result<()> {
    let mut command = cargo();
    command.arg("check");
    if args.cargo_args.is_empty() {
        command.arg("--all-targets");
    } else {
        command.args(&args.cargo_args);
    }
    command
        .env("CHX_TMP", tmp)
        .env("CHX_OFFLINE", "false")
        .env("CLICKHOUSE_URL", &args.clickhouse_url);
    let status = command.status()?;
    if !status.success() {
        return Err(Error::Internal(
            "cargo check failed; fix the errors above and run again".to_string(),
        ));
    }
    Ok(())
}

/// Makes `dir` hold exactly `fresh`: stale files go, changed and new ones
/// are written, unchanged ones are left alone so their mtime says so.
fn write(dir: &Path, fresh: &BTreeMap<String, QueryData>) -> Result<()> {
    let existing =
        cache::list(dir).map_err(|err| Error::Internal(format!("{}: {err}", dir.display())))?;
    for data in existing {
        match fresh.get(&data.hash) {
            Some(new) if *new == data => {}
            _ => std::fs::remove_file(dir.join(data.file_name()))?,
        }
    }
    for data in fresh.values() {
        let path = dir.join(data.file_name());
        if !path.exists() {
            data.save_in(dir)?;
        }
    }
    Ok(())
}

fn compare(dir: &Path, fresh: &BTreeMap<String, QueryData>) -> Result<()> {
    let existing: BTreeMap<String, QueryData> = cache::list(dir)
        .map_err(|err| Error::Internal(format!("{}: {err}", dir.display())))?
        .into_iter()
        .map(|data| (data.hash.clone(), data))
        .collect();

    let mut problems = Vec::new();
    for (hash, data) in fresh {
        match existing.get(hash) {
            None => problems.push(format!("missing: {}", cache::file_name(hash))),
            Some(old) if old != data => problems.push(format!("stale: {}", cache::file_name(hash))),
            Some(_) => {}
        }
    }
    for hash in existing.keys() {
        if !fresh.contains_key(hash) {
            problems.push(format!("no longer used: {}", cache::file_name(hash)));
        }
    }

    if problems.is_empty() {
        println!("{} is up to date ({} queries)", dir.display(), fresh.len());
        Ok(())
    } else {
        Err(Error::Internal(format!(
            "{} is out of date; run `chx prepare`:\n  {}",
            dir.display(),
            problems.join("\n  ")
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chx_types::cache::{Describe, Field};

    fn data(query: &str, ty: &str) -> QueryData {
        QueryData::new(
            query,
            Describe {
                parameters: vec![],
                columns: vec![Field {
                    name: "x".to_string(),
                    ty: ty.to_string(),
                }],
            },
        )
    }

    #[test]
    fn write_makes_the_directory_exactly_the_fresh_set() {
        let dir = tempfile::tempdir().unwrap();
        let old = data("SELECT 1", "UInt8");
        let changed_before = data("SELECT 2", "UInt8");
        let changed_after = data("SELECT 2", "UInt16");
        old.save_in(dir.path()).unwrap();
        changed_before.save_in(dir.path()).unwrap();

        let fresh: BTreeMap<_, _> = [changed_after.clone(), data("SELECT 3", "UInt8")]
            .into_iter()
            .map(|d| (d.hash.clone(), d))
            .collect();
        write(dir.path(), &fresh).unwrap();

        let listed: Vec<_> = cache::list(dir.path())
            .unwrap()
            .into_iter()
            .map(|d| d.query)
            .collect();
        assert_eq!(listed.len(), 2);
        assert!(listed.contains(&"SELECT 2".to_string()));
        assert!(listed.contains(&"SELECT 3".to_string()));
        assert_eq!(
            cache::list(dir.path())
                .unwrap()
                .into_iter()
                .find(|d| d.query == "SELECT 2")
                .unwrap(),
            changed_after
        );
    }

    #[test]
    fn compare_names_every_kind_of_difference() {
        let dir = tempfile::tempdir().unwrap();
        data("SELECT 1", "UInt8").save_in(dir.path()).unwrap();
        data("SELECT 2", "UInt8").save_in(dir.path()).unwrap();

        let fresh: BTreeMap<_, _> = [data("SELECT 2", "UInt16"), data("SELECT 3", "UInt8")]
            .into_iter()
            .map(|d| (d.hash.clone(), d))
            .collect();
        let err = compare(dir.path(), &fresh).unwrap_err().to_string();
        assert!(err.contains("stale:"), "{err}");
        assert!(err.contains("missing:"), "{err}");
        assert!(err.contains("no longer used:"), "{err}");

        let same: BTreeMap<_, _> = [data("SELECT 1", "UInt8"), data("SELECT 2", "UInt8")]
            .into_iter()
            .map(|d| (d.hash.clone(), d))
            .collect();
        compare(dir.path(), &same).unwrap();
    }
}
