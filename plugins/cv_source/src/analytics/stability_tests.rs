//! Noisy recorded-style sequences through the tracker and rules: what a
//! camera really gives (jitter, dropped detections, wavering scores) must
//! come out as one event per thing that happened.

use super::rules::{RuleEngine, RuleEvent, RuleInputs};
use super::track::{TrackUpdate, Tracker};
use crate::graph::config::RuleConfig;
use crate::results::Object;
use serde_json::json;

const FPS: f64 = 30.0;
const SEEDS: u64 = 25;

/// A small deterministic generator.
struct Noise(u64);

impl Noise {
    fn next(&mut self) -> f32 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.0 >> 40) as f32) / (1u64 << 24) as f32
    }

    fn around(&mut self, spread: f32) -> f32 {
        (self.next() - 0.5) * 2.0 * spread
    }
}

/// A person at (x, y) of a 640x480 frame (the bottom centre), as a detector
/// sees them: jittering, wavering, sometimes missed.
fn seen(n: &mut Noise, x: f32, y: f32) -> Vec<Object> {
    if n.next() < 0.12 {
        return vec![];
    }
    let (px, py) = (x * 640.0 + n.around(4.0), y * 480.0 + n.around(4.0));
    let (w, h) = (60.0 + n.around(4.0), 160.0 + n.around(6.0));
    vec![Object {
        tracker_id: None,
        class_id: 0,
        class_name: Some("person".into()),
        confidence: 0.72 + n.around(0.2),
        bbox: [px - w / 2.0, py - h, px + w / 2.0, py],
        keypoints: vec![],
        children: vec![],
    }]
}

fn run(
    rule: serde_json::Value,
    seed: u64,
    path: impl Fn(f32) -> (f32, f32),
    frames: usize,
) -> Vec<RuleEvent> {
    let tracker = serde_json::from_value(json!({})).unwrap();
    let mut tracker = Tracker::new(tracker);
    let config: RuleConfig = serde_json::from_value(rule).unwrap();
    let mut engine = RuleEngine::new("r", config);
    let mut noise = Noise(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0x5555);
    let mut out = vec![];
    for f in 0..frames {
        let (x, y) = path(f as f32 / frames as f32);
        let time = f as f64 / FPS;
        let TrackUpdate {
            objects,
            tracks,
            events,
        } = tracker.update(&seen(&mut noise, x, y), time);
        out.extend(engine.update(&RuleInputs {
            objects: &objects,
            classes: &[],
            tracks: &tracks,
            track_events: &events,
            width: 640.0,
            height: 480.0,
            time,
        }));
    }
    out
}

const STEADY: (f32, f32) = (0.15, 0.015);

#[test]
fn walking_across_a_line_is_one_crossing() {
    for seed in 0..SEEDS {
        let events = run(
            json!({"hysteresis": STEADY.1, "debounce_s": STEADY.0,
                   "rule": {"type": "line_crossing", "line": [[0.5, 0.0], [0.5, 1.0]]}}),
            seed,
            |t| (0.2 + 0.6 * t, 0.8),
            150,
        );
        assert_eq!(events.len(), 1, "seed {seed}: {events:?}");
        assert_eq!(events[0].action, "out");
    }
}

#[test]
fn standing_on_a_line_never_fires() {
    for seed in 0..SEEDS {
        let events = run(
            json!({"hysteresis": 0.02, "debounce_s": STEADY.0,
                   "rule": {"type": "line_crossing", "line": [[0.5, 0.0], [0.5, 1.0]]}}),
            seed,
            |_| (0.5, 0.8),
            300,
        );
        assert!(events.is_empty(), "seed {seed}: {events:?}");
    }
}

#[test]
fn a_visit_to_a_zone_is_one_enter_and_one_exit() {
    for seed in 0..SEEDS {
        let events = run(
            json!({"hysteresis": STEADY.1, "debounce_s": STEADY.0,
                   "rule": {"type": "zone", "zone": [[0.4, 0.5], [0.6, 0.5], [0.6, 1.0], [0.4, 1.0]]}}),
            seed,
            |t| (0.2 + 0.6 * t, 0.8),
            210,
        );
        let actions: Vec<&str> = events.iter().map(|e| e.action.as_str()).collect();
        assert_eq!(actions, ["enter", "exit"], "seed {seed}");
    }
}

