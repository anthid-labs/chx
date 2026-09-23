//! One run at a time.
//!
//! ClickHouse has no advisory locks and no unique constraints. What it does
//! have is `CREATE TABLE` without `IF NOT EXISTS`, which fails with
//! `TABLE_ALREADY_EXISTS` for everyone but the first caller. The lock is a
//! table: whoever created `_chx_lock` holds it, and dropping it releases it.
//!
//! On a cluster the create goes `ON CLUSTER`. Distributed DDL is applied in one
//! queue order on every node, so two runs racing through different nodes still
//! get exactly one winner. A `Replicated` database and ClickHouse Cloud
//! serialise DDL themselves, so there a plain create is already cluster-wide.
//!
//! The table uses the `Null` engine: it never holds data, only metadata. Its
//! comment says who took it, and `system.tables` says when.
//!
//! # A run that dies holding the lock
//!
//! The lock stays. Nothing expires it on a timer, because a migration running
//! a long mutation looks exactly like a dead one from the outside, and
//! breaking its lock would let a second run start on top of it. The next run
//! waits, then fails naming the holder, and [`unlock`] clears it once someone
//! has checked that run is gone.

use std::time::{Duration, Instant};

use crate::client::Client;
use crate::error::{Error, Result};
use crate::migrate::history::{self, Placement};

/// The lock table, inside the target database.
pub const TABLE: &str = "_chx_lock";

/// ClickHouse's `TABLE_ALREADY_EXISTS`.
const TABLE_ALREADY_EXISTS: u32 = 57;

/// How often a waiting run tries again.
const POLL: Duration = Duration::from_secs(1);

/// A held lock. Release it with [`Lock::release`].
#[derive(Debug)]
#[must_use = "a lock that is never released blocks every later run"]
pub struct Lock<'a> {
    client: &'a Client,
    on_cluster: String,
}

/// Who holds the lock, as recorded when it was taken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holder {
    /// Host, process id and chx version of the run that took it.
    pub description: String,
    /// When it was taken, UTC, as the server records table metadata.
    pub since: String,
}

/// Takes the lock, waiting up to `timeout` for another run to release it.
///
/// A zero timeout tries once. Fails with [`Error::Locked`] naming the holder
/// if the wait runs out.
pub async fn acquire<'a>(
    client: &'a Client,
    placement: &Placement,
    timeout: Duration,
) -> Result<Lock<'a>> {
    let on_cluster = on_cluster(placement);
    let create = format!(
        "CREATE TABLE {TABLE}{on_cluster} (held UInt8) ENGINE = Null COMMENT {}",
        history::quote(&holder_description())
    );
    let started = Instant::now();
    let mut announced = false;

    loop {
        match client.execute(&create).await {
            Ok(()) => return Ok(Lock { client, on_cluster }),
            Err(Error::ClickHouse {
                code: Some(TABLE_ALREADY_EXISTS),
                ..
            }) => {}
            Err(err) => return Err(err),
        }

        // Released between the create and this read: try again straight away.
        let Some(holder) = holder(client).await? else {
            continue;
        };

        if started.elapsed() >= timeout {
            return Err(Error::Locked {
                holder: holder.description,
                since: holder.since,
            });
        }

        if !announced {
            tracing::warn!(
                holder = %holder.description,
                since = %holder.since,
                timeout_s = timeout.as_secs(),
                "waiting for another run to release the migration lock"
            );
            announced = true;
        }

        tokio::time::sleep(POLL.min(timeout.saturating_sub(started.elapsed()))).await;
    }
}

impl Lock<'_> {
    pub async fn release(self) -> Result<()> {
        drop_lock(self.client, &self.on_cluster).await
    }
}

/// Releases the lock whoever holds it, and returns who that was, or `None`
/// if nobody did.
///
/// For clearing the lock of a run that died. Calling it while a run is live
/// lets a second run start on top of the first, so check first.
pub async fn unlock(client: &Client, placement: &Placement) -> Result<Option<Holder>> {
    let holder = holder(client).await?;
    if holder.is_some() {
        drop_lock(client, &on_cluster(placement)).await?;
    }

    Ok(holder)
}

/// The current holder, read from the node `client` is connected to.
pub async fn holder(client: &Client) -> Result<Option<Holder>> {
    let body = client
        .query(&format!(
            "SELECT comment, metadata_modification_time FROM system.tables \
             WHERE database = currentDatabase() AND name = '{TABLE}' \
             SETTINGS session_timezone = 'UTC' FORMAT TabSeparated"
        ))
        .await?;

    Ok(body.lines().next().map(|line| {
        let (description, since) = line.split_once('\t').unwrap_or((line, ""));
        Holder {
            description: history::unescape(description),
            since: since.to_string(),
        }
    }))
}

async fn drop_lock(client: &Client, on_cluster: &str) -> Result<()> {
    client
        .execute(&format!("DROP TABLE IF EXISTS {TABLE}{on_cluster} SYNC"))
        .await
}

fn on_cluster(placement: &Placement) -> String {
    match placement {
        Placement::OnCluster(cluster) => {
            format!(" ON CLUSTER {}", history::quote_identifier(cluster))
        }
        Placement::Local | Placement::Replicated => String::new(),
    }
}

/// Enough to find the run that holds the lock: which machine, which process.
fn holder_description() -> String {
    let host = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .or_else(|| std::env::var("HOSTNAME").ok())
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .map(|host| host.trim().to_string())
        .filter(|host| !host.is_empty())
        .unwrap_or_else(|| "unknown host".to_string());

    format!(
        "chx {} on {host}, pid {}",
        env!("CARGO_PKG_VERSION"),
        std::process::id()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cluster_lock_goes_on_cluster() {
        assert_eq!(
            on_cluster(&Placement::OnCluster("prod".to_string())),
            " ON CLUSTER `prod`"
        );
        assert_eq!(on_cluster(&Placement::Replicated), "");
        assert_eq!(on_cluster(&Placement::Local), "");
    }

    #[test]
    fn the_holder_names_the_process() {
        let description = holder_description();

        assert!(description.starts_with("chx "), "{description}");
        assert!(
            description.ends_with(&format!("pid {}", std::process::id())),
            "{description}"
        );
    }
}
