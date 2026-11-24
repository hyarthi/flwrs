//! Writing a whole graph to live at once: what a draft's submit and the bulk
//! endpoint (`POST …/graph/batch`) share. [`Service::apply_graph`] makes the
//! live graph equal a target graph inside the caller's transaction (by id,
//! touching only what differs), checks the graph rules, and counts one
//! revision; [`Service::after_apply`] then collects unused files and tells
//! the director once.

use super::draft::{content_hash, edge_content_hash, node_hash};
use super::*;

/// A scene's settings and graph, as live should become.
#[derive(Debug)]
pub(crate) struct TargetGraph {
    pub name: String,
    pub max_hops: Option<u32>,
    pub defaults: SceneDefaults,
    pub groups: Vec<Group>,
    pub nodes: Vec<TargetNode>,
    pub edges: Vec<TargetEdge>,
}

#[derive(Debug)]
pub(crate) struct TargetNode {
    pub id: String,
    pub name: Option<String>,
    pub node_type: String,
    pub plugin_id: String,
    pub plugin_version: Option<String>,
    /// The config as stored (JSON text).
    pub config: String,
    pub uploads: Vec<NodeUpload>,
    /// A configurator's run in this change: its files become uploads.
    pub configured: Option<Configured>,
    /// Layout (the `ui` column); not content, so not in the hash.
    pub ui: Option<String>,
}

impl TargetNode {
    pub(crate) fn from_row(row: NodeRow, uploads: Vec<NodeUpload>) -> Self {
        Self {
            id: row.id,
            name: row.name,
            node_type: row.node_type,
            plugin_id: row.plugin_id,
            plugin_version: row.plugin_version,
            config: row.config,
            uploads,
            configured: None,
            ui: row.ui,
        }
    }

    fn hash(&self) -> String {
        content_hash(
            self.name.as_deref(),
            &self.node_type,
            &self.plugin_id,
            self.plugin_version.as_deref(),
            &self.config,
            &self.uploads,
        )
    }

    fn values(&self) -> NodeValues<'_> {
        NodeValues {
            name: self.name.as_deref(),
            node_type: &self.node_type,
            plugin_id: &self.plugin_id,
            plugin_version: self.plugin_version.as_deref(),
            config: &self.config,
        }
    }

    /// As the node model (for checks within a batch).
    fn as_node(&self, scene_id: &str) -> Result<Node, ServiceError> {
        let now = Local::now();
        let mut node: Node = NodeRow {
            id: self.id.clone(),
            name: self.name.clone(),
            scene_id: scene_id.to_string(),
            node_type: self.node_type.clone(),
            plugin_id: self.plugin_id.clone(),
            plugin_version: self.plugin_version.clone(),
            config: self.config.clone(),
            ui: self.ui.clone(),
            create_time: now,
            update_time: now,
        }
        .try_into()?;
        node.uploads = self.uploads.clone();
        Ok(node)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct TargetEdge {
    pub id: String,
    pub name: Option<String>,
    pub source_node_id: String,
    pub target_node_id: String,
    /// The mapping as stored (JSON text); `None` = pass through.
    pub mapping: Option<String>,
    /// Layout (the `ui` column); not content, so not in the hash.
    pub ui: Option<String>,
}

impl TargetEdge {
    pub(crate) fn from_row(row: EdgeRow) -> Self {
        Self {
            id: row.id,
            name: row.name,
            source_node_id: row.source_node_id,
            target_node_id: row.target_node_id,
            mapping: row.mapping,
            ui: row.ui,
        }
    }

    fn hash(&self) -> String {
        edge_content_hash(
            self.name.as_deref(),
            &self.source_node_id,
            &self.target_node_id,
            self.mapping.as_deref(),
        )
    }

    fn values(&self) -> EdgeValues<'_> {
        EdgeValues {
            name: self.name.as_deref(),
            source_node_id: &self.source_node_id,
            target_node_id: &self.target_node_id,
            mapping: self.mapping.as_deref(),
        }
    }
}

