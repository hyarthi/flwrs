//! Scene drafts: a scene's graph edited apart from the live one, then
//! submitted at once (one transaction, one revision, one reload).
//!
//! - At most one draft per scene, started from the live graph on its first
//!   edit (or explicitly). Node and edge ids are the live ones; a node added
//!   in the draft gets its id there.
//! - Every draft row remembers the content hash of the live row it was
//!   copied from (`base_hash`; `None` = added in the draft). Rows removed in
//!   the draft stay as tombstones (`removed`), so a diff can show them and a
//!   rebase knows what the draft deleted.
//! - Saves are lenient: an incomplete node is stored (its problems show in
//!   the draft's check); structural errors are refused as live.
//! - The draft records the live `revision` it's based on. Submitting needs
//!   it to be the current one: after direct edits to live, the draft is
//!   rebased first (`rebase`): a three-way merge per node, edge and the
//!   settings (base, draft, live). Where both sides changed the same thing,
//!   the row is marked with a conflict, and submit waits until each is
//!   resolved (`resolve`: keep the draft's, or take live's).
//! - Submitting to an enabled scene runs the pre-flight checks first
//!   (`submit`): a draft that fails them stays a draft, marked.

pub(crate) mod history;
pub(crate) mod query;
pub(crate) mod submit;

use super::apply::{TargetEdge, TargetGraph, TargetNode};
use super::*;
use query::{DraftRow, RowMeta};
use std::collections::HashSet;
use submit::DraftSubmit;

const NODES: &str = "scene_draft_nodes";
const EDGES: &str = "scene_draft_edges";

/// A row's state against the draft's base.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Change {
    Unchanged,
    Added,
    Changed,
    Removed,
}

/// Why a rebase couldn't merge a row by itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Conflict {
    /// Changed in the draft and live, differently.
    ChangedBoth,
    /// Removed live, changed in the draft.
    RemovedLive,
    /// Changed live, removed in the draft.
    ChangedLive,
}

impl Conflict {
    fn as_str(self) -> &'static str {
        match self {
            Conflict::ChangedBoth => "changed_both",
            Conflict::RemovedLive => "removed_live",
            Conflict::ChangedLive => "changed_live",
        }
    }

    fn parse(text: Option<&str>) -> Option<Self> {
        match text? {
            "changed_both" => Some(Conflict::ChangedBoth),
            "removed_live" => Some(Conflict::RemovedLive),
            "changed_live" => Some(Conflict::ChangedLive),
            _ => None,
        }
    }

    fn describe(self) -> &'static str {
        match self {
            Conflict::ChangedBoth => "changed both here and live",
            Conflict::RemovedLive => "removed live, but changed here",
            Conflict::ChangedLive => "changed live, but removed here",
        }
    }
}

/// A draft node with its state.
#[derive(Debug, Clone)]
pub(crate) struct DraftNode {
    /// Its content (a tombstone's last content).
    pub node: Node,
    pub change: Change,
    pub conflict: Option<Conflict>,
    /// For a changed node, what differs from live: `name`, `plugin`,
    /// `config.<key>`, `uploads`.
    pub parts: Vec<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct DraftEdge {
    pub edge: Edge,
    pub change: Change,
    pub conflict: Option<Conflict>,
    /// For a changed edge: `name`, `source`, `target`, `mapping`.
    pub parts: Vec<String>,
}

/// A draft as a whole.
#[derive(Debug, Clone)]
pub(crate) struct Draft {
    pub row: DraftRow,
    /// The live graph's revision now: the draft is behind when it differs
    /// from `row.base_revision` (rebase it before submitting).
    pub live_revision: i64,
    pub settings_change: Change,
    pub settings_conflict: Option<Conflict>,
    pub nodes: Vec<DraftNode>,
    pub edges: Vec<DraftEdge>,
    /// The step undo would take back, and the one redo would make again.
    pub undo: Option<String>,
    pub redo: Option<String>,
    /// A submit its pre-flight check held back (`WAITING` / `FAILED`).
    pub submit: Option<DraftSubmit>,
}

impl Draft {
    pub(crate) fn behind(&self) -> bool {
        self.row.base_revision != self.live_revision
    }

    /// Every unresolved conflict, described.
    pub(crate) fn conflicts(&self) -> Vec<String> {
        let mut out = vec![];
        if let Some(c) = self.settings_conflict {
            out.push(format!("scene settings: {}", c.describe()));
        }
        for n in &self.nodes {
            if let Some(c) = n.conflict {
                out.push(format!("node {}: {}", n.node.id, c.describe()));
            }
        }
        for e in &self.edges {
            if let Some(c) = e.conflict {
                out.push(format!("edge {}: {}", e.edge.id, c.describe()));
            }
        }
        out
    }

