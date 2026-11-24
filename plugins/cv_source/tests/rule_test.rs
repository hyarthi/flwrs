//! `flwrs-cv-source rule-test` through the real binary: a graph file,
//! frames on standard input, events as JSON lines on standard output.
#![cfg(feature = "lua")]

use serde_json::{Value, json};
use std::io::Write;
use std::process::{Command, Stdio};

/// A detector feeding a Lua rule (no model is loaded by `rule-test`).
fn graph(script: &str) -> Value {
    json!({
        "nodes": {
            "camera": {"type": "source"},
            "det": {"type": "model", "model": "det.onnx",
                "pre": {"width": 64, "height": 64, "resize": "stretch", "color": "rgb", "dtype": "f32", "layout": "nchw"},
                "decode": {"type": "detections_e2e", "output": "o"}},
            "rule": {"type": "lua", "script": script}
        },
        "links": [
            {"from": "camera.frame", "to": "det.frame"},
            {"from": "det.objects", "to": "rule.objects"}
        ]
    })
}

#[test]
fn a_rule_runs_on_frames_from_stdin() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("graph.json");
    let script = "if once('w', held('w', #objects > 0, 0.5)) then \
                  return {event = 'seen', action = 'person', tracker_id = objects[1].tracker_id} end";
    std::fs::write(&path, json!({ "graph": graph(script) }).to_string()).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_flwrs-cv-source"))
        .args(["rule-test", "--graph"])
        .arg(&path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let frames: String = (0..10)
        .map(|n| {
            format!(
                "{{\"time\": {}, \"objects\": [{{\"tracker_id\": 7, \"box\": [0, 0, 10, 10]}}]}}\n",
                n as f64 * 0.1
            )
        })
        .collect();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(frames.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let lines: Vec<Value> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0]["event"], "seen");
    assert_eq!(lines[0]["tracker_id"], 7);
    assert_eq!(lines[0]["time"], 0.5);
}

#[test]
fn a_failing_rule_exits_with_1_and_a_bad_graph_with_2() {
    let dir = tempfile::tempdir().unwrap();
    let run = |graph: Value| {
        let path = dir.path().join("g.json");
        std::fs::write(&path, graph.to_string()).unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_flwrs-cv-source"))
            .args(["rule-test", "--graph"])
            .arg(&path)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"{\"time\": 0}\n")
            .unwrap();
        child.wait_with_output().unwrap()
    };
    let failing = run(graph("error('boom')"));
    assert_eq!(failing.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&failing.stderr).contains("boom"));
    let bad = run(json!({"nodes": {"camera": {"type": "source"}}, "links": []}));
    assert_eq!(bad.status.code(), Some(2));
}
