//! The broadcast core of the push sinks (HTTP: SSE and long polling;
//! WebSocket): events go to named channels, each with a ring of recent events
//! for replay; subscribers read a channel through a cursor into its ring.
//!
//! - **Ids** are `<epoch>-<seq>`: the epoch is new each time the plugin
//!   starts, the seq counts up per channel. A subscriber resuming from an id
//!   of another run, or one the ring has dropped, gets a [`Item::Gap`] first.
//! - **Slow subscribers** never hold up the producer: one whose cursor falls
//!   behind the ring's oldest event is cut off ([`SlowPolicy::Disconnect`])
//!   or jumps to the oldest with a gap ([`SlowPolicy::Skip`]).
//! - **Acks** ([`AckMode`]): `Buffered` = done once the event is in the ring;
//!   `Delivered` = done once a subscriber has written it to its connection
//!   ([`Subscriber::delivered`]). With nobody to take it, a `Delivered` event
//!   waits (unacked, so the hub's window holds the rest back in the node's
//!   queue) until someone subscribes, or until `delivery_timeout`.
//! - **Channels** are created when first used (by a publish or a subscribe)
//!   and removed when they're empty and nobody reads them; names must match
//!   `allowed_channels` (glob patterns: `*` within a `/`-separated segment,
//!   `**` across them), and there are at most `max_channels`.

mod args;
mod envelope;
mod sink;
pub use args::{BroadcastArgs, BroadcastSettings, in_schema, message_of, schema_properties};
pub use envelope::{envelope, text_of};
pub use sink::publish;

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{oneshot, watch};

/// When a published event counts as handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AckMode {
    /// Once it's in the ring (the guarantee ends there).
    Buffered,
    /// Once a subscriber's connection has taken it.
    Delivered,
}

/// What happens to a subscriber that falls behind the ring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlowPolicy {
    /// Its subscription ends ([`Item::Lagged`]).
    Disconnect,
    /// It goes on from the oldest event in the ring, after an [`Item::Gap`].
    Skip,
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Events kept per channel.
    pub max_events: usize,
    /// Bytes (of `data`) kept per channel.
    pub max_bytes: usize,
    pub max_channels: usize,
    /// Subscribers in all (long polls waiting included).
    pub max_subscribers: usize,
    pub max_subscribers_per_channel: usize,
    pub slow_policy: SlowPolicy,
    /// Patterns channel names must match (empty: any name).
    pub allowed_channels: Vec<String>,
    /// For events that name no channel.
    pub default_channel: String,
    /// `Delivered`: how long an event waits for a subscriber (`None`: as long
    /// as it takes).
    pub delivery_timeout: Option<Duration>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_events: 1000,
            max_bytes: 8 * 1024 * 1024,
            max_channels: 256,
            max_subscribers: 10_000,
            max_subscribers_per_channel: 1_000,
            slow_policy: SlowPolicy::Disconnect,
            allowed_channels: vec![],
            default_channel: "default".into(),
            delivery_timeout: None,
        }
    }
}

/// An event as subscribers get it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// The event type (SSE `event:`, the WebSocket envelope's `event`).
    pub event: Option<String>,
    pub content_type: Option<String>,
    pub data: Vec<u8>,
}

/// A message in a channel's ring.
#[derive(Debug, PartialEq, Eq)]
pub struct Stored {
    /// `<epoch>-<seq>`.
    pub id: String,
    pub seq: u64,
    pub message: Message,
}

/// What a subscriber reads next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    Event(Arc<Stored>),
    /// Events were missed: how many, when known (not across a restart).
    Gap {
        missed: Option<u64>,
    },
    /// Fell behind the ring under [`SlowPolicy::Disconnect`]: the end.
    Lagged,
    /// The broadcaster stopped: the end.
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// The name isn't allowed (or isn't a valid channel name).
    Channel(String),
    /// `max_channels` channels exist.
    TooManyChannels,
    /// `max_subscribers` (in all, or for the channel) read now.
    TooManySubscribers,
    /// One event larger than a channel may hold.
    TooLarge { bytes: usize, max: usize },
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refused::Channel(why) => write!(f, "{why}"),
            Refused::TooManyChannels => write!(f, "too many channels"),
            Refused::TooManySubscribers => write!(f, "too many subscribers"),
            Refused::TooLarge { bytes, max } => {
                write!(f, "the event's {bytes} bytes exceed a channel's {max}")
            }
        }
    }
}