/// What [`Service::apply_graph`] changed.
#[derive(Debug, Default)]
pub(crate) struct Applied {
    /// Nodes that are gone (their folders go once nothing runs them).
    pub removed_nodes: Vec<String>,
    /// The new live revision.
    pub revision: i64,
}

/// One change of a batch (`POST …/graph/batch`). Nodes and edges created
/// earlier in the batch are named `$<key>` by later operations.
#[derive(Debug, Clone)]
pub(crate) enum BatchOp {
    UpdateScene(SceneInput),
    CreateNode {
        key: Option<String>,
        input: NodeInput,
    },
    UpdateNode {
        id: String,
        input: NodeInput,
    },
    DeleteNode {
        id: String,
    },
    CreateEdge {
        key: Option<String>,
        input: EdgeInput,
    },
    UpdateEdge {
        id: String,
        input: EdgeInput,
    },
    DeleteEdge {
        id: String,
    },
}

impl BatchOp {
    fn name(&self) -> &'static str {
        match self {
            BatchOp::UpdateScene(_) => "update_scene",
            BatchOp::CreateNode { .. } => "create_node",
            BatchOp::UpdateNode { .. } => "update_node",
            BatchOp::DeleteNode { .. } => "delete_node",
            BatchOp::CreateEdge { .. } => "create_edge",
            BatchOp::UpdateEdge { .. } => "update_edge",
            BatchOp::DeleteEdge { .. } => "delete_edge",
        }
    }
}

/// What a batch did.
#[derive(Debug, Clone)]
pub(crate) struct BatchResult {
    pub scene: Scene,
    /// Per operation, the id of the node or edge it created or changed
    /// (`None` for scene settings).
    pub ids: Vec<Option<String>>,
    /// Keys given to created nodes and edges → their ids.
    pub keys: BTreeMap<String, String>,
}

