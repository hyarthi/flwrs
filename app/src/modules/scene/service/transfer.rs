//! Scene export and import (`.flwr` files), for moving a scene to another
//! server. An import gets new ids, arrives disabled, and goes through even if
//! this server lacks the plugins it names: those show up as problems in
//! [`Service::check`] until they're installed.

use crate::modules::node_config::SceneDefaults;
use super::query_sqlite::{self, EdgeValues, NodeValues};
use super::*;
use crate::modules::node_config::LaunchMode;
use crate::modules::plugin::manifest::is_valid_id;
use crate::modules::scene::scene_file::{self, pb};
use std::collections::HashSet;

const MAX_NODES: usize = 10_000;
const MAX_EDGES: usize = 100_000;
const MAX_VERSION_CHARS: usize = 64;

/// An exported scene, ready to download.
pub(crate) struct Export {
    pub file_name: String,
    pub bytes: Vec<u8>,
}

/// An imported scene and what it still needs before it can run here.
pub(crate) struct Imported {
    pub scene: Scene,
    pub check: SceneCheck,
}

impl Service {
    /// Serializes a scene with its whole graph. Remote nodes always carry
    /// their `remote_id` (their node id if none was set), so remote plugins
    /// keep working after an import, where node ids change. Tokens are left
    /// out unless `include_secrets`.
    pub(crate) async fn export_scene(
        &self,
        id: &str,
        include_secrets: bool,
    ) -> Result<Export, ServiceError> {
        let _guard = self.lock.lock().await;
        let mut conn = self.conn().await?;
        let scene = query_sqlite::get_scene(&mut conn, id)
            .await
            .map_err(not_found("scene"))?;
        let mut nodes = vec![];
        for row in query_sqlite::list_nodes(&mut conn, id).await? {
            let node = Node::try_from(row)?;
            let mut config = node.config.clone();
            if config.launch == LaunchMode::Remote {
                config.remote_id.get_or_insert_with(|| node.id.clone());
            }
            if !include_secrets {
                config.token = None;
            }
            nodes.push(pb::Node {
                key: node.id,
                name: node.name,
                r#type: plugin_type_to_pb(&node.node_type) as i32,
                plugin_id: node.plugin_id,
                plugin_version: node.plugin_version.unwrap_or_default(),
                config_json: serde_json::to_string(&config).expect("config serializes"),
            });
        }
        let edges = query_sqlite::list_edges(&mut conn, id)
            .await?
            .into_iter()
            .map(|e| pb::Edge {
                name: e.name,
                source_key: e.source_node_id,
                target_key: e.target_node_id,
                mapping_json: e.mapping,
            })
            .collect();
        let file = pb::SceneFile {
            name: scene.name.clone(),
            max_hops: scene.max_hops,
            nodes,
            edges,
            export_time: Some(now()),
            includes_secrets: include_secrets,
            exporter_version: env!("CARGO_PKG_VERSION").into(),
            defaults_json: (scene.defaults != SceneDefaults::default())
                .then(|| serde_json::to_string(&scene.defaults).expect("defaults serialize")),
        };
        log::info!(
            "Scene service: exported scene [{id}]{}",
            if include_secrets { " with secrets" } else { "" }
        );
        Ok(Export {
            file_name: scene_file::file_name(&scene.name),
            bytes: scene_file::encode(&file),
        })
    }

