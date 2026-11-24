//! Live feeds: per node, what's being watched and the newest item of each
//! watched channel. In memory only, and only while someone watches: nothing
//! here reaches a disk, the telemetry or an export.
//!
//! Viewers survive a plugin's reconnect or relaunch: their subscription stays,
//! the new connection is told what's watched, and frames resume.

use bytes::Bytes;
use flwrs_plugin::plugin::live::{MAX_CHANNELS, channel_is_valid, media_type_is_valid};
use flwrs_plugin::schema::protocol::LiveFeed;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use tokio::sync::watch;

/// One item of a channel.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LiveFrame {
    pub media_type: String,
    pub data: Bytes,
    pub seq: u64,
    /// Unix milliseconds.
    pub capture_time: Option<i64>,
}

/// A channel a node's plugin announced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LiveChannel {
    pub channel: String,
    pub media_type: String,
    /// Viewers now.
    pub viewers: usize,
}

/// Every node's live feeds. Cheap to clone.
#[derive(Clone, Default)]
pub(crate) struct LiveFeeds {
    nodes: Arc<Mutex<HashMap<String, NodeFeed>>>,
}

struct NodeFeed {
    /// The connection with `LIVE_FEED` now.
    conn: Option<u64>,
    /// Channel → media type, from the current connection.
    announced: BTreeMap<String, String>,
    viewers: HashMap<String, Viewers>,
    /// What the connection's writer tells the plugin.
    watched: watch::Sender<BTreeSet<String>>,
}

struct Viewers {
    count: usize,
    newest: watch::Sender<Option<Arc<LiveFrame>>>,
}

impl NodeFeed {
    fn new() -> Self {
        Self {
            conn: None,
            announced: BTreeMap::new(),
            viewers: HashMap::new(),
            watched: watch::Sender::new(BTreeSet::new()),
        }
    }

    fn publish_watched(&self) {
        let channels: BTreeSet<String> = self.viewers.keys().cloned().collect();
        self.watched.send_if_modified(|watched| {
            let changed = *watched != channels;
            if changed {
                *watched = channels;
            }
            changed
        });
    }

    fn unused(&self) -> bool {
        self.conn.is_none() && self.viewers.is_empty()
    }
}

/// Why a `LiveFeed` frame broke the protocol.
pub(crate) fn violation(feed: &LiveFeed) -> Option<String> {
    if !channel_is_valid(&feed.channel) {
        return Some(format!("invalid live feed channel {:?}", feed.channel));
    }
    if !media_type_is_valid(&feed.media_type) {
        return Some(format!(
            "invalid live feed media type {:?}",
            feed.media_type
        ));
    }
    None
}

impl LiveFeeds {
    fn nodes(&self) -> MutexGuard<'_, HashMap<String, NodeFeed>> {
        self.nodes.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A connection with the capability is up. What it announced before is
    /// forgotten. The receiver says what to tell the plugin: it reports a
    /// change at once if something is already being watched.
    pub(crate) fn connected(
        &self,
        node_id: &str,
        conn_id: u64,
    ) -> watch::Receiver<BTreeSet<String>> {
        let mut nodes = self.nodes();
        let node = nodes
            .entry(node_id.to_string())
            .or_insert_with(NodeFeed::new);
        node.conn = Some(conn_id);
        node.announced.clear();
        for viewers in node.viewers.values() {
            viewers.newest.send_replace(None);
        }
        let mut rx = node.watched.subscribe();
        if !rx.borrow().is_empty() {
            rx.mark_changed();
        }
        rx
    }

    /// The connection is gone (if it's still the node's). Viewers stay, with
    /// nothing to show until the plugin is back.
    pub(crate) fn disconnected(&self, node_id: &str, conn_id: u64) {
        let mut nodes = self.nodes();
        let Some(node) = nodes.get_mut(node_id) else {
            return;
        };
        if node.conn != Some(conn_id) {
            return;
        }
        node.conn = None;
        node.announced.clear();
        for viewers in node.viewers.values() {
            viewers.newest.send_replace(None);
        }
        if node.unused() {
            nodes.remove(node_id);
        }
    }

