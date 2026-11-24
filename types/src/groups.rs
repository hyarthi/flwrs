//! Groups: named sets of members with a policy, in one of three families.
//! The same shape at two levels: a scene's nodes, and the inner nodes of a
//! plugin's graph. Each level accepts the policies it can honour and refuses
//! the rest. Membership is stored on the member (`groups: [id, …]`).

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// The longest group id.
pub const MAX_ID_LEN: usize = 64;

/// A group, as `{"id", "family", "policy"}`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "RawGroup", into = "RawGroup")]
pub struct Group {
    pub id: String,
    pub policy: Policy,
}

/// What a group decides.
#[derive(Clone, Debug, PartialEq)]
pub enum Policy {
    Placement(Placement),
    Resources(Resources),
    Execution(Execution),
}

/// A group's family.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Family {
    Placement,
    Resources,
    Execution,
}

impl Family {
    pub fn name(self) -> &'static str {
        match self {
            Family::Placement => "placement",
            Family::Resources => "resources",
            Family::Execution => "execution",
        }
    }
}

impl Policy {
    pub fn family(&self) -> Family {
        match self {
            Policy::Placement(_) => Family::Placement,
            Policy::Resources(_) => Family::Resources,
            Policy::Execution(_) => Family::Execution,
        }
    }
}

/// Where members run relative to each other.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Placement {
    pub strategy: Strategy,
    /// A rule that must hold; otherwise a preference.
    #[serde(default)]
    pub required: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Strategy {
    /// Together.
    Pack,
    /// Apart.
    Spread,
}

/// How much the members may use, together.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resources {
    pub limits: Limits,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    /// Cores.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu: Option<u32>,
}

/// How members run. What's left out is the level's default.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Execution {
    /// `cpu`, `gpu`, `npu`: the level says which it has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_fallback: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<Mode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threads: Option<u32>,
    /// Higher goes first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<i32>,
}

/// How a group's work shares its threads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Sequential,
    Parallel,
    Batched,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawGroup {
    id: String,
    family: Family,
    policy: Value,
}

impl TryFrom<RawGroup> for Group {
    type Error = String;

    fn try_from(raw: RawGroup) -> Result<Self, String> {
        fn parse<T: serde::de::DeserializeOwned>(v: Value, f: Family) -> Result<T, String> {
            serde_json::from_value(v).map_err(|e| format!("{} policy: {e}", f.name()))
        }
        let policy = match raw.family {
            f @ Family::Placement => Policy::Placement(parse(raw.policy, f)?),
            f @ Family::Resources => Policy::Resources(parse(raw.policy, f)?),
            f @ Family::Execution => Policy::Execution(parse(raw.policy, f)?),
        };
        Ok(Group { id: raw.id, policy })
    }
}

impl From<Group> for RawGroup {
    fn from(g: Group) -> Self {
        let family = g.policy.family();
        let policy = match &g.policy {
            Policy::Placement(p) => serde_json::to_value(p),
            Policy::Resources(p) => serde_json::to_value(p),
            Policy::Execution(p) => serde_json::to_value(p),
        }
        .expect("policies serialize");
        RawGroup {
            id: g.id,
            family,
            policy,
        }
    }
}

fn id_problem(id: &str) -> Option<String> {
    let mut chars = id.chars();
    let ok = id.len() <= MAX_ID_LEN
        && chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    (!ok).then(|| {
        format!(
            "group id {id:?}: use lowercase letters, digits, _ and -, starting with a letter, at most {MAX_ID_LEN} characters"
        )
    })
}

/// Every problem with a level's groups and who's in them: ids; a `limits.cpu`
/// or `threads` of 0; memberships naming a group that doesn't exist; and,
/// within a family, groups that overlap without one containing the other
/// (it would be unclear which budget or policy applies). Groups of
/// different families may overlap freely.
pub fn check(groups: &[Group], members: &BTreeMap<String, Vec<String>>) -> Vec<String> {
    let mut problems = vec![];
    let mut known: BTreeMap<&str, Family> = BTreeMap::new();
    for g in groups {
        problems.extend(id_problem(&g.id));
        if known.insert(&g.id, g.policy.family()).is_some() {
            problems.push(format!("group {:?} is defined twice", g.id));
        }
        match &g.policy {
            Policy::Resources(r) if r.limits.cpu == Some(0) => {
                problems.push(format!("group {}: limits.cpu must be at least 1", g.id));
            }
            Policy::Execution(e) if e.threads == Some(0) => {
                problems.push(format!("group {}: threads must be at least 1", g.id));
            }
            _ => {}
        }
    }
    let mut of_group: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for (member, ids) in members {
        let mut seen = BTreeSet::new();
        for id in ids {
            if !seen.insert(id) {
                problems.push(format!("{member}: in group {id:?} twice"));
            }
            if known.contains_key(id.as_str()) {
                of_group.entry(id).or_default().insert(member);
            } else {
                problems.push(format!("{member}: there's no group {id:?}"));
            }
        }
    }
    let ids: Vec<&str> = of_group.keys().copied().collect();
    for (i, a) in ids.iter().enumerate() {
        for b in &ids[i + 1..] {
            if known[a] != known[b] {
                continue;
            }
            let (x, y) = (&of_group[a], &of_group[b]);
            if !x.is_disjoint(y) && !x.is_subset(y) && !y.is_subset(x) {
                let shared: Vec<&str> = x.intersection(y).copied().collect();
                problems.push(format!(
                    "{} groups {a} and {b} overlap ({}): nest one inside the other, or share no members",
                    known[a].name(),
                    shared.join(", ")
                ));
            }
        }
    }
    problems
}

