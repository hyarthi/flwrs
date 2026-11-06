//! The local file sink (`[[telemetry.file]]`): the director's stats written
//! to time-partitioned files in a folder, in one of four formats that
//! common tools read (see the format modules).
//!
//! **Partitions.** Each file covers one `partition` (e.g. a day), aligned to
//! the Unix epoch in UTC, and is named
//! `flwrs-metrics.<UTC start, YYYYMMDDTHHMMSSZ>.<extension>`. While its
//! partition is current, a file is *open* and appended to (at each flush);
//! when the partition is over, it's *closed*: text formats are gzipped
//! (`compress`), OpenMetrics is assembled into a valid exposition. A
//! partition left open by an earlier run is closed at the next start, or
//! appended to if it's still current.
//!
//! **Retention.** Whenever a partition opens, the oldest ones are deleted
//! beyond `max_files` (default 30), while the entry's files take more than
//! `max_bytes` (default 1 GiB), or older than `max_age` (optional). 0 turns
//! a limit off. The current partition is never deleted.
//!
//! **Writes.** Samples are collected every `interval` and kept in memory;
//! they're written every `flush_interval`, when a partition ends and at
//! shutdown. A crash loses at most one flush interval.

mod influx;
mod openmetrics;
#[cfg(feature = "otel")]
mod otlp_json;
mod sqlite;

use super::config::{FileFormat, FileSink};
use super::samples::{self, Samples};
use crate::modules::director::DirectorHandle;
use crate::modules::rate_limit::RateLimit;
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio_util::sync::CancellationToken;

/// Every file name starts with this.
pub(crate) const PREFIX: &str = "flwrs-metrics";

/// How a format writes an open partition.
#[async_trait]
trait Format: Send {
    /// Appends samples (in time order, all in one partition) to the open
    /// partition at `path`, creating it if needed.
    async fn append(&mut self, path: &Path, batch: &[Samples]) -> Result<(), String>;

    /// Lets go of the open partition (before it's closed, and at shutdown).
    async fn release(&mut self) {}
}

/// File names of a format: `stem` is unique per format, so formats can share
/// a folder.
#[derive(Debug, Clone, Copy)]
struct Names {
    format: FileFormat,
    compress: bool,
}

impl Names {
    fn stem(self) -> &'static str {
        match self.format {
            FileFormat::OtlpJson => "jsonl",
            FileFormat::Sqlite => "db",
            FileFormat::Influx => "lp",
            FileFormat::Openmetrics => "om",
        }
    }

    /// Extension of an open partition (OpenMetrics: a folder of parts).
    fn open_ext(self) -> String {
        match self.format {
            FileFormat::Openmetrics => "om.d".into(),
            _ => self.stem().into(),
        }
    }

    /// Extension of a closed partition.
    fn closed_ext(self) -> String {
        match self.format {
            FileFormat::Sqlite => "db".into(),
            _ if self.compress => format!("{}.gz", self.stem()),
            _ => self.stem().into(),
        }
    }

    fn path(self, dir: &Path, start: u64, ext: &str) -> PathBuf {
        dir.join(format!("{PREFIX}.{}.{ext}", stamp(start)))
    }
}

/// The start of the partition `time` falls in (seconds since the epoch).
fn partition_start(time: SystemTime, partition: Duration) -> u64 {
    let secs = time.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let len = partition.as_secs().max(1);
    secs / len * len
}

const STAMP: &str = "%Y%m%dT%H%M%SZ";

/// `20261106T000000Z`: sortable, and valid in file names everywhere.
fn stamp(start: u64) -> String {
    i64::try_from(start)
        .ok()
        .and_then(|s| chrono::DateTime::from_timestamp(s, 0))
        .map_or_else(|| "0".into(), |t| t.format(STAMP).to_string())
}

fn parse_stamp(text: &str) -> Option<u64> {
    let time = chrono::NaiveDateTime::parse_from_str(text, STAMP).ok()?;
    u64::try_from(time.and_utc().timestamp()).ok()
}

