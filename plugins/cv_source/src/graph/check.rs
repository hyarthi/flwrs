//! Checking a graph: ids, ports, link types, every input linked once, no
//! cycles, for-each scopes. Every problem is reported, with its node or link.

use super::config::{
    GraphConfig, Link, NodeConfig, RuleConfig, RuleInput, RuleType, Sampling, TrackerConfig,
};
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
    problems.extend(placement_problems(graph, &scope));
    problems.extend(super::groups::problems(graph));
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
                    if t.embedder.is_some() {
                        if t.dims != 0
                            || t.keypoints.is_some()
                            || t.normalize != super::config::Normalize::None
                        {
                            problems.push(
                                "tensor_input.embedder takes no dims, normalize or keypoints"
                                    .into(),
                            );
                        }
                    } else if !(2..=3).contains(&t.dims) {
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
        NodeConfig::Emit(e) => {
            if e.max_objects == 0 {
                problems.push("max_objects must be at least 1".into());
            }
        }
        NodeConfig::Tracker(t) => problems.extend(tracker_problems(t)),
        NodeConfig::Rule(r) => problems.extend(rule_problems(r)),
        NodeConfig::Lua(l) => {
            if l.script.trim().is_empty() {
                problems.push("script is empty".into());
            }
            #[cfg(feature = "lua")]
            {
                let params: Vec<String> = crate::analytics::script::PARAMS
                    .iter()
                    .map(|p| p.to_string())
                    .collect();
                if let Err(e) = flwrs_types::lua::check_syntax("script", &l.script, &params) {
                    problems.push(e);
                }
            }
            #[cfg(not(feature = "lua"))]
            problems.push("this build has no Lua (feature lua)".into());
        }
    }
    problems
}

fn tracker_problems(t: &TrackerConfig) -> Vec<String> {
    let mut out = vec![];
    for (name, v) in [
        ("high_confidence", t.high_confidence),
        ("low_confidence", t.low_confidence),
        ("new_track_confidence", t.new_track_confidence),
        ("min_iou", t.min_iou),
        ("min_iou_low", t.min_iou_low),
    ] {
        if !(0.0..=1.0).contains(&v) {
            out.push(format!("{name} must be between 0 and 1"));
        }
    }
    if t.low_confidence > t.high_confidence {
        out.push("low_confidence can't be above high_confidence".into());
    }
    if t.new_track_confidence < t.high_confidence {
        out.push("new_track_confidence can't be below high_confidence".into());
    }
    if t.min_hits == 0 {
        out.push("min_hits must be at least 1".into());
    }
    if !(t.max_lost_s >= 0.0 && t.max_lost_s.is_finite()) {
        out.push("max_lost_s must be 0 or more".into());
    }
    if let Some(c) = &t.control {
        if c.tracker_frames > 300 {
            out.push("control.tracker_frames must be at most 300".into());
        }
        if !(0.0..=1.0).contains(&c.min_visible) {
            out.push("control.min_visible must be between 0 and 1".into());
        }
        if !(c.box_smoothing >= 1.0 && c.box_smoothing.is_finite()) {
            out.push("control.box_smoothing must be 1 or more".into());
        }
        if c.max_missed == 0 {
            out.push("control.max_missed must be at least 1".into());
        }
    }
    if let Some(s) = &t.smooth_keypoints
        && !(s.min_cutoff > 0.0 && s.min_cutoff.is_finite() && s.beta >= 0.0 && s.beta.is_finite())
    {
        out.push("smooth_keypoints: min_cutoff must be above 0 and beta 0 or more".into());
    }
    out
}

fn shape_problems(name: &str, points: &[[f32; 2]], least: usize) -> Vec<String> {
    let mut out = vec![];
    if points.len() < least {
        out.push(format!("{name} needs at least {least} points"));
    }
    if points.iter().flatten().any(|v| !(0.0..=1.0).contains(v)) {
        out.push(format!("{name}: points are normalised, 0 to 1"));
    }
    out
}

