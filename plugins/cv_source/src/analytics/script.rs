//! Lua rules: `function(frame, objects, tracks, state)`, in the sandbox the
//! rest of flwrs uses. Returns nothing, an event (a table) or a list of them.
//!
//! Coordinates are frame pixels, so distances and angles aren't skewed by
//! the frame's shape. `state` is the rule's own table across frames, and
//! `state.tracks[id]` is dropped when that track ends.

use super::pose;
use super::rules::RuleEvent;
use super::track::{TrackEvent, TrackInfo};
use crate::graph::config::{LuaLimits, LuaRuleConfig};
use crate::keypoints::KeypointLayout;
use crate::results::{Class, Keypoint, Object, Results};
use flwrs_types::lua::mlua::{self, Lua, Table, Value};
use flwrs_types::lua::{self, Arg, Chunk, Limits, Runtime};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

/// The parameters of a rule's code.
pub const PARAMS: [&str; 4] = ["frame", "objects", "tracks", "state"];

/// Remembered `held` / `once` keys before the stale ones are swept.
const SWEEP_ABOVE: usize = 4096;
/// A key not asked about for this long is forgotten.
const FORGET_AFTER_S: f64 = 60.0;
/// The most keys a rule may keep.
const MAX_KEYS: usize = 100_000;

#[derive(Default)]
struct Timers {
    now: f64,
    held: HashMap<String, (f64, f64)>,
    once: HashMap<String, (bool, Option<f64>, f64)>,
}

impl Timers {
    fn sweep(&mut self) {
        let now = self.now;
        if self.held.len() > SWEEP_ABOVE {
            self.held.retain(|_, v| now - v.1 < FORGET_AFTER_S);
        }
        if self.once.len() > SWEEP_ABOVE {
            self.once.retain(|_, v| now - v.2 < FORGET_AFTER_S);
        }
    }

    fn full(&self) -> bool {
        self.held.len() + self.once.len() >= MAX_KEYS
    }
}

fn point(v: &Value) -> mlua::Result<(f32, f32)> {
    let Value::Table(t) = v else {
        return Err(mlua::Error::runtime(
            "expected a point (a table with x and y)",
        ));
    };
    Ok((t.get("x")?, t.get("y")?))
}

fn key_text(key: &Value) -> mlua::Result<String> {
    match key {
        Value::String(s) => Ok(s.to_str()?.to_string()),
        Value::Integer(n) => Ok(n.to_string()),
        Value::Number(n) => Ok(n.to_string()),
        _ => Err(mlua::Error::runtime("a key is a string or a number")),
    }
}

/// `pose.angle`, `pose.dist`, `pose.torso`, `pose.visible`.
fn pose_library(lua: &Lua) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.raw_set(
        "angle",
        lua.create_function(|_, (a, b, c): (Value, Value, Value)| {
            Ok(pose::angle(point(&a)?, point(&b)?, point(&c)?))
        })?,
    )?;
    t.raw_set(
        "dist",
        lua.create_function(|_, (a, b): (Value, Value)| {
            Ok(pose::distance(point(&a)?, point(&b)?))
        })?,
    )?;
    t.raw_set(
        "visible",
        lua.create_function(|_, (p, min): (Value, Option<f32>)| {
            let Value::Table(p) = p else {
                return Ok(false);
            };
            let c: Option<f32> = p.get("confidence")?;
            Ok(c.unwrap_or(0.0) >= min.unwrap_or(pose::MIN_VISIBLE))
        })?,
    )?;
    t.raw_set(
        "torso",
        lua.create_function(|_, o: Table| {
            let Ok(kp) = o.get::<Table>("kp") else {
                return Ok(None);
            };
            let at = |name: &str| -> mlua::Result<Option<(f32, f32)>> {
                match kp.get::<Value>(name)? {
                    Value::Table(p)
                        if p.get::<Option<f32>>("confidence")?.unwrap_or(0.0)
                            >= pose::MIN_VISIBLE =>
                    {
                        Ok(Some((p.get("x")?, p.get("y")?)))
                    }
                    _ => Ok(None),
                }
            };
            let (a, b, c, d) = (
                at("left_shoulder")?,
                at("right_shoulder")?,
                at("left_hip")?,
                at("right_hip")?,
            );
            Ok(match (a, b, c, d) {
                (Some(a), Some(b), Some(c), Some(d)) => Some(pose::torso_length([a, b], [c, d])),
                _ => None,
            })
        })?,
    )?;
    Ok(t)
}

