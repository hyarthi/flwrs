//! Object tracking, ByteTrack-style: a constant-velocity Kalman filter per
//! track, and two rounds of optimal assignment by box overlap (confident
//! detections first, then the weak ones on the tracks still unmatched, which
//! keeps a partly hidden object's track alive).

use super::assign::assign;
use super::pose::{MIN_VISIBLE, OneEuro, OneEuroSettings};
use crate::graph::config::TrackerConfig;
use crate::results::{Keypoint, Object};
use serde::Serialize;
use std::collections::VecDeque;

/// Tracks, as rules see them.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TrackInfo {
    pub id: u64,
    pub class_id: u32,
    pub class_name: Option<String>,
    /// Frame pixels: the last match, or the prediction while lost.
    pub bbox: [f32; 4],
    /// Of the box's centre, pixels per second.
    pub velocity: [f32; 2],
    /// Seconds since it first appeared.
    pub age_s: f32,
    /// Not matched on this frame.
    pub lost: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TrackEvent {
    /// Confirmed (seen on `min_hits` frames).
    Started(TrackInfo),
    /// Gone for `max_lost_s`.
    Ended(TrackInfo),
}

/// What an update gives.
#[derive(Clone, Debug, Default)]
pub struct TrackUpdate {
    /// The frame's objects, those on a confirmed track with its id.
    pub objects: Vec<Object>,
    /// Every confirmed track, including the ones lost for now.
    pub tracks: Vec<TrackInfo>,
    pub events: Vec<TrackEvent>,
}

/// One value and its speed, with their uncertainty.
#[derive(Clone, Copy, Debug)]
struct Axis {
    x: f32,
    v: f32,
    pxx: f32,
    pxv: f32,
    pvv: f32,
}

impl Axis {
    fn new(x: f32, scale: f32) -> Self {
        Self {
            x,
            v: 0.0,
            pxx: (scale * MEASUREMENT).powi(2) * 4.0,
            pxv: 0.0,
            pvv: (scale * VELOCITY_START).powi(2),
        }
    }

    fn predict(&mut self, dt: f32, scale: f32) {
        self.x += self.v * dt;
        self.pxx += dt * (2.0 * self.pxv + dt * self.pvv) + (scale * POSITION_NOISE).powi(2) * dt;
        self.pxv += dt * self.pvv;
        self.pvv += (scale * VELOCITY_NOISE).powi(2) * dt;
    }

    fn update(&mut self, z: f32, scale: f32, gentleness: f32) {
        let r = (scale * MEASUREMENT * gentleness).powi(2).max(1e-6);
        let s = self.pxx + r;
        let (kx, kv) = (self.pxx / s, self.pxv / s);
        let error = z - self.x;
        self.x += kx * error;
        self.v += kv * error;
        let (pxx, pxv) = (self.pxx, self.pxv);
        self.pxx = (1.0 - kx) * pxx;
        self.pxv = (1.0 - kx) * pxv;
        self.pvv -= kv * pxv;
    }
}

// As shares of the box's height (per second for the noises).
const MEASUREMENT: f32 = 0.05;
const POSITION_NOISE: f32 = 0.2;
const VELOCITY_NOISE: f32 = 1.0;
const VELOCITY_START: f32 = 1.0;
/// A box's largest predicted speed, in heights per second.
const MAX_SPEED: f32 = 8.0;
/// Positions kept per track (seconds of them at any frame rate that matters).
const HISTORY: usize = 90;
/// With control frames: unconfirmed for this long, a track counts as lost.
const LOST_AFTER_S: f64 = 0.3;
/// With control frames: how fast the box's size eases to a detection's, in
/// seconds per `box_smoothing`.
const EASE_S: f32 = 0.05;
/// Keypoints seen on two frames in a row that it takes to move a box.
const MIN_SHARED_POINTS: usize = 3;

fn bare(o: &Object) -> Object {
    let mut o = o.clone();
    o.children = vec![];
    o.tracker_id = None;
    o
}

/// How far the centre moved from box `a` to box `b`.
fn shift_between(a: [f32; 4], b: [f32; 4]) -> (f32, f32) {
    (
        (b[0] + b[2] - a[0] - a[2]) / 2.0,
        (b[1] + b[3] - a[1] - a[3]) / 2.0,
    )
}