    /// Creates a new (disabled) scene from a `.flwr` file, with new ids. The
    /// file's structure is checked strictly (every problem reported, nothing
    /// stored on failure); whether this server's plugins fit is not: that's
    /// reported in [`Imported::check`].
    pub(crate) async fn import_scene(&self, bytes: &[u8]) -> Result<Imported, ServiceError> {
        let file = scene_file::decode(bytes).map_err(|e| ServiceError::Invalid(vec![e]))?;
        let plan = plan_import(file)?;

        let _guard = self.lock.lock().await;
        let mut conn = self.conn().await?;
        let mut tx = conn.begin_with(WRITE_TX).await?;
        let scene_id = Ulid::generate().to_string();
        let scene = query_sqlite::create_scene(
            &mut tx,
            &scene_id,
            &plan.name,
            plan.max_hops,
            &plan.defaults,
        )
        .await?;
        // Ascending ids keep the file's node order in listings (all imported
        // nodes share one create_time).
        let mut generator = ulid::Generator::new();
        let mut next_id = || {
            generator
                .generate()
                .unwrap_or_else(|overflow| overflow.commit_overflow_increment())
                .to_string()
        };
        let mut ids: HashMap<&str, String> = HashMap::new();
        for node in &plan.nodes {
            let id = next_id();
            let values = NodeValues {
                name: node.name.as_deref(),
                node_type: node.node_type,
                plugin_id: &node.plugin_id,
                plugin_version: node.plugin_version.as_deref(),
                config: &node.config_json,
            };
            query_sqlite::insert_node(&mut tx, &scene_id, &id, &values).await?;
            ids.insert(node.key.as_str(), id);
        }
        for edge in &plan.edges {
            let values = EdgeValues {
                name: edge.name.as_deref(),
                source_node_id: &ids[edge.source_key.as_str()],
                target_node_id: &ids[edge.target_key.as_str()],
                mapping: edge.mapping_json.as_deref(),
            };
            query_sqlite::insert_edge(&mut tx, &scene_id, &next_id(), &values)
                .await
                .map_err(|e| match e.as_database_error().map(|d| d.kind()) {
                    Some(ErrorKind::UniqueViolation) => ServiceError::Invalid(vec![format!(
                        "edges: more than one edge {} → {}",
                        edge.source_key, edge.target_key
                    )]),
                    _ => e.into(),
                })?;
        }
        // Edge direction by node type, no `block` on a cycle.
        check_graph(&mut tx, &scene_id).await?;
        tx.commit().await?;

        let check = self.check_nodes(&mut conn, &scene_id).await?;
        log::info!(
            "Scene service: imported scene [{scene_id}] ({} nodes, {} edges, {} problems)",
            plan.nodes.len(),
            plan.edges.len(),
            check.problems.len()
        );
        Ok(Imported { scene, check })
    }
}

struct PlannedNode {
    key: String,
    name: Option<String>,
    node_type: &'static str,
    plugin_id: String,
    plugin_version: Option<String>,
    config_json: String,
}

struct PlannedEdge {
    name: Option<String>,
    source_key: String,
    target_key: String,
    mapping_json: Option<String>,
}

struct Plan {
    name: String,
    max_hops: Option<u32>,
    defaults: SceneDefaults,
    nodes: Vec<PlannedNode>,
    edges: Vec<PlannedEdge>,
}

