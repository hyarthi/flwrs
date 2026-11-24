//! Groups at the scene level: which policies a scene accepts, and how a
//! `resources` group's shared core budget becomes its members' threads.
//!
//! A scene runs on one server and each node is its own process, so only
//! `resources` groups mean anything here; `placement` and `execution` are
//! refused rather than ignored. The budget is advisory: it sets how many
//! threads each plugin is told to use, and pins nothing to a CPU.

use crate::modules::node_config::{LaunchMode, MAX_THREADS, NodeConfig, RuntimeConfig};
use crate::modules::plugin::service::Plugin;
use crate::modules::scene::service::Node;
use flwrs_types::groups::{self, Family, Group, Policy};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use utoipa::ToSchema;

/// A group, as the API and `.flwr` files carry it.
#[derive(ToSchema)]
#[allow(
    dead_code,
    reason = "describes the JSON of `flwrs_types::groups::Group`"
)]
pub(crate) struct SceneGroup {
    /// Lowercase letters, digits, `_` and `-`, starting with a letter; at
    /// most 64 characters.
    id: String,
    /// `resources`. `placement` and `execution` are refused at the scene level.
    family: String,
    /// For `resources`: `{"limits": {"cpu": <cores>}}`.
    #[schema(value_type = Object)]
    policy: serde_json::Value,
}

/// The policies a scene can't honour, and limits out of range.
fn level_problems(groups: &[Group]) -> Vec<String> {
    let mut problems = vec![];
    for g in groups {
        match &g.policy {
            Policy::Placement(_) => problems.push(format!(
                "group {}: placement policies aren't supported in a scene: one server runs all its nodes, so there's nowhere else to place them",
                g.id
            )),
            Policy::Execution(_) => problems.push(format!(
                "group {}: execution policies aren't supported in a scene: each node is its own process (they apply inside a plugin's graph)",
                g.id
            )),
            Policy::Resources(r) => {
                if r.limits.cpu.is_some_and(|n| n > MAX_THREADS) {
                    problems.push(format!(
                        "group {}: limits.cpu can't exceed {MAX_THREADS}",
                        g.id
                    ));
                }
            }
        }
    }
    problems
}

/// The problems with a scene's groups on their own, without their members:
/// what saving the settings refuses.
pub(crate) fn settings_problems(groups: &[Group]) -> Vec<String> {
    let mut problems = groups::check(groups, &BTreeMap::new());
    problems.extend(level_problems(groups));
    problems
}

/// The group definitions of a file being imported, as far as refusing the
/// file goes: a policy this server doesn't honour is left to the scene's
/// check, which then keeps it from being enabled.
pub(crate) fn import_problems(groups: &[Group]) -> Vec<String> {
    groups::check(groups, &BTreeMap::new())
}

/// A node as a budget sees it.
pub(crate) struct Member<'a> {
    pub id: &'a str,
    pub groups: &'a [String],
    /// The node's own `runtime.threads`.
    pub own: Option<u32>,
    /// A local node whose plugin takes a core count.
    pub takes_threads: bool,
    /// Why it doesn't, when that's worth saying (not for a missing plugin,
    /// which is a problem of its own).
    pub why_not: Option<String>,
}

impl<'a> Member<'a> {
    pub(crate) fn of(node: &'a Node, plugin: Option<&Plugin>) -> Self {
        Self::from_config(&node.id, &node.config, plugin)
    }

    pub(crate) fn from_config(
        id: &'a str,
        config: &'a NodeConfig,
        plugin: Option<&Plugin>,
    ) -> Self {
        let declares = plugin
            .and_then(Plugin::parsed_manifest)
            .is_some_and(|m| m.threads.is_some());
        let why_not = match (config.launch, plugin) {
            (LaunchMode::Remote, _) => {
                Some("it's a remote node, started by whoever runs it".to_string())
            }
            (LaunchMode::Local, Some(p)) if !declares => Some(format!(
                "plugin {} doesn't say how it takes a core count (manifest threads)",
                p.id
            )),
            _ => None,
        };
        Self {
            id,
            groups: &config.groups,
            own: config.runtime.threads,
            takes_threads: config.launch == LaunchMode::Local && declares,
            why_not,
        }
    }
}