/// One file or folder of a partition.
#[derive(Debug)]
struct Entry {
    path: PathBuf,
    /// The name after `<PREFIX>.<stamp>.`, e.g. `jsonl.gz`, `db-wal`.
    suffix: String,
    bytes: u64,
}

/// The format's partitions in `dir`, by start.
fn scan(dir: &Path, names: Names) -> io::Result<BTreeMap<u64, Vec<Entry>>> {
    let stem = names.stem();
    let mut partitions: BTreeMap<u64, Vec<Entry>> = BTreeMap::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(rest) = name.strip_prefix(PREFIX).and_then(|r| r.strip_prefix('.')) else {
            continue;
        };
        let Some((stamp, suffix)) = rest.split_once('.') else {
            continue;
        };
        let ours = suffix
            .strip_prefix(stem)
            .is_some_and(|r| r.is_empty() || r.starts_with(['.', '-']));
        let Some(start) = parse_stamp(stamp).filter(|_| ours) else {
            continue;
        };
        let path = entry.path();
        let bytes = size_of(&path);
        partitions.entry(start).or_default().push(Entry {
            path,
            suffix: suffix.to_string(),
            bytes,
        });
    }
    Ok(partitions)
}

fn size_of(path: &Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if meta.is_dir() {
        std::fs::read_dir(path).map_or(0, |entries| {
            entries.flatten().map(|e| size_of(&e.path())).sum()
        })
    } else {
        meta.len()
    }
}

fn remove(path: &Path) -> io::Result<()> {
    if path.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
}

/// Closes every partition still open except `current`, then applies
/// retention (keeping `current`). Blocking: runs on the blocking pool.
/// Returns what it did, for the log.
fn sweep(sink: &FileSink, names: Names, current: u64, now: u64) -> io::Result<Vec<String>> {
    let mut done = vec![];
    let open_ext = names.open_ext();
    let closed_ext = names.closed_ext();
    for (start, entries) in scan(&sink.dir, names)? {
        if start == current {
            continue;
        }
        for entry in &entries {
            if entry.suffix.ends_with(".tmp") {
                // An interrupted close; the open file is still there.
                remove(&entry.path)?;
            }
        }
        let open = names.path(&sink.dir, start, &open_ext);
        if open_ext == closed_ext || !open.exists() {
            continue;
        }
        let closed = names.path(&sink.dir, start, &closed_ext);
        if !closed.exists() {
            close(names, &open, &closed)?;
            done.push(format!("closed {}", file_name(&closed)));
        }
        // (If both exist, an earlier close was interrupted after the rename.)
        remove(&open)?;
    }

    // Retention, oldest first; the current partition always stays.
    let partitions = scan(&sink.dir, names)?;
    let mut total: u64 = partitions.values().flatten().map(|e| e.bytes).sum();
    let mut starts: std::collections::BTreeSet<u64> = partitions.keys().copied().collect();
    starts.insert(current); // counts even before its file exists
    let len = sink.partition.as_secs();
    for (start, entries) in &partitions {
        if *start == current {
            continue;
        }
        let newer = starts.range(start + 1..).count();
        let too_many = sink.max_files.is_some_and(|max| newer >= max as usize);
        let too_old = sink
            .max_age
            .is_some_and(|age| start + len + age.as_secs() < now);
        let too_big = sink.max_bytes.is_some_and(|max| total > max);
        if too_many || too_old || too_big {
            for entry in entries {
                remove(&entry.path)?;
                total = total.saturating_sub(entry.bytes);
            }
            done.push(format!("deleted the partition of {}", stamp(*start)));
        }
    }
    Ok(done)
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
}

