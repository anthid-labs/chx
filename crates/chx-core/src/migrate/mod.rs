//! Applying migrations.
//!
//! A run does three things in order, and the first two touch nothing:
//!
//! 1. Read every file in the migrations directory ([`source`]).
//! 2. Read the history table and check it against those files ([`plan`]). Any
//!    disagreement stops the run here, before a statement is sent.
//! 3. Apply what is left, one statement at a time, writing a history row after
//!    each.
//!
//! # What the history check refuses
//!
//! - A file that ran whose checksum has since changed. The schema is whatever
//!   the old file did, and the repository now says otherwise.
//! - A version in the history with no file. Whoever reads the repository next
//!   cannot see what built the schema.
//! - A new file numbered below the latest applied version. Usually a branch
//!   merged after a later one; renumber it above the latest.
//!
//! # Partial migrations
//!
//! ClickHouse DDL is not transactional, so a migration that fails on its third
//! statement has already applied two. The history records that: the row for
//! the migration says two statements applied and `success = false`. The next
//! run skips those two and starts at the third, which is the one that failed.
//!
//! The checksum is not enforced on an incomplete migration, because fixing the
//! statement that failed is the normal next step. The statements before it
//! must stay as they are, since they are not run again. Editing one of them
//! changes nothing in the database, and chx cannot tell.

pub mod history;
pub mod source;
pub mod split;

use std::path::Path;
use std::time::{Duration, Instant};

use crate::client::Client;
use crate::error::{Error, Result};

pub use history::Record;
pub use source::Migration;

/// A loaded migrations directory.
#[derive(Debug, Clone)]
pub struct Migrator {
    migrations: Vec<Migration>,
}

/// One migration this run brought to completion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    pub version: i64,
    pub description: String,
    /// Statements in the file.
    pub statements: usize,
    /// Statements skipped because an earlier, failed run had already applied
    /// them. Zero for a migration that ran from the top.
    pub resumed_at: usize,
    pub elapsed: Duration,
}

/// One migration the run will execute, and where in it to start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step<'a> {
    pub migration: &'a Migration,
    pub statements: Vec<String>,
    /// Statements to skip, because they already ran.
    pub resume_at: usize,
}

impl Migrator {
    pub fn from_dir(dir: impl AsRef<Path>) -> Result<Self> {
        Ok(Self {
            migrations: source::read_dir(dir.as_ref())?,
        })
    }

    pub fn migrations(&self) -> &[Migration] {
        &self.migrations
    }

    /// Applies every pending migration, calling `on_applied` as each one
    /// finishes so a caller can report progress while a long run continues.
    ///
    /// Stops at the first failing statement with [`Error::Statement`]. What
    /// ran before it is recorded, and running again resumes there.
    pub async fn run(
        &self,
        client: &Client,
        mut on_applied: impl FnMut(&Applied),
    ) -> Result<Vec<Applied>> {
        history::ensure_table(client).await?;
        let recorded = history::read(client).await?;
        let steps = plan(&self.migrations, &recorded)?;

        let mut done = Vec::with_capacity(steps.len());
        for step in steps {
            let applied = apply(client, &step).await?;
            on_applied(&applied);
            done.push(applied);
        }

        Ok(done)
    }
}

