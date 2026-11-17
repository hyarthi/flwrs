//! The broker connection and the inbox.
//!
//! One task owns rumqttc's event loop (manual acks) and builds a new one
//! after every disconnect, with backoff. It subscribes on every CONNACK and
//! puts each message into the [`Inbox`] as an [`Item`]; the source's `run`
//! emits them and acknowledges them to the broker through [`Connection::ack`].
//!
//! Flow control: QoS 1 / 2 messages are acknowledged only once they're done,
//! so the broker never has more of them outstanding than its in-flight
//! window (MQTT 5: our Receive Maximum, `max_inflight`): that bounds the
//! inbox. QoS 0 messages have no such window, so they're capped
//! (`max_qos0_buffered`) and dropped beyond it. The event loop never waits on
//! the inbox, so keepalive pings go on while the hub is slow.

use crate::config::{LastWill, ProtocolVersion, RetainHandling, Settings, Subscription};
use flwrs_plugin::schema::payload::PluginPayload;
use flwrs_types::value::Value;
use rumqttc::v5::mqttbytes::v5::{
    Filter, LastWill as LastWill5, Packet as Packet5, Publish as Publish5, RetainForwardRule,
    SubscribeReasonCode as SubCode5,
};
use std::collections::hash_map::RandomState;
use std::collections::{BTreeMap, VecDeque};
use std::hash::BuildHasher;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::Notify;

const MIN_DELAY: Duration = Duration::from_secs(1);
const MAX_DELAY: Duration = Duration::from_secs(30);
/// A connection that lived this long resets the backoff.
const STABLE: Duration = Duration::from_secs(10);

/// What acknowledges a message to the broker: the PUBLISH without its
/// payload, and the connection it came on (an ack can't go out on another).
#[derive(Debug, Clone)]
enum AckToken {
    V4(u64, rumqttc::Publish),
    V5(u64, Publish5),
}

/// A received message, as the event it becomes.
#[derive(Debug, Clone)]
pub struct Item {
    pub payload: PluginPayload,
    pub qos: u8,
    token: Option<AckToken>,
}

#[derive(Default)]
struct InboxState {
    /// Received, not yet emitted.
    queue: VecDeque<Item>,
    /// Emitted, awaiting the hub's confirmation, in emit order.
    unconfirmed: VecDeque<Item>,
    /// QoS 0 items in `queue`.
    qos0: usize,
    dropped_qos0: u64,
    last_drop_warning: Option<Instant>,
}

/// Messages between the connection and the source's `run`.
#[derive(Default)]
pub struct Inbox {
    state: Mutex<InboxState>,
    arrived: Notify,
}

impl Inbox {
    fn lock(&self) -> MutexGuard<'_, InboxState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn push(&self, item: Item, max_qos0: usize) {
        let mut s = self.lock();
        if item.qos == 0 {
            if s.qos0 >= max_qos0 {
                s.dropped_qos0 += 1;
                if s.last_drop_warning
                    .is_none_or(|t| t.elapsed() >= Duration::from_secs(60))
                {
                    log::warn!(
                        "QoS 0 messages dropped: {} so far (the hub isn't taking events and max_qos0_buffered = {max_qos0} are waiting)",
                        s.dropped_qos0
                    );
                    s.last_drop_warning = Some(Instant::now());
                }
                return;
            }
            s.qos0 += 1;
        }
        s.queue.push_back(item);
        drop(s);
        self.arrived.notify_one();
    }

    /// The next message's event, waiting for one. It stays first in the
    /// queue until [`Self::emitted`]: a `run` cancelled meanwhile loses nothing.
    pub async fn next(&self) -> PluginPayload {
        loop {
            if let Some(item) = self.lock().queue.front() {
                return item.payload.clone();
            }
            self.arrived.notified().await;
        }
    }

    fn pop_queue(s: &mut InboxState) -> Option<Item> {
        let item = s.queue.pop_front()?;
        if item.qos == 0 {
            s.qos0 -= 1;
        }
        Some(item)
    }

    /// The first message was emitted: it now awaits its confirmation.
    pub fn emitted(&self) {
        let mut s = self.lock();
        if let Some(item) = Self::pop_queue(&mut s) {
            s.unconfirmed.push_back(item);
        }
    }

    /// The first message was emitted and needs no confirmation (`on_emit`).
    pub fn take(&self) -> Option<Item> {
        Self::pop_queue(&mut self.lock())
    }

    /// The oldest emitted message was confirmed (or finally refused).
    pub fn confirmed(&self) -> Option<Item> {
        self.lock().unconfirmed.pop_front()
    }

    /// Messages emitted but never confirmed (the hub connection dropped)
    /// go back to the front of the queue, in order, to be emitted again.
    pub fn requeue_unconfirmed(&self) {
        let mut s = self.lock();
        while let Some(item) = s.unconfirmed.pop_back() {
            if item.qos == 0 {
                s.qos0 += 1;
            }
            s.queue.push_front(item);
        }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        let s = self.lock();
        s.queue.len() + s.unconfirmed.len()
    }
}

