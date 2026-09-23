//! The history table: what has run, with which checksum, and how far it got.
//!
//! # Where the table lives
//!
//! On a single server it is a local table. On a cluster every node has to see
//! the same history, or a run through another node starts again from the top.
//! Three cases, tried in this order:
//!
//! 1. The table already exists. Its engine decides, whatever was asked for:
//!    converting a history table in place is not something to do implicitly.
//! 2. The database uses the `Replicated` engine (or `Shared`, on ClickHouse
//!    Cloud). A multi-shard `Replicated` database is the one case with no
//!    cluster-wide option
//!    when its nodes sit on different shards, and chx warns. ClickHouse replicates the
//!    table and its data by itself, so a plain `CREATE` is right and
//!    `ON CLUSTER` would be refused.
//! 3. A cluster was named. chx tries `ON CLUSTER` with a replicated engine,
//!    and falls back to a local table with a warning if the cluster cannot
//!    take it: no Keeper, no `{replica}` macro, or no such cluster. A
//!    migration tool that refused to run on a server without Keeper would be
//!    refusing the common case.

use crate::client::Client;
use crate::error::{Error, Result};

/// The table chx keeps its history in, inside the target database.
pub const TABLE: &str = "_chx_migrations";

/// The columns, shared by every way the table is created.
///
/// `statements` is recorded so an incomplete row says how far it had left to
/// go without anyone opening the file.
const COLUMNS: &str = "(
    version Int64,
    description String,
    checksum String,
    statements UInt32,
    applied UInt32,
    success Bool,
    execution_ms UInt64,
    installed_at DateTime64(3, 'UTC') DEFAULT now64(3)
)";

/// One row per migration, and progress is written as a new row after every
/// statement rather than an update.
///
/// `ReplacingMergeTree(applied)` keeps the row with the most statements
/// applied, and on a tie the one inserted last, which is the latest attempt.
/// Reads use `FINAL`, so the answer does not depend on whether a background
/// merge has run yet. The table stays tiny, so `FINAL` costs nothing.
const LOCAL_ENGINE: &str = "ReplacingMergeTree(applied)";

/// The same engine, replicated to every node of the cluster.
///
/// The Keeper path has no `{shard}`: history is one list for the whole
/// cluster, not one per shard. That makes `{replica}` a cluster-wide name, so
/// it has to be unique across shards, which it is when it is the host name.
/// `{database}` keeps two databases on one cluster apart.
const REPLICATED_ENGINE: &str = "ReplicatedReplacingMergeTree('/clickhouse/chx/{database}/_chx_migrations', '{replica}', applied)";

/// The replicated engine inside a `Replicated` database, which supplies the
/// Keeper path and replica name itself.
const DATABASE_REPLICATED_ENGINE: &str = "ReplicatedReplacingMergeTree(applied)";

/// How the history table is stored, which decides how it is read and how far
/// the run lock has to reach.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Placement {
    /// On the connected server only.
    Local,
    /// Shared by a database that replicates its own DDL: a `Replicated`
    /// database, or ClickHouse Cloud. Plain DDL already reaches every node.
    Replicated,
    /// Replicated through Keeper by `ON CLUSTER`, so DDL has to say
    /// `ON CLUSTER` too.
    OnCluster(String),
}

/// The latest row for one version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub version: i64,
    pub description: String,
    pub checksum: String,
    pub statements: u32,
    /// How many statements, counted from the top of the file, have run.
    pub applied: u32,
    /// Every statement ran. `false` means the migration stopped partway and
    /// the next run resumes it at statement `applied + 1`.
    pub success: bool,
}

/// Creates the history table if it is missing, and reports how it is stored.
/// See the module documentation for which placement wins.
pub async fn ensure_table(client: &Client, cluster: Option<&str>) -> Result<Placement> {
    let database_engine = database_engine(client).await?;

    // The nodes that should all share the history: the named cluster, or for
    // a `Replicated` database, the cluster it lists itself as.
    let peers = match (database_engine.as_str(), cluster) {
        ("Replicated", _) => Some(Peers::Database),
        (_, Some(cluster)) => Some(Peers::Cluster(cluster)),
        _ => None,
    };

    let engine = match table_engine(client).await? {
        Some(engine) => {
            if cluster.is_some() && !is_replicated(&engine) {
                tracing::warn!(
                    "{TABLE} already exists as a local {engine} table; the cluster setting only \
                     applies when chx creates it"
                );
            }
            engine
        }
        None => {
            create(client, &database_engine, cluster).await?;

            // Read back rather than assumed: an `ON CLUSTER` that failed on
            // other hosts may still have created the table here.
            table_engine(client).await?.ok_or_else(|| {
                Error::Internal(format!("{TABLE} is missing right after creating it"))
            })?
        }
    };

    if engine.starts_with("Replicated")
        && let Some(peers) = peers
    {
        warn_if_not_shared(client, peers).await?;
    }

    Ok(placement(&engine, &database_engine, cluster))
}

