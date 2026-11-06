//! SQLite (`.db`), one database per partition, with a small documented
//! schema (README.md, *Telemetry*) that Grafana (the SQLite data source),
//! Datasette, DuckDB's `sqlite` scanner or any SQL client can read:
//!
//! - `series`: one row per metric and label set (`metric`, `type` =
//!   `counter` / `gauge`, `unit`, `description`, `scene_id`, `node_id`,
//!   `plugin_id`, `plugin_version`; `''` where a label doesn't apply);
//! - `samples`: `time` (Unix seconds), `series_id`, `value`;
//! - `metrics`: a view joining the two;
//! - `meta`: `schema_version` (1), `partition_start` / `partition_end`
//!   (RFC 3339), `writer`.
//!
//! WAL journal, `synchronous = NORMAL`; one transaction per flush. Not
//! compressed when the partition closes.

use super::Format;
use crate::modules::telemetry::samples::{Kind, Samples};
use async_trait::async_trait;
use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions, SqliteSynchronous,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

pub(crate) const SCHEMA_VERSION: u32 = 1;

/// The schema, as documented in README.md.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS series (
    id             INTEGER PRIMARY KEY,
    metric         TEXT NOT NULL,
    type           TEXT NOT NULL CHECK (type IN ('counter', 'gauge')),
    unit           TEXT NOT NULL,
    description    TEXT NOT NULL,
    scene_id       TEXT NOT NULL DEFAULT '',
    node_id        TEXT NOT NULL DEFAULT '',
    plugin_id      TEXT NOT NULL DEFAULT '',
    plugin_version TEXT NOT NULL DEFAULT '',
    UNIQUE (metric, scene_id, node_id, plugin_id, plugin_version)
);
CREATE TABLE IF NOT EXISTS samples (
    time      INTEGER NOT NULL,
    series_id INTEGER NOT NULL REFERENCES series (id),
    value     REAL    NOT NULL,
    PRIMARY KEY (series_id, time)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS samples__time_idx ON samples (time);
CREATE VIEW IF NOT EXISTS metrics AS
    SELECT s.time, r.metric, r.type, r.unit, r.scene_id, r.node_id,
           r.plugin_id, r.plugin_version, s.value
    FROM samples s JOIN series r ON r.id = s.series_id;
";

/// Labels as the `series` columns.
const LABELS: [(&str, &str); 4] = [
    ("scene.id", "scene_id"),
    ("node.id", "node_id"),
    ("plugin.id", "plugin_id"),
    ("plugin.version", "plugin_version"),
];

type SeriesKey = (String, [String; 4]);

pub(super) struct Sqlite {
    partition: Duration,
    open: Option<(PathBuf, SqlitePool)>,
    series: HashMap<SeriesKey, i64>,
}

impl Sqlite {
    pub(super) fn new(partition: Duration) -> Self {
        Self {
            partition,
            open: None,
            series: HashMap::new(),
        }
    }

    async fn open(&mut self, path: &Path) -> Result<SqlitePool, sqlx::Error> {
        if let Some((open, pool)) = &self.open
            && open == path
        {
            return Ok(pool.clone());
        }
        self.release().await;
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(path)
                    .create_if_missing(true)
                    .journal_mode(SqliteJournalMode::Wal)
                    .synchronous(SqliteSynchronous::Normal),
            )
            .await?;
        sqlx::raw_sql(SCHEMA).execute(&pool).await?;
        let start = partition_of(path);
        let rfc3339 = |secs: u64| {
            chrono::DateTime::from_timestamp(secs as i64, 0)
                .map_or_else(String::new, |t| t.to_rfc3339())
        };
        for (key, value) in [
            ("schema_version", SCHEMA_VERSION.to_string()),
            ("partition_start", rfc3339(start)),
            ("partition_end", rfc3339(start + self.partition.as_secs())),
            ("writer", format!("flwrs {}", env!("CARGO_PKG_VERSION"))),
        ] {
            sqlx::query("INSERT OR IGNORE INTO meta (key, value) VALUES (?, ?)")
                .bind(key)
                .bind(value)
                .execute(&pool)
                .await?;
        }
        // An earlier run may have written to this partition already.
        let rows: Vec<(i64, String, String, String, String, String)> = sqlx::query_as(
            "SELECT id, metric, scene_id, node_id, plugin_id, plugin_version FROM series",
        )
        .fetch_all(&pool)
        .await?;
        self.series = rows
            .into_iter()
            .map(|(id, metric, a, b, c, d)| ((metric, [a, b, c, d]), id))
            .collect();
        self.open = Some((path.to_path_buf(), pool.clone()));
        Ok(pool)
    }

    async fn write(&mut self, path: &Path, batch: &[Samples]) -> Result<(), sqlx::Error> {
        let pool = self.open(path).await?;
        let mut conn = pool.acquire().await?;
        let mut tx = sqlx::Connection::begin_with(&mut *conn, "BEGIN IMMEDIATE").await?;
        for samples in batch {
            let time = samples
                .time
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs() as i64);
            for sample in &samples.samples {
                let labels = LABELS.map(|(name, _)| {
                    sample
                        .labels
                        .iter()
                        .find(|(k, _)| *k == name)
                        .map_or_else(String::new, |(_, v)| v.clone())
                });
                let key = (sample.metric.name.to_string(), labels);
                let id = match self.series.get(&key) {
                    Some(id) => *id,
                    None => {
                        let [a, b, c, d] = &key.1;
                        let (id,): (i64,) = sqlx::query_as(
                            "INSERT INTO series
                                 (metric, type, unit, description, scene_id, node_id, plugin_id, plugin_version)
                             VALUES (?, ?, ?, ?, ?, ?, ?, ?)
                             ON CONFLICT (metric, scene_id, node_id, plugin_id, plugin_version)
                             DO UPDATE SET unit = excluded.unit
                             RETURNING id",
                        )
                        .bind(sample.metric.name)
                        .bind(match sample.metric.kind {
                            Kind::Counter => "counter",
                            Kind::Gauge => "gauge",
                        })
                        .bind(sample.metric.unit)
                        .bind(sample.metric.description)
                        .bind(a)
                        .bind(b)
                        .bind(c)
                        .bind(d)
                        .fetch_one(&mut *tx)
                        .await?;
                        self.series.insert(key, id);
                        id
                    }
                };
                sqlx::query(
                    "INSERT OR REPLACE INTO samples (time, series_id, value) VALUES (?, ?, ?)",
                )
                .bind(time)
                .bind(id)
                .bind(sample.value)
                .execute(&mut *tx)
                .await?;
            }
        }
        tx.commit().await
    }
}