fn install(lua: &Lua, globals: &Table, timers: Arc<Mutex<Timers>>) -> mlua::Result<()> {
    globals.raw_set("pose", pose_library(lua)?)?;
    // `held(key, condition, seconds)`: true once it's been true that long.
    let t = timers.clone();
    globals.raw_set(
        "held",
        lua.create_function(move |_, (key, on, seconds): (Value, bool, f64)| {
            let key = key_text(&key)?;
            let mut timers = t.lock().unwrap_or_else(PoisonError::into_inner);
            let now = timers.now;
            if !on {
                timers.held.remove(&key);
                return Ok(false);
            }
            if !timers.held.contains_key(&key) && timers.full() {
                return Err(mlua::Error::runtime("too many held / once keys"));
            }
            let entry = timers.held.entry(key).or_insert((now, now));
            entry.1 = now;
            Ok(now - entry.0 >= seconds)
        })?,
    )?;
    // `once(key, condition, cooldown?)`: true on the frame it becomes true.
    let t = timers;
    globals.raw_set(
        "once",
        lua.create_function(move |_, (key, on, cooldown): (Value, bool, Option<f64>)| {
            let key = key_text(&key)?;
            let mut timers = t.lock().unwrap_or_else(PoisonError::into_inner);
            let now = timers.now;
            if !timers.once.contains_key(&key) && timers.full() {
                return Err(mlua::Error::runtime("too many held / once keys"));
            }
            let entry = timers.once.entry(key).or_insert((false, None, now));
            entry.2 = now;
            let rose = on && !entry.0;
            entry.0 = on;
            if !rose
                || entry
                    .1
                    .is_some_and(|last| now - last < cooldown.unwrap_or(0.0))
            {
                return Ok(false);
            }
            entry.1 = Some(now);
            Ok(true)
        })?,
    )?;
    Ok(())
}

fn box_table(lua: &Lua, b: [f32; 4]) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    for (k, v) in ["x1", "y1", "x2", "y2"].iter().zip(b) {
        t.raw_set(*k, v)?;
    }
    Ok(t)
}

fn point_table(lua: &Lua, k: &Keypoint) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.raw_set("x", k.x)?;
    t.raw_set("y", k.y)?;
    t.raw_set("confidence", k.confidence)?;
    Ok(t)
}

fn classes_table(lua: &Lua, classes: &[Class]) -> mlua::Result<Table> {
    let list = lua.create_table()?;
    for (i, c) in classes.iter().enumerate() {
        let t = lua.create_table()?;
        t.raw_set("class_id", c.class_id)?;
        t.raw_set("class_name", c.class_name.as_deref())?;
        t.raw_set("confidence", c.confidence)?;
        list.raw_set(i + 1, t)?;
    }
    Ok(list)
}

fn object_table(
    lua: &Lua,
    o: &Object,
    layout: Option<&KeypointLayout>,
    layouts: &BTreeMap<String, KeypointLayout>,
) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.raw_set("class_id", o.class_id)?;
    t.raw_set("class_name", o.class_name.as_deref())?;
    t.raw_set("confidence", o.confidence)?;
    t.raw_set("tracker_id", o.tracker_id.map(|i| i as i64))?;
    t.raw_set("box", box_table(lua, o.bbox)?)?;
    let (list, named) = (lua.create_table()?, lua.create_table()?);
    let names = layout.filter(|l| l.points.len() == o.keypoints.len());
    for (i, k) in o.keypoints.iter().enumerate() {
        let p = point_table(lua, k)?;
        if let Some(l) = names {
            named.raw_set(l.points[i].as_str(), p.clone())?;
        }
        list.raw_set(i + 1, p)?;
    }
    t.raw_set("keypoints", list)?;
    t.raw_set("kp", named)?;
    let children = lua.create_table()?;
    for (node, r) in &o.children {
        let value = match r {
            Results::Classes(c) => Value::Table(classes_table(lua, c)?),
            Results::Text(Some(x)) => Value::String(lua.create_string(&x.text)?),
            Results::Objects(list) => {
                let out = lua.create_table()?;
                for (i, c) in list.iter().enumerate() {
                    out.raw_set(i + 1, object_table(lua, c, layouts.get(node), layouts)?)?;
                }
                Value::Table(out)
            }
            _ => continue,
        };
        children.raw_set(node.as_str(), value)?;
    }
    t.raw_set("children", children)?;
    Ok(t)
}

