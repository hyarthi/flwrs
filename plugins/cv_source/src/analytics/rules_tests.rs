use super::*;
use serde_json::json;

const DT: f64 = 0.1;

fn rule(v: serde_json::Value) -> RuleEngine {
    let config: RuleConfig = serde_json::from_value(v).unwrap();
    RuleEngine::new("r", config)
}

/// A 10x10 box whose bottom centre is at (x, y) of a 100x100 frame's 0..1.
fn at(id: u64, x: f32, y: f32) -> Object {
    let (px, py) = (x * 100.0, y * 100.0);
    Object {
        tracker_id: Some(id),
        class_id: 0,
        class_name: Some("person".into()),
        confidence: 0.9,
        bbox: [px - 5.0, py - 10.0, px + 5.0, py],
        keypoints: vec![],
        children: vec![],
    }
}

fn step(r: &mut RuleEngine, objects: &[Object], n: usize) -> Vec<RuleEvent> {
    r.update(&RuleInputs {
        objects,
        classes: &[],
        tracks: &[],
        track_events: &[],
        width: 100.0,
        height: 100.0,
        time: n as f64 * DT,
    })
}

/// Runs a path for track 1; the actions fired, with their frames.
fn walk(r: &mut RuleEngine, path: &[(f32, f32)]) -> Vec<(usize, String)> {
    let mut out = vec![];
    for (n, (x, y)) in path.iter().enumerate() {
        for e in step(r, &[at(1, *x, *y)], n) {
            out.push((n, e.action));
        }
    }
    out
}

fn actions(v: &[(usize, String)]) -> Vec<&str> {
    v.iter().map(|(_, a)| a.as_str()).collect()
}

fn line_rule(extra: serde_json::Value) -> RuleEngine {
    let mut v = json!({"rule": {"type": "line_crossing", "line": [[0.5, 0.0], [0.5, 1.0]]}});
    for (k, x) in extra.as_object().unwrap() {
        v[k] = x.clone();
    }
    rule(v)
}

fn square() -> serde_json::Value {
    json!([[0.4, 0.4], [0.6, 0.4], [0.6, 0.6], [0.4, 0.6]])
}

#[test]
fn geometry() {
    let z = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
    assert!(in_zone(&z, [0.5, 0.5]).0);
    assert!(!in_zone(&z, [1.5, 0.5]).0);
    assert!((in_zone(&z, [0.9, 0.5]).1 - 0.1).abs() < 1e-5);
    let l = [[0.0, 0.0], [1.0, 0.0]];
    assert!(
        line_side(&l, [0.5, 0.2]).0,
        "y is down: to the right of a line going right"
    );
    assert!(!line_side(&l, [0.5, -0.2]).0);
    assert!(line_crossed(&l, [0.5, -0.1], [0.5, 0.1]));
    assert!(!line_crossed(&l, [1.5, -0.1], [1.5, 0.1]));
}

#[test]
fn a_line_crossing_fires_once_with_its_direction() {
    let mut r = line_rule(json!({}));
    let right: Vec<_> = (0..10).map(|i| (0.3 + i as f32 * 0.05, 0.5)).collect();
    assert_eq!(actions(&walk(&mut r, &right)), ["out"]);
    let mut r = line_rule(json!({}));
    let left: Vec<_> = (0..10).map(|i| (0.7 - i as f32 * 0.05, 0.5)).collect();
    assert_eq!(actions(&walk(&mut r, &left)), ["in"]);
}

#[test]
fn a_direction_filter_keeps_only_that_way() {
    let mut r = rule(
        json!({"rule": {"type": "line_crossing", "line": [[0.5, 0.0], [0.5, 1.0]], "direction": "in"}}),
    );
    let there_and_back: Vec<_> = [0.3, 0.4, 0.6, 0.7, 0.6, 0.4, 0.3]
        .iter()
        .map(|x| (*x, 0.5))
        .collect();
    assert_eq!(actions(&walk(&mut r, &there_and_back)), ["in"]);
}

#[test]
fn jitter_on_a_line_doesnt_flicker_with_hysteresis() {
    let path: Vec<_> = (0..40)
        .map(|i| (if i % 2 == 0 { 0.49 } else { 0.51 }, 0.5))
        .collect();
    let mut plain = line_rule(json!({}));
    assert!(
        walk(&mut plain, &path).len() > 10,
        "without a margin it flickers"
    );
    let mut steady = line_rule(json!({"hysteresis": 0.05}));
    assert!(walk(&mut steady, &path).is_empty());
}

#[test]
fn a_line_needs_a_real_crossing() {
    let mut r = rule(json!({"rule": {"type": "line_crossing", "line": [[0.5, 0.4], [0.5, 0.6]]}}));
    // Around the short line's end.
    let path = [(0.3, 0.5), (0.3, 0.2), (0.7, 0.2), (0.7, 0.5)];
    assert!(walk(&mut r, &path).is_empty());
}