async fn create(client: &Client, database_engine: &str, cluster: Option<&str>) -> Result<()> {
    let create = |on_cluster: &str, engine: &str| {
        format!(
            "CREATE TABLE IF NOT EXISTS {TABLE}{on_cluster} {COLUMNS} \
             ENGINE = {engine} ORDER BY version"
        )
    };
    let local = create("", LOCAL_ENGINE);

    match (database_engine, cluster) {
        // The database replicates the DDL, but not the data unless the engine
        // asks for it. No path arguments: a `Replicated` database refuses
        // explicit ones and fills in its own.
        ("Replicated", _) => {
            client
                .execute(&create("", DATABASE_REPLICATED_ENGINE))
                .await
        }
        // ClickHouse Cloud turns a plain engine into a shared one by itself,
        // and ignores `ON CLUSTER`.
        ("Shared", _) | (_, None) => client.execute(&local).await,
        (_, Some(cluster)) => {
            let on_cluster = create(
                &format!(" ON CLUSTER {}", quote_identifier(cluster)),
                REPLICATED_ENGINE,
            );

            // Only a refusal from the server falls back. A connection that
            // broke is not evidence the cluster cannot take the table, and
            // falling back then would split the history for no reason.
            match client.execute(&on_cluster).await {
                Err(err @ Error::ClickHouse { .. }) => {
                    tracing::warn!(
                        %err,
                        "could not create {TABLE} on cluster {cluster}; using a local table, so \
                         other nodes will not see this history"
                    );
                    client.execute(&local).await
                }
                other => other,
            }
        }
    }
}

/// Every recorded version, in version order.
pub async fn read(client: &Client, placement: &Placement) -> Result<Vec<Record>> {
    // A replica serves what it has fetched, which can trail the node that ran
    // the last migration. Reading a stale history would re-run a migration
    // that already applied, so catch up first.
    if *placement != Placement::Local {
        client
            .execute(&format!("SYSTEM SYNC REPLICA {TABLE}"))
            .await?;
    }

    let body = client
        .query(&format!(
            "SELECT version, checksum, statements, applied, success, description \
             FROM {TABLE} FINAL ORDER BY version FORMAT TabSeparated"
        ))
        .await?;

    body.lines()
        .filter(|line| !line.is_empty())
        .map(parse_row)
        .collect()
}

async fn table_engine(client: &Client) -> Result<Option<String>> {
    let body = client
        .query(&format!(
            "SELECT engine FROM system.tables \
             WHERE database = currentDatabase() AND name = '{TABLE}' FORMAT TabSeparated"
        ))
        .await?;

    Ok(body.lines().next().map(str::to_string))
}

async fn database_engine(client: &Client) -> Result<String> {
    let body = client
        .query(
            "SELECT engine FROM system.databases \
             WHERE name = currentDatabase() FORMAT TabSeparated",
        )
        .await?;

    Ok(body.trim().to_string())
}

/// Which nodes are expected to share the history.
#[derive(Debug, Clone, Copy)]
enum Peers<'a> {
    /// A `Replicated` database, which lists its replicas in `system.clusters`
    /// under its own name.
    Database,
    Cluster(&'a str),
}

/// Warns when the history reaches fewer nodes than the cluster has.
///
/// Asked of Keeper rather than inferred from configuration, because the
/// configuration is not what decides it. Two ways it happens:
///
/// - A `Replicated` database fills the default table path from each server's
///   own `{shard}` macro, so nodes on different shards keep separate
///   histories. It also refuses an explicit path that would join them.
/// - `ON CLUSTER` with a `{replica}` macro that is not unique across shards.
///
/// A warning rather than a refusal: running through one node, or one shard,
/// works, and refusing would lock these setups out entirely.
async fn warn_if_not_shared(client: &Client, peers: Peers<'_>) -> Result<()> {
    let cluster = match peers {
        Peers::Database => "currentDatabase()".to_string(),
        Peers::Cluster(cluster) => quote(cluster),
    };
    let body = client
        .query(&format!(
            "SELECT \
               (SELECT count() FROM system.clusters WHERE cluster = {cluster}), \
               (SELECT total_replicas FROM system.replicas \
                WHERE database = currentDatabase() AND table = '{TABLE}') \
             FORMAT TabSeparated"
        ))
        .await?;

    let mut counts = body.split_whitespace().map(str::parse::<u64>);
    if let (Some(Ok(nodes)), Some(Ok(replicas))) = (counts.next(), counts.next())
        && replicas < nodes
    {
        tracing::warn!(
            nodes,
            replicas,
            "{TABLE} is replicated to {replicas} of {nodes} nodes; a run through a node \
                 outside that set will not see this history"
        );
    }

    Ok(())
}