/// Checks everything in the file that doesn't depend on this server's
/// plugins. Returns every problem at once.
fn plan_import(file: pb::SceneFile) -> Result<Plan, ServiceError> {
    let mut errors = vec![];
    let defaults = match file.defaults_json.as_deref().map(SceneDefaults::parse) {
        None => SceneDefaults::default(),
        Some(Ok(defaults)) => defaults,
        Some(Err(err)) => {
            errors.push(err);
            SceneDefaults::default()
        }
    };
    let (name, max_hops) = match check_scene(&SceneInput {
        name: file.name,
        max_hops: file.max_hops,
        defaults: defaults.clone(),
    }) {
        Ok(settings) => settings,
        Err(ServiceError::Invalid(problems)) => {
            errors.extend(problems);
            (String::new(), None)
        }
        Err(other) => return Err(other),
    };
    if file.nodes.len() > MAX_NODES {
        errors.push(format!("more than {MAX_NODES} nodes"));
    }
    if file.edges.len() > MAX_EDGES {
        errors.push(format!("more than {MAX_EDGES} edges"));
    }

    let mut keys = HashSet::new();
    let mut nodes = vec![];
    for (i, node) in file.nodes.into_iter().enumerate() {
        let label = format!("nodes[{i}] ({:?})", node.key);
        let mut node_errors = vec![];
        if node.key.is_empty() {
            node_errors.push("key is empty".to_string());
        } else if !keys.insert(node.key.clone()) {
            node_errors.push("key is used by another node".to_string());
        }
        let node_type = plugin_type_from_pb(node.r#type);
        if node_type.is_none() {
            node_errors.push("type must be SOURCE, SINK or TRANSFORM".to_string());
        }
        if !is_valid_id(&node.plugin_id) {
            node_errors.push(format!(
                "plugin_id {:?} isn't a valid plugin id",
                node.plugin_id
            ));
        }
        if node.plugin_version.chars().count() > MAX_VERSION_CHARS {
            node_errors.push(format!(
                "plugin_version is longer than {MAX_VERSION_CHARS} characters"
            ));
        }
        let name = optional_name(node.name, &mut node_errors);
        let config_json = match serde_json::from_str::<Value>(&node.config_json)
            .map_err(|e| format!("config isn't JSON: {e}"))
            .and_then(|value| NodeConfig::parse(&value))
        {
            Ok(config) => {
                node_errors.extend(config.validate_own());
                serde_json::to_string(&config).expect("config serializes")
            }
            Err(e) => {
                node_errors.push(e);
                String::new()
            }
        };
        errors.extend(node_errors.into_iter().map(|e| format!("{label}: {e}")));
        nodes.push(PlannedNode {
            key: node.key,
            name,
            node_type: node_type.unwrap_or("SINK"),
            plugin_id: node.plugin_id,
            plugin_version: Some(node.plugin_version).filter(|v| !v.is_empty()),
            config_json,
        });
    }

    let mut edges = vec![];
    for (i, edge) in file.edges.into_iter().enumerate() {
        let label = format!("edges[{i}] ({} → {})", edge.source_key, edge.target_key);
        let mut edge_errors = vec![];
        for (end, key) in [
            ("source_key", &edge.source_key),
            ("target_key", &edge.target_key),
        ] {
            if !keys.contains(key) {
                edge_errors.push(format!("{end} {key:?} isn't a node in the file"));
            }
        }
        let name = optional_name(edge.name, &mut edge_errors);
        let mapping_json = match edge
            .mapping_json
            .as_deref()
            .map(serde_json::from_str::<Value>)
        {
            None => None,
            Some(Err(e)) => {
                edge_errors.push(format!("mapping isn't JSON: {e}"));
                None
            }
            Some(Ok(value)) => match MappingSpec::parse(&value) {
                Ok(spec) => spec.map(|s| serde_json::to_string(&s).expect("mapping serializes")),
                Err(problems) => {
                    edge_errors.extend(problems);
                    None
                }
            },
        };
        errors.extend(edge_errors.into_iter().map(|e| format!("{label}: {e}")));
        edges.push(PlannedEdge {
            name,
            source_key: edge.source_key,
            target_key: edge.target_key,
            mapping_json,
        });
    }

    if errors.is_empty() {
        Ok(Plan {
            name,
            max_hops,
            defaults,
            nodes,
            edges,
        })
    } else {
        Err(ServiceError::Invalid(errors))
    }
}

fn plugin_type_to_pb(node_type: &str) -> pb::plugin_type::Enum {
    match node_type {
        "SOURCE" => pb::plugin_type::Enum::Source,
        "SINK" => pb::plugin_type::Enum::Sink,
        "TRANSFORM" => pb::plugin_type::Enum::Transform,
        _ => pb::plugin_type::Enum::Undefined,
    }
}

fn plugin_type_from_pb(value: i32) -> Option<&'static str> {
    match pb::plugin_type::Enum::try_from(value) {
        Ok(pb::plugin_type::Enum::Source) => Some("SOURCE"),
        Ok(pb::plugin_type::Enum::Sink) => Some("SINK"),
        Ok(pb::plugin_type::Enum::Transform) => Some("TRANSFORM"),
        _ => None,
    }
}

