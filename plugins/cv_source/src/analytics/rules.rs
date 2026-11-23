//! Rules: geometry and thresholds over tracks that become events, each with
//! debounce (a change must hold), hysteresis (a margin to change back) and
//! cooldown (a least time between events of one track). Names follow ONVIF
//! Profile M where it has one.
//!
//! Lines and zones are in normalised frame coordinates (0..1, y down).

use super::track::{TrackEvent, TrackInfo};
use crate::graph::config::{
    Anchor, ClassRef, LineDirection, RuleConfig, RuleInput, RuleType, TrackOn, ZoneOn,
};
use crate::results::{Class, Object, Results};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

/// Something a rule saw happen.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RuleEvent {
    /// The rule's graph node.
    pub node: String,
    /// The rule's name.
    pub rule: String,
    /// `line_crossing`, `zone`, `loitering`, `counting`, `track`,
    /// `label_changed`, `score_above`, or a Lua rule's own.
    pub event: String,
    /// `in` / `out`, `enter` / `exit`, `exceeded`, `above` / `below`,
    /// `started` / `ended`, `changed`.
    pub action: String,
    /// Seconds on the stream's clock.
    pub time: f64,
    pub tracker_id: Option<u64>,
    pub class_name: Option<String>,
    /// The object's box (frame pixels), if the event is about one.
    pub bbox: Option<[f32; 4]>,
    pub count: Option<u32>,
    pub duration_s: Option<f32>,
    pub score: Option<f32>,
    pub label: Option<String>,
    pub previous: Option<String>,
    /// A Lua rule's own values.
    pub data: Option<serde_json::Value>,
}

/// A yes / no that changes only after holding for the debounce, and reports
/// a change only outside the cooldown. The first value is taken as it is.
#[derive(Clone, Copy, Debug, Default)]
struct Gate {
    state: Option<bool>,
    since: Option<f64>,
    last_fire: Option<f64>,
}

impl Gate {
    fn step(
        &mut self,
        value: Option<bool>,
        time: f64,
        debounce: f32,
        cooldown: f32,
    ) -> Option<bool> {
        let value = value?;
        let Some(state) = self.state else {
            self.state = Some(value);
            return None;
        };
        if value == state {
            self.since = None;
            return None;
        }
        let since = *self.since.get_or_insert(time);
        if time - since < f64::from(debounce) {
            return None;
        }
        self.state = Some(value);
        self.since = None;
        fired(&mut self.last_fire, time, cooldown).then_some(value)
    }
}

/// Whether an event may go out now; if so, notes it.
fn fired(last: &mut Option<f64>, time: f64, cooldown: f32) -> bool {
    if last.is_some_and(|l| time - l < f64::from(cooldown)) {
        return false;
    }
    *last = Some(time);
    true
}

fn cross(a: [f32; 2], b: [f32; 2], p: [f32; 2]) -> f32 {
    (b[0] - a[0]) * (p[1] - a[1]) - (b[1] - a[1]) * (p[0] - a[0])
}

fn segment_distance(a: [f32; 2], b: [f32; 2], p: [f32; 2]) -> f32 {
    let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
    let len = dx * dx + dy * dy;
    let t = if len > 0.0 {
        (((p[0] - a[0]) * dx + (p[1] - a[1]) * dy) / len).clamp(0.0, 1.0)
    } else {
        0.0
    };
    (p[0] - a[0] - t * dx).hypot(p[1] - a[1] - t * dy)
}

fn segments_cross(p: [f32; 2], q: [f32; 2], a: [f32; 2], b: [f32; 2]) -> bool {
    let (d1, d2) = (cross(a, b, p), cross(a, b, q));
    let (d3, d4) = (cross(p, q, a), cross(p, q, b));
    d1 * d2 <= 0.0 && d3 * d4 <= 0.0 && !(d1 == 0.0 && d2 == 0.0)
}

/// Which side of a polyline a point is on (the nearest segment's; true =
/// to the right, walking from its first point to its last) and how far.
pub(crate) fn line_side(line: &[[f32; 2]], p: [f32; 2]) -> (bool, f32) {
    let mut best = (true, f32::INFINITY);
    for s in line.windows(2) {
        let d = segment_distance(s[0], s[1], p);
        if d < best.1 {
            best = (cross(s[0], s[1], p) > 0.0, d);
        }
    }
    best
}