    /// Whether submitting would change anything.
    pub(crate) fn has_changes(&self) -> bool {
        self.settings_change != Change::Unchanged
            || self.nodes.iter().any(|n| n.change != Change::Unchanged)
            || self.edges.iter().any(|e| e.change != Change::Unchanged)
    }
}

/// What a rebase did.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct RebaseReport {
    pub from_revision: i64,
    pub to_revision: i64,
    /// Rows that took live's version (`node <id>`, `edge <id>`, `settings`).
    pub taken_live: Vec<String>,
    /// Rows now in conflict.
    pub conflicts: Vec<String>,
    /// Edges that went with a node removed on the other side.
    pub dropped_edges: Vec<String>,
}

/// What [`Service::resolve`] keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Keep {
    Draft,
    Live,
}

/// What a conflict resolution is about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Target {
    Settings,
    Node(String),
    Edge(String),
}

// ---- content hashes ----

/// JSON with object keys sorted, at every level.
pub(crate) fn canonical(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            Value::Object(
                keys.into_iter()
                    .map(|k| (k.clone(), canonical(&map[k])))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.iter().map(canonical).collect()),
        other => other.clone(),
    }
}

fn hash_of(value: &Value) -> String {
    sha256::digest(canonical(value).to_string())
}

/// A node's content hash: everything but its id, scene and times (uploads
/// included: a new file changes the node).
pub(super) fn node_hash(row: &NodeRow, uploads: &[NodeUpload]) -> String {
    content_hash(
        row.name.as_deref(),
        &row.node_type,
        &row.plugin_id,
        row.plugin_version.as_deref(),
        &row.config,
        uploads,
    )
}

pub(super) fn content_hash(
    name: Option<&str>,
    node_type: &str,
    plugin_id: &str,
    plugin_version: Option<&str>,
    config: &str,
    uploads: &[NodeUpload],
) -> String {
    let mut files: Vec<(&str, &str)> = uploads
        .iter()
        .map(|u| (u.name.as_str(), u.sha256.as_str()))
        .collect();
    files.sort();
    hash_of(&serde_json::json!({
        "name": name,
        "type": node_type,
        "plugin_id": plugin_id,
        "plugin_version": plugin_version,
        "config": serde_json::from_str::<Value>(config).unwrap_or(Value::Null),
        "uploads": files,
    }))
}

pub(super) fn edge_hash(row: &EdgeRow) -> String {
    edge_content_hash(
        row.name.as_deref(),
        &row.source_node_id,
        &row.target_node_id,
        row.mapping.as_deref(),
    )
}

pub(super) fn edge_content_hash(
    name: Option<&str>,
    source: &str,
    target: &str,
    mapping: Option<&str>,
) -> String {
    hash_of(&serde_json::json!({
        "name": name,
        "source": source,
        "target": target,
        "mapping": mapping.and_then(|m| serde_json::from_str::<Value>(m).ok()),
    }))
}

pub(super) fn settings_hash(
    name: &str,
    max_hops: Option<u32>,
    defaults: &SceneDefaults,
    groups: &[Group],
) -> String {
    let mut settings = serde_json::json!({
        "name": name,
        "max_hops": max_hops,
        "defaults": serde_json::to_value(defaults).unwrap_or(Value::Null),
    });
    // Only when there are some, so the hashes drafts already hold stay right.
    if !groups.is_empty() {
        settings["groups"] = serde_json::to_value(groups).unwrap_or(Value::Null);
    }
    hash_of(&settings)
}

/// What differs between a draft node and its live counterpart.
fn node_parts(
    draft: &NodeRow,
    d_up: &[NodeUpload],
    live: &NodeRow,
    l_up: &[NodeUpload],
) -> Vec<String> {
    let mut parts = vec![];
    if draft.name != live.name {
        parts.push("name".to_string());
    }
    if draft.plugin_id != live.plugin_id
        || draft.plugin_version != live.plugin_version
        || draft.node_type != live.node_type
    {
        parts.push("plugin".to_string());
    }
    let parse = |c: &str| serde_json::from_str::<Value>(c).unwrap_or(Value::Null);
    let (dc, lc) = (parse(&draft.config), parse(&live.config));
    let empty = serde_json::Map::new();
    let (dm, lm) = (
        dc.as_object().unwrap_or(&empty),
        lc.as_object().unwrap_or(&empty),
    );
    let mut keys: Vec<&String> = dm.keys().chain(lm.keys()).collect();
    keys.sort();
    keys.dedup();
    for key in keys {
        if dm.get(key).map(canonical) != lm.get(key).map(canonical) {
            parts.push(format!("config.{key}"));
        }
    }
    let files = |u: &[NodeUpload]| {
        let mut f: Vec<(String, String)> = u
            .iter()
            .map(|u| (u.name.clone(), u.sha256.clone()))
            .collect();
        f.sort();
        f
    };
    if files(d_up) != files(l_up) {
        parts.push("uploads".to_string());
    }
    parts
}

fn edge_parts(draft: &EdgeRow, live: &EdgeRow) -> Vec<String> {
    let mut parts = vec![];
    if draft.name != live.name {
        parts.push("name".to_string());
    }
    if draft.source_node_id != live.source_node_id {
        parts.push("source".to_string());
    }
    if draft.target_node_id != live.target_node_id {
        parts.push("target".to_string());
    }
    let parse = |m: &Option<String>| {
        m.as_deref()
            .and_then(|m| serde_json::from_str::<Value>(m).ok())
            .map(|v| canonical(&v))
    };
    if parse(&draft.mapping) != parse(&live.mapping) {
        parts.push("mapping".to_string());
    }
    parts
}