/// `Shared*` is ClickHouse Cloud, where every replica reads one copy of the
/// data but can still serve a stale view of it, so it is read like a
/// replicated table.
fn is_replicated(engine: &str) -> bool {
    engine.starts_with("Replicated") || engine.starts_with("Shared")
}

/// Decided from what exists, not from what was asked for: a table that is
/// already there keeps whatever it was created as.
fn placement(engine: &str, database_engine: &str, cluster: Option<&str>) -> Placement {
    if !is_replicated(engine) {
        return Placement::Local;
    }
    if matches!(database_engine, "Replicated" | "Shared") {
        return Placement::Replicated;
    }

    match cluster {
        Some(cluster) => Placement::OnCluster(cluster.to_string()),
        None => {
            // Created `ON CLUSTER` by an earlier run, but this one was not told
            // the cluster, so it cannot reach the other nodes with DDL.
            tracing::warn!(
                "{TABLE} is replicated but no cluster was given, so the run lock only covers \
                 this node; pass the cluster used to create it"
            );
            Placement::Replicated
        }
    }
}

/// Appends a progress row. `execution_ms` is the time spent on this migration
/// in the current run only.
pub async fn record(client: &Client, record: &Record, execution_ms: u64) -> Result<()> {
    let sql = format!(
        "INSERT INTO {TABLE} \
         (version, description, checksum, statements, applied, success, execution_ms) \
         VALUES ({}, {}, {}, {}, {}, {}, {})",
        record.version,
        quote(&record.description),
        quote(&record.checksum),
        record.statements,
        record.applied,
        record.success,
        execution_ms,
    );

    client.execute(&sql).await
}

fn parse_row(line: &str) -> Result<Record> {
    let bad = || Error::Internal(format!("unreadable row in {TABLE}: {line:?}"));
    let mut fields = line.splitn(6, '\t');
    let mut next = || fields.next().ok_or_else(bad);

    let version = next()?.parse().map_err(|_| bad())?;
    let checksum = next()?.to_string();
    let statements = next()?.parse().map_err(|_| bad())?;
    let applied = next()?.parse().map_err(|_| bad())?;
    let success = match next()? {
        "true" => true,
        "false" => false,
        _ => return Err(bad()),
    };
    let description = unescape(next()?);

    Ok(Record {
        version,
        description,
        checksum,
        statements,
        applied,
        success,
    })
}

/// A backquoted ClickHouse identifier.
pub(crate) fn quote_identifier(value: &str) -> String {
    format!("`{}`", value.replace('\\', "\\\\").replace('`', "\\`"))
}

/// A single-quoted ClickHouse string literal.
pub(crate) fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// Reverses the escaping `TabSeparated` applies to a string field.
pub(crate) fn unescape(field: &str) -> String {
    let mut out = String::with_capacity(field.len());
    let mut chars = field.chars();

    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('0') => out.push('\0'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_tab_separated_row() {
        let row = parse_row("7\tabc123\t4\t2\tfalse\tcreate trades").unwrap();

        assert_eq!(
            row,
            Record {
                version: 7,
                description: "create trades".to_string(),
                checksum: "abc123".to_string(),
                statements: 4,
                applied: 2,
                success: false,
            }
        );
    }

    #[test]
    fn an_escaped_description_round_trips() {
        let row = parse_row("1\tx\t1\t1\ttrue\tit\\'s a\\ttab").unwrap();

        assert_eq!(row.description, "it's a\ttab");
    }

    #[test]
    fn an_empty_description_is_fine() {
        let row = parse_row("1\tx\t0\t0\ttrue\t").unwrap();

        assert_eq!(row.description, "");
    }

    #[test]
    fn a_short_row_is_an_error_not_a_default() {
        assert!(parse_row("1\tx\t0").is_err());
    }

    #[test]
    fn quoting_escapes_quotes_and_backslashes() {
        assert_eq!(quote(r"it's a \ path"), r"'it\'s a \\ path'");
    }

    #[test]
    fn identifiers_are_backquoted() {
        assert_eq!(quote_identifier("prod"), "`prod`");
        assert_eq!(quote_identifier("we`ird"), r"`we\`ird`");
    }

    #[test]
    fn placement_follows_the_table_and_the_database() {
        assert_eq!(
            placement("ReplacingMergeTree", "Atomic", Some("prod")),
            Placement::Local
        );
        assert_eq!(
            placement("ReplicatedReplacingMergeTree", "Atomic", Some("prod")),
            Placement::OnCluster("prod".to_string())
        );
        assert_eq!(
            placement("ReplicatedReplacingMergeTree", "Replicated", Some("prod")),
            Placement::Replicated
        );
        assert_eq!(
            placement("SharedReplacingMergeTree", "Shared", None),
            Placement::Replicated
        );
    }
}