/// Checks the history against the files and returns what is left to run, in
/// order. Pure, so every refusal is testable without a server.
pub fn plan<'a>(migrations: &'a [Migration], recorded: &[Record]) -> Result<Vec<Step<'a>>> {
    let find = |version: i64| migrations.iter().find(|m| m.version == version);
    let latest = recorded.iter().map(|r| r.version).max();
    let mut steps = Vec::new();

    for record in recorded {
        let Some(migration) = find(record.version) else {
            return Err(Error::History(format!(
                "migration {} ({}) was applied but its file is missing",
                record.version, record.description
            )));
        };

        if record.success {
            if record.checksum != migration.checksum {
                return Err(Error::History(format!(
                    "migration {} ({}) was applied with checksum {}, but {} now has checksum {}. \
                     Restore the file and put the change in a new migration.",
                    record.version,
                    record.description,
                    record.checksum,
                    migration.path.display(),
                    migration.checksum
                )));
            }
            continue;
        }

        // A run stops at its first failure, so an incomplete migration can
        // only be the newest one. Anything else means the table was edited
        // by hand or written by something that is not chx.
        if Some(record.version) != latest {
            return Err(Error::History(format!(
                "migration {} ({}) is incomplete but later migrations have run since; \
                 the history table has been changed outside chx",
                record.version, record.description
            )));
        }

        let statements = split::statements(&migration.sql);
        let resume_at = record.applied as usize;

        if resume_at > statements.len() {
            return Err(Error::History(format!(
                "migration {} ({}) had {} statements applied, but {} now has only {}",
                record.version,
                record.description,
                resume_at,
                migration.path.display(),
                statements.len()
            )));
        }

        if record.checksum != migration.checksum {
            tracing::warn!(
                version = record.version,
                resume_at = resume_at + 1,
                "incomplete migration has changed since it failed; the first {resume_at} \
                 statements are not run again"
            );
        }

        steps.push(Step {
            migration,
            statements,
            resume_at,
        });
    }

    for migration in migrations {
        if recorded.iter().any(|r| r.version == migration.version) {
            continue;
        }

        if let Some(latest) = latest.filter(|&latest| migration.version < latest) {
            return Err(Error::History(format!(
                "migration {} ({}) has not run but is numbered below {latest}, which has. \
                 Renumber it above {latest}.",
                migration.version, migration.description
            )));
        }

        steps.push(Step {
            migration,
            statements: split::statements(&migration.sql),
            resume_at: 0,
        });
    }

    Ok(steps)
}