impl Service {
    /// Makes the live graph equal `target` in `tx`: settings, then edges
    /// that go or change, nodes that go, nodes added or changed (and their
    /// uploads), edges added or changed. Unchanged rows aren't touched. Then
    /// the graph rules, and one revision.
    pub(super) async fn apply_graph(
        &self,
        tx: &mut sqlx::SqliteConnection,
        scene: &Scene,
        target: &TargetGraph,
    ) -> Result<Applied, ServiceError> {
        let id = &scene.id;
        if scene.name != target.name
            || scene.max_hops != target.max_hops
            || scene.defaults != target.defaults
            || scene.groups != target.groups
        {
            query_sqlite::update_scene(
                tx,
                id,
                &target.name,
                target.max_hops,
                &target.defaults,
                &target.groups,
            )
            .await?;
        }

        let mut live_uploads: HashMap<String, Vec<NodeUpload>> = HashMap::new();
        for u in query_sqlite::list_scene_uploads(tx, Store::Live, id).await? {
            live_uploads.entry(u.node_id.clone()).or_default().push(u);
        }
        let live_nodes = query_sqlite::list_nodes(tx, Store::Live, id).await?;
        let live_edges = query_sqlite::list_edges(tx, Store::Live, id).await?;

        let target_edges: HashMap<&str, &TargetEdge> =
            target.edges.iter().map(|e| (e.id.as_str(), e)).collect();
        let mut kept_edges = std::collections::HashSet::new();
        for edge in &live_edges {
            let same = target_edges
                .get(edge.id.as_str())
                .is_some_and(|t| t.hash() == super::draft::edge_hash(edge));
            if same {
                kept_edges.insert(edge.id.clone());
            } else {
                query_sqlite::delete_edge(tx, Store::Live, id, &edge.id).await?;
            }
        }

        let target_nodes: HashMap<&str, &TargetNode> =
            target.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
        let mut applied = Applied::default();
        let mut live_by_id = HashMap::new();
        for node in live_nodes {
            if target_nodes.contains_key(node.id.as_str()) {
                live_by_id.insert(node.id.clone(), node);
            } else {
                query_sqlite::delete_node(tx, Store::Live, id, &node.id).await?;
                applied.removed_nodes.push(node.id);
            }
        }
        for t in &target.nodes {
            match live_by_id.get(&t.id) {
                None => {
                    query_sqlite::insert_node(tx, Store::Live, id, &t.id, &t.values()).await?;
                    draft::query::replace_uploads(tx, Store::Live, &t.id, &t.uploads).await?;
                    if t.ui.is_some() {
                        query_sqlite::set_ui(tx, Store::Live, id, &t.id, t.ui.as_deref()).await?;
                    }
                }
                Some(live) => {
                    let files = live_uploads.get(&t.id).map(Vec::as_slice).unwrap_or(&[]);
                    if node_hash(live, files) != t.hash() {
                        query_sqlite::update_node(tx, Store::Live, id, &t.id, &t.values()).await?;
                        draft::query::replace_uploads(tx, Store::Live, &t.id, &t.uploads).await?;
                    }
                    if t.ui.is_some() && live.ui != t.ui {
                        query_sqlite::set_ui(tx, Store::Live, id, &t.id, t.ui.as_deref()).await?;
                    }
                }
            }
            if let Some(configured) = &t.configured {
                configurator::store_files(
                    tx,
                    Store::Live,
                    &self.files.files_dir,
                    &t.id,
                    configured,
                )
                .await
                .map_err(ServiceError::Unknown)?;
            }
        }

        for t in &target.edges {
            if kept_edges.contains(&t.id) {
                let live = live_edges.iter().find(|e| e.id == t.id);
                if t.ui.is_some() && live.is_some_and(|e| e.ui != t.ui) {
                    query_sqlite::set_edge_ui(tx, Store::Live, id, &t.id, t.ui.as_deref()).await?;
                }
            } else {
                query_sqlite::insert_edge(tx, Store::Live, id, &t.id, &t.values())
                    .await
                    .map_err(|e| match e.as_database_error().map(|d| d.kind()) {
                        Some(ErrorKind::UniqueViolation) => ServiceError::Conflict(
                            "edge already exists".into(),
                            vec![format!(
                                "there is already an edge from {} to {}",
                                t.source_node_id, t.target_node_id
                            )],
                        ),
                        Some(ErrorKind::ForeignKeyViolation) => {
                            ServiceError::Invalid(vec![format!(
                                "edge {}: its nodes aren't both in the scene",
                                t.id
                            )])
                        }
                        _ => e.into(),
                    })?;
                if t.ui.is_some() {
                    query_sqlite::set_edge_ui(tx, Store::Live, id, &t.id, t.ui.as_deref()).await?;
                }
            }
        }
        check_graph(tx, Store::Live, id).await?;
        query_sqlite::bump_revision(tx, id).await?;
        applied.revision = scene.revision + 1;
        Ok(applied)
    }