#[test]
fn line_debounce_ignores_a_brief_step_across() {
    let mut r = line_rule(json!({"debounce_s": 0.25}));
    let path = [(0.3, 0.5), (0.6, 0.5), (0.3, 0.5), (0.3, 0.5)];
    assert!(walk(&mut r, &path).is_empty());
    let mut r = line_rule(json!({"debounce_s": 0.25}));
    let path = [
        (0.3, 0.5),
        (0.6, 0.5),
        (0.6, 0.5),
        (0.6, 0.5),
        (0.6, 0.5),
        (0.6, 0.5),
    ];
    let got = walk(&mut r, &path);
    assert_eq!(actions(&got), ["out"]);
    assert_eq!(got[0].0, 4, "after holding for the debounce");
}

#[test]
fn line_cooldown_limits_one_track() {
    let mut r = line_rule(json!({"cooldown_s": 1.0}));
    let path: Vec<_> = (0..8)
        .map(|i| (if i % 2 == 0 { 0.3 } else { 0.7 }, 0.5))
        .collect();
    assert_eq!(walk(&mut r, &path).len(), 1);
}

#[test]
fn a_zone_fires_on_enter_and_exit() {
    let mut r = rule(json!({"rule": {"type": "zone", "zone": square()}}));
    let path = [
        (0.2, 0.5),
        (0.3, 0.5),
        (0.5, 0.5),
        (0.5, 0.5),
        (0.8, 0.5),
        (0.9, 0.5),
    ];
    assert_eq!(actions(&walk(&mut r, &path)), ["enter", "exit"]);
}

#[test]
fn a_zone_can_watch_one_side_and_starts_silent() {
    let mut r = rule(json!({"rule": {"type": "zone", "zone": square(), "on": "exit"}}));
    // Already inside on the first frame: no enter.
    let path = [(0.5, 0.5), (0.8, 0.5), (0.5, 0.5), (0.8, 0.5)];
    assert_eq!(actions(&walk(&mut r, &path)), ["exit", "exit"]);
}

#[test]
fn zone_hysteresis_holds_at_the_edge() {
    let mut r = rule(json!({"hysteresis": 0.03, "rule": {"type": "zone", "zone": square()}}));
    let mut path = vec![(0.2, 0.5), (0.5, 0.5)];
    path.extend((0..20).map(|i| (if i % 2 == 0 { 0.59 } else { 0.61 }, 0.5)));
    assert_eq!(actions(&walk(&mut r, &path)), ["enter"]);
}

#[test]
fn loitering_fires_once_per_stay() {
    let mut r = rule(json!({"rule": {"type": "loitering", "zone": square(), "seconds": 0.5}}));
    let mut path = vec![(0.2, 0.5)];
    path.extend(std::iter::repeat_n((0.5, 0.5), 12));
    path.push((0.8, 0.5));
    path.extend(std::iter::repeat_n((0.5, 0.5), 3));
    let got = walk(&mut r, &path);
    assert_eq!(actions(&got), ["exceeded"]);
}

#[test]
fn counting_has_hysteresis() {
    let mut r = rule(json!({"hysteresis": 1.0, "rule": {"type": "counting", "above": 3}}));
    let group = |n: u64| {
        (0..n)
            .map(|i| at(i + 1, 0.1 * i as f32, 0.5))
            .collect::<Vec<_>>()
    };
    let counts = [1, 2, 3, 2, 3, 2, 3, 1, 3];
    let mut fired = vec![];
    for (n, c) in counts.iter().enumerate() {
        for e in step(&mut r, &group(*c), n) {
            fired.push((n, e.action, e.count));
        }
    }
    assert_eq!(
        fired,
        [
            (2, "above".to_string(), Some(3)),
            (8, "above".to_string(), Some(3))
        ],
        "2 is within the margin; only 1 re-arms it"
    );
}

#[test]
fn counting_below_and_in_a_zone() {
    let mut r = rule(json!({"rule": {"type": "counting", "zone": square(), "below": 0}}));
    let inside = [at(1, 0.5, 0.5)];
    let outside = [at(1, 0.9, 0.5)];
    assert!(step(&mut r, &inside, 0).is_empty());
    let e = step(&mut r, &outside, 1);
    assert_eq!((e[0].action.as_str(), e[0].count), ("below", Some(0)));
}

fn info(id: u64, age_s: f32) -> TrackInfo {
    TrackInfo {
        id,
        class_id: 0,
        class_name: Some("person".into()),
        bbox: [0.0; 4],
        velocity: [0.0; 2],
        age_s,
        lost: false,
    }
}

