//! What leaves the plugin: rule events, and optionally the frames' objects
//! (`emit` node: `rules`, `changes`, `frames`), as payloads in the default
//! schema's shape. Boxes and keypoints are normalised to the frame.
//!
//! The [`Outbox`] sits between the frame loop and the hub. Rule events wait
//! in a bounded queue and go first; frame results replace each other, so a
//! slow workflow gets the newest. The frame loop never waits for the hub.

use crate::analytics::rules::RuleEvent;
use crate::graph::config::{EmitConfig, EmitMode, GraphConfig, NodeConfig};
use crate::graph::exec::Tick;
use crate::results::{Keypoint, Object, Results};
use flwrs_types::value::Value;
use pbjson_types::Timestamp;
use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Notify;

/// Rule events waiting for the hub before the oldest are dropped.
pub const MAX_QUEUED_EVENTS: usize = 256;

/// The graph's `emit` settings (its node's, or the default: rule events).
pub fn config_of(graph: &GraphConfig) -> EmitConfig {
    graph
        .nodes
        .values()
        .find_map(|n| match n {
            NodeConfig::Emit(e) => Some(e.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

/// The package's default out schema (`schema/out.json`): every field any
/// graph can send.
pub const DEFAULT_OUT_SCHEMA: &str = include_str!("../schema/out.json");

/// Top-level fields only a rule event fills in.
const RULE_FIELDS: [&str; 8] = [
    "rule",
    "action",
    "tracker_id",
    "label",
    "previous",
    "score",
    "count",
    "duration_s",
];

/// The payload schema of what this graph sends: the default one without the
/// fields it can never fill in.
pub fn out_schema(graph: &GraphConfig) -> serde_json::Value {
    let mut schema: serde_json::Value =
        serde_json::from_str(DEFAULT_OUT_SCHEMA).expect("the package's schema is JSON");
    let has = |f: fn(&NodeConfig) -> bool| graph.nodes.values().any(f);
    let rules = has(|n| matches!(n, NodeConfig::Rule(_) | NodeConfig::Lua(_)));
    let lua = has(|n| matches!(n, NodeConfig::Lua(_)));
    let keypoints = config_of(graph).keypoints;
    let key = |f: &serde_json::Value| f["key"].as_str().unwrap_or_default().to_string();
    if let Some(fields) = schema["fields"].as_array_mut() {
        fields.retain(|f| {
            let k = key(f);
            (rules || !RULE_FIELDS.contains(&k.as_str())) && (lua || k != "data")
        });
        if !keypoints
            && let Some(object) = fields.iter_mut().find(|f| key(f) == "objects")
            && let Some(inner) = object["nested_type_definition"]["object_fields"].as_array_mut()
        {
            inner.retain(|f| key(f) != "keypoints");
        }
    }
    schema
}

/// The frame an emission is about.
#[derive(Clone, Copy, Debug)]
pub struct FrameInfo {
    /// From 1.
    pub seq: u64,
    pub capture_time: SystemTime,
    pub width: u32,
    pub height: u32,
}

/// A payload to send, as a value.
#[derive(Clone, Debug, PartialEq)]
pub enum Emission {
    /// A rule's event: it must get through.
    Event(Value),
    /// A frame's objects: only the newest matters.
    Frame(Value),
}

fn f(v: f32) -> Value {
    Value::F32(v)
}

fn text(v: Option<&str>) -> Value {
    v.map_or(Value::Null, |s| Value::String(s.to_string()))
}

fn timestamp(t: SystemTime) -> Value {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    Value::DateTime(Timestamp {
        seconds: d.as_secs() as i64,
        nanos: d.subsec_nanos() as i32,
    })
}

fn object_fields(fields: Vec<(&str, Value)>) -> Value {
    Value::Object(
        fields
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
    )
}

/// An object's keypoints: its own, or those a model found on its crop.
fn keypoints_of(o: &Object) -> &[Keypoint] {
    if !o.keypoints.is_empty() {
        return &o.keypoints;
    }
    o.children
        .iter()
        .find_map(|(_, r)| match r {
            Results::Objects(list) => list
                .iter()
                .find(|c| !c.keypoints.is_empty())
                .map(|c| c.keypoints.as_slice()),
            _ => None,
        })
        .unwrap_or(&[])
}

fn object_value(source: &str, o: &Object, info: &FrameInfo, keypoints: bool) -> Value {
    let (w, h) = (info.width as f32, info.height as f32);
    let unit = |v: f32, size: f32| (v / size).clamp(0.0, 1.0);
    let points = if keypoints { keypoints_of(o) } else { &[] };
    let classes = o
        .children
        .iter()
        .filter_map(|(node, r)| match r {
            Results::Classes(c) => Some((node, c)),
            _ => None,
        })
        .flat_map(|(node, list)| {
            list.iter().map(move |c| {
                object_fields(vec![
                    ("source", Value::String(node.clone())),
                    ("class_id", Value::U32(c.class_id)),
                    (
                        "class_name",
                        Value::String(c.class_name.clone().unwrap_or_default()),
                    ),
                    ("confidence", f(c.confidence)),
                ])
            })
        })
        .collect();
    object_fields(vec![
        ("class_id", Value::U32(o.class_id)),
        (
            "class_name",
            Value::String(o.class_name.clone().unwrap_or_default()),
        ),
        ("confidence", f(o.confidence)),
        ("tracker_id", Value::U64(o.tracker_id.unwrap_or(0))),
        ("source", Value::String(source.to_string())),
        (
            "box",
            object_fields(vec![
                ("x1", f(unit(o.bbox[0], w))),
                ("y1", f(unit(o.bbox[1], h))),
                ("x2", f(unit(o.bbox[2], w))),
                ("y2", f(unit(o.bbox[3], h))),
            ]),
        ),
        (
            "keypoints",
            Value::Array(
                points
                    .iter()
                    .map(|k| {
                        object_fields(vec![
                            ("x", f(unit(k.x, w))),
                            ("y", f(unit(k.y, h))),
                            ("confidence", f(k.confidence)),
                        ])
                    })
                    .collect(),
            ),
        ),
        ("classes", Value::Array(classes)),
    ])
}

/// The most confident `max` objects, in their order.
fn capped(objects: &[Object], max: usize) -> Vec<&Object> {
    if objects.len() <= max {
        return objects.iter().collect();
    }
    let mut order: Vec<usize> = (0..objects.len()).collect();
    order.sort_by(|&a, &b| objects[b].confidence.total_cmp(&objects[a].confidence));
    order.truncate(max);
    order.sort_unstable();
    order.into_iter().map(|i| &objects[i]).collect()
}

fn payload(info: &FrameInfo, event: &str, objects: Vec<Value>, e: Option<&RuleEvent>) -> Value {
    let data = e
        .and_then(|e| e.data.as_ref())
        .map_or(Value::Null, |d| Value::String(d.to_string()));
    object_fields(vec![
        ("event", Value::String(event.to_string())),
        ("frame_time", timestamp(info.capture_time)),
        ("frame_seq", Value::U64(info.seq)),
        ("width", Value::U32(info.width)),
        ("height", Value::U32(info.height)),
        ("objects", Value::Array(objects)),
        ("rule", text(e.map(|e| e.rule.as_str()))),
        ("action", text(e.map(|e| e.action.as_str()))),
        (
            "tracker_id",
            e.and_then(|e| e.tracker_id).map_or(Value::Null, Value::U64),
        ),
        ("label", text(e.and_then(|e| e.label.as_deref()))),
        ("previous", text(e.and_then(|e| e.previous.as_deref()))),
        ("score", e.and_then(|e| e.score).map_or(Value::Null, f)),
        (
            "count",
            e.and_then(|e| e.count).map_or(Value::Null, Value::U32),
        ),
        (
            "duration_s",
            e.and_then(|e| e.duration_s).map_or(Value::Null, f),
        ),
        ("data", data),
    ])
}

/// What counts as a change for `changes`: who is there and what they're
/// labelled, not where exactly or how sure.
fn signature(objects: &[Object]) -> String {
    let mut parts: Vec<String> = objects
        .iter()
        .map(|o| {
            let labels: Vec<String> = o
                .children
                .iter()
                .filter_map(|(node, r)| match r {
                    Results::Classes(c) => c.first().map(|c| format!("{node}={}", c.class_id)),
                    Results::Text(Some(t)) => Some(format!("{node}={}", t.text)),
                    _ => None,
                })
                .collect();
            format!("{:?}:{}:{}", o.tracker_id, o.class_id, labels.join(","))
        })
        .collect();
    parts.sort();
    parts.join("|")
}

/// Decides what each frame sends.
pub struct Emitter {
    config: EmitConfig,
    last_signature: Option<String>,
    last_frame: Option<f64>,
}

impl Emitter {
    pub fn new(config: EmitConfig) -> Self {
        Self {
            config,
            last_signature: None,
            last_frame: None,
        }
    }

    /// The object an event is about, wherever the graph reports it.
    fn about<'a>(tick: &'a Tick, e: &RuleEvent) -> Option<(&'a str, &'a Object)> {
        let id = e.tracker_id?;
        tick.results.iter().find_map(|(node, r)| match r {
            Results::Objects(list) => list
                .iter()
                .find(|o| o.tracker_id == Some(id))
                .map(|o| (node.as_str(), o)),
            _ => None,
        })
    }

    /// What this frame sends, at `time` on the stream's clock.
    pub fn on_tick(&mut self, tick: &Tick, info: &FrameInfo, time: f64) -> Vec<Emission> {
        let keypoints = self.config.keypoints;
        let mut out: Vec<Emission> = tick
            .events
            .iter()
            .map(|e| {
                let objects = Self::about(tick, e)
                    .map(|(node, o)| object_value(node, o, info, keypoints))
                    .into_iter()
                    .collect();
                Emission::Event(payload(info, &e.event, objects, Some(e)))
            })
            .collect();
        if self.config.mode == EmitMode::Rules {
            return out;
        }
        let Some((source, objects)) = &tick.emit_objects else {
            return out;
        };
        let interval = f64::from(self.config.min_interval_ms) / 1000.0;
        if self.last_frame.is_some_and(|t| time - t < interval) {
            return out;
        }
        if self.config.mode == EmitMode::Changes {
            let now = signature(objects);
            if self.last_signature.as_ref() == Some(&now) {
                return out;
            }
            self.last_signature = Some(now);
        }
        self.last_frame = Some(time);
        let list = capped(objects, self.config.max_objects)
            .into_iter()
            .map(|o| object_value(source, o, info, keypoints))
            .collect();
        out.push(Emission::Frame(payload(info, "detections", list, None)));
        out
    }
}

/// How the outbox has fared.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OutboxStats {
    pub events_sent: u64,
    pub frames_sent: u64,
    /// Rule events dropped because the hub didn't take them in time.
    pub events_dropped: u64,
    /// Frame results replaced by newer ones before they could go.
    pub frames_replaced: u64,
}

struct Inner<T> {
    events: VecDeque<T>,
    frame: Option<T>,
    stats: OutboxStats,
}

/// What to send next.
#[derive(Clone, Debug, PartialEq)]
pub enum Outgoing<T> {
    /// Stays first in the queue until [`Outbox::event_done`].
    Event(T),
    Frame(T),
}

/// Between the frame loop (which never waits) and the hub.
pub struct Outbox<T> {
    inner: Mutex<Inner<T>>,
    changed: Notify,
    capacity: usize,
}

impl<T: Clone> Outbox<T> {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                events: VecDeque::new(),
                frame: None,
                stats: OutboxStats::default(),
            }),
            changed: Notify::new(),
            capacity: capacity.max(2),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner<T>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Queues a rule event; when full, the oldest not being sent is dropped.
    pub fn push_event(&self, item: T) {
        {
            let mut g = self.lock();
            if g.events.len() >= self.capacity {
                // The first may be in flight: it goes when it's confirmed.
                g.events.remove(1);
                g.stats.events_dropped += 1;
            }
            g.events.push_back(item);
        }
        self.changed.notify_one();
    }

    /// Sets the newest frame result.
    pub fn push_frame(&self, item: T) {
        {
            let mut g = self.lock();
            if g.frame.replace(item).is_some() {
                g.stats.frames_replaced += 1;
            }
        }
        self.changed.notify_one();
    }

    /// The next thing to send, waiting for one. Cancel-safe: an event stays
    /// queued until [`Self::event_done`]; rule events go before frames.
    pub async fn next(&self) -> Outgoing<T> {
        loop {
            let notified = self.changed.notified();
            {
                let mut g = self.lock();
                if let Some(e) = g.events.front() {
                    return Outgoing::Event(e.clone());
                }
                if let Some(frame) = g.frame.take() {
                    g.stats.frames_sent += 1;
                    return Outgoing::Frame(frame);
                }
            }
            notified.await;
        }
    }

    /// The event [`Self::next`] gave went through (or was refused for good).
    pub fn event_done(&self) {
        let mut g = self.lock();
        if g.events.pop_front().is_some() {
            g.stats.events_sent += 1;
        }
    }

    pub fn stats(&self) -> OutboxStats {
        self.lock().stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::results::{Class, Text};
    use flwrs_plugin::schema::rules;
    use flwrs_types::value::coerce_payload;
    use std::time::Duration;

    const INFO: FrameInfo = FrameInfo {
        seq: 7,
        capture_time: UNIX_EPOCH,
        width: 200,
        height: 100,
    };

    fn person(id: Option<u64>, confidence: f32) -> Object {
        Object {
            tracker_id: id,
            class_id: 0,
            class_name: Some("person".into()),
            confidence,
            bbox: [20.0, 10.0, 100.0, 90.0],
            keypoints: vec![],
            children: vec![
                (
                    "pose".into(),
                    Results::Objects(vec![Object {
                        tracker_id: None,
                        class_id: 0,
                        class_name: None,
                        confidence: 0.8,
                        bbox: [0.0; 4],
                        keypoints: vec![Keypoint {
                            x: 50.0,
                            y: 25.0,
                            confidence: 0.9,
                        }],
                        children: vec![],
                    }]),
                ),
                (
                    "gesture".into(),
                    Results::Classes(vec![Class {
                        class_id: 2,
                        class_name: Some("wave".into()),
                        confidence: 0.7,
                    }]),
                ),
                (
                    "plate".into(),
                    Results::Text(Some(Text {
                        text: "A1".into(),
                        confidence: 0.5,
                    })),
                ),
            ],
        }
    }

    fn event() -> RuleEvent {
        RuleEvent {
            node: "door".into(),
            rule: "door".into(),
            event: "line_crossing".into(),
            action: "in".into(),
            time: 1.0,
            tracker_id: Some(3),
            class_name: Some("person".into()),
            bbox: None,
            count: Some(2),
            duration_s: Some(1.5),
            score: Some(0.5),
            label: Some("x".into()),
            previous: Some("y".into()),
            data: Some(serde_json::json!({"a": 1})),
        }
    }

    fn get<'a>(v: &'a Value, key: &str) -> &'a Value {
        let Value::Object(fields) = v else {
            panic!("{v:?}")
        };
        &fields
            .iter()
            .find(|(k, _)| k == key)
            .unwrap_or_else(|| panic!("{key}"))
            .1
    }

    fn config(mode: &str, interval: u32) -> EmitConfig {
        serde_json::from_value(serde_json::json!({"mode": mode, "min_interval_ms": interval}))
            .unwrap()
    }

    fn frame_tick(objects: Vec<Object>) -> Tick {
        Tick {
            emit_objects: Some(("people".into(), objects)),
            ..Tick::default()
        }
    }

    #[test]
    fn a_rule_event_carries_its_object_normalised() {
        let tick = Tick {
            results: vec![(
                "people".into(),
                Results::Objects(vec![person(Some(3), 0.9)]),
            )],
            events: vec![event()],
            ..Tick::default()
        };
        let mut em = Emitter::new(EmitConfig::default());
        let out = em.on_tick(&tick, &INFO, 1.0);
        let [Emission::Event(v)] = &out[..] else {
            panic!("{out:?}")
        };
        assert_eq!(get(v, "event"), &Value::String("line_crossing".into()));
        assert_eq!(get(v, "action"), &Value::String("in".into()));
        assert_eq!(get(v, "tracker_id"), &Value::U64(3));
        assert_eq!(get(v, "frame_seq"), &Value::U64(7));
        assert_eq!(get(v, "data"), &Value::String("{\"a\":1}".into()));
        let Value::Array(objects) = get(v, "objects") else {
            panic!()
        };
        let o = &objects[0];
        assert_eq!(get(o, "source"), &Value::String("people".into()));
        assert_eq!(get(get(o, "box"), "x2"), &Value::F32(0.5));
        let Value::Array(points) = get(o, "keypoints") else {
            panic!()
        };
        assert_eq!(
            get(&points[0], "x"),
            &Value::F32(0.25),
            "from the crop's pose"
        );
        let Value::Array(classes) = get(o, "classes") else {
            panic!()
        };
        assert_eq!(
            get(&classes[0], "class_name"),
            &Value::String("wave".into())
        );
    }

    #[test]
    fn payloads_fit_the_default_schema_exactly() {
        let schema = rules::parse_json(include_str!("../schema/out.json")).unwrap();
        let tick = Tick {
            results: vec![(
                "people".into(),
                Results::Objects(vec![person(Some(3), 0.9)]),
            )],
            events: vec![event()],
            emit_objects: Some(("people".into(), vec![person(Some(3), 0.9)])),
            ..Tick::default()
        };
        let mut em = Emitter::new(config("frames", 0));
        let out = em.on_tick(&tick, &INFO, 1.0);
        assert_eq!(out.len(), 2);
        for e in out {
            let (Emission::Event(v) | Emission::Frame(v)) = e;
            let coerced = coerce_payload(v.clone(), &schema).unwrap();
            assert_eq!(coerced, v, "nothing dropped or changed by the schema");
            coerced.into_payload().unwrap();
        }
    }

    #[test]
    fn rules_mode_sends_only_events() {
        let mut em = Emitter::new(EmitConfig::default());
        assert!(
            em.on_tick(&frame_tick(vec![person(Some(1), 0.9)]), &INFO, 0.0)
                .is_empty()
        );
    }

    #[test]
    fn frames_mode_is_rate_limited() {
        let mut em = Emitter::new(config("frames", 500));
        let t = frame_tick(vec![person(Some(1), 0.9)]);
        let sent: Vec<usize> = (0..12)
            .filter(|n| !em.on_tick(&t, &INFO, f64::from(*n) * 0.1).is_empty())
            .map(|n| n as usize)
            .collect();
        assert_eq!(sent, [0, 5, 10]);
    }

    #[test]
    fn changes_mode_ignores_movement_and_scores() {
        let mut em = Emitter::new(config("changes", 0));
        let a = person(Some(1), 0.9);
        assert_eq!(
            em.on_tick(&frame_tick(vec![a.clone()]), &INFO, 0.0).len(),
            1
        );
        let mut moved = a.clone();
        moved.bbox = [0.0, 0.0, 10.0, 10.0];
        moved.confidence = 0.5;
        assert!(em.on_tick(&frame_tick(vec![moved]), &INFO, 0.1).is_empty());
        let mut other = a.clone();
        other.children[1] = (
            "gesture".into(),
            Results::Classes(vec![Class {
                class_id: 5,
                class_name: None,
                confidence: 0.9,
            }]),
        );
        assert_eq!(em.on_tick(&frame_tick(vec![other]), &INFO, 0.2).len(), 1);
        let both = vec![a.clone(), person(Some(2), 0.8)];
        assert_eq!(em.on_tick(&frame_tick(both), &INFO, 0.3).len(), 1);
    }

    #[test]
    fn a_frame_keeps_its_most_confident_objects() {
        let mut c = config("frames", 0);
        c.max_objects = 2;
        let mut em = Emitter::new(c);
        let list = vec![
            person(Some(1), 0.2),
            person(Some(2), 0.9),
            person(Some(3), 0.8),
        ];
        let out = em.on_tick(&frame_tick(list), &INFO, 0.0);
        let [Emission::Frame(v)] = &out[..] else {
            panic!()
        };
        let Value::Array(objects) = get(v, "objects") else {
            panic!()
        };
        let ids: Vec<&Value> = objects.iter().map(|o| get(o, "tracker_id")).collect();
        assert_eq!(ids, [&Value::U64(2), &Value::U64(3)]);
    }

    fn now<T>(f: impl Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async {
                tokio::time::timeout(Duration::from_secs(5), f)
                    .await
                    .expect("timed out")
            })
    }

    #[test]
    fn events_go_first_and_stay_until_done() {
        let o = Outbox::new(8);
        o.push_frame("f1");
        o.push_event("e1");
        o.push_event("e2");
        assert_eq!(now(o.next()), Outgoing::Event("e1"));
        assert_eq!(
            now(o.next()),
            Outgoing::Event("e1"),
            "not confirmed yet: sent again"
        );
        o.event_done();
        assert_eq!(now(o.next()), Outgoing::Event("e2"));
        o.event_done();
        assert_eq!(now(o.next()), Outgoing::Frame("f1"));
        assert_eq!(o.stats().events_sent, 2);
    }

    #[test]
    fn frames_replace_each_other() {
        let o = Outbox::new(8);
        for f in ["a", "b", "c"] {
            o.push_frame(f);
        }
        assert_eq!(now(o.next()), Outgoing::Frame("c"));
        assert_eq!(o.stats().frames_replaced, 2);
    }

    #[test]
    fn a_full_queue_drops_the_oldest_but_not_the_one_in_flight() {
        let o = Outbox::new(3);
        for e in [1, 2, 3, 4, 5] {
            o.push_event(e);
        }
        assert_eq!(o.stats().events_dropped, 2);
        let mut got = vec![];
        for _ in 0..3 {
            let Outgoing::Event(e) = now(o.next()) else {
                panic!()
            };
            got.push(e);
            o.event_done();
        }
        assert_eq!(got, [1, 4, 5]);
    }

    #[test]
    fn a_waiting_reader_is_woken() {
        let o = std::sync::Arc::new(Outbox::new(4));
        let writer = o.clone();
        std::thread::spawn(move || writer.push_event(9));
        assert_eq!(now(o.next()), Outgoing::Event(9));
    }
}