struct Track {
    /// 0 until it's confirmed: ids aren't spent on flicker.
    id: u64,
    class_id: u32,
    class_name: Option<String>,
    axes: [Axis; 4],
    start: f64,
    last_seen: f64,
    hits: u32,
    matched: bool,
    smooth: Vec<[OneEuro; 2]>,
    /// The last detection (no children), to carry it between control frames.
    last: Object,
    /// Control frames in a row that didn't find it.
    missed: u32,
    /// Where it was lately: a late detection is matched against this.
    history: VecDeque<(f64, [f32; 4])>,
    /// The pose on its last crop, to measure how the body moved.
    last_pose: Vec<Keypoint>,
    /// With control frames: how gently detections correct the box (1 = not).
    gentleness: f32,
    /// The centre when the last pose was taken.
    steer_from: (f32, f32),
    /// With control frames: the size the box eases towards, a little each
    /// frame, so a detection's correction never shows as a jump.
    target_size: Option<[f32; 2]>,
}

fn to_box(a: &[Axis; 4]) -> [f32; 4] {
    let (w, h) = (a[2].x.max(1.0), a[3].x.max(1.0));
    [
        a[0].x - w / 2.0,
        a[1].x - h / 2.0,
        a[0].x + w / 2.0,
        a[1].x + h / 2.0,
    ]
}

fn measure(b: [f32; 4]) -> [f32; 4] {
    [
        (b[0] + b[2]) / 2.0,
        (b[1] + b[3]) / 2.0,
        b[2] - b[0],
        b[3] - b[1],
    ]
}

fn iou(a: [f32; 4], b: [f32; 4]) -> f32 {
    let w = (a[2].min(b[2]) - a[0].max(b[0])).max(0.0);
    let h = (a[3].min(b[3]) - a[1].max(b[1])).max(0.0);
    let inter = w * h;
    let union = (a[2] - a[0]).max(0.0) * (a[3] - a[1]).max(0.0)
        + (b[2] - b[0]).max(0.0) * (b[3] - b[1]).max(0.0)
        - inter;
    if union > 0.0 { inter / union } else { 0.0 }
}

impl Track {
    fn new(o: &Object, time: f64) -> Self {
        let m = measure(o.bbox);
        let scale = m[3].max(1.0);
        Self {
            id: 0,
            class_id: o.class_id,
            class_name: o.class_name.clone(),
            axes: m.map(|v| Axis::new(v, scale)),
            start: time,
            last_seen: time,
            hits: 1,
            matched: true,
            smooth: vec![],
            last: bare(o),
            missed: 0,
            history: VecDeque::new(),
            last_pose: vec![],
            gentleness: 1.0,
            steer_from: (m[0], m[1]),
            target_size: None,
        }
    }

    fn remember(&mut self, time: f64) {
        if self.history.len() == HISTORY {
            self.history.pop_front();
        }
        self.history.push_back((time, to_box(&self.axes)));
    }

    /// Where it was at `time`.
    fn box_at(&self, time: f64) -> [f32; 4] {
        self.history
            .iter()
            .rev()
            .find(|(t, _)| *t <= time + 1e-9)
            .or(self.history.front())
            .map_or_else(|| to_box(&self.axes), |(_, b)| *b)
    }

    /// The object as it stands now, from its last detection.
    fn carried(&self) -> Object {
        let mut o = self.last.clone();
        let now = to_box(&self.axes);
        let (dx, dy) = shift_between(o.bbox, now);
        for k in &mut o.keypoints {
            k.x += dx;
            k.y += dy;
        }
        o.bbox = now;
        o.tracker_id = Some(self.id);
        o
    }

    fn scale(&self) -> f32 {
        self.axes[3].x.max(1.0)
    }

    fn predict(&mut self, dt: f32) {
        let scale = self.scale();
        for a in &mut self.axes {
            a.predict(dt, scale);
            let limit = scale * MAX_SPEED;
            a.v = a.v.clamp(-limit, limit);
        }
        if let Some(target) = self.target_size {
            let share = 1.0 - (-dt / (EASE_S * self.gentleness)).exp();
            for (a, t) in self.axes[2..].iter_mut().zip(target) {
                a.x += (t - a.x) * share;
                a.v = 0.0;
            }
        }
        self.matched = false;
    }

