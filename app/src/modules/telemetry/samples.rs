//! A director snapshot as a flat list of samples: what every telemetry sink
//! writes, whatever its format.

// Used by the sinks; a build with neither has nothing to write.
#![cfg_attr(
    not(any(feature = "otel", feature = "telemetry-file")),
    allow(dead_code)
)]

use crate::modules::director::handle::{SceneState, StatsSnapshot};
use crate::modules::supervisor::NodeState;
use std::time::SystemTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// Only goes up (it restarts at zero when a scene restarts).
    Counter,
    /// Any value at the time of the sample.
    Gauge,
}

/// What a sample measures. Names follow OpenTelemetry's conventions
/// (dot-separated, unit in `unit`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Metric {
    pub name: &'static str,
    pub kind: Kind,
    /// UCUM-style unit: `{event}`, `By`, `ms`, `1`.
    pub unit: &'static str,
    pub description: &'static str,
}

macro_rules! metrics {
    ($($id:ident = $name:literal, $kind:ident, $unit:literal, $desc:literal;)*) => {
        $(pub(crate) const $id: Metric = Metric {
            name: $name,
            kind: Kind::$kind,
            unit: $unit,
            description: $desc,
        };)*
        /// Every metric, for sinks that declare them up front (and tests).
        #[cfg_attr(not(test), allow(dead_code))]
        pub(crate) const ALL: &[Metric] = &[$($id),*];
    };
}

