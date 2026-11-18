pub(crate) mod apply;
#[cfg(all(test, unix))]
mod configure_tests;
pub(crate) mod draft;
#[cfg(test)]
mod draft_tests;
mod query_sqlite;
mod transfer;
mod uploads;

pub(crate) use uploads::{NodeFiles, NodeUpload, stored_name};

use crate::modules::changes::ChangeLock;
use crate::modules::configurator;
use crate::modules::director::DirectorHandle;
use crate::modules::director::handle::{SceneState, SceneStatus};
use crate::modules::mapping::{self, MappingSpec};
use crate::modules::node_config::{NodeConfig, SceneDefaults, WhenFull};
use crate::modules::plugin::node_rules::{NodeRules, Side};
use crate::modules::plugin::service::{
    Plugin, Service as PluginService, ServiceError as PluginError,
};
use crate::modules::scene::graph;
use chrono::Local;
use flwrs_core::db::{Database, DbError};
use flwrs_plugin::package::configure::{self, Configured};
use flwrs_plugin::schema::payload::SchemaDefinition;
pub(crate) use query_sqlite::Store;
use query_sqlite::{EdgeValues, NodeValues};
use serde_json::Value;
use sqlx::Connection;
use sqlx::error::ErrorKind;
use std::collections::{BTreeMap, HashMap};
use thiserror::Error;
use ulid::Ulid;

const MAX_NAME_CHARS: usize = 200;

#[derive(sqlx::FromRow, Debug, Clone)]
pub(crate) struct Scene {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    /// Hop limit for this scene; `None` = the server default.
    pub max_hops: Option<u32>,
    /// Defaults for the scene's nodes (durable queues, remote auth).
    #[sqlx(json)]
    pub defaults: SceneDefaults,
    /// Counts committed changes to the live graph (settings, nodes, edges,
    /// uploads); a draft remembers the one it's based on.
    pub revision: i64,
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
    /// Layout (`Position` as JSON); not part of the node's content.
    pub ui: Option<String>,
    pub create_time: chrono::DateTime<Local>,
    pub update_time: chrono::DateTime<Local>,
}

/// Where a node sits on the editor's canvas. Layout only: setting it is no
/// change to the graph (no revision, no reload, no draft change).
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Position {
    pub x: f64,
    pub y: f64,
}

/// Canvas coordinates beyond this are refused (a typo, not a layout).
const MAX_COORDINATE: f64 = 1e7;

impl Position {
    pub(crate) fn check(&self) -> Result<(), String> {
        if self.x.is_finite()
            && self.y.is_finite()
            && self.x.abs() <= MAX_COORDINATE
            && self.y.abs() <= MAX_COORDINATE
        {
            Ok(())
        } else {
            Err(format!(
                "position: x and y must be finite numbers within ±{MAX_COORDINATE}"
            ))
        }
    }

    /// As stored in the `ui` column.
    pub(crate) fn to_json(self) -> String {
        serde_json::to_string(&self).expect("a position serializes")
    }