fn now() -> prost_types::Timestamp {
    let since = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    prost_types::Timestamp {
        seconds: i64::try_from(since.as_secs()).unwrap_or(i64::MAX),
        nanos: i32::try_from(since.subsec_nanos()).unwrap_or(0),
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{Fixture, edge_input, fixture, node_input};
    use super::*;
    use serde_json::json;

    #[cfg_attr(not(feature = "remote"), allow(dead_code))]
    const TOKEN: &str = "a-long-enough-shared-secret";

    #[cfg_attr(not(feature = "remote"), allow(dead_code))]
    /// source (test-source, no files) → local sink and remote sink.
    async fn sample(f: &Fixture) -> (Scene, Vec<Node>) {
        let scene = f
            .service
            .create_scene(&SceneInput {
                name: "demo scene".into(),
                max_hops: Some(50),
                defaults: crate::modules::node_config::SceneDefaults {
                    durable: Some(true),
                    ..Default::default()
                },
            })
            .await
            .unwrap();
        let source = f.node(&scene.id, "test-source", Value::Null).await;
        let local = f
            .node(
                &scene.id,
                "test-sink",
                json!({"launch_args": {"timeout": 5}}),
            )
            .await;
        let mut remote_input = node_input("test-sink", json!({"launch": "remote", "token": TOKEN}));
        remote_input.name = Some("far away".into());
        let remote = f
            .service
            .create_node(&scene.id, remote_input)
            .await
            .unwrap();
        let mapping = json!({"rules": [{"target": "url", "value": "http://x"}]});
        for target in [&local.id, &remote.id] {
            f.service
                .create_edge(&scene.id, edge_input(&source.id, target, mapping.clone()))
                .await
                .unwrap();
        }
        (scene, vec![source, local, remote])
    }

    #[cfg_attr(not(feature = "remote"), allow(dead_code))]
    fn decode(export: &Export) -> pb::SceneFile {
        scene_file::decode(&export.bytes).unwrap()
    }

    #[cfg(feature = "remote")] // creates remote nodes
    #[tokio::test]
    async fn export_then_import_recreates_the_scene_with_new_ids() {
        let f = fixture().await;
        let (scene, nodes) = sample(&f).await;
        let export = f.service.export_scene(&scene.id, false).await.unwrap();
        assert_eq!(export.file_name, "demo_scene.flwr");

        let file = decode(&export);
        assert!(!file.includes_secrets);
        let remote = file.nodes.iter().find(|n| n.key == nodes[2].id).unwrap();
        let remote: NodeConfig = serde_json::from_str(&remote.config_json).unwrap();
        assert_eq!(remote.token, None, "tokens are left out by default");
        assert_eq!(
            remote.remote_id.as_deref(),
            Some(nodes[2].id.as_str()),
            "remote_id is filled in from the node id"
        );

        let imported = f.service.import_scene(&export.bytes).await.unwrap();
        let new = imported.scene;
        assert_ne!(new.id, scene.id);
        assert_eq!((new.name.as_str(), new.max_hops), ("demo scene", Some(50)));
        assert_eq!(new.defaults.durable, Some(true), "defaults travel too");
        assert!(!new.enabled, "imports arrive disabled");

        // The file has the nodes in listing order; the import keeps it.
        let nodes = f.service.list_nodes(&scene.id).await.unwrap();
        let new_nodes = f.service.list_nodes(&new.id).await.unwrap();
        assert_eq!(new_nodes.len(), 3);
        for (old, new) in nodes.iter().zip(&new_nodes) {
            assert_ne!(old.id, new.id);
            assert_eq!(
                (
                    &old.name,
                    &old.node_type,
                    &old.plugin_id,
                    &old.plugin_version
                ),
                (
                    &new.name,
                    &new.node_type,
                    &new.plugin_id,
                    &new.plugin_version
                )
            );
        }
        let by_name = |name: Option<&str>, launch_args: bool| {
            new_nodes
                .iter()
                .find(|n| {
                    n.name.as_deref() == name && n.config.launch_args.is_null() != launch_args
                })
                .unwrap()
        };
        assert_eq!(
            by_name(None, true).config.launch_args,
            json!({"timeout": 5}),
            "integers survive the round trip"
        );
        let new_remote = by_name(Some("far away"), false);
        let old_remote = nodes
            .iter()
            .find(|n| n.name.as_deref() == Some("far away"))
            .unwrap();
        assert_eq!(
            new_remote.config.instance_id(&new_remote.id),
            old_remote.id,
            "the remote plugin's id is unchanged"
        );
        let edges = f.service.list_edges(&new.id).await.unwrap();
        assert_eq!(edges.len(), 2);
        let new_source = new_nodes.iter().find(|n| n.node_type == "SOURCE").unwrap();
        assert!(edges.iter().all(|e| e.source_node_id == new_source.id));
        assert!(edges.iter().all(|e| e.mapping.is_some()));

        // Not runnable yet: test-source has no files, the remote node no token.
        let problems = imported.check.problems;
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(
            problems.iter().any(|p| p.contains("files are missing")),
            "{problems:?}"
        );
        assert!(
            problems
                .iter()
                .any(|p| p.contains("far away") && p.contains("config.token is required")),
            "{problems:?}"
        );
        assert!(matches!(
            f.service.set_enabled(&new.id, true).await,
            Err(ServiceError::Conflict(_, p)) if p == problems
        ));
    }

    #[cfg(feature = "remote")] // creates remote nodes
    #[tokio::test]
    async fn secrets_are_exported_only_when_asked() {
        let f = fixture().await;
        let (scene, _) = sample(&f).await;
        let export = f.service.export_scene(&scene.id, true).await.unwrap();
        assert!(decode(&export).includes_secrets);
        let imported = f.service.import_scene(&export.bytes).await.unwrap();
        let nodes = f.service.list_nodes(&imported.scene.id).await.unwrap();
        let remote = nodes.iter().find(|n| n.config.token.is_some()).unwrap();
        assert_eq!(remote.config.token.as_deref(), Some(TOKEN));
        assert_eq!(
            imported.check.problems.len(),
            1,
            "only test-source's missing files"
        );
    }

    fn file(nodes: Vec<pb::Node>, edges: Vec<pb::Edge>) -> Vec<u8> {
        scene_file::encode(&pb::SceneFile {
            name: "from elsewhere".into(),
            nodes,
            edges,
            ..Default::default()
        })
    }

    fn pb_node(key: &str, t: pb::plugin_type::Enum, plugin: &str, config: &str) -> pb::Node {
        pb::Node {
            key: key.into(),
            r#type: t as i32,
            plugin_id: plugin.into(),
            plugin_version: "2.0.0".into(),
            config_json: config.into(),
            ..Default::default()
        }
    }

    fn pb_edge(from: &str, to: &str) -> pb::Edge {
        pb::Edge {
            source_key: from.into(),
            target_key: to.into(),
            ..Default::default()
        }
    }

    #[cfg(feature = "remote")] // creates remote nodes
    #[tokio::test]
    async fn plugins_missing_here_dont_block_the_import() {
        let f = fixture().await;
        let bytes = file(
            vec![
                pb_node("a", pb::plugin_type::Enum::Source, "not-here", "{}"),
                pb_node("b", pb::plugin_type::Enum::Source, "test-sink", "{}"),
                pb_node("c", pb::plugin_type::Enum::Sink, "test-sink", "{}"),
                pb_node(
                    "d",
                    pb::plugin_type::Enum::Sink,
                    "elsewhere",
                    r#"{"launch": "remote"}"#,
                ),
            ],
            vec![pb_edge("a", "c")],
        );
        let imported = f.service.import_scene(&bytes).await.unwrap();
        let check = imported.check;
        assert_eq!(check.problems.len(), 4, "{check:?}");
        assert!(
            check.problems[2].contains("plugin elsewhere isn't installed"),
            "{check:?}"
        );
        assert!(
            check.problems[3].contains("config.token is required"),
            "listed even though the plugin is missing: {check:?}"
        );
        assert!(check.problems[0].contains("plugin not-here isn't installed on this server"));
        assert!(
            check.problems[1]
                .contains("it's a SOURCE, but plugin test-sink on this server is a SINK")
        );
        assert_eq!(check.warnings.len(), 1, "{check:?}");
        assert!(
            check.warnings[0]
                .contains("configured with plugin test-sink 2.0.0; this server has 1.0.0")
        );
        assert_eq!(f.service.check(&imported.scene.id).await.unwrap(), check);
    }

    #[tokio::test]
    async fn an_invalid_file_is_rejected_with_every_problem_and_stores_nothing() {
        let f = fixture().await;
        let bytes = file(
            vec![
                pb_node("a", pb::plugin_type::Enum::Source, "test-source", "{}"),
                pb_node("a", pb::plugin_type::Enum::Undefined, "Bad Id", "not json"),
                pb_node(
                    "r",
                    pb::plugin_type::Enum::Sink,
                    "test-sink",
                    r#"{"launch": "remote", "remote_id": "has space", "bogus": 1}"#,
                ),
            ],
            vec![pb_edge("a", "missing")],
        );
        let problems = match f.service.import_scene(&bytes).await {
            Err(ServiceError::Invalid(problems)) => problems,
            other => panic!("expected Invalid, got {:?}", other.map(|i| i.scene)),
        };
        for expected in [
            "nodes[1] (\"a\"): key is used by another node",
            "nodes[1] (\"a\"): type must be SOURCE, SINK or TRANSFORM",
            "nodes[1] (\"a\"): plugin_id \"Bad Id\" isn't a valid plugin id",
            "nodes[1] (\"a\"): config isn't JSON",
            "nodes[2] (\"r\"): config: unknown field `bogus`",
            "edges[0] (a → missing): target_key \"missing\" isn't a node in the file",
        ] {
            assert!(
                problems.iter().any(|p| p.starts_with(expected)),
                "{expected:?} not in {problems:?}"
            );
        }
        let (scenes, _) = f
            .service
            .list_scenes(ListFilters::new(0, 10))
            .await
            .unwrap();
        assert!(scenes.is_empty(), "nothing was stored");

        // Graph rules run on the stored rows, inside the import transaction.
        let backwards = file(
            vec![
                pb_node("s", pb::plugin_type::Enum::Source, "test-source", "{}"),
                pb_node("k", pb::plugin_type::Enum::Sink, "test-sink", "{}"),
            ],
            vec![pb_edge("k", "s")],
        );
        assert!(matches!(
            f.service.import_scene(&backwards).await,
            Err(ServiceError::Invalid(p)) if p.len() == 2
        ));
        let (scenes, _) = f
            .service
            .list_scenes(ListFilters::new(0, 10))
            .await
            .unwrap();
        assert!(scenes.is_empty(), "rolled back");

        let not_a_file = f.service.import_scene(b"hello").await;
        assert!(
            matches!(not_a_file, Err(ServiceError::Invalid(p)) if p[0].contains("not a flwrs scene file"))
        );
    }

    #[tokio::test]
    async fn imported_mappings_are_checked_against_this_servers_plugins() {
        let f = fixture().await;
        let mapped = |key: &str, json: &str| pb::Edge {
            mapping_json: Some(json.into()),
            ..pb_edge("s", key)
        };
        let bytes = file(
            vec![
                pb_node("s", pb::plugin_type::Enum::Source, "test-source", "{}"),
                pb_node("ok", pb::plugin_type::Enum::Sink, "test-sink", "{}"),
                pb_node("old", pb::plugin_type::Enum::Sink, "test-sink", "{}"),
                pb_node("lua", pb::plugin_type::Enum::Sink, "test-sink", "{}"),
                pb_node("bad-lua", pb::plugin_type::Enum::Sink, "test-sink", "{}"),
            ],
            vec![
                mapped(
                    "ok",
                    r#"{"rules": [{"target": "url", "value": "http://x"}]}"#,
                ),
                // A field this server's test-sink doesn't have (e.g. another version).
                mapped("old", r#"{"rules": [{"target": "headers", "value": "x"}]}"#),
                mapped(
                    "lua",
                    r#"{"rules": [{"target": "url", "lua": "return 'x'"}]}"#,
                ),
                mapped(
                    "bad-lua",
                    r#"{"rules": [{"target": "url", "lua": "return ("}]}"#,
                ),
            ],
        );
        let imported = f.service.import_scene(&bytes).await.unwrap();
        let mapping_problems: Vec<&String> = imported
            .check
            .problems
            .iter()
            .filter(|p| p.starts_with("edge "))
            .collect();
        // Lua depends on this server: with Lua, valid code is fine and a
        // syntax error is a problem; without Lua (built without the
        // mapping-lua feature), any Lua blocks launching. Never the import.
        #[cfg(feature = "mapping-lua")]
        let expected: &[&str] = &["no field \"headers\"", "mapping.rules[0].lua:"];
        #[cfg(not(feature = "mapping-lua"))]
        let expected: &[&str] = &[
            "no field \"headers\"",
            "built without Lua support",
            "built without Lua support",
        ];
        assert_eq!(
            mapping_problems.len(),
            expected.len(),
            "{:?}",
            imported.check
        );
        for (problem, fragment) in mapping_problems.iter().zip(expected) {
            assert!(
                problem.contains(fragment),
                "{fragment:?} not in {problem:?}"
            );
        }
        assert!(matches!(
            f.service.set_enabled(&imported.scene.id, true).await,
            Err(ServiceError::Conflict(..))
        ));

        // A path that isn't even valid syntax is part of the file's structure.
        let broken = file(
            vec![
                pb_node("s", pb::plugin_type::Enum::Source, "test-source", "{}"),
                pb_node("k", pb::plugin_type::Enum::Sink, "test-sink", "{}"),
            ],
            vec![mapped("k", r#"{"rules": [{"target": "a[", "value": 1}]}"#)],
        );
        assert!(matches!(
            f.service.import_scene(&broken).await,
            Err(ServiceError::Invalid(p)) if p[0].contains("path \"a[\"")
        ));
    }
}
