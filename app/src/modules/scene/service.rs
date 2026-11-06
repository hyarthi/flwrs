mod query_sqlite;
mod transfer;

use crate::modules::director::DirectorHandle;
use crate::modules::director::handle::{SceneState, SceneStatus};
use crate::modules::mapping::{self, MappingSpec};
use crate::modules::node_config::{NodeConfig, PluginRules, WhenFull};
use crate::modules::plugin::service::{
    Plugin, Service as PluginService, ServiceError as PluginError,
};
use crate::modules::scene::graph;
use chrono::Local;
use flwrs_core::db::{Database, DbError};
use flwrs_plugin::schema::rules;
use flwrs_plugin::schema::schema::SchemaDefinition;
use query_sqlite::{EdgeValues, NodeValues};
use serde_json::Value;
use sqlx::Connection;
use sqlx::error::ErrorKind;
use std::collections::HashMap;
use thiserror::Error;
use tokio::sync::Mutex;
use ulid::Ulid;

const MAX_NAME_CHARS: usize = 200;

#[derive(sqlx::FromRow, Debug, Clone)]
pub(crate) struct Scene {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    /// Hop limit for this scene; `None` = the server default.
    pub max_hops: Option<u32>,
    pub create_time: chrono::DateTime<Local>,
    pub update_time: chrono::DateTime<Local>,
}

#[derive(sqlx::FromRow, Debug)]
pub(crate) struct NodeRow {
    pub id: String,
    pub name: Option<String>,
    pub scene_id: String,
    #[sqlx(rename = "type")]
    pub node_type: String,
    pub plugin_id: String,
    pub plugin_version: Option<String>,
    pub config: String,
    pub create_time: chrono::DateTime<Local>,
    pub update_time: chrono::DateTime<Local>,
}

/// A scene node: one plugin instance.
#[derive(Debug, Clone)]
pub(crate) struct Node {
    pub id: String,
    pub name: Option<String>,
    pub scene_id: String,
    /// SOURCE, SINK or TRANSFORM. Created through the API it's the plugin's
    /// type; an imported node keeps its type even if this server's plugin
    /// differs (then the scene has a problem).
    pub node_type: String,
    /// Registry id of the plugin. Not a foreign key: an imported scene may
    /// name a plugin this server doesn't have.
    pub plugin_id: String,
    /// The plugin version the node was configured with, if known.
    pub plugin_version: Option<String>,
    pub config: NodeConfig,
    pub create_time: chrono::DateTime<Local>,
    pub update_time: chrono::DateTime<Local>,
}

impl TryFrom<NodeRow> for Node {
    type Error = ServiceError;

    fn try_from(row: NodeRow) -> Result<Self, ServiceError> {
        let value: Value = serde_json::from_str(&row.config).map_err(|e| {
            ServiceError::Unknown(format!("node {} has unreadable config: {e}", row.id))
        })?;
        let config = NodeConfig::parse(&value).map_err(|e| {
            ServiceError::Unknown(format!("node {} has an outdated config: {e}", row.id))
        })?;
        Ok(Self {
            id: row.id,
            name: row.name,
            scene_id: row.scene_id,
            node_type: row.node_type,
            plugin_id: row.plugin_id,
            plugin_version: row.plugin_version,
            config,
            create_time: row.create_time,
            update_time: row.update_time,
        })
    }
}

#[derive(sqlx::FromRow, Debug)]
pub(crate) struct EdgeRow {
    pub id: String,
    pub name: Option<String>,
    pub scene_id: String,
    pub source_node_id: String,
    pub target_node_id: String,
    pub mapping: Option<String>,
    pub create_time: chrono::DateTime<Local>,
    pub update_time: chrono::DateTime<Local>,
}

/// A directed data handover between two nodes of a scene.
#[derive(Debug, Clone)]
pub(crate) struct Edge {
    pub id: String,
    pub name: Option<String>,
    pub scene_id: String,
    pub source_node_id: String,
    pub target_node_id: String,
    /// `None` = pass events through as-is.
    pub mapping: Option<Value>,
    pub create_time: chrono::DateTime<Local>,
    pub update_time: chrono::DateTime<Local>,
}

/// What the director runs: a scene, its graph, and the registry entries of
/// the plugins its nodes use (a missing entry means the plugin isn't registered).
#[derive(Debug, Clone)]
pub(crate) struct SceneSnapshot {
    pub scene: Scene,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub plugins: HashMap<String, crate::modules::plugin::service::Plugin>,
}

impl From<EdgeRow> for Edge {
    fn from(row: EdgeRow) -> Self {
        Self {
            mapping: row
                .mapping
                .as_deref()
                .and_then(|m| serde_json::from_str(m).ok()),
            id: row.id,
            name: row.name,
            scene_id: row.scene_id,
            source_node_id: row.source_node_id,
            target_node_id: row.target_node_id,
            create_time: row.create_time,
            update_time: row.update_time,
        }
    }
}

pub(crate) struct NodeInput {
    pub name: Option<String>,
    pub plugin_id: String,
    /// Optional; if given it must match the plugin's type.
    pub node_type: Option<String>,
    pub config: Value,
}

pub(crate) struct EdgeInput {
    pub name: Option<String>,
    pub source_node_id: String,
    pub target_node_id: String,
    pub mapping: Value,
}