fn rule_problems(r: &RuleConfig) -> Vec<String> {
    let mut out = vec![];
    for (name, v) in [
        ("debounce_s", r.debounce_s),
        ("cooldown_s", r.cooldown_s),
        ("hysteresis", r.hysteresis),
    ] {
        if !(v >= 0.0 && v.is_finite()) {
            out.push(format!("{name} must be 0 or more"));
        }
    }
    let classes_input = r.input == RuleInput::Classes;
    match &r.rule {
        RuleType::LineCrossing { line, .. } => out.extend(shape_problems("line", line, 2)),
        RuleType::Zone { zone, .. } => out.extend(shape_problems("zone", zone, 3)),
        RuleType::Loitering { zone, seconds } => {
            out.extend(shape_problems("zone", zone, 3));
            if !(*seconds > 0.0 && seconds.is_finite()) {
                out.push("seconds must be above 0".into());
            }
        }
        RuleType::Counting { zone, above, below } => {
            if let Some(z) = zone {
                out.extend(shape_problems("zone", z, 3));
            }
            if above.is_none() && below.is_none() {
                out.push("counting needs above, below or both".into());
            }
        }
        RuleType::Track { min_age_s, .. } => {
            if !(*min_age_s >= 0.0 && min_age_s.is_finite()) {
                out.push("min_age_s must be 0 or more".into());
            }
        }
        RuleType::LabelChanged { from } => {
            if classes_input && from.is_some() {
                out.push("from is for rules on objects".into());
            }
        }
        RuleType::ScoreAbove {
            threshold, from, ..
        } => {
            if !(0.0..=1.0).contains(threshold) {
                out.push("threshold must be between 0 and 1".into());
            }
            if classes_input && from.is_some() {
                out.push("from is for rules on objects".into());
            }
        }
    }
    if classes_input
        && !matches!(
            r.rule,
            RuleType::LabelChanged { .. } | RuleType::ScoreAbove { .. }
        )
    {
        out.push("input classes is for label_changed and score_above".into());
    }
    out
}

/// Whether the objects reaching `id`'s `objects` input come from a tracker
/// (through filters).
fn tracked_input(graph: &GraphConfig, id: &str) -> bool {
    let mut at = id.to_string();
    for _ in 0..=graph.nodes.len() {
        let Some(from) = graph
            .links
            .iter()
            .find(|l| l.to == format!("{at}.objects"))
            .and_then(|l| l.from.split_once('.'))
            .map(|(n, _)| n)
        else {
            return false;
        };
        match graph.nodes.get(from) {
            Some(NodeConfig::Tracker(_)) => return true,
            Some(NodeConfig::Filter(_)) => at = from.to_string(),
            _ => return false,
        }
    }
    false
}

