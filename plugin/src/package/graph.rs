//! Graph catalogs (`x-flwrs-graph`): a package file telling a generic editor
//! how to draw and edit a launch arg that holds a graph of typed nodes. The
//! format is specified in `docs/plugin-spec.md`.
//!
//! The graph itself keeps one shape: `{"nodes": {id: {"type": …, …}},
//! "links": [{"from": "<node>.<port>", "to": "<node>.<port>"}], "groups":
//! […]}`. The catalog names the node types, where each one's settings live in
//! the launch-args schema (no second copy), their ports and which port types
//! connect.

use super::manifest::parse_pointer;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// The catalog format this module reads.
pub const CATALOG_FORMAT: u32 = 1;

/// A graph catalog.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Catalog {
    /// [`CATALOG_FORMAT`].
    pub format: u32,
    /// What can travel on a link, by id.
    pub port_types: BTreeMap<String, PortType>,
    /// Links between different port types (a port always takes its own type).
    #[serde(default)]
    pub connects: Vec<Connects>,
    pub node_types: BTreeMap<String, NodeType>,
    /// The top-level list upload setting that holds the files the graph's
    /// nodes name (an editor adds a picked file to it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files_field: Option<String>,
    /// Which node types can join the graph's groups, if it has any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub groups: Option<Groups>,
    /// The top-level setting that turns the plugin's live channels
    /// (`LIVE_FEED`) on: an editor points to it while a node has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_setting: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PortType {
    pub title: String,
}

/// An output of type `from` may link to an input of type `to`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Connects {
    pub from: String,
    pub to: String,
    /// Shown on such a link.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub badge: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct NodeType {
    pub title: String,
    /// The palette's section.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// A local reference into the launch-args schema: `#/$defs/<name>`.
    pub settings: String,
    /// What a new node of this type starts with (besides its `type`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub defaults: Option<Value>,
    /// At most this many in one graph.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<u32>,
    #[serde(default)]
    pub inputs: Vec<Port>,
    #[serde(default)]
    pub outputs: Vec<Port>,
    /// Top-level settings of the node that name one of the node's uploaded
    /// files.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub file_settings: Vec<String>,
    /// Where settings proposed for a picked file come from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposal: Option<Proposal>,
    /// Shown on the links into a node of this type.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub link_badges: Vec<LinkBadge>,
}

/// A port. Its type is fixed (`type`) or read from the node's settings
/// (`type_from`); `name` defaults to the type's id.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Port {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub port_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub type_from: Option<TypeFrom>,
    /// The node has this port only under this condition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<Condition>,
}

/// A port type picked by a value in the node's settings.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TypeFrom {
    /// JSON pointer into the node.
    pub pointer: String,
    /// Value → port type.
    pub map: BTreeMap<String, String>,
}

/// A test on a value in the node's settings: exactly one of `present`,
/// `equals`, `not_equals`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Condition {
    /// JSON pointer into the node.
    pub pointer: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub present: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub equals: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_equals: Option<Value>,
    /// The value when the setting is left out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
}

/// Settings proposed for a file: a configured field holds an object keyed by
/// file name, and each entry the proposal at a pointer.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Proposal {
    /// A top-level launch arg the configurator sets.
    pub field: String,
    /// The node's setting naming the file (one of `file_settings`).
    pub file_setting: String,
    /// JSON pointer to the proposed settings in the field's entry.
    pub at: String,
}

/// A label on the links into a node, while its condition holds. `{value}` in
/// the label stands for the value at the condition's pointer.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LinkBadge {
    pub label: String,
    pub when: Condition,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Groups {
    /// The node types that can be members.
    pub node_types: Vec<String>,
    /// The families the graph accepts.
    pub families: Vec<String>,
    /// A local reference into the launch-args schema: `#/$defs/<name>`.
    pub settings: String,
}

impl Catalog {
    /// Reads a catalog and checks it against the launch-args schema. Every
    /// problem is returned.
    pub fn parse(text: &str, schema: &Value) -> Result<Catalog, Vec<String>> {
        let catalog: Catalog = serde_json::from_str(text).map_err(|e| vec![e.to_string()])?;
        let problems = catalog.problems(schema);
        if problems.is_empty() {
            Ok(catalog)
        } else {
            Err(problems)
        }
    }