    /// After an applied graph is committed: unused files go, and the director
    /// reloads the scene once (if it was enabled). A disabled scene's removed
    /// nodes lose their folders now (a running one's go with the reload).
    pub(super) async fn after_apply(
        &self,
        conn: &mut sqlx::SqliteConnection,
        scene: &Scene,
        applied: &Applied,
        disabled_now: bool,
    ) -> Result<(), ServiceError> {
        self.collect_garbage(conn).await?;
        for node_id in &applied.removed_nodes {
            crate::modules::node_logs::forget(node_id);
        }
        if scene.enabled || disabled_now {
            // A reload of a disabled scene stops it.
            self.director.request_reload(&scene.id);
        } else {
            for node_id in &applied.removed_nodes {
                let dir = crate::modules::supervisor::workdir::node_dir(
                    &self.files.nodes_dir,
                    &scene.id,
                    node_id,
                );
                let shown = dir.display().to_string();
                match tokio::task::spawn_blocking(move || {
                    crate::modules::supervisor::workdir::remove(&dir)
                })
                .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        log::warn!("Scene service: couldn't remove node folder {shown}: {e}")
                    }
                    Err(e) => log::warn!("Scene service: couldn't remove node folder {shown}: {e}"),
                }
            }
        }
        Ok(())
    }

    /// Applies several changes to the live graph at once: each is checked as
    /// its own endpoint would check it, in order (later ones see earlier
    /// ones), then all are written in one transaction, with one revision and
    /// one reload. Nothing is written if any fails. `base_revision`, if
    /// given, must be the live revision.
    pub(crate) async fn batch(
        &self,
        scene_id: &str,
        base_revision: Option<i64>,
        ops: Vec<BatchOp>,
    ) -> Result<BatchResult, ServiceError> {
        let _guard = self.lock.lock().await;
        let mut conn = self.conn().await?;
        let scene = query_sqlite::get_scene(&mut conn, scene_id)
            .await
            .map_err(not_found("scene"))?;
        if let Some(b) = base_revision
            && b != scene.revision
        {
            return Err(ServiceError::Conflict(
                format!("the scene is at revision {}, not {b}", scene.revision),
                vec![],
            ));
        }

        let mut uploads: HashMap<String, Vec<NodeUpload>> = HashMap::new();
        for u in query_sqlite::list_scene_uploads(&mut conn, Store::Live, scene_id).await? {
            uploads.entry(u.node_id.clone()).or_default().push(u);
        }
        let mut target = TargetGraph {
            name: scene.name.clone(),
            max_hops: scene.max_hops,
            defaults: scene.defaults.clone(),
            groups: scene.groups.clone(),
            nodes: query_sqlite::list_nodes(&mut conn, Store::Live, scene_id)
                .await?
                .into_iter()
                .map(|n| {
                    let files = uploads.remove(&n.id).unwrap_or_default();
                    TargetNode::from_row(n, files)
                })
                .collect(),
            edges: query_sqlite::list_edges(&mut conn, Store::Live, scene_id)
                .await?
                .into_iter()
                .map(TargetEdge::from_row)
                .collect(),
        };
        drop(conn);

        let mut keys: BTreeMap<String, String> = BTreeMap::new();
        let mut ids = Vec::with_capacity(ops.len());
        for (i, op) in ops.into_iter().enumerate() {
            let name = op.name();
            let id = self
                .batch_op(scene_id, &mut target, &mut keys, op)
                .await
                .map_err(|e| prefix_error(e, &format!("ops[{i}] ({name})")))?;
            ids.push(id);
        }

        let entries = target
            .nodes
            .iter()
            .map(|n| {
                let node = n.as_node(scene_id)?;
                Ok((node.id, node.config, node.plugin_id))
            })
            .collect::<Result<Vec<_>, ServiceError>>()?;
        self.refuse_group_problems(scene.enabled, &target.groups, &entries)
            .await?;

        let mut conn = self.conn().await?;
        let mut tx = conn.begin_with(WRITE_TX).await?;
        let applied = self.apply_graph(&mut tx, &scene, &target).await?;
        tx.commit().await?;
        self.after_apply(&mut conn, &scene, &applied, false).await?;
        log::info!(
            "Scene service: applied a batch of {} change(s) to scene [{scene_id}] (revision {})",
            ids.len(),
            applied.revision
        );
        let scene = query_sqlite::get_scene(&mut conn, scene_id).await?;
        Ok(BatchResult { scene, ids, keys })
    }

    async fn batch_op(
        &self,
        scene_id: &str,
        target: &mut TargetGraph,
        keys: &mut BTreeMap<String, String>,
        op: BatchOp,
    ) -> Result<Option<String>, ServiceError> {
        // `$key` → the id it got; anything else is an id.
        let resolve = |keys: &BTreeMap<String, String>, r: &str| -> Result<String, ServiceError> {
            match r.strip_prefix('$') {
                Some(key) => keys.get(key).cloned().ok_or_else(|| {
                    ServiceError::Invalid(vec![format!("no earlier operation has key {key:?}")])
                }),
                None => Ok(r.to_string()),
            }
        };
        let add_key = |keys: &mut BTreeMap<String, String>, key: Option<String>, id: &str| match key
        {
            Some(key) if key.is_empty() || keys.contains_key(&key) => {
                Err(ServiceError::Invalid(vec![format!(
                    "key {key:?} is empty or already used in this batch"
                )]))
            }
            Some(key) => {
                keys.insert(key, id.to_string());
                Ok(())
            }
            None => Ok(()),
        };
        match op {
            BatchOp::UpdateScene(input) => {
                let (name, max_hops) = check_scene(&input)?;
                target.name = name;
                target.max_hops = max_hops;
                target.defaults = input.defaults;
                if let Some(groups) = input.groups {
                    target.groups = groups;
                }
                Ok(None)
            }
            BatchOp::CreateNode { key, input } => {
                let id = Ulid::generate().to_string();
                add_key(keys, key, &id)?;
                let ui = layout(&input)?.map(Position::to_json);
                let prepared = self
                    .prepare_node(Store::Live, &id, input, None, &target.defaults)
                    .await?;
                let mut node = target_node(&id, prepared, vec![]);
                node.ui = ui;
                target.nodes.push(node);
                Ok(Some(id))
            }
            BatchOp::UpdateNode { id, input } => {
                let id = resolve(keys, &id)?;
                let index = target
                    .nodes
                    .iter()
                    .position(|n| n.id == id)
                    .ok_or(ServiceError::NotFound("node"))?;
                let existing = target.nodes[index].as_node(scene_id)?;
                let position = layout(&input)?;
                let old_ui = target.nodes[index].ui.take();
                let ui = match position {
                    Some(p) => {
                        let mut merged = NodeLayout::from_json(old_ui.as_deref());
                        merged.set_position(Some(p));
                        merged.to_json()
                    }
                    None => old_ui,
                };
                let prepared = self
                    .prepare_node(Store::Live, &id, input, Some(&existing), &target.defaults)
                    .await?;
                let files = std::mem::take(&mut target.nodes[index].uploads);
                target.nodes[index] = target_node(&id, prepared, files);
                target.nodes[index].ui = ui;
                check_rules(scene_id, target)?;
                Ok(Some(id))
            }
            BatchOp::DeleteNode { id } => {
                let id = resolve(keys, &id)?;
                let before = target.nodes.len();
                target.nodes.retain(|n| n.id != id);
                if target.nodes.len() == before {
                    return Err(ServiceError::NotFound("node"));
                }
                target
                    .edges
                    .retain(|e| e.source_node_id != id && e.target_node_id != id);
                Ok(Some(id))
            }
            BatchOp::CreateEdge { key, input } => {
                let id = Ulid::generate().to_string();
                add_key(keys, key, &id)?;
                let edge = self
                    .batch_edge(scene_id, target, keys, &id, input, &resolve)
                    .await?;
                target.edges.push(edge);
                check_rules(scene_id, target)?;
                Ok(Some(id))
            }
            BatchOp::UpdateEdge { id, input } => {
                let id = resolve(keys, &id)?;
                let index = target
                    .edges
                    .iter()
                    .position(|e| e.id == id)
                    .ok_or(ServiceError::NotFound("edge"))?;
                let edge = self
                    .batch_edge(scene_id, target, keys, &id, input, &resolve)
                    .await?;
                target.edges[index] = edge;
                check_rules(scene_id, target)?;
                Ok(Some(id))
            }
            BatchOp::DeleteEdge { id } => {
                let id = resolve(keys, &id)?;
                let before = target.edges.len();
                target.edges.retain(|e| e.id != id);
                if target.edges.len() == before {
                    return Err(ServiceError::NotFound("edge"));
                }
                Ok(Some(id))
            }
        }
    }

    /// An edge for a batch, checked as `write_edge` checks one.
    async fn batch_edge(
        &self,
        scene_id: &str,
        target: &TargetGraph,
        keys: &BTreeMap<String, String>,
        id: &str,
        input: EdgeInput,
        resolve: &impl Fn(&BTreeMap<String, String>, &str) -> Result<String, ServiceError>,
    ) -> Result<TargetEdge, ServiceError> {
        let mut errors = vec![];
        let name = optional_name(input.name, &mut errors);
        let source = resolve(keys, &input.source_node_id)?;
        let dest = resolve(keys, &input.target_node_id)?;
        let mapping = match MappingSpec::parse(&input.mapping) {
            Ok(spec) => spec,
            Err(e) => {
                errors.extend(e);
                None
            }
        };
        let find = |node_id: &str| target.nodes.iter().find(|n| n.id == node_id);
        for (field, node_id) in [("source_node_id", &source), ("target_node_id", &dest)] {
            if find(node_id).is_none() {
                errors.push(format!("{field} {node_id:?} isn't a node of this scene"));
            }
        }
        if target
            .edges
            .iter()
            .any(|e| e.id != id && e.source_node_id == source && e.target_node_id == dest)
        {
            return Err(ServiceError::Conflict(
                "edge already exists".into(),
                vec![format!("there is already an edge from {source} to {dest}")],
            ));
        }
        if errors.is_empty()
            && let Some(spec) = &mapping
        {
            let source_node = find(&source).map(|n| n.as_node(scene_id)).transpose()?;
            let target_node = find(&dest).map(|n| n.as_node(scene_id)).transpose()?;
            let (s, t) = self
                .node_schemas(source_node.as_ref(), target_node.as_ref())
                .await?;
            if let Err(problems) = mapping::compile(
                spec,
                mapping::Schemas {
                    source: s.as_ref(),
                    target: t.as_ref(),
                },
            ) {
                errors.extend(problems);
            }
        }
        if !errors.is_empty() {
            return Err(ServiceError::Invalid(errors));
        }
        Ok(TargetEdge {
            id: id.to_string(),
            name,
            source_node_id: source,
            target_node_id: dest,
            mapping: mapping.map(|s| serde_json::to_string(&s).expect("mapping serializes")),
            ui: target
                .edges
                .iter()
                .find(|e| e.id == id)
                .and_then(|e| e.ui.clone()),
        })
    }
}