    /// Moves the box by how the body moved. Only the centre: the pose's
    /// extent changes with every raised arm, so the size stays the detector's.
    fn steer(&mut self, dx: f32, dy: f32, time: f64) {
        let scale = self.scale();
        let (x, y) = (self.steer_from.0 + dx, self.steer_from.1 + dy);
        self.axes[0].update(x, scale, 1.0);
        self.axes[1].update(y, scale, 1.0);
        self.last_seen = time;
        self.matched = true;
    }

    fn update(&mut self, o: &Object, time: f64) {
        let m = measure(o.bbox);
        let scale = m[3].max(1.0);
        let gentle = self.gentleness;
        if self.target_size.is_some() {
            self.target_size = Some([m[2], m[3]]);
            for (a, z) in self.axes[..2].iter_mut().zip(&m[..2]) {
                a.update(*z, scale, gentle);
            }
        } else {
            for (a, z) in self.axes.iter_mut().zip(m) {
                a.update(z, scale, gentle);
            }
        }
        self.class_id = o.class_id;
        self.class_name = o.class_name.clone();
        self.last_seen = time;
        self.hits = self.hits.saturating_add(1);
        self.matched = true;
        self.last = bare(o);
        self.missed = 0;
    }

    fn info(&self, time: f64, carried: bool) -> TrackInfo {
        TrackInfo {
            id: self.id,
            class_id: self.class_id,
            class_name: self.class_name.clone(),
            bbox: to_box(&self.axes),
            velocity: [self.axes[0].v, self.axes[1].v],
            age_s: (time - self.start) as f32,
            lost: if carried {
                time - self.last_seen > LOST_AFTER_S
            } else {
                !self.matched
            },
        }
    }

    fn smooth_keypoints(&mut self, points: &mut [Keypoint], time: f64, s: OneEuroSettings) {
        if self.smooth.len() != points.len() {
            self.smooth = vec![[OneEuro::default(); 2]; points.len()];
        }
        for (p, f) in points.iter_mut().zip(&mut self.smooth) {
            if p.confidence >= MIN_VISIBLE {
                p.x = f[0].filter(p.x, time, s);
                p.y = f[1].filter(p.y, time, s);
            }
        }
    }
}

/// Tracks objects across frames.
pub struct Tracker {
    config: TrackerConfig,
    tracks: Vec<Track>,
    next_id: u64,
    last_time: Option<f64>,
}

impl Tracker {
    pub fn new(config: TrackerConfig) -> Self {
        Self {
            config,
            tracks: vec![],
            next_id: 1,
            last_time: None,
        }
    }

    /// Pairs `dets` (indexes into `objects`) with `candidates` (indexes into
    /// the tracks, compared where they were at `at`, or now), best total
    /// overlap first.
    fn match_round(
        &self,
        objects: &[Object],
        dets: &[usize],
        candidates: &[usize],
        min_iou: f32,
        at: Option<f64>,
    ) -> Vec<(usize, usize)> {
        if dets.is_empty() || candidates.is_empty() {
            return vec![];
        }
        let cost: Vec<Vec<f64>> = dets
            .iter()
            .map(|&d| {
                candidates
                    .iter()
                    .map(|&t| {
                        let track = &self.tracks[t];
                        if self.config.class_aware && track.class_id != objects[d].class_id {
                            return f64::INFINITY;
                        }
                        let then = at.map_or_else(|| to_box(&track.axes), |t| track.box_at(t));
                        let overlap = iou(objects[d].bbox, then);
                        if overlap < min_iou {
                            f64::INFINITY
                        } else {
                            f64::from(1.0 - overlap)
                        }
                    })
                    .collect()
            })
            .collect();
        assign(&cost)
            .into_iter()
            .enumerate()
            .filter_map(|(i, c)| c.map(|c| (dets[i], candidates[c])))
            .collect()
    }

    /// Takes a frame's objects at `time` (seconds, not going back).
    pub fn update(&mut self, objects: &[Object], time: f64) -> TrackUpdate {
        self.advance(Some((objects, time)), time)
    }