#[test]
fn track_events_wait_for_the_minimum_age() {
    let mut r = rule(json!({"rule": {"type": "track", "min_age_s": 1.0}}));
    let mut run = |t: Vec<TrackInfo>, ev: Vec<TrackEvent>, n: usize| {
        r.update(&RuleInputs {
            objects: &[],
            classes: &[],
            tracks: &t,
            track_events: &ev,
            width: 100.0,
            height: 100.0,
            time: n as f64,
        })
    };
    assert!(run(vec![info(1, 0.5)], vec![], 0).is_empty());
    assert_eq!(run(vec![info(1, 1.0)], vec![], 1)[0].action, "started");
    assert!(run(vec![info(1, 2.0)], vec![], 2).is_empty());
    let ended = run(vec![], vec![TrackEvent::Ended(info(1, 3.0))], 3);
    assert_eq!(ended[0].action, "ended");
    // Too short to have been announced: it never happened.
    assert!(run(vec![info(2, 0.1)], vec![], 4).is_empty());
    assert!(run(vec![], vec![TrackEvent::Ended(info(2, 0.2))], 5).is_empty());
}

#[test]
fn a_label_change_must_hold() {
    let mut r = rule(json!({"debounce_s": 0.25, "rule": {"type": "label_changed"}}));
    let named = |name: &str| {
        let mut o = at(1, 0.5, 0.5);
        o.class_name = Some(name.into());
        o
    };
    let seq = ["open", "shut", "open", "shut", "shut", "shut", "shut"];
    let mut got = vec![];
    for (n, s) in seq.iter().enumerate() {
        for e in step(&mut r, &[named(s)], n) {
            got.push((n, e.previous.unwrap(), e.label.unwrap()));
        }
    }
    assert_eq!(got, [(6, "open".to_string(), "shut".to_string())]);
}

#[test]
fn a_label_can_come_from_a_model_on_the_object() {
    let mut r = rule(json!({"rule": {"type": "label_changed", "from": "gesture"}}));
    let with = |name: &str| {
        let mut o = at(1, 0.5, 0.5);
        o.children = vec![(
            "gesture".to_string(),
            Results::Classes(vec![Class {
                class_id: 1,
                class_name: Some(name.into()),
                confidence: 0.9,
            }]),
        )];
        o
    };
    assert!(step(&mut r, &[with("fist")], 0).is_empty());
    assert_eq!(
        step(&mut r, &[with("palm")], 1)[0].label.as_deref(),
        Some("palm")
    );
}

#[test]
fn a_score_fires_on_the_rise_with_hysteresis() {
    let mut r = rule(json!({"hysteresis": 0.1, "rule": {"type": "score_above", "threshold": 0.8}}));
    let scored = |c: f32| {
        let mut o = at(1, 0.5, 0.5);
        o.confidence = c;
        o
    };
    let seq = [0.5, 0.85, 0.75, 0.85, 0.75, 0.6, 0.85];
    let mut hits = vec![];
    for (n, s) in seq.iter().enumerate() {
        if !step(&mut r, &[scored(*s)], n).is_empty() {
            hits.push(n);
        }
    }
    assert_eq!(hits, [1, 6]);
}

#[test]
fn whole_frame_classes_can_drive_a_rule() {
    let mut r = rule(
        json!({"input": "classes", "rule": {"type": "score_above", "threshold": 0.5, "class": "fire"}}),
    );
    let classes = |p: f32| {
        vec![Class {
            class_id: 3,
            class_name: Some("fire".into()),
            confidence: p,
        }]
    };
    let mut run = |p: f32, n: usize| {
        r.update(&RuleInputs {
            objects: &[],
            classes: &classes(p),
            tracks: &[],
            track_events: &[],
            width: 1.0,
            height: 1.0,
            time: n as f64,
        })
    };
    assert!(run(0.2, 0).is_empty());
    let e = run(0.9, 1);
    assert_eq!(
        (e[0].label.as_deref(), e[0].tracker_id),
        (Some("fire"), None)
    );
}

#[test]
fn classes_filter_what_a_rule_sees() {
    let mut r = rule(json!({"classes": ["car"], "rule": {"type": "zone", "zone": square()}}));
    assert!(step(&mut r, &[at(1, 0.2, 0.5)], 0).is_empty());
    assert!(
        step(&mut r, &[at(1, 0.5, 0.5)], 1).is_empty(),
        "a person isn't a car"
    );
}

#[test]
fn objects_without_a_track_are_ignored_by_track_rules() {
    let mut r = line_rule(json!({}));
    let mut o = at(1, 0.3, 0.5);
    o.tracker_id = None;
    step(&mut r, &[o.clone()], 0);
    o.bbox[0] += 40.0;
    o.bbox[2] += 40.0;
    assert!(step(&mut r, &[o], 1).is_empty());
    assert!(r.remembered().is_empty());
}

#[test]
fn a_finished_track_is_forgotten() {
    let mut r = line_rule(json!({}));
    step(&mut r, &[at(7, 0.3, 0.5)], 0);
    assert!(r.remembered().contains(&7));
    r.update(&RuleInputs {
        objects: &[],
        classes: &[],
        tracks: &[],
        track_events: &[TrackEvent::Ended(info(7, 1.0))],
        width: 100.0,
        height: 100.0,
        time: 1.0,
    });
    assert!(r.remembered().is_empty());
    assert_eq!(
        ended_ids(&[TrackEvent::Ended(info(7, 1.0))]),
        BTreeSet::from([7])
    );
}