async fn apply(client: &Client, step: &Step<'_>) -> Result<Applied> {
    let migration = step.migration;
    let total = step.statements.len();
    let started = Instant::now();

    let progress = |applied: usize| Record {
        version: migration.version,
        description: migration.description.clone(),
        checksum: migration.checksum.clone(),
        statements: total as u32,
        applied: applied as u32,
        success: applied == total,
    };
    let elapsed_ms = || started.elapsed().as_millis() as u64;

    for (index, sql) in step.statements.iter().enumerate().skip(step.resume_at) {
        tracing::info!(
            version = migration.version,
            statement = index + 1,
            total,
            "running statement"
        );

        client
            .execute(sql)
            .await
            .map_err(|source| Error::Statement {
                version: migration.version,
                description: migration.description.clone(),
                statement: index + 1,
                total,
                source: Box::new(source),
            })?;

        // If this write fails the statement has run but is not recorded, and
        // the next attempt will run it again. There is no ordering that avoids
        // that without transactions; the error at least says which statement.
        history::record(client, &progress(index + 1), elapsed_ms()).await?;
    }

    // An empty file, or a resumed one whose remaining statements were edited
    // away, runs nothing in the loop and still has to be marked complete.
    if step.resume_at == total {
        history::record(client, &progress(total), elapsed_ms()).await?;
    }

    Ok(Applied {
        version: migration.version,
        description: migration.description.clone(),
        statements: total,
        resumed_at: step.resume_at,
        elapsed: started.elapsed(),
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn migration(version: i64, sql: &str) -> Migration {
        Migration {
            version,
            description: format!("m{version}"),
            path: PathBuf::from(format!("migrations/{version}_m{version}.sql")),
            sql: sql.to_string(),
            checksum: source::checksum(sql.as_bytes()),
        }
    }

    fn done(migration: &Migration) -> Record {
        let statements = split::statements(&migration.sql).len() as u32;
        Record {
            version: migration.version,
            description: migration.description.clone(),
            checksum: migration.checksum.clone(),
            statements,
            applied: statements,
            success: true,
        }
    }

    fn versions(steps: &[Step<'_>]) -> Vec<i64> {
        steps.iter().map(|s| s.migration.version).collect()
    }

    #[test]
    fn a_fresh_database_runs_everything_in_order() {
        let files = [migration(1, "SELECT 1"), migration(2, "SELECT 2")];

        assert_eq!(versions(&plan(&files, &[]).unwrap()), [1, 2]);
    }

    #[test]
    fn applied_migrations_are_skipped_and_new_ones_appended() {
        let files = [migration(1, "SELECT 1"), migration(2, "SELECT 2")];

        let steps = plan(&files, &[done(&files[0])]).unwrap();

        assert_eq!(versions(&steps), [2]);
    }

    #[test]
    fn up_to_date_is_an_empty_plan() {
        let files = [migration(1, "SELECT 1")];

        assert!(plan(&files, &[done(&files[0])]).unwrap().is_empty());
    }

    #[test]
    fn an_edited_applied_file_is_refused() {
        let files = [migration(1, "SELECT 1")];
        let mut record = done(&files[0]);
        record.checksum = source::checksum(b"SELECT 0");

        let err = plan(&files, &[record]).unwrap_err();

        assert!(matches!(err, Error::History(_)));
        assert!(err.to_string().contains("checksum"), "{err}");
    }

    #[test]
    fn a_missing_applied_file_is_refused() {
        let gone = migration(1, "SELECT 1");
        let files = [migration(2, "SELECT 2")];

        let err = plan(&files, &[done(&gone)]).unwrap_err();

        assert!(err.to_string().contains("missing"), "{err}");
    }

    #[test]
    fn a_new_file_below_the_latest_is_refused() {
        let files = [migration(1, "SELECT 1"), migration(3, "SELECT 3")];

        let err = plan(&files, &[done(&files[1])]).unwrap_err();

        assert!(err.to_string().contains("Renumber"), "{err}");
    }

    #[test]
    fn an_incomplete_migration_resumes_where_it_stopped() {
        let files = [migration(1, "SELECT 1; SELECT 2; SELECT 3")];
        let mut record = done(&files[0]);
        record.applied = 2;
        record.success = false;

        let steps = plan(&files, &[record]).unwrap();

        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].resume_at, 2);
        assert_eq!(steps[0].statements[steps[0].resume_at], "SELECT 3");
    }

    #[test]
    fn an_incomplete_migration_may_be_fixed_before_resuming() {
        let failed = migration(1, "SELECT 1; SELEC 2");
        let fixed = [migration(1, "SELECT 1; SELECT 2")];
        let mut record = done(&failed);
        record.applied = 1;
        record.success = false;

        let steps = plan(&fixed, &[record]).unwrap();

        assert_eq!(steps[0].resume_at, 1);
        assert_eq!(steps[0].statements[1], "SELECT 2");
    }

    #[test]
    fn an_incomplete_migration_runs_before_new_ones() {
        let files = [migration(1, "SELECT 1; SELECT 2"), migration(2, "SELECT 3")];
        let mut record = done(&files[0]);
        record.applied = 1;
        record.success = false;

        assert_eq!(versions(&plan(&files, &[record]).unwrap()), [1, 2]);
    }

    #[test]
    fn an_incomplete_migration_that_is_not_the_latest_is_refused() {
        let files = [migration(1, "SELECT 1; SELECT 2"), migration(2, "SELECT 3")];
        let mut first = done(&files[0]);
        first.applied = 1;
        first.success = false;

        let err = plan(&files, &[first, done(&files[1])]).unwrap_err();

        assert!(err.to_string().contains("outside chx"), "{err}");
    }

    #[test]
    fn an_incomplete_migration_cut_below_what_ran_is_refused() {
        let files = [migration(1, "SELECT 1")];
        let mut record = done(&migration(1, "SELECT 1; SELECT 2; SELECT 3"));
        record.applied = 2;
        record.success = false;

        assert!(matches!(plan(&files, &[record]), Err(Error::History(_))));
    }
}
