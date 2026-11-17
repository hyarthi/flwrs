//! Windows on processing time (epoch ms): which windows an event at `t`
//! belongs to. Pure functions; the engine keeps the state and the timers.

use flwrs_script_transform::store::parse_duration_ms;
use serde::Deserialize;

/// Most windows one event can belong to (`size / hop` for hopping windows).
pub const MAX_OVERLAP: i64 = 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Window {
    /// One running aggregate per key; it never closes (only `step` emits).
    /// `idle_ttl`: a key not updated for that long is forgotten.
    None { idle_ttl: Option<i64> },
    /// Back-to-back windows of `size`, aligned to the epoch.
    Tumbling { size: i64 },
    /// Windows of `size` starting every `hop`; an event is in each that covers it.
    Hopping { size: i64, hop: i64 },
    /// Per key: a window that stays open while events come within `gap`.
    Session { gap: i64 },
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct WindowFile {
    #[serde(rename = "type")]
    window_type: Option<String>,
    size: Option<String>,
    hop: Option<String>,
    gap: Option<String>,
    idle_ttl: Option<String>,
}

impl Window {
    /// From the `window` setting (`None`: a running aggregate).
    pub fn parse(json: Option<&serde_json::Value>) -> Result<Window, Vec<String>> {
        let Some(json) = json else {
            return Ok(Window::None { idle_ttl: None });
        };
        let file: WindowFile =
            serde_json::from_value(json.clone()).map_err(|e| vec![format!("window: {e}")])?;
        let mut problems = vec![];
        let mut duration = |name: &str, value: &Option<String>, needed: bool| -> Option<i64> {
            match value {
                None if needed => {
                    problems.push(format!("window.{name} is required for this type"));
                    None
                }
                None => None,
                Some(text) => match parse_duration_ms(text) {
                    Ok(ms) if ms > 0 => Some(ms),
                    Ok(_) => {
                        problems.push(format!("window.{name} must be longer than 0"));
                        None
                    }
                    Err(e) => {
                        problems.push(format!("window.{name}: {e}"));
                        None
                    }
                },
            }
        };
        let kind = file.window_type.as_deref().unwrap_or("none");
        let unused = |problems: &mut Vec<String>, name: &str, set: bool| {
            if set {
                problems.push(format!("window.{name} doesn't apply to type {kind:?}"));
            }
        };
        let window = match kind {
            "none" => {
                let idle_ttl = duration("idle_ttl", &file.idle_ttl, false);
                unused(&mut problems, "size", file.size.is_some());
                unused(&mut problems, "hop", file.hop.is_some());
                unused(&mut problems, "gap", file.gap.is_some());
                Some(Window::None { idle_ttl })
            }
            "tumbling" => {
                let size = duration("size", &file.size, true);
                unused(&mut problems, "hop", file.hop.is_some());
                unused(&mut problems, "gap", file.gap.is_some());
                unused(&mut problems, "idle_ttl", file.idle_ttl.is_some());
                size.map(|size| Window::Tumbling { size })
            }
            "hopping" => {
                let size = duration("size", &file.size, true);
                let hop = duration("hop", &file.hop, true);
                unused(&mut problems, "gap", file.gap.is_some());
                unused(&mut problems, "idle_ttl", file.idle_ttl.is_some());
                match (size, hop) {
                    (Some(size), Some(hop)) if hop > size => {
                        problems.push("window.hop must not be longer than window.size".into());
                        None
                    }
                    (Some(size), Some(hop)) if size / hop > MAX_OVERLAP => {
                        problems.push(format!(
                            "window.size / window.hop must be at most {MAX_OVERLAP} (overlapping windows per event)"
                        ));
                        None
                    }
                    (Some(size), Some(hop)) => Some(Window::Hopping { size, hop }),
                    _ => None,
                }
            }
            "session" => {
                let gap = duration("gap", &file.gap, true);
                unused(&mut problems, "size", file.size.is_some());
                unused(&mut problems, "hop", file.hop.is_some());
                unused(&mut problems, "idle_ttl", file.idle_ttl.is_some());
                gap.map(|gap| Window::Session { gap })
            }
            other => {
                problems.push(format!(
                    "window.type {other:?} must be none, tumbling, hopping or session"
                ));
                None
            }
        };
        match window {
            Some(w) if problems.is_empty() => Ok(w),
            _ => Err(problems),
        }
    }

    /// The `[start, end)` of the fixed windows (tumbling, hopping) that
    /// cover `t`, oldest first. Empty for the other types.
    pub fn covering(&self, t: i64) -> Vec<(i64, i64)> {
        match *self {
            Window::Tumbling { size } => {
                let start = t.div_euclid(size) * size;
                vec![(start, start + size)]
            }
            Window::Hopping { size, hop } => {
                let last = t.div_euclid(hop) * hop;
                let mut starts = vec![];
                let mut start = last;
                while start > t - size {
                    starts.push((start, start + size));
                    start -= hop;
                }
                starts.reverse();
                starts
            }
            Window::None { .. } | Window::Session { .. } => vec![],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_each_type() {
        assert_eq!(
            Window::parse(None).unwrap(),
            Window::None { idle_ttl: None }
        );
        assert_eq!(
            Window::parse(Some(&json!({"type": "none", "idle_ttl": "1h"}))).unwrap(),
            Window::None {
                idle_ttl: Some(3_600_000)
            }
        );
        assert_eq!(
            Window::parse(Some(&json!({"type": "tumbling", "size": "1m"}))).unwrap(),
            Window::Tumbling { size: 60_000 }
        );
        assert_eq!(
            Window::parse(Some(
                &json!({"type": "hopping", "size": "1m", "hop": "10s"})
            ))
            .unwrap(),
            Window::Hopping {
                size: 60_000,
                hop: 10_000
            }
        );
        assert_eq!(
            Window::parse(Some(&json!({"type": "session", "gap": "5m"}))).unwrap(),
            Window::Session { gap: 300_000 }
        );
        for (bad, expected) in [
            (json!({"type": "tumbling"}), "window.size is required"),
            (
                json!({"type": "hopping", "size": "10s", "hop": "1m"}),
                "hop must not be longer",
            ),
            (
                json!({"type": "hopping", "size": "1h", "hop": "1ms"}),
                "at most",
            ),
            (
                json!({"type": "session", "gap": "5m", "size": "1m"}),
                "doesn't apply",
            ),
            (json!({"type": "sliding"}), "must be none"),
            (
                json!({"type": "tumbling", "size": "soon"}),
                "isn't a duration",
            ),
            (json!({"type": "tumbling", "size": "0s"}), "longer than 0"),
        ] {
            let e = Window::parse(Some(&bad)).unwrap_err();
            assert!(
                e.iter().any(|p| p.contains(expected)),
                "{expected:?} not in {e:?}"
            );
        }
    }

    #[test]
    fn windows_covering_a_time() {
        let t = Window::Tumbling { size: 1_000 };
        assert_eq!(t.covering(2_500), [(2_000, 3_000)]);
        assert_eq!(t.covering(3_000), [(3_000, 4_000)]);
        let h = Window::Hopping {
            size: 3_000,
            hop: 1_000,
        };
        assert_eq!(
            h.covering(2_500),
            [(0, 3_000), (1_000, 4_000), (2_000, 5_000)]
        );
        assert_eq!(
            h.covering(3_000),
            [(1_000, 4_000), (2_000, 5_000), (3_000, 6_000)]
        );
        assert!(Window::Session { gap: 1 }.covering(5).is_empty());
    }
}