    fn problems(&self, schema: &Value) -> Vec<String> {
        let mut out = vec![];
        if self.format != CATALOG_FORMAT {
            out.push(format!(
                "format {} isn't known (this server reads {CATALOG_FORMAT})",
                self.format
            ));
        }
        if self.port_types.is_empty() {
            out.push("port_types is empty".into());
        }
        if self.node_types.is_empty() {
            out.push("node_types is empty".into());
        }
        let known = |t: &str| self.port_types.contains_key(t);
        for (i, c) in self.connects.iter().enumerate() {
            for end in [&c.from, &c.to] {
                if !known(end) {
                    out.push(format!("connects[{i}]: there's no port type {end:?}"));
                }
            }
            if c.from == c.to {
                out.push(format!(
                    "connects[{i}]: a port type always connects to itself"
                ));
            }
        }
        if let Some(field) = &self.files_field
            && schema
                .pointer(&format!("/properties/{}", escape(field)))
                .is_none()
        {
            out.push(format!("files_field: the launch args have no {field:?}"));
        }
        if let Some(field) = &self.live_setting
            && schema
                .pointer(&format!("/properties/{}", escape(field)))
                .is_none()
        {
            out.push(format!("live_setting: the launch args have no {field:?}"));
        }
        for (id, node) in &self.node_types {
            self.check_node(id, node, schema, &mut out);
        }
        if let Some(groups) = &self.groups {
            for t in &groups.node_types {
                if !self.node_types.contains_key(t) {
                    out.push(format!("groups.node_types: there's no node type {t:?}"));
                }
            }
            if groups.families.is_empty() {
                out.push("groups.families is empty".into());
            }
            if resolve(schema, &groups.settings).is_none() {
                out.push(format!(
                    "groups.settings: {:?} isn't in the launch-args schema",
                    groups.settings
                ));
            }
        }
        out
    }

    fn check_node(&self, id: &str, node: &NodeType, schema: &Value, out: &mut Vec<String>) {
        let at = format!("node_types.{id}");
        let settings = resolve(schema, &node.settings);
        match settings {
            None => out.push(format!(
                "{at}.settings: {:?} isn't in the launch-args schema (expected #/$defs/<name>)",
                node.settings
            )),
            Some(s) => {
                if s.pointer("/properties/type/const").and_then(Value::as_str) != Some(id) {
                    out.push(format!(
                        "{at}.settings: its `type` property must be the constant {id:?}"
                    ));
                }
                for key in &node.file_settings {
                    if s.pointer(&format!("/properties/{}", escape(key))).is_none() {
                        out.push(format!("{at}.file_settings: there's no setting {key:?}"));
                    }
                }
            }
        }
        if let Some(defaults) = &node.defaults {
            if !defaults.is_object() {
                out.push(format!("{at}.defaults must be an object"));
            } else if defaults.get("type").is_some() {
                out.push(format!("{at}.defaults can't set `type`"));
            }
        }
        if node.max == Some(0) {
            out.push(format!("{at}.max must be at least 1"));
        }
        for (side, ports) in [("inputs", &node.inputs), ("outputs", &node.outputs)] {
            for (i, port) in ports.iter().enumerate() {
                self.check_port(&format!("{at}.{side}[{i}]"), port, out);
            }
        }
        if let Some(p) = &node.proposal {
            if !node.file_settings.contains(&p.file_setting) {
                out.push(format!(
                    "{at}.proposal.file_setting: {:?} isn't in file_settings",
                    p.file_setting
                ));
            }
            let configured = schema
                .pointer(&format!("/properties/{}", escape(&p.field)))
                .is_some();
            if !configured {
                out.push(format!(
                    "{at}.proposal.field: the launch args have no {:?}",
                    p.field
                ));
            }
            pointer_problem(&format!("{at}.proposal.at"), &p.at, out);
        }
        for (i, b) in node.link_badges.iter().enumerate() {
            let at = format!("{at}.link_badges[{i}]");
            if b.label.is_empty() {
                out.push(format!("{at}.label is empty"));
            }
            condition_problems(&format!("{at}.when"), &b.when, out);
        }
    }

    fn check_port(&self, at: &str, port: &Port, out: &mut Vec<String>) {
        let known = |t: &str| self.port_types.contains_key(t);
        match (&port.port_type, &port.type_from) {
            (Some(t), None) => {
                if !known(t) {
                    out.push(format!("{at}: there's no port type {t:?}"));
                }
            }
            (None, Some(from)) => {
                pointer_problem(&format!("{at}.type_from.pointer"), &from.pointer, out);
                if from.map.is_empty() {
                    out.push(format!("{at}.type_from.map is empty"));
                }
                for t in from.map.values() {
                    if !known(t) {
                        out.push(format!("{at}.type_from: there's no port type {t:?}"));
                    }
                }
            }
            _ => out.push(format!("{at}: set exactly one of type and type_from")),
        }
        if let Some(when) = &port.when {
            condition_problems(&format!("{at}.when"), when, out);
        }
    }

    /// Node types and their ports' names, sorted (for a plugin's own tests).
    pub fn port_names(&self, node_type: &str) -> Option<BTreeSet<String>> {
        let node = self.node_types.get(node_type)?;
        Some(
            node.inputs
                .iter()
                .chain(&node.outputs)
                .filter_map(|p| p.name.clone().or_else(|| p.port_type.clone()))
                .collect(),
        )
    }
}

fn condition_problems(at: &str, c: &Condition, out: &mut Vec<String>) {
    pointer_problem(&format!("{at}.pointer"), &c.pointer, out);
    let tests = usize::from(c.present.is_some())
        + usize::from(c.equals.is_some())
        + usize::from(c.not_equals.is_some());
    if tests != 1 {
        out.push(format!(
            "{at}: set exactly one of present, equals and not_equals"
        ));
    }
}

