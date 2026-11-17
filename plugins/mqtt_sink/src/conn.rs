//! The broker connection: one task owns rumqttc's event loop and builds a new
//! one after every disconnect (with backoff), so nothing is buffered while
//! offline: the hub's queue is the buffer.
//!
//! A publish registers a waiter, in order, under the same lock as the
//! request it hands to rumqttc. rumqttc sends requests in order and reports
//! each as `Outgoing::Publish(pkid)`, so the oldest waiter is that publish:
//! QoS 0 is done there (written), QoS 1 waits for PUBACK and QoS 2 for
//! PUBCOMP by packet id. A disconnect fails every waiter, retryably.

use crate::config::{LastWill, ProtocolVersion, Settings};
use rumqttc::v5::mqttbytes::v5::{
    LastWill as LastWill5, Packet as Packet5, PubAckReason, PubRecReason, PublishProperties,
};
use std::collections::hash_map::RandomState;
use std::collections::{HashMap, VecDeque};
use std::hash::BuildHasher;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};
use tokio::sync::oneshot;

const MIN_DELAY: Duration = Duration::from_secs(1);
const MAX_DELAY: Duration = Duration::from_secs(30);
/// A connection that lived this long resets the backoff.
const STABLE: Duration = Duration::from_secs(10);
/// While not connected, a publish waits this long for the connection before
/// it fails (smooths the start and brief reconnects).
const CONNECT_GRACE: Duration = Duration::from_secs(1);

/// Why a publish didn't complete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub message: String,
    pub retryable: bool,
}

impl Failure {
    pub fn retry(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retryable: true,
        }
    }
    pub fn fatal(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retryable: false,
        }
    }
}

/// MQTT 5 properties of a message (ignored over 3.1.1).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Properties {
    pub content_type: Option<String>,
    pub response_topic: Option<String>,
    pub correlation_data: Option<Vec<u8>>,
    pub user_properties: Vec<(String, String)>,
    pub message_expiry_s: Option<u32>,
}

impl Properties {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub topic: String,
    pub payload: Vec<u8>,
    pub qos: u8,
    pub retain: bool,
    pub properties: Properties,
}

type Reply = oneshot::Sender<Result<(), Failure>>;

enum Client {
    V4(rumqttc::AsyncClient),
    V5(rumqttc::v5::AsyncClient),
}

enum EventLoop {
    V4(Box<rumqttc::EventLoop>),
    V5(Box<rumqttc::v5::EventLoop>),
}

/// What the run loop needs from an event, either version.
enum Seen {
    ConnAck,
    /// A publish went out (pkid 0 for QoS 0).
    Published(u16),
    /// The broker's last word on a QoS 1 / 2 publish.
    Done(u16, Result<(), Failure>),
    Disconnect(String),
    Other,
}

#[derive(Default)]
struct State {
    client: Option<Client>,
    connected: bool,
    /// Handed to rumqttc, not yet reported as sent: (qos, reply), in order.
    queued: VecDeque<(u8, Reply)>,
    /// Sent with QoS 1 / 2, by packet id.
    inflight: HashMap<u16, Reply>,
    last_error: Option<String>,
}

impl State {
    fn fail_all(&mut self, failure: &Failure) {
        for (_, reply) in self.queued.drain(..) {
            let _ = reply.send(Err(failure.clone()));
        }
        for (_, reply) in self.inflight.drain() {
            let _ = reply.send(Err(failure.clone()));
        }
    }
}

#[derive(Clone)]
pub struct Connection {
    state: Arc<Mutex<State>>,
    settings: Arc<Settings>,
    /// Whether connected, for publishes waiting on [`CONNECT_GRACE`].
    connected: tokio::sync::watch::Sender<bool>,
}