    /// A frame at `time`, with what the detector found, if it finished a run
    /// (on the frame at its own, maybe earlier, time). With control frames,
    /// every live track is in the output, detected on this frame or not.
    pub fn advance(&mut self, detections: Option<(&[Object], f64)>, time: f64) -> TrackUpdate {
        let c = self.config.clone();
        let carried = c.control.is_some();
        let dt = self
            .last_time
            .map_or(0.0, |last| (time - last).max(0.0) as f32);
        self.last_time = Some(time);
        for t in &mut self.tracks {
            t.predict(dt);
        }
        let (objects, det_time) = detections.unwrap_or((&[], time));
        // A late detection is compared with where the tracks were then.
        let at = (det_time < time).then_some(det_time);
        let (mut high, mut low) = (vec![], vec![]);
        for (i, o) in objects.iter().enumerate() {
            if o.confidence >= c.high_confidence {
                high.push(i);
            } else if o.confidence >= c.low_confidence {
                low.push(i);
            }
        }
        let mut track_of: Vec<Option<usize>> = vec![None; objects.len()];

        // Round one: the confident detections, on every track. Confirmed
        // tracks choose first, so a new flicker can't take over one.
        let confirmed: Vec<usize> = (0..self.tracks.len())
            .filter(|&t| self.tracks[t].id != 0)
            .collect();
        let mut pairs = self.match_round(objects, &high, &confirmed, c.min_iou, at);
        let taken: Vec<usize> = pairs.iter().map(|p| p.0).collect();
        let free: Vec<usize> = high
            .iter()
            .copied()
            .filter(|d| !taken.contains(d))
            .collect();
        let tentative: Vec<usize> = (0..self.tracks.len())
            .filter(|&t| self.tracks[t].id == 0)
            .collect();
        pairs.extend(self.match_round(objects, &free, &tentative, c.min_iou, at));
        for (d, t) in pairs {
            track_of[d] = Some(t);
        }
        // Round two: the weak detections, on tracks not long lost.
        let used: Vec<usize> = track_of.iter().flatten().copied().collect();
        let open: Vec<usize> = confirmed
            .iter()
            .copied()
            .filter(|t| !used.contains(t))
            .filter(|&t| time - self.tracks[t].last_seen <= c.max_lost_s as f64)
            .collect();
        for (d, t) in self.match_round(objects, &low, &open, c.min_iou_low, at) {
            track_of[d] = Some(t);
        }

        let mut events = vec![];
        // Detections as they'd be now: moved by what their track moved since.
        let mut moved: Vec<Option<Object>> = vec![None; objects.len()];
        for (d, t) in track_of.iter().enumerate() {
            if let Some(t) = *t {
                let mut o = objects[d].clone();
                if let Some(then) = at {
                    let track = &self.tracks[t];
                    let (dx, dy) = shift_between(track.box_at(then), to_box(&track.axes));
                    o.bbox = [
                        o.bbox[0] + dx,
                        o.bbox[1] + dy,
                        o.bbox[2] + dx,
                        o.bbox[3] + dy,
                    ];
                    for k in &mut o.keypoints {
                        k.x += dx;
                        k.y += dy;
                    }
                }
                self.tracks[t].update(&o, time);
                moved[d] = Some(o);
            }
        }
        // New tracks from what's confident and unmatched.
        for &d in &high {
            if track_of[d].is_none() && objects[d].confidence >= c.new_track_confidence {
                let mut track = Track::new(&objects[d], time);
                if let Some(x) = c.control {
                    track.gentleness = x.box_smoothing;
                    track.target_size = Some([track.axes[2].x, track.axes[3].x]);
                }
                self.tracks.push(track);
                track_of[d] = Some(self.tracks.len() - 1);
            }
        }
        // Confirmation.
        for t in &mut self.tracks {
            if t.id == 0 && t.hits >= c.min_hits {
                t.id = self.next_id;
                self.next_id += 1;
                events.push(TrackEvent::Started(t.info(time, carried)));
            }
        }

        let mut out = Vec::with_capacity(objects.len());
        for (d, o) in objects.iter().enumerate() {
            match track_of[d] {
                Some(t) => {
                    let mut o = moved[d].take().unwrap_or_else(|| o.clone());
                    let track = &mut self.tracks[t];
                    o.tracker_id = (track.id != 0).then_some(track.id);
                    if carried && track.id != 0 {
                        // The steady box, not the detection's raw one.
                        let steady = to_box(&track.axes);
                        let (dx, dy) = shift_between(o.bbox, steady);
                        for k in &mut o.keypoints {
                            k.x += dx;
                            k.y += dy;
                        }
                        o.bbox = steady;
                    }
                    if let Some(s) = c.smooth_keypoints {
                        let s = OneEuroSettings {
                            min_cutoff: s.min_cutoff,
                            beta: s.beta,
                        };
                        track.smooth_keypoints(&mut o.keypoints, time, s);
                    }
                    out.push(o);
                }
                // A weak detection no track wants is noise.
                None if o.confidence >= c.high_confidence => out.push(o.clone()),
                None => {}
            }
        }

        // Control frames: a run that didn't find a track counts against it.
        let max_missed = c.control.map_or(u32::MAX, |x| x.max_missed);
        if detections.is_some() && carried {
            for t in self.tracks.iter_mut().filter(|t| t.id != 0 && !t.matched) {
                t.missed += 1;
            }
        }
        // Tracks that are gone. A tentative one goes on the first run that
        // doesn't find it (between runs, nothing says it's gone).
        let max_lost = c.max_lost_s as f64;
        let mut kept = Vec::with_capacity(self.tracks.len());
        for t in std::mem::take(&mut self.tracks) {
            if t.id == 0 {
                if t.matched || detections.is_none() {
                    kept.push(t);
                }
            } else if (!t.matched && time - t.last_seen > max_lost) || t.missed >= max_missed {
                events.push(TrackEvent::Ended(t.info(time, carried)));
            } else {
                kept.push(t);
            }
        }
        self.tracks = kept;
        for t in &mut self.tracks {
            t.remember(time);
        }
        if carried {
            out.extend(
                self.tracks
                    .iter()
                    .filter(|t| t.id != 0 && !t.matched)
                    .map(Track::carried),
            );
        }
        let tracks = self
            .tracks
            .iter()
            .filter(|t| t.id != 0)
            .map(|t| t.info(time, carried))
            .collect();
        TrackUpdate {
            objects: out,
            tracks,
            events,
        }
    }

