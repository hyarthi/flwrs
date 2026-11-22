//! Checking a graph: ids, ports, link types, every input linked once, no
//! cycles, for-each scopes. Every problem is reported, with its node or link.

use super::config::{GraphConfig, Link, NodeConfig};
use super::{PortType, Ports};
use std::collections::{BTreeMap, BTreeSet};

/// A checked graph: its nodes in an order that runs every node after its
/// inputs, and the for-each scope of each node (the crop node whose regions
/// it runs per item; `None` = once per frame).
#[derive(Clone, Debug)]
pub struct Checked {
    pub order: Vec<String>,
    pub scope: BTreeMap<String, Option<String>>,
}

/// `<node>.<port>` split.
fn split(port: &str) -> Option<(&str, &str)> {
    port.split_once('.')
}

fn valid_id(id: &str) -> bool {
    let mut chars = id.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && id.len() <= 64
}

/// Checks the graph; `Err` holds every problem found.
pub fn check(graph: &GraphConfig) -> Result<Checked, Vec<String>> {
    let mut problems = vec![];
    let mut ports = BTreeMap::new();
    for (id, node) in &graph.nodes {
        if !valid_id(id) {
            problems.push(format!(
                "nodes.{id}: an id is lowercase letters, digits and _, starting with a letter (at most 64)"
            ));
        }
        problems.extend(
            own_problems(node)
                .into_iter()
                .map(|p| format!("nodes.{id}: {p}")),
        );
        ports.insert(id.as_str(), Ports::of(node));
    }
    let sources = graph
        .nodes
        .values()
        .filter(|n| matches!(n, NodeConfig::Source {}))
        .count();
    if sources != 1 {
        problems.push(format!(
            "the graph needs exactly one source node (it has {sources})"
        ));
    }

    // Links: ends exist, directions, types; each input linked once.
    let mut inputs: BTreeMap<(&str, &str), &Link> = BTreeMap::new();
    let mut edges: Vec<(&str, &str, PortType, PortType)> = vec![];
    for (i, link) in graph.links.iter().enumerate() {
        let at = format!("links[{i}] ({} → {})", link.from, link.to);
        let (Some((fnode, fport)), Some((tnode, tport))) = (split(&link.from), split(&link.to))
        else {
            problems.push(format!("{at}: ends are written <node>.<port>"));
            continue;
        };
        let (Some(fp), Some(tp)) = (ports.get(fnode), ports.get(tnode)) else {
            for n in [fnode, tnode]
                .into_iter()
                .filter(|n| !ports.contains_key(n))
            {
                problems.push(format!("{at}: there's no node {n}"));
            }
            continue;
        };
        let Some(out) = fp.output(fport) else {
            problems.push(format!(
                "{at}: {fnode} has no output {fport} (outputs: {})",
                fp.output_names()
            ));
            continue;
        };
        let Some(inp) = tp.input(tport) else {
            problems.push(format!(
                "{at}: {tnode} has no input {tport} (inputs: {})",
                tp.input_names()
            ));
            continue;
        };
        if !(out == inp || (out == PortType::Regions && inp == PortType::Frame)) {
            problems.push(format!(
                "{at}: {} doesn't fit an input of type {}",
                out.name(),
                inp.name()
            ));
            continue;
        }
        if inputs.insert((tnode, tport), link).is_some() {
            problems.push(format!("{at}: {tnode}.{tport} is linked more than once"));
            continue;
        }
        edges.push((fnode, tnode, out, inp));
    }
    for (id, p) in &ports {
        for (name, _) in &p.inputs {
            if !inputs.contains_key(&(*id, *name)) {
                problems.push(format!("nodes.{id}: input {name} isn't linked"));
            }
        }
    }

    // Order (Kahn); what's left is on a cycle.
    let mut indegree: BTreeMap<&str, usize> = ports.keys().map(|id| (*id, 0)).collect();
    for (_, to, _, _) in &edges {
        *indegree.get_mut(to).expect("checked") += 1;
    }
    let mut ready: Vec<&str> = indegree
        .iter()
        .filter(|(_, d)| **d == 0)
        .map(|(id, _)| *id)
        .collect();
    let mut order = vec![];
    while let Some(id) = ready.pop() {
        order.push(id.to_string());
        for (_, to, _, _) in edges.iter().filter(|(from, _, _, _)| *from == id) {
            let d = indegree.get_mut(to).expect("checked");
            *d -= 1;
            if *d == 0 {
                ready.push(to);
            }
        }
    }
    if order.len() < ports.len() {
        let on: BTreeSet<&str> = indegree
            .iter()
            .filter(|(_, d)| **d > 0)
            .map(|(id, _)| *id)
            .collect();
        problems.push(format!(
            "the graph has a cycle through {}",
            on.into_iter().collect::<Vec<_>>().join(", ")
        ));
        return Err(problems);
    }

    // For-each scopes: a regions → frame link opens one (the crop node's);
    // a node's inputs are all in one scope or once per frame.
    let mut scope: BTreeMap<String, Option<String>> = BTreeMap::new();
    for id in &order {
        let mut mine: BTreeSet<Option<String>> = BTreeSet::new();
        for (from, _, out, inp) in edges.iter().filter(|(_, to, _, _)| to == id) {
            let theirs = scope.get(*from).cloned().flatten();
            if *out == PortType::Regions && *inp == PortType::Frame {
                if theirs.is_some() {
                    problems.push(format!(
                        "nodes.{id}: regions of regions (a for-each inside a for-each) aren't supported"
                    ));
                }
                mine.insert(Some(from.to_string()));
            } else {
                mine.insert(theirs);
            }
        }
        let per_item: Vec<&String> = mine.iter().flatten().collect();
        if per_item.len() > 1 {
            problems.push(format!(
                "nodes.{id}: its inputs run per region of different crop nodes ({})",
                per_item
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        scope.insert(id.clone(), per_item.first().map(|s| s.to_string()));
    }
    // Sampling skips frames: only for models that run once per frame (a
    // region's results can't be reused for the next frame's regions).
    for (id, node) in &graph.nodes {
        if let NodeConfig::Model(m) = node
            && matches!(m.sampling, Some(super::config::Sampling::EveryNFrames(n)) if n > 1)
            && scope.get(id).is_some_and(Option::is_some)
        {
            problems.push(format!(
                "nodes.{id}: sampling.every_n_frames is for models that run once per frame, not per region"
            ));
        }
    }
    // Crop nodes take their objects once per frame.
    for (id, node) in &graph.nodes {
        if matches!(node, NodeConfig::Crop(_)) && scope.get(id).is_some_and(Option::is_some) {
            problems.push(format!(
                "nodes.{id}: a crop node runs once per frame (regions of regions aren't supported)"
            ));
        }
    }
    if problems.is_empty() {
        Ok(Checked { order, scope })
    } else {
        Err(problems)
    }
}

fn own_problems(node: &NodeConfig) -> Vec<String> {
    let mut problems = vec![];
    match node {
        NodeConfig::Source {} => {}
        NodeConfig::Model(m) => {
            match (&m.pre, &m.tensor_input) {
                (Some(pre), None) => problems.extend(pre.problems()),
                (None, Some(t)) => {
                    if !(2..=3).contains(&t.dims) {
                        problems.push("tensor_input.dims must be 2 or 3".into());
                    }
                }
                _ => problems.push("set exactly one of pre and tensor_input".into()),
            }
            problems.extend(m.decode.problems());
            if m.decode.needs_charset() != m.charset.is_some() {
                problems.push(if m.charset.is_some() {
                    "charset is only for the ctc decoder".into()
                } else {
                    "the ctc decoder needs a charset".into()
                });
            }
            if m.model.is_empty() {
                problems.push("model must name an uploaded file".into());
            }
            if let Some(c) = m.min_confidence {
                if m.decode.default_min_confidence().is_none() {
                    problems.push(format!(
                        "min_confidence: the {} decoder's results have no confidence",
                        m.decode.type_name()
                    ));
                } else if !(0.0..=1.0).contains(&c) {
                    problems.push("min_confidence must be between 0 and 1".into());
                }
            }
            if let Some(name) = &m.keypoint_layout {
                match crate::keypoints::layout(name) {
                    None => problems.push(format!(
                        "keypoint_layout: no layout {name:?} (there are: {})",
                        crate::keypoints::NAMES.join(", ")
                    )),
                    Some(l) => {
                        if let Some(n) = m.decode.keypoint_count()
                            && n != l.points.len()
                        {
                            problems.push(format!(
                                "keypoint_layout {name:?} has {} points, but the decoder gives {n}",
                                l.points.len()
                            ));
                        }
                    }
                }
            }
            if let Some(super::config::Sampling::PerTrack { .. }) = m.sampling {
                problems.push(
                    "sampling.per_track needs object tracking, which this version doesn't have yet"
                        .into(),
                );
            }
            if let Some(e) = &m.execution {
                problems.extend(e.problems());
            }
            if let Some(super::config::Sampling::EveryNFrames(0)) = m.sampling {
                problems.push("sampling.every_n_frames must be at least 1".into());
            }
        }
        NodeConfig::Crop(c) => {
            if c.max == 0 {
                problems.push("max must be at least 1".into());
            }
            if !(c.scale > 0.0 && c.scale.is_finite()) {
                problems.push("scale must be a positive number".into());
            }
            if c.aspect.is_some_and(|a| !(a > 0.0 && a.is_finite())) {
                problems.push("aspect must be a positive number".into());
            }
        }
        NodeConfig::Filter(f) => {
            if f.min_confidence.is_some_and(|s| !(0.0..=1.0).contains(&s)) {
                problems.push("min_confidence must be between 0 and 1".into());
            }
        }
    }
    problems
}

#[cfg(test)]
mod tests {
    use super::super::parse;
    use serde_json::{Value, json};

    fn model(decode: Value) -> Value {
        json!({"kind": "model", "model": "m.onnx",
            "pre": {"width": 8, "height": 8, "resize": "stretch", "color": "rgb", "dtype": "f32", "layout": "nchw"},
            "decode": decode})
    }

    fn detector() -> Value {
        model(json!({"type": "detections_e2e", "output": "o"}))
    }

    fn classifier() -> Value {
        model(json!({"type": "classify", "output": "o", "softmax": true}))
    }

    fn problems(graph: Value) -> Vec<String> {
        match parse(&graph) {
            Ok(_) => vec![],
            Err(p) => p,
        }
    }

    fn has(graph: Value, needle: &str) {
        let p = problems(graph);
        assert!(
            p.iter().any(|p| p.contains(needle)),
            "{needle:?} not in {p:?}"
        );
    }

    #[test]
    fn a_detector_and_a_classifier_per_object_pass_in_order() {
        let (_, checked) = parse(&json!({"nodes": {
                "camera": {"kind": "source"}, "det": detector(),
                "crop": {"kind": "crop"}, "cls": classifier()},
            "links": [
                {"from": "camera.frame", "to": "det.frame"},
                {"from": "camera.frame", "to": "crop.frame"},
                {"from": "det.objects", "to": "crop.objects"},
                {"from": "crop.regions", "to": "cls.frame"}]}))
        .unwrap();
        let at = |id: &str| checked.order.iter().position(|n| n == id).unwrap();
        assert!(at("camera") < at("det") && at("det") < at("crop") && at("crop") < at("cls"));
        assert_eq!(checked.scope["cls"].as_deref(), Some("crop"));
        assert_eq!(checked.scope["det"], None);
    }

    #[test]
    fn link_problems_are_all_reported() {
        let p = problems(json!({"nodes": {
                "camera": {"kind": "source"}, "det": detector(), "cls": classifier()},
            "links": [
                {"from": "camera.frame", "to": "det.frame"},
                {"from": "camera.frame", "to": "det.frame"},
                {"from": "det.boxes", "to": "cls.frame"},
                {"from": "det.objects", "to": "cls.frame"},
                {"from": "nope.frame", "to": "cls.frame"},
                {"from": "camera", "to": "cls.frame"}]}));
        for needle in [
            "linked more than once",
            "det has no output boxes (outputs: objects)",
            "objects doesn't fit an input of type frame",
            "there's no node nope",
            "ends are written <node>.<port>",
            "nodes.cls: input frame isn't linked",
        ] {
            assert!(
                p.iter().any(|p| p.contains(needle)),
                "{needle:?} not in {p:?}"
            );
        }
    }

    #[test]
    fn exactly_one_source() {
        has(
            json!({"nodes": {"det": detector()}}),
            "exactly one source node (it has 0)",
        );
        has(
            json!({"nodes": {"a": {"kind": "source"}, "b": {"kind": "source"}}}),
            "it has 2",
        );
    }

    #[test]
    fn cycles_are_refused() {
        has(
            json!({"nodes": {"camera": {"kind": "source"}, "f": {"kind": "filter"}, "g": {"kind": "filter"}},
                "links": [{"from": "f.objects", "to": "g.objects"}, {"from": "g.objects", "to": "f.objects"}]}),
            "a cycle through f, g",
        );
    }

    #[test]
    fn for_each_scopes_and_no_nesting() {
        let crop = |from: &str| {
            [
                json!({"from": "camera.frame", "to": format!("{from}.frame")}),
                json!({"from": "det.objects", "to": format!("{from}.objects")}),
            ]
        };
        let mut links = vec![json!({"from": "camera.frame", "to": "det.frame"})];
        links.extend(crop("a"));
        links.extend(crop("b"));
        links.extend([
            json!({"from": "a.regions", "to": "x.frame"}),
            json!({"from": "b.regions", "to": "y.frame"}),
            json!({"from": "x.objects", "to": "z.objects"}),
        ]);
        // Two for-each scopes side by side, z downstream of one of them.
        let nodes = json!({"camera": {"kind": "source"}, "det": detector(),
            "a": {"kind": "crop"}, "b": {"kind": "crop"}, "x": detector(), "y": detector(),
            "z": {"kind": "filter"}});
        let (_, checked) = parse(&json!({"nodes": nodes, "links": links})).unwrap();
        assert_eq!(checked.scope["z"].as_deref(), Some("a"));
        assert_eq!(checked.scope["y"].as_deref(), Some("b"));
        // (Mixing two scopes needs a node with several inputs of those
        // kinds: rule and emit nodes, later.)
        // Cropping regions again.
        has(
            json!({"nodes": {"camera": {"kind": "source"}, "det": detector(), "a": {"kind": "crop"},
                    "x": detector(), "c": {"kind": "crop"}},
                "links": [{"from": "camera.frame", "to": "det.frame"},
                    {"from": "camera.frame", "to": "a.frame"}, {"from": "det.objects", "to": "a.objects"},
                    {"from": "a.regions", "to": "x.frame"},
                    {"from": "camera.frame", "to": "c.frame"}, {"from": "x.objects", "to": "c.objects"}]}),
            "a crop node runs once per frame",
        );
    }

    #[test]
    fn node_settings_are_checked() {
        let mut both = detector();
        both["tensor_input"] = json!({"dims": 2});
        let mut bad_pre = detector();
        bad_pre["pre"]["width"] = json!(0);
        let p = problems(
            json!({"nodes": {"camera": {"kind": "source"}, "Bad": {"kind": "filter", "min_confidence": 2},
            "both": both, "pre": bad_pre,
            "ctc": model(json!({"type": "ctc", "output": "o"})),
            "thr": model(json!({"type": "yolo", "output": "o", "channels_first": true, "classes": 0, "iou": 1.5}))}}),
        );
        for needle in [
            "nodes.Bad: an id is lowercase",
            "min_confidence must be between 0 and 1",
            "set exactly one of pre and tensor_input",
            "pre.width must be between 1 and 8192",
            "the ctc decoder needs a charset",
            "decode.iou must be between 0 and 1",
            "decode.classes must be at least 1",
        ] {
            assert!(
                p.iter().any(|p| p.contains(needle)),
                "{needle:?} not in {p:?}"
            );
        }
        has(
            json!({"nodes": {"camera": {"kind": "camera"}}}),
            "unknown variant `camera`",
        );
        has(
            json!({"nodes": {"camera": {"kind": "source", "fps": 30}}}),
            "unknown field `fps`",
        );
    }

    #[test]
    fn sampling_and_execution_are_checked() {
        let classify = json!({"type": "classify", "output": "o", "softmax": true});
        let on_regions = |extra: Value| {
            let mut m = model(classify.clone());
            m.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            json!({"nodes": {"camera": {"kind": "source"}, "det": detector(), "roi": {"kind": "crop"}, "m": m},
                "links": [{"from": "camera.frame", "to": "det.frame"}, {"from": "camera.frame", "to": "roi.frame"},
                          {"from": "det.objects", "to": "roi.objects"}, {"from": "roi.regions", "to": "m.frame"}]})
        };
        let problems = |g: Value| parse(&g).err().unwrap_or_default().join("\n");
        assert!(
            problems(on_regions(json!({"sampling": {"every_n_frames": 3}})))
                .contains("not per region")
        );
        assert!(
            problems(on_regions(json!({"sampling": {"per_track": {}}})))
                .contains("needs object tracking")
        );
        assert!(
            problems(on_regions(
                json!({"execution": {"provider_options": {"a": "b"}}})
            ))
            .contains("need an execution.provider")
        );
        assert!(
            problems(on_regions(
                json!({"execution": {"runtime": "tract", "device": "gpu"}})
            ))
            .contains("tract runs on the CPU only")
        );
        assert!(parse(&on_regions(json!({"execution": {"device": "gpu", "provider": "coreml", "allow_fallback": true}}))).is_ok());
    }

    #[test]
    fn the_confidence_cutoff_is_checked() {
        let graph = |decode: Value, cutoff: Value| {
            let mut m = model(decode);
            m["min_confidence"] = cutoff;
            json!({"nodes": {"camera": {"kind": "source"}, "m": m}, "links": [{"from": "camera.frame", "to": "m.frame"}]})
        };
        let yolo = json!({"type": "yolo", "output": "o", "channels_first": true, "classes": 2});
        assert!(parse(&graph(yolo.clone(), json!(0.5))).is_ok());
        let e = parse(&graph(yolo, json!(1.5))).err().unwrap();
        assert!(
            e.iter()
                .any(|p| p.contains("min_confidence must be between 0 and 1")),
            "{e:?}"
        );
        let e = parse(&graph(
            json!({"type": "semantic", "output": "o"}),
            json!(0.5),
        ))
        .err()
        .unwrap();
        assert!(
            e.iter()
                .any(|p| p.contains("the semantic decoder's results have no confidence")),
            "{e:?}"
        );
    }

    #[test]
    fn uploads_are_listed() {
        let mut m = classifier();
        m["labels"] = json!({"file": "labels.txt"});
        let (graph, _) = parse(&json!({"nodes": {"camera": {"kind": "source"}, "m": m},
            "links": [{"from": "camera.frame", "to": "m.frame"}]}))
        .unwrap();
        assert_eq!(super::super::uploads(&graph), ["labels.txt", "m.onnx"]);
    }
}