    /// A checked frame from connection `conn_id`. Never blocks. Empty `data`
    /// only announces the channel; data for an unwatched channel is dropped.
    /// `false`: a new channel beyond the limit, ignored.
    pub(crate) fn receive(&self, node_id: &str, conn_id: u64, feed: LiveFeed) -> bool {
        let mut nodes = self.nodes();
        let Some(node) = nodes.get_mut(node_id).filter(|n| n.conn == Some(conn_id)) else {
            return true; // a connection that's been replaced
        };
        if !node.announced.contains_key(&feed.channel) && node.announced.len() >= MAX_CHANNELS {
            return false;
        }
        node.announced
            .insert(feed.channel.clone(), feed.media_type.clone());
        if feed.data.is_empty() {
            return true;
        }
        if let Some(viewers) = node.viewers.get(&feed.channel) {
            let capture_time = feed
                .capture_time
                .map(|t| t.seconds.saturating_mul(1000) + i64::from(t.nanos) / 1_000_000);
            viewers.newest.send_replace(Some(Arc::new(LiveFrame {
                media_type: feed.media_type,
                data: feed.data.into(),
                seq: feed.seq,
                capture_time,
            })));
        }
        true
    }

    /// The channels of a node's current connection.
    pub(crate) fn channels(&self, node_id: &str) -> Vec<LiveChannel> {
        let nodes = self.nodes();
        let Some(node) = nodes.get(node_id) else {
            return vec![];
        };
        node.announced
            .iter()
            .map(|(channel, media_type)| LiveChannel {
                channel: channel.clone(),
                media_type: media_type.clone(),
                viewers: node.viewers.get(channel).map_or(0, |v| v.count),
            })
            .collect()
    }

    /// Starts watching an announced channel (`None`: it isn't). Dropping the
    /// subscription stops it.
    pub(crate) fn subscribe(&self, node_id: &str, channel: &str) -> Option<LiveSubscription> {
        let mut nodes = self.nodes();
        let node = nodes.get_mut(node_id)?;
        let media_type = node.announced.get(channel)?.clone();
        let viewers = node
            .viewers
            .entry(channel.to_string())
            .or_insert_with(|| Viewers {
                count: 0,
                newest: watch::Sender::new(None),
            });
        viewers.count += 1;
        let mut frames = viewers.newest.subscribe();
        frames.mark_changed(); // the newest item, if there is one, comes first
        node.publish_watched();
        Some(LiveSubscription {
            feeds: self.clone(),
            node_id: node_id.to_string(),
            channel: channel.to_string(),
            media_type,
            frames,
        })
    }

    fn release(&self, node_id: &str, channel: &str) {
        let mut nodes = self.nodes();
        let Some(node) = nodes.get_mut(node_id) else {
            return;
        };
        if let Some(viewers) = node.viewers.get_mut(channel) {
            viewers.count -= 1;
            if viewers.count == 0 {
                node.viewers.remove(channel); // and its newest item
                node.publish_watched();
            }
        }
        if node.unused() {
            nodes.remove(node_id);
        }
    }
}

/// One viewer of a channel.
pub(crate) struct LiveSubscription {
    feeds: LiveFeeds,
    node_id: String,
    channel: String,
    /// What the channel carried when the viewer joined.
    media_type: String,
    frames: watch::Receiver<Option<Arc<LiveFrame>>>,
}

impl LiveSubscription {
    #[cfg(test)]
    pub(crate) fn media_type(&self) -> &str {
        &self.media_type
    }

    /// The next item (skipping any it was too slow for). `None`: the channel
    /// changed its media type, so this viewer's stream has to end.
    pub(crate) async fn next(&mut self) -> Option<Arc<LiveFrame>> {
        loop {
            self.frames.changed().await.ok()?;
            let frame = self.frames.borrow_and_update().clone();
            if let Some(frame) = frame {
                return (frame.media_type == self.media_type).then_some(frame);
            }
        }
    }
}

