//! Groups inside the graph: which execution policy and core budget each
//! model runs under. The shape is `flwrs_types::groups`; this level takes
//! `execution` and `resources` policies, and refuses `placement` (every
//! model of a node runs in one process on one machine).
//!
//! A model's execution policy is its **innermost** execution group's, whole:
//! an outer group's fields don't fill in what an inner one leaves out.

use super::config::{Execution, GraphConfig, NodeConfig};
use crate::backend::Device;
use flwrs_types::groups::{self, Execution as GroupExecution, Family, Policy};
use std::collections::{BTreeMap, BTreeSet};

/// Each model's groups, by node id.
pub fn memberships(graph: &GraphConfig) -> BTreeMap<String, Vec<String>> {
    graph
        .nodes
        .iter()
        .filter_map(|(id, node)| match node {
            NodeConfig::Model(m) if !m.groups.is_empty() => Some((id.clone(), m.groups.clone())),
            _ => None,
        })
        .collect()
}

/// A group's `device` as this plugin's, if it names one it knows.
fn device_of(e: &GroupExecution) -> Result<Option<Device>, String> {
    match e.device.as_deref() {
        None => Ok(None),
        Some("cpu") => Ok(Some(Device::Cpu)),
        Some("gpu") => Ok(Some(Device::Gpu)),
        Some("npu") => Ok(Some(Device::Npu)),
        Some(other) => Err(format!("device {other:?}: use cpu, gpu or npu")),
    }
}

/// The group a model runs under, with its policy.
pub fn execution_of<'a>(graph: &'a GraphConfig, id: &str) -> Option<(&'a str, &'a GroupExecution)> {
    if graph.groups.is_empty() {
        return None;
    }
    let members = memberships(graph);
    let found = groups::of_member(&graph.groups, &members, id, Family::Execution);
    found.first().and_then(|g| match &g.policy {
        Policy::Execution(e) => Some((g.id.as_str(), e)),
        _ => None,
    })
}

/// The smallest core budget a model is under.
pub fn cpu_limit(graph: &GraphConfig, id: &str) -> Option<usize> {
    if graph.groups.is_empty() {
        return None;
    }
    let members = memberships(graph);
    groups::of_member(&graph.groups, &members, id, Family::Resources)
        .iter()
        .filter_map(|g| match &g.policy {
            Policy::Resources(r) => r.limits.cpu.map(|c| c as usize),
            _ => None,
        })
        .min()
}

/// The execution groups some model runs under, with their members.
fn effective(graph: &GraphConfig) -> BTreeMap<&str, (&GroupExecution, BTreeSet<&str>)> {
    let mut out: BTreeMap<&str, (&GroupExecution, BTreeSet<&str>)> = BTreeMap::new();
    for id in graph.nodes.keys() {
        if let Some((group, e)) = execution_of(graph, id) {
            out.entry(group)
                .or_insert_with(|| (e, BTreeSet::new()))
                .1
                .insert(id);
        }
    }
    out
}

/// The cores the execution groups ask for together.
pub fn thread_sum(graph: &GraphConfig) -> u64 {
    effective(graph)
        .values()
        .filter_map(|(e, _)| e.threads)
        .map(u64::from)
        .sum()
}

/// How many execution groups leave `threads` open.
pub fn open_groups(graph: &GraphConfig) -> usize {
    effective(graph)
        .values()
        .filter(|(e, _)| e.threads.is_none())
        .count()
}

/// The problem when the groups ask for more than the plugin's `--threads`.
pub fn budget_problem(graph: &GraphConfig, threads: usize) -> Option<String> {
    let sum = thread_sum(graph);
    (threads > 0 && sum > threads as u64).then(|| {
        format!(
            "the execution groups' threads add up to {sum}, but the node has {threads} (--threads)"
        )
    })
}

/// A group's settings over the plugin's. A provider belongs to its device,
/// so a group that moves to another device or provider doesn't inherit the
/// plugin's provider or its options.
pub fn over(e: &GroupExecution, plugin: &Execution) -> Execution {
    let device = device_of(e).ok().flatten();
    let moved = device.is_some_and(|d| d != plugin.device.unwrap_or_default());
    let other_provider = e.provider.is_some() && e.provider != plugin.provider;
    let (provider, provider_options) = if other_provider {
        (e.provider.clone(), BTreeMap::new())
    } else if moved {
        (None, BTreeMap::new())
    } else {
        (plugin.provider.clone(), plugin.provider_options.clone())
    };
    Execution {
        runtime: plugin.runtime,
        device: device.or(plugin.device),
        provider,
        provider_options,
        allow_fallback: e.allow_fallback.or(plugin.allow_fallback),
    }
}