/// Turns an open partition into its closed file (via `<closed>.tmp` and a
/// rename, so a closed file is always complete).
fn close(names: Names, open: &Path, closed: &Path) -> io::Result<()> {
    let tmp = closed.with_file_name(format!("{}.tmp", file_name(closed)));
    let file = std::fs::File::create(&tmp)?;
    let mut out: Box<dyn Write> = if names.compress {
        Box::new(flate2::write::GzEncoder::new(
            io::BufWriter::new(file),
            flate2::Compression::default(),
        ))
    } else {
        Box::new(io::BufWriter::new(file))
    };
    if names.format == FileFormat::Openmetrics {
        openmetrics::assemble(open, &mut out)?;
    } else {
        io::copy(&mut std::fs::File::open(open)?, &mut out)?;
    }
    out.flush()?;
    drop(out); // finishes the gzip stream
    std::fs::File::open(&tmp)?.sync_all()?;
    std::fs::rename(&tmp, closed)
}

/// Appends to text files. The first write to a file after opening it makes
/// sure it ends with a newline, so a line torn by a crash doesn't swallow
/// the next one.
#[derive(Default)]
struct Appender {
    checked: Option<PathBuf>,
}

impl Appender {
    async fn append(&mut self, path: &Path, bytes: Vec<u8>) -> Result<(), String> {
        let check = self.checked.as_deref() != Some(path);
        let owned = path.to_path_buf();
        tokio::task::spawn_blocking(move || append_file(&owned, &bytes, check))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| format!("couldn't write {}: {e}", path.display()))?;
        self.checked = Some(path.to_path_buf());
        Ok(())
    }
}

fn append_file(path: &Path, bytes: &[u8], check_tail: bool) -> io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(path)?;
    if check_tail && file.metadata()?.len() > 0 {
        let mut last = [0u8];
        file.seek(SeekFrom::End(-1))?;
        file.read_exact(&mut last)?;
        if last[0] != b'\n' {
            file.write_all(b"\n")?;
        }
    }
    file.write_all(bytes)?;
    file.sync_data()
}

/// Buffers samples and writes them into the right partitions.
pub(crate) struct Writer {
    sink: FileSink,
    names: Names,
    format: Box<dyn Format>,
    /// The partition the format has open.
    current: Option<u64>,
    buffer: Vec<Samples>,
}

impl Writer {
    pub(crate) fn new(sink: FileSink) -> Result<Self, String> {
        let format: Box<dyn Format> = match sink.format {
            #[cfg(feature = "otel")]
            FileFormat::OtlpJson => Box::new(otlp_json::OtlpJson::new()),
            #[cfg(not(feature = "otel"))]
            FileFormat::OtlpJson => {
                return Err(
                    "the otlp-json format needs a server built with the otel feature".into(),
                );
            }
            FileFormat::Sqlite => Box::new(sqlite::Sqlite::new(sink.partition)),
            FileFormat::Influx => Box::new(influx::Influx::default()),
            FileFormat::Openmetrics => Box::new(openmetrics::OpenMetrics::default()),
        };
        std::fs::create_dir_all(&sink.dir)
            .map_err(|e| format!("couldn't create {}: {e}", sink.dir.display()))?;
        let names = Names {
            format: sink.format,
            compress: sink.compress,
        };
        Ok(Self {
            sink,
            names,
            format,
            current: None,
            buffer: vec![],
        })
    }

    /// Keeps a collection for the next flush. True if it starts a new
    /// partition (worth flushing now, so the old one closes promptly).
    pub(crate) fn push(&mut self, samples: Samples) -> bool {
        let start = partition_start(samples.time, self.sink.partition);
        let rolls = self.current.is_some_and(|current| start > current);
        self.buffer.push(samples);
        rolls
    }

    /// Writes what's buffered. On an error the batch is dropped (the error
    /// is returned for the log); the next flush starts over.
    pub(crate) async fn flush(&mut self) -> Result<(), String> {
        let buffer = std::mem::take(&mut self.buffer);
        let mut batch: Vec<Samples> = vec![];
        let mut batch_start = None;
        for samples in buffer {
            // A clock that went back writes into the current partition.
            let start =
                partition_start(samples.time, self.sink.partition).max(self.current.unwrap_or(0));
            if batch_start.is_some_and(|s| s != start) {
                self.write(batch_start.unwrap_or(start), std::mem::take(&mut batch))
                    .await?;
            }
            batch_start = Some(start);
            batch.push(samples);
        }
        if let Some(start) = batch_start {
            self.write(start, batch).await?;
        }
        Ok(())
    }