pub(crate) fn line_crossed(line: &[[f32; 2]], from: [f32; 2], to: [f32; 2]) -> bool {
    line.windows(2)
        .any(|s| segments_cross(from, to, s[0], s[1]))
}

/// Whether a point is in a polygon, and how far from its edge.
pub(crate) fn in_zone(zone: &[[f32; 2]], p: [f32; 2]) -> (bool, f32) {
    let mut inside = false;
    let mut distance = f32::INFINITY;
    let mut j = zone.len() - 1;
    for i in 0..zone.len() {
        let (a, b) = (zone[i], zone[j]);
        if (a[1] > p[1]) != (b[1] > p[1])
            && p[0] < (b[0] - a[0]) * (p[1] - a[1]) / (b[1] - a[1]) + a[0]
        {
            inside = !inside;
        }
        distance = distance.min(segment_distance(a, b, p));
        j = i;
    }
    (inside, distance)
}

/// The zone's verdict with a margin: `None` inside the margin (no change).
fn zone_state(zone: &[[f32; 2]], p: [f32; 2], margin: f32) -> Option<bool> {
    let (inside, distance) = in_zone(zone, p);
    (distance >= margin).then_some(inside)
}

fn anchor(a: Anchor, b: [f32; 4], w: f32, h: f32) -> [f32; 2] {
    let x = (b[0] + b[2]) / 2.0 / w;
    match a {
        Anchor::BottomCenter => [x, b[3] / h],
        Anchor::Center => [x, (b[1] + b[3]) / 2.0 / h],
        Anchor::TopCenter => [x, b[1] / h],
    }
}

fn class_in(classes: &[ClassRef], id: u32, name: Option<&str>) -> bool {
    classes.is_empty()
        || classes.iter().any(|c| match c {
            ClassRef::Id(i) => *i == id,
            ClassRef::Label(l) => name == Some(l.as_str()),
        })
}

fn top(classes: &[Class], wanted: Option<&ClassRef>) -> Option<(String, f32)> {
    let c = match wanted {
        None => classes.first()?,
        Some(ClassRef::Id(id)) => classes.iter().find(|c| c.class_id == *id)?,
        Some(ClassRef::Label(l)) => classes
            .iter()
            .find(|c| c.class_name.as_deref() == Some(l.as_str()))?,
    };
    let name = c
        .class_name
        .clone()
        .unwrap_or_else(|| c.class_id.to_string());
    Some((name, c.confidence))
}

fn child_classes<'a>(o: &'a Object, node: &str) -> Option<&'a [Class]> {
    o.children.iter().find_map(|(n, r)| match r {
        Results::Classes(c) if n == node => Some(c.as_slice()),
        _ => None,
    })
}

/// A rule's memory of one track (or of the frame: key 0).
#[derive(Clone, Debug, Default)]
struct Memory {
    gate: Gate,
    gate2: Gate,
    /// Line: the settled side, and where the object last was on it.
    side: Option<(bool, [f32; 2])>,
    pending: Option<(bool, f64)>,
    last_fire: Option<f64>,
    /// Loitering: since when it's been inside.
    entered: Option<f64>,
    done: bool,
    label: Option<String>,
    candidate: Option<(String, f64)>,
    announced: bool,
}

/// What a rule looks at on a frame.
pub struct RuleInputs<'a> {
    pub objects: &'a [Object],
    pub classes: &'a [Class],
    pub tracks: &'a [TrackInfo],
    pub track_events: &'a [TrackEvent],
    pub width: f32,
    pub height: f32,
    pub time: f64,
}

/// A score to judge: its key, object, value (`None`: no result this frame)
/// and label.
type Scored<'a> = (u64, Option<&'a Object>, Option<f32>, Option<String>);

/// A rule node, with its memory.
pub struct RuleEngine {
    node: String,
    name: String,
    config: RuleConfig,
    memory: BTreeMap<u64, Memory>,
}

impl RuleEngine {
    pub fn new(node: &str, config: RuleConfig) -> Self {
        Self {
            node: node.to_string(),
            name: config.name.clone().unwrap_or_else(|| node.to_string()),
            config,
            memory: BTreeMap::new(),
        }
    }