fn uploads_by_node(uploads: Vec<NodeUpload>) -> HashMap<String, Vec<NodeUpload>> {
    let mut map: HashMap<String, Vec<NodeUpload>> = HashMap::new();
    for u in uploads {
        map.entry(u.node_id.clone()).or_default().push(u);
    }
    map
}

fn no_draft() -> ServiceError {
    ServiceError::NotFound("draft")
}

impl Service {
    /// The scene's draft, started from the live graph if it has none. The
    /// caller holds the service lock.
    pub(super) async fn ensure_draft(
        &self,
        conn: &mut sqlx::SqliteConnection,
        scene: &Scene,
    ) -> Result<DraftRow, ServiceError> {
        if let Some(row) = query::get(conn, &scene.id).await? {
            return Ok(row);
        }
        let mut tx = conn.begin_with(WRITE_TX).await?;
        let base_hash = settings_hash(&scene.name, scene.max_hops, &scene.defaults, &scene.groups);
        let row = query::insert(&mut tx, scene, &base_hash).await?;
        let uploads = uploads_by_node(
            query_sqlite::list_scene_uploads(&mut tx, Store::Live, &scene.id).await?,
        );
        for node in query_sqlite::list_nodes(&mut tx, Store::Live, &scene.id).await? {
            let files = uploads.get(&node.id).map(Vec::as_slice).unwrap_or(&[]);
            let meta = RowMeta {
                id: node.id.clone(),
                base_hash: Some(node_hash(&node, files)),
                removed: false,
                conflict: None,
            };
            query::put_node(&mut tx, &node, &meta).await?;
            query::replace_uploads(&mut tx, Store::Draft, &node.id, files).await?;
        }
        for edge in query_sqlite::list_edges(&mut tx, Store::Live, &scene.id).await? {
            let meta = RowMeta {
                id: edge.id.clone(),
                base_hash: Some(edge_hash(&edge)),
                removed: false,
                conflict: None,
            };
            query::put_edge(&mut tx, &edge, &meta).await?;
        }
        // The first state undo can go back to.
        history::record_start(&mut tx, &scene.id).await?;
        tx.commit().await?;
        log::debug!(
            "Scene service: started a draft of scene [{}] at revision {}",
            scene.id,
            scene.revision
        );
        Ok(row)
    }

    /// Undoes (`back`) or redoes the draft's last change: puts its state
    /// before (or after) in place. Conflict when there's nothing to.
    pub(crate) async fn step_draft(
        &self,
        scene_id: &str,
        back: bool,
    ) -> Result<(Draft, String), ServiceError> {
        let label = {
            let _guard = self.lock.lock().await;
            let mut conn = self.conn().await?;
            self.scene_row(&mut conn, scene_id).await?;
            query::get(&mut conn, scene_id)
                .await?
                .ok_or_else(no_draft)?;
            let mut tx = conn.begin_with(WRITE_TX).await?;
            let label = history::step(&mut tx, scene_id, back)
                .await?
                .ok_or_else(|| {
                    ServiceError::Conflict(
                        format!("nothing to {}", if back { "undo" } else { "redo" }),
                        vec![],
                    )
                })?;
            tx.commit().await?;
            label
        };
        Ok((self.get_draft(scene_id).await?, label))
    }

    /// Starts the scene's draft from live (or returns the one it has, and
    /// whether it's new).
    pub(crate) async fn start_draft(&self, scene_id: &str) -> Result<(Draft, bool), ServiceError> {
        let created = {
            let _guard = self.lock.lock().await;
            let mut conn = self.conn().await?;
            let scene = self.scene_row(&mut conn, scene_id).await?;
            let existed = query::get(&mut conn, scene_id).await?.is_some();
            self.ensure_draft(&mut conn, &scene).await?;
            !existed
        };
        Ok((self.get_draft(scene_id).await?, created))
    }

    async fn scene_row(
        &self,
        conn: &mut sqlx::SqliteConnection,
        scene_id: &str,
    ) -> Result<Scene, ServiceError> {
        query_sqlite::get_scene(conn, scene_id)
            .await
            .map_err(not_found("scene"))
    }

    /// The scene's draft with every row's state.
    pub(crate) async fn get_draft(&self, scene_id: &str) -> Result<Draft, ServiceError> {
        let mut conn = self.conn().await?;
        let scene = self.scene_row(&mut conn, scene_id).await?;
        let row = query::get(&mut conn, scene_id)
            .await?
            .ok_or_else(no_draft)?;
        self.draft_view(&mut conn, &scene, row).await
    }

