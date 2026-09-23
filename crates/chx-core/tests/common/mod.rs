//! Shared setup for the live tests: the compose stack and scratch databases.
//!
//! Every test binary that includes this uses a different subset of it.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use chx::client::Client;

/// The `single` service in `docker/compose.yaml`.
pub const SINGLE: &str = "http://default:chx@127.0.0.1:58123";
/// The two cluster nodes, on different shards.
pub const CH_1: &str = "http://default:chx@127.0.0.1:58124";
pub const CH_2: &str = "http://default:chx@127.0.0.1:58125";
pub const CLUSTER: &str = "chx";

/// Services already brought up by this test binary.
static STARTED: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());

/// Starts the named compose services and waits for their health checks.
///
/// Left running afterwards: `up` on a running stack is a no-op, so the next
/// `cargo test` starts in a second rather than a minute. Every test works in
/// its own database, so nothing leaks between runs.
pub fn compose_up(services: &[&'static str]) {
    let mut started = STARTED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let missing: Vec<_> = services
        .iter()
        .filter(|service| !started.contains(service))
        .copied()
        .collect();
    if missing.is_empty() {
        return;
    }

    let output = Command::new("docker")
        .args(["compose", "-f"])
        .arg(compose_file())
        .args(["up", "-d", "--wait"])
        .args(&missing)
        .output()
        .expect("run docker compose; is Docker installed?");

    assert!(
        output.status.success(),
        "docker compose up {missing:?} failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    started.extend(missing);
}

fn compose_file() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docker/compose.yaml")
}

pub fn client(url: &str) -> Client {
    Client::from_url(url).expect("test url parses")
}

/// A fresh database, dropped by [`Scratch::drop`].
///
/// With a cluster, it is created and dropped `ON CLUSTER`, and `on` holds a
/// client for each node.
pub struct Scratch {
    servers: Vec<Client>,
    cluster: Option<&'static str>,
    pub name: String,
    pub on: Vec<Client>,
}

impl Scratch {
    pub async fn new(url: &str, test: &str) -> Self {
        Self::create(&[url], None, test, "").await
    }

    pub async fn on_cluster(urls: &[&str], test: &str) -> Self {
        Self::create(urls, Some(CLUSTER), test, "").await
    }

    /// A database with the `Replicated` engine, which replicates its own DDL.
    ///
    /// One database shard, with every node a replica of it.
    pub async fn replicated(urls: &[&str], test: &str) -> Self {
        let cluster = (urls.len() > 1).then_some(CLUSTER);
        Self::create(
            urls,
            cluster,
            test,
            "ENGINE = Replicated('/clickhouse/chx_test/{name}', '1', '{replica}')",
        )
        .await
    }

    async fn create(
        urls: &[&str],
        cluster: Option<&'static str>,
        test: &str,
        engine: &str,
    ) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let name = format!("chx_test_{test}_{nanos}");
        let servers: Vec<Client> = urls.iter().map(|url| client(url)).collect();

        let sql = format!(
            "CREATE DATABASE {name}{} {}",
            on_cluster(cluster),
            engine.replace("{name}", &name)
        );
        servers[0]
            .execute(&sql)
            .await
            .expect("create scratch database");

        let on = servers
            .iter()
            .map(|server| server.with_database(&name))
            .collect();
        Self {
            servers,
            cluster,
            name,
            on,
        }
    }

    /// The first node, for tests with only one.
    pub fn client(&self) -> &Client {
        &self.on[0]
    }

    pub async fn scalar(&self, node: usize, sql: &str) -> String {
        self.on[node].query(sql).await.unwrap().trim().to_string()
    }

    pub async fn history_engine(&self, node: usize) -> String {
        self.scalar(
            node,
            "SELECT engine FROM system.tables \
             WHERE database = currentDatabase() AND name = '_chx_migrations'",
        )
        .await
    }

    /// `SYNC` so a replicated table leaves Keeper before the test ends, rather
    /// than whenever the server gets round to it.
    pub async fn drop(self) {
        let sql = format!(
            "DROP DATABASE IF EXISTS {}{} SYNC",
            self.name,
            on_cluster(self.cluster)
        );
        self.servers[0]
            .execute(&sql)
            .await
            .expect("drop scratch database");
    }
}

fn on_cluster(cluster: Option<&str>) -> String {
    cluster
        .map(|c| format!(" ON CLUSTER {c}"))
        .unwrap_or_default()
}

pub fn write(dir: &Path, name: &str, body: &str) {
    std::fs::write(dir.join(name), body).unwrap();
}