fn track_table(lua: &Lua, x: &TrackInfo) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.raw_set("id", x.id as i64)?;
    t.raw_set("class_id", x.class_id)?;
    t.raw_set("class_name", x.class_name.as_deref())?;
    t.raw_set("box", box_table(lua, x.bbox)?)?;
    t.raw_set("vx", x.velocity[0])?;
    t.raw_set("vy", x.velocity[1])?;
    t.raw_set("age", x.age_s)?;
    t.raw_set("lost", x.lost)?;
    Ok(t)
}

/// A frame's facts for a rule.
pub struct ScriptInput<'a> {
    pub objects: &'a [Object],
    pub tracks: &'a [TrackInfo],
    pub track_events: &'a [TrackEvent],
    pub seq: u64,
    pub width: u32,
    pub height: u32,
    pub time: f64,
}

/// A Lua rule node.
pub struct LuaRule {
    node: String,
    name: String,
    runtime: Runtime,
    chunk: Chunk,
    state: Table,
    timers: Arc<Mutex<Timers>>,
    layout: Option<KeypointLayout>,
    /// By model node: names the keypoints of what ran on an object.
    layouts: BTreeMap<String, KeypointLayout>,
}

fn limits(l: &LuaLimits) -> Limits {
    let d = Limits::default();
    Limits {
        max_instructions: l.max_instructions.unwrap_or(d.max_instructions),
        timeout: l.timeout_ms.map_or(d.timeout, Duration::from_millis),
        max_memory_bytes: l.max_memory_bytes.unwrap_or(d.max_memory_bytes),
    }
}

impl LuaRule {
    pub fn new(
        node: &str,
        config: &LuaRuleConfig,
        layout: Option<KeypointLayout>,
        layouts: BTreeMap<String, KeypointLayout>,
    ) -> Result<Self, String> {
        let timers = Arc::new(Mutex::new(Timers::default()));
        let shared = timers.clone();
        let runtime = Runtime::with_host(limits(&config.limits), &move |lua, globals| {
            install(lua, globals, shared.clone())
        })?;
        let chunk = runtime.chunk_with(node, &config.script, &PARAMS)?;
        let state = runtime.lua().create_table().map_err(|e| e.to_string())?;
        Ok(Self {
            node: node.to_string(),
            name: config.name.clone().unwrap_or_else(|| node.to_string()),
            runtime,
            chunk,
            state,
            timers,
            layout,
            layouts,
        })
    }

    fn event(&self, t: &Table, time: f64) -> Result<RuleEvent, String> {
        let text = |e: mlua::Error| format!("a returned event: {e}");
        let id: Option<i64> = t.get("tracker_id").map_err(text)?;
        let data = match t.get::<Value>("data").map_err(text)? {
            Value::Nil => None,
            v => {
                let value = lua::from_lua_lossless(v)?;
                Some(flwrs_types::json::to_json(&value)?)
            }
        };
        Ok(RuleEvent {
            node: self.node.clone(),
            rule: self.name.clone(),
            event: t
                .get::<Option<String>>("event")
                .map_err(text)?
                .unwrap_or_else(|| "custom".into()),
            action: t
                .get::<Option<String>>("action")
                .map_err(text)?
                .unwrap_or_else(|| "fired".into()),
            time,
            tracker_id: id.and_then(|i| u64::try_from(i).ok()),
            class_name: t.get("class_name").map_err(text)?,
            bbox: None,
            count: t.get("count").map_err(text)?,
            duration_s: t.get("duration").map_err(text)?,
            score: t.get("score").map_err(text)?,
            label: t.get("label").map_err(text)?,
            previous: t.get("previous").map_err(text)?,
            data,
        })
    }