    /// From the `ui` column; anything unreadable counts as no position.
    pub(crate) fn from_json(text: Option<&str>) -> Option<Self> {
        text.and_then(|t| serde_json::from_str::<Self>(t).ok())
            .filter(|p| p.check().is_ok())
    }
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
    /// Where the node sits on the editor's canvas, if placed.
    pub position: Option<Position>,
    pub create_time: chrono::DateTime<Local>,
    pub update_time: chrono::DateTime<Local>,
    /// The launch-args secrets the plugin declares (JSON pointers) and
    /// whether this node has each set. Filled by the API-facing reads and
    /// writes; empty when the plugin isn't installed.
    pub secrets: BTreeMap<String, bool>,
    /// The files uploaded to the node. Filled for the director's snapshot
    /// and for `check`.
    pub uploads: Vec<NodeUpload>,
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
            position: Position::from_json(row.ui.as_deref()),
            create_time: row.create_time,
            update_time: row.update_time,
            secrets: BTreeMap::new(),
            uploads: vec![],
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

#[derive(Debug, Clone)]
pub(crate) struct NodeInput {
    pub name: Option<String>,
    pub plugin_id: String,
    /// Optional; if given it must match the plugin's type.
    pub node_type: Option<String>,
    pub config: Value,
    /// Where to place the node; `None` keeps its position (or none, new).
    pub position: Option<Position>,
}

#[derive(Debug, Clone)]
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
    /// Not something the API hands out (e.g. a secret upload's content).
    #[error("{0}")]
    Forbidden(String),
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
    /// Where nodes' folders and uploaded files are.
    files: NodeFiles,
    /// Serializes graph changes, so whole-scene checks see a stable graph.
    /// Also held while uploaded files are stored or removed. Its release
    /// counts as a change for the live views (`changes()`).
    lock: ChangeLock,
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
        files: NodeFiles,
    ) -> Self {
        Self {
            db,
            plugins,
            director,
            files,
            lock: ChangeLock::new(),
        }
    }

    /// Bumped after every change to scenes, nodes, edges or uploads (for the
    /// live views).
    pub(crate) fn changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.lock.subscribe()
    }

    /// The director this service reports to.
    pub(crate) fn director(&self) -> &DirectorHandle {
        &self.director
    }

    /// The plugin registry's changes (scenes' checks depend on it).
    pub(crate) fn plugin_changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.plugins.changes()
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
        let scene =
            query_sqlite::create_scene(&mut conn, &id, &name, max_hops, &input.defaults).await?;
        log::debug!("Scene service: created scene [{}]", scene.id);
        self.lock.bump();
        Ok(scene)
    }

    /// Replaces the scene's settings (name, hop limit, defaults).
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
        let scene = query_sqlite::update_scene(&mut conn, id, &name, max_hops, &input.defaults)
            .await
            .map_err(not_found("scene"))?;
        query_sqlite::bump_revision(&mut conn, id).await?;
        let scene = Scene {
            revision: scene.revision + 1,
            ..scene
        };
        // A rename doesn't change the workflow; a new hop limit or new
        // defaults do.
        if scene.enabled && (scene.max_hops != before.max_hops || scene.defaults != before.defaults)
        {
            self.director.request_reload(id);
        }
        Ok(scene)
    }

    pub(crate) async fn delete_scene(&self, id: &str) -> Result<(), ServiceError> {
        let _guard = self.lock.lock().await;
        let mut conn = self.conn().await?;
        query_sqlite::get_scene(&mut conn, id)
            .await
            .map_err(not_found("scene"))?;
        query_sqlite::delete_scene(&mut conn, id).await?;
        // Its nodes' uploads went with them (cascade); so do unused files.
        self.collect_garbage(&mut conn).await?;
        // A reload of a scene that's gone stops it (if it runs) and removes
        // its node folders and spool files, enabled or not.
        self.director.request_reload(id);
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

    /// Every scene with its graph's size and its check (the live list).
    /// Checks are per scene, so a scene whose check fails to run still lists
    /// (with the failure as a problem).
    pub(crate) async fn overview_rows(&self) -> Result<Vec<SceneRow>, ServiceError> {
        let mut conn = self.conn().await?;
        let scenes = query_sqlite::all_scenes(&mut conn).await?;
        let sizes: HashMap<String, (i64, i64)> = query_sqlite::graph_sizes(&mut conn)
            .await?
            .into_iter()
            .map(|(id, nodes, edges)| (id, (nodes, edges)))
            .collect();
        let mut rows = Vec::with_capacity(scenes.len());
        for scene in scenes {
            let (nodes, edges) = sizes.get(&scene.id).copied().unwrap_or_default();
            let check = match self.check_nodes(&mut conn, &scene.id).await {
                Ok(check) => check,
                Err(e) => SceneCheck {
                    problems: vec![format!("couldn't check the scene: {e}")],
                    warnings: vec![],
                },
            };
            rows.push(SceneRow {
                scene,
                nodes: usize::try_from(nodes).unwrap_or_default(),
                edges: usize::try_from(edges).unwrap_or_default(),
                check,
            });
        }
        Ok(rows)
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

    /// Where a scene's local nodes listen: (label, setting, address), from
    /// their plugins' `x-flwrs-listen` settings (or those settings' defaults).
    async fn listeners(
        &self,
        conn: &mut sqlx::SqliteConnection,
        store: Store,
        scene_id: &str,
    ) -> Result<Vec<(String, String, String)>, ServiceError> {
        let mut out = vec![];
        for row in query_sqlite::list_nodes(conn, store, scene_id).await? {
            let node = Node::try_from(row)?;
            if node.config.launch != crate::modules::node_config::LaunchMode::Local {
                continue;
            }
            let Ok(plugin) = self.plugins.get(&node.plugin_id).await else {
                continue;
            };
            let Ok(rules) = NodeRules::of(&plugin) else {
                continue;
            };
            let label = match &node.name {
                Some(name) => format!("node {} ({name})", node.id),
                None => format!("node {}", node.id),
            };
            for (field, address) in rules.spec.listen_addresses(&node.config.launch_args) {
                out.push((label.clone(), field, address));
            }
        }
        Ok(out)
    }

    async fn check_nodes(
        &self,
        conn: &mut sqlx::SqliteConnection,
        scene_id: &str,
    ) -> Result<SceneCheck, ServiceError> {
        let defaults = query_sqlite::get_scene(conn, scene_id)
            .await
            .map_err(not_found("scene"))?
            .defaults;
        self.check_store(conn, Store::Live, scene_id, &defaults)
            .await
    }

    /// The scene's check, on its live graph or its draft (with the draft's
    /// scene defaults).
    async fn check_store(
        &self,
        conn: &mut sqlx::SqliteConnection,
        store: Store,
        scene_id: &str,
        defaults: &SceneDefaults,
    ) -> Result<SceneCheck, ServiceError> {
        let mut check = SceneCheck::default();
        for row in query_sqlite::list_nodes(conn, store, scene_id).await? {
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
                    check.problems.extend(
                        [
                            node.config.missing_token(defaults),
                            node.config.unsupported(),
                        ]
                        .into_iter()
                        .flatten()
                        .map(|p| format!("{label}: {p}")),
                    );
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
                validate_against_plugin(&node.config, &plugin, defaults)?
                    .into_iter()
                    .map(|p| format!("{label}: {p}")),
            );
            let rules = NodeRules::of(&plugin).map_err(ServiceError::Unknown)?;
            // A local node's configured fields come from its plugin's configurator.
            if let Some(c) = &node.config.configurator {
                if !configurator::url_allowed(
                    &self.plugins.config().remote_configurator_urls,
                    &c.url,
                ) {
                    check.problems.push(format!(
                        "{label}: config.configurator.url {} isn't within [plugins] remote_configurator_urls",
                        c.url
                    ));
                }
                if c.url.to_ascii_lowercase().starts_with("http://") {
                    check.warnings.push(format!(
                        "{label}: config.configurator.url is plain http: the node's settings (and credentials) cross the network unencrypted; use it on a trusted network only"
                    ));
                }
            }
            let configured_here = node.config.launch
                == crate::modules::node_config::LaunchMode::Local
                || node.config.configurator.is_some();
            if configured_here && !rules.spec.configured_fields().is_empty() {
                // A configurator may leave some of its fields unset; none set
                // means it hasn't run (e.g. waiting for an upload, or imported).
                let never_ran = !rules
                    .spec
                    .configured_fields()
                    .iter()
                    .any(|f| node.config.launch_args.get(f.as_str()).is_some());
                let stale = node.plugin_version.as_deref() != Some(plugin.version.as_str());
                if never_ran || stale {
                    let why = if never_ran {
                        "its plugin's configurator hasn't set its configured settings yet"
                            .to_string()
                    } else {
                        format!(
                            "its configured settings come from another version of plugin {} than {}",
                            plugin.id, plugin.version
                        )
                    };
                    check.problems.push(format!(
                        "{label}: {why}; run its configurator: POST …/nodes/by-id/{}/configure (or save the node)",
                        node.id
                    ));
                }
            }
            // A core count the plugin wouldn't get.
            if node.config.runtime.threads.is_some_and(|n| n > 0) {
                if node.config.launch != crate::modules::node_config::LaunchMode::Local {
                    check.warnings.push(format!(
                        "{label}: config.runtime.threads only applies to local nodes (a remote plugin is started by whoever runs it)"
                    ));
                } else if plugin
                    .parsed_manifest()
                    .is_some_and(|m| m.threads.is_none())
                {
                    check.warnings.push(format!(
                        "{label}: config.runtime.threads is ignored: plugin {} doesn't say how it takes a core count (manifest threads)",
                        plugin.id
                    ));
                }
            }
            if !rules.spec.code_fields().is_empty()
                && rules.code_problems(&node.config.launch_args).is_none()
            {
                check.warnings.push(format!(
                    "{label}: its code isn't checked here (this server is built without Lua); the plugin reports errors when it runs it"
                ));
            }
            // Uploaded files its launch args name (a local node gets them).
            if node.config.launch == crate::modules::node_config::LaunchMode::Local {
                let uploads = query_sqlite::list_uploads(conn, store, &node.id).await?;
                for (field, name, _) in rules.uploads_referenced(&node.config.launch_args) {
                    if !uploads.iter().any(|u| u.name == name) {
                        check.problems.push(format!(
                            "{label}: launch_args/{field} names the uploaded file {name:?}, which the node doesn't have (upload it to …/nodes/by-id/{}/uploads/{name})",
                            node.id
                        ));
                    }
                }
            }
        }
        // Two local nodes listening on the same address: only one can bind it.
        let mine = self.listeners(conn, store, scene_id).await?;
        for (i, (a_label, a_field, a)) in mine.iter().enumerate() {
            for (b_label, b_field, b) in &mine[i + 1..] {
                if listen_clash(a, b) {
                    check.warnings.push(format!(
                        "{a_label} ({a_field} {a}) and {b_label} ({b_field} {b}) listen on the same address: only one of them can bind it"
                    ));
                }
            }
        }
        for other in query_sqlite::enabled_scene_ids(conn).await? {
            if other == scene_id {
                continue;
            }
            let theirs = self.listeners(conn, Store::Live, &other).await?;
            for (a_label, a_field, a) in &mine {
                for (b_label, _, b) in theirs.iter().filter(|(_, _, b)| listen_clash(a, b)) {
                    check.warnings.push(format!(
                        "{a_label} ({a_field} {a}) listens on the same address as {b_label} ({b}) of enabled scene {other}: only one of them can bind it"
                    ));
                }
            }
        }
        // Mappings, against this server's plugin schemas.
        for edge in query_sqlite::list_edges(conn, store, scene_id).await? {
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
                .edge_schemas(
                    conn,
                    store,
                    scene_id,
                    &edge.source_node_id,
                    &edge.target_node_id,
                )
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
    /// plugins (or the nodes' own, when the plugin takes them from the node),
    /// where those are installed with the node's type.
    async fn edge_schemas(
        &self,
        conn: &mut sqlx::SqliteConnection,
        store: Store,
        scene_id: &str,
        source_node_id: &str,
        target_node_id: &str,
    ) -> Result<(Option<SchemaDefinition>, Option<SchemaDefinition>), ServiceError> {
        let mut nodes = [None, None];
        for (slot, node_id) in nodes.iter_mut().zip([source_node_id, target_node_id]) {
            if let Ok(row) = query_sqlite::get_node(conn, store, scene_id, node_id).await {
                *slot = Some(Node::try_from(row)?);
            }
        }
        let [source, target] = nodes;
        self.node_schemas(source.as_ref(), target.as_ref()).await
    }

    /// The source node's `out` and the target node's `in` schema (see
    /// [`Service::edge_schemas`]), for nodes at hand.
    pub(super) async fn node_schemas(
        &self,
        source: Option<&Node>,
        target: Option<&Node>,
    ) -> Result<(Option<SchemaDefinition>, Option<SchemaDefinition>), ServiceError> {
        let mut schemas = [None, None];
        for (slot, (node, side)) in schemas.iter_mut().zip([(source, "out"), (target, "in")]) {
            let Some(node) = node else {
                continue;
            };
            let plugin = match self.plugins.get(&node.plugin_id).await {
                Ok(plugin) if plugin.plugin_type == node.node_type => plugin,
                Ok(_) | Err(PluginError::NotFound) => continue,
                Err(e) => return Err(ServiceError::Unknown(e.to_string())),
            };
            let rules = NodeRules::of(&plugin).map_err(ServiceError::Unknown)?;
            let side = if side == "out" { Side::Out } else { Side::In };
            // A node's broken schema is its own problem (reported with it).
            *slot = rules
                .node_schema(side, &node.config.launch_args)
                .ok()
                .flatten();
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

    /// Starts the queues of the scene's nodes (or of one node) over: while the
    /// scene runs, their queued and in-flight events go and their spool files
    /// are replaced by fresh ones; otherwise its spool files are deleted
    /// (durable queues of a disabled scene keep theirs until then). Returns
    /// how many events went.
    pub(crate) async fn purge_spool(
        &self,
        scene_id: &str,
        node_id: Option<&str>,
    ) -> Result<u64, ServiceError> {
        self.get_scene(scene_id).await?;
        if let Some(node_id) = node_id {
            self.get_node(scene_id, node_id).await?;
        }
        self.director
            .purge(scene_id, node_id)
            .await
            .map_err(|e| ServiceError::Unavailable(format!("director: {e}")))
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
        let _guard = self.lock.lock_read().await;
        let mut conn = self.conn().await?;
        let scene = query_sqlite::get_scene(&mut conn, id)
            .await
            .map_err(not_found("scene"))?;
        let mut nodes = query_sqlite::list_nodes(&mut conn, Store::Live, id)
            .await?
            .into_iter()
            .map(Node::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        let mut uploads = query_sqlite::list_scene_uploads(&mut conn, Store::Live, id).await?;
        for node in &mut nodes {
            node.uploads = uploads.extract_if(.., |u| u.node_id == node.id).collect();
        }
        let edges: Vec<Edge> = query_sqlite::list_edges(&mut conn, Store::Live, id)
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
        let mut nodes = vec![];
        for row in query_sqlite::list_nodes(&mut conn, Store::Live, scene_id).await? {
            nodes.push(self.with_secrets(row.try_into()?).await?);
        }
        Ok(nodes)
    }

    pub(crate) async fn get_node(&self, scene_id: &str, id: &str) -> Result<Node, ServiceError> {
        self.get_node_in(Store::Live, scene_id, id).await
    }

    pub(crate) async fn get_node_in(
        &self,
        store: Store,
        scene_id: &str,
        id: &str,
    ) -> Result<Node, ServiceError> {
        let mut conn = self.conn().await?;
        let node = query_sqlite::get_node(&mut conn, store, scene_id, id)
            .await
            .map_err(not_found("node"))?
            .try_into()?;
        self.with_secrets(node).await
    }

    /// Fills in which of its plugin's secrets the node has.
    async fn with_secrets(&self, mut node: Node) -> Result<Node, ServiceError> {
        match self.plugins.get(&node.plugin_id).await {
            Ok(plugin) => {
                if let Ok(rules) = NodeRules::of(&plugin) {
                    node.secrets = rules.secrets_set(&node.config.launch_args);
                }
                Ok(node)
            }
            Err(PluginError::NotFound) => Ok(node),
            Err(e) => Err(ServiceError::Unknown(e.to_string())),
        }
    }

    pub(crate) async fn create_node(
        &self,
        scene_id: &str,
        input: NodeInput,
    ) -> Result<Node, ServiceError> {
        self.save_node(Store::Live, scene_id, None, input).await
    }

    pub(crate) async fn update_node(
        &self,
        scene_id: &str,
        id: &str,
        input: NodeInput,
    ) -> Result<Node, ServiceError> {
        self.save_node(Store::Live, scene_id, Some(id), input).await
    }

    /// Creates (`id` = `None`) or replaces a node, live or in the scene's
    /// draft (started from live if there's none). A draft takes an incomplete
    /// node (its problems show in the draft's check); live refuses it.
    pub(super) async fn save_node(
        &self,
        store: Store,
        scene_id: &str,
        id: Option<&str>,
        input: NodeInput,
    ) -> Result<Node, ServiceError> {
        let _guard = self.lock.lock().await;
        let mut conn = self.conn().await?;
        let scene = query_sqlite::get_scene(&mut conn, scene_id)
            .await
            .map_err(not_found("scene"))?;
        let defaults = match store {
            Store::Live => scene.defaults.clone(),
            Store::Draft => self.ensure_draft(&mut conn, &scene).await?.defaults,
        };
        let existing: Option<Node> = match id {
            Some(id) => Some(
                query_sqlite::get_node(&mut conn, store, scene_id, id)
                    .await
                    .map_err(not_found("node"))?
                    .try_into()?,
            ),
            None => None,
        };
        let id = id.map_or_else(|| Ulid::generate().to_string(), str::to_string);
        let position = input.position;
        if let Some(p) = &position {
            p.check().map_err(|e| ServiceError::Invalid(vec![e]))?;
        }
        let prepared = self
            .prepare_node(store, &id, input, existing.as_ref(), &defaults)
            .await?;
        let mut tx = conn.begin_with(WRITE_TX).await?;
        let mut row = match existing {
            None => {
                query_sqlite::insert_node(&mut tx, store, scene_id, &id, &prepared.values()).await?
            }
            Some(_) => {
                let row =
                    query_sqlite::update_node(&mut tx, store, scene_id, &id, &prepared.values())
                        .await?;
                // A new plugin type or queue policy can break edges that
                // were valid before.
                check_graph(&mut tx, store, scene_id).await?;
                row
            }
        };
        if let Some(p) = position {
            let ui = p.to_json();
            query_sqlite::set_ui(&mut tx, store, scene_id, &id, Some(&ui)).await?;
            row.ui = Some(ui);
        }
        self.store_configured(&mut tx, store, &id, &prepared)
            .await?;
        self.committed(&mut tx, store, scene_id).await?;
        tx.commit().await?;
        if store == Store::Live {
            self.changed(&scene);
        }
        self.with_secrets(row.try_into()?).await
    }

    /// Places nodes on the canvas, live and in the draft (wherever each one
    /// is; tombstones too). All or nothing: an unknown node or a bad
    /// position refuses the whole request. Layout isn't content: no
    /// revision, no reload, no draft change (the change counter still moves,
    /// so live views see it).
    pub(crate) async fn set_layout(
        &self,
        scene_id: &str,
        nodes: &BTreeMap<String, Option<Position>>,
    ) -> Result<(), ServiceError> {
        let errors: Vec<String> = nodes
            .iter()
            .filter_map(|(id, p)| p.as_ref()?.check().err().map(|e| format!("{id}: {e}")))
            .collect();
        if !errors.is_empty() {
            return Err(ServiceError::Invalid(errors));
        }
        let _guard = self.lock.lock().await;
        let mut conn = self.conn().await?;
        query_sqlite::get_scene(&mut conn, scene_id)
            .await
            .map_err(not_found("scene"))?;
        let mut tx = conn.begin_with(WRITE_TX).await?;
        let mut unknown = vec![];
        for (id, position) in nodes {
            let ui = position.map(Position::to_json);
            let live =
                query_sqlite::set_ui(&mut tx, Store::Live, scene_id, id, ui.as_deref()).await?;
            let draft =
                query_sqlite::set_ui(&mut tx, Store::Draft, scene_id, id, ui.as_deref()).await?;
            if !live && !draft {
                unknown.push(format!("{id}: no such node in this scene"));
            }
        }
        if !unknown.is_empty() {
            return Err(ServiceError::Invalid(unknown));
        }
        tx.commit().await?;
        Ok(())
    }

    /// Records a committed change in the transaction: the live graph's
    /// revision, or the draft's update time.
    pub(super) async fn committed(
        &self,
        tx: &mut sqlx::SqliteConnection,
        store: Store,
        scene_id: &str,
    ) -> Result<(), ServiceError> {
        match store {
            Store::Live => query_sqlite::bump_revision(tx, scene_id).await?,
            Store::Draft => draft::query::touch(tx, scene_id).await?,
        }
        Ok(())
    }

    /// Deletes a node and (by cascade) its edges.
    pub(crate) async fn delete_node(&self, scene_id: &str, id: &str) -> Result<(), ServiceError> {
        let _guard = self.lock.lock().await;
        let mut conn = self.conn().await?;
        let scene = query_sqlite::get_scene(&mut conn, scene_id)
            .await
            .map_err(not_found("scene"))?;
        if query_sqlite::delete_node(&mut conn, Store::Live, scene_id, id).await? == 0 {
            return Err(ServiceError::NotFound("node"));
        }
        query_sqlite::bump_revision(&mut conn, scene_id).await?;
        self.collect_garbage(&mut conn).await?;
        if !scene.enabled {
            // Nothing runs it: its folder goes now. (A running scene's
            // reload removes it once the node has stopped.)
            let dir =
                crate::modules::supervisor::workdir::node_dir(&self.files.nodes_dir, scene_id, id);
            let shown = dir.display().to_string();
            match tokio::task::spawn_blocking(move || {
                crate::modules::supervisor::workdir::remove(&dir)
            })
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(e)) => log::warn!("Scene service: couldn't remove node folder {shown}: {e}"),
                Err(e) => log::warn!("Scene service: couldn't remove node folder {shown}: {e}"),
            }
        }
        self.changed(&scene);
        Ok(())
    }

    /// Validates a node request against its plugin. `existing` is the node
    /// being updated: a remote node's token and its launch-args secrets are
    /// kept if the request omits them.
    ///
    /// In a draft, a node that doesn't validate against its plugin yet is
    /// taken as it is (incomplete): its configurator doesn't run, and its
    /// configured fields are left out until it validates. Errors in its
    /// structure (an unknown plugin, an unreadable config, a wrong type) are
    /// refused either way.
    pub(super) async fn prepare_node(
        &self,
        store: Store,
        node_id: &str,
        input: NodeInput,
        existing: Option<&Node>,
        defaults: &SceneDefaults,
    ) -> Result<PreparedNode, ServiceError> {
        let lenient = store == Store::Draft;
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
        // The configurator endpoint's secrets too (same auth type and user).
        if let (Some(config), Some(existing)) = (config.as_mut(), existing)
            && let (Some(new), Some(old)) = (
                config.configurator.as_mut(),
                existing.config.configurator.as_ref(),
            )
        {
            new.keep_secrets(old);
        }
        // Secrets the request leaves out are kept (same plugin); `null` clears one.
        if let (Some(plugin), Some(config)) = (&plugin, config.as_mut()) {
            let rules = NodeRules::of(plugin).map_err(ServiceError::Unknown)?;
            let empty = Value::Null;
            let before = existing
                .filter(|e| e.plugin_id == plugin.id)
                .map_or(&empty, |e| &e.config.launch_args);
            config.launch_args = rules.merge_secrets(&config.launch_args, before);
        }
        let mut configured = None;
        let mut pending: &[String] = &[];
        let rules = plugin
            .as_ref()
            .map(NodeRules::of)
            .transpose()
            .map_err(ServiceError::Unknown)?;
        if let (Some(plugin), Some(t)) = (&plugin, &input.node_type)
            && *t != plugin.plugin_type
        {
            errors.push(format!(
                "type {t:?} doesn't match plugin {}, which is a {}",
                plugin.id, plugin.plugin_type
            ));
        }
        // A draft's incomplete node: stored without running its configurator.
        let mut incomplete = false;
        if lenient
            && errors.is_empty()
            && let (Some(plugin), Some(config), Some(rules)) = (&plugin, config.as_mut(), &rules)
        {
            let configured_fields = rules.spec.configured_fields();
            let user_part = configure::without_configured(&rules.spec, &config.launch_args);
            let mut probe = config.clone();
            probe.launch_args = user_part.clone();
            incomplete = validate_against_plugin(&probe, plugin, defaults)?
                .iter()
                .any(|e| {
                    !configured_fields
                        .iter()
                        .any(|f| e.starts_with(&format!("config.launch_args/{f}")))
                });
            if incomplete && !configured_fields.is_empty() {
                config.launch_args = user_part;
            }
        }
        if let (Some(plugin), Some(config)) = (&plugin, config.as_mut())
            && errors.is_empty()
            && !incomplete
        {
            match self
                .configure(store, node_id, plugin, config, existing)
                .await?
            {
                // The configurator's own errors are the answer.
                Err(problems) => return Err(ServiceError::Invalid(problems)),
                Ok(Configure::Ran(c)) => configured = Some(c),
                Ok(Configure::Pending) => {
                    pending = rules.as_ref().map_or(&[], |r| r.spec.configured_fields());
                }
                Ok(Configure::Kept | Configure::NotApplicable) => {}
            }
        }
        if let (Some(plugin), Some(config)) = (&plugin, &config)
            && !lenient
        {
            // Waiting for an upload, the configured fields aren't set yet:
            // that's for `check` to report, not a reason to refuse the node.
            errors.extend(
                validate_against_plugin(config, plugin, defaults)?
                    .into_iter()
                    .filter(|e| {
                        !pending
                            .iter()
                            .any(|f| e.starts_with(&format!("config.launch_args/{f}")))
                    }),
            );
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
            configured,
        })
    }

    /// The configurator step of saving a local node whose plugin has one:
    /// values sent for its fields must be the stored ones (it sets them);
    /// it runs when the other settings (or the plugin's version) changed, and
    /// the stored values are kept otherwise. `Ok(Err(..))`: the request's
    /// problems.
    async fn configure(
        &self,
        store: Store,
        node_id: &str,
        plugin: &Plugin,
        config: &mut NodeConfig,
        existing: Option<&Node>,
    ) -> Result<Result<Configure, Vec<String>>, ServiceError> {
        // A remote node runs its configurator at its endpoint, if it has one;
        // without one, it sets the configured fields itself.
        let remote = match config.launch {
            crate::modules::node_config::LaunchMode::Local => None,
            crate::modules::node_config::LaunchMode::Remote => match &config.configurator {
                None => return Ok(Ok(Configure::NotApplicable)),
                Some(c) => Some(c.clone()),
            },
        };
        let Some(package) = configurator::PackageRef::of(plugin).filter(|p| p.has_configurator())
        else {
            return Ok(Ok(Configure::NotApplicable));
        };
        let rules = NodeRules::of(plugin).map_err(ServiceError::Unknown)?;
        let stored =
            existing.filter(|e| e.plugin_id == plugin.id && e.config.launch == config.launch);
        let mut errors = vec![];
        for field in rules.spec.configured_fields() {
            if let Some(value) = config.launch_args.get(field)
                && stored.and_then(|e| e.config.launch_args.get(field)) != Some(value)
            {
                errors.push(format!(
                    "launch_args/{field} is set by the plugin's configurator (leave it out, or send the stored value)"
                ));
            }
        }
        if !errors.is_empty() {
            return Ok(Err(errors));
        }
        let user_part = |args: &Value| configure::without_configured(&rules.spec, args);
        // It runs when the settings, the endpoint or the plugin's version changed.
        if let Some(stored) = stored
            && stored.plugin_version.as_deref() == Some(plugin.version.as_str())
            && user_part(&stored.config.launch_args) == user_part(&config.launch_args)
            && match (&remote, &stored.config.configurator) {
                (None, _) => true,
                (Some(r), Some(s)) => r.same_target(s),
                (Some(_), None) => false,
            }
        {
            config.launch_args = stored.config.launch_args.clone();
            return Ok(Ok(Configure::Kept));
        }
        let mut conn = self.conn().await?;
        let uploads =
            configurator::uploads(&mut conn, store, &self.files.files_dir, node_id).await?;
        drop(conn);
        // A file the settings name may not be uploaded yet (a new node can't
        // have uploads): the configurator runs once it is (`put_upload`);
        // meanwhile the node lacks its configured settings (a `check` problem).
        let endpoint_files = remote.as_ref().map(|r| r.uploads()).unwrap_or_default();
        if rules
            .uploads_referenced(&config.launch_args)
            .iter()
            .any(|(_, name, _)| !uploads.contains_key(name))
            || endpoint_files
                .iter()
                .any(|name| !uploads.contains_key(*name))
        {
            config.launch_args = user_part(&config.launch_args);
            return Ok(Ok(Configure::Pending));
        }
        Ok(self
            .run_configurator(
                &package,
                node_id,
                &config.launch_args,
                uploads,
                remote.as_ref(),
            )
            .await
            .map(|c| {
                config.launch_args = c.launch_args.clone();
                Configure::Ran(c)
            }))
    }

    async fn run_configurator(
        &self,
        package: &configurator::PackageRef,
        node_id: &str,
        launch_args: &Value,
        uploads: HashMap<String, std::path::PathBuf>,
        remote: Option<&crate::modules::node_config::RemoteConfigurator>,
    ) -> Result<Configured, Vec<String>> {
        let limits = self.configure_limits();
        let ran = match remote {
            None => configurator::run(package, launch_args, node_id, uploads, &limits).await,
            #[cfg(feature = "remote")]
            Some(remote) => {
                let allowed = &self.plugins.config().remote_configurator_urls;
                configurator::run_remote(
                    package,
                    remote,
                    launch_args,
                    node_id,
                    uploads,
                    &limits,
                    allowed,
                )
                .await
            }
            #[cfg(not(feature = "remote"))]
            Some(_) => Err(vec!["remote nodes aren't supported by this build".into()]),
        };
        let configured = ran.map_err(|errors| {
            errors
                .into_iter()
                .map(|e| format!("configurator: {e}"))
                .collect::<Vec<_>>()
        })?;
        for warning in &configured.warnings {
            log::info!("Scene service: node [{node_id}]: configurator: {warning}");
        }
        Ok(configured)
    }

    pub(crate) fn configure_limits(&self) -> configurator::Limits {
        let c = self.plugins.config();
        configurator::Limits {
            timeout: std::time::Duration::from_millis(c.configure_timeout_ms),
            max_response_bytes: c.configure_max_response_bytes,
            max_file_bytes: self.files.max_upload_bytes,
        }
    }

    /// Stores a configurator's files as the node's uploads (in `tx`).
    pub(super) async fn store_configured(
        &self,
        tx: &mut sqlx::SqliteConnection,
        store: Store,
        node_id: &str,
        prepared: &PreparedNode,
    ) -> Result<(), ServiceError> {
        if let Some(configured) = &prepared.configured {
            configurator::store_files(tx, store, &self.files.files_dir, node_id, configured)
                .await
                .map_err(ServiceError::Unknown)?;
        }
        Ok(())
    }

    /// Runs a local node's configurator again (e.g. after an import from a
    /// server with another version of the plugin) and stores the result.
    pub(crate) async fn reconfigure_node(
        &self,
        scene_id: &str,
        id: &str,
    ) -> Result<Node, ServiceError> {
        let _guard = self.lock.lock().await;
        let mut conn = self.conn().await?;
        let scene = query_sqlite::get_scene(&mut conn, scene_id)
            .await
            .map_err(not_found("scene"))?;
        let node: Node = query_sqlite::get_node(&mut conn, Store::Live, scene_id, id)
            .await
            .map_err(not_found("node"))?
            .try_into()?;
        let plugin = match self.plugins.get(&node.plugin_id).await {
            Ok(p) => p,
            Err(PluginError::NotFound) => {
                return Err(ServiceError::Invalid(vec![format!(
                    "plugin {} isn't installed",
                    node.plugin_id
                )]));
            }
            Err(e) => return Err(ServiceError::Unknown(e.to_string())),
        };
        let package = configurator::PackageRef::of(&plugin)
            .filter(|p| p.has_configurator())
            .ok_or_else(|| {
                ServiceError::Invalid(vec![format!("plugin {} has no configurator", plugin.id)])
            })?;
        let remote = match node.config.launch {
            crate::modules::node_config::LaunchMode::Local => None,
            crate::modules::node_config::LaunchMode::Remote => match &node.config.configurator {
                Some(c) => Some(c.clone()),
                None => {
                    return Err(ServiceError::Invalid(vec![
                        "a remote node without config.configurator sets its configured fields itself".into(),
                    ]));
                }
            },
        };
        let uploads =
            configurator::uploads(&mut conn, Store::Live, &self.files.files_dir, id).await?;
        let configured = self
            .run_configurator(
                &package,
                id,
                &node.config.launch_args,
                uploads,
                remote.as_ref(),
            )
            .await
            .map_err(ServiceError::Invalid)?;
        let mut config = node.config.clone();
        config.launch_args = configured.launch_args.clone();
        let mut errors = validate_against_plugin(&config, &plugin, &scene.defaults)?;
        if !errors.is_empty() {
            errors.insert(0, "the configured node isn't valid".into());
            return Err(ServiceError::Invalid(errors));
        }
        let prepared = PreparedNode {
            name: node.name.clone(),
            node_type: plugin.plugin_type.clone(),
            plugin_id: plugin.id.clone(),
            plugin_version: Some(plugin.version.clone()),
            config_json: serde_json::to_string(&config).expect("config serializes"),
            configured: Some(configured),
        };
        let mut tx = conn.begin_with(WRITE_TX).await?;
        let row = query_sqlite::update_node(&mut tx, Store::Live, scene_id, id, &prepared.values())
            .await?;
        self.store_configured(&mut tx, Store::Live, id, &prepared)
            .await?;
        query_sqlite::bump_revision(&mut tx, scene_id).await?;
        tx.commit().await?;
        self.collect_garbage(&mut conn).await?;
        self.changed(&scene);
        self.with_secrets(row.try_into()?).await
    }

    // ----- edges -----

    pub(crate) async fn list_edges(&self, scene_id: &str) -> Result<Vec<Edge>, ServiceError> {
        let mut conn = self.conn().await?;
        query_sqlite::get_scene(&mut conn, scene_id)
            .await
            .map_err(not_found("scene"))?;
        Ok(query_sqlite::list_edges(&mut conn, Store::Live, scene_id)
            .await?
            .into_iter()
            .map(Edge::from)
            .collect())
    }

    pub(crate) async fn get_edge(&self, scene_id: &str, id: &str) -> Result<Edge, ServiceError> {
        self.get_edge_in(Store::Live, scene_id, id).await
    }

    pub(crate) async fn get_edge_in(
        &self,
        store: Store,
        scene_id: &str,
        id: &str,
    ) -> Result<Edge, ServiceError> {
        let mut conn = self.conn().await?;
        Ok(query_sqlite::get_edge(&mut conn, store, scene_id, id)
            .await
            .map_err(not_found("edge"))?
            .into())
    }

    pub(crate) async fn create_edge(
        &self,
        scene_id: &str,
        input: EdgeInput,
    ) -> Result<Edge, ServiceError> {
        self.write_edge(Store::Live, scene_id, None, input).await
    }

    pub(crate) async fn update_edge(
        &self,
        scene_id: &str,
        id: &str,
        input: EdgeInput,
    ) -> Result<Edge, ServiceError> {
        self.write_edge(Store::Live, scene_id, Some(id), input)
            .await
    }

    pub(crate) async fn delete_edge(&self, scene_id: &str, id: &str) -> Result<(), ServiceError> {
        let _guard = self.lock.lock().await;
        let mut conn = self.conn().await?;
        let scene = query_sqlite::get_scene(&mut conn, scene_id)
            .await
            .map_err(not_found("scene"))?;
        if query_sqlite::delete_edge(&mut conn, Store::Live, scene_id, id).await? == 0 {
            return Err(ServiceError::NotFound("edge"));
        }
        query_sqlite::bump_revision(&mut conn, scene_id).await?;
        self.changed(&scene);
        Ok(())
    }

    /// Creates (`id` = `None`) or replaces an edge, live or in the scene's
    /// draft (started from live if there's none). Edges are checked the same
    /// way in both: their mapping must compile against the nodes' schemas.
    pub(super) async fn write_edge(
        &self,
        store: Store,
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
        if store == Store::Draft {
            self.ensure_draft(&mut conn, &scene).await?;
        }
        for (field, node_id) in [
            ("source_node_id", &input.source_node_id),
            ("target_node_id", &input.target_node_id),
        ] {
            if let Err(sqlx::Error::RowNotFound) =
                query_sqlite::get_node(&mut conn, store, scene_id, node_id).await
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
                    store,
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
                let id = Ulid::generate().to_string();
                query_sqlite::insert_edge(&mut tx, store, scene_id, &id, &values).await
            }
            Some(id) => query_sqlite::update_edge(&mut tx, store, scene_id, id, &values).await,
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
        check_graph(&mut tx, store, scene_id).await?;
        self.committed(&mut tx, store, scene_id).await?;
        tx.commit().await?;
        if store == Store::Live {
            self.changed(&scene);
        }
        Ok(row.into())
    }

    /// Tells the director about a committed change to an enabled scene.
    fn changed(&self, scene: &Scene) {
        if scene.enabled {
            self.director.request_reload(&scene.id);
        }
    }
}

/// What the configurator step of saving a node did.
enum Configure {
    /// It ran: its result.
    Ran(Configured),
    /// The settings didn't change: the stored configured values are kept.
    Kept,
    /// A file the settings name isn't uploaded yet: it runs once it is.
    Pending,
    /// A remote node, or a plugin without a configurator.
    NotApplicable,
}

pub(super) struct PreparedNode {
    name: Option<String>,
    node_type: String,
    plugin_id: String,
    plugin_version: Option<String>,
    config_json: String,
    /// The configurator's run, if it ran: its files become the node's uploads.
    configured: Option<flwrs_plugin::package::configure::Configured>,
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
    defaults: &SceneDefaults,
) -> Result<Vec<String>, ServiceError> {
    let rules = NodeRules::of(plugin).map_err(ServiceError::Unknown)?;
    Ok(config.validate(&rules.plugin_rules(), defaults))
}

/// Whole-scene rules the DB can't express, checked before commit:
/// - a SINK can't be an edge source, and a SOURCE can't be an edge target;
/// - a node on a cycle can't use the `block` queue policy (it can deadlock).
pub(super) async fn check_graph(
    conn: &mut sqlx::SqliteConnection,
    store: Store,
    scene_id: &str,
) -> Result<(), ServiceError> {
    let nodes: Vec<Node> = query_sqlite::list_nodes(conn, store, scene_id)
        .await?
        .into_iter()
        .map(Node::try_from)
        .collect::<Result<_, _>>()?;
    let edges = query_sqlite::list_edges(conn, store, scene_id).await?;
    let pairs: Vec<(&str, &str)> = edges
        .iter()
        .map(|e| (e.source_node_id.as_str(), e.target_node_id.as_str()))
        .collect();
    let errors = graph_rule_errors(&nodes, &pairs);
    if errors.is_empty() {
        Ok(())
    } else {
        Err(ServiceError::Invalid(errors))
    }
}

/// The rules of [`check_graph`] on a graph at hand: edges as (source,
/// target) node ids.
pub(super) fn graph_rule_errors(nodes: &[Node], pairs: &[(&str, &str)]) -> Vec<String> {
    let type_of = |id: &str| {
        nodes
            .iter()
            .find(|n| n.id == id)
            .map(|n| n.node_type.as_str())
    };
    let mut errors = vec![];
    for (source, target) in pairs {
        if type_of(source) == Some("SINK") {
            errors.push(format!(
                "edge {source} → {target}: node {source} is a SINK, which can't send events"
            ));
        }
        if type_of(target) == Some("SOURCE") {
            errors.push(format!(
                "edge {source} → {target}: node {target} is a SOURCE, which can't receive events"
            ));
        }
    }
    let ids: Vec<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
    let in_cycles = graph::nodes_in_cycles(&ids, pairs);
    for node in nodes {
        if in_cycles.contains(node.id.as_str())
            && node.config.runtime.when_full() == Some(WhenFull::Block)
        {
            errors.push(format!(
                "node {}: it's on a cycle, where queue.when_full \"block\" can deadlock; use \"drop_newest\" or \"drop_oldest\"",
                node.id
            ));
        }
    }
    errors
}

/// A scene as the live list shows it.
#[derive(Debug)]
pub(crate) struct SceneRow {
    pub scene: Scene,
    pub nodes: usize,
    pub edges: usize,
    pub check: SceneCheck,
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
    pub defaults: SceneDefaults,
}

impl SceneInput {
    #[cfg(test)]
    pub(crate) fn named(name: &str) -> Self {
        Self {
            name: name.into(),
            ..Default::default()
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

/// Whether two listen addresses (`host:port`) can't both be bound: the same
/// port (not 0) on the same address, or on a wildcard one (`0.0.0.0`, `::`).
/// Addresses that aren't IP:port (host names) clash when they're equal.
fn listen_clash(a: &str, b: &str) -> bool {
    match (
        a.parse::<std::net::SocketAddr>(),
        b.parse::<std::net::SocketAddr>(),
    ) {
        (Ok(a), Ok(b)) => {
            a.port() != 0
                && a.port() == b.port()
                && (a.ip() == b.ip() || a.ip().is_unspecified() || b.ip().is_unspecified())
        }
        _ => a == b,
    }
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
        fixture_with(|_| {}).await
    }

    /// A fixture with the plugin registry's settings adjusted.
    pub(crate) async fn fixture_with(
        config: impl FnOnce(&mut crate::modules::plugin::config::PluginsConfig),
    ) -> Fixture {
        let plugins = plugin_fixture(config).await;
        // test-sink: a SINK with a launch-args schema (timeout: integer >= 1)
        plugins
            .service
            .install(&plugins.upload("test-sink", "1.0.0"))
            .await
            .unwrap();
        install_other_types(&plugins).await;
        let (director, rx) = DirectorHandle::channel();
        let files = NodeFiles {
            nodes_dir: plugins.dir.path().join("nodes"),
            files_dir: plugins.dir.path().join("files"),
            max_upload_bytes: 64 * 1024,
            max_import_bytes: 1024 * 1024,
            max_import_entries: 100,
        };
        let service: &'static Service = Box::leak(Box::new(Service::new(
            plugins.service.db(),
            plugins.service,
            director,
            files,
        )));
        Fixture {
            plugins,
            service,
            director: tokio::sync::Mutex::new(rx),
        }
    }

    /// Registers a SOURCE (`test-source`), a TRANSFORM (`test-transform`) and
    /// a manifest v2 TRANSFORM (`test-script`: payload schemas from its
    /// launch args, a Lua code field, secrets) straight in the DB; their
    /// files aren't needed for config-time tests.
    async fn install_other_types(plugins: &PluginFixture) {
        let Database::SQLite(db) = plugins.service.db();
        let mut conn = db.get_connection().await.unwrap();
        sqlx::query("INSERT INTO plugins (id, name, version, \"type\", manifest, launch_args_schema, out_schema, install_path, archive_sha256) VALUES ('test-script', 'Script', '1.0.0', 'TRANSFORM', $1, $2, $3, '/nonexistent/test-script', 'x')")
            .bind(r#"{"manifest_version":2,"id":"test-script","name":"Script","version":"1.0.0","type":"TRANSFORM","executable":"run","launch_args_schema":"l.json","payload_schemas":{"in":{"from_launch_args":"/schemas/in"},"out":{"from_launch_args":"/schemas/out","default":"o.json"}}}"#)
            .bind(r#"{"type":"object","additionalProperties":false,"properties":{
                "schemas":{"type":"object","properties":{"in":{"type":"object"},"out":{"type":"object"}}},
                "script":{"type":"string","x-flwrs-launch":{"via":"file"},"x-flwrs-code":{"language":"lua","params":["payload","meta"]}},
                "api_key":{"type":"string","writeOnly":true,"x-flwrs-launch":{"via":"env"}},
                "model":{"type":"string","x-flwrs-launch":{"via":"file","source":"upload"}},
                "key":{"type":"string","writeOnly":true,"x-flwrs-launch":{"via":"file","source":"upload"}}}}"#)
            .bind(r#"{"fields":[{"key":"text","type":"STRING"}]}"#)
            .execute(&mut *conn)
            .await
            .unwrap();
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
                    DirectorCommand::Purge {
                        scene_id, node_id, ..
                    } => format!("purge {scene_id} {}", node_id.unwrap_or_default()),
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
            position: None,
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

    #[cfg(not(feature = "remote"))]
    #[tokio::test]
    async fn remote_nodes_need_the_remote_feature() {
        let f = fixture().await;
        let scene = f.scene().await;
        let err = f
            .service
            .create_node(
                &scene.id,
                node_input(
                    "test-sink",
                    json!({"launch": "remote", "token": "a-long-enough-token"}),
                ),
            )
            .await
            .unwrap_err();
        let ServiceError::Invalid(problems) = err else {
            panic!("expected Invalid, got {err:?}");
        };
        assert!(
            problems
                .iter()
                .any(|p| p.contains("built without the remote feature")),
            "{problems:?}"
        );
    }

    #[cfg(feature = "remote")] // creates remote nodes
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
            ..Default::default()
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
        // A reload of a deleted scene stops it and removes its files.
        assert_eq!(f.commands().await, [format!("reload {}", s.id)]);
        // A disabled one too: its node folders and durable spool files go.
        let s2 = f.scene().await;
        f.service.delete_scene(&s2.id).await.unwrap();
        assert_eq!(f.commands().await, [format!("reload {}", s2.id)]);
    }

    #[tokio::test]
    async fn scene_settings_are_validated() {
        let f = fixture().await;
        let bad = SceneInput {
            name: " ".into(),
            max_hops: Some(0),
            ..Default::default()
        };
        assert_eq!(
            invalid(f.service.create_scene(&bad).await.unwrap_err()),
            ["name is empty", "max_hops must be between 1 and 10000"]
        );
        let too_high = SceneInput {
            name: "x".into(),
            max_hops: Some(MAX_HOPS_LIMIT + 1),
            ..Default::default()
        };
        assert!(f.service.create_scene(&too_high).await.is_err());
        let ok = SceneInput {
            name: "x".into(),
            max_hops: Some(MAX_HOPS_LIMIT),
            ..Default::default()
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

    #[test]
    fn listen_clashes() {
        assert!(listen_clash("127.0.0.1:8080", "127.0.0.1:8080"));
        assert!(listen_clash("0.0.0.0:8080", "127.0.0.1:8080"));
        assert!(listen_clash("[::]:8080", "127.0.0.1:8080"));
        assert!(!listen_clash("127.0.0.1:8080", "127.0.0.2:8080"));
        assert!(!listen_clash("127.0.0.1:8080", "127.0.0.1:8081"));
        assert!(!listen_clash("127.0.0.1:0", "127.0.0.1:0"));
        assert!(listen_clash("host:1", "host:1") && !listen_clash("host:1", "other:1"));
    }

    #[tokio::test]
    async fn a_core_count_the_plugin_wont_get_is_warned_about() {
        let f = fixture().await;
        let scene = f.scene().await;
        // test-sink's manifest has no `threads`.
        f.node(
            &scene.id,
            "test-sink",
            json!({"launch": "local", "runtime": {"threads": 2}}),
        )
        .await;
        f.node(
            &scene.id,
            "test-sink",
            json!({"launch": "local", "runtime": {"threads": 0}}),
        )
        .await;
        let check = f.service.check(&scene.id).await.unwrap();
        let found: Vec<_> = check
            .warnings
            .iter()
            .filter(|w| w.contains("runtime.threads"))
            .collect();
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(
            found[0].contains("doesn't say how it takes a core count"),
            "{found:?}"
        );
        // Out of range: refused when saved.
        let refused = f
            .service
            .create_node(
                &scene.id,
                node_input(
                    "test-sink",
                    json!({"launch": "local", "runtime": {"threads": 100000}}),
                ),
            )
            .await;
        assert!(
            matches!(refused, Err(ServiceError::Invalid(e)) if e.join(" ").contains("threads can't exceed"))
        );
    }

    #[tokio::test]
    async fn nodes_listening_on_the_same_address_are_warned_about() {
        let f = fixture().await;
        {
            let Database::SQLite(db) = f.service.plugins.db();
            let mut conn = db.get_connection().await.unwrap();
            sqlx::query("INSERT INTO plugins (id, name, version, \"type\", manifest, launch_args_schema, in_schema, install_path, archive_sha256) VALUES ('test-listener', 'L', '1.0.0', 'SINK', $1, $2, $3, '/nonexistent/test-listener', 'x')")
                .bind(r#"{"manifest_version":2,"id":"test-listener","name":"L","version":"1.0.0","type":"SINK","executable":"run","launch_args_schema":"l.json","payload_schemas":{"in":"i.json"}}"#)
                .bind(r#"{"type":"object","additionalProperties":false,"properties":{
                    "listen":{"type":"string","default":"127.0.0.1:8080","x-flwrs-listen":true}}}"#)
                .bind(r#"{"fields":[{"key":"text","type":"STRING"}]}"#)
                .execute(&mut *conn)
                .await
                .unwrap();
        }
        let listen = |at: Option<&str>| match at {
            Some(at) => json!({"launch": "local", "launch_args": {"listen": at}}),
            None => json!({"launch": "local", "launch_args": {}}),
        };
        let warnings = |c: SceneCheck| {
            c.warnings
                .into_iter()
                .filter(|w| w.contains("same address"))
                .collect::<Vec<_>>()
        };
        let scene = f.scene().await;
        // Both on the default: a clash; port 0 and another port don't.
        let a = f.node(&scene.id, "test-listener", listen(None)).await;
        let b = f.node(&scene.id, "test-listener", listen(None)).await;
        f.node(&scene.id, "test-listener", listen(Some("127.0.0.1:0")))
            .await;
        f.node(&scene.id, "test-listener", listen(Some("127.0.0.1:9000")))
            .await;
        let found = warnings(f.service.check(&scene.id).await.unwrap());
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(
            found[0].contains(&a.id)
                && found[0].contains(&b.id)
                && found[0].contains("127.0.0.1:8080"),
            "{found:?}"
        );
        // Another scene: a wildcard on 9000 clashes with this one's 127.0.0.1:9000, once it's enabled.
        let other = f
            .service
            .create_scene(&SceneInput::named("other"))
            .await
            .unwrap();
        f.node(&other.id, "test-listener", listen(Some("0.0.0.0:9000")))
            .await;
        assert_eq!(warnings(f.service.check(&scene.id).await.unwrap()).len(), 1);
        {
            let Database::SQLite(db) = f.service.plugins.db();
            let mut conn = db.get_connection().await.unwrap();
            sqlx::query("UPDATE scenes SET enabled = TRUE WHERE id = $1")
                .bind(&other.id)
                .execute(&mut *conn)
                .await
                .unwrap();
        }
        let found = warnings(f.service.check(&scene.id).await.unwrap());
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(
            found[1].contains(&other.id) && found[1].contains("0.0.0.0:9000"),
            "{found:?}"
        );
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

    fn script_config(extra: Value) -> Value {
        let mut args = json!({
            "schemas": {"in": {"fields": [{"key": "msg", "type": "STRING"}]}},
            "script": "return { url = payload.msg }"
        });
        args.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        json!({"launch_args": args})
    }

    #[tokio::test]
    async fn secrets_are_reported_kept_and_cleared() {
        let f = fixture().await;
        let s = f.scene().await;
        let node = f
            .node(
                &s.id,
                "test-script",
                script_config(json!({"api_key": "k1"})),
            )
            .await;
        assert_eq!(
            node.secrets,
            BTreeMap::from([("/api_key".into(), true), ("/key".into(), false)])
        );
        // left out: kept
        let update = |config: Value| {
            f.service
                .update_node(&s.id, &node.id, node_input("test-script", config))
        };
        let kept = update(script_config(json!({}))).await.unwrap();
        assert_eq!(kept.config.launch_args["api_key"], "k1");
        assert!(kept.secrets["/api_key"]);
        // null: cleared
        let cleared = update(script_config(json!({"api_key": null})))
            .await
            .unwrap();
        assert!(cleared.config.launch_args.get("api_key").is_none());
        assert!(!cleared.secrets["/api_key"]);
        // a value: replaced
        let replaced = update(script_config(json!({"api_key": "k2"})))
            .await
            .unwrap();
        assert_eq!(replaced.config.launch_args["api_key"], "k2");
        assert!(f.service.get_node(&s.id, &node.id).await.unwrap().secrets["/api_key"]);

        // exports leave them out (and say so), unless asked
        let export = f.service.export_scene(&s.id, false).await.unwrap();
        let file = crate::modules::scene::scene_file::decode(&export.bytes).unwrap();
        let config: Value = serde_json::from_str(&file.nodes[0].config_json).unwrap();
        assert!(config["launch_args"].get("api_key").is_none());
        assert_eq!(file.nodes[0].omitted_secrets, ["/api_key"]);
        let imported = f.service.import_scene(&export.bytes).await.unwrap();
        assert!(
            imported
                .check
                .warnings
                .iter()
                .any(|w| w.contains("left out its secrets /api_key")),
            "{:?}",
            imported.check
        );
        let with = f.service.export_scene(&s.id, true).await.unwrap();
        let file = crate::modules::scene::scene_file::decode(&with.bytes).unwrap();
        let config: Value = serde_json::from_str(&file.nodes[0].config_json).unwrap();
        assert_eq!(config["launch_args"]["api_key"], "k2");
        assert!(file.nodes[0].omitted_secrets.is_empty());
    }

    #[tokio::test]
    async fn node_schemas_and_code_are_checked_when_saved() {
        let f = fixture().await;
        let s = f.scene().await;
        let create = |config: Value| {
            f.service
                .create_node(&s.id, node_input("test-script", config))
        };
        let errors = invalid(
            create(json!({"launch_args": {"script": "return 1"}}))
                .await
                .unwrap_err(),
        );
        assert!(
            errors.iter().any(
                |e| e.contains("launch_args/schemas/in: the node's payload schema is required")
            ),
            "{errors:?}"
        );
        let errors = invalid(
            create(script_config(
                json!({"schemas": {"in": {"fields": [{"key": "a"}]}}}),
            ))
            .await
            .unwrap_err(),
        );
        assert!(
            errors.iter().any(|e| e.contains("launch_args/schemas/in")),
            "{errors:?}"
        );
        #[cfg(feature = "mapping-lua")]
        {
            let errors = invalid(
                create(script_config(json!({"script": "local x =\nreturn ("})))
                    .await
                    .unwrap_err(),
            );
            assert!(
                errors[0].starts_with("config.launch_args/script: launch_args/script:2:"),
                "{errors:?}"
            );
        }
        assert!(create(script_config(json!({}))).await.is_ok());
    }

    #[tokio::test]
    async fn mappings_use_the_nodes_own_schemas() {
        let f = fixture().await;
        let s = f.scene().await;
        let source = f.node(&s.id, "test-source", Value::Null).await;
        // its out schema is the package's default: {text: STRING}
        let script = f.node(&s.id, "test-script", script_config(json!({}))).await;
        let sink = f.node(&s.id, "test-sink", Value::Null).await;
        // into the script: its own in schema {msg: STRING}
        let to_script = |mapping: Value| {
            f.service
                .create_edge(&s.id, edge_input(&source.id, &script.id, mapping))
        };
        let errors = invalid(
            to_script(json!({"rules": [{"target": "text", "value": "x"}]}))
                .await
                .unwrap_err(),
        );
        assert!(errors[0].contains("no field \"text\""), "{errors:?}");
        to_script(json!({"rules": [{"target": "msg", "value": "x"}]}))
            .await
            .unwrap();
        // out of the script: the default out schema {text: STRING}
        f.service
            .create_edge(
                &s.id,
                edge_input(
                    &script.id,
                    &sink.id,
                    json!({"rules": [{"target": "url", "source": "text"}]}),
                ),
            )
            .await
            .unwrap();
        // the node changes its in schema: the mapping into it no longer fits
        f.service
            .update_node(
                &s.id,
                &script.id,
                node_input(
                    "test-script",
                    script_config(json!({"schemas": {"in": {"fields": [{"key": "other", "type": "STRING"}]}}})),
                ),
            )
            .await
            .unwrap();
        let check = f.service.check(&s.id).await.unwrap();
        assert!(
            check
                .problems
                .iter()
                .any(|p| p.contains("no field \"msg\"")),
            "{check:?}"
        );
    }

    #[cfg(feature = "remote")] // creates remote nodes
    #[tokio::test]
    async fn remote_nodes_may_carry_launch_args_for_their_schemas() {
        let f = fixture().await;
        let s = f.scene().await;
        let mut config = script_config(json!({}));
        config["launch"] = json!("remote");
        config["token"] = json!("a-long-enough-shared-secret");
        assert!(
            f.service
                .create_node(&s.id, node_input("test-script", config))
                .await
                .is_ok()
        );
        // a plugin without per-node schemas: still refused
        let errors = invalid(
            f.service
                .create_node(
                    &s.id,
                    node_input(
                        "test-sink",
                        json!({"launch": "remote", "token": "a-long-enough-shared-secret", "launch_args": {"timeout": 1}}),
                    ),
                )
                .await
                .unwrap_err(),
        );
        assert!(
            errors[0].contains("only used for local nodes"),
            "{errors:?}"
        );
    }

    async fn upload(f: &Fixture, scene: &str, node: &str, name: &str, content: &[u8]) {
        let path = f.service.incoming_path().await.unwrap();
        std::fs::write(&path, content).unwrap();
        f.service
            .put_upload(scene, node, name, &path)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn archives_carry_uploads_and_leave_secret_ones_out() {
        let f = fixture().await;
        let s = f.scene().await;
        let node = f
            .node(
                &s.id,
                "test-script",
                script_config(json!({"model": "model.onnx", "key": "k.pem"})),
            )
            .await;
        upload(&f, &s.id, &node.id, "model.onnx", b"weights").await;
        upload(&f, &s.id, &node.id, "k.pem", b"PRIVATE").await;

        // the scene file lists every upload
        let plain = f.service.export_scene(&s.id, false).await.unwrap();
        let file = crate::modules::scene::scene_file::decode(&plain.bytes).unwrap();
        let names: Vec<_> = file.nodes[0]
            .files
            .iter()
            .map(|f| f.name.as_str())
            .collect();
        assert_eq!(names, ["k.pem", "model.onnx"]);

        // .far without secrets: the secret field's file stays behind
        let far = f.service.export_archive(&s.id, false).await.unwrap();
        assert!(far.file_name.ends_with(".far"));
        let imported = f.service.import_file(&far.path).await.unwrap();
        assert!(!far.path.exists(), "the import removes its input");
        let snapshot = f.service.snapshot(&imported.scene.id).await.unwrap();
        let uploads: Vec<_> = snapshot.nodes[0]
            .uploads
            .iter()
            .map(|u| u.name.as_str())
            .collect();
        assert_eq!(uploads, ["model.onnx"]);
        assert!(
            imported
                .check
                .warnings
                .iter()
                .any(|w| w.contains("\"k.pem\" isn't in the import")),
            "{:?}",
            imported.check
        );
        let content = f
            .service
            .upload_content(&imported.scene.id, &snapshot.nodes[0].id, "model.onnx")
            .await
            .unwrap();
        assert_eq!(std::fs::read(content.1).unwrap(), b"weights");

        // with secrets: both
        let far = f.service.export_archive(&s.id, true).await.unwrap();
        let imported = f.service.import_file(&far.path).await.unwrap();
        let snapshot = f.service.snapshot(&imported.scene.id).await.unwrap();
        assert_eq!(snapshot.nodes[0].uploads.len(), 2);
    }

    #[tokio::test]
    async fn hand_made_tar_gz_imports_find_files_by_content() {
        use std::io::Write;
        let f = fixture().await;
        let s = f.scene().await;
        let node = f
            .node(
                &s.id,
                "test-script",
                script_config(json!({"model": "model.onnx"})),
            )
            .await;
        upload(&f, &s.id, &node.id, "model.onnx", b"weights").await;
        let plain = f.service.export_scene(&s.id, false).await.unwrap();

        let mut builder = tar::Builder::new(vec![]);
        for (name, data) in [
            ("exported.flwr", &plain.bytes[..]),
            ("models/whatever.bin", b"weights"),
            ("notes.txt", b"unreferenced"),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, name, data).unwrap();
        }
        let mut gz = flate2::write::GzEncoder::new(vec![], flate2::Compression::default());
        gz.write_all(&builder.into_inner().unwrap()).unwrap();
        let path = f.service.incoming_path().await.unwrap();
        std::fs::write(&path, gz.finish().unwrap()).unwrap();

        let imported = f.service.import_file(&path).await.unwrap();
        let snapshot = f.service.snapshot(&imported.scene.id).await.unwrap();
        assert_eq!(snapshot.nodes[0].uploads[0].name, "model.onnx");
        // nothing left in the work area; one stored file serves both nodes
        let incoming = f.service.files().files_dir.join(".incoming");
        assert_eq!(std::fs::read_dir(&incoming).unwrap().count(), 0);
        let stored = std::fs::read_dir(&f.service.files().files_dir)
            .unwrap()
            .filter(|e| {
                !e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with('.')
            })
            .count();
        assert_eq!(stored, 1);

        // not a scene or an archive
        let path = f.service.incoming_path().await.unwrap();
        std::fs::write(&path, b"hello").unwrap();
        assert!(matches!(
            f.service.import_file(&path).await,
            Err(ServiceError::Invalid(_))
        ));
    }
}