    /// A pose found on track `id`'s crop: smooths it, and if enough of it is
    /// visible, lets it steer the box. False when it's too weak to trust.
    pub fn refine(&mut self, id: u64, points: &mut [Keypoint], time: f64) -> bool {
        let Some(control) = self.config.control else {
            return false;
        };
        let smooth = self.config.smooth_keypoints;
        let Some(track) = self.tracks.iter_mut().find(|t| t.id == id) else {
            return false;
        };
        if let Some(s) = smooth {
            let s = OneEuroSettings {
                min_cutoff: s.min_cutoff,
                beta: s.beta,
            };
            track.smooth_keypoints(points, time, s);
        }
        let visible: Vec<&Keypoint> = points
            .iter()
            .filter(|p| p.confidence >= MIN_VISIBLE)
            .collect();
        if points.is_empty() || (visible.len() as f32) < control.min_visible * points.len() as f32 {
            return false;
        }
        // How the body moved: the median of the points seen on both frames,
        // so one waving arm doesn't drag the box along.
        let (mut xs, mut ys) = (vec![], vec![]);
        if track.last_pose.len() == points.len() {
            for (p, q) in track.last_pose.iter().zip(points.iter()) {
                if p.confidence >= MIN_VISIBLE && q.confidence >= MIN_VISIBLE {
                    xs.push(q.x - p.x);
                    ys.push(q.y - p.y);
                }
            }
        }
        let shared = xs.len();
        track.last_pose = points.to_vec();
        if shared >= MIN_SHARED_POINTS {
            let median = |v: &mut Vec<f32>| {
                v.sort_by(f32::total_cmp);
                v[v.len() / 2]
            };
            let (dx, dy) = (median(&mut xs), median(&mut ys));
            track.steer(dx, dy, time);
        } else {
            track.last_seen = time;
            track.matched = true;
        }
        track.steer_from = (track.axes[0].x, track.axes[1].x);
        true
    }

