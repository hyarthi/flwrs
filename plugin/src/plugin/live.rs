//! The live feed (capability `LIVE_FEED`): observation data such as stats and
//! preview frames, outside the data flow.
//!
//! A plugin creates one [`LiveFeedPublisher`], offers it with
//! `InitializeRequest::with_live_feed`, announces its channels, and publishes
//! only while [`LiveFeedPublisher::is_watched`] says someone is looking.
//! Publishing never blocks: each channel keeps only its newest item, and one
//! task writes them to the hub, so a slow link just skips frames.

use crate::plugin::frame::encode_message;
use crate::plugin::msg_client::HubClient;
use crate::schema::protocol::LiveFeed;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{Notify, watch};

/// Longest channel name.
pub const MAX_CHANNEL_LEN: usize = 64;
/// Longest media type (RFC 6838: 127 for each of its two parts is the upper
/// bound; flwrs keeps the whole type within it).
pub const MAX_MEDIA_TYPE_LEN: usize = 127;
/// Most channels one plugin may have.
pub const MAX_CHANNELS: usize = 16;

/// Whether `channel` is a valid channel name.
pub fn channel_is_valid(channel: &str) -> bool {
    (1..=MAX_CHANNEL_LEN).contains(&channel.len())
        && channel
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Whether `media_type` is a valid `type/subtype`, without parameters.
pub fn media_type_is_valid(media_type: &str) -> bool {
    let token = |part: &str| {
        !part.is_empty()
            && part.as_bytes()[0].is_ascii_alphanumeric()
            && part.bytes().all(|b| {
                b.is_ascii_alphanumeric()
                    || matches!(
                        b,
                        b'!' | b'#' | b'$' | b'&' | b'-' | b'^' | b'_' | b'.' | b'+'
                    )
            })
    };
    media_type.len() <= MAX_MEDIA_TYPE_LEN
        && media_type
            .split_once('/')
            .is_some_and(|(kind, subtype)| token(kind) && token(subtype))
}

/// Why a channel couldn't be announced.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LiveFeedError {
    #[error("invalid live feed channel name {0:?}")]
    InvalidChannel(String),
    #[error("invalid media type {0:?}")]
    InvalidMediaType(String),
    #[error("too many live feed channels (at most {MAX_CHANNELS})")]
    TooManyChannels,
}

/// A plugin's handle to the live feed. Cheap to clone; publishing is
/// synchronous and never waits for the hub.
#[derive(Clone)]
pub struct LiveFeedPublisher {
    inner: Arc<Inner>,
}

struct Inner {
    channels: Mutex<BTreeMap<String, Slot>>,
    watched: watch::Sender<BTreeSet<String>>,
    wake: Notify,
    /// Set once announced channels must be announced again.
    reannounce: AtomicBool,
}

struct Slot {
    media_type: String,
    /// Sent on the current connection.
    announced: bool,
    seq: u64,
    /// The newest item not sent yet.
    newest: Option<Pending>,
}

struct Pending {
    data: Vec<u8>,
    seq: u64,
    capture_time: Option<pbjson_types::Timestamp>,
}

impl Default for LiveFeedPublisher {
    fn default() -> Self {
        Self::new()
    }
}

impl LiveFeedPublisher {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                channels: Mutex::new(BTreeMap::new()),
                watched: watch::Sender::new(BTreeSet::new()),
                wake: Notify::new(),
                reannounce: AtomicBool::new(false),
            }),
        }
    }

    /// Declares a channel and what it carries, so the hub can list it before
    /// anyone watches. Announced again on every connection. Announcing a
    /// channel with another media type changes it.
    pub fn announce(&self, channel: &str, media_type: &str) -> Result<(), LiveFeedError> {
        if !channel_is_valid(channel) {
            return Err(LiveFeedError::InvalidChannel(channel.into()));
        }
        if !media_type_is_valid(media_type) {
            return Err(LiveFeedError::InvalidMediaType(media_type.into()));
        }
        {
            let mut channels = self.inner.lock();
            match channels.get_mut(channel) {
                Some(slot) if slot.media_type == media_type => return Ok(()),
                Some(slot) => {
                    slot.media_type = media_type.into();
                    slot.announced = false;
                    slot.newest = None;
                }
                None => {
                    if channels.len() >= MAX_CHANNELS {
                        return Err(LiveFeedError::TooManyChannels);
                    }
                    channels.insert(
                        channel.into(),
                        Slot {
                            media_type: media_type.into(),
                            announced: false,
                            seq: 0,
                            newest: None,
                        },
                    );
                }
            }
        }
        self.inner.wake.notify_one();
        Ok(())
    }

    /// Whether someone is watching `channel` now. Check it before doing the
    /// work of producing an item.
    pub fn is_watched(&self, channel: &str) -> bool {
        self.inner.watched.borrow().contains(channel)
    }

    /// The set of watched channels, to wait for a change.
    pub fn watched(&self) -> watch::Receiver<BTreeSet<String>> {
        self.inner.watched.subscribe()
    }

    /// Replaces the channel's newest item. Returns whether it was taken:
    /// `false` if the channel isn't announced or nobody is watching it.
    /// Never waits; an item the hub hasn't been sent yet is dropped.
    pub fn publish(
        &self,
        channel: &str,
        data: impl Into<Vec<u8>>,
        capture_time: Option<SystemTime>,
    ) -> bool {
        if !self.is_watched(channel) {
            return false;
        }
        {
            let mut channels = self.inner.lock();
            let Some(slot) = channels.get_mut(channel) else {
                return false;
            };
            slot.seq += 1;
            slot.newest = Some(Pending {
                data: data.into(),
                seq: slot.seq,
                capture_time: capture_time.and_then(timestamp),
            });
        }
        self.inner.wake.notify_one();
        true
    }

    pub(crate) fn set_watched(&self, channels: BTreeSet<String>) {
        self.inner.watched.send_if_modified(|watched| {
            let changed = *watched != channels;
            if changed {
                *watched = channels;
            }
            changed
        });
    }
}