    fn event(&self, event: &str, action: &str, time: f64) -> RuleEvent {
        RuleEvent {
            node: self.node.clone(),
            rule: self.name.clone(),
            event: event.to_string(),
            action: action.to_string(),
            time,
            tracker_id: None,
            class_name: None,
            bbox: None,
            count: None,
            duration_s: None,
            score: None,
            label: None,
            previous: None,
            data: None,
        }
    }

    fn about(&self, mut e: RuleEvent, o: &Object) -> RuleEvent {
        e.tracker_id = o.tracker_id;
        e.class_name = o.class_name.clone();
        e.bbox = Some(o.bbox);
        e
    }

    /// The tracked objects of the rule's classes.
    fn tracked<'a>(&self, objects: &'a [Object]) -> Vec<(u64, &'a Object)> {
        objects
            .iter()
            .filter(|o| class_in(&self.config.classes, o.class_id, o.class_name.as_deref()))
            .filter_map(|o| o.tracker_id.map(|id| (id, o)))
            .collect()
    }

    /// The events of a frame.
    pub fn update(&mut self, input: &RuleInputs) -> Vec<RuleEvent> {
        let rule = self.config.rule.clone();
        let mut out = match &rule {
            RuleType::LineCrossing { line, direction } => self.line(input, line, *direction),
            RuleType::Zone { zone, on } => self.zone(input, zone, *on),
            RuleType::Loitering { zone, seconds } => self.loitering(input, zone, *seconds),
            RuleType::Counting { zone, above, below } => {
                self.counting(input, zone.as_deref(), *above, *below)
            }
            RuleType::Track { on, min_age_s } => self.track(input, *on, *min_age_s),
            RuleType::LabelChanged { from } => self.label_changed(input, from.as_deref()),
            RuleType::ScoreAbove {
                threshold,
                from,
                class,
            } => self.score_above(input, *threshold, from.as_deref(), class.as_ref()),
        };
        out.sort_by_key(|e| e.tracker_id);
        // A finished track's memory goes with it.
        if !matches!(rule, RuleType::Track { .. }) {
            for e in input.track_events {
                if let TrackEvent::Ended(t) = e {
                    self.memory.remove(&t.id);
                }
            }
        }
        out
    }

    fn line(&mut self, i: &RuleInputs, line: &[[f32; 2]], want: LineDirection) -> Vec<RuleEvent> {
        let (margin, debounce, cooldown) = (
            self.config.hysteresis,
            self.config.debounce_s,
            self.config.cooldown_s,
        );
        let mut out = vec![];
        for (id, o) in self.tracked(i.objects) {
            let p = anchor(self.config.anchor, o.bbox, i.width, i.height);
            let (side, distance) = line_side(line, p);
            let m = self.memory.entry(id).or_default();
            if distance < margin {
                m.pending = None;
                continue;
            }
            let Some((settled, from)) = m.side else {
                m.side = Some((side, p));
                continue;
            };
            if side == settled {
                m.side = Some((side, p));
                m.pending = None;
                continue;
            }
            // Around the end of the line isn't across it.
            if !line_crossed(line, from, p) {
                m.side = Some((side, p));
                m.pending = None;
                continue;
            }
            let since = m.pending.get_or_insert((side, i.time)).1;
            if i.time - since < f64::from(debounce) {
                continue;
            }
            m.side = Some((side, p));
            m.pending = None;
            let wanted = match want {
                LineDirection::Both => true,
                LineDirection::In => side,
                LineDirection::Out => !side,
            };
            if wanted && fired(&mut m.last_fire, i.time, cooldown) {
                let action = if side { "in" } else { "out" };
                let e = self.event("line_crossing", action, i.time);
                out.push(self.about(e, o));
            }
        }
        out
    }

    fn zone(&mut self, i: &RuleInputs, zone: &[[f32; 2]], on: ZoneOn) -> Vec<RuleEvent> {
        let (margin, debounce, cooldown) = (
            self.config.hysteresis,
            self.config.debounce_s,
            self.config.cooldown_s,
        );
        let mut out = vec![];
        for (id, o) in self.tracked(i.objects) {
            let p = anchor(self.config.anchor, o.bbox, i.width, i.height);
            let state = zone_state(zone, p, margin);
            let m = self.memory.entry(id).or_default();
            let Some(inside) = m.gate.step(state, i.time, debounce, cooldown) else {
                continue;
            };
            let wanted = match on {
                ZoneOn::Both => true,
                ZoneOn::Enter => inside,
                ZoneOn::Exit => !inside,
            };
            if wanted {
                let action = if inside { "enter" } else { "exit" };
                let e = self.event("zone", action, i.time);
                out.push(self.about(e, o));
            }
        }
        out
    }

    fn loitering(&mut self, i: &RuleInputs, zone: &[[f32; 2]], seconds: f32) -> Vec<RuleEvent> {
        let (margin, debounce, cooldown) = (
            self.config.hysteresis,
            self.config.debounce_s,
            self.config.cooldown_s,
        );
        let mut out = vec![];
        for (id, o) in self.tracked(i.objects) {
            let p = anchor(self.config.anchor, o.bbox, i.width, i.height);
            let state = zone_state(zone, p, margin);
            let m = self.memory.entry(id).or_default();
            // Stepping out for less than the debounce doesn't reset the time.
            m.gate.step(state, i.time, debounce, 0.0);
            if m.gate.state == Some(true) {
                let since = *m.entered.get_or_insert(i.time);
                let held = (i.time - since) as f32;
                if !m.done && held >= seconds && fired(&mut m.last_fire, i.time, cooldown) {
                    m.done = true;
                    let mut e = self.event("loitering", "exceeded", i.time);
                    e.duration_s = Some(held);
                    out.push(self.about(e, o));
                }
            } else {
                m.entered = None;
                m.done = false;
            }
        }
        out
    }

    fn counting(
        &mut self,
        i: &RuleInputs,
        zone: Option<&[[f32; 2]]>,
        above: Option<u32>,
        below: Option<u32>,
    ) -> Vec<RuleEvent> {
        let (debounce, cooldown) = (self.config.debounce_s, self.config.cooldown_s);
        let margin = self.config.hysteresis;
        let count = i
            .objects
            .iter()
            .filter(|o| class_in(&self.config.classes, o.class_id, o.class_name.as_deref()))
            .filter(|o| {
                zone.is_none_or(|z| {
                    in_zone(z, anchor(self.config.anchor, o.bbox, i.width, i.height)).0
                })
            })
            .count() as u32;
        let n = count as f32;
        let m = self.memory.entry(0).or_default();
        let mut hits = vec![];
        if let Some(limit) = above {
            let limit = limit as f32;
            let was = m.gate.state.unwrap_or(n >= limit);
            let value = if was {
                n > limit - 1.0 - margin
            } else {
                n >= limit
            };
            if m.gate.step(Some(value), i.time, debounce, 0.0) == Some(true)
                && fired(&mut m.last_fire, i.time, cooldown)
            {
                hits.push("above");
            }
        }
        if let Some(limit) = below {
            let limit = limit as f32;
            let was = m.gate2.state.unwrap_or(n <= limit);
            let value = if was {
                n < limit + 1.0 + margin
            } else {
                n <= limit
            };
            if m.gate2.step(Some(value), i.time, debounce, 0.0) == Some(true)
                && fired(&mut m.last_fire, i.time, cooldown)
            {
                hits.push("below");
            }
        }
        hits.into_iter()
            .map(|action| {
                let mut e = self.event("counting", action, i.time);
                e.count = Some(count);
                e
            })
            .collect()
    }

    fn track(&mut self, i: &RuleInputs, on: TrackOn, min_age: f32) -> Vec<RuleEvent> {
        let cooldown = self.config.cooldown_s;
        let classes = self.config.classes.clone();
        let wanted = |id: u32, name: Option<&str>| class_in(&classes, id, name);
        let mut out = vec![];
        let of_track = |this: &Self, action: &str, t: &TrackInfo| {
            let mut e = this.event("track", action, i.time);
            e.tracker_id = Some(t.id);
            e.class_name = t.class_name.clone();
            e.bbox = Some(t.bbox);
            e.duration_s = Some(t.age_s);
            e
        };
        for t in i.tracks {
            if t.age_s >= min_age && wanted(t.class_id, t.class_name.as_deref()) {
                let m = self.memory.entry(t.id).or_default();
                if !m.announced {
                    m.announced = true;
                    if on != TrackOn::Ended && fired(&mut m.last_fire, i.time, cooldown) {
                        out.push(of_track(self, "started", t));
                    }
                }
            }
        }
        for e in i.track_events {
            if let TrackEvent::Ended(t) = e
                && let Some(m) = self.memory.remove(&t.id)
                && m.announced
                && on != TrackOn::Started
            {
                out.push(of_track(self, "ended", t));
            }
        }
        out
    }

    fn label_changed(&mut self, i: &RuleInputs, from: Option<&str>) -> Vec<RuleEvent> {
        let (debounce, cooldown) = (self.config.debounce_s, self.config.cooldown_s);
        let mut seen: Vec<(u64, Option<&Object>, String)> = vec![];
        if self.config.input == RuleInput::Classes {
            if let Some((label, _)) = top(i.classes, None) {
                seen.push((0, None, label));
            }
        } else {
            for (id, o) in self.tracked(i.objects) {
                let label = match from {
                    Some(node) => child_classes(o, node)
                        .and_then(|c| top(c, None))
                        .map(|t| t.0),
                    None => o
                        .class_name
                        .clone()
                        .or_else(|| Some(o.class_id.to_string())),
                };
                if let Some(label) = label {
                    seen.push((id, Some(o), label));
                }
            }
        }
        let mut out = vec![];
        for (id, o, label) in seen {
            let m = self.memory.entry(id).or_default();
            let Some(current) = m.label.clone() else {
                m.label = Some(label);
                continue;
            };
            if label == current {
                m.candidate = None;
                continue;
            }
            let since = match &m.candidate {
                Some((c, t)) if *c == label => *t,
                _ => {
                    m.candidate = Some((label.clone(), i.time));
                    i.time
                }
            };
            if i.time - since < f64::from(debounce) {
                continue;
            }
            m.label = Some(label.clone());
            m.candidate = None;
            if fired(&mut m.last_fire, i.time, cooldown) {
                let mut e = self.event("label_changed", "changed", i.time);
                e.label = Some(label);
                e.previous = Some(current);
                out.push(match o {
                    Some(o) => self.about(e, o),
                    None => e,
                });
            }
        }
        out
    }

    fn score_above(
        &mut self,
        i: &RuleInputs,
        threshold: f32,
        from: Option<&str>,
        class: Option<&ClassRef>,
    ) -> Vec<RuleEvent> {
        let (margin, debounce, cooldown) = (
            self.config.hysteresis,
            self.config.debounce_s,
            self.config.cooldown_s,
        );
        let mut seen: Vec<Scored> = vec![];
        if self.config.input == RuleInput::Classes {
            let t = top(i.classes, class);
            let label = t.as_ref().map(|t| t.0.clone());
            seen.push((0, None, Some(t.map_or(0.0, |t| t.1)), label));
        } else {
            for (id, o) in self.tracked(i.objects) {
                match from {
                    Some(node) => {
                        // No result on this frame says nothing either way.
                        let t = child_classes(o, node).map(|c| top(c, class));
                        let label = t.clone().flatten().map(|t| t.0);
                        let score = t.map(|t| t.map_or(0.0, |t| t.1));
                        seen.push((id, Some(o), score, label));
                    }
                    None => seen.push((id, Some(o), Some(o.confidence), o.class_name.clone())),
                }
            }
        }
        let mut out = vec![];
        for (id, o, score, label) in seen {
            let m = self.memory.entry(id).or_default();
            let was = m.gate.state.unwrap_or(false);
            // A first value above the threshold counts as a rise.
            if m.gate.state.is_none() {
                m.gate.state = Some(false);
            }
            let value = score.map(|s| {
                if was {
                    s > threshold - margin
                } else {
                    s >= threshold
                }
            });
            if m.gate.step(value, i.time, debounce, cooldown) == Some(true) {
                let mut e = self.event("score_above", "above", i.time);
                e.score = score;
                e.label = label;
                out.push(match o {
                    Some(o) => self.about(e, o),
                    None => e,
                });
            }
        }
        out
    }

    /// The tracks this rule remembers (for tests).
    #[cfg(test)]
    fn remembered(&self) -> BTreeSet<u64> {
        self.memory.keys().copied().collect()
    }
}

/// The ids that ended on a frame.
pub(crate) fn ended_ids(events: &[TrackEvent]) -> BTreeSet<u64> {
    events
        .iter()
        .filter_map(|e| match e {
            TrackEvent::Ended(t) => Some(t.id),
            TrackEvent::Started(_) => None,
        })
        .collect()
}

#[path = "rules_tests.rs"]
#[cfg(test)]
mod tests;