    /// Whether a detection should run as soon as it can: something is
    /// waiting to be confirmed, or a track isn't being followed.
    pub fn wants_detection(&self, time: f64) -> bool {
        self.tracks
            .iter()
            .any(|t| t.id == 0 || time - t.last_seen > LOST_AFTER_S)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DT: f64 = 1.0 / 30.0;

    fn config() -> TrackerConfig {
        serde_json::from_value(serde_json::json!({})).unwrap()
    }

    fn det(class_id: u32, confidence: f32, x: f32, y: f32) -> Object {
        Object {
            tracker_id: None,
            class_id,
            class_name: Some(format!("c{class_id}")),
            confidence,
            bbox: [x, y, x + 40.0, y + 80.0],
            keypoints: vec![],
            children: vec![],
        }
    }

    fn ids(u: &TrackUpdate) -> Vec<Option<u64>> {
        u.objects.iter().map(|o| o.tracker_id).collect()
    }

    fn started(u: &TrackUpdate) -> usize {
        u.events
            .iter()
            .filter(|e| matches!(e, TrackEvent::Started(_)))
            .count()
    }

    fn ended(u: &TrackUpdate) -> usize {
        u.events
            .iter()
            .filter(|e| matches!(e, TrackEvent::Ended(_)))
            .count()
    }

    #[test]
    fn a_track_is_confirmed_after_its_hits_and_keeps_its_id() {
        let mut t = Tracker::new(config());
        let mut last = None;
        let mut starts = 0;
        for i in 0..20 {
            let u = t.update(&[det(0, 0.9, 100.0 + i as f32 * 3.0, 50.0)], i as f64 * DT);
            starts += started(&u);
            if i < 2 {
                assert_eq!(ids(&u), [None], "frame {i}");
            } else {
                assert_eq!(ids(&u), [Some(1)], "frame {i}");
            }
            last = u.tracks.first().cloned();
        }
        assert_eq!(starts, 1);
        let track = last.unwrap();
        assert!(
            (track.velocity[0] - 90.0).abs() < 25.0,
            "{:?}",
            track.velocity
        );
        assert!(track.age_s > 0.6);
    }

    #[test]
    fn a_flicker_gets_no_track_and_spends_no_id() {
        let mut t = Tracker::new(config());
        for i in 0..10 {
            let dets = if i % 3 == 0 {
                vec![det(0, 0.9, 10.0, 10.0)]
            } else {
                vec![]
            };
            let u = t.update(&dets, i as f64 * DT);
            assert!(u.tracks.is_empty() && u.events.is_empty());
        }
        let mut t2 = t;
        for i in 10..13 {
            t2.update(&[det(0, 0.9, 300.0, 10.0)], i as f64 * DT);
        }
        assert_eq!(t2.tracks[0].id, 1);
    }

    #[test]
    fn a_short_gap_keeps_the_id_and_a_long_one_ends_it() {
        let mut t = Tracker::new(config());
        let mut time = 0.0;
        for i in 0..10 {
            t.update(&[det(0, 0.9, 100.0 + i as f32 * 2.0, 50.0)], time);
            time += DT;
        }
        for _ in 0..10 {
            let u = t.update(&[], time);
            assert_eq!(u.tracks.len(), 1);
            assert!(u.tracks[0].lost);
            time += DT;
        }
        let u = t.update(&[det(0, 0.9, 140.0, 50.0)], time);
        assert_eq!(ids(&u), [Some(1)], "picked up again, where it was heading");
        time += DT;
        let mut ends = 0;
        for _ in 0..60 {
            ends += ended(&t.update(&[], time));
            time += DT;
        }
        assert_eq!(ends, 1);
        assert!(t.tracks.is_empty());
    }

    #[test]
    fn weak_detections_keep_a_track_but_dont_start_one() {
        let mut t = Tracker::new(config());
        let mut time = 0.0;
        for _ in 0..5 {
            t.update(&[det(0, 0.9, 100.0, 50.0)], time);
            time += DT;
        }
        for _ in 0..60 {
            let u = t.update(&[det(0, 0.2, 100.0, 50.0)], time);
            assert_eq!(ids(&u), [Some(1)]);
            assert!(!u.tracks[0].lost);
            time += DT;
        }
        let mut fresh = Tracker::new(config());
        for i in 0..10 {
            let u = fresh.update(&[det(0, 0.2, 100.0, 50.0)], i as f64 * DT);
            assert!(u.objects.is_empty(), "noise is dropped");
            assert!(u.tracks.is_empty());
        }
    }

    #[test]
    fn two_objects_passing_keep_their_ids() {
        let mut t = Tracker::new(config());
        let mut last = TrackUpdate::default();
        for i in 0..60 {
            let x = i as f32 * 4.0;
            // Vertically apart enough never to be confused.
            let dets = [det(0, 0.9, x, 0.0), det(0, 0.9, 240.0 - x, 200.0)];
            last = t.update(&dets, i as f64 * DT);
        }
        assert_eq!(ids(&last), [Some(1), Some(2)]);
    }

    #[test]
    fn classes_stay_apart_unless_told_otherwise() {
        let mut t = Tracker::new(config());
        for i in 0..4 {
            t.update(&[det(0, 0.9, 10.0, 10.0)], i as f64 * DT);
        }
        let u = t.update(&[det(1, 0.9, 10.0, 10.0)], 4.0 * DT);
        assert_eq!(ids(&u), [None], "another class is another object");
        let mut any: TrackerConfig = config();
        any.class_aware = false;
        let mut t = Tracker::new(any);
        for i in 0..4 {
            t.update(&[det(0, 0.9, 10.0, 10.0)], i as f64 * DT);
        }
        let u = t.update(&[det(1, 0.9, 10.0, 10.0)], 4.0 * DT);
        assert_eq!(ids(&u), [Some(1)]);
        assert_eq!(u.tracks[0].class_id, 1);
    }

    #[test]
    fn keypoints_are_smoothed_when_asked() {
        let mut c = config();
        c.smooth_keypoints = Some(crate::graph::config::SmoothKeypoints {
            min_cutoff: 1.0,
            beta: 0.0,
        });
        let mut t = Tracker::new(c);
        let mut spread: f32 = 0.0;
        for i in 0..60 {
            let mut o = det(0, 0.9, 100.0, 50.0);
            let noise = if i % 2 == 0 { 3.0 } else { -3.0 };
            o.keypoints = vec![Keypoint {
                x: 120.0 + noise,
                y: 60.0,
                confidence: 0.9,
            }];
            let u = t.update(&[o], i as f64 * DT);
            if i > 30 {
                spread = spread.max((u.objects[0].keypoints[0].x - 120.0).abs());
            }
        }
        assert!(spread < 1.0, "{spread}");
    }

    #[test]
    fn time_standing_still_doesnt_break_it() {
        let mut t = Tracker::new(config());
        for _ in 0..5 {
            t.update(&[det(0, 0.9, 100.0, 50.0)], 1.0);
        }
        assert_eq!(t.tracks.len(), 1);
        assert!(t.tracks[0].axes[0].x.is_finite());
    }

    fn controlled(extra: serde_json::Value) -> TrackerConfig {
        let mut control = serde_json::json!({"tracker_frames": 5});
        for (k, v) in extra.as_object().unwrap() {
            control[k] = v.clone();
        }
        serde_json::from_value(serde_json::json!({ "control": control })).unwrap()
    }

    /// A confirmed track from three detections, at 100, 50.
    fn confirmed(c: TrackerConfig) -> (Tracker, f64) {
        let mut t = Tracker::new(c);
        let mut time = 0.0;
        for _ in 0..3 {
            let d = [det(0, 0.9, 100.0, 50.0)];
            t.advance(Some((&d, time)), time);
            time += DT;
        }
        (t, time)
    }

    #[test]
    fn between_control_frames_a_track_is_carried() {
        let (mut t, mut time) = confirmed(controlled(serde_json::json!({})));
        for _ in 0..5 {
            let u = t.advance(None, time);
            assert_eq!(ids(&u), [Some(1)]);
            assert!(u.events.is_empty());
            time += DT;
        }
    }

    #[test]
    fn a_tentative_track_waits_for_the_next_run() {
        let mut t = Tracker::new(controlled(serde_json::json!({})));
        let d = [det(0, 0.9, 100.0, 50.0)];
        t.advance(Some((&d, 0.0)), 0.0);
        assert!(t.wants_detection(0.0), "asks for a run to confirm it");
        for i in 1..4 {
            t.advance(None, i as f64 * DT);
        }
        assert_eq!(t.tracks.len(), 1);
        let u = t.advance(Some((&[], 4.0 * DT)), 4.0 * DT);
        assert!(
            u.objects.is_empty() && t.tracks.is_empty(),
            "the run didn't find it"
        );
    }

    #[test]
    fn a_pose_steers_the_box() {
        let (mut t, mut time) = confirmed(controlled(serde_json::json!({})));
        let start = to_box(&t.tracks[0].axes);
        // The person walks right; the box follows only through the pose.
        let mut x = 120.0;
        for _ in 0..30 {
            t.advance(None, time);
            let mut points = vec![
                Keypoint {
                    x,
                    y: 60.0,
                    confidence: 0.9,
                },
                Keypoint {
                    x: x + 20.0,
                    y: 60.0,
                    confidence: 0.9,
                },
                Keypoint {
                    x,
                    y: 120.0,
                    confidence: 0.9,
                },
                Keypoint {
                    x: x + 20.0,
                    y: 120.0,
                    confidence: 0.9,
                },
            ];
            assert!(t.refine(1, &mut points, time));
            x += 3.0;
            time += DT;
        }
        let now = to_box(&t.tracks[0].axes);
        let moved = (now[0] + now[2] - start[0] - start[2]) / 2.0;
        assert!((moved - 90.0).abs() < 20.0, "{moved}");
        assert!(!t.wants_detection(time - DT));
    }

    #[test]
    fn a_weak_pose_doesnt_steer_and_asks_for_a_run() {
        let (mut t, time) = confirmed(controlled(serde_json::json!({})));
        t.advance(None, time);
        let before = to_box(&t.tracks[0].axes);
        let mut points = vec![
            Keypoint {
                x: 900.0,
                y: 900.0,
                confidence: 0.9,
            },
            Keypoint {
                x: 0.0,
                y: 0.0,
                confidence: 0.1,
            },
            Keypoint {
                x: 0.0,
                y: 0.0,
                confidence: 0.1,
            },
        ];
        assert!(!t.refine(1, &mut points, time));
        assert_eq!(to_box(&t.tracks[0].axes), before);
        assert!(t.wants_detection(time + 1.0));
    }

    #[test]
    fn a_pose_cant_collapse_the_box() {
        let (mut t, time) = confirmed(controlled(serde_json::json!({})));
        t.advance(None, time);
        let before = to_box(&t.tracks[0].axes);
        let mut points = vec![
            Keypoint {
                x: 120.0,
                y: 90.0,
                confidence: 0.9,
            },
            Keypoint {
                x: 121.0,
                y: 91.0,
                confidence: 0.9,
            },
        ];
        assert!(t.refine(1, &mut points, time));
        let after = to_box(&t.tracks[0].axes);
        assert!(after[3] - after[1] >= (before[3] - before[1]) * 0.8);
    }

    #[test]
    fn a_ghost_kept_up_by_its_pose_ends_when_runs_miss_it() {
        let (mut t, mut time) = confirmed(controlled(serde_json::json!({"max_missed": 2})));
        let mut ends = 0;
        for i in 0..4 {
            let dets: [Object; 0] = [];
            let u = t.advance(Some((&dets, time)), time);
            ends += ended(&u);
            if !t.tracks.is_empty() {
                let mut points = vec![
                    Keypoint {
                        x: 120.0,
                        y: 90.0,
                        confidence: 0.9
                    };
                    2
                ];
                t.refine(1, &mut points, time);
            }
            time += DT;
            if i == 1 {
                assert_eq!(ends, 1, "on the second run that misses it");
            }
        }
        assert_eq!(ends, 1);
    }

    #[test]
    fn a_late_detection_matches_where_the_track_was() {
        let (mut t, mut time) = confirmed(controlled(serde_json::json!({})));
        let det_time = time;
        // The track moves fast while the detector works on an older frame.
        let mut x = 100.0;
        for _ in 0..6 {
            x += 12.0;
            t.advance(None, time);
            let mut points = vec![
                Keypoint {
                    x,
                    y: 50.0,
                    confidence: 0.9,
                },
                Keypoint {
                    x: x + 40.0,
                    y: 50.0,
                    confidence: 0.9,
                },
                Keypoint {
                    x,
                    y: 130.0,
                    confidence: 0.9,
                },
                Keypoint {
                    x: x + 40.0,
                    y: 130.0,
                    confidence: 0.9,
                },
            ];
            t.refine(1, &mut points, time);
            time += DT;
        }
        // Where it was when the frame was taken, far from where it is now.
        let then = [det(0, 0.9, 100.0, 50.0)];
        let u = t.advance(Some((&then, det_time)), time);
        assert_eq!(u.objects.len(), 1, "not a second object: {:?}", ids(&u));
        assert_eq!(u.objects[0].tracker_id, Some(1));
        assert!(
            u.objects[0].bbox[0] > 140.0,
            "moved up to now: {:?}",
            u.objects[0].bbox
        );
    }
}