fn pointer_problem(at: &str, pointer: &str, out: &mut Vec<String>) {
    if pointer.is_empty() {
        out.push(format!("{at} must not be empty"));
    } else if let Err(e) = parse_pointer(pointer) {
        out.push(format!("{at}: {e}"));
    }
}

/// `#/$defs/<name>` in the schema.
fn resolve<'a>(schema: &'a Value, reference: &str) -> Option<&'a Value> {
    let pointer = reference.strip_prefix('#')?;
    pointer
        .starts_with("/$defs/")
        .then(|| schema.pointer(pointer))
        .flatten()
}

fn escape(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> Value {
        json!({
            "properties": {"info": {"type": "object"}},
            "$defs": {
                "a": {"properties": {"type": {"const": "a"}, "file": {"type": "string"}}},
                "b": {"properties": {"type": {"const": "b"}}},
                "group": {"type": "object"}
            }
        })
    }

    fn valid() -> Value {
        json!({
            "format": 1,
            "port_types": {"x": {"title": "X"}, "y": {"title": "Y"}},
            "connects": [{"from": "y", "to": "x", "badge": "for each"}],
            "node_types": {
                "a": {
                    "title": "A",
                    "settings": "#/$defs/a",
                    "defaults": {"file": ""},
                    "max": 1,
                    "outputs": [{"type_from": {"pointer": "/file", "map": {"f": "x"}}}],
                    "file_settings": ["file"],
                    "proposal": {"field": "info", "file_setting": "file", "at": "/proposal"}
                },
                "b": {
                    "title": "B",
                    "settings": "#/$defs/b",
                    "inputs": [{"name": "in", "type": "x", "when": {"pointer": "/m", "not_equals": 1, "default": 1}}],
                    "link_badges": [{"label": "every {value}", "when": {"pointer": "/n", "present": true}}]
                }
            },
            "groups": {"node_types": ["a"], "families": ["execution"], "settings": "#/$defs/group"}
        })
    }

    fn problems(change: impl FnOnce(&mut Value)) -> Vec<String> {
        let mut c = valid();
        change(&mut c);
        Catalog::parse(&c.to_string(), &schema())
            .err()
            .unwrap_or_default()
    }

    #[test]
    fn a_valid_catalog_reads() {
        assert_eq!(problems(|_| {}), Vec::<String>::new());
    }

    #[test]
    fn unknown_keys_and_bad_json_are_refused() {
        assert!(!problems(|c| c["extra"] = json!(1)).is_empty());
        assert!(!problems(|c| c["node_types"]["a"]["extra"] = json!(1)).is_empty());
        assert!(Catalog::parse("{", &schema()).is_err());
    }

    #[test]
    fn every_reference_is_checked() {
        type Change = fn(&mut Value);
        let cases: Vec<(&str, Change)> = vec![
            ("format", |c| c["format"] = json!(2)),
            ("no port type", |c| c["connects"][0]["to"] = json!("z")),
            ("always connects", |c| c["connects"][0]["to"] = json!("y")),
            ("isn't in the launch-args", |c| {
                c["node_types"]["a"]["settings"] = json!("#/$defs/nope")
            }),
            ("constant", |c| {
                c["node_types"]["a"]["settings"] = json!("#/$defs/b")
            }),
            ("no setting", |c| {
                c["node_types"]["a"]["file_settings"] = json!(["nope"])
            }),
            ("can't set", |c| {
                c["node_types"]["a"]["defaults"] = json!({"type": "a"})
            }),
            ("max", |c| c["node_types"]["a"]["max"] = json!(0)),
            ("exactly one of type", |c| {
                c["node_types"]["b"]["inputs"][0]["type_from"] =
                    json!({"pointer": "/x", "map": {"a": "x"}})
            }),
            ("no port type", |c| {
                c["node_types"]["a"]["outputs"][0]["type_from"]["map"]["f"] = json!("z")
            }),
            ("exactly one of present", |c| {
                c["node_types"]["b"]["inputs"][0]["when"]["equals"] = json!(2)
            }),
            ("isn't in file_settings", |c| {
                c["node_types"]["a"]["proposal"]["file_setting"] = json!("other")
            }),
            ("have no", |c| {
                c["node_types"]["a"]["proposal"]["field"] = json!("nope")
            }),
            ("files_field", |c| c["files_field"] = json!("nope")),
            ("live_setting", |c| c["live_setting"] = json!("nope")),
            ("no node type", |c| c["groups"]["node_types"] = json!(["z"])),
            ("families", |c| c["groups"]["families"] = json!([])),
        ];
        for (expected, change) in cases {
            let found = problems(change);
            assert!(
                found.iter().any(|p| p.contains(expected)),
                "{expected}: {found:?}"
            );
        }
    }
}