#[test]
fn someone_staying_keeps_one_track_and_one_start() {
    for seed in 0..SEEDS {
        let events = run(
            json!({"rule": {"type": "track", "on": "started", "min_age_s": 0.3}}),
            seed,
            |_| (0.5, 0.8),
            300,
        );
        assert_eq!(events.len(), 1, "seed {seed}: {events:?}");
        assert_eq!(events[0].tracker_id, Some(1), "seed {seed}");
    }
}

#[test]
fn loitering_is_one_event_despite_missed_frames() {
    for seed in 0..SEEDS {
        let events = run(
            json!({"debounce_s": 0.3, "rule": {"type": "loitering",
                   "zone": [[0.3, 0.5], [0.7, 0.5], [0.7, 1.0], [0.3, 1.0]], "seconds": 2.0}}),
            seed,
            |_| (0.5, 0.8),
            240,
        );
        assert_eq!(events.len(), 1, "seed {seed}: {events:?}");
        assert!(events[0].duration_s.unwrap() >= 2.0);
    }
}

/// One gesture, one event: a hand held up, on a noisy pose, in Lua.
#[cfg(feature = "lua")]
#[test]
fn a_raised_hand_is_one_event_per_gesture() {
    use super::script::{LuaRule, ScriptInput};
    use crate::graph::config::LuaRuleConfig;
    use crate::keypoints;
    use crate::results::Keypoint;
    let example: serde_json::Value =
        serde_json::from_str(include_str!("../../examples/graphs/pose_hand_raised.json")).unwrap();
    let script = example["graph"]["nodes"]["hand_raised"]["script"]
        .as_str()
        .unwrap();
    let layout = keypoints::layout("coco17").unwrap();
    let at = |name: &str| layout.points.iter().position(|p| p == name).unwrap();
    let (wrist, shoulder) = (at("left_wrist"), at("left_shoulder"));
    let smoothing = json!({"smooth_keypoints": {}});
    for seed in 0..SEEDS {
        let mut noise = Noise(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xAAAA);
        let mut tracker = Tracker::new(serde_json::from_value(smoothing.clone()).unwrap());
        let config: LuaRuleConfig = serde_json::from_value(json!({"script": script})).unwrap();
        let mut rule =
            LuaRule::new("hand", &config, Some(layout.clone()), Default::default()).unwrap();
        let mut fired = vec![];
        // Down 2 s, up 2 s, down 2 s, up 2 s, down 1 s.
        for f in 0..270 {
            let time = f as f64 / FPS;
            let up = (60..120).contains(&f) || (180..240).contains(&f);
            let mut dets = seen(&mut noise, 0.5, 0.8);
            for o in &mut dets {
                let mut points = vec![
                    Keypoint {
                        x: 320.0,
                        y: 300.0,
                        confidence: 0.1
                    };
                    layout.points.len()
                ];
                points[shoulder] = Keypoint {
                    x: 300.0 + noise.around(2.0),
                    y: 260.0 + noise.around(3.0),
                    confidence: 0.8,
                };
                let wrist_y = if up { 210.0 } else { 330.0 };
                points[wrist] = Keypoint {
                    x: 290.0 + noise.around(3.0),
                    y: wrist_y + noise.around(6.0),
                    confidence: 0.4 + noise.next() * 0.5,
                };
                o.keypoints = points;
            }
            let u = tracker.update(&dets, time);
            let events = rule
                .update(&ScriptInput {
                    objects: &u.objects,
                    tracks: &u.tracks,
                    track_events: &u.events,
                    seq: f as u64,
                    width: 640,
                    height: 480,
                    time,
                })
                .unwrap();
            fired.extend(events.into_iter().map(|e| (f, e.event, e.action)));
        }
        assert_eq!(fired.len(), 2, "seed {seed}: {fired:?}");
        assert!(
            (60..120).contains(&fired[0].0) && (180..240).contains(&fired[1].0),
            "seed {seed}: {fired:?}"
        );
    }
}