fn target_node(id: &str, prepared: PreparedNode, uploads: Vec<NodeUpload>) -> TargetNode {
    TargetNode {
        id: id.to_string(),
        name: prepared.name,
        node_type: prepared.node_type,
        plugin_id: prepared.plugin_id,
        plugin_version: prepared.plugin_version,
        config: prepared.config_json,
        uploads,
        configured: prepared.configured,
        ui: None,
    }
}

/// A node input's position as the `ui` column holds it (checked).
fn layout(input: &NodeInput) -> Result<Option<Position>, ServiceError> {
    input
        .position
        .map(|p| {
            p.check().map_err(|e| ServiceError::Invalid(vec![e]))?;
            Ok(p)
        })
        .transpose()
}

/// The graph rules (`check_graph`) on a batch's graph in memory.
fn check_rules(scene_id: &str, target: &TargetGraph) -> Result<(), ServiceError> {
    let nodes = target
        .nodes
        .iter()
        .map(|n| n.as_node(scene_id))
        .collect::<Result<Vec<_>, _>>()?;
    let edges: Vec<(&str, &str)> = target
        .edges
        .iter()
        .map(|e| (e.source_node_id.as_str(), e.target_node_id.as_str()))
        .collect();
    let errors = graph_rule_errors(&nodes, &edges);
    if errors.is_empty() {
        Ok(())
    } else {
        Err(ServiceError::Invalid(errors))
    }
}

/// Puts `context: ` before an error's details (or message).
fn prefix_error(e: ServiceError, context: &str) -> ServiceError {
    match e {
        ServiceError::Invalid(details) => ServiceError::Invalid(
            details
                .into_iter()
                .map(|d| format!("{context}: {d}"))
                .collect(),
        ),
        ServiceError::Conflict(message, details) => {
            ServiceError::Conflict(format!("{context}: {message}"), details)
        }
        ServiceError::NotFound(what) => {
            ServiceError::Invalid(vec![format!("{context}: {what} not found")])
        }
        other => other,
    }
}