#[derive(Error, Debug)]
pub(crate) enum ServiceError {
    #[error("{0} not found")]
    NotFound(&'static str),
    #[error("invalid request")]
    Invalid(Vec<String>),
    #[error("{0}")]
    Conflict(String, Vec<String>),
    #[error("failed to execute query: {0}")]
    Query(sqlx::Error),
    #[error("failed to get connection: {0}")]
    Connection(#[from] DbError),
    #[error("unknown error occurred: {0}")]
    Unknown(String),
    /// A service this one needs (e.g. the director) isn't answering.
    #[error("{0}")]
    Unavailable(String),
}

impl From<sqlx::Error> for ServiceError {
    fn from(error: sqlx::Error) -> Self {
        match &error {
            sqlx::Error::RowNotFound => ServiceError::NotFound("record"),
            _ => match error.as_database_error().map(|e| e.kind()) {
                Some(ErrorKind::UniqueViolation) => {
                    ServiceError::Conflict("already exists".into(), vec![])
                }
                Some(ErrorKind::ForeignKeyViolation) => {
                    ServiceError::Invalid(vec!["refers to something that doesn't exist".into()])
                }
                _ => ServiceError::Query(error),
            },
        }
    }
}

/// Maps "row not found" to a not-found error for `what`.
fn not_found(what: &'static str) -> impl Fn(sqlx::Error) -> ServiceError {
    move |e| match e {
        sqlx::Error::RowNotFound => ServiceError::NotFound(what),
        other => other.into(),
    }
}

pub(crate) struct ListFilters {
    offset: i64,
    limit: i64,
}

impl ListFilters {
    pub(crate) fn new(offset: i64, limit: i64) -> Self {
        Self { offset, limit }
    }
}

/// Config-time management of scenes and their graphs (nodes and edges).
/// Committed changes to enabled scenes are passed to the director.
pub(crate) struct Service {
    db: &'static Database,
    plugins: &'static PluginService,
    director: DirectorHandle,
    /// Serializes graph changes, so whole-scene checks see a stable graph.
    lock: Mutex<()>,
}

type Conn = sqlx::pool::PoolConnection<sqlx::Sqlite>;

/// Write transactions take SQLite's write lock up front. A deferred one
/// would read first and then fail with "database is locked" (instead of
/// waiting) if it can't upgrade to a write.
const WRITE_TX: &str = "BEGIN IMMEDIATE";

impl Service {
    pub(crate) fn new(
        db: &'static Database,
        plugins: &'static PluginService,
        director: DirectorHandle,
    ) -> Self {
        Self {
            db,
            plugins,
            director,
            lock: Mutex::new(()),
        }
    }

    async fn conn(&self) -> Result<Conn, ServiceError> {
        match self.db {
            Database::SQLite(db) => Ok(db.get_connection().await?),
        }
    }

    // ----- scenes -----

    pub(crate) async fn get_scene(&self, id: &str) -> Result<Scene, ServiceError> {
        let mut conn = self.conn().await?;
        query_sqlite::get_scene(&mut conn, id)
            .await
            .map_err(not_found("scene"))
    }

    pub(crate) async fn create_scene(&self, input: &SceneInput) -> Result<Scene, ServiceError> {
        let (name, max_hops) = check_scene(input)?;
        let mut conn = self.conn().await?;
        let id = Ulid::generate().to_string();
        let scene = query_sqlite::create_scene(&mut conn, &id, &name, max_hops).await?;
        log::debug!("Scene service: created scene [{}]", scene.id);
        Ok(scene)
    }

    /// Replaces the scene's settings (name, hop limit).
    pub(crate) async fn update_scene(
        &self,
        id: &str,
        input: &SceneInput,
    ) -> Result<Scene, ServiceError> {
        let (name, max_hops) = check_scene(input)?;
        let _guard = self.lock.lock().await;
        let mut conn = self.conn().await?;
        let before = query_sqlite::get_scene(&mut conn, id)
            .await
            .map_err(not_found("scene"))?;
        let scene = query_sqlite::update_scene(&mut conn, id, &name, max_hops)
            .await
            .map_err(not_found("scene"))?;
        // A rename doesn't change the workflow; a new hop limit does.
        if scene.enabled && scene.max_hops != before.max_hops {
            self.director.request_reload(id);
        }
        Ok(scene)
    }

    pub(crate) async fn delete_scene(&self, id: &str) -> Result<(), ServiceError> {
        let _guard = self.lock.lock().await;
        let mut conn = self.conn().await?;
        let scene = query_sqlite::get_scene(&mut conn, id)
            .await
            .map_err(not_found("scene"))?;
        query_sqlite::delete_scene(&mut conn, id).await?;
        if scene.enabled {
            self.director.request_stop(id);
        }
        Ok(())
    }

    /// Returns a page of scenes plus whether there are more.
    pub(crate) async fn list_scenes(
        &self,
        filters: ListFilters,
    ) -> Result<(Vec<Scene>, bool), ServiceError> {
        let mut conn = self.conn().await?;
        let probe = ListFilters {
            offset: filters.offset,
            limit: filters.limit + 1,
        };
        let mut scenes = query_sqlite::list_scenes(&mut conn, &probe).await?;
        let has_more = scenes.len() > filters.limit as usize;
        scenes.truncate(filters.limit as usize);
        Ok((scenes, has_more))
    }

    /// Enables (runs) or disables (stops) a scene's workflow. Enabling checks
    /// that every node's plugin is installed.
    pub(crate) async fn set_enabled(&self, id: &str, enabled: bool) -> Result<Scene, ServiceError> {
        let _guard = self.lock.lock().await;
        let mut conn = self.conn().await?;
        let scene = query_sqlite::get_scene(&mut conn, id)
            .await
            .map_err(not_found("scene"))?;
        if scene.enabled == enabled {
            return Ok(scene);
        }
        if enabled {
            let check = self.check_nodes(&mut conn, id).await?;
            if !check.problems.is_empty() {
                return Err(ServiceError::Conflict(
                    "scene can't be enabled".into(),
                    check.problems,
                ));
            }
        }
        let scene = query_sqlite::set_scene_enabled(&mut conn, id, enabled).await?;
        if enabled {
            self.director.request_reload(id);
        } else {
            self.director.request_stop(id);
        }
        log::info!(
            "Scene service: scene [{id}] {}",
            if enabled { "enabled" } else { "disabled" }
        );
        Ok(scene)
    }

    /// Whether the scene can run on this server, and why not. Enabling is
    /// refused while there are problems. A scene created here normally has
    /// none; an imported one (or one whose plugin was removed or replaced)
    /// may.
    pub(crate) async fn check(&self, id: &str) -> Result<SceneCheck, ServiceError> {
        let mut conn = self.conn().await?;
        query_sqlite::get_scene(&mut conn, id)
            .await
            .map_err(not_found("scene"))?;
        self.check_nodes(&mut conn, id).await
    }

    async fn check_nodes(
        &self,
        conn: &mut sqlx::SqliteConnection,
        scene_id: &str,
    ) -> Result<SceneCheck, ServiceError> {
        let mut check = SceneCheck::default();
        for row in query_sqlite::list_nodes(conn, scene_id).await? {
            let node = Node::try_from(row)?;
            let label = match &node.name {
                Some(name) => format!("node {} ({name})", node.id),
                None => format!("node {}", node.id),
            };
            let plugin = match self.plugins.get(&node.plugin_id).await {
                Ok(plugin) => plugin,
                Err(PluginError::NotFound) => {
                    check.problems.push(format!(
                        "{label}: plugin {} isn't installed on this server",
                        node.plugin_id
                    ));
                    // What doesn't depend on the plugin can still be listed.
                    check
                        .problems
                        .extend(node.config.missing_token().map(|p| format!("{label}: {p}")));
                    continue;
                }
                Err(e) => return Err(ServiceError::Unknown(e.to_string())),
            };
            if !self.plugins.is_installed(&plugin) {
                check.problems.push(format!(
                    "{label}: plugin {} is registered but its files are missing",
                    plugin.id
                ));
            }
            if plugin.plugin_type != node.node_type {
                check.problems.push(format!(
                    "{label}: it's a {}, but plugin {} on this server is a {}",
                    node.node_type, plugin.id, plugin.plugin_type
                ));
                continue;
            }
            if let Some(version) = &node.plugin_version
                && *version != plugin.version
            {
                check.warnings.push(format!(
                    "{label}: configured with plugin {} {version}; this server has {}",
                    plugin.id, plugin.version
                ));
            }
            check.problems.extend(
                validate_against_plugin(&node.config, &plugin)?
                    .into_iter()
                    .map(|p| format!("{label}: {p}")),
            );
        }
        // Mappings, against this server's plugin schemas.
        for edge in query_sqlite::list_edges(conn, scene_id).await? {
            let Some(mapping) = edge.mapping.as_deref() else {
                continue;
            };
            let spec = serde_json::from_str::<Value>(mapping)
                .ok()
                .and_then(|v| MappingSpec::parse(&v).ok().flatten());
            let Some(spec) = spec else {
                check.problems.push(format!(
                    "edge {} → {}: the stored mapping is unreadable",
                    edge.source_node_id, edge.target_node_id
                ));
                continue;
            };
            let (source, target) = self
                .edge_schemas(conn, scene_id, &edge.source_node_id, &edge.target_node_id)
                .await?;
            if let Err(errors) = mapping::compile(
                &spec,
                mapping::Schemas {
                    source: source.as_ref(),
                    target: target.as_ref(),
                },
            ) {
                check.problems.extend(errors.into_iter().map(|e| {
                    format!(
                        "edge {} → {}: {e}",
                        edge.source_node_id, edge.target_node_id
                    )
                }));
            }
        }
        Ok(check)
    }

    /// The source node's `out` and the target node's `in` schema, from their
    /// plugins, where those are installed with the node's type.
    async fn edge_schemas(
        &self,
        conn: &mut sqlx::SqliteConnection,
        scene_id: &str,
        source_node_id: &str,
        target_node_id: &str,
    ) -> Result<(Option<SchemaDefinition>, Option<SchemaDefinition>), ServiceError> {
        let mut schemas = [None, None];
        for (slot, (node_id, side)) in schemas
            .iter_mut()
            .zip([(source_node_id, "out"), (target_node_id, "in")])
        {
            let Ok(node) = query_sqlite::get_node(conn, scene_id, node_id).await else {
                continue;
            };
            let plugin = match self.plugins.get(&node.plugin_id).await {
                Ok(plugin) if plugin.plugin_type == node.node_type => plugin,
                Ok(_) | Err(PluginError::NotFound) => continue,
                Err(e) => return Err(ServiceError::Unknown(e.to_string())),
            };
            let json = if side == "out" {
                plugin.out_schema
            } else {
                plugin.in_schema
            };
            *slot = json.as_deref().and_then(|j| rules::parse_json(j).ok());
        }
        let [source, target] = schemas;
        Ok((source, target))
    }

    /// The scene's run-time status from the director. A scene it hasn't run
    /// (e.g. disabled since the server started) reports `STOPPED`.
    pub(crate) async fn runtime_status(
        &self,
        id: &str,
    ) -> Result<(Scene, SceneStatus), ServiceError> {
        let scene = self.get_scene(id).await?;
        let status = self
            .director
            .status(id)
            .await
            .map_err(|e| ServiceError::Unavailable(format!("director: {e}")))?
            .unwrap_or(SceneStatus {
                state: SceneState::Stopped,
                errors: vec![],
                max_hops: None,
                nodes: vec![],
            });
        Ok((scene, status))
    }

    // ----- for the director -----

    /// Ids of the scenes that should run.
    pub(crate) async fn enabled_scene_ids(&self) -> Result<Vec<String>, ServiceError> {
        let mut conn = self.conn().await?;
        Ok(query_sqlite::enabled_scene_ids(&mut conn).await?)
    }

    /// A scene with its whole graph and the registry entries of its plugins,
    /// read under the service lock, so it never reflects a half-applied change.
    pub(crate) async fn snapshot(&self, id: &str) -> Result<SceneSnapshot, ServiceError> {
        let _guard = self.lock.lock().await;
        let mut conn = self.conn().await?;
        let scene = query_sqlite::get_scene(&mut conn, id)
            .await
            .map_err(not_found("scene"))?;
        let nodes = query_sqlite::list_nodes(&mut conn, id)
            .await?
            .into_iter()
            .map(Node::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        let edges: Vec<Edge> = query_sqlite::list_edges(&mut conn, id)
            .await?
            .into_iter()
            .map(Edge::from)
            .collect();
        let mut plugins = HashMap::new();
        for node in &nodes {
            if plugins.contains_key(&node.plugin_id) {
                continue;
            }
            match self.plugins.get(&node.plugin_id).await {
                Ok(plugin) => {
                    plugins.insert(node.plugin_id.clone(), plugin);
                }
                Err(PluginError::NotFound) => {}
                Err(e) => return Err(ServiceError::Unknown(e.to_string())),
            }
        }
        Ok(SceneSnapshot {
            scene,
            nodes,
            edges,
            plugins,
        })
    }

    // ----- nodes -----

    pub(crate) async fn list_nodes(&self, scene_id: &str) -> Result<Vec<Node>, ServiceError> {
        let mut conn = self.conn().await?;
        query_sqlite::get_scene(&mut conn, scene_id)
            .await
            .map_err(not_found("scene"))?;
        query_sqlite::list_nodes(&mut conn, scene_id)
            .await?
            .into_iter()
            .map(Node::try_from)
            .collect()
    }

    pub(crate) async fn get_node(&self, scene_id: &str, id: &str) -> Result<Node, ServiceError> {
        let mut conn = self.conn().await?;
        query_sqlite::get_node(&mut conn, scene_id, id)
            .await
            .map_err(not_found("node"))?
            .try_into()
    }

    pub(crate) async fn create_node(
        &self,
        scene_id: &str,
        input: NodeInput,
    ) -> Result<Node, ServiceError> {
        let _guard = self.lock.lock().await;
        let prepared = self.prepare_node(input, None).await?;
        let mut conn = self.conn().await?;
        let mut tx = conn.begin_with(WRITE_TX).await?;
        let scene = query_sqlite::get_scene(&mut tx, scene_id)
            .await
            .map_err(not_found("scene"))?;
        let row = query_sqlite::insert_node(
            &mut tx,
            scene_id,
            &Ulid::generate().to_string(),
            &prepared.values(),
        )
        .await?;
        tx.commit().await?;
        self.changed(&scene);
        row.try_into()
    }

    pub(crate) async fn update_node(
        &self,
        scene_id: &str,
        id: &str,
        input: NodeInput,
    ) -> Result<Node, ServiceError> {
        let _guard = self.lock.lock().await;
        let mut conn = self.conn().await?;
        let scene = query_sqlite::get_scene(&mut conn, scene_id)
            .await
            .map_err(not_found("scene"))?;
        let existing: Node = query_sqlite::get_node(&mut conn, scene_id, id)
            .await
            .map_err(not_found("node"))?
            .try_into()?;
        let prepared = self.prepare_node(input, Some(&existing)).await?;
        let mut tx = conn.begin_with(WRITE_TX).await?;
        let row = query_sqlite::update_node(&mut tx, scene_id, id, &prepared.values()).await?;
        // A new plugin type or queue policy can break edges that were valid before.
        check_graph(&mut tx, scene_id).await?;
        tx.commit().await?;
        self.changed(&scene);
        row.try_into()
    }

    /// Deletes a node and (by cascade) its edges.
    pub(crate) async fn delete_node(&self, scene_id: &str, id: &str) -> Result<(), ServiceError> {
        let _guard = self.lock.lock().await;
        let mut conn = self.conn().await?;
        let scene = query_sqlite::get_scene(&mut conn, scene_id)
            .await
            .map_err(not_found("scene"))?;
        if query_sqlite::delete_node(&mut conn, scene_id, id).await? == 0 {
            return Err(ServiceError::NotFound("node"));
        }
        self.changed(&scene);
        Ok(())
    }

    /// Validates a node request against its plugin. `existing` is the node
    /// being updated: a remote node's token is kept if the request omits it.
    async fn prepare_node(
        &self,
        input: NodeInput,
        existing: Option<&Node>,
    ) -> Result<PreparedNode, ServiceError> {
        let mut errors = vec![];
        let name = optional_name(input.name, &mut errors);
        let plugin = match self.plugins.get(&input.plugin_id).await {
            Ok(p) => Some(p),
            Err(PluginError::NotFound) => {
                errors.push(format!(
                    "plugin_id {:?} isn't a registered plugin",
                    input.plugin_id
                ));
                None
            }
            Err(e) => return Err(ServiceError::Unknown(e.to_string())),
        };
        let mut config = match NodeConfig::parse(&input.config) {
            Ok(c) => Some(c),
            Err(e) => {
                errors.push(e);
                None
            }
        };
        if let (Some(config), Some(existing)) = (config.as_mut(), existing)
            && config.token.is_none()
            && config.launch == existing.config.launch
        {
            config.token = existing.config.token.clone();
        }
        if let (Some(plugin), Some(t)) = (&plugin, &input.node_type)
            && *t != plugin.plugin_type
        {
            errors.push(format!(
                "type {t:?} doesn't match plugin {}, which is a {}",
                plugin.id, plugin.plugin_type
            ));
        }
        if let (Some(plugin), Some(config)) = (&plugin, &config) {
            errors.extend(validate_against_plugin(config, plugin)?);
        }
        if !errors.is_empty() {
            return Err(ServiceError::Invalid(errors));
        }
        let plugin = plugin.expect("no errors");
        let config = config.expect("no errors");
        Ok(PreparedNode {
            name,
            node_type: plugin.plugin_type,
            plugin_id: plugin.id,
            plugin_version: Some(plugin.version),
            config_json: serde_json::to_string(&config).expect("config serializes"),
        })
    }

    // ----- edges -----

    pub(crate) async fn list_edges(&self, scene_id: &str) -> Result<Vec<Edge>, ServiceError> {
        let mut conn = self.conn().await?;
        query_sqlite::get_scene(&mut conn, scene_id)
            .await
            .map_err(not_found("scene"))?;
        Ok(query_sqlite::list_edges(&mut conn, scene_id)
            .await?
            .into_iter()
            .map(Edge::from)
            .collect())
    }

    pub(crate) async fn get_edge(&self, scene_id: &str, id: &str) -> Result<Edge, ServiceError> {
        let mut conn = self.conn().await?;
        Ok(query_sqlite::get_edge(&mut conn, scene_id, id)
            .await
            .map_err(not_found("edge"))?
            .into())
    }

    pub(crate) async fn create_edge(
        &self,
        scene_id: &str,
        input: EdgeInput,
    ) -> Result<Edge, ServiceError> {
        self.write_edge(scene_id, None, input).await
    }

    pub(crate) async fn update_edge(
        &self,
        scene_id: &str,
        id: &str,
        input: EdgeInput,
    ) -> Result<Edge, ServiceError> {
        self.write_edge(scene_id, Some(id), input).await
    }

    pub(crate) async fn delete_edge(&self, scene_id: &str, id: &str) -> Result<(), ServiceError> {
        let _guard = self.lock.lock().await;
        let mut conn = self.conn().await?;
        let scene = query_sqlite::get_scene(&mut conn, scene_id)
            .await
            .map_err(not_found("scene"))?;
        if query_sqlite::delete_edge(&mut conn, scene_id, id).await? == 0 {
            return Err(ServiceError::NotFound("edge"));
        }
        self.changed(&scene);
        Ok(())
    }

    async fn write_edge(
        &self,
        scene_id: &str,
        id: Option<&str>,
        input: EdgeInput,
    ) -> Result<Edge, ServiceError> {
        let _guard = self.lock.lock().await;
        let mut errors = vec![];
        let name = optional_name(input.name, &mut errors);
        let mapping = match MappingSpec::parse(&input.mapping) {
            Ok(spec) => spec.map(|s| serde_json::to_string(&s).expect("mapping serializes")),
            Err(e) => {
                errors.extend(e);
                None
            }
        };
        let mut conn = self.conn().await?;
        let scene = query_sqlite::get_scene(&mut conn, scene_id)
            .await
            .map_err(not_found("scene"))?;
        for (field, node_id) in [
            ("source_node_id", &input.source_node_id),
            ("target_node_id", &input.target_node_id),
        ] {
            if let Err(sqlx::Error::RowNotFound) =
                query_sqlite::get_node(&mut conn, scene_id, node_id).await
            {
                errors.push(format!("{field} {node_id:?} isn't a node of this scene"));
            }
        }
        if errors.is_empty()
            && let Ok(Some(spec)) = MappingSpec::parse(&input.mapping)
        {
            let (source, target) = self
                .edge_schemas(
                    &mut conn,
                    scene_id,
                    &input.source_node_id,
                    &input.target_node_id,
                )
                .await?;
            if let Err(problems) = mapping::compile(
                &spec,
                mapping::Schemas {
                    source: source.as_ref(),
                    target: target.as_ref(),
                },
            ) {
                errors.extend(problems);
            }
        }
        if !errors.is_empty() {
            return Err(ServiceError::Invalid(errors));
        }
        let values = EdgeValues {
            name: name.as_deref(),
            source_node_id: &input.source_node_id,
            target_node_id: &input.target_node_id,
            mapping: mapping.as_deref(),
        };
        let mut tx = conn.begin_with(WRITE_TX).await?;
        let written = match id {
            None => {
                query_sqlite::insert_edge(&mut tx, scene_id, &Ulid::generate().to_string(), &values)
                    .await
            }
            Some(id) => query_sqlite::update_edge(&mut tx, scene_id, id, &values).await,
        };
        let row = written.map_err(|e| match e {
            sqlx::Error::RowNotFound => ServiceError::NotFound("edge"),
            e if e.as_database_error().map(|d| d.kind()) == Some(ErrorKind::UniqueViolation) => {
                ServiceError::Conflict(
                    "edge already exists".into(),
                    vec![format!(
                        "there is already an edge from {} to {}",
                        input.source_node_id, input.target_node_id
                    )],
                )
            }
            e => e.into(),
        })?;
        check_graph(&mut tx, scene_id).await?;
        tx.commit().await?;
        self.changed(&scene);
        Ok(row.into())
    }

    /// Tells the director about a committed change to an enabled scene.
    fn changed(&self, scene: &Scene) {
        if scene.enabled {
            self.director.request_reload(&scene.id);
        }
    }
}

struct PreparedNode {
    name: Option<String>,
    node_type: String,
    plugin_id: String,
    plugin_version: Option<String>,
    config_json: String,
}

impl PreparedNode {
    fn values(&self) -> NodeValues<'_> {
        NodeValues {
            name: self.name.as_deref(),
            node_type: &self.node_type,
            plugin_id: &self.plugin_id,
            plugin_version: self.plugin_version.as_deref(),
            config: &self.config_json,
        }
    }
}

/// Checks a node's config against its plugin's manifest and launch-args schema.
fn validate_against_plugin(
    config: &NodeConfig,
    plugin: &Plugin,
) -> Result<Vec<String>, ServiceError> {
    let manifest = plugin.parsed_manifest().ok_or_else(|| {
        ServiceError::Unknown(format!("plugin {} has an unreadable manifest", plugin.id))
    })?;
    let validator = match &plugin.launch_args_schema {
        None => None,
        Some(schema) => {
            let value: Value = serde_json::from_str(schema).map_err(|e| {
                ServiceError::Unknown(format!("plugin {} schema isn't JSON: {e}", plugin.id))
            })?;
            Some(
                jsonschema::validator_for(&value)
                    .map_err(|e| ServiceError::Unknown(e.to_string()))?,
            )
        }
    };
    Ok(config.validate(&PluginRules {
        launch_args_schema: validator.as_ref(),
        launch_args_style: manifest.launch_args_style,
    }))
}

/// Whole-scene rules the DB can't express, checked before commit:
/// - a SINK can't be an edge source, and a SOURCE can't be an edge target;
/// - a node on a cycle can't use the `block` queue policy (it can deadlock).
async fn check_graph(
    conn: &mut sqlx::SqliteConnection,
    scene_id: &str,
) -> Result<(), ServiceError> {
    let nodes: Vec<Node> = query_sqlite::list_nodes(conn, scene_id)
        .await?
        .into_iter()
        .map(Node::try_from)
        .collect::<Result<_, _>>()?;
    let edges = query_sqlite::list_edges(conn, scene_id).await?;
    let type_of = |id: &str| {
        nodes
            .iter()
            .find(|n| n.id == id)
            .map(|n| n.node_type.as_str())
    };
    let mut errors = vec![];
    for edge in &edges {
        if type_of(&edge.source_node_id) == Some("SINK") {
            errors.push(format!(
                "edge {} → {}: node {} is a SINK, which can't send events",
                edge.source_node_id, edge.target_node_id, edge.source_node_id
            ));
        }
        if type_of(&edge.target_node_id) == Some("SOURCE") {
            errors.push(format!(
                "edge {} → {}: node {} is a SOURCE, which can't receive events",
                edge.source_node_id, edge.target_node_id, edge.target_node_id
            ));
        }
    }
    let ids: Vec<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
    let pairs: Vec<(&str, &str)> = edges
        .iter()
        .map(|e| (e.source_node_id.as_str(), e.target_node_id.as_str()))
        .collect();
    let in_cycles = graph::nodes_in_cycles(&ids, &pairs);
    for node in &nodes {
        if in_cycles.contains(node.id.as_str())
            && node.config.runtime.when_full() == Some(WhenFull::Block)
        {
            errors.push(format!(
                "node {}: it's on a cycle, where queue.when_full \"block\" can deadlock; use \"drop_newest\" or \"drop_oldest\"",
                node.id
            ));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(ServiceError::Invalid(errors))
    }
}

/// What [`Service::check`] found.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct SceneCheck {
    /// Why the scene can't be enabled (e.g. a plugin isn't installed, a remote
    /// node has no token). Empty = it can run.
    pub problems: Vec<String>,
    /// Worth knowing, but not blocking (e.g. a different plugin version).
    pub warnings: Vec<String>,
}

/// Upper bound for a scene's `max_hops` (the DB has the same CHECK). High
/// enough for complex scenes; low enough that a typo can't make a cycle
/// effectively unbounded.
pub(crate) const MAX_HOPS_LIMIT: u32 = 10_000;

/// A scene's own settings, for create and update.
#[derive(Debug, Clone, Default)]
pub(crate) struct SceneInput {
    pub name: String,
    /// `None` = use the server default (`[director] max_hops`).
    pub max_hops: Option<u32>,
}

impl SceneInput {
    #[cfg(test)]
    pub(crate) fn named(name: &str) -> Self {
        Self {
            name: name.into(),
            max_hops: None,
        }
    }
}

/// Checks a scene's settings; returns every problem.
fn check_scene(input: &SceneInput) -> Result<(String, Option<u32>), ServiceError> {
    let mut problems = vec![];
    let name = match required_name(&input.name) {
        Ok(name) => name,
        Err(ServiceError::Invalid(mut errors)) => {
            problems.append(&mut errors);
            String::new()
        }
        Err(other) => return Err(other),
    };
    if let Some(hops) = input.max_hops
        && !(1..=MAX_HOPS_LIMIT).contains(&hops)
    {
        problems.push(format!("max_hops must be between 1 and {MAX_HOPS_LIMIT}"));
    }
    if problems.is_empty() {
        Ok((name, input.max_hops))
    } else {
        Err(ServiceError::Invalid(problems))
    }
}

fn required_name(name: &str) -> Result<String, ServiceError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(ServiceError::Invalid(vec!["name is empty".into()]));
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return Err(ServiceError::Invalid(vec![format!(
            "name is longer than {MAX_NAME_CHARS} characters"
        )]));
    }
    Ok(name.to_string())
}

