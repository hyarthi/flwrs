//! The live feed against a fake hub. The plugin logger is process-wide, so
//! this file runs one plugin, in one test.

mod common;

use common::{FakeHub, connection, listen};
use flwrs_plugin::plugin::core::InitializeRequest;
use flwrs_plugin::plugin::error::{InitializeError, ShutdownError, SourceError};
use flwrs_plugin::plugin::live::LiveFeedPublisher;
use flwrs_plugin::schema::protocol::{
    LiveFeed, LiveFeedControl, capability::Enum as Capability, log_level::Enum as LogLevel,
    msg_code::Enum as MsgCode,
};
use flwrs_plugin::source::local_sink::LocalSink;
use flwrs_plugin::source::plugin::Source;
use flwrs_plugin::source::runner::{SourceRunner, SourceRunnerConfig};
use std::time::Duration;

struct LiveSource {
    live: LiveFeedPublisher,
}

impl Source for LiveSource {
    fn initialize(
        &mut self,
        id: String,
        _: LogLevel,
        _: LocalSink,
    ) -> Result<InitializeRequest, InitializeError> {
        Ok(InitializeRequest::new()
            .with_id(id)
            .with_version(self.version())
            .with_live_feed(self.live.clone()))
    }
    fn shutdown(&mut self) -> Result<(), ShutdownError> {
        Ok(())
    }
    fn version(&self) -> String {
        "1.0.0".into()
    }
    async fn run(&self) -> Result<(), SourceError> {
        std::future::pending().await
    }
}

async fn next_live(hub: &mut FakeHub) -> LiveFeed {
    loop {
        let frame = hub.frame().await;
        match frame.code() {
            MsgCode::LiveFeed => return frame.decode().unwrap(),
            MsgCode::Log | MsgCode::Error => {}
            other => panic!("unexpected {other:?}"),
        }
    }
}

async fn wait_watched(live: &LiveFeedPublisher, channel: &str, watched: bool) {
    let mut rx = live.watched();
    tokio::time::timeout(
        Duration::from_secs(5),
        rx.wait_for(|set| set.contains(channel) == watched),
    )
    .await
    .expect("timed out waiting for the watched set")
    .unwrap();
}

#[tokio::test]
async fn the_feed_follows_the_hub_across_a_reconnect() {
    let (listener, port) = listen().await;
    let live = LiveFeedPublisher::new();
    live.announce("preview", "image/jpeg").unwrap();
    let plugin = LiveSource { live: live.clone() };
    let runner = tokio::spawn(async move {
        let mut runner = SourceRunner::initialize(
            plugin,
            SourceRunnerConfig {
                plugin_id: "src-1".into(),
                log_level: LogLevel::Info,
                hub_connection: connection(port),
            },
        )
        .await
        .unwrap();
        let _ = runner.run().await;
    });

    let (mut hub, init) = FakeHub::handshake(&listener).await;
    assert!(init.capabilities.contains(&(Capability::LiveFeed as i32)));
    hub.accept_plugin(&[Capability::LiveFeed], 0).await;

    // Announced at once; no data until someone watches.
    let announcement = next_live(&mut hub).await;
    assert_eq!(
        (
            announcement.channel.as_str(),
            announcement.media_type.as_str()
        ),
        ("preview", "image/jpeg")
    );
    assert!(announcement.data.is_empty());
    assert!(!live.publish("preview", vec![1], None));

    hub.send(&LiveFeedControl {
        channels: vec!["preview".into()],
    })
    .await;
    wait_watched(&live, "preview", true).await;
    assert!(live.publish("preview", vec![7, 7], None));
    let frame = next_live(&mut hub).await;
    assert_eq!((frame.data, frame.seq), (vec![7, 7], 1));

    hub.send(&LiveFeedControl { channels: vec![] }).await;
    wait_watched(&live, "preview", false).await;

    // A new connection: nothing is watched until the hub says so, and the
    // channel is announced again.
    hub.send(&LiveFeedControl {
        channels: vec!["preview".into()],
    })
    .await;
    wait_watched(&live, "preview", true).await;
    drop(hub);
    wait_watched(&live, "preview", false).await;
    let (mut hub, _) = FakeHub::handshake(&listener).await;
    hub.accept_plugin(&[Capability::LiveFeed], 0).await;
    assert!(next_live(&mut hub).await.data.is_empty());

    // A hub that doesn't enable the capability hears nothing of it.
    drop(hub);
    let (mut hub, _) = FakeHub::handshake(&listener).await;
    hub.accept_plugin(&[], 0).await;
    let quiet = tokio::time::timeout(Duration::from_millis(200), async {
        loop {
            let frame = hub.frame().await;
            assert_ne!(frame.code(), MsgCode::LiveFeed, "not negotiated");
        }
    })
    .await;
    assert!(quiet.is_err());
    runner.abort();
}