    /// Runs the rule on a frame.
    pub fn update(&mut self, i: &ScriptInput) -> Result<Vec<RuleEvent>, String> {
        {
            let mut timers = self.timers.lock().unwrap_or_else(PoisonError::into_inner);
            timers.now = i.time;
            timers.sweep();
        }
        let describe = |e: mlua::Error| e.to_string();
        // A finished track's own state goes with it.
        if let Ok(per_track) = self.state.get::<Table>("tracks") {
            for e in i.track_events {
                if let TrackEvent::Ended(t) = e {
                    per_track
                        .raw_set(t.id as i64, Value::Nil)
                        .map_err(describe)?;
                }
            }
        }
        let lua = self.runtime.lua();
        let frame = lua.create_table().map_err(describe)?;
        frame.raw_set("seq", i.seq as i64).map_err(describe)?;
        frame.raw_set("time", i.time).map_err(describe)?;
        frame.raw_set("width", i.width).map_err(describe)?;
        frame.raw_set("height", i.height).map_err(describe)?;
        let objects = lua.create_table().map_err(describe)?;
        for (n, o) in i.objects.iter().enumerate() {
            let t = object_table(lua, o, self.layout.as_ref(), &self.layouts).map_err(describe)?;
            objects.raw_set(n + 1, t).map_err(describe)?;
        }
        let tracks = lua.create_table().map_err(describe)?;
        for (n, x) in i.tracks.iter().enumerate() {
            let t = track_table(lua, x).map_err(describe)?;
            tracks.raw_set(n + 1, t).map_err(describe)?;
        }
        let results = self.runtime.call_with(
            &self.chunk,
            vec![
                Arg::Lua(Value::Table(frame)),
                Arg::Lua(Value::Table(objects)),
                Arg::Lua(Value::Table(tracks)),
                Arg::Lua(Value::Table(self.state.clone())),
            ],
        )?;
        let Some(Value::Table(result)) = results.into_iter().next() else {
            return Ok(vec![]);
        };
        // One event, or a list of them.
        if result.raw_len() == 0 {
            // An empty table is an empty list.
            if result.pairs::<Value, Value>().next().is_none() {
                return Ok(vec![]);
            }
            return Ok(vec![self.event(&result, i.time)?]);
        }
        let mut out = vec![];
        for item in result.sequence_values::<Value>() {
            match item.map_err(describe)? {
                Value::Table(t) => out.push(self.event(&t, i.time)?),
                other => {
                    return Err(format!(
                        "a returned event is a table, not {}",
                        other.type_name()
                    ));
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keypoints;

    fn config(script: &str) -> LuaRuleConfig {
        serde_json::from_value(serde_json::json!({ "script": script })).unwrap()
    }

    fn person(wrist_y: f32) -> Object {
        let names = keypoints::layout("coco17").unwrap().points;
        let mut kps = vec![
            Keypoint {
                x: 50.0,
                y: 50.0,
                confidence: 0.9
            };
            names.len()
        ];
        let at = |n: &str| names.iter().position(|p| p == n).unwrap();
        kps[at("left_wrist")].y = wrist_y;
        kps[at("left_shoulder")] = Keypoint {
            x: 40.0,
            y: 40.0,
            confidence: 0.9,
        };
        kps[at("right_shoulder")] = Keypoint {
            x: 60.0,
            y: 40.0,
            confidence: 0.9,
        };
        kps[at("left_hip")] = Keypoint {
            x: 40.0,
            y: 80.0,
            confidence: 0.9,
        };
        kps[at("right_hip")] = Keypoint {
            x: 60.0,
            y: 80.0,
            confidence: 0.9,
        };
        Object {
            tracker_id: Some(3),
            class_id: 0,
            class_name: Some("person".into()),
            confidence: 0.9,
            bbox: [30.0, 10.0, 70.0, 100.0],
            keypoints: kps,
            children: vec![],
        }
    }

    fn input<'a>(objects: &'a [Object], time: f64) -> ScriptInput<'a> {
        ScriptInput {
            objects,
            tracks: &[],
            track_events: &[],
            seq: 0,
            width: 100,
            height: 100,
            time,
        }
    }

    const HAND_RAISED: &str = r#"
        local out = {}
        for _, o in ipairs(objects) do
            local w, s = o.kp.left_wrist, o.kp.left_shoulder
            local up = pose.visible(w) and pose.visible(s) and w.y < s.y
            if once(o.tracker_id, held(o.tracker_id, up, 0.5)) then
                out[#out + 1] = {event = "hand_raised", action = "left", tracker_id = o.tracker_id,
                    data = {torso = pose.torso(o)}}
            end
        end
        return out
    "#;

    #[test]
    fn hand_raised_fires_once_after_holding() {
        let layout = keypoints::layout("coco17");
        let mut r = LuaRule::new("hand", &config(HAND_RAISED), layout, BTreeMap::new()).unwrap();
        let mut fired = vec![];
        // Down, a brief flicker up, down, held up, then down and up again.
        let ys = [60, 20, 60, 20, 20, 20, 20, 20, 20, 20, 60, 60, 20];
        for (n, y) in ys.iter().enumerate() {
            let objects = [person(*y as f32)];
            for e in r.update(&input(&objects, n as f64 * 0.1)).unwrap() {
                fired.push((n, e));
            }
        }
        assert_eq!(fired.len(), 1, "{fired:?}");
        let (n, e) = &fired[0];
        assert_eq!(*n, 8);
        assert_eq!(
            (e.event.as_str(), e.action.as_str(), e.tracker_id),
            ("hand_raised", "left", Some(3))
        );
        assert_eq!(e.data.as_ref().unwrap()["torso"], 40.0);
    }

    #[test]
    fn state_lasts_and_a_tracks_part_ends_with_it() {
        let code = r#"
            state.frames = (state.frames or 0) + 1
            state.tracks = state.tracks or {}
            state.tracks[3] = (state.tracks[3] or 0) + 1
            return {count = state.frames, score = state.tracks[3]}
        "#;
        let mut r = LuaRule::new("n", &config(code), None, BTreeMap::new()).unwrap();
        r.update(&input(&[], 0.0)).unwrap();
        let e = r.update(&input(&[], 0.1)).unwrap();
        assert_eq!((e[0].count, e[0].score), (Some(2), Some(2.0)));
        let info = TrackInfo {
            id: 3,
            class_id: 0,
            class_name: None,
            bbox: [0.0; 4],
            velocity: [0.0; 2],
            age_s: 1.0,
            lost: false,
        };
        let ended = [TrackEvent::Ended(info)];
        let i = ScriptInput {
            track_events: &ended,
            ..input(&[], 0.2)
        };
        let e = r.update(&i).unwrap();
        assert_eq!((e[0].count, e[0].score), (Some(3), Some(1.0)));
    }

    #[test]
    fn results_of_every_shape() {
        let run = |code: &str| {
            let mut r = LuaRule::new("n", &config(code), None, BTreeMap::new()).unwrap();
            r.update(&input(&[], 0.0))
        };
        assert!(run("return nil").unwrap().is_empty());
        assert!(run("").unwrap().is_empty());
        assert!(run("return {}").unwrap().is_empty());
        assert_eq!(run("return {action = 'x'}").unwrap()[0].action, "x");
        assert_eq!(
            run("return {{label = 'a'}, {label = 'b'}}").unwrap().len(),
            2
        );
        assert!(run("return {1}").is_err());
    }

    #[test]
    fn errors_and_limits_are_reported() {
        assert!(LuaRule::new("n", &config("this is not lua"), None, BTreeMap::new()).is_err());
        let mut r = LuaRule::new("n", &config("error('boom')"), None, BTreeMap::new()).unwrap();
        assert!(r.update(&input(&[], 0.0)).unwrap_err().contains("boom"));
        let mut c = config("while true do end");
        c.limits.max_instructions = Some(10_000);
        let mut r = LuaRule::new("n", &c, None, BTreeMap::new()).unwrap();
        assert!(r.update(&input(&[], 0.0)).is_err());
    }

    #[test]
    fn the_sandbox_is_closed() {
        for code in ["return os.time()", "return io.open('x')", "x = 1"] {
            let mut r = LuaRule::new("n", &config(code), None, BTreeMap::new()).unwrap();
            assert!(r.update(&input(&[], 0.0)).is_err(), "{code}");
        }
    }
}