/// What a scene's groups come to.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Split {
    /// Wrong definitions or memberships, and policies a scene can't honour:
    /// refused on any live save.
    pub structure: Vec<String>,
    /// Budgets that don't add up: a `check` problem.
    pub budget: Vec<String>,
    pub warnings: Vec<String>,
    /// Members without their own value: their share, and the innermost
    /// budget it came from.
    pub shares: BTreeMap<String, (u32, String)>,
}

impl Split {
    pub(crate) fn problems(&self) -> Vec<String> {
        [self.structure.as_slice(), self.budget.as_slice()].concat()
    }

    /// Each budget's explicit values are counted first; every member without
    /// one then gets 1, and what's left goes to the most deeply nested
    /// members first, equally within each set of members sharing the same
    /// budgets (a remainder too small to go round stays unused), so an inner
    /// budget is spent before its enclosing one's rest is shared out.
    pub(crate) fn of(groups: &[Group], members: &[Member]) -> Self {
        let mut split = Split::default();
        let memberships: BTreeMap<String, Vec<String>> = members
            .iter()
            .filter(|m| !m.groups.is_empty())
            .map(|m| (format!("node {}", m.id), m.groups.to_vec()))
            .collect();
        split.structure = groups::check(groups, &memberships);
        split.structure.extend(level_problems(groups));
        if !split.structure.is_empty() {
            return split;
        }
        let limits: BTreeMap<&str, u64> = groups
            .iter()
            .filter_map(|g| match &g.policy {
                Policy::Resources(r) => Some((g.id.as_str(), u64::from(r.limits.cpu?))),
                _ => None,
            })
            .collect();
        let mut used: BTreeMap<&str, u64> = limits.keys().map(|id| (*id, 0)).collect();
        let mut waiting: BTreeMap<&str, u64> = used.clone();
        // Members without a value of their own, by the budgets they're in.
        let mut classes: BTreeMap<Vec<&str>, Vec<&str>> = BTreeMap::new();
        for m in members {
            let mut budgets: Vec<&str> = m
                .groups
                .iter()
                .map(String::as_str)
                .filter(|g| limits.contains_key(g))
                .collect();
            if budgets.is_empty() {
                continue;
            }
            budgets.sort_unstable();
            if !m.takes_threads {
                if let Some(why) = &m.why_not {
                    split.warnings.push(format!(
                        "node {}: core budget {} doesn't apply to it: {why}",
                        m.id,
                        budgets.join(", ")
                    ));
                }
                continue;
            }
            match m.own {
                Some(0) => split.warnings.push(format!(
                    "node {}: runtime.threads 0 lets the plugin decide, so core budget {} doesn't limit it",
                    m.id,
                    budgets.join(", ")
                )),
                Some(n) => {
                    for b in &budgets {
                        *used.get_mut(b).expect("a known budget") += u64::from(n);
                    }
                }
                None => {
                    for b in &budgets {
                        *waiting.get_mut(b).expect("a known budget") += 1;
                    }
                    classes.entry(budgets).or_default().push(m.id);
                }
            }
        }
        for (id, limit) in &limits {
            let (explicit, free) = (used[id], waiting[id]);
            if explicit > *limit {
                split.budget.push(format!(
                    "group {id}: its members' own runtime.threads add up to {explicit}, over its limits.cpu of {limit}"
                ));
            } else if explicit + free > *limit {
                split.budget.push(format!(
                    "group {id}: limits.cpu {limit} leaves {} core(s) for {free} member(s) without their own runtime.threads: each needs at least 1",
                    limit - explicit
                ));
            }
        }
        if !split.budget.is_empty() {
            return split;
        }
        let mut share: BTreeMap<&Vec<&str>, u32> = classes.keys().map(|k| (k, 1)).collect();
        for (budgets, ids) in &classes {
            for b in budgets {
                *used.get_mut(b).expect("a known budget") += ids.len() as u64;
            }
        }
        let deepest = classes.keys().map(Vec::len).max().unwrap_or(0);
        for depth in (1..=deepest).rev() {
            loop {
                let mut grew = false;
                for (budgets, ids) in classes.iter().filter(|(k, _)| k.len() == depth) {
                    let size = ids.len() as u64;
                    let fits = budgets.iter().all(|b| used[b] + size <= limits[b]);
                    let current = share.get_mut(budgets).expect("a known class");
                    if fits && *current < MAX_THREADS {
                        *current += 1;
                        for b in budgets {
                            *used.get_mut(b).expect("a known budget") += size;
                        }
                        grew = true;
                    }
                }
                if !grew {
                    break;
                }
            }
        }
        let by_id: BTreeMap<String, Vec<String>> = members
            .iter()
            .map(|m| (m.id.to_string(), m.groups.to_vec()))
            .collect();
        for (budgets, ids) in &classes {
            for id in ids {
                let innermost = groups::of_member(groups, &by_id, id, Family::Resources)
                    .into_iter()
                    .find(|g| budgets.contains(&g.id.as_str()))
                    .map_or_else(|| budgets[0].to_string(), |g| g.id.clone());
                split
                    .shares
                    .insert((*id).to_string(), (share[budgets], innermost));
            }
        }
        split
    }
}