/// What only works on nodes run once per frame, or on tracked objects.
fn placement_problems(
    graph: &GraphConfig,
    scope: &BTreeMap<String, Option<String>>,
) -> Vec<String> {
    let mut out = vec![];
    let emits = graph
        .nodes
        .values()
        .filter(|n| matches!(n, NodeConfig::Emit(_)))
        .count();
    if emits > 1 {
        out.push(format!("the graph can have one emit node (it has {emits})"));
    }
    let per_region = |id: &str| scope.get(id).is_some_and(Option::is_some);
    for (id, node) in &graph.nodes {
        match node {
            NodeConfig::Tracker(_)
            | NodeConfig::Rule(_)
            | NodeConfig::Lua(_)
            | NodeConfig::Emit(_)
                if per_region(id) =>
            {
                out.push(format!(
                    "nodes.{id}: a {} node runs once per frame, not per region",
                    node.type_name()
                ));
            }
            NodeConfig::Tracker(t) if t.control.is_some() => {
                // Through filters, to the model that detects.
                let mut at = id.clone();
                let detector = loop {
                    let from = graph
                        .links
                        .iter()
                        .find(|l| l.to == format!("{at}.objects"))
                        .and_then(|l| l.from.split_once('.'))
                        .map(|(n, _)| n);
                    match from.and_then(|n| graph.nodes.get(n).map(|x| (n, x))) {
                        Some((n, NodeConfig::Filter(_))) => at = n.to_string(),
                        Some((n, NodeConfig::Model(m))) => break Some((n, m)),
                        _ => break None,
                    }
                };
                match detector {
                    None => out.push(format!(
                        "nodes.{id}: control needs a model feeding this tracker (directly or through filters)"
                    )),
                    Some((n, m)) => {
                        if m.pre.is_none() {
                            out.push(format!("nodes.{id}: control needs a detector that runs on the frame; {n} takes objects"));
                        }
                        if m.sampling.is_some() {
                            out.push(format!("nodes.{n}: control frames decide when it runs, so it can't have sampling"));
                        }
                    }
                }
            }
            NodeConfig::Rule(r) if r.input == RuleInput::Objects => {
                let needs_tracks = !matches!(r.rule, RuleType::Counting { .. });
                if needs_tracks && !tracked_input(graph, id) {
                    out.push(format!(
                        "nodes.{id}: this rule follows objects over time, so its objects must come from a tracker node"
                    ));
                }
                let from = match &r.rule {
                    RuleType::LabelChanged { from } | RuleType::ScoreAbove { from, .. } => {
                        from.as_deref()
                    }
                    _ => None,
                };
                if let Some(from) = from
                    && !matches!(graph.nodes.get(from), Some(NodeConfig::Model(_)))
                {
                    out.push(format!("nodes.{id}: from: there's no model node {from}"));
                }
            }
            NodeConfig::Model(m) => {
                if let Some(Sampling::PerTrack { .. }) = m.sampling {
                    let crop = scope.get(id).cloned().flatten();
                    let on_tracks = m.tensor_input.is_none()
                        && crop.as_deref().is_some_and(|c| tracked_input(graph, c));
                    if !on_tracks {
                        out.push(format!(
                            "nodes.{id}: sampling.per_track is for models run on the regions of tracked objects (a crop node fed by a tracker)"
                        ));
                    }
                }
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::parse;
    use serde_json::{Value, json};

    fn model(decode: Value) -> Value {
        json!({"type": "model", "model": "m.onnx",
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
                "camera": {"type": "source"}, "det": detector(),
                "crop": {"type": "crop"}, "cls": classifier()},
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
                "camera": {"type": "source"}, "det": detector(), "cls": classifier()},
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
            json!({"nodes": {"a": {"type": "source"}, "b": {"type": "source"}}}),
            "it has 2",
        );
    }

    #[test]
    fn cycles_are_refused() {
        has(
            json!({"nodes": {"camera": {"type": "source"}, "f": {"type": "filter"}, "g": {"type": "filter"}},
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
        let nodes = json!({"camera": {"type": "source"}, "det": detector(),
            "a": {"type": "crop"}, "b": {"type": "crop"}, "x": detector(), "y": detector(),
            "z": {"type": "filter"}});
        let (_, checked) = parse(&json!({"nodes": nodes, "links": links})).unwrap();
        assert_eq!(checked.scope["z"].as_deref(), Some("a"));
        assert_eq!(checked.scope["y"].as_deref(), Some("b"));
        // (Mixing two scopes needs a node with several inputs of those
        // types: rule and emit nodes, later.)
        // Cropping regions again.
        has(
            json!({"nodes": {"camera": {"type": "source"}, "det": detector(), "a": {"type": "crop"},
                    "x": detector(), "c": {"type": "crop"}},
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
            json!({"nodes": {"camera": {"type": "source"}, "Bad": {"type": "filter", "min_confidence": 2},
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
            json!({"nodes": {"camera": {"type": "camera"}}}),
            "unknown variant `camera`",
        );
        has(
            json!({"nodes": {"camera": {"type": "source", "fps": 30}}}),
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
            json!({"nodes": {"camera": {"type": "source"}, "det": detector(), "roi": {"type": "crop"}, "m": m},
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
                .contains("sampling.per_track is for models run on the regions of tracked objects")
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
            json!({"nodes": {"camera": {"type": "source"}, "m": m}, "links": [{"from": "camera.frame", "to": "m.frame"}]})
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
        let (graph, _) = parse(&json!({"nodes": {"camera": {"type": "source"}, "m": m},
            "links": [{"from": "camera.frame", "to": "m.frame"}]}))
        .unwrap();
        assert_eq!(super::super::uploads(&graph), ["labels.txt", "m.onnx"]);
    }
}