    async fn draft_view(
        &self,
        conn: &mut sqlx::SqliteConnection,
        scene: &Scene,
        row: DraftRow,
    ) -> Result<Draft, ServiceError> {
        let live_nodes: HashMap<String, NodeRow> =
            query_sqlite::list_nodes(conn, Store::Live, &scene.id)
                .await?
                .into_iter()
                .map(|n| (n.id.clone(), n))
                .collect();
        let live_uploads =
            uploads_by_node(query_sqlite::list_scene_uploads(conn, Store::Live, &scene.id).await?);
        let draft_uploads = uploads_by_node(query::all_uploads(conn, &scene.id).await?);
        let live_edges: HashMap<String, EdgeRow> =
            query_sqlite::list_edges(conn, Store::Live, &scene.id)
                .await?
                .into_iter()
                .map(|e| (e.id.clone(), e))
                .collect();

        let mut nodes = vec![];
        for (node_row, meta) in query::all_nodes(conn, &scene.id).await? {
            let files = draft_uploads.get(&node_row.id).cloned().unwrap_or_default();
            let change = row_change(&meta, || node_hash(&node_row, &files));
            let parts = match (change, live_nodes.get(&node_row.id)) {
                (Change::Changed, Some(live)) => node_parts(
                    &node_row,
                    &files,
                    live,
                    live_uploads.get(&live.id).map(Vec::as_slice).unwrap_or(&[]),
                ),
                _ => vec![],
            };
            let mut node: Node = node_row.try_into()?;
            node = self.with_secrets(node).await?;
            node.uploads = files;
            nodes.push(DraftNode {
                node,
                change,
                conflict: Conflict::parse(meta.conflict.as_deref()),
                parts,
            });
        }
        let mut edges = vec![];
        for (edge_row, meta) in query::all_edges(conn, &scene.id).await? {
            let change = row_change(&meta, || edge_hash(&edge_row));
            let parts = match (change, live_edges.get(&edge_row.id)) {
                (Change::Changed, Some(live)) => edge_parts(&edge_row, live),
                _ => vec![],
            };
            edges.push(DraftEdge {
                edge: edge_row.into(),
                change,
                conflict: Conflict::parse(meta.conflict.as_deref()),
                parts,
            });
        }
        let settings_change = if settings_hash(&row.name, row.max_hops, &row.defaults, &row.groups)
            == row.base_hash
        {
            Change::Unchanged
        } else {
            Change::Changed
        };
        let (undo, redo) = history::steps(conn, &row.scene_id).await?;
        let submit = self.draft_submit(&row);
        Ok(Draft {
            submit,
            settings_conflict: Conflict::parse(row.conflict.as_deref()),
            live_revision: scene.revision,
            row,
            settings_change,
            nodes,
            edges,
            undo,
            redo,
        })
    }

    /// Discards the draft (its uploads' files go once nothing names them).
    pub(crate) async fn discard_draft(&self, scene_id: &str) -> Result<(), ServiceError> {
        self.clear_submit(scene_id).await?;
        let _guard = self.lock.lock().await;
        let mut conn = self.conn().await?;
        self.scene_row(&mut conn, scene_id).await?;
        if query::delete(&mut conn, scene_id).await? == 0 {
            return Err(no_draft());
        }
        self.collect_garbage(&mut conn).await?;
        Ok(())
    }

    /// Replaces the draft's scene settings (checked as live).
    pub(crate) async fn update_draft_settings(
        &self,
        scene_id: &str,
        input: &SceneInput,
    ) -> Result<Draft, ServiceError> {
        let (name, max_hops) = check_scene(input)?;
        {
            let _guard = self.lock.lock().await;
            let mut conn = self.conn().await?;
            let scene = self.scene_row(&mut conn, scene_id).await?;
            let draft_groups = self.ensure_draft(&mut conn, &scene).await?.groups;
            let mut tx = conn.begin_with(WRITE_TX).await?;
            let groups = input.groups.as_ref().unwrap_or(&draft_groups);
            query::set_settings(&mut tx, scene_id, &name, max_hops, &input.defaults, groups)
                .await?;
            history::record(&mut tx, scene_id).await?;
            tx.commit().await?;
        }
        self.get_draft(scene_id).await
    }

    /// The draft's check: as [`Service::check`], on the draft's graph and
    /// settings, plus its unresolved conflicts.
    pub(crate) async fn check_draft(&self, scene_id: &str) -> Result<SceneCheck, ServiceError> {
        let draft = self.get_draft(scene_id).await?;
        let mut conn = self.conn().await?;
        let mut check = self
            .check_store(
                &mut conn,
                Store::Draft,
                scene_id,
                &draft.row.defaults,
                &draft.row.groups,
            )
            .await?;
        check.problems.extend(
            draft
                .conflicts()
                .into_iter()
                .map(|c| format!("{c}: resolve it (POST …/draft/resolve)")),
        );
        if draft.behind() {
            check.warnings.push(format!(
                "live has changed since the draft started (revision {} → {}): rebase the draft before submitting (POST …/draft/rebase)",
                draft.row.base_revision, draft.live_revision
            ));
        }
        Ok(check)
    }

    // ---- removing and restoring ----

