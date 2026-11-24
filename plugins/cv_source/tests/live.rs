//! The live view through the real binary and the real protocol, against a
//! small fake hub: a watched channel gets its data, an unwatched one nothing.

use flwrs_plugin::plugin::frame::{PROTOCOL_VERSION, read_frame, write_message};
use flwrs_plugin::schema::protocol::{
    Initialize, InitializeResponse, LiveFeed, LiveFeedControl, capability::Enum as Capability,
    msg_code::Enum as MsgCode,
};
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio::net::tcp::OwnedWriteHalf;
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

const WAIT: Duration = Duration::from_secs(20);
/// How long a channel nobody watches is listened to.
const QUIET: Duration = Duration::from_millis(1500);

struct Hub {
    writer: OwnedWriteHalf,
    feed: mpsc::UnboundedReceiver<LiveFeed>,
    _plugin: Child,
}

impl Hub {
    async fn watch(&mut self, channels: &[&str]) {
        let control = LiveFeedControl {
            channels: channels.iter().map(|c| c.to_string()).collect(),
        };
        write_message(&mut self.writer, &control).await.unwrap();
    }

    /// The next item of `channel` that carries data.
    async fn next_data(&mut self, channel: &str) -> LiveFeed {
        let deadline = Instant::now() + WAIT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let item = tokio::time::timeout(left, self.feed.recv())
                .await
                .unwrap_or_else(|_| panic!("no data on {channel}"))
                .expect("the plugin closed the connection");
            if item.channel == channel && !item.data.is_empty() {
                return item;
            }
        }
    }

    /// Items with data over `QUIET`.
    async fn data_within_quiet(&mut self) -> Vec<String> {
        let deadline = tokio::time::Instant::now() + QUIET;
        let mut seen = vec![];
        while let Ok(Some(item)) = tokio::time::timeout_at(deadline, self.feed.recv()).await {
            if !item.data.is_empty() {
                seen.push(item.channel);
            }
        }
        seen
    }
}

/// Starts the plugin on a looping picture through a fixed detector.
async fn start(dir: &Path, live: Option<serde_json::Value>) -> (Hub, Initialize) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let frames = dir.join("frames");
    std::fs::create_dir_all(&frames).unwrap();
    image::RgbImage::from_pixel(64, 32, image::Rgb([60, 90, 120]))
        .save(frames.join("a.png"))
        .unwrap();
    let graph = serde_json::json!({
        "nodes": {
            "camera": { "type": "source" },
            "det": {
                "type": "model", "model": "fixed_det.onnx",
                "pre": { "width": 64, "height": 32, "resize": "stretch", "color": "rgb", "dtype": "f32", "layout": "nchw" },
                "decode": { "type": "detections_e2e", "output": "rows" },
                "labels": ["person", "dog"]
            }
        },
        "links": [{ "from": "camera.frame", "to": "det.frame" }]
    });
    let input = serde_json::json!({ "type": "images", "path": frames, "fps": 30, "loop": true });
    std::fs::write(dir.join("graph.json"), graph.to_string()).unwrap();
    std::fs::write(dir.join("input.json"), input.to_string()).unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let mut command = Command::new(env!("CARGO_BIN_EXE_flwrs-cv-source"));
    command
        .args(["--id", "n", "--host", "127.0.0.1", "--port"])
        .arg(port.to_string())
        .arg("--graph")
        .arg(dir.join("graph.json"))
        .arg("--input")
        .arg(dir.join("input.json"))
        .arg("--file")
        .arg(root.join("tests/trellis/fixed_det.onnx"))
        .env("FLWRS_TOKEN", "token")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    if let Some(live) = live {
        std::fs::write(dir.join("live.json"), live.to_string()).unwrap();
        command.arg("--live").arg(dir.join("live.json"));
    }
    let plugin = command.spawn().unwrap();
    let (stream, _) = tokio::time::timeout(WAIT, listener.accept())
        .await
        .expect("the plugin didn't connect")
        .unwrap();
    let (mut reader, mut writer) = stream.into_split();
    let first = tokio::time::timeout(WAIT, read_frame(&mut reader))
        .await
        .expect("no Initialize")
        .unwrap()
        .unwrap();
    assert_eq!(first.code(), MsgCode::Initialize);
    let init: Initialize = first.decode().unwrap();
    let response = InitializeResponse {
        protocol_version: PROTOCOL_VERSION,
        capabilities: init
            .capabilities
            .iter()
            .copied()
            .filter(|c| *c == Capability::LiveFeed as i32)
            .collect(),
        heartbeat_interval_ms: 0,
        max_unacked: 0,
    };
    write_message(&mut writer, &response).await.unwrap();
    let (tx, feed) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok(Some(frame)) = read_frame(&mut reader).await {
            if frame.code() == MsgCode::LiveFeed
                && tx.send(frame.decode::<LiveFeed>().unwrap()).is_err()
            {
                return;
            }
        }
    });
    (
        Hub {
            writer,
            feed,
            _plugin: plugin,
        },
        init,
    )
}