/// Where a node's core count comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ThreadsSource {
    /// The node's own `runtime.threads`.
    Node,
    /// Its share of a `resources` group's `limits.cpu`.
    Group,
    /// The plugin manifest's `runtime_defaults`.
    Manifest,
    /// `[director] plugin_threads`.
    Server,
    /// Nothing sets one: the plugin decides.
    Plugin,
}

/// The core count a node effectively gets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub(crate) struct NodeThreads {
    /// `null`: the plugin decides.
    pub threads: Option<u32>,
    pub source: ThreadsSource,
    /// The budget it's a share of (`source: group`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
}

/// A device a node's settings claim (`x-flwrs-device`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub(crate) struct NodeDevice {
    /// What sort of device, e.g. `camera`.
    pub kind: String,
    /// As the node names it. Claims are compared by this string: two names
    /// for one device aren't recognised as the same.
    pub device: String,
    /// The launch-args setting it comes from.
    pub field: String,
}

/// A node's core count: its own value, its group share, the manifest's
/// default, then `[director] plugin_threads`.
pub(crate) fn effective_threads(
    runtime: &RuntimeConfig,
    share: Option<&(u32, String)>,
    manifest: &RuntimeConfig,
    server: Option<u32>,
) -> NodeThreads {
    let (threads, source, group) = if let Some(n) = runtime.threads {
        (n, ThreadsSource::Node, None)
    } else if let Some((n, group)) = share {
        (*n, ThreadsSource::Group, Some(group.clone()))
    } else if let Some(n) = manifest.threads {
        (n, ThreadsSource::Manifest, None)
    } else if let Some(n) = server {
        (n, ThreadsSource::Server, None)
    } else {
        (0, ThreadsSource::Plugin, None)
    };
    NodeThreads {
        threads: (threads > 0).then_some(threads),
        source,
        group,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn budget(id: &str, cpu: u32) -> Group {
        serde_json::from_value(
            json!({"id": id, "family": "resources", "policy": {"limits": {"cpu": cpu}}}),
        )
        .unwrap()
    }

    /// (id, groups, own threads); all take threads.
    fn split(groups: &[Group], nodes: &[(&str, &[&str], Option<u32>)]) -> Split {
        let owned: Vec<(String, Vec<String>, Option<u32>)> = nodes
            .iter()
            .map(|(id, g, own)| {
                (
                    id.to_string(),
                    g.iter().map(|s| s.to_string()).collect(),
                    *own,
                )
            })
            .collect();
        let members: Vec<Member> = owned
            .iter()
            .map(|(id, g, own)| Member {
                id,
                groups: g,
                own: *own,
                takes_threads: true,
                why_not: None,
            })
            .collect();
        Split::of(groups, &members)
    }

    fn shares(s: &Split) -> Vec<(&str, u32)> {
        s.shares
            .iter()
            .map(|(id, (n, _))| (id.as_str(), *n))
            .collect()
    }

    #[test]
    fn one_budget_is_split_equally_after_own_values() {
        let s = split(
            &[budget("b", 8)],
            &[
                ("a", &["b"], Some(2)),
                ("c", &["b"], None),
                ("d", &["b"], None),
            ],
        );
        assert_eq!(shares(&s), [("c", 3), ("d", 3)]);
        // A remainder that can't go round stays unused.
        let s = split(
            &[budget("b", 8)],
            &[
                ("a", &["b"], None),
                ("c", &["b"], None),
                ("d", &["b"], None),
            ],
        );
        assert_eq!(shares(&s), [("a", 2), ("c", 2), ("d", 2)]);
    }

    #[test]
    fn an_inner_budget_is_spent_first_and_the_outer_one_holds() {
        let g = [budget("inner", 8), budget("outer", 10)];
        let s = split(
            &g,
            &[
                ("a", &["outer", "inner"], None),
                ("b", &["outer", "inner"], None),
                ("c", &["outer"], None),
            ],
        );
        assert_eq!(shares(&s), [("a", 4), ("b", 4), ("c", 2)]);
        assert_eq!(s.shares["a"].1, "inner");
        assert_eq!(s.shares["c"].1, "outer");
        // An enclosing budget smaller than the inner one caps it, keeping 1
        // for each other member.
        let g = [budget("inner", 8), budget("outer", 4)];
        let s = split(
            &g,
            &[
                ("a", &["outer", "inner"], None),
                ("b", &["outer", "inner"], None),
                ("c", &["outer"], None),
            ],
        );
        assert_eq!(shares(&s), [("a", 1), ("b", 1), ("c", 2)]);
    }

    #[test]
    fn budgets_that_dont_add_up_are_problems() {
        let s = split(
            &[budget("b", 3)],
            &[("a", &["b"], Some(2)), ("c", &["b"], Some(2))],
        );
        assert!(s.budget[0].contains("add up to 4"), "{:?}", s.budget);
        let s = split(
            &[budget("b", 2)],
            &[("a", &["b"], Some(2)), ("c", &["b"], None)],
        );
        assert!(s.budget[0].contains("at least 1"), "{:?}", s.budget);
        assert!(s.shares.is_empty());
    }

    #[test]
    fn members_it_cant_apply_to_are_warned_about_and_not_counted() {
        let g = [budget("b", 2)];
        let (a, c, groups) = ("a".to_string(), "c".to_string(), vec!["b".to_string()]);
        let members = [
            Member {
                id: &a,
                groups: &groups,
                own: Some(100),
                takes_threads: false,
                why_not: Some("it's a remote node".into()),
            },
            Member {
                id: &c,
                groups: &groups,
                own: None,
                takes_threads: true,
                why_not: None,
            },
        ];
        let s = Split::of(&g, &members);
        assert!(s.budget.is_empty(), "{:?}", s.budget);
        assert!(s.warnings[0].contains("remote"), "{:?}", s.warnings);
        assert_eq!(s.shares["c"].0, 2);
    }

    #[test]
    fn placement_and_execution_are_refused() {
        let place: Group = serde_json::from_value(
            json!({"id": "p", "family": "placement", "policy": {"strategy": "pack"}}),
        )
        .unwrap();
        let exec: Group = serde_json::from_value(
            json!({"id": "e", "family": "execution", "policy": {"threads": 2}}),
        )
        .unwrap();
        let problems = settings_problems(&[place, exec, budget("b", MAX_THREADS + 1)]);
        assert_eq!(problems.len(), 3, "{problems:?}");
        assert!(problems[0].contains("one server"));
        assert!(problems[1].contains("its own process"));
        assert!(problems[2].contains("can't exceed"));
    }

    #[test]
    fn precedence_is_node_group_manifest_server() {
        let with = |n| RuntimeConfig {
            threads: n,
            ..Default::default()
        };
        let share = (3, "b".to_string());
        let t = |own, s: Option<&(u32, String)>, m, srv| {
            let e = effective_threads(&with(own), s, &with(m), srv);
            (e.threads, e.source)
        };
        assert_eq!(
            t(Some(5), Some(&share), Some(7), Some(9)),
            (Some(5), ThreadsSource::Node)
        );
        assert_eq!(
            t(None, Some(&share), Some(7), Some(9)),
            (Some(3), ThreadsSource::Group)
        );
        assert_eq!(
            t(None, None, Some(7), Some(9)),
            (Some(7), ThreadsSource::Manifest)
        );
        assert_eq!(
            t(None, None, None, Some(9)),
            (Some(9), ThreadsSource::Server)
        );
        assert_eq!(t(None, None, None, None), (None, ThreadsSource::Plugin));
        assert_eq!(
            t(Some(0), Some(&share), None, None),
            (None, ThreadsSource::Node)
        );
    }
}