#[derive(Clone)]
enum Client {
    V4(rumqttc::AsyncClient),
    V5(rumqttc::v5::AsyncClient),
}

enum EventLoop {
    V4(Box<rumqttc::EventLoop>),
    V5(Box<rumqttc::v5::EventLoop>),
}

#[derive(Default)]
struct State {
    client: Option<Client>,
    /// Counts connections; an ack token is valid for its own only.
    generation: u64,
}

#[derive(Clone)]
pub struct Connection {
    state: Arc<Mutex<State>>,
    pub inbox: Arc<Inbox>,
    settings: Arc<Settings>,
}

impl Connection {
    /// Starts connecting (and reconnecting) in the background.
    pub fn start(settings: Arc<Settings>) -> Self {
        let conn = Self {
            state: Arc::default(),
            inbox: Arc::default(),
            settings,
        };
        tokio::spawn(conn.clone().run());
        conn
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Acknowledges a message to the broker (nothing for QoS 0). A message
    /// from an earlier connection can't be: with a persistent session the
    /// broker sends it again (a duplicate), otherwise it's done.
    pub async fn ack(&self, item: &Item) {
        let client = {
            let state = self.lock();
            let current = state.generation;
            match (&item.token, &state.client) {
                (Some(AckToken::V4(g, _) | AckToken::V5(g, _)), Some(client)) if *g == current => {
                    client.clone()
                }
                _ => return,
            }
        };
        let result = match (&client, &item.token) {
            (Client::V4(c), Some(AckToken::V4(_, p))) => c.ack(p).await.map_err(|e| e.to_string()),
            (Client::V5(c), Some(AckToken::V5(_, p))) => c.ack(p).await.map_err(|e| e.to_string()),
            _ => Ok(()),
        };
        if let Err(e) = result {
            log::debug!("an ack didn't reach the client ({e}); the broker sends the message again");
        }
    }

    /// A clean disconnect (the broker drops the Last Will).
    pub fn disconnect(&self) {
        match &self.lock().client {
            Some(Client::V4(c)) => drop(c.try_disconnect()),
            Some(Client::V5(c)) => drop(c.try_disconnect()),
            None => {}
        }
    }

    async fn run(self) {
        let mut delay = MIN_DELAY;
        let mut warned_at: Option<Instant> = None;
        let (host, port) = (self.settings.host.clone(), self.settings.port);
        loop {
            let (client, mut event_loop) = build(&self.settings);
            let generation = {
                let mut state = self.lock();
                state.generation += 1;
                state.client = Some(client.clone());
                state.generation
            };
            let started = Instant::now();
            let mut connected = false;
            let reason = loop {
                let polled = match &mut event_loop {
                    EventLoop::V4(el) => el.poll().await.map(Polled::V4).map_err(|e| e.to_string()),
                    EventLoop::V5(el) => el.poll().await.map(Polled::V5).map_err(|e| e.to_string()),
                };
                match polled {
                    Ok(Polled::V4(rumqttc::Event::Incoming(packet))) => match packet {
                        rumqttc::Packet::ConnAck(ack) => {
                            connected = true;
                            warned_at = None;
                            self.connected(&client, ack.session_present);
                        }
                        rumqttc::Packet::SubAck(ack) => {
                            let failed = ack.return_codes.iter().map(|c| {
                                matches!(c, rumqttc::SubscribeReasonCode::Failure)
                                    .then(|| "refused".to_string())
                            });
                            self.check_subscriptions(failed);
                        }
                        rumqttc::Packet::Publish(p) => self.received4(p, generation),
                        rumqttc::Packet::Disconnect => break "the broker disconnected".to_string(),
                        _ => {}
                    },
                    Ok(Polled::V5(rumqttc::v5::Event::Incoming(packet))) => match packet {
                        Packet5::ConnAck(ack) => {
                            connected = true;
                            warned_at = None;
                            self.connected(&client, ack.session_present);
                        }
                        Packet5::SubAck(ack) => {
                            let failed = ack.return_codes.iter().map(|c| match c {
                                SubCode5::Success(_) => None,
                                other => Some(format!("{other:?}")),
                            });
                            self.check_subscriptions(failed);
                        }
                        Packet5::Publish(p) => self.received5(p, generation),
                        Packet5::Disconnect(d) => {
                            break format!("the broker disconnected ({:?})", d.reason_code);
                        }
                        _ => {}
                    },
                    Ok(_) => {}
                    Err(e) => break e,
                }
            };
            self.lock().client = None;
            if started.elapsed() >= STABLE {
                delay = MIN_DELAY;
            }
            // WARN when a connection drops, then at most once a minute while it stays down.
            if connected || warned_at.is_none_or(|t| t.elapsed() >= Duration::from_secs(60)) {
                log::warn!("broker {host}:{port}: {reason}; retrying");
                warned_at = Some(Instant::now());
            } else {
                log::debug!("broker {host}:{port}: {reason}");
            }
            tokio::time::sleep(jitter(delay)).await;
            delay = (delay * 2).min(MAX_DELAY);
        }
    }

    /// Subscribes (again: harmless when the broker kept the session).
    fn connected(&self, client: &Client, session_present: bool) {
        let s = &self.settings;
        log::info!(
            "connected to the broker at {}:{} ({})",
            s.host,
            s.port,
            if session_present {
                "session resumed"
            } else {
                "new session"
            }
        );
        let result =
            match client {
                Client::V4(c) => c
                    .try_subscribe_many(s.subscriptions.iter().map(|sub| {
                        rumqttc::SubscribeFilter::new(sub.filter.clone(), qos4(sub.qos))
                    }))
                    .map_err(|e| e.to_string()),
                Client::V5(c) => c
                    .try_subscribe_many(s.subscriptions.iter().map(filter5))
                    .map_err(|e| e.to_string()),
            };
        if let Err(e) = result {
            log::error!("subscribing failed: {e}");
        }
    }

    fn check_subscriptions(&self, results: impl Iterator<Item = Option<String>>) {
        for (sub, failure) in self.settings.subscriptions.iter().zip(results) {
            if let Some(why) = failure {
                log::error!(
                    "the broker refused the subscription {:?}: {why}",
                    sub.filter
                );
            }
        }
    }

    fn received4(&self, mut p: rumqttc::Publish, generation: u64) {
        let fields = Fields {
            topic: std::mem::take(&mut p.topic),
            payload: p.payload.to_vec(),
            qos: p.qos as u8,
            retain: p.retain,
            dup: p.dup,
            ..Default::default()
        };
        p.payload = Default::default();
        let qos = fields.qos;
        let token = (qos > 0).then(|| AckToken::V4(generation, p));
        self.inbox.push(
            Item {
                payload: fields.into_payload(),
                qos,
                token,
            },
            self.settings.max_qos0_buffered,
        );
    }

    fn received5(&self, mut p: Publish5, generation: u64) {
        let props = p.properties.take().unwrap_or_default();
        let mut user_properties = BTreeMap::<String, Vec<String>>::new();
        for (name, value) in props.user_properties {
            user_properties.entry(name).or_default().push(value);
        }
        let fields = Fields {
            topic: String::from_utf8_lossy(&p.topic).into_owned(),
            payload: p.payload.to_vec(),
            qos: p.qos as u8,
            retain: p.retain,
            dup: p.dup,
            content_type: props.content_type,
            response_topic: props.response_topic,
            correlation_data: props.correlation_data.map(|d| d.to_vec()),
            user_properties,
            message_expiry_s: props.message_expiry_interval,
        };
        p.payload = Default::default();
        p.topic = Default::default();
        let qos = fields.qos;
        let token = (qos > 0).then(|| AckToken::V5(generation, p));
        self.inbox.push(
            Item {
                payload: fields.into_payload(),
                qos,
                token,
            },
            self.settings.max_qos0_buffered,
        );
    }
}

// One per poll, on the stack: boxing would allocate for every message.
#[allow(clippy::large_enum_variant)]
enum Polled {
    V4(rumqttc::Event),
    V5(rumqttc::v5::Event),
}

/// A message's fields, in the out schema's order.
#[derive(Default)]
struct Fields {
    topic: String,
    payload: Vec<u8>,
    qos: u8,
    retain: bool,
    dup: bool,
    content_type: Option<String>,
    response_topic: Option<String>,
    correlation_data: Option<Vec<u8>>,
    user_properties: BTreeMap<String, Vec<String>>,
    message_expiry_s: Option<u32>,
}

impl Fields {
    fn into_payload(self) -> PluginPayload {
        let opt = |v: Option<Value>| v.unwrap_or(Value::Null);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let user_properties = (!self.user_properties.is_empty()).then(|| {
            Value::Map(
                self.user_properties
                    .into_iter()
                    .map(|(k, vs)| (k, Value::Array(vs.into_iter().map(Value::String).collect())))
                    .collect(),
            )
        });
        Value::Object(vec![
            ("topic".into(), Value::String(self.topic)),
            ("payload".into(), Value::Bytes(self.payload)),
            ("qos".into(), Value::U32(self.qos.into())),
            ("retain".into(), Value::Bool(self.retain)),
            ("dup".into(), Value::Bool(self.dup)),
            (
                "content_type".into(),
                opt(self.content_type.map(Value::String)),
            ),
            (
                "response_topic".into(),
                opt(self.response_topic.map(Value::String)),
            ),
            (
                "correlation_data".into(),
                opt(self.correlation_data.map(Value::Bytes)),
            ),
            ("user_properties".into(), opt(user_properties)),
            (
                "message_expiry_s".into(),
                opt(self.message_expiry_s.map(Value::U32)),
            ),
            (
                "receive_time".into(),
                Value::DateTime(pbjson_types::Timestamp {
                    seconds: now.as_secs() as i64,
                    nanos: now.subsec_nanos() as i32,
                }),
            ),
        ])
        .into_payload()
        .expect("a message's fields convert")
    }
}

fn build(s: &Settings) -> (Client, EventLoop) {
    // Acks for every message the broker may have outstanding, plus control requests.
    let capacity = usize::from(s.max_inflight) * 2 + 16;
    let transport = s.tls.clone().map(|config| {
        rumqttc::Transport::tls_with_config(rumqttc::TlsConfiguration::Rustls(config))
    });
    match s.version {
        ProtocolVersion::V311 => {
            let mut o = rumqttc::MqttOptions::new(&s.client_id, &s.host, s.port);
            o.set_keep_alive(s.keepalive)
                .set_clean_session(s.clean_start)
                .set_manual_acks(true)
                .set_max_packet_size(s.max_packet_bytes as usize, s.max_packet_bytes as usize)
                .set_request_channel_capacity(capacity);
            if let Some((user, password)) = &s.credentials {
                o.set_credentials(user, password);
            }
            if let Some(transport) = transport {
                o.set_transport(transport);
            }
            if let Some(LastWill {
                topic,
                payload,
                qos,
                retain,
            }) = &s.last_will
            {
                o.set_last_will(rumqttc::LastWill::new(
                    topic,
                    payload.as_bytes().to_vec(),
                    qos4(*qos),
                    *retain,
                ));
            }
            let (client, el) = rumqttc::AsyncClient::new(o, capacity);
            (Client::V4(client), EventLoop::V4(Box::new(el)))
        }
        ProtocolVersion::V5 => {
            let mut o = rumqttc::v5::MqttOptions::new(&s.client_id, &s.host, s.port);
            o.set_keep_alive(s.keepalive)
                .set_clean_start(s.clean_start)
                .set_manual_acks(true)
                .set_receive_maximum(Some(s.max_inflight))
                .set_max_packet_size(Some(s.max_packet_bytes))
                .set_session_expiry_interval(s.session_expiry_s)
                .set_request_channel_capacity(capacity);
            if let Some((user, password)) = &s.credentials {
                o.set_credentials(user, password);
            }
            if let Some(transport) = transport {
                o.set_transport(transport);
            }
            if let Some(LastWill {
                topic,
                payload,
                qos,
                retain,
            }) = &s.last_will
            {
                o.set_last_will(LastWill5::new(
                    topic,
                    payload.as_bytes().to_vec(),
                    qos5(*qos),
                    *retain,
                    None,
                ));
            }
            let (client, el) = rumqttc::v5::AsyncClient::new(o, capacity);
            (Client::V5(client), EventLoop::V5(Box::new(el)))
        }
    }
}

fn filter5(sub: &Subscription) -> Filter {
    let mut filter = Filter::new(sub.filter.clone(), qos5(sub.qos));
    filter.nolocal = sub.no_local;
    filter.preserve_retain = sub.retain_as_published;
    filter.retain_forward_rule = match sub.retain_handling {
        RetainHandling::Send => RetainForwardRule::OnEverySubscribe,
        RetainHandling::NewOnly => RetainForwardRule::OnNewSubscribe,
        RetainHandling::Never => RetainForwardRule::Never,
    };
    filter
}

fn qos4(qos: u8) -> rumqttc::QoS {
    match qos {
        0 => rumqttc::QoS::AtMostOnce,
        1 => rumqttc::QoS::AtLeastOnce,
        _ => rumqttc::QoS::ExactlyOnce,
    }
}

fn qos5(qos: u8) -> rumqttc::v5::mqttbytes::QoS {
    use rumqttc::v5::mqttbytes::QoS;
    match qos {
        0 => QoS::AtMostOnce,
        1 => QoS::AtLeastOnce,
        _ => QoS::ExactlyOnce,
    }
}

/// Spreads `delay` by up to ±10 %, so many plugins don't retry in lockstep.
fn jitter(delay: Duration) -> Duration {
    let random = RandomState::new().hash_one(SystemTime::now());
    delay.mul_f64(0.9 + (random % 2001) as f64 / 10_000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(n: u8, qos: u8) -> Item {
        let payload = Value::Object(vec![("n".into(), Value::U32(n.into()))])
            .into_payload()
            .unwrap();
        Item {
            payload,
            qos,
            token: None,
        }
    }

    #[tokio::test]
    async fn the_inbox_keeps_messages_until_confirmed() {
        let inbox = Inbox::default();
        for n in 1..=3 {
            inbox.push(item(n, 1), 10);
        }
        // Peeking twice gives the same message: a cancelled emit loses nothing.
        assert_eq!(inbox.next().await, inbox.next().await);
        inbox.emitted();
        inbox.emitted();
        assert_eq!(inbox.len(), 3);
        // The hub connection dropped: both go back in front, in order.
        inbox.requeue_unconfirmed();
        assert_eq!(inbox.next().await, item(1, 1).payload);
        inbox.emitted();
        assert_eq!(inbox.confirmed().unwrap().payload, item(1, 1).payload);
        assert_eq!(inbox.take().unwrap().payload, item(2, 1).payload);
        assert_eq!(inbox.len(), 1);
    }

    #[test]
    fn qos0_is_capped() {
        let inbox = Inbox::default();
        for n in 0..5 {
            inbox.push(item(n, 0), 2);
        }
        inbox.push(item(9, 1), 2);
        assert_eq!(inbox.len(), 3);
        assert_eq!(inbox.lock().dropped_qos0, 3);
        inbox.take();
        inbox.push(item(7, 0), 2);
        assert_eq!(inbox.len(), 3);
    }
}