impl Inner {
    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Slot>> {
        self.channels.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// What's to send now: announcements first, then the newest item of each
    /// watched channel. Items of channels nobody watches any more are dropped.
    fn take(&self) -> Vec<LiveFeed> {
        let watched = self.watched.borrow().clone();
        let reannounce = self.reannounce.swap(false, Ordering::AcqRel);
        let mut frames = vec![];
        for (channel, slot) in self.lock().iter_mut() {
            if reannounce {
                slot.announced = false;
            }
            if !slot.announced {
                slot.announced = true;
                frames.push(LiveFeed {
                    channel: channel.clone(),
                    media_type: slot.media_type.clone(),
                    ..Default::default()
                });
            }
            if let Some(item) = slot.newest.take()
                && watched.contains(channel)
            {
                frames.push(LiveFeed {
                    channel: channel.clone(),
                    media_type: slot.media_type.clone(),
                    data: item.data,
                    seq: item.seq,
                    capture_time: item.capture_time,
                });
            }
        }
        frames
    }
}

fn timestamp(time: SystemTime) -> Option<pbjson_types::Timestamp> {
    let since = time.duration_since(UNIX_EPOCH).ok()?;
    Some(pbjson_types::Timestamp {
        seconds: i64::try_from(since.as_secs()).ok()?,
        nanos: i32::try_from(since.subsec_nanos()).ok()?,
    })
}

/// Sends the feed on each connection where the hub enabled it. Ends when the
/// client is gone.
pub(crate) async fn run(
    client: Weak<HubClient>,
    mut conn: watch::Receiver<Option<u64>>,
    live: LiveFeedPublisher,
) {
    loop {
        let current = *conn.borrow_and_update();
        let Some(id) = current else {
            if conn.changed().await.is_err() {
                return;
            }
            continue;
        };
        live.inner.reannounce.store(true, Ordering::Release);
        'session: loop {
            let notified = live.inner.wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let frames = live.inner.take();
            if frames.is_empty() {
                tokio::select! {
                    () = &mut notified => continue,
                    changed = conn.changed() => {
                        if changed.is_err() {
                            return;
                        }
                        break 'session;
                    }
                }
            }
            let Some(client) = client.upgrade() else {
                return;
            };
            for frame in frames {
                // One frame at a time, so events and heartbeats wait for at
                // most one of them. A failure means the connection is gone.
                if client
                    .send_live_feed(id, &encode_message(&frame))
                    .await
                    .is_err()
                {
                    break 'session;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_media_types() {
        for good in [
            "stats",
            "preview.main",
            "a-b_c",
            &"x".repeat(MAX_CHANNEL_LEN),
        ] {
            assert!(channel_is_valid(good), "{good}");
        }
        for bad in ["", "a b", "a/b", "é", &"x".repeat(MAX_CHANNEL_LEN + 1)] {
            assert!(!channel_is_valid(bad), "{bad}");
        }
        for good in ["image/jpeg", "application/json", "application/vnd.a+json"] {
            assert!(media_type_is_valid(good), "{good}");
        }
        for bad in [
            "",
            "image",
            "/jpeg",
            "image/",
            "image/jpeg; q=1",
            "text/plain\r\nX: y",
            "a/b/c",
        ] {
            assert!(!media_type_is_valid(bad), "{bad}");
        }
    }

    #[test]
    fn publishes_only_while_watched_and_keeps_the_newest() {
        let live = LiveFeedPublisher::new();
        live.announce("preview", "image/jpeg").unwrap();
        assert!(!live.is_watched("preview"));
        assert!(!live.publish("preview", vec![1], None));
        // The announcement alone.
        let first = live.inner.take();
        assert_eq!(first.len(), 1);
        assert!(first[0].data.is_empty());

        live.set_watched(["preview".to_string()].into());
        assert!(live.publish("preview", vec![1], None));
        assert!(live.publish("preview", vec![2], Some(UNIX_EPOCH)));
        let frames = live.inner.take();
        assert_eq!(frames.len(), 1, "latest wins");
        assert_eq!((frames[0].data.as_slice(), frames[0].seq), (&[2u8][..], 2));
        assert_eq!(frames[0].capture_time.as_ref().unwrap().seconds, 0);

        assert!(live.publish("preview", vec![3], None));
        live.set_watched(BTreeSet::new());
        assert!(live.inner.take().is_empty(), "nobody watches it any more");
        assert!(!live.publish("other", vec![1], None));
    }

    #[test]
    fn announcing_checks_its_input_and_the_channel_limit() {
        let live = LiveFeedPublisher::new();
        assert!(matches!(
            live.announce("a b", "image/jpeg"),
            Err(LiveFeedError::InvalidChannel(_))
        ));
        assert!(matches!(
            live.announce("a", "jpeg"),
            Err(LiveFeedError::InvalidMediaType(_))
        ));
        for n in 0..MAX_CHANNELS {
            live.announce(&format!("c{n}"), "application/json").unwrap();
        }
        assert_eq!(
            live.announce("one-more", "application/json"),
            Err(LiveFeedError::TooManyChannels)
        );
        // An existing channel can still change type.
        live.announce("c0", "image/png").unwrap();
    }
}