/// Why a `Delivered` event wasn't delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Undelivered {
    /// Nobody took it within `delivery_timeout` (it's out of the ring).
    Timeout,
    /// Newer events pushed it out of the ring before anyone took it.
    Evicted,
    /// The broadcaster stopped first.
    Closed,
}

/// A published event's ack.
pub struct Delivery {
    inner: Option<(oneshot::Receiver<()>, Arc<Chan>, u64)>,
    timeout: Option<Duration>,
    closed: watch::Receiver<bool>,
}

impl Delivery {
    /// Done when the ack mode says so.
    pub async fn done(mut self) -> Result<(), Undelivered> {
        let Some((rx, chan, seq)) = self.inner.take() else {
            return Ok(());
        };
        let wait = async {
            tokio::select! {
                r = rx => match r {
                    Ok(()) => Ok(()),
                    Err(_) if *self.closed.borrow() => Err(Undelivered::Closed),
                    Err(_) => Err(Undelivered::Evicted),
                },
                _ = stopped(&mut self.closed) => Err(Undelivered::Closed),
            }
        };
        let result = match self.timeout {
            None => wait.await,
            Some(t) => tokio::time::timeout(t, wait)
                .await
                .unwrap_or(Err(Undelivered::Timeout)),
        };
        if result.is_err() {
            // Out of the ring: the hub redelivers it, and replay mustn't send it twice.
            chan.withdraw(seq);
        }
        result
    }
}

impl Drop for Delivery {
    fn drop(&mut self) {
        // Dropped unfinished (e.g. the hub connection went): withdrawn too.
        if let Some((_, chan, seq)) = self.inner.take() {
            chan.withdraw(seq);
        }
    }
}

async fn stopped(closed: &mut watch::Receiver<bool>) {
    let _ = closed.wait_for(|c| *c).await;
}

struct Entry {
    stored: Arc<Stored>,
    withdrawn: bool,
}

#[derive(Default)]
struct Ring {
    entries: VecDeque<Entry>,
    bytes: usize,
    /// The next event's seq (seqs start at 1).
    next_seq: u64,
    /// `Delivered` events not taken yet.
    pending: HashMap<u64, oneshot::Sender<()>>,
    subscribers: usize,
}

impl Ring {
    fn first_seq(&self) -> u64 {
        self.entries.front().map_or(self.next_seq, |e| e.stored.seq)
    }
}

struct Chan {
    name: String,
    ring: Mutex<Ring>,
    /// The newest seq, for subscribers waiting for more.
    head: watch::Sender<u64>,
}

impl Chan {
    fn withdraw(&self, seq: u64) {
        let mut ring = self.ring.lock().unwrap_or_else(|e| e.into_inner());
        ring.pending.remove(&seq);
        let first = ring.first_seq();
        if let Some(entry) = seq
            .checked_sub(first)
            .and_then(|i| ring.entries.get_mut(i as usize))
        {
            entry.withdrawn = true;
        }
    }
}

/// Glob match: `*` within a segment, `**` across segments.
fn glob(pattern: &str, name: &str) -> bool {
    fn go(p: &[u8], n: &[u8]) -> bool {
        match p {
            [] => n.is_empty(),
            [b'*', b'*', rest @ ..] => (0..=n.len()).any(|i| go(rest, &n[i..])),
            [b'*', rest @ ..] => (0..=n.len())
                .take_while(|&i| i == 0 || n[i - 1] != b'/')
                .any(|i| go(rest, &n[i..])),
            [c, rest @ ..] => n.first() == Some(c) && go(rest, &n[1..]),
        }
    }
    go(pattern.as_bytes(), name.as_bytes())
}

/// Checks a list of `allowed_channels` patterns.
pub fn check_patterns(patterns: &[String]) -> Vec<String> {
    patterns
        .iter()
        .filter(|p| p.is_empty() || p.len() > 256 || p.chars().any(|c| c.is_control()))
        .map(|p| format!("allowed_channels: {p:?} isn't a pattern"))
        .collect()
}