metrics! {
    NODE_EMITTED = "flwrs.node.events.emitted", Counter, "{event}", "Events the node's plugin produced";
    NODE_RECEIVED = "flwrs.node.events.received", Counter, "{event}", "Events delivered to the node's plugin";
    NODE_DROPPED = "flwrs.node.events.dropped", Counter, "{event}", "Events for the node that were dropped (all reasons)";
    NODE_MAPPING_ERRORS = "flwrs.node.mapping.errors", Counter, "{event}", "The node's events that an out-edge's mapping couldn't map";
    NODE_RESTARTS = "flwrs.node.restarts", Counter, "{restart}", "Relaunches of the node's plugin";
    NODE_UP = "flwrs.node.up", Gauge, "1", "1 if the node's plugin is connected and accepted, else 0";
    QUEUE_DEPTH = "flwrs.node.queue.depth", Gauge, "{event}", "Events in the node's inbound queue (memory and disk)";
    QUEUE_BYTES = "flwrs.node.queue.size", Gauge, "By", "Bytes in the node's inbound queue (memory and disk)";
    QUEUE_BLOCKED = "flwrs.node.queue.blocked_time", Counter, "ms", "Time producers waited for room in the node's queue";
    QUEUE_SPILLED = "flwrs.node.queue.spilled", Counter, "{event}", "Events the node's queue wrote to disk";
    SCENE_UP = "flwrs.scene.up", Gauge, "1", "1 if the scene is running, else 0";
    HUB_QUEUED_MEMORY = "flwrs.hub.queued_memory.usage", Gauge, "By", "Bytes of the hub-wide queue budget in use";
    HUB_QUEUED_MEMORY_LIMIT = "flwrs.hub.queued_memory.limit", Gauge, "By", "The hub-wide queue budget";
    SPILL_MEMORY = "flwrs.spill.memory.usage", Gauge, "By", "Memory used by spilling queues";
    SPILL_DISK = "flwrs.spill.disk.usage", Gauge, "By", "Bytes in spool files";
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Sample {
    pub metric: Metric,
    /// `scene.id`, `node.id`, `plugin.id`, `plugin.version`, as they apply.
    pub labels: Vec<(&'static str, String)>,
    pub value: f64,
}

/// One collection: when, and every sample.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Samples {
    pub time: SystemTime,
    pub samples: Vec<Sample>,
}

pub(crate) fn from_snapshot(snapshot: &StatsSnapshot, time: SystemTime) -> Samples {
    let mut samples = vec![];
    let mut push = |metric: Metric, labels: Vec<(&'static str, String)>, value: u64| {
        samples.push(Sample {
            metric,
            labels,
            value: value as f64,
        });
    };
    let o = &snapshot.overview;
    push(HUB_QUEUED_MEMORY, vec![], o.queued_memory.used_bytes);
    push(HUB_QUEUED_MEMORY_LIMIT, vec![], o.queued_memory.limit_bytes);
    push(SPILL_MEMORY, vec![], o.spill_memory.used_bytes);
    push(SPILL_DISK, vec![], o.spill_disk.used_bytes);
    for (scene_id, status) in &snapshot.scenes {
        let scene = vec![("scene.id", scene_id.clone())];
        push(
            SCENE_UP,
            scene.clone(),
            u64::from(status.state == SceneState::Running),
        );
        for node in &status.nodes {
            let labels = vec![
                ("scene.id", scene_id.clone()),
                ("node.id", node.node_id.clone()),
                ("plugin.id", node.plugin_id.clone()),
                ("plugin.version", node.plugin_version.clone()),
            ];
            let c = &node.counters;
            push(NODE_EMITTED, labels.clone(), c.emitted);
            push(NODE_RECEIVED, labels.clone(), c.received);
            push(NODE_DROPPED, labels.clone(), c.dropped);
            push(NODE_MAPPING_ERRORS, labels.clone(), c.mapping_errors);
            push(NODE_RESTARTS, labels.clone(), c.restarts);
            push(
                NODE_UP,
                labels.clone(),
                u64::from(node.status.state == NodeState::Running),
            );
            if let Some(q) = &node.queue {
                push(QUEUE_DEPTH, labels.clone(), q.len as u64 + q.disk_len);
                push(QUEUE_BYTES, labels.clone(), q.bytes as u64 + q.disk_bytes);
                push(QUEUE_BLOCKED, labels.clone(), q.blocked_ms);
                if q.spill {
                    push(QUEUE_SPILLED, labels.clone(), q.spilled);
                }
            }
        }
    }
    Samples { time, samples }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::director::handle::{Overview, SceneStatus, Usage};
    use crate::modules::director::queue::QueueStats;
    use crate::modules::director::running::{NodeCounters, NodeReport};
    use crate::modules::supervisor::NodeStatus;

    #[test]
    fn metric_names_are_unique_and_documented() {
        let mut names: Vec<&str> = ALL.iter().map(|m| m.name).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), ALL.len());
        assert!(ALL.iter().all(|m| m.name.starts_with("flwrs.")
            && !m.unit.is_empty()
            && !m.description.is_empty()));
    }

    #[test]
    fn a_snapshot_becomes_labelled_samples() {
        let usage = |used| Usage {
            used_bytes: used,
            limit_bytes: 100,
        };
        let node = NodeReport {
            node_id: "n1".into(),
            plugin_id: "http-sink".into(),
            plugin_version: "0.0.1".into(),
            status: NodeStatus {
                state: NodeState::Running,
                launches: 3,
                failed_launches: 0,
                pid: Some(1),
                last_error: None,
            },
            counters: NodeCounters {
                emitted: 0,
                received: 7,
                dropped: 1,
                mapping_errors: 0,
                restarts: 2,
            },
            queue: Some(QueueStats {
                len: 2,
                bytes: 20,
                disk_len: 3,
                disk_bytes: 30,
                blocked_ms: 5,
                ..Default::default()
            }),
            dropped_hops: 0,
            mapping_errors: 0,
        };
        let snapshot = StatsSnapshot {
            overview: Overview {
                hub_address: "127.0.0.1:7400".into(),
                queued_memory: usage(10),
                spill_memory: usage(0),
                spill_disk: usage(0),
                scenes: vec![],
            },
            scenes: vec![(
                "s1".into(),
                SceneStatus {
                    state: SceneState::Running,
                    errors: vec![],
                    max_hops: Some(32),
                    nodes: vec![node],
                },
            )],
        };
        let samples = from_snapshot(&snapshot, SystemTime::UNIX_EPOCH).samples;
        let value = |name: &str| {
            samples
                .iter()
                .find(|s| s.metric.name == name)
                .map(|s| (s.labels.clone(), s.value))
        };
        assert_eq!(value("flwrs.hub.queued_memory.usage"), Some((vec![], 10.0)));
        assert_eq!(value("flwrs.scene.up").unwrap().1, 1.0);
        let (labels, received) = value("flwrs.node.events.received").unwrap();
        assert_eq!(received, 7.0);
        assert_eq!(
            labels,
            [
                ("scene.id", "s1".to_string()),
                ("node.id", "n1".to_string()),
                ("plugin.id", "http-sink".to_string()),
                ("plugin.version", "0.0.1".to_string()),
            ]
        );
        assert_eq!(value("flwrs.node.restarts").unwrap().1, 2.0);
        assert_eq!(
            value("flwrs.node.queue.depth").unwrap().1,
            5.0,
            "memory + disk"
        );
        assert_eq!(value("flwrs.node.queue.size").unwrap().1, 50.0);
        assert_eq!(value("flwrs.node.up").unwrap().1, 1.0);
        assert!(value("flwrs.node.queue.spilled").is_none(), "spill is off");
    }
}