fn optional_name(name: Option<String>, errors: &mut Vec<String>) -> Option<String> {
    let name = name.map(|n| n.trim().to_string()).filter(|n| !n.is_empty());
    if name
        .as_ref()
        .is_some_and(|n| n.chars().count() > MAX_NAME_CHARS)
    {
        errors.push(format!("name is longer than {MAX_NAME_CHARS} characters"));
    }
    name
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::modules::director::handle::DirectorCommand;
    use crate::modules::plugin::service::tests::{
        Fixture as PluginFixture, fixture as plugin_fixture,
    };
    use serde_json::json;
    use tokio::sync::mpsc;

    pub(crate) struct Fixture {
        /// Keeps the temp dir and the plugin registry alive for the test.
        #[allow(dead_code)]
        pub plugins: PluginFixture,
        pub service: &'static Service,
        pub director: tokio::sync::Mutex<mpsc::Receiver<DirectorCommand>>,
    }

    pub(crate) async fn fixture() -> Fixture {
        let plugins = plugin_fixture(|_| {}).await;
        // test-sink: a SINK with a launch-args schema (timeout: integer >= 1)
        plugins
            .service
            .install(&plugins.upload("test-sink", "1.0.0"))
            .await
            .unwrap();
        install_other_types(&plugins).await;
        let (director, rx) = DirectorHandle::channel();
        let service: &'static Service = Box::leak(Box::new(Service::new(
            plugins.service.db(),
            plugins.service,
            director,
        )));
        Fixture {
            plugins,
            service,
            director: tokio::sync::Mutex::new(rx),
        }
    }

    /// Registers a SOURCE (`test-source`) and a TRANSFORM (`test-transform`)
    /// straight in the DB; their files aren't needed for config-time tests.
    async fn install_other_types(plugins: &PluginFixture) {
        let Database::SQLite(db) = plugins.service.db();
        let mut conn = db.get_connection().await.unwrap();
        for (id, t, manifest) in [
            (
                "test-source",
                "SOURCE",
                r#"{"manifest_version":1,"id":"test-source","name":"S","version":"1.0.0","type":"SOURCE","executable":"run","payload_schemas":{"out":"o.json"}}"#,
            ),
            (
                "test-transform",
                "TRANSFORM",
                r#"{"manifest_version":1,"id":"test-transform","name":"T","version":"1.0.0","type":"TRANSFORM","executable":"run","payload_schemas":{"in":"i.json","out":"o.json"}}"#,
            ),
        ] {
            sqlx::query("INSERT INTO plugins (id, name, version, \"type\", manifest, install_path, archive_sha256) VALUES ($1, $1, '1.0.0', $2, $3, $4, 'x')")
                .bind(id)
                .bind(t)
                .bind(manifest)
                .bind(format!("/nonexistent/{id}"))
                .execute(&mut *conn)
                .await
                .unwrap();
        }
    }

    impl Fixture {
        pub(crate) async fn scene(&self) -> Scene {
            self.service
                .create_scene(&SceneInput::named("scene"))
                .await
                .unwrap()
        }

        pub(crate) async fn node(&self, scene: &str, plugin: &str, config: Value) -> Node {
            self.service
                .create_node(scene, node_input(plugin, config))
                .await
                .unwrap()
        }

        pub(crate) async fn edge(
            &self,
            scene: &str,
            from: &str,
            to: &str,
        ) -> Result<Edge, ServiceError> {
            self.service
                .create_edge(scene, edge_input(from, to, Value::Null))
                .await
        }

        /// Commands the director received so far.
        pub(crate) async fn commands(&self) -> Vec<String> {
            let mut rx = self.director.lock().await;
            let mut out = vec![];
            while let Ok(cmd) = rx.try_recv() {
                out.push(match cmd {
                    DirectorCommand::Reload { scene_id, .. } => format!("reload {scene_id}"),
                    DirectorCommand::Stop { scene_id, .. } => format!("stop {scene_id}"),
                    DirectorCommand::Status { scene_id, .. } => format!("status {scene_id}"),
                    DirectorCommand::Overview { .. } => "overview".to_string(),
                    DirectorCommand::Snapshot { .. } => "snapshot".to_string(),
                });
            }
            out
        }
    }

    pub(crate) fn node_input(plugin: &str, config: Value) -> NodeInput {
        NodeInput {
            name: None,
            plugin_id: plugin.into(),
            node_type: None,
            config,
        }
    }

    pub(crate) fn edge_input(from: &str, to: &str, mapping: Value) -> EdgeInput {
        EdgeInput {
            name: None,
            source_node_id: from.into(),
            target_node_id: to.into(),
            mapping,
        }
    }

    fn invalid(e: ServiceError) -> Vec<String> {
        match e {
            ServiceError::Invalid(errors) => errors,
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn node_type_comes_from_the_plugin() {
        let f = fixture().await;
        let s = f.scene().await;
        let node = f
            .node(&s.id, "test-sink", json!({"launch_args": {"timeout": 5}}))
            .await;
        assert_eq!(node.node_type, "SINK");
        assert_eq!(node.config.launch_args, json!({"timeout": 5}));

        let mut input = node_input("test-sink", Value::Null);
        input.node_type = Some("SOURCE".into());
        let errors = invalid(f.service.create_node(&s.id, input).await.unwrap_err());
        assert!(
            errors[0].contains("doesn't match plugin test-sink, which is a SINK"),
            "{errors:?}"
        );
    }

    #[tokio::test]
    async fn node_validation_reports_every_problem() {
        let f = fixture().await;
        let s = f.scene().await;
        let errors = invalid(
            f.service
                .create_node(&s.id, node_input("nope", json!({"launch": "remote"})))
                .await
                .unwrap_err(),
        );
        assert!(
            errors
                .iter()
                .any(|e| e.contains("isn't a registered plugin")),
            "{errors:?}"
        );

        let errors = invalid(
            f.service
                .create_node(
                    &s.id,
                    node_input(
                        "test-sink",
                        json!({"launch_args": {"timeout": 0, "host": "evil"}, "runtime": {"kill_grace_ms": 0}}),
                    ),
                )
                .await
                .unwrap_err(),
        );
        for fragment in [
            "launch_args/timeout",
            "--host is set by the hub",
            "kill_grace_ms must be at least 1",
        ] {
            assert!(
                errors.iter().any(|e| e.contains(fragment)),
                "missing {fragment:?}: {errors:?}"
            );
        }
        assert!(matches!(
            f.service
                .create_node("missing-scene", node_input("test-sink", Value::Null))
                .await
                .unwrap_err(),
            ServiceError::NotFound("scene")
        ));
    }

    #[tokio::test]
    async fn remote_token_is_kept_on_update_and_never_lost() {
        let f = fixture().await;
        let s = f.scene().await;
        let token = "a-long-enough-shared-secret";
        let node = f
            .node(
                &s.id,
                "test-sink",
                json!({"launch": "remote", "token": token}),
            )
            .await;
        assert_eq!(node.config.token.as_deref(), Some(token));
        // update without the token keeps it
        let mut input = node_input("test-sink", json!({"launch": "remote"}));
        input.name = Some("renamed".into());
        let updated = f.service.update_node(&s.id, &node.id, input).await.unwrap();
        assert_eq!(updated.config.token.as_deref(), Some(token));
        assert_eq!(updated.name.as_deref(), Some("renamed"));
        // switching to local requires dropping the token, which the request does by omitting it
        let local = f
            .service
            .update_node(&s.id, &node.id, node_input("test-sink", json!({})))
            .await
            .unwrap();
        assert_eq!(local.config.token, None);
    }

    #[tokio::test]
    async fn edge_direction_rules() {
        let f = fixture().await;
        let s = f.scene().await;
        let src = f.node(&s.id, "test-source", Value::Null).await;
        let tr = f.node(&s.id, "test-transform", Value::Null).await;
        let sink = f.node(&s.id, "test-sink", Value::Null).await;
        f.edge(&s.id, &src.id, &tr.id).await.unwrap();
        f.edge(&s.id, &tr.id, &sink.id).await.unwrap();
        f.edge(&s.id, &src.id, &sink.id).await.unwrap();

        let errors = invalid(f.edge(&s.id, &sink.id, &tr.id).await.unwrap_err());
        assert!(
            errors[0].contains("is a SINK, which can't send events"),
            "{errors:?}"
        );
        let errors = invalid(f.edge(&s.id, &tr.id, &src.id).await.unwrap_err());
        assert!(
            errors[0].contains("is a SOURCE, which can't receive events"),
            "{errors:?}"
        );
        assert_eq!(
            f.service.list_edges(&s.id).await.unwrap().len(),
            3,
            "rejected edges weren't committed"
        );
    }

    #[tokio::test]
    async fn edges_need_nodes_of_the_same_scene_and_are_unique() {
        let f = fixture().await;
        let s1 = f.scene().await;
        let s2 = f.scene().await;
        let a = f.node(&s1.id, "test-source", Value::Null).await;
        let b = f.node(&s1.id, "test-sink", Value::Null).await;
        let other = f.node(&s2.id, "test-sink", Value::Null).await;
        let errors = invalid(f.edge(&s1.id, &a.id, &other.id).await.unwrap_err());
        assert!(
            errors[0].contains("isn't a node of this scene"),
            "{errors:?}"
        );
        f.edge(&s1.id, &a.id, &b.id).await.unwrap();
        assert!(matches!(
            f.edge(&s1.id, &a.id, &b.id).await.unwrap_err(),
            ServiceError::Conflict(..)
        ));
    }

    #[tokio::test]
    async fn mapping_is_validated_and_stored() {
        let f = fixture().await;
        let s = f.scene().await;
        let a = f.node(&s.id, "test-source", Value::Null).await;
        let b = f.node(&s.id, "test-sink", Value::Null).await;
        let bad = json!({"rules": [{"target": "x"}]});
        let errors = invalid(
            f.service
                .create_edge(&s.id, edge_input(&a.id, &b.id, bad))
                .await
                .unwrap_err(),
        );
        assert!(errors[0].contains("exactly one of"), "{errors:?}");
        let good = json!({"rules": [{"target": "url", "value": "http://example.com"}]});
        let edge = f
            .service
            .create_edge(&s.id, edge_input(&a.id, &b.id, good.clone()))
            .await
            .unwrap();
        assert_eq!(edge.mapping, Some(good));
    }

    #[tokio::test]
    async fn block_policy_rejected_on_cycles() {
        let f = fixture().await;
        let s = f.scene().await;
        let t1 = f
            .node(
                &s.id,
                "test-transform",
                json!({"runtime": {"queue": {"when_full": "block"}}}),
            )
            .await;
        let t2 = f.node(&s.id, "test-transform", Value::Null).await;
        f.edge(&s.id, &t1.id, &t2.id).await.unwrap();
        // closing the cycle puts t1 (explicit block) on it
        let errors = invalid(f.edge(&s.id, &t2.id, &t1.id).await.unwrap_err());
        assert!(errors[0].contains("on a cycle"), "{errors:?}");
        // with a drop policy the cycle is fine
        f.service
            .update_node(
                &s.id,
                &t1.id,
                node_input(
                    "test-transform",
                    json!({"runtime": {"queue": {"when_full": "drop_newest"}}}),
                ),
            )
            .await
            .unwrap();
        f.edge(&s.id, &t2.id, &t1.id).await.unwrap();
        // and switching back to block on a node that's now on the cycle is rejected
        let errors = invalid(
            f.service
                .update_node(
                    &s.id,
                    &t2.id,
                    node_input(
                        "test-transform",
                        json!({"runtime": {"queue": {"when_full": "block"}}}),
                    ),
                )
                .await
                .unwrap_err(),
        );
        assert!(errors[0].contains("on a cycle"), "{errors:?}");
        // self-loops count too
        let t3 = f
            .node(
                &s.id,
                "test-transform",
                json!({"runtime": {"queue": {"when_full": "block"}}}),
            )
            .await;
        assert!(f.edge(&s.id, &t3.id, &t3.id).await.is_err());
    }

    #[tokio::test]
    async fn changing_a_nodes_plugin_rechecks_its_edges() {
        let f = fixture().await;
        let s = f.scene().await;
        let src = f.node(&s.id, "test-source", Value::Null).await;
        let tr = f.node(&s.id, "test-transform", Value::Null).await;
        let sink = f.node(&s.id, "test-sink", Value::Null).await;
        f.edge(&s.id, &src.id, &tr.id).await.unwrap();
        f.edge(&s.id, &tr.id, &sink.id).await.unwrap();
        // the transform can't become a sink: it still has an outgoing edge
        let errors = invalid(
            f.service
                .update_node(&s.id, &tr.id, node_input("test-sink", Value::Null))
                .await
                .unwrap_err(),
        );
        assert!(
            errors[0].contains("is a SINK, which can't send events"),
            "{errors:?}"
        );
        assert_eq!(
            f.service.get_node(&s.id, &tr.id).await.unwrap().node_type,
            "TRANSFORM",
            "rolled back"
        );
    }

    #[tokio::test]
    async fn deleting_a_node_removes_its_edges() {
        let f = fixture().await;
        let s = f.scene().await;
        let a = f.node(&s.id, "test-source", Value::Null).await;
        let b = f.node(&s.id, "test-sink", Value::Null).await;
        f.edge(&s.id, &a.id, &b.id).await.unwrap();
        f.service.delete_node(&s.id, &b.id).await.unwrap();
        assert!(f.service.list_edges(&s.id).await.unwrap().is_empty());
        assert!(matches!(
            f.service.delete_node(&s.id, &b.id).await.unwrap_err(),
            ServiceError::NotFound("node")
        ));
    }

    #[tokio::test]
    async fn director_is_told_about_enabled_scenes_only() {
        let f = fixture().await;
        let s = f.scene().await;
        let sink = f.node(&s.id, "test-sink", Value::Null).await;
        assert!(f.commands().await.is_empty(), "disabled scene: no commands");

        let enabled = f.service.set_enabled(&s.id, true).await.unwrap();
        assert!(enabled.enabled);
        assert_eq!(f.commands().await, [format!("reload {}", s.id)]);
        f.service.set_enabled(&s.id, true).await.unwrap();
        assert!(f.commands().await.is_empty(), "enabling twice is a no-op");

        f.service
            .update_node(
                &s.id,
                &sink.id,
                node_input("test-sink", json!({"launch_args": {"timeout": 9}})),
            )
            .await
            .unwrap();
        f.service
            .update_scene(&s.id, &SceneInput::named("renamed"))
            .await
            .unwrap();
        assert_eq!(
            f.commands().await,
            [format!("reload {}", s.id)],
            "graph change reloads, rename doesn't"
        );
        let with_limit = SceneInput {
            name: "renamed".into(),
            max_hops: Some(100),
        };
        let scene = f.service.update_scene(&s.id, &with_limit).await.unwrap();
        assert_eq!(scene.max_hops, Some(100));
        f.service.update_scene(&s.id, &with_limit).await.unwrap();
        assert_eq!(
            f.commands().await,
            [format!("reload {}", s.id)],
            "a new hop limit reloads; the same one again doesn't"
        );

        f.service.set_enabled(&s.id, false).await.unwrap();
        assert_eq!(f.commands().await, [format!("stop {}", s.id)]);

        f.service.set_enabled(&s.id, true).await.unwrap();
        f.commands().await;
        f.service.delete_scene(&s.id).await.unwrap();
        assert_eq!(f.commands().await, [format!("stop {}", s.id)]);
    }

    #[tokio::test]
    async fn scene_settings_are_validated() {
        let f = fixture().await;
        let bad = SceneInput {
            name: " ".into(),
            max_hops: Some(0),
        };
        assert_eq!(
            invalid(f.service.create_scene(&bad).await.unwrap_err()),
            ["name is empty", "max_hops must be between 1 and 10000"]
        );
        let too_high = SceneInput {
            name: "x".into(),
            max_hops: Some(MAX_HOPS_LIMIT + 1),
        };
        assert!(f.service.create_scene(&too_high).await.is_err());
        let ok = SceneInput {
            name: "x".into(),
            max_hops: Some(MAX_HOPS_LIMIT),
        };
        let scene = f.service.create_scene(&ok).await.unwrap();
        assert_eq!(scene.max_hops, Some(MAX_HOPS_LIMIT));
        // Back to the server default.
        let scene = f
            .service
            .update_scene(&scene.id, &SceneInput::named("x"))
            .await
            .unwrap();
        assert_eq!(scene.max_hops, None);
    }

    #[tokio::test]
    async fn edge_mappings_are_checked_against_the_plugin_schemas() {
        let f = fixture().await;
        let s = f.scene().await;
        let source = f.node(&s.id, "test-source", Value::Null).await;
        let sink = f.node(&s.id, "test-sink", Value::Null).await;
        let create = |mapping: Value| {
            f.service
                .create_edge(&s.id, edge_input(&source.id, &sink.id, mapping))
        };
        // test-sink takes {url: STRING, body: BYTES}.
        let errors = invalid(
            create(json!({"rules": [
                {"target": "nope", "value": "x"},
                {"target": "url", "value": 5},
                {"target": "body", "value": "bytes from a string"}
            ]}))
            .await
            .unwrap_err(),
        );
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert!(errors[0].contains("no field \"nope\""), "{errors:?}");
        assert!(errors[1].contains("5 can't be a STRING"), "{errors:?}");
        // Lua: compiled when saved (with the mapping-lua feature), refused without it.
        let lua = create(json!({"rules": [{"target": "url", "lua": "return 'x'"}]})).await;
        #[cfg(feature = "mapping-lua")]
        {
            let edge = lua.unwrap();
            f.service.delete_edge(&s.id, &edge.id).await.unwrap();
            let errors = invalid(
                create(json!({"rules": [{"target": "url", "lua": "return ("}]}))
                    .await
                    .unwrap_err(),
            );
            assert!(errors[0].starts_with("mapping.rules[0].lua:"), "{errors:?}");
        }
        #[cfg(not(feature = "mapping-lua"))]
        {
            let errors = invalid(lua.unwrap_err());
            assert!(
                errors[0].contains("built without Lua support"),
                "{errors:?}"
            );
        }
        let errors = invalid(
            create(json!({"rules": [{"target": "a..b", "value": 1}]}))
                .await
                .unwrap_err(),
        );
        assert!(errors[0].contains("path \"a..b\""), "{errors:?}");
        create(json!({"mode": "replace", "rules": [{"target": "url", "value": "http://x"}]}))
            .await
            .unwrap();
        assert!(
            f.service
                .check(&s.id)
                .await
                .unwrap()
                .problems
                .iter()
                .all(|p| !p.contains("mapping"))
        );
    }

    #[tokio::test]
    async fn enabling_requires_installed_plugins() {
        let f = fixture().await;
        let s = f.scene().await;
        // test-source is registered without files
        f.node(&s.id, "test-source", Value::Null).await;
        match f.service.set_enabled(&s.id, true).await.unwrap_err() {
            ServiceError::Conflict(_, problems) => {
                assert!(problems[0].contains("files are missing"), "{problems:?}")
            }
            other => panic!("expected Conflict, got {other:?}"),
        }
        assert!(!f.service.get_scene(&s.id).await.unwrap().enabled);
        assert!(f.commands().await.is_empty());
    }
}