/// The groups a member is in that are of `family`, innermost (fewest
/// members) first.
pub fn of_member<'a>(
    groups: &'a [Group],
    members: &BTreeMap<String, Vec<String>>,
    member: &str,
    family: Family,
) -> Vec<&'a Group> {
    let Some(ids) = members.get(member) else {
        return vec![];
    };
    let size = |id: &str| {
        members
            .values()
            .filter(|m| m.iter().any(|g| g == id))
            .count()
    };
    let mut found: Vec<&Group> = groups
        .iter()
        .filter(|g| g.policy.family() == family && ids.contains(&g.id))
        .collect();
    found.sort_by_key(|g| (size(&g.id), g.id.clone()));
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn group(v: Value) -> Group {
        serde_json::from_value(v).unwrap()
    }

    fn cpu(id: &str, n: u32) -> Group {
        group(json!({"id": id, "family": "resources", "policy": {"limits": {"cpu": n}}}))
    }

    fn members(m: &[(&str, &[&str])]) -> BTreeMap<String, Vec<String>> {
        m.iter()
            .map(|(k, v)| (k.to_string(), v.iter().map(|s| s.to_string()).collect()))
            .collect()
    }

    #[test]
    fn the_agreed_shapes_round_trip() {
        for v in [
            json!({"id": "vision", "family": "placement", "policy": {"strategy": "pack", "required": true}}),
            json!({"id": "edge-budget", "family": "resources", "policy": {"limits": {"cpu": 4}}}),
            json!({"id": "pose", "family": "execution", "policy": {"device": "gpu", "mode": "parallel", "threads": 2, "priority": 10}}),
        ] {
            assert_eq!(serde_json::to_value(group(v.clone())).unwrap(), v);
        }
    }

    #[test]
    fn unknown_keys_and_a_policy_of_another_family_are_refused() {
        for v in [
            json!({"id": "a", "family": "placement", "policy": {"strategy": "pack", "weight": 1}}),
            json!({"id": "a", "family": "resources", "policy": {"requests": {"cpu": 1}, "limits": {}}}),
            json!({"id": "a", "family": "execution", "policy": {"strategy": "pack"}}),
            json!({"id": "a", "family": "other", "policy": {}}),
            json!({"id": "a", "family": "execution", "policy": {}, "extra": 1}),
        ] {
            assert!(serde_json::from_value::<Group>(v.clone()).is_err(), "{v}");
        }
    }

    #[test]
    fn groups_of_one_family_nest_or_stay_apart() {
        let groups = [cpu("all", 8), cpu("half", 4), cpu("other", 2)];
        let nested = members(&[("a", &["all", "half"]), ("b", &["all"]), ("c", &["other"])]);
        assert_eq!(check(&groups, &nested), Vec::<String>::new());
        let crossing = members(&[("a", &["all", "half"]), ("b", &["all"]), ("c", &["half"])]);
        let problems = check(&groups, &crossing);
        assert!(
            problems[0].contains("all and half overlap (a)"),
            "{problems:?}"
        );
    }

    #[test]
    fn families_overlap_freely() {
        let exec = group(json!({"id": "fast", "family": "execution", "policy": {"threads": 2}}));
        let groups = [cpu("budget", 4), exec];
        let m = members(&[
            ("a", &["budget", "fast"]),
            ("b", &["budget"]),
            ("c", &["fast"]),
        ]);
        assert_eq!(check(&groups, &m), Vec::<String>::new());
    }

    #[test]
    fn bad_ids_values_and_references_are_reported() {
        let groups = [cpu("Bad Id", 0), cpu("dup", 1), cpu("dup", 1)];
        let problems = check(&groups, &members(&[("a", &["nope", "dup", "dup"])]));
        for want in [
            "group id",
            "limits.cpu",
            "defined twice",
            "no group",
            "twice",
        ] {
            assert!(
                problems.iter().any(|p| p.contains(want)),
                "{want}: {problems:?}"
            );
        }
    }

    #[test]
    fn a_members_groups_come_innermost_first() {
        let groups = [cpu("all", 8), cpu("half", 4)];
        let m = members(&[("a", &["all", "half"]), ("b", &["all"])]);
        let found = of_member(&groups, &m, "a", Family::Resources);
        assert_eq!(found[0].id, "half");
        assert_eq!(found[1].id, "all");
    }
}