/// Every problem with the graph's groups.
pub fn problems(graph: &GraphConfig) -> Vec<String> {
    if graph.groups.is_empty() && memberships(graph).is_empty() {
        return vec![];
    }
    let members = memberships(graph);
    let mut out: Vec<String> = groups::check(&graph.groups, &members)
        .into_iter()
        .map(|p| format!("groups: {p}"))
        .collect();
    for g in &graph.groups {
        match &g.policy {
            Policy::Placement(_) => out.push(format!(
                "groups.{}: placement policies aren't available inside a graph: its models all run in one process on one machine (use an execution or resources group)",
                g.id
            )),
            Policy::Execution(e) => {
                if let Err(p) = device_of(e) {
                    out.push(format!("groups.{}: {p}", g.id));
                }
            }
            Policy::Resources(_) => {}
        }
    }
    if !out.is_empty() {
        return out;
    }
    for (id, node) in &graph.nodes {
        let NodeConfig::Model(m) = node else {
            continue;
        };
        let found = groups::of_member(&graph.groups, &members, id, Family::Execution);
        if let [a, b, ..] = &found[..]
            && same_members(&members, &a.id, &b.id)
        {
            out.push(format!(
                "nodes.{id}: execution groups {} and {} have the same members, so neither is its innermost policy",
                a.id, b.id
            ));
        }
        if let Some((group, e)) = execution_of(graph, id) {
            let merged = m
                .execution
                .clone()
                .unwrap_or_default()
                .over(&over(e, &Execution::default()));
            out.extend(
                merged
                    .problems()
                    .into_iter()
                    .map(|p| format!("nodes.{id} in group {group}: {p}")),
            );
        }
    }
    out.extend(budgets(graph));
    out
}

fn same_members(members: &BTreeMap<String, Vec<String>>, a: &str, b: &str) -> bool {
    let of = |g: &str| -> BTreeSet<&str> {
        members
            .iter()
            .filter(|(_, ids)| ids.iter().any(|i| i == g))
            .map(|(m, _)| m.as_str())
            .collect()
    };
    of(a) == of(b)
}