#[tokio::test]
async fn a_watched_channel_gets_data_and_an_unwatched_one_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let live = serde_json::json!({ "stats": true, "preview": { "enabled": true, "fps": 10 } });
    let (mut hub, init) = start(dir.path(), Some(live)).await;
    assert!(init.capabilities.contains(&(Capability::LiveFeed as i32)));

    // Announced, and quiet while nobody watches.
    let mut announced = vec![];
    while announced.len() < 2 {
        let item = tokio::time::timeout(WAIT, hub.feed.recv())
            .await
            .expect("no announcement")
            .unwrap();
        assert!(item.data.is_empty(), "data before anyone watched");
        announced.push((item.channel, item.media_type));
    }
    announced.sort();
    assert_eq!(
        announced,
        [
            ("preview".to_string(), "image/jpeg".to_string()),
            ("stats".to_string(), "application/json".to_string())
        ]
    );
    assert_eq!(hub.data_within_quiet().await, Vec::<String>::new());

    // Only the preview.
    hub.watch(&["preview"]).await;
    let frame = hub.next_data("preview").await;
    assert_eq!(frame.media_type, "image/jpeg");
    assert!(frame.capture_time.is_some());
    let picture = image::load_from_memory_with_format(&frame.data, image::ImageFormat::Jpeg)
        .unwrap()
        .to_rgb8();
    assert_eq!(picture.dimensions(), (64, 32), "not enlarged");
    let seen = hub.data_within_quiet().await;
    assert!(seen.iter().all(|c| c == "preview"), "{seen:?}");

    // Only the stats.
    hub.watch(&["stats"]).await;
    let item = hub.next_data("stats").await;
    assert_eq!(item.media_type, "application/json");
    let stats: serde_json::Value = serde_json::from_slice(&item.data).unwrap();
    assert_eq!(stats["version"], 1);
    assert!(stats["frames"]["processed_fps"].as_f64().unwrap() > 0.0);
    assert!(stats["nodes"]["det"]["runs"].as_u64().unwrap() > 0);
    assert!(stats["nodes"]["det"]["latency_ms"]["p95"].is_number());

    // Nobody again: whatever was on its way, then silence.
    hub.watch(&[]).await;
    let _ = hub.data_within_quiet().await;
    assert_eq!(hub.data_within_quiet().await, Vec::<String>::new());
}

/// Without the setting the capability isn't even offered.
#[tokio::test]
async fn without_the_setting_no_live_feed_is_offered() {
    let dir = tempfile::tempdir().unwrap();
    let (mut hub, init) = start(dir.path(), None).await;
    assert!(!init.capabilities.contains(&(Capability::LiveFeed as i32)));
    assert_eq!(hub.data_within_quiet().await, Vec::<String>::new());
}