/// The event-id epoch: the start time in milliseconds, base 36.
fn new_epoch() -> String {
    let mut n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let mut out = vec![];
    while n > 0 {
        out.push(b"0123456789abcdefghijklmnopqrstuvwxyz"[(n % 36) as usize]);
        n /= 36;
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

/// The channels.
pub struct Broadcaster {
    config: Config,
    epoch: String,
    channels: Mutex<HashMap<String, Arc<Chan>>>,
    /// Subscribers in all (changed under the channel table's lock).
    subscribers: AtomicUsize,
    closed: watch::Sender<bool>,
}

/// Counters for status and tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    pub channels: usize,
    pub subscribers: usize,
    pub events: usize,
    pub bytes: usize,
    pub pending: usize,
}

impl Broadcaster {
    pub fn new(config: Config) -> Arc<Self> {
        Self::with_epoch(config, new_epoch())
    }

    pub fn with_epoch(config: Config, epoch: String) -> Arc<Self> {
        Arc::new(Self {
            config,
            epoch,
            channels: Mutex::default(),
            subscribers: AtomicUsize::new(0),
            closed: watch::channel(false).0,
        })
    }

    pub fn epoch(&self) -> &str {
        &self.epoch
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// An event's channel: its own, or the default; checked against the patterns.
    pub fn channel_name<'a>(&'a self, named: Option<&'a str>) -> Result<&'a str, Refused> {
        let name = named
            .filter(|n| !n.is_empty())
            .unwrap_or(&self.config.default_channel);
        if name.len() > 256 || name.chars().any(|c| c.is_control()) {
            return Err(Refused::Channel(format!("{name:?} isn't a channel name")));
        }
        if !self.config.allowed_channels.is_empty()
            && !self.config.allowed_channels.iter().any(|p| glob(p, name))
        {
            return Err(Refused::Channel(format!(
                "channel {name:?} isn't in allowed_channels"
            )));
        }
        Ok(name)
    }

    /// The channel, created if need be. The caller holds the channel table's
    /// lock until it has used the channel, so cleanup can't remove it between.
    fn channel(
        &self,
        channels: &mut HashMap<String, Arc<Chan>>,
        name: &str,
    ) -> Result<Arc<Chan>, Refused> {
        if let Some(c) = channels.get(name) {
            return Ok(c.clone());
        }
        if channels.len() >= self.config.max_channels {
            // Make room: channels nobody reads and that hold nothing.
            channels.retain(|_, c| {
                let ring = c.ring.lock().unwrap_or_else(|e| e.into_inner());
                ring.subscribers > 0 || !ring.entries.is_empty() || !ring.pending.is_empty()
            });
            if channels.len() >= self.config.max_channels {
                return Err(Refused::TooManyChannels);
            }
        }
        let chan = Arc::new(Chan {
            name: name.to_string(),
            ring: Mutex::new(Ring {
                next_seq: 1,
                ..Ring::default()
            }),
            head: watch::channel(0).0,
        });
        channels.insert(name.to_string(), chan.clone());
        Ok(chan)
    }

    /// Puts an event into its channel. The [`Delivery`] says when it's handled.
    pub fn publish(
        &self,
        channel: Option<&str>,
        message: Message,
        ack: AckMode,
    ) -> Result<Delivery, Refused> {
        let name = self.channel_name(channel)?;
        let bytes = message.data.len();
        if bytes > self.config.max_bytes {
            return Err(Refused::TooLarge {
                bytes,
                max: self.config.max_bytes,
            });
        }
        let mut channels = self.channels.lock().unwrap_or_else(|e| e.into_inner());
        let chan = self.channel(&mut channels, name)?;
        let mut ring = chan.ring.lock().unwrap_or_else(|e| e.into_inner());
        let seq = ring.next_seq;
        ring.next_seq += 1;
        // Room first: the oldest go. One still waiting for a subscriber
        // can't be taken any more: its delivery fails (the hub sends it again).
        while ring.entries.len() >= self.config.max_events
            || ring.bytes + bytes > self.config.max_bytes
        {
            let Some(old) = ring.entries.pop_front() else {
                break;
            };
            ring.bytes -= old.stored.message.data.len();
            ring.pending.remove(&old.stored.seq);
        }
        ring.bytes += bytes;
        let stored = Arc::new(Stored {
            id: format!("{}-{seq}", self.epoch),
            seq,
            message,
        });
        ring.entries.push_back(Entry {
            stored,
            withdrawn: false,
        });
        let inner = match ack {
            AckMode::Buffered => None,
            AckMode::Delivered => {
                let (tx, rx) = oneshot::channel();
                ring.pending.insert(seq, tx);
                Some((rx, chan.clone(), seq))
            }
        };
        drop(ring);
        drop(channels);
        chan.head.send_replace(seq);
        Ok(Delivery {
            inner,
            timeout: self.config.delivery_timeout,
            closed: self.closed.subscribe(),
        })
    }

    /// Subscribes to a channel: live events, or from after `last_id` (an
    /// id from an earlier event; replayed from the ring, with a gap if the
    /// ring no longer has the ones right after it).
    pub fn subscribe(
        self: &Arc<Self>,
        channel: &str,
        last_id: Option<&str>,
    ) -> Result<Subscriber, Refused> {
        let name = self.channel_name(Some(channel))?.to_string();
        let mut channels = self.channels.lock().unwrap_or_else(|e| e.into_inner());
        if self.subscribers.load(Ordering::SeqCst) >= self.config.max_subscribers {
            return Err(Refused::TooManySubscribers);
        }
        let chan = self.channel(&mut channels, &name)?;
        let mut gap = None;
        let cursor = {
            let mut ring = chan.ring.lock().unwrap_or_else(|e| e.into_inner());
            if ring.subscribers >= self.config.max_subscribers_per_channel {
                return Err(Refused::TooManySubscribers);
            }
            ring.subscribers += 1;
            self.subscribers.fetch_add(1, Ordering::SeqCst);
            let first = ring.first_seq();
            match last_id {
                // Live, but events still waiting for a first taker come first.
                None => ring.pending.keys().min().copied().unwrap_or(ring.next_seq),
                Some(id) => match id.rsplit_once('-') {
                    Some((epoch, seq)) if epoch == self.epoch && seq.parse::<u64>().is_ok() => {
                        let after = seq.parse::<u64>().unwrap_or_default() + 1;
                        let after = after.min(ring.next_seq);
                        if after < first {
                            gap = Some(Item::Gap {
                                missed: Some(first - after),
                            });
                            first
                        } else {
                            after
                        }
                    }
                    // Another run's (or not an id): what's in the ring, after a gap.
                    _ => {
                        gap = Some(Item::Gap { missed: None });
                        first
                    }
                },
            }
        };
        drop(channels);
        let head = chan.head.subscribe();
        Ok(Subscriber {
            owner: self.clone(),
            chan,
            cursor,
            gap,
            head,
            closed: self.closed.subscribe(),
            ended: false,
        })
    }

    /// Stops: subscribers end with [`Item::Closed`], waiting deliveries fail.
    pub fn close(&self) {
        self.closed.send_replace(true);
    }

    pub fn stats(&self) -> Stats {
        let channels = self.channels.lock().unwrap_or_else(|e| e.into_inner());
        let mut s = Stats {
            channels: channels.len(),
            ..Stats::default()
        };
        for c in channels.values() {
            let ring = c.ring.lock().unwrap_or_else(|e| e.into_inner());
            s.subscribers += ring.subscribers;
            s.events += ring.entries.len();
            s.bytes += ring.bytes;
            s.pending += ring.pending.len();
        }
        s
    }

    /// Whether anyone reads `channel` now.
    pub fn has_subscribers(&self, channel: &str) -> bool {
        let channels = self.channels.lock().unwrap_or_else(|e| e.into_inner());
        channels
            .get(channel)
            .is_some_and(|c| c.ring.lock().unwrap_or_else(|e| e.into_inner()).subscribers > 0)
    }

    fn release(&self, chan: &Arc<Chan>) {
        let mut channels = self.channels.lock().unwrap_or_else(|e| e.into_inner());
        let idle = {
            let mut ring = chan.ring.lock().unwrap_or_else(|e| e.into_inner());
            ring.subscribers -= 1;
            self.subscribers.fetch_sub(1, Ordering::SeqCst);
            ring.subscribers == 0 && ring.entries.is_empty() && ring.pending.is_empty()
        };
        if idle
            && channels
                .get(&chan.name)
                .is_some_and(|c| Arc::ptr_eq(c, chan))
        {
            channels.remove(&chan.name);
        }
    }
}

/// One reader of a channel.
pub struct Subscriber {
    owner: Arc<Broadcaster>,
    chan: Arc<Chan>,
    /// The next seq to read.
    cursor: u64,
    gap: Option<Item>,
    head: watch::Receiver<u64>,
    closed: watch::Receiver<bool>,
    ended: bool,
}

impl Subscriber {
    pub fn channel(&self) -> &str {
        &self.chan.name
    }

    /// An id to resume from where this subscriber is (the last event it
    /// read, or the channel's newest when it started live): a long poll's
    /// `next_cursor`.
    pub fn cursor_id(&self) -> String {
        format!("{}-{}", self.owner.epoch, self.cursor.saturating_sub(1))
    }

    /// What's next, without waiting (`None`: nothing yet).
    pub fn try_next(&mut self) -> Option<Item> {
        if self.ended {
            return Some(Item::Closed);
        }
        if *self.closed.borrow() {
            self.ended = true;
            return Some(Item::Closed);
        }
        if let Some(gap) = self.gap.take() {
            return Some(gap);
        }
        let ring = self.chan.ring.lock().unwrap_or_else(|e| e.into_inner());
        let first = ring.first_seq();
        if self.cursor < first {
            let missed = first - self.cursor;
            self.cursor = first;
            return Some(match self.owner.config.slow_policy {
                SlowPolicy::Disconnect => {
                    self.ended = true;
                    Item::Lagged
                }
                SlowPolicy::Skip => Item::Gap {
                    missed: Some(missed),
                },
            });
        }
        let mut i = (self.cursor - first) as usize;
        while let Some(entry) = ring.entries.get(i) {
            self.cursor = entry.stored.seq + 1;
            if !entry.withdrawn {
                return Some(Item::Event(entry.stored.clone()));
            }
            i += 1;
        }
        None
    }

    /// The next item: waits for one.
    pub async fn next(&mut self) -> Item {
        loop {
            // Marked seen before looking, so a publish in between wakes us.
            self.head.borrow_and_update();
            if let Some(item) = self.try_next() {
                return item;
            }
            tokio::select! {
                r = self.head.changed() => if r.is_err() { return Item::Closed },
                _ = stopped(&mut self.closed) => {}
            }
        }
    }

    /// Up to `max` events (and gaps) available now, for long polling.
    pub fn take_available(&mut self, max: usize) -> Vec<Item> {
        let mut out = vec![];
        while out.len() < max {
            match self.try_next() {
                Some(item @ (Item::Lagged | Item::Closed)) => {
                    out.push(item);
                    break;
                }
                Some(item) => out.push(item),
                None => break,
            }
        }
        out
    }

    /// The subscriber's connection took `stored` (written to it): a
    /// `Delivered` event is acked.
    pub fn delivered(&self, stored: &Stored) {
        let tx = self
            .chan
            .ring
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pending
            .remove(&stored.seq);
        if let Some(tx) = tx {
            let _ = tx.send(());
        }
    }
}

impl Drop for Subscriber {
    fn drop(&mut self) {
        self.owner.release(&self.chan);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(data: &str) -> Message {
        Message {
            event: None,
            content_type: None,
            data: data.as_bytes().to_vec(),
        }
    }

    fn broadcaster(f: impl FnOnce(&mut Config)) -> Arc<Broadcaster> {
        let mut config = Config::default();
        f(&mut config);
        Broadcaster::with_epoch(config, "e1".into())
    }

    fn data(item: Item) -> String {
        match item {
            Item::Event(s) => String::from_utf8(s.message.data.clone()).unwrap(),
            other => format!("{other:?}"),
        }
    }

    #[test]
    fn globs() {
        assert!(glob("devices/*", "devices/a"));
        assert!(!glob("devices/*", "devices/a/b"));
        assert!(glob("devices/**", "devices/a/b"));
        assert!(glob("*-alerts", "x-alerts"));
        assert!(glob("news", "news") && !glob("news", "newsy"));
    }

    #[tokio::test]
    async fn live_events_and_replay_by_id() {
        let b = broadcaster(|_| {});
        let mut live = b.subscribe("c", None).unwrap();
        for d in ["a", "b", "c"] {
            b.publish(Some("c"), msg(d), AckMode::Buffered)
                .unwrap()
                .done()
                .await
                .unwrap();
        }
        let first = live.next().await;
        let Item::Event(s) = &first else { panic!() };
        assert_eq!(s.id, "e1-1");
        assert_eq!(data(live.next().await), "b");
        // From after the first: the rest, from the ring.
        let mut resumed = b.subscribe("c", Some("e1-1")).unwrap();
        assert_eq!(
            resumed
                .take_available(10)
                .into_iter()
                .map(data)
                .collect::<Vec<_>>(),
            ["b", "c"]
        );
        // Another run's id: a gap, then everything in the ring.
        let mut stale = b.subscribe("c", Some("old-7")).unwrap();
        assert_eq!(stale.next().await, Item::Gap { missed: None });
        assert_eq!(data(stale.next().await), "a");
        // A new subscriber without an id: only what comes next.
        let mut fresh = b.subscribe("c", None).unwrap();
        assert_eq!(fresh.try_next(), None);
    }

    #[tokio::test]
    async fn the_ring_is_bounded_and_slow_subscribers_go_or_skip() {
        for policy in [SlowPolicy::Disconnect, SlowPolicy::Skip] {
            let b = broadcaster(|c| {
                c.max_events = 3;
                c.slow_policy = policy;
            });
            let mut slow = b.subscribe("c", None).unwrap();
            for i in 0..5 {
                b.publish(Some("c"), msg(&i.to_string()), AckMode::Buffered)
                    .unwrap();
            }
            assert_eq!(b.stats().events, 3);
            match policy {
                SlowPolicy::Disconnect => {
                    assert_eq!(slow.next().await, Item::Lagged);
                    assert_eq!(slow.next().await, Item::Closed);
                }
                SlowPolicy::Skip => {
                    assert_eq!(slow.next().await, Item::Gap { missed: Some(2) });
                    assert_eq!(data(slow.next().await), "2");
                }
            }
            // An id the ring dropped: a gap of the missing ones.
            let mut late = b.subscribe("c", Some("e1-1")).unwrap();
            assert_eq!(late.next().await, Item::Gap { missed: Some(1) });
            assert_eq!(data(late.next().await), "2");
        }
        let b = broadcaster(|c| c.max_bytes = 4);
        b.publish(Some("c"), msg("abc"), AckMode::Buffered).unwrap();
        b.publish(Some("c"), msg("de"), AckMode::Buffered).unwrap();
        assert_eq!((b.stats().events, b.stats().bytes), (1, 2));
        assert!(matches!(
            b.publish(Some("c"), msg("12345"), AckMode::Buffered),
            Err(Refused::TooLarge { .. })
        ));
    }

    #[tokio::test]
    async fn delivered_waits_for_a_subscriber_to_take_it() {
        let b = broadcaster(|_| {});
        let delivery = b.publish(Some("c"), msg("x"), AckMode::Delivered).unwrap();
        let waiting = tokio::spawn(delivery.done());
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!waiting.is_finished(), "nobody has it yet");
        // A subscriber resuming from before it takes it (from the ring).
        let mut sub = b.subscribe("c", Some("e1-0")).unwrap();
        let Item::Event(stored) = sub.next().await else {
            panic!()
        };
        sub.delivered(&stored);
        assert_eq!(waiting.await.unwrap(), Ok(()));
        assert_eq!(b.stats().pending, 0);
    }

    #[tokio::test]
    async fn a_live_subscriber_gets_the_events_waiting_for_one() {
        let b = broadcaster(|_| {});
        b.publish(Some("c"), msg("old"), AckMode::Buffered).unwrap();
        let waiting = tokio::spawn(
            b.publish(Some("c"), msg("held"), AckMode::Delivered)
                .unwrap()
                .done(),
        );
        let mut sub = b.subscribe("c", None).unwrap();
        let Item::Event(stored) = sub.next().await else {
            panic!()
        };
        assert_eq!(stored.message.data, b"held");
        sub.delivered(&stored);
        assert_eq!(waiting.await.unwrap(), Ok(()));
        // Taken: the next live subscriber starts after it.
        assert_eq!(b.subscribe("c", None).unwrap().try_next(), None);
    }

    #[tokio::test]
    async fn an_evicted_event_waiting_for_a_subscriber_fails() {
        let b = broadcaster(|c| c.max_events = 2);
        let first = b.publish(Some("c"), msg("1"), AckMode::Delivered).unwrap();
        b.publish(Some("c"), msg("2"), AckMode::Buffered).unwrap();
        b.publish(Some("c"), msg("3"), AckMode::Buffered).unwrap();
        assert_eq!(first.done().await, Err(Undelivered::Evicted));
        assert_eq!(b.stats().pending, 0);
    }

    #[tokio::test]
    async fn undelivered_events_leave_the_ring() {
        let b = broadcaster(|c| c.delivery_timeout = Some(Duration::from_millis(30)));
        assert_eq!(
            b.publish(Some("c"), msg("x"), AckMode::Delivered)
                .unwrap()
                .done()
                .await,
            Err(Undelivered::Timeout)
        );
        b.publish(Some("c"), msg("y"), AckMode::Buffered).unwrap();
        // Replay skips the withdrawn one (the hub sends it again).
        let mut sub = b.subscribe("c", Some("e1-0")).unwrap();
        assert_eq!(data(sub.next().await), "y");
        // A dropped delivery (lost hub connection) is withdrawn too.
        drop(b.publish(Some("c"), msg("z"), AckMode::Delivered).unwrap());
        b.publish(Some("c"), msg("w"), AckMode::Buffered).unwrap();
        assert_eq!(data(sub.next().await), "w");
        assert_eq!(b.stats().pending, 0);
    }

    #[tokio::test]
    async fn close_ends_subscribers_and_deliveries() {
        let b = broadcaster(|_| {});
        let mut sub = b.subscribe("c", None).unwrap();
        let delivery = b
            .publish(Some("other"), msg("x"), AckMode::Delivered)
            .unwrap();
        let waiting = tokio::spawn(async move { sub.next().await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        b.close();
        assert_eq!(waiting.await.unwrap(), Item::Closed);
        assert_eq!(delivery.done().await, Err(Undelivered::Closed));
    }

    #[tokio::test]
    async fn channels_are_checked_counted_and_cleaned_up() {
        let b = broadcaster(|c| {
            c.allowed_channels = vec!["devices/*".into(), "default".into()];
            c.max_channels = 2;
        });
        assert!(matches!(
            b.publish(Some("other"), msg("x"), AckMode::Buffered),
            Err(Refused::Channel(_))
        ));
        assert_eq!(b.channel_name(None).unwrap(), "default");
        let s1 = b.subscribe("devices/a", None).unwrap();
        let s2 = b.subscribe("devices/b", None).unwrap();
        assert!(matches!(
            b.subscribe("devices/c", None),
            Err(Refused::TooManyChannels)
        ));
        assert_eq!(s1.channel(), "devices/a");
        // Idle and empty: gone with its last subscriber, so there's room again.
        drop(s1);
        assert_eq!(b.stats().channels, 1);
        b.publish(Some("devices/c"), msg("x"), AckMode::Buffered)
            .unwrap();
        drop(s2);
        // A channel holding events stays (for replay).
        assert_eq!(b.stats().channels, 1);
        assert!(!b.has_subscribers("devices/c"));
    }

    #[tokio::test]
    async fn subscribers_are_limited_and_cursors_resume() {
        let b = broadcaster(|c| {
            c.max_subscribers = 3;
            c.max_subscribers_per_channel = 2;
        });
        let _a = b.subscribe("a", None).unwrap();
        let a2 = b.subscribe("a", None).unwrap();
        assert!(matches!(
            b.subscribe("a", None),
            Err(Refused::TooManySubscribers)
        ));
        let _b = b.subscribe("b", None).unwrap();
        assert!(matches!(
            b.subscribe("c", None),
            Err(Refused::TooManySubscribers)
        ));
        drop(a2);
        let mut c = b.subscribe("c", None).unwrap();
        assert_eq!(c.cursor_id(), "e1-0");
        b.publish(Some("c"), msg("x"), AckMode::Buffered).unwrap();
        b.publish(Some("c"), msg("y"), AckMode::Buffered).unwrap();
        assert_eq!(data(c.next().await), "x");
        let cursor = c.cursor_id();
        drop(c);
        let mut again = b.subscribe("c", Some(&cursor)).unwrap();
        assert_eq!(data(again.next().await), "y");
    }
}