/// The partition start in a file name (`flwrs-metrics.<stamp>.db`).
fn partition_of(path: &Path) -> u64 {
    path.file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.split('.').nth(1))
        .and_then(super::parse_stamp)
        .unwrap_or(0)
}

#[async_trait]
impl Format for Sqlite {
    async fn append(&mut self, path: &Path, batch: &[Samples]) -> Result<(), String> {
        self.write(path, batch)
            .await
            .map_err(|e| format!("couldn't write {}: {e}", path.display()))
    }

    /// Closing the pool checkpoints the WAL into the database file.
    async fn release(&mut self) {
        if let Some((_, pool)) = self.open.take() {
            pool.close().await;
        }
        self.series.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::super::Writer;
    use super::super::tests::{names_in, samples, sink};
    use super::*;

    #[tokio::test]
    async fn partitions_are_queryable_databases() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = Writer::new(sink(dir.path(), "sqlite", "")).unwrap();
        w.push(samples(3_600, 1.0));
        w.push(samples(3_610, 2.0));
        w.flush().await.unwrap();
        w.finish().await.unwrap();
        // A restart within the same hour appends to the same file.
        let mut w = Writer::new(sink(dir.path(), "sqlite", "")).unwrap();
        w.push(samples(3_620, 3.0));
        w.push(samples(7_200, 4.0));
        w.finish().await.unwrap();
        assert_eq!(
            names_in(dir.path()),
            [
                "flwrs-metrics.19700101T010000Z.db",
                "flwrs-metrics.19700101T020000Z.db",
            ],
            "no WAL files left after a clean stop"
        );

        let pool = SqlitePool::connect(&format!(
            "sqlite://{}",
            dir.path()
                .join("flwrs-metrics.19700101T010000Z.db")
                .display()
        ))
        .await
        .unwrap();
        let rows: Vec<(i64, String, f64)> = sqlx::query_as(
            "SELECT time, type, value FROM metrics
             WHERE metric = 'flwrs.node.events.emitted' AND node_id = 'n1' ORDER BY time",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            rows,
            [
                (3_600, "counter".to_string(), 1.0),
                (3_610, "counter".to_string(), 2.0),
                (3_620, "counter".to_string(), 3.0),
            ]
        );
        let (series,): (i64,) = sqlx::query_as("SELECT count(*) FROM series")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(series, 5, "the series were reused, not duplicated");
        let (hub_scene,): (String,) = sqlx::query_as(
            "SELECT scene_id FROM series WHERE metric = 'flwrs.hub.queued_memory.usage'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(hub_scene, "");
        let meta: Vec<(String, String)> =
            sqlx::query_as("SELECT key, value FROM meta ORDER BY key")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert!(meta.contains(&("partition_start".into(), "1970-01-01T01:00:00+00:00".into())));
        assert!(meta.contains(&("partition_end".into(), "1970-01-01T02:00:00+00:00".into())));
        assert!(meta.contains(&("schema_version".into(), "1".into())));
    }
}