/// `limits.cpu` over the threads of the execution groups inside it. A group
/// with any member inside counts in full.
fn budgets(graph: &GraphConfig) -> Vec<String> {
    let members = memberships(graph);
    let effective = effective(graph);
    let mut out = vec![];
    for g in &graph.groups {
        let Policy::Resources(r) = &g.policy else {
            continue;
        };
        let Some(limit) = r.limits.cpu else {
            continue;
        };
        let inside = |m: &str| {
            members
                .get(m)
                .is_some_and(|ids| ids.iter().any(|i| i == &g.id))
        };
        let (mut sum, mut names) = (0u64, vec![]);
        for (name, (e, who)) in &effective {
            if let Some(t) = e.threads
                && who.iter().any(|m| inside(m))
            {
                sum += u64::from(t);
                names.push(format!("{name}: {t}"));
            }
        }
        if sum > u64::from(limit) {
            out.push(format!(
                "groups.{}: limits.cpu is {limit}, but the execution groups in it ask for {sum} threads ({})",
                g.id,
                names.join(", ")
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::parse;
    use super::*;
    use serde_json::{Value, json};

    /// A detector and a crop model, with these groups.
    fn graph(groups: Value, det: Value, pose: Value) -> Value {
        let pre = json!({"width": 8, "height": 8, "resize": "stretch", "color": "rgb", "dtype": "f32", "layout": "nchw"});
        json!({
            "groups": groups,
            "nodes": {
                "camera": {"type": "source"},
                "det": {"type": "model", "model": "d.onnx", "pre": pre, "groups": det,
                        "decode": {"type": "detections_e2e", "output": "rows"}},
                "roi": {"type": "crop"},
                "pose": {"type": "model", "model": "p.onnx", "pre": pre, "groups": pose,
                         "decode": {"type": "classify", "output": "y", "softmax": false}}
            },
            "links": [
                {"from": "camera.frame", "to": "det.frame"},
                {"from": "camera.frame", "to": "roi.frame"},
                {"from": "det.objects", "to": "roi.objects"},
                {"from": "roi.regions", "to": "pose.frame"}
            ]
        })
    }

    fn exec(id: &str, policy: Value) -> Value {
        json!({"id": id, "family": "execution", "policy": policy})
    }

    fn cpu(id: &str, n: u32) -> Value {
        json!({"id": id, "family": "resources", "policy": {"limits": {"cpu": n}}})
    }

    fn refused(v: Value, want: &str) {
        let problems = parse(&v).map(|_| ()).unwrap_err();
        assert!(
            problems.iter().any(|p| p.contains(want)),
            "{want}: {problems:?}"
        );
    }

    #[test]
    fn execution_and_resources_groups_are_accepted() {
        let v = graph(
            json!([
                exec("fast", json!({"threads": 2, "mode": "parallel"})),
                cpu("all", 4)
            ]),
            json!(["all"]),
            json!(["fast", "all"]),
        );
        let (g, _) = parse(&v).unwrap();
        assert_eq!(execution_of(&g, "pose").unwrap().0, "fast");
        assert!(execution_of(&g, "det").is_none());
        assert_eq!(cpu_limit(&g, "det"), Some(4));
        assert_eq!(thread_sum(&g), 2);
    }

    #[test]
    fn a_graph_without_groups_stays_as_it_serializes() {
        let v = graph(json!([]), json!([]), json!([]));
        let (g, _) = parse(&v).unwrap();
        let text = serde_json::to_string(&g).unwrap();
        assert!(!text.contains("groups"), "{text}");
    }

    #[test]
    fn placement_is_refused_with_its_reason() {
        let v = graph(
            json!([{"id": "p", "family": "placement", "policy": {"strategy": "pack"}}]),
            json!(["p"]),
            json!([]),
        );
        refused(v, "placement policies aren't available");
    }

    #[test]
    fn unknown_members_devices_and_budgets_are_refused() {
        refused(graph(json!([]), json!(["nope"]), json!([])), "no group");
        refused(
            graph(
                json!([exec("a", json!({"device": "tpu"}))]),
                json!(["a"]),
                json!([]),
            ),
            "use cpu, gpu or npu",
        );
        refused(
            graph(
                json!([
                    exec("a", json!({"threads": 3})),
                    exec("b", json!({"threads": 2})),
                    cpu("all", 4)
                ]),
                json!(["a", "all"]),
                json!(["b", "all"]),
            ),
            "limits.cpu is 4",
        );
    }

    #[test]
    fn two_execution_policies_of_the_same_members_are_refused() {
        refused(
            graph(
                json!([exec("a", json!({})), exec("b", json!({}))]),
                json!(["a", "b"]),
                json!([]),
            ),
            "neither is its innermost",
        );
    }

    #[test]
    fn nested_execution_groups_use_the_innermost_whole() {
        let v = graph(
            json!([
                exec("outer", json!({"threads": 4, "device": "cpu"})),
                exec("inner", json!({"threads": 1}))
            ]),
            json!(["outer"]),
            json!(["outer", "inner"]),
        );
        let (g, _) = parse(&v).unwrap();
        assert_eq!(execution_of(&g, "pose").unwrap().0, "inner");
        assert_eq!(execution_of(&g, "det").unwrap().0, "outer");
        assert_eq!(thread_sum(&g), 5);
        assert!(budget_problem(&g, 4).unwrap().contains("add up to 5"));
        assert!(budget_problem(&g, 5).is_none());
        assert!(budget_problem(&g, 0).is_none(), "unknown: not a problem");
    }

    #[test]
    fn tract_on_a_groups_gpu_is_refused() {
        let mut v = graph(
            json!([exec("g", json!({"device": "gpu"}))]),
            json!(["g"]),
            json!([]),
        );
        v["nodes"]["det"]["execution"] = json!({"runtime": "tract"});
        refused(v, "tract runs on the CPU only");
    }

    #[test]
    fn a_groups_device_drops_the_plugins_provider() {
        let plugin = Execution {
            device: Some(Device::Gpu),
            provider: Some("coreml".into()),
            provider_options: [("ModelFormat".to_string(), "MLProgram".to_string())].into(),
            ..Default::default()
        };
        let cpu_group = GroupExecution {
            device: Some("cpu".into()),
            ..Default::default()
        };
        let e = over(&cpu_group, &plugin);
        assert_eq!(e.device, Some(Device::Cpu));
        assert_eq!(e.provider, None);
        assert!(e.provider_options.is_empty());
        // Nothing about the device: the plugin's stay.
        let e = over(&GroupExecution::default(), &plugin);
        assert_eq!(e.provider.as_deref(), Some("coreml"));
        assert_eq!(e.provider_options.len(), 1);
    }
}