impl Connection {
    /// Starts connecting (and reconnecting) in the background.
    pub fn start(settings: Arc<Settings>) -> Self {
        let conn = Self {
            state: Arc::default(),
            settings,
            connected: tokio::sync::watch::Sender::new(false),
        };
        tokio::spawn(conn.clone().run());
        conn
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Publishes and waits for the broker (QoS 1 / 2) or the write (QoS 0).
    pub async fn publish(&self, message: Message) -> Result<(), Failure> {
        let mut connected = self.connected.subscribe();
        let _ = tokio::time::timeout(CONNECT_GRACE, connected.wait_for(|c| *c)).await;
        let receiver = {
            let mut state = self.lock();
            let client = match (&state.client, state.connected) {
                (Some(client), true) => client,
                _ => {
                    let why = state.last_error.as_deref().unwrap_or("connecting");
                    return Err(Failure::retry(format!(
                        "not connected to the broker ({why})"
                    )));
                }
            };
            let (reply, receiver) = oneshot::channel();
            let qos = message.qos;
            let sent = match client {
                Client::V4(c) => c
                    .try_publish(message.topic, qos4(qos), message.retain, message.payload)
                    .map_err(|e| e.to_string()),
                Client::V5(c) => c
                    .try_publish_with_properties(
                        message.topic,
                        qos5(qos),
                        message.retain,
                        message.payload,
                        properties5(message.properties),
                    )
                    .map_err(|e| e.to_string()),
            };
            if let Err(e) = sent {
                return Err(Failure::retry(format!(
                    "the MQTT client refused the message: {e}"
                )));
            }
            // Under the lock, so the run loop sees waiters in send order.
            state.queued.push_back((qos, reply));
            receiver
        };
        receiver
            .await
            .unwrap_or_else(|_| Err(Failure::retry("the connection to the broker was lost")))
    }

    /// A clean disconnect (the broker drops the Last Will).
    pub fn disconnect(&self) {
        let state = self.lock();
        match &state.client {
            Some(Client::V4(c)) => drop(c.try_disconnect()),
            Some(Client::V5(c)) => drop(c.try_disconnect()),
            None => {}
        }
    }

    async fn run(self) {
        let mut delay = MIN_DELAY;
        let mut warned_at: Option<Instant> = None;
        loop {
            let (client, mut event_loop) = build(&self.settings);
            self.lock().client = Some(client);
            let started = Instant::now();
            let reason = loop {
                let seen = match &mut event_loop {
                    EventLoop::V4(el) => el.poll().await.map(see4).map_err(|e| e.to_string()),
                    EventLoop::V5(el) => el.poll().await.map(see5).map_err(|e| e.to_string()),
                };
                match seen {
                    Ok(Seen::ConnAck) => {
                        let mut state = self.lock();
                        state.connected = true;
                        self.connected.send_replace(true);
                        state.last_error = None;
                        log::info!(
                            "connected to the broker at {}:{}",
                            self.settings.host,
                            self.settings.port
                        );
                        warned_at = None;
                    }
                    Ok(Seen::Published(pkid)) => {
                        let mut state = self.lock();
                        match state.queued.pop_front() {
                            Some((0, reply)) => drop(reply.send(Ok(()))),
                            Some((_, reply)) => drop(state.inflight.insert(pkid, reply)),
                            None => log::debug!("a publish without a waiter (pkid {pkid})"),
                        }
                    }
                    Ok(Seen::Done(pkid, result)) => {
                        if let Some(reply) = self.lock().inflight.remove(&pkid) {
                            let _ = reply.send(result);
                        }
                    }
                    Ok(Seen::Disconnect(why)) => break why,
                    Ok(Seen::Other) => {}
                    Err(e) => break e,
                }
            };
            let was_connected = {
                let mut state = self.lock();
                let was = state.connected;
                state.connected = false;
                self.connected.send_replace(false);
                state.client = None;
                state.fail_all(&Failure::retry(format!(
                    "the connection to the broker was lost: {reason}"
                )));
                state.last_error = Some(reason.clone());
                was
            };
            if started.elapsed() >= STABLE {
                delay = MIN_DELAY;
            }
            // WARN when a connection drops, then at most once a minute while it stays down.
            if was_connected || warned_at.is_none_or(|t| t.elapsed() >= Duration::from_secs(60)) {
                log::warn!(
                    "broker {}:{}: {reason}; retrying",
                    self.settings.host,
                    self.settings.port
                );
                warned_at = Some(Instant::now());
            } else {
                log::debug!(
                    "broker {}:{}: {reason}",
                    self.settings.host,
                    self.settings.port
                );
            }
            tokio::time::sleep(jitter(delay)).await;
            delay = (delay * 2).min(MAX_DELAY);
        }
    }
}

fn build(s: &Settings) -> (Client, EventLoop) {
    // Room for every publish the sink may have outstanding, plus control requests.
    let capacity = usize::from(s.max_inflight) * 2 + 8;
    let transport = s.tls.clone().map(|config| {
        rumqttc::Transport::tls_with_config(rumqttc::TlsConfiguration::Rustls(config))
    });
    match s.version {
        ProtocolVersion::V311 => {
            let mut o = rumqttc::MqttOptions::new(&s.client_id, &s.host, s.port);
            o.set_keep_alive(s.keepalive)
                .set_clean_session(s.clean_start)
                .set_inflight(s.max_inflight)
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
                .set_outgoing_inflight_upper_limit(s.max_inflight)
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

fn see4(event: rumqttc::Event) -> Seen {
    use rumqttc::{Event, Outgoing, Packet};
    match event {
        Event::Incoming(Packet::ConnAck(_)) => Seen::ConnAck,
        Event::Incoming(Packet::PubAck(ack)) => Seen::Done(ack.pkid, Ok(())),
        Event::Incoming(Packet::PubComp(comp)) => Seen::Done(comp.pkid, Ok(())),
        Event::Incoming(Packet::Disconnect) => Seen::Disconnect("the broker disconnected".into()),
        Event::Outgoing(Outgoing::Publish(pkid)) => Seen::Published(pkid),
        _ => Seen::Other,
    }
}

fn see5(event: rumqttc::v5::Event) -> Seen {
    use rumqttc::Outgoing;
    use rumqttc::v5::Event;
    match event {
        Event::Incoming(Packet5::ConnAck(_)) => Seen::ConnAck,
        Event::Incoming(Packet5::PubAck(ack)) => Seen::Done(ack.pkid, puback(&ack.reason)),
        // A failed PUBREC ends a QoS 2 exchange (no PUBCOMP follows).
        Event::Incoming(Packet5::PubRec(rec)) => match pubrec(&rec.reason) {
            Ok(()) => Seen::Other,
            Err(failure) => Seen::Done(rec.pkid, Err(failure)),
        },
        Event::Incoming(Packet5::PubComp(comp)) => Seen::Done(comp.pkid, Ok(())),
        Event::Incoming(Packet5::Disconnect(d)) => {
            Seen::Disconnect(format!("the broker disconnected ({:?})", d.reason_code))
        }
        Event::Outgoing(Outgoing::Publish(pkid)) => Seen::Published(pkid),
        _ => Seen::Other,
    }
}

/// MQTT 5 reason codes: a refusal that a config change must fix isn't retried.
fn puback(reason: &PubAckReason) -> Result<(), Failure> {
    match reason {
        PubAckReason::Success | PubAckReason::NoMatchingSubscribers => Ok(()),
        PubAckReason::NotAuthorized
        | PubAckReason::TopicNameInvalid
        | PubAckReason::PayloadFormatInvalid => Err(Failure::fatal(format!(
            "the broker refused the message: {reason:?}"
        ))),
        _ => Err(Failure::retry(format!(
            "the broker refused the message: {reason:?}"
        ))),
    }
}

fn pubrec(reason: &PubRecReason) -> Result<(), Failure> {
    match reason {
        PubRecReason::Success | PubRecReason::NoMatchingSubscribers => Ok(()),
        PubRecReason::NotAuthorized
        | PubRecReason::TopicNameInvalid
        | PubRecReason::PayloadFormatInvalid => Err(Failure::fatal(format!(
            "the broker refused the message: {reason:?}"
        ))),
        _ => Err(Failure::retry(format!(
            "the broker refused the message: {reason:?}"
        ))),
    }
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

fn properties5(p: Properties) -> PublishProperties {
    PublishProperties {
        content_type: p.content_type,
        response_topic: p.response_topic,
        correlation_data: p.correlation_data.map(Into::into),
        user_properties: p.user_properties,
        message_expiry_interval: p.message_expiry_s,
        ..Default::default()
    }
}

/// Spreads `delay` by up to ±10 %, so many plugins don't retry in lockstep.
fn jitter(delay: Duration) -> Duration {
    let random = RandomState::new().hash_one(std::time::SystemTime::now());
    delay.mul_f64(0.9 + (random % 2001) as f64 / 10_000.0)
}