    async fn write(&mut self, start: u64, batch: Vec<Samples>) -> Result<(), String> {
        if self.current != Some(start) {
            self.format.release().await;
            self.current = Some(start);
            let now = batch.last().map_or(start, |s| {
                s.time.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
            });
            let (sink, names) = (self.sink.clone(), self.names);
            let done = tokio::task::spawn_blocking(move || sweep(&sink, names, start, now))
                .await
                .map_err(|e| e.to_string())?
                .map_err(|e| {
                    format!("couldn't rotate files in {}: {e}", self.sink.dir.display())
                })?;
            for line in done {
                log::debug!("Telemetry ({}): {line}", self.sink.format.name());
            }
        }
        let path = self
            .names
            .path(&self.sink.dir, start, &self.names.open_ext());
        self.format.append(&path, &batch).await
    }

    /// Writes what's left and lets go of the files. The current partition
    /// stays open on disk (a restart within it appends to it).
    pub(crate) async fn finish(&mut self) -> Result<(), String> {
        let result = self.flush().await;
        self.format.release().await;
        result
    }
}

/// Collects the director's stats every `interval` and writes them, until
/// `shutdown` (then a last collection and flush). Failures are logged
/// (rate-limited), never fatal.
pub(crate) async fn run(
    director: &'static DirectorHandle,
    sink: FileSink,
    shutdown: CancellationToken,
) {
    let tag = format!("Telemetry ({})", sink.format.name());
    log::info!(
        "{tag}: writing to {} every {} s (partitions of {} s; at most {} files, {} bytes)",
        sink.dir.display(),
        sink.flush_interval.as_secs(),
        sink.partition.as_secs(),
        sink.max_files.map_or("unlimited".into(), |n| n.to_string()),
        sink.max_bytes.map_or("unlimited".into(), |n| n.to_string()),
    );
    let (interval, flush_interval) = (sink.interval, sink.flush_interval);
    let mut writer = match Writer::new(sink) {
        Ok(writer) => writer,
        Err(err) => return log::error!("{tag}: {err}"),
    };
    let failures = RateLimit::new(Duration::from_secs(300));
    let mut last_flush = Instant::now();
    loop {
        let stopping = tokio::select! {
            _ = shutdown.cancelled() => true,
            _ = tokio::time::sleep(interval) => false,
        };
        let mut rolls = false;
        match director.snapshot().await {
            Ok(snapshot) => {
                rolls = writer.push(samples::from_snapshot(&snapshot, SystemTime::now()));
            }
            Err(err) => log::debug!("{tag}: no snapshot: {err}"),
        }
        let result = if stopping {
            writer.finish().await
        } else if rolls || last_flush.elapsed() >= flush_interval {
            last_flush = Instant::now();
            writer.flush().await
        } else {
            Ok(())
        };
        if let Err(err) = result
            && failures.allow()
        {
            log::warn!("{tag}: {err}");
        }
        if stopping {
            return;
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::super::config::FileConfig;
    use super::super::samples::{
        HUB_QUEUED_MEMORY, NODE_EMITTED, NODE_UP, QUEUE_BLOCKED, QUEUE_BYTES, Sample,
    };
    use super::*;

    pub(crate) fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    /// One collection: a hub gauge, and two counters and two gauges of node n1.
    pub(crate) fn samples(secs: u64, emitted: f64) -> Samples {
        let node = vec![
            ("scene.id", "s1".to_string()),
            ("node.id", "n1".to_string()),
            ("plugin.id", "letters".to_string()),
            ("plugin.version", "1.0.0".to_string()),
        ];
        let sample = |metric, labels: &Vec<_>, value| Sample {
            metric,
            labels: labels.clone(),
            value,
        };
        Samples {
            time: at(secs),
            samples: vec![
                sample(HUB_QUEUED_MEMORY, &vec![], 2048.0),
                sample(NODE_EMITTED, &node, emitted),
                sample(NODE_UP, &node, 1.0),
                sample(QUEUE_BYTES, &node, 512.0),
                sample(QUEUE_BLOCKED, &node, 3.0),
            ],
        }
    }

    pub(crate) fn sink(dir: &Path, format: &str, extra: &str) -> FileSink {
        let config: FileConfig = toml::from_str(&format!(
            "format = \"{format}\"\ndir = {:?}\npartition = \"1h\"\n{extra}",
            dir.display().to_string()
        ))
        .unwrap();
        config.resolve().unwrap()
    }

    pub(crate) fn names_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    pub(crate) fn gunzip(path: &Path) -> String {
        let mut text = String::new();
        flate2::read::GzDecoder::new(std::fs::File::open(path).unwrap())
            .read_to_string(&mut text)
            .unwrap();
        text
    }

    #[test]
    fn partitions_and_stamps() {
        let hour = Duration::from_secs(3_600);
        assert_eq!(partition_start(at(7_199), hour), 3_600);
        assert_eq!(partition_start(at(7_200), hour), 7_200);
        assert_eq!(stamp(1_793_923_200), "20261106T000000Z");
        assert_eq!(parse_stamp("20261106T000000Z"), Some(1_793_923_200));
        assert_eq!(parse_stamp("2026-11-06"), None);
    }

    #[test]
    fn scan_only_sees_its_own_format() {
        let dir = tempfile::tempdir().unwrap();
        for name in [
            "flwrs-metrics.20261106T000000Z.lp",
            "flwrs-metrics.20261106T010000Z.lp.gz",
            "flwrs-metrics.20261106T010000Z.lpx",
            "flwrs-metrics.20261106T000000Z.jsonl",
            "flwrs-metrics.garbage.lp",
            "other.20261106T000000Z.lp",
        ] {
            std::fs::write(dir.path().join(name), "x").unwrap();
        }
        let names = Names {
            format: FileFormat::Influx,
            compress: true,
        };
        let found = scan(dir.path(), names).unwrap();
        let found: Vec<(String, Vec<String>)> = found
            .into_iter()
            .map(|(s, e)| (stamp(s), e.into_iter().map(|e| e.suffix).collect()))
            .collect();
        assert_eq!(
            found,
            [
                ("20261106T000000Z".to_string(), vec!["lp".to_string()]),
                ("20261106T010000Z".to_string(), vec!["lp.gz".to_string()]),
            ]
        );
    }

    #[tokio::test]
    async fn text_partitions_roll_over_and_are_gzipped() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = Writer::new(sink(dir.path(), "influx", "")).unwrap();
        assert!(!w.push(samples(3_600, 1.0)));
        assert!(!w.push(samples(3_610, 2.0)));
        w.flush().await.unwrap();
        assert_eq!(names_in(dir.path()), ["flwrs-metrics.19700101T010000Z.lp"]);
        // The next hour: the first partition is closed (gzipped).
        assert!(w.push(samples(7_200, 3.0)));
        w.finish().await.unwrap();
        assert_eq!(
            names_in(dir.path()),
            [
                "flwrs-metrics.19700101T010000Z.lp.gz",
                "flwrs-metrics.19700101T020000Z.lp",
            ]
        );
        let first = gunzip(&dir.path().join("flwrs-metrics.19700101T010000Z.lp.gz"));
        assert_eq!(
            first
                .lines()
                .filter(|l| l.starts_with("flwrs_node"))
                .count(),
            2
        );

        // A new run in a later hour closes the partition the last run left open.
        let mut w = Writer::new(sink(dir.path(), "influx", "")).unwrap();
        w.push(samples(10_800, 4.0));
        w.finish().await.unwrap();
        assert_eq!(
            names_in(dir.path()),
            [
                "flwrs-metrics.19700101T010000Z.lp.gz",
                "flwrs-metrics.19700101T020000Z.lp.gz",
                "flwrs-metrics.19700101T030000Z.lp",
            ]
        );
    }

    #[tokio::test]
    async fn a_torn_line_is_ended_before_appending() {
        let dir = tempfile::tempdir().unwrap();
        let open = dir.path().join("flwrs-metrics.19700101T010000Z.lp");
        std::fs::write(&open, "flwrs_hub queued_memory_us").unwrap();
        let mut w = Writer::new(sink(dir.path(), "influx", "")).unwrap();
        w.push(samples(3_600, 1.0));
        w.finish().await.unwrap();
        let text = std::fs::read_to_string(&open).unwrap();
        assert!(
            text.starts_with("flwrs_hub queued_memory_us\nflwrs_hub "),
            "{text}"
        );
    }

    #[tokio::test]
    async fn retention_by_count_age_and_bytes() {
        let dir = tempfile::tempdir().unwrap();
        // Ten closed hourly partitions of 100 bytes, an interrupted close.
        for h in 0..10u64 {
            let name = format!("flwrs-metrics.{}.lp.gz", stamp(h * 3_600));
            std::fs::write(dir.path().join(name), [b'x'; 100]).unwrap();
        }
        std::fs::write(
            dir.path()
                .join(format!("flwrs-metrics.{}.lp.gz.tmp", stamp(0))),
            "half",
        )
        .unwrap();
        let hours = |dir: &Path| {
            names_in(dir)
                .iter()
                .map(|n| parse_stamp(&n[14..30]).unwrap() / 3_600)
                .collect::<Vec<_>>()
        };

        // Count: the current partition (hour 10) plus 7 = 8 kept.
        let s = sink(dir.path(), "influx", "max_files = 8");
        sweep(&s, names(&s), 36_000, 36_000).unwrap();
        assert_eq!(hours(dir.path()), [3, 4, 5, 6, 7, 8, 9]);

        // Age: partitions that ended more than 3 h before now (hour 10).
        let s = sink(dir.path(), "influx", "max_age = \"3h\"");
        sweep(&s, names(&s), 36_000, 36_000).unwrap();
        assert_eq!(hours(dir.path()), [6, 7, 8, 9]);

        // Bytes: at most 250 of them, the current partition never deleted.
        std::fs::write(
            dir.path()
                .join(format!("flwrs-metrics.{}.lp", stamp(36_000))),
            [b'x'; 100],
        )
        .unwrap();
        let s = sink(dir.path(), "influx", "max_bytes = 250");
        sweep(&s, names(&s), 36_000, 36_000).unwrap();
        assert_eq!(hours(dir.path()), [9, 10]);
    }

    #[test]
    fn zero_limits_keep_everything() {
        let dir = tempfile::tempdir().unwrap();
        for h in 0..40u64 {
            let name = format!("flwrs-metrics.{}.lp.gz", stamp(h * 3_600));
            std::fs::write(dir.path().join(name), [b'x'; 100]).unwrap();
        }
        let s = sink(dir.path(), "influx", "max_files = 0\nmax_bytes = 0");
        sweep(&s, names(&s), 40 * 3_600, 40 * 3_600).unwrap();
        assert_eq!(names_in(dir.path()).len(), 40);
        // The defaults: 30 files, the current one included.
        let s = sink(dir.path(), "influx", "");
        sweep(&s, names(&s), 40 * 3_600, 40 * 3_600).unwrap();
        assert_eq!(names_in(dir.path()).len(), 29);
    }

    fn names(sink: &FileSink) -> Names {
        Names {
            format: sink.format,
            compress: sink.compress,
        }
    }

    #[tokio::test]
    async fn uncompressed_files_keep_their_names() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = Writer::new(sink(dir.path(), "influx", "compress = false")).unwrap();
        w.push(samples(3_600, 1.0));
        w.push(samples(7_200, 2.0));
        w.finish().await.unwrap();
        assert_eq!(
            names_in(dir.path()),
            [
                "flwrs-metrics.19700101T010000Z.lp",
                "flwrs-metrics.19700101T020000Z.lp",
            ]
        );
    }
}