    /// Removes a node from the draft: a node from live stays as a tombstone
    /// (its edges too), one added in the draft goes.
    pub(crate) async fn remove_draft_node(
        &self,
        scene_id: &str,
        id: &str,
    ) -> Result<(), ServiceError> {
        let _guard = self.lock.lock().await;
        let mut conn = self.conn().await?;
        let scene = self.scene_row(&mut conn, scene_id).await?;
        self.ensure_draft(&mut conn, &scene).await?;
        let meta = query::meta(&mut conn, NODES, scene_id, id)
            .await?
            .filter(|m| !m.removed)
            .ok_or(ServiceError::NotFound("node"))?;
        let mut tx = conn.begin_with(WRITE_TX).await?;
        for (edge, edge_meta) in query::all_edges(&mut tx, scene_id).await? {
            if !edge_meta.removed && (edge.source_node_id == id || edge.target_node_id == id) {
                remove_row(&mut tx, EDGES, edge_meta).await?;
            }
        }
        remove_row(&mut tx, NODES, meta).await?;
        query::touch(&mut tx, scene_id).await?;
        tx.commit().await?;
        self.collect_garbage(&mut conn).await?;
        Ok(())
    }

    /// Removes an edge from the draft (a tombstone if it's from live).
    pub(crate) async fn remove_draft_edge(
        &self,
        scene_id: &str,
        id: &str,
    ) -> Result<(), ServiceError> {
        let _guard = self.lock.lock().await;
        let mut conn = self.conn().await?;
        let scene = self.scene_row(&mut conn, scene_id).await?;
        self.ensure_draft(&mut conn, &scene).await?;
        let meta = query::meta(&mut conn, EDGES, scene_id, id)
            .await?
            .filter(|m| !m.removed)
            .ok_or(ServiceError::NotFound("edge"))?;
        let mut tx = conn.begin_with(WRITE_TX).await?;
        remove_row(&mut tx, EDGES, meta).await?;
        query::touch(&mut tx, scene_id).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Brings back a node removed in the draft (its edges stay removed:
    /// restore them one by one).
    pub(crate) async fn restore_draft_node(
        &self,
        scene_id: &str,
        id: &str,
    ) -> Result<Node, ServiceError> {
        self.restore(NODES, scene_id, id).await?;
        self.get_node_in(Store::Draft, scene_id, id).await
    }

    /// Brings back an edge removed in the draft; both its nodes must be there.
    pub(crate) async fn restore_draft_edge(
        &self,
        scene_id: &str,
        id: &str,
    ) -> Result<Edge, ServiceError> {
        self.restore(EDGES, scene_id, id).await?;
        let mut conn = self.conn().await?;
        Ok(
            query_sqlite::get_edge(&mut conn, Store::Draft, scene_id, id)
                .await
                .map_err(not_found("edge"))?
                .into(),
        )
    }

    async fn restore(&self, table: &str, scene_id: &str, id: &str) -> Result<(), ServiceError> {
        let what = if table == NODES { "node" } else { "edge" };
        let _guard = self.lock.lock().await;
        let mut conn = self.conn().await?;
        self.scene_row(&mut conn, scene_id).await?;
        query::get(&mut conn, scene_id)
            .await?
            .ok_or_else(no_draft)?;
        let mut meta = query::meta(&mut conn, table, scene_id, id)
            .await?
            .ok_or(ServiceError::NotFound(what))?;
        if !meta.removed {
            return Err(ServiceError::Conflict(
                format!("the {what} isn't removed"),
                vec![],
            ));
        }
        if meta.conflict.is_some() {
            return Err(ServiceError::Conflict(
                format!("the {what} has a conflict with live; resolve it instead"),
                vec![],
            ));
        }
        if table == EDGES {
            let edge = query::all_edges(&mut conn, scene_id)
                .await?
                .into_iter()
                .find(|(e, _)| e.id == id)
                .map(|(e, _)| e)
                .ok_or(ServiceError::NotFound("edge"))?;
            let mut errors = vec![];
            for node_id in [&edge.source_node_id, &edge.target_node_id] {
                if query_sqlite::get_node(&mut conn, Store::Draft, scene_id, node_id)
                    .await
                    .is_err()
                {
                    errors.push(format!(
                        "node {node_id} is removed in the draft: restore it first"
                    ));
                }
            }
            if !errors.is_empty() {
                return Err(ServiceError::Conflict(
                    "the edge's nodes aren't all there".into(),
                    errors,
                ));
            }
        }
        meta.removed = false;
        let mut tx = conn.begin_with(WRITE_TX).await?;
        query::set_meta(&mut tx, table, id, &meta)
            .await
            .map_err(|e| {
                if e.as_database_error().map(|d| d.kind()) == Some(ErrorKind::UniqueViolation) {
                    ServiceError::Conflict(
                        "another edge in the draft has the same direction".into(),
                        vec![],
                    )
                } else {
                    e.into()
                }
            })?;
        if table == EDGES {
            // Graph rules (a SINK sends nothing, …) hold for restored edges too.
            check_graph(&mut tx, Store::Draft, scene_id).await?;
        }
        query::touch(&mut tx, scene_id).await?;
        tx.commit().await?;
        Ok(())
    }

    // ---- rebase and conflicts ----

    /// Brings the draft up to the live revision: a three-way merge (base,
    /// draft, live) per node, edge and the settings. What changed on one side
    /// only is merged; what changed on both is marked as a conflict.
    pub(crate) async fn rebase_draft(&self, scene_id: &str) -> Result<RebaseReport, ServiceError> {
        let _guard = self.lock.lock().await;
        let mut conn = self.conn().await?;
        let scene = self.scene_row(&mut conn, scene_id).await?;
        let draft = query::get(&mut conn, scene_id)
            .await?
            .ok_or_else(no_draft)?;
        let mut report = RebaseReport {
            from_revision: draft.base_revision,
            to_revision: scene.revision,
            ..Default::default()
        };
        if draft.base_revision == scene.revision {
            return Ok(report);
        }
        let mut tx = conn.begin_with(WRITE_TX).await?;

        // Settings.
        let live_settings =
            settings_hash(&scene.name, scene.max_hops, &scene.defaults, &scene.groups);
        let draft_settings =
            settings_hash(&draft.name, draft.max_hops, &draft.defaults, &draft.groups);
        if live_settings != draft.base_hash {
            if draft_settings == draft.base_hash {
                query::set_settings(
                    &mut tx,
                    scene_id,
                    &scene.name,
                    scene.max_hops,
                    &scene.defaults,
                    &scene.groups,
                )
                .await?;
                query::set_settings_meta(&mut tx, scene_id, &live_settings, None).await?;
                report.taken_live.push("settings".into());
            } else if draft_settings == live_settings {
                query::set_settings_meta(&mut tx, scene_id, &live_settings, None).await?;
            } else {
                query::set_settings_meta(
                    &mut tx,
                    scene_id,
                    &live_settings,
                    Some(Conflict::ChangedBoth.as_str()),
                )
                .await?;
                report.conflicts.push("settings".into());
            }
        }

        // Nodes.
        let live_uploads = uploads_by_node(
            query_sqlite::list_scene_uploads(&mut tx, Store::Live, scene_id).await?,
        );
        let draft_uploads = uploads_by_node(query::all_uploads(&mut tx, scene_id).await?);
        let mut live_nodes: HashMap<String, NodeRow> =
            query_sqlite::list_nodes(&mut tx, Store::Live, scene_id)
                .await?
                .into_iter()
                .map(|n| (n.id.clone(), n))
                .collect();
        let edges_before: Vec<String> = query::all_edges(&mut tx, scene_id)
            .await?
            .into_iter()
            .filter(|(_, m)| !m.removed)
            .map(|(e, _)| e.id)
            .collect();
        for (node, meta) in query::all_nodes(&mut tx, scene_id).await? {
            let live = live_nodes.remove(&node.id);
            let Some(base) = meta.base_hash.clone() else {
                continue; // added in the draft
            };
            let draft_files = draft_uploads
                .get(&node.id)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let draft_changed = meta.removed || node_hash(&node, draft_files) != base;
            let label = format!("node {}", node.id);
            match live {
                None => {
                    if !draft_changed || meta.removed {
                        query::delete_row(&mut tx, NODES, &node.id).await?;
                        report.taken_live.push(label);
                    } else {
                        let meta = RowMeta {
                            base_hash: None,
                            conflict: Some(Conflict::RemovedLive.as_str().into()),
                            ..meta
                        };
                        query::set_meta(&mut tx, NODES, &node.id, &meta).await?;
                        report.conflicts.push(label);
                    }
                }
                Some(live) => {
                    let live_files = live_uploads.get(&live.id).map(Vec::as_slice).unwrap_or(&[]);
                    let live_hash = node_hash(&live, live_files);
                    if live_hash == base {
                        continue;
                    }
                    if !draft_changed {
                        take_live_node(&mut tx, &live, live_files, &live_hash).await?;
                        report.taken_live.push(label);
                    } else if !meta.removed && node_hash(&node, draft_files) == live_hash {
                        let meta = RowMeta {
                            base_hash: Some(live_hash),
                            ..meta
                        };
                        query::set_meta(&mut tx, NODES, &node.id, &meta).await?;
                    } else {
                        let conflict = if meta.removed {
                            Conflict::ChangedLive
                        } else {
                            Conflict::ChangedBoth
                        };
                        let meta = RowMeta {
                            base_hash: Some(live_hash),
                            conflict: Some(conflict.as_str().into()),
                            ..meta
                        };
                        query::set_meta(&mut tx, NODES, &node.id, &meta).await?;
                        report.conflicts.push(label);
                    }
                }
            }
        }
        // Added live since the base.
        for live in live_nodes.values() {
            let files = live_uploads.get(&live.id).map(Vec::as_slice).unwrap_or(&[]);
            take_live_node(&mut tx, live, files, &node_hash(live, files)).await?;
            report.taken_live.push(format!("node {}", live.id));
        }

        // Edges.
        let mut live_edges: HashMap<String, EdgeRow> =
            query_sqlite::list_edges(&mut tx, Store::Live, scene_id)
                .await?
                .into_iter()
                .map(|e| (e.id.clone(), e))
                .collect();
        let draft_edges = query::all_edges(&mut tx, scene_id).await?;
        let present: std::collections::HashSet<String> = edges_before.into_iter().collect();
        for id in present
            .iter()
            .filter(|id| !draft_edges.iter().any(|(e, _)| &e.id == *id))
        {
            report.dropped_edges.push(format!("edge {id}"));
        }
        for (edge, meta) in draft_edges {
            let live = live_edges.remove(&edge.id);
            let Some(base) = meta.base_hash.clone() else {
                continue;
            };
            let draft_changed = meta.removed || edge_hash(&edge) != base;
            let label = format!("edge {}", edge.id);
            match live {
                None => {
                    if !draft_changed || meta.removed {
                        query::delete_row(&mut tx, EDGES, &edge.id).await?;
                        report.taken_live.push(label);
                    } else {
                        let meta = RowMeta {
                            base_hash: None,
                            conflict: Some(Conflict::RemovedLive.as_str().into()),
                            ..meta
                        };
                        query::set_meta(&mut tx, EDGES, &edge.id, &meta).await?;
                        report.conflicts.push(label);
                    }
                }
                Some(live) => {
                    let live_hash = edge_hash(&live);
                    if live_hash == base {
                        continue;
                    }
                    if !draft_changed {
                        if take_live_edge(&mut tx, &live, &live_hash).await? {
                            report.taken_live.push(label);
                        } else {
                            let meta = RowMeta {
                                base_hash: Some(live_hash),
                                conflict: Some(Conflict::ChangedBoth.as_str().into()),
                                ..meta
                            };
                            query::set_meta(&mut tx, EDGES, &edge.id, &meta).await?;
                            report.conflicts.push(label);
                        }
                    } else if !meta.removed && edge_hash(&edge) == live_hash {
                        let meta = RowMeta {
                            base_hash: Some(live_hash),
                            ..meta
                        };
                        query::set_meta(&mut tx, EDGES, &edge.id, &meta).await?;
                    } else {
                        let conflict = if meta.removed {
                            Conflict::ChangedLive
                        } else {
                            Conflict::ChangedBoth
                        };
                        let meta = RowMeta {
                            base_hash: Some(live_hash),
                            conflict: Some(conflict.as_str().into()),
                            ..meta
                        };
                        query::set_meta(&mut tx, EDGES, &edge.id, &meta).await?;
                        report.conflicts.push(label);
                    }
                }
            }
        }
        for live in live_edges.values() {
            if take_live_edge(&mut tx, live, &edge_hash(live)).await? {
                report.taken_live.push(format!("edge {}", live.id));
            }
        }
        // An edge whose node is removed (on either side) goes with it.
        let nodes_there: std::collections::HashSet<String> =
            query_sqlite::list_nodes(&mut tx, Store::Draft, scene_id)
                .await?
                .into_iter()
                .map(|n| n.id)
                .collect();
        for (edge, meta) in query::all_edges(&mut tx, scene_id).await? {
            if !meta.removed
                && (!nodes_there.contains(&edge.source_node_id)
                    || !nodes_there.contains(&edge.target_node_id))
            {
                remove_row(&mut tx, EDGES, meta).await?;
                report.dropped_edges.push(format!("edge {}", edge.id));
            }
        }
        report.dropped_edges.sort();
        report.dropped_edges.dedup();

        query::set_base_revision(&mut tx, scene_id, scene.revision).await?;
        query::touch(&mut tx, scene_id).await?;
        tx.commit().await?;
        self.collect_garbage(&mut conn).await?;
        log::info!(
            "Scene service: rebased the draft of scene [{scene_id}] from revision {} to {} ({} conflict(s))",
            report.from_revision,
            report.to_revision,
            report.conflicts.len()
        );
        Ok(report)
    }

    /// Resolves a conflict a rebase found: keep the draft's version, or take
    /// live's. Live must not have moved on since the rebase.
    pub(crate) async fn resolve_conflict(
        &self,
        scene_id: &str,
        target: Target,
        keep: Keep,
    ) -> Result<Draft, ServiceError> {
        {
            let _guard = self.lock.lock().await;
            let mut conn = self.conn().await?;
            let scene = self.scene_row(&mut conn, scene_id).await?;
            let draft = query::get(&mut conn, scene_id)
                .await?
                .ok_or_else(no_draft)?;
            if draft.base_revision != scene.revision {
                return Err(ServiceError::Conflict(
                    "live has changed again since the rebase: rebase the draft first".into(),
                    vec![],
                ));
            }
            let mut tx = conn.begin_with(WRITE_TX).await?;
            match &target {
                Target::Settings => {
                    if draft.conflict.is_none() {
                        return Err(ServiceError::Conflict(
                            "the settings have no conflict".into(),
                            vec![],
                        ));
                    }
                    if keep == Keep::Live {
                        query::set_settings(
                            &mut tx,
                            scene_id,
                            &scene.name,
                            scene.max_hops,
                            &scene.defaults,
                            &scene.groups,
                        )
                        .await?;
                    }
                    query::set_settings_meta(&mut tx, scene_id, &draft.base_hash, None).await?;
                }
                Target::Node(id) | Target::Edge(id) => {
                    let (table, what) = match &target {
                        Target::Node(_) => (NODES, "node"),
                        _ => (EDGES, "edge"),
                    };
                    let meta = query::meta(&mut tx, table, scene_id, id)
                        .await?
                        .ok_or(ServiceError::NotFound(what))?;
                    if meta.conflict.is_none() {
                        return Err(ServiceError::Conflict(
                            format!("the {what} has no conflict"),
                            vec![],
                        ));
                    }
                    match keep {
                        Keep::Draft => {
                            let meta = RowMeta {
                                conflict: None,
                                ..meta
                            };
                            query::set_meta(&mut tx, table, id, &meta).await?;
                        }
                        Keep::Live if table == NODES => {
                            match query_sqlite::get_node(&mut tx, Store::Live, scene_id, id).await {
                                Ok(live) => {
                                    let files =
                                        query_sqlite::list_uploads(&mut tx, Store::Live, id)
                                            .await?;
                                    let hash = node_hash(&live, &files);
                                    take_live_node(&mut tx, &live, &files, &hash).await?;
                                }
                                Err(sqlx::Error::RowNotFound) => {
                                    query::delete_row(&mut tx, NODES, id).await?
                                }
                                Err(e) => return Err(e.into()),
                            }
                        }
                        Keep::Live => {
                            match query_sqlite::get_edge(&mut tx, Store::Live, scene_id, id).await {
                                Ok(live) => {
                                    if !take_live_edge(&mut tx, &live, &edge_hash(&live)).await? {
                                        return Err(ServiceError::Conflict(
                                        "live's edge clashes with the draft: its nodes are removed here, or another edge has its direction".into(),
                                        vec![],
                                    ));
                                    }
                                }
                                Err(sqlx::Error::RowNotFound) => {
                                    query::delete_row(&mut tx, EDGES, id).await?
                                }
                                Err(e) => return Err(e.into()),
                            }
                        }
                    }
                    check_graph(&mut tx, Store::Draft, scene_id).await?;
                }
            }
            query::touch(&mut tx, scene_id).await?;
            tx.commit().await?;
            self.collect_garbage(&mut conn).await?;
        }
        self.get_draft(scene_id).await
    }
}

/// A row's state from its own columns (`hash` computes its content hash).
fn row_change(meta: &RowMeta, hash: impl FnOnce() -> String) -> Change {
    if meta.removed {
        Change::Removed
    } else {
        match &meta.base_hash {
            None => Change::Added,
            Some(base) if *base == hash() => Change::Unchanged,
            Some(_) => Change::Changed,
        }
    }
}

/// Removes a draft row: a tombstone if it's from live, gone if it was added
/// in the draft.
async fn remove_row(
    tx: &mut sqlx::SqliteConnection,
    table: &str,
    meta: RowMeta,
) -> Result<(), ServiceError> {
    if meta.base_hash.is_some() {
        let id = meta.id.clone();
        query::set_meta(
            tx,
            table,
            &id,
            &RowMeta {
                removed: true,
                conflict: None,
                ..meta
            },
        )
        .await?;
    } else {
        query::delete_row(tx, table, &meta.id).await?;
    }
    Ok(())
}

/// Puts live's node (and its uploads) into the draft, as its new base.
async fn take_live_node(
    tx: &mut sqlx::SqliteConnection,
    live: &NodeRow,
    files: &[NodeUpload],
    hash: &str,
) -> Result<(), ServiceError> {
    let meta = RowMeta {
        id: live.id.clone(),
        base_hash: Some(hash.to_string()),
        removed: false,
        conflict: None,
    };
    query::put_node(tx, live, &meta).await?;
    query::replace_uploads(tx, Store::Draft, &live.id, files).await?;
    Ok(())
}

/// Puts live's edge into the draft, as its new base. `false` if it doesn't
/// fit the draft (a node removed here, or another edge in its direction).
async fn take_live_edge(
    tx: &mut sqlx::SqliteConnection,
    live: &EdgeRow,
    hash: &str,
) -> Result<bool, ServiceError> {
    let meta = RowMeta {
        id: live.id.clone(),
        base_hash: Some(hash.to_string()),
        removed: false,
        conflict: None,
    };
    for node_id in [&live.source_node_id, &live.target_node_id] {
        if query_sqlite::get_node(tx, Store::Draft, &live.scene_id, node_id)
            .await
            .is_err()
        {
            return Ok(false);
        }
    }
    match query::put_edge(tx, live, &meta).await {
        Ok(()) => Ok(true),
        Err(e) if e.as_database_error().map(|d| d.kind()) == Some(ErrorKind::UniqueViolation) => {
            Ok(false)
        }
        Err(e) => Err(e.into()),
    }
}