impl Drop for LiveSubscription {
    fn drop(&mut self) {
        self.feeds.release(&self.node_id, &self.channel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(channel: &str, media_type: &str, data: &[u8], seq: u64) -> LiveFeed {
        LiveFeed {
            channel: channel.into(),
            media_type: media_type.into(),
            data: data.to_vec(),
            seq,
            capture_time: None,
        }
    }

    #[test]
    fn checks_names_and_media_types() {
        assert!(violation(&frame("stats", "application/json", b"", 0)).is_none());
        assert!(violation(&frame("a b", "application/json", b"", 0)).is_some());
        assert!(violation(&frame("stats", "json", b"", 0)).is_some());
    }

    #[tokio::test]
    async fn viewers_are_counted_and_frames_kept_only_while_watched() {
        let feeds = LiveFeeds::default();
        let watched = feeds.connected("n", 1);
        assert!(feeds.subscribe("n", "preview").is_none(), "not announced");
        assert!(feeds.receive("n", 1, frame("preview", "image/jpeg", b"", 0)));
        // Nobody watches: dropped.
        feeds.receive("n", 1, frame("preview", "image/jpeg", b"old", 1));
        let mut first = feeds.subscribe("n", "preview").unwrap();
        let second = feeds.subscribe("n", "preview").unwrap();
        assert_eq!(feeds.channels("n")[0].viewers, 2);
        assert!(watched.borrow().contains("preview"));

        feeds.receive("n", 1, frame("preview", "image/jpeg", b"a", 2));
        feeds.receive("n", 1, frame("preview", "image/jpeg", b"b", 3));
        assert_eq!(first.next().await.unwrap().data, Bytes::from_static(b"b"));

        drop(second);
        assert!(watched.borrow().contains("preview"));
        drop(first);
        assert!(watched.borrow().is_empty());
        assert_eq!(feeds.channels("n")[0].viewers, 0);
    }

    #[tokio::test]
    async fn a_reconnect_keeps_viewers_and_forgets_the_old_connection() {
        let feeds = LiveFeeds::default();
        feeds.connected("n", 1);
        feeds.receive("n", 1, frame("stats", "application/json", b"", 0));
        let mut viewer = feeds.subscribe("n", "stats").unwrap();
        feeds.receive("n", 1, frame("stats", "application/json", b"1", 1));
        assert_eq!(viewer.next().await.unwrap().seq, 1);

        feeds.disconnected("n", 1);
        assert!(
            feeds.channels("n").is_empty(),
            "announcements are per connection"
        );
        let mut watched = feeds.connected("n", 2);
        assert!(watched.has_changed().unwrap(), "the new connection is told");
        assert!(watched.borrow_and_update().contains("stats"));
        // A late frame from the old connection is ignored.
        feeds.receive("n", 1, frame("stats", "application/json", b"late", 2));
        feeds.disconnected("n", 1);
        assert_eq!(feeds.nodes().get("n").unwrap().conn, Some(2));
        feeds.receive("n", 2, frame("stats", "application/json", b"2", 3));
        assert_eq!(viewer.next().await.unwrap().seq, 3);
    }

    #[tokio::test]
    async fn a_changed_media_type_ends_the_stream() {
        let feeds = LiveFeeds::default();
        feeds.connected("n", 1);
        feeds.receive("n", 1, frame("c", "image/jpeg", b"", 0));
        let mut viewer = feeds.subscribe("n", "c").unwrap();
        assert_eq!(viewer.media_type(), "image/jpeg");
        feeds.receive("n", 1, frame("c", "image/png", b"x", 1));
        assert!(viewer.next().await.is_none());
    }

    #[test]
    fn limits_channels_and_cleans_up() {
        let feeds = LiveFeeds::default();
        feeds.connected("n", 1);
        for n in 0..MAX_CHANNELS {
            assert!(feeds.receive("n", 1, frame(&format!("c{n}"), "a/b", b"", 0)));
        }
        assert!(!feeds.receive("n", 1, frame("more", "a/b", b"", 0)));
        assert!(feeds.receive("n", 1, frame("c0", "a/c", b"", 0)), "known");
        feeds.disconnected("n", 1);
        assert!(feeds.nodes().is_empty());
    }
}
