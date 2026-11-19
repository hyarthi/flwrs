//! REST for scene drafts (`…/scenes/by-id/{id}/draft…`): edit a scene's graph
//! apart from the live one, then submit it at once; and the bulk endpoint
//! (`…/graph/batch`) for several direct changes in one step. See
//! `service/draft.rs` and `service/apply.rs`.

use super::api::{Scene, SceneRequest, Svc};
use super::api_graph::{Edge, EdgeRequest, Node, NodeRequest};
use super::api_transfer::SceneCheckResponse;
use super::api_uploads::{Upload, file_response, receive};
use crate::modules::api_error::{ApiError, ErrorBody};
use crate::modules::node_config::SceneDefaults;
use crate::modules::scene::service::apply::{BatchOp, BatchResult};
use crate::modules::scene::service::draft::{self, Change, Conflict, Keep, RebaseReport, Target};
use crate::modules::scene::service::{EdgeInput, NodeInput, Position, SceneInput, Store};
use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{get, post, put};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use utoipa::{OpenApi, ToSchema};

const DRAFT: &str = "/scenes/by-id/{id}/draft";

// ---- responses ----

/// A scene's draft: its settings and graph, with each row's state against
/// the live graph it's based on.
#[derive(Serialize, ToSchema)]
pub(crate) struct DraftResponse {
    pub scene_id: String,
    /// The live revision the draft is based on.
    pub base_revision: i64,
    /// The live revision now.
    pub live_revision: i64,
    /// Live changed since the draft's base: rebase before submitting.
    pub behind: bool,
    /// Whether submitting would change anything.
    pub has_changes: bool,
    pub settings: DraftSettings,
    /// Every node, removed ones included (`change: removed`, a shadow).
    pub nodes: Vec<DraftNode>,
    /// Every edge, removed ones included.
    pub edges: Vec<DraftEdge>,
    /// Unresolved conflicts (from a rebase), described. Submit waits for none.
    pub conflicts: Vec<String>,
    /// What `…/draft/undo` would take back (e.g. "Add Ticker"); `null` = nothing.
    pub undo: Option<String>,
    /// What `…/draft/redo` would make again; `null` = nothing.
    pub redo: Option<String>,
    pub create_time: i64,
    pub update_time: i64,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct DraftSettings {
    pub name: String,
    pub max_hops: Option<u32>,
    pub defaults: SceneDefaults,
    /// `unchanged` or `changed`.
    pub change: Change,
    pub conflict: Option<Conflict>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct DraftNode {
    #[serde(flatten)]
    pub node: Node,
    pub change: Change,
    pub conflict: Option<Conflict>,
    /// For a changed node, what differs from live: `name`, `plugin`,
    /// `config.<key>`, `uploads`.
    pub parts: Vec<String>,
    pub uploads: Vec<Upload>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct DraftEdge {
    #[serde(flatten)]
    pub edge: Edge,
    pub change: Change,
    pub conflict: Option<Conflict>,
    /// For a changed edge: `name`, `source`, `target`, `mapping`.
    pub parts: Vec<String>,
}

impl From<draft::Draft> for DraftResponse {
    fn from(d: draft::Draft) -> Self {
        Self {
            behind: d.behind(),
            has_changes: d.has_changes(),
            conflicts: d.conflicts(),
            undo: d.undo.clone(),
            redo: d.redo.clone(),
            scene_id: d.row.scene_id,
            base_revision: d.row.base_revision,
            live_revision: d.live_revision,
            settings: DraftSettings {
                name: d.row.name,
                max_hops: d.row.max_hops,
                defaults: d.row.defaults,
                change: d.settings_change,
                conflict: d.settings_conflict,
            },
            nodes: d
                .nodes
                .into_iter()
                .map(|n| {
                    let uploads = n
                        .node
                        .uploads
                        .clone()
                        .into_iter()
                        .map(Upload::from)
                        .collect();
                    DraftNode {
                        node: n.node.into(),
                        change: n.change,
                        conflict: n.conflict,
                        parts: n.parts,
                        uploads,
                    }
                })
                .collect(),
            edges: d
                .edges
                .into_iter()
                .map(|e| DraftEdge {
                    edge: e.edge.into(),
                    change: e.change,
                    conflict: e.conflict,
                    parts: e.parts,
                })
                .collect(),
            create_time: d.row.create_time.timestamp_millis(),
            update_time: d.row.update_time.timestamp_millis(),
        }
    }
}

/// What a draft would change, rows that differ only.
#[derive(Serialize, ToSchema)]
pub(crate) struct DiffResponse {
    pub base_revision: i64,
    pub live_revision: i64,
    pub settings: Change,
    pub nodes: Vec<DiffEntry>,
    pub edges: Vec<DiffEntry>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct DiffEntry {
    pub id: String,
    pub change: Change,
    pub conflict: Option<Conflict>,
    pub parts: Vec<String>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct RebaseResponse {
    pub from_revision: i64,
    pub to_revision: i64,
    /// Rows that took live's version (`node <id>`, `edge <id>`, `settings`).
    pub taken_live: Vec<String>,
    /// Rows now in conflict: resolve each (`POST …/draft/resolve`).
    pub conflicts: Vec<String>,
    /// Edges that went with a node removed on the other side.
    pub dropped_edges: Vec<String>,
}

impl From<RebaseReport> for RebaseResponse {
    fn from(r: RebaseReport) -> Self {
        Self {
            from_revision: r.from_revision,
            to_revision: r.to_revision,
            taken_live: r.taken_live,
            conflicts: r.conflicts,
            dropped_edges: r.dropped_edges,
        }
    }
}

// ---- requests ----

#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ResolveWhat {
    Settings,
    Node,
    Edge,
}

#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum KeepSide {
    /// Keep the draft's version.
    Draft,
    /// Take live's version.
    Live,
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ResolveRequest {
    pub target: ResolveWhat,
    /// The node or edge (not for `settings`).
    pub id: Option<String>,
    pub keep: KeepSide,
}

#[derive(Deserialize, ToSchema, Default)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct SubmitRequest {
    /// The base revision the client saw (`GET …/draft`); refused if the draft
    /// has been rebased since.
    pub base_revision: Option<i64>,
    /// Disable the scene in the same step (lets an enabled scene take a draft
    /// with problems).
    pub disable: bool,
}

/// One operation of a batch.
#[derive(Deserialize, ToSchema)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum BatchOpRequest {
    UpdateScene {
        name: String,
        #[serde(default)]
        max_hops: Option<u32>,
        #[serde(default)]
        defaults: SceneDefaults,
    },
    CreateNode {
        /// Names the new node for later operations (`"$<key>"`).
        key: Option<String>,
        name: Option<String>,
        plugin_id: String,
        #[serde(rename = "type")]
        node_type: Option<String>,
        #[schema(value_type = Option<Object>)]
        config: Option<serde_json::Value>,
        /// Where to place the node on the canvas (layout only).
        position: Option<Position>,
    },
    UpdateNode {
        /// A node id, or `"$<key>"`.
        id: String,
        name: Option<String>,
        plugin_id: String,
        #[serde(rename = "type")]
        node_type: Option<String>,
        #[schema(value_type = Option<Object>)]
        config: Option<serde_json::Value>,
        /// Where to place the node on the canvas (layout only).
        position: Option<Position>,
    },
    DeleteNode {
        id: String,
    },
    CreateEdge {
        key: Option<String>,
        name: Option<String>,
        /// A node id, or `"$<key>"`.
        source_node_id: String,
        target_node_id: String,
        #[schema(value_type = Option<Object>)]
        mapping: Option<serde_json::Value>,
    },
    UpdateEdge {
        id: String,
        name: Option<String>,
        source_node_id: String,
        target_node_id: String,
        #[schema(value_type = Option<Object>)]
        mapping: Option<serde_json::Value>,
    },
    DeleteEdge {
        id: String,
    },
}

impl From<BatchOpRequest> for BatchOp {
    fn from(op: BatchOpRequest) -> Self {
        let node = |name,
                    plugin_id,
                    node_type,
                    config: Option<serde_json::Value>,
                    position: Option<Position>| NodeInput {
            name,
            plugin_id,
            node_type,
            config: config.unwrap_or(serde_json::Value::Null),
            position,
        };
        let edge =
            |name, source_node_id, target_node_id, mapping: Option<serde_json::Value>| EdgeInput {
                name,
                source_node_id,
                target_node_id,
                mapping: mapping.unwrap_or(serde_json::Value::Null),
            };
        match op {
            BatchOpRequest::UpdateScene {
                name,
                max_hops,
                defaults,
            } => BatchOp::UpdateScene(SceneInput {
                name,
                max_hops,
                defaults,
            }),
            BatchOpRequest::CreateNode {
                key,
                name,
                plugin_id,
                node_type,
                config,
                position,
            } => BatchOp::CreateNode {
                key,
                input: node(name, plugin_id, node_type, config, position),
            },
            BatchOpRequest::UpdateNode {
                id,
                name,
                plugin_id,
                node_type,
                config,
                position,
            } => BatchOp::UpdateNode {
                id,
                input: node(name, plugin_id, node_type, config, position),
            },
            BatchOpRequest::DeleteNode { id } => BatchOp::DeleteNode { id },
            BatchOpRequest::CreateEdge {
                key,
                name,
                source_node_id,
                target_node_id,
                mapping,
            } => BatchOp::CreateEdge {
                key,
                input: edge(name, source_node_id, target_node_id, mapping),
            },
            BatchOpRequest::UpdateEdge {
                id,
                name,
                source_node_id,
                target_node_id,
                mapping,
            } => BatchOp::UpdateEdge {
                id,
                input: edge(name, source_node_id, target_node_id, mapping),
            },
            BatchOpRequest::DeleteEdge { id } => BatchOp::DeleteEdge { id },
        }
    }
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct BatchRequest {
    /// If given, the scene's live revision must be this one (409 otherwise).
    pub base_revision: Option<i64>,
    pub ops: Vec<BatchOpRequest>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct BatchResponse {
    /// The scene after the batch (its new revision).
    pub scene: Scene,
    /// Per operation, the node or edge it created or changed (`null` for
    /// `update_scene`).
    pub ids: Vec<Option<String>>,
    /// The keys of created nodes and edges → their ids.
    pub keys: BTreeMap<String, String>,
}

impl From<BatchResult> for BatchResponse {
    fn from(r: BatchResult) -> Self {
        Self {
            scene: r.scene.into(),
            ids: r.ids,
            keys: r.keys,
        }
    }
}

// ---- the draft ----

#[utoipa::path(
    get,
    path = "/by-id/{id}/draft",
    operation_id = "get-scene-draft",
    summary = "Get the scene's draft",
    params(("id" = String, Path, description = "Scene ID")),
    responses(
        (status = 200, description = "The draft", body = DraftResponse),
        (status = 404, description = "No such scene, or no draft", body = ErrorBody),
    )
)]
async fn get_draft(
    State(service): State<Svc>,
    Path(id): Path<String>,
) -> Result<Json<DraftResponse>, ApiError> {
    Ok(Json(service.get_draft(&id).await?.into()))
}

#[utoipa::path(
    post,
    path = "/by-id/{id}/draft",
    operation_id = "start-scene-draft",
    summary = "Start a draft from the live graph",
    description = "201 with a new draft, or 200 with the one the scene already has. Any draft edit starts one too.",
    params(("id" = String, Path, description = "Scene ID")),
    responses(
        (status = 201, description = "Started", body = DraftResponse),
        (status = 200, description = "The existing draft", body = DraftResponse),
        (status = 404, description = "No such scene", body = ErrorBody),
    )
)]
async fn start_draft(
    State(service): State<Svc>,
    Path(id): Path<String>,
) -> Result<(StatusCode, Json<DraftResponse>), ApiError> {
    let (draft, created) = service.start_draft(&id).await?;
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(draft.into())))
}

#[utoipa::path(
    delete,
    path = "/by-id/{id}/draft",
    operation_id = "discard-scene-draft",
    summary = "Discard the draft",
    params(("id" = String, Path, description = "Scene ID")),
    responses(
        (status = 204, description = "Discarded"),
        (status = 404, description = "No such scene, or no draft", body = ErrorBody),
    )
)]
async fn discard_draft(
    State(service): State<Svc>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    service.discard_draft(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// What undo or redo did, and the draft after it.
#[derive(Serialize, ToSchema)]
pub(crate) struct StepResponse {
    /// The step taken back (undo) or made again (redo).
    pub step: String,
    pub draft: DraftResponse,
}

#[utoipa::path(
    post,
    path = "/by-id/{id}/draft/undo",
    operation_id = "undo-scene-draft",
    summary = "Undo the draft's last change",
    description = "Puts the draft's state before its last change back (every draft change counts, layout moves too; requests sent with the same `X-Flwrs-Undo-Group` header are one change). The draft's last 100 states are kept. Live isn't touched.",
    params(("id" = String, Path, description = "Scene ID")),
    responses(
        (status = 200, description = "Undone", body = StepResponse),
        (status = 404, description = "No such scene, or no draft", body = ErrorBody),
        (status = 409, description = "Nothing to undo", body = ErrorBody),
    )
)]
async fn undo(
    State(service): State<Svc>,
    Path(id): Path<String>,
) -> Result<Json<StepResponse>, ApiError> {
    let (draft, step) = service.step_draft(&id, true).await?;
    Ok(Json(StepResponse {
        step,
        draft: draft.into(),
    }))
}

#[utoipa::path(
    post,
    path = "/by-id/{id}/draft/redo",
    operation_id = "redo-scene-draft",
    summary = "Redo the change undo took back",
    description = "Until the draft changes otherwise (a change after an undo drops what could be redone).",
    params(("id" = String, Path, description = "Scene ID")),
    responses(
        (status = 200, description = "Redone", body = StepResponse),
        (status = 404, description = "No such scene, or no draft", body = ErrorBody),
        (status = 409, description = "Nothing to redo", body = ErrorBody),
    )
)]
async fn redo(
    State(service): State<Svc>,
    Path(id): Path<String>,
) -> Result<Json<StepResponse>, ApiError> {
    let (draft, step) = service.step_draft(&id, false).await?;
    Ok(Json(StepResponse {
        step,
        draft: draft.into(),
    }))
}

#[utoipa::path(
    put,
    path = "/by-id/{id}/draft/settings",
    operation_id = "update-scene-draft-settings",
    summary = "Replace the draft's scene settings",
    params(("id" = String, Path, description = "Scene ID")),
    request_body = SceneRequest,
    responses(
        (status = 200, description = "The draft", body = DraftResponse),
        (status = 404, description = "No such scene", body = ErrorBody),
        (status = 422, description = "Invalid settings", body = ErrorBody),
    )
)]
async fn update_settings(
    State(service): State<Svc>,
    Path(id): Path<String>,
    Json(request): Json<SceneRequest>,
) -> Result<Json<DraftResponse>, ApiError> {
    Ok(Json(
        service
            .update_draft_settings(&id, &request.into())
            .await?
            .into(),
    ))
}

#[utoipa::path(
    get,
    path = "/by-id/{id}/draft/diff",
    operation_id = "diff-scene-draft",
    summary = "What submitting the draft would change",
    params(("id" = String, Path, description = "Scene ID")),
    responses(
        (status = 200, description = "Changed rows", body = DiffResponse),
        (status = 404, description = "No such scene, or no draft", body = ErrorBody),
    )
)]
async fn diff_draft(
    State(service): State<Svc>,
    Path(id): Path<String>,
) -> Result<Json<DiffResponse>, ApiError> {
    let d = service.get_draft(&id).await?;
    let entry =
        |id: String, change: Change, conflict: Option<Conflict>, parts: Vec<String>| DiffEntry {
            id,
            change,
            conflict,
            parts,
        };
    Ok(Json(DiffResponse {
        base_revision: d.row.base_revision,
        live_revision: d.live_revision,
        settings: d.settings_change,
        nodes: d
            .nodes
            .into_iter()
            .filter(|n| n.change != Change::Unchanged || n.conflict.is_some())
            .map(|n| entry(n.node.id, n.change, n.conflict, n.parts))
            .collect(),
        edges: d
            .edges
            .into_iter()
            .filter(|e| e.change != Change::Unchanged || e.conflict.is_some())
            .map(|e| entry(e.edge.id, e.change, e.conflict, e.parts))
            .collect(),
    }))
}

#[utoipa::path(
    get,
    path = "/by-id/{id}/draft/check",
    operation_id = "check-scene-draft",
    summary = "Whether the draft could run: the scene check on the draft",
    description = "As `…/check`, on the draft's graph and settings, with its unresolved conflicts as problems (and a warning while it's behind live). An enabled scene takes a draft only without problems.",
    params(("id" = String, Path, description = "Scene ID")),
    responses(
        (status = 200, description = "The check", body = SceneCheckResponse),
        (status = 404, description = "No such scene, or no draft", body = ErrorBody),
    )
)]
async fn check_draft(
    State(service): State<Svc>,
    Path(id): Path<String>,
) -> Result<Json<SceneCheckResponse>, ApiError> {
    Ok(Json(service.check_draft(&id).await?.into()))
}

#[utoipa::path(
    post,
    path = "/by-id/{id}/draft/rebase",
    operation_id = "rebase-scene-draft",
    summary = "Bring the draft up to the live revision",
    description = "A three-way merge per node, edge and the settings: what changed on one side only is merged; what changed on both is marked as a conflict (resolve each before submitting). Edges whose node is removed on the other side go with it.",
    params(("id" = String, Path, description = "Scene ID")),
    responses(
        (status = 200, description = "What the rebase did", body = RebaseResponse),
        (status = 404, description = "No such scene, or no draft", body = ErrorBody),
    )
)]
async fn rebase_draft(
    State(service): State<Svc>,
    Path(id): Path<String>,
) -> Result<Json<RebaseResponse>, ApiError> {
    Ok(Json(service.rebase_draft(&id).await?.into()))
}

#[utoipa::path(
    post,
    path = "/by-id/{id}/draft/resolve",
    operation_id = "resolve-scene-draft-conflict",
    summary = "Resolve a conflict: keep the draft's version, or take live's",
    params(("id" = String, Path, description = "Scene ID")),
    request_body = ResolveRequest,
    responses(
        (status = 200, description = "The draft", body = DraftResponse),
        (status = 404, description = "Not found", body = ErrorBody),
        (status = 409, description = "No conflict there, or live moved on (rebase again)", body = ErrorBody),
        (status = 422, description = "Invalid request", body = ErrorBody),
    )
)]
async fn resolve_conflict(
    State(service): State<Svc>,
    Path(id): Path<String>,
    Json(request): Json<ResolveRequest>,
) -> Result<Json<DraftResponse>, ApiError> {
    let target = match (request.target, request.id) {
        (ResolveWhat::Settings, None) => Target::Settings,
        (ResolveWhat::Node, Some(node)) => Target::Node(node),
        (ResolveWhat::Edge, Some(edge)) => Target::Edge(edge),
        (ResolveWhat::Settings, Some(_)) => {
            return Err(crate::modules::api_error::api_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid request",
                vec!["id: not for target settings".into()],
            ));
        }
        (_, None) => {
            return Err(crate::modules::api_error::api_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid request",
                vec!["id: required for a node or an edge".into()],
            ));
        }
    };
    let keep = match request.keep {
        KeepSide::Draft => Keep::Draft,
        KeepSide::Live => Keep::Live,
    };
    Ok(Json(
        service.resolve_conflict(&id, target, keep).await?.into(),
    ))
}

#[utoipa::path(
    post,
    path = "/by-id/{id}/draft/submit",
    operation_id = "submit-scene-draft",
    summary = "Commit the draft to live, at once",
    description = "One transaction: the scene's live graph becomes the draft's, its revision goes up once, the draft goes, and a running scene reloads once (only changed nodes restart). Refused (409) while the draft is behind live (rebase first) or has conflicts, and for an enabled scene while it has problems, unless `disable`.",
    params(("id" = String, Path, description = "Scene ID")),
    request_body = SubmitRequest,
    responses(
        (status = 200, description = "The scene after submitting", body = Scene),
        (status = 404, description = "No such scene, or no draft", body = ErrorBody),
        (status = 409, description = "Behind live, conflicts, or problems (listed)", body = ErrorBody),
        (status = 422, description = "The draft breaks a graph rule", body = ErrorBody),
    )
)]
async fn submit_draft(
    State(service): State<Svc>,
    Path(id): Path<String>,
    body: Option<Json<SubmitRequest>>,
) -> Result<Json<Scene>, ApiError> {
    let request = body.map(|Json(b)| b).unwrap_or_default();
    Ok(Json(
        service
            .submit_draft(&id, request.base_revision, request.disable)
            .await?
            .into(),
    ))
}

// ---- draft nodes and edges ----

#[utoipa::path(
    get,
    path = "/by-id/{id}/draft/nodes",
    operation_id = "list-scene-draft-nodes",
    summary = "List the draft's nodes (removed ones left out)",
    params(("id" = String, Path, description = "Scene ID")),
    responses(
        (status = 200, description = "Nodes", body = [Node]),
        (status = 404, description = "No such scene, or no draft", body = ErrorBody),
    )
)]
async fn list_nodes(
    State(service): State<Svc>,
    Path(id): Path<String>,
) -> Result<Json<Vec<Node>>, ApiError> {
    let draft = service.get_draft(&id).await?;
    Ok(Json(
        draft
            .nodes
            .into_iter()
            .filter(|n| n.change != Change::Removed)
            .map(|n| n.node.into())
            .collect(),
    ))
}

#[utoipa::path(
    post,
    path = "/by-id/{id}/draft/nodes",
    operation_id = "create-scene-draft-node",
    summary = "Add a node to the draft",
    description = "As `…/nodes`, but an incomplete node is stored (its problems are in `…/draft/check`). Starts the draft if there's none.",
    params(("id" = String, Path, description = "Scene ID")),
    request_body = NodeRequest,
    responses(
        (status = 201, description = "Added", body = Node),
        (status = 404, description = "No such scene", body = ErrorBody),
        (status = 422, description = "Invalid node (structure: unknown plugin, unreadable config, wrong type)", body = ErrorBody),
    )
)]
async fn create_node(
    State(service): State<Svc>,
    Path(id): Path<String>,
    Json(request): Json<NodeRequest>,
) -> Result<(StatusCode, Json<Node>), ApiError> {
    let node = service
        .save_node(Store::Draft, &id, None, request.into())
        .await?;
    Ok((StatusCode::CREATED, Json(node.into())))
}

#[utoipa::path(
    get,
    path = "/by-id/{id}/draft/nodes/by-id/{node_id}",
    operation_id = "get-scene-draft-node",
    summary = "Get a draft node",
    params(
        ("id" = String, Path, description = "Scene ID"),
        ("node_id" = String, Path, description = "Node ID"),
    ),
    responses(
        (status = 200, description = "Node", body = Node),
        (status = 404, description = "Not found (or removed)", body = ErrorBody),
    )
)]
async fn get_node(
    State(service): State<Svc>,
    Path((id, node_id)): Path<(String, String)>,
) -> Result<Json<Node>, ApiError> {
    Ok(Json(
        service
            .get_node_in(Store::Draft, &id, &node_id)
            .await?
            .into(),
    ))
}

#[utoipa::path(
    put,
    path = "/by-id/{id}/draft/nodes/by-id/{node_id}",
    operation_id = "update-scene-draft-node",
    summary = "Replace a draft node",
    params(
        ("id" = String, Path, description = "Scene ID"),
        ("node_id" = String, Path, description = "Node ID"),
    ),
    request_body = NodeRequest,
    responses(
        (status = 200, description = "Replaced", body = Node),
        (status = 404, description = "Not found (or removed)", body = ErrorBody),
        (status = 422, description = "Invalid node", body = ErrorBody),
    )
)]
async fn update_node(
    State(service): State<Svc>,
    Path((id, node_id)): Path<(String, String)>,
    Json(request): Json<NodeRequest>,
) -> Result<Json<Node>, ApiError> {
    Ok(Json(
        service
            .save_node(Store::Draft, &id, Some(&node_id), request.into())
            .await?
            .into(),
    ))
}

#[utoipa::path(
    delete,
    path = "/by-id/{id}/draft/nodes/by-id/{node_id}",
    operation_id = "remove-scene-draft-node",
    summary = "Remove a node from the draft (and its edges)",
    description = "A node from live stays as a removed shadow (restore it with `…/restore`); one added in the draft goes.",
    params(
        ("id" = String, Path, description = "Scene ID"),
        ("node_id" = String, Path, description = "Node ID"),
    ),
    responses(
        (status = 204, description = "Removed"),
        (status = 404, description = "Not found", body = ErrorBody),
    )
)]
async fn delete_node(
    State(service): State<Svc>,
    Path((id, node_id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    service.remove_draft_node(&id, &node_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    post,
    path = "/by-id/{id}/draft/nodes/by-id/{node_id}/configure",
    operation_id = "configure-scene-draft-node",
    summary = "Run a draft node's configurator again",
    description = "As `…/nodes/by-id/{node_id}/configure`, on the draft's node (a draft is started if there's none); live gets the result when the draft is submitted.",
    params(("id" = String, Path, description = "Scene ID"), ("node_id" = String, Path, description = "Node ID")),
    responses(
        (status = 200, description = "The node, configured", body = Node),
        (status = 404, description = "Not found", body = ErrorBody),
        (status = 422, description = "No configurator, a remote node without an endpoint, or the configurator failed", body = ErrorBody),
    )
)]
async fn configure_node(
    State(service): State<Svc>,
    Path((id, node_id)): Path<(String, String)>,
) -> Result<Json<Node>, ApiError> {
    Ok(Json(
        service
            .reconfigure_node_in(Store::Draft, &id, &node_id)
            .await?
            .into(),
    ))
}

#[utoipa::path(
    post,
    path = "/by-id/{id}/draft/nodes/by-id/{node_id}/restore",
    operation_id = "restore-scene-draft-node",
    summary = "Bring back a node removed in the draft",
    description = "Its edges stay removed: restore them one by one.",
    params(
        ("id" = String, Path, description = "Scene ID"),
        ("node_id" = String, Path, description = "Node ID"),
    ),
    responses(
        (status = 200, description = "Restored", body = Node),
        (status = 404, description = "Not found", body = ErrorBody),
        (status = 409, description = "Not removed, or in conflict", body = ErrorBody),
    )
)]
async fn restore_node(
    State(service): State<Svc>,
    Path((id, node_id)): Path<(String, String)>,
) -> Result<Json<Node>, ApiError> {
    Ok(Json(
        service.restore_draft_node(&id, &node_id).await?.into(),
    ))
}

#[utoipa::path(
    get,
    path = "/by-id/{id}/draft/edges",
    operation_id = "list-scene-draft-edges",
    summary = "List the draft's edges (removed ones left out)",
    params(("id" = String, Path, description = "Scene ID")),
    responses(
        (status = 200, description = "Edges", body = [Edge]),
        (status = 404, description = "No such scene, or no draft", body = ErrorBody),
    )
)]
async fn list_edges(
    State(service): State<Svc>,
    Path(id): Path<String>,
) -> Result<Json<Vec<Edge>>, ApiError> {
    let draft = service.get_draft(&id).await?;
    Ok(Json(
        draft
            .edges
            .into_iter()
            .filter(|e| e.change != Change::Removed)
            .map(|e| e.edge.into())
            .collect(),
    ))
}

#[utoipa::path(
    post,
    path = "/by-id/{id}/draft/edges",
    operation_id = "create-scene-draft-edge",
    summary = "Add an edge to the draft",
    description = "Checked as `…/edges` (directions, the mapping against the nodes' schemas). Starts the draft if there's none.",
    params(("id" = String, Path, description = "Scene ID")),
    request_body = EdgeRequest,
    responses(
        (status = 201, description = "Added", body = Edge),
        (status = 404, description = "No such scene", body = ErrorBody),
        (status = 409, description = "An edge in that direction exists", body = ErrorBody),
        (status = 422, description = "Invalid edge", body = ErrorBody),
    )
)]
async fn create_edge(
    State(service): State<Svc>,
    Path(id): Path<String>,
    Json(request): Json<EdgeRequest>,
) -> Result<(StatusCode, Json<Edge>), ApiError> {
    let edge = service
        .write_edge(Store::Draft, &id, None, request.into())
        .await?;
    Ok((StatusCode::CREATED, Json(edge.into())))
}

#[utoipa::path(
    get,
    path = "/by-id/{id}/draft/edges/by-id/{edge_id}",
    operation_id = "get-scene-draft-edge",
    summary = "Get a draft edge",
    params(
        ("id" = String, Path, description = "Scene ID"),
        ("edge_id" = String, Path, description = "Edge ID"),
    ),
    responses(
        (status = 200, description = "Edge", body = Edge),
        (status = 404, description = "Not found (or removed)", body = ErrorBody),
    )
)]
async fn get_edge(
    State(service): State<Svc>,
    Path((id, edge_id)): Path<(String, String)>,
) -> Result<Json<Edge>, ApiError> {
    Ok(Json(
        service
            .get_edge_in(Store::Draft, &id, &edge_id)
            .await?
            .into(),
    ))
}

#[utoipa::path(
    put,
    path = "/by-id/{id}/draft/edges/by-id/{edge_id}",
    operation_id = "update-scene-draft-edge",
    summary = "Replace a draft edge",
    params(
        ("id" = String, Path, description = "Scene ID"),
        ("edge_id" = String, Path, description = "Edge ID"),
    ),
    request_body = EdgeRequest,
    responses(
        (status = 200, description = "Replaced", body = Edge),
        (status = 404, description = "Not found (or removed)", body = ErrorBody),
        (status = 409, description = "An edge in that direction exists", body = ErrorBody),
        (status = 422, description = "Invalid edge", body = ErrorBody),
    )
)]
async fn update_edge(
    State(service): State<Svc>,
    Path((id, edge_id)): Path<(String, String)>,
    Json(request): Json<EdgeRequest>,
) -> Result<Json<Edge>, ApiError> {
    Ok(Json(
        service
            .write_edge(Store::Draft, &id, Some(&edge_id), request.into())
            .await?
            .into(),
    ))
}

#[utoipa::path(
    delete,
    path = "/by-id/{id}/draft/edges/by-id/{edge_id}",
    operation_id = "remove-scene-draft-edge",
    summary = "Remove an edge from the draft",
    params(
        ("id" = String, Path, description = "Scene ID"),
        ("edge_id" = String, Path, description = "Edge ID"),
    ),
    responses(
        (status = 204, description = "Removed"),
        (status = 404, description = "Not found", body = ErrorBody),
    )
)]
async fn delete_edge(
    State(service): State<Svc>,
    Path((id, edge_id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    service.remove_draft_edge(&id, &edge_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    post,
    path = "/by-id/{id}/draft/edges/by-id/{edge_id}/restore",
    operation_id = "restore-scene-draft-edge",
    summary = "Bring back an edge removed in the draft",
    params(
        ("id" = String, Path, description = "Scene ID"),
        ("edge_id" = String, Path, description = "Edge ID"),
    ),
    responses(
        (status = 200, description = "Restored", body = Edge),
        (status = 404, description = "Not found", body = ErrorBody),
        (status = 409, description = "Not removed, in conflict, its nodes removed, or its direction taken", body = ErrorBody),
    )
)]
async fn restore_edge(
    State(service): State<Svc>,
    Path((id, edge_id)): Path<(String, String)>,
) -> Result<Json<Edge>, ApiError> {
    Ok(Json(
        service.restore_draft_edge(&id, &edge_id).await?.into(),
    ))
}

// ---- draft uploads ----

#[utoipa::path(
    put,
    path = "/by-id/{id}/draft/nodes/by-id/{node_id}/uploads/{name}",
    operation_id = "put-scene-draft-node-upload",
    summary = "Upload a file to a draft node (or replace it)",
    description = "As `…/nodes/by-id/{node_id}/uploads/{name}`; live gets it when the draft is submitted.",
    params(
        ("id" = String, Path, description = "Scene ID"),
        ("node_id" = String, Path, description = "Node ID"),
        ("name" = String, Path, description = "File name"),
    ),
    request_body(content = Vec<u8>, content_type = "application/octet-stream"),
    responses(
        (status = 201, description = "Uploaded", body = Upload),
        (status = 200, description = "Replaced", body = Upload),
        (status = 404, description = "Not found", body = ErrorBody),
        (status = 413, description = "Too large", body = ErrorBody),
        (status = 422, description = "Invalid name", body = ErrorBody),
    )
)]
async fn put_upload(
    State(service): State<Svc>,
    Path((id, node_id, name)): Path<(String, String, String)>,
    body: Body,
) -> Result<(StatusCode, Json<Upload>), ApiError> {
    let path = receive(service, body).await?;
    let (upload, created) = service
        .put_upload_in(Store::Draft, &id, &node_id, &name, &path)
        .await?;
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(upload.into())))
}

#[utoipa::path(
    get,
    path = "/by-id/{id}/draft/nodes/by-id/{node_id}/uploads",
    operation_id = "list-scene-draft-node-uploads",
    summary = "List a draft node's files",
    params(
        ("id" = String, Path, description = "Scene ID"),
        ("node_id" = String, Path, description = "Node ID"),
    ),
    responses(
        (status = 200, description = "Uploads", body = [Upload]),
        (status = 404, description = "Not found", body = ErrorBody),
    )
)]
async fn list_uploads(
    State(service): State<Svc>,
    Path((id, node_id)): Path<(String, String)>,
) -> Result<Json<Vec<Upload>>, ApiError> {
    let uploads = service.list_uploads_in(Store::Draft, &id, &node_id).await?;
    Ok(Json(uploads.into_iter().map(Upload::from).collect()))
}

#[utoipa::path(
    get,
    path = "/by-id/{id}/draft/nodes/by-id/{node_id}/uploads/{name}",
    operation_id = "get-scene-draft-node-upload",
    summary = "Get a draft node's file details",
    params(
        ("id" = String, Path, description = "Scene ID"),
        ("node_id" = String, Path, description = "Node ID"),
        ("name" = String, Path, description = "File name"),
    ),
    responses(
        (status = 200, description = "Upload", body = Upload),
        (status = 404, description = "Not found", body = ErrorBody),
    )
)]
async fn get_upload(
    State(service): State<Svc>,
    Path((id, node_id, name)): Path<(String, String, String)>,
) -> Result<Json<Upload>, ApiError> {
    Ok(Json(
        service
            .get_upload_in(Store::Draft, &id, &node_id, &name)
            .await?
            .into(),
    ))
}

#[utoipa::path(
    get,
    path = "/by-id/{id}/draft/nodes/by-id/{node_id}/uploads/{name}/content",
    operation_id = "get-scene-draft-node-upload-content",
    summary = "Download a draft node's file",
    description = "Refused for a file a secret launch arg names.",
    params(
        ("id" = String, Path, description = "Scene ID"),
        ("node_id" = String, Path, description = "Node ID"),
        ("name" = String, Path, description = "File name"),
    ),
    responses(
        (status = 200, description = "The file", content_type = "application/octet-stream", body = Vec<u8>),
        (status = 403, description = "A secret", body = ErrorBody),
        (status = 404, description = "Not found", body = ErrorBody),
    )
)]
async fn get_upload_content(
    State(service): State<Svc>,
    Path((id, node_id, name)): Path<(String, String, String)>,
) -> Result<Response, ApiError> {
    let (upload, path) = service
        .upload_content_in(Store::Draft, &id, &node_id, &name)
        .await?;
    file_response(upload, &path).await
}

#[utoipa::path(
    delete,
    path = "/by-id/{id}/draft/nodes/by-id/{node_id}/uploads/{name}",
    operation_id = "delete-scene-draft-node-upload",
    summary = "Delete a draft node's file",
    params(
        ("id" = String, Path, description = "Scene ID"),
        ("node_id" = String, Path, description = "Node ID"),
        ("name" = String, Path, description = "File name"),
    ),
    responses(
        (status = 204, description = "Deleted"),
        (status = 404, description = "Not found", body = ErrorBody),
        (status = 409, description = "The node's launch args still name it", body = ErrorBody),
    )
)]
async fn delete_upload(
    State(service): State<Svc>,
    Path((id, node_id, name)): Path<(String, String, String)>,
) -> Result<StatusCode, ApiError> {
    service
        .delete_upload_in(Store::Draft, &id, &node_id, &name)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---- the bulk endpoint ----

#[utoipa::path(
    post,
    path = "/by-id/{id}/graph/batch",
    operation_id = "batch-scene-graph",
    summary = "Change the live graph in one step: several operations, all or nothing",
    description = "Operations (`op`: `update_scene`, `create_node`, `update_node`, `delete_node`, `create_edge`, `update_edge`, `delete_edge`) are checked in order, each as its own endpoint checks it, later ones seeing earlier ones. Created nodes and edges can be given a `key`, and later operations name them `\"$<key>\"`. Then everything is written in one transaction: one revision, one reload. With `base_revision`, the scene's live revision must match.",
    params(("id" = String, Path, description = "Scene ID")),
    request_body = BatchRequest,
    responses(
        (status = 200, description = "Applied", body = BatchResponse),
        (status = 404, description = "No such scene", body = ErrorBody),
        (status = 409, description = "Another revision, or an edge direction taken", body = ErrorBody),
        (status = 422, description = "An operation is invalid (details name it: `ops[<i>] (<op>): …`)", body = ErrorBody),
    )
)]
async fn batch(
    State(service): State<Svc>,
    Path(id): Path<String>,
    Json(request): Json<BatchRequest>,
) -> Result<Json<BatchResponse>, ApiError> {
    let ops = request.ops.into_iter().map(BatchOp::from).collect();
    Ok(Json(
        service.batch(&id, request.base_revision, ops).await?.into(),
    ))
}

#[derive(OpenApi)]
#[openapi(
    paths(
        get_draft,
        start_draft,
        discard_draft,
        undo,
        redo,
        update_settings,
        diff_draft,
        check_draft,
        rebase_draft,
        resolve_conflict,
        submit_draft,
        list_nodes,
        create_node,
        get_node,
        update_node,
        delete_node,
        restore_node,
        configure_node,
        list_edges,
        create_edge,
        get_edge,
        update_edge,
        delete_edge,
        restore_edge,
        put_upload,
        list_uploads,
        get_upload,
        get_upload_content,
        delete_upload,
        batch,
    ),
    components(schemas(
        DraftResponse,
        StepResponse,
        DraftSettings,
        DraftNode,
        DraftEdge,
        DiffResponse,
        DiffEntry,
        RebaseResponse,
        ResolveRequest,
        ResolveWhat,
        KeepSide,
        SubmitRequest,
        BatchRequest,
        BatchOpRequest,
        BatchResponse,
        Change,
        Conflict,
        ErrorBody,
    ))
)]
pub(crate) struct Api;

pub(super) fn routes() -> Router<Svc> {
    let node = format!("{DRAFT}/nodes/by-id/{{node_id}}");
    let edge = format!("{DRAFT}/edges/by-id/{{edge_id}}");
    Router::new()
        .route(
            DRAFT,
            get(get_draft).post(start_draft).delete(discard_draft),
        )
        .route(&format!("{DRAFT}/settings"), put(update_settings))
        .route(&format!("{DRAFT}/diff"), get(diff_draft))
        .route(&format!("{DRAFT}/undo"), post(undo))
        .route(&format!("{DRAFT}/redo"), post(redo))
        .route(&format!("{DRAFT}/check"), get(check_draft))
        .route(&format!("{DRAFT}/rebase"), post(rebase_draft))
        .route(&format!("{DRAFT}/resolve"), post(resolve_conflict))
        .route(&format!("{DRAFT}/submit"), post(submit_draft))
        .route(&format!("{DRAFT}/nodes"), get(list_nodes).post(create_node))
        .route(&node, get(get_node).put(update_node).delete(delete_node))
        .route(&format!("{node}/restore"), post(restore_node))
        .route(&format!("{node}/configure"), post(configure_node))
        .route(&format!("{node}/uploads"), get(list_uploads))
        .route(
            &format!("{node}/uploads/{{name}}"),
            get(get_upload)
                .put(put_upload)
                .delete(delete_upload)
                // The service enforces its own limit while streaming.
                .layer(DefaultBodyLimit::disable()),
        )
        .route(
            &format!("{node}/uploads/{{name}}/content"),
            get(get_upload_content),
        )
        .route(&format!("{DRAFT}/edges"), get(list_edges).post(create_edge))
        .route(&edge, get(get_edge).put(update_edge).delete(delete_edge))
        .route(&format!("{edge}/restore"), post(restore_edge))
        .route("/scenes/by-id/{id}/graph/batch", post(batch))
}

#[cfg(test)]
mod tests {
    use crate::modules::scene::api::Api;
    use crate::modules::scene::service::tests::fixture;
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    use tower::ServiceExt;

    async fn call(
        service: super::Svc,
        method: &str,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut request = Request::builder().method(method).uri(uri);
        let body = match body {
            Some(json) => {
                request = request.header(header::CONTENT_TYPE, "application/json");
                Body::from(json.to_string())
            }
            None => Body::empty(),
        };
        let response = Api::build_router(service)
            .oneshot(request.body(body).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    #[tokio::test]
    async fn drafts_over_http() {
        let f = fixture().await;
        let svc = f.service;
        let s = f.scene().await;
        let a = f.node(&s.id, "test-source", Value::Null).await;
        let base = format!("/scenes/by-id/{}", s.id);

        let (status, _) = call(svc, "GET", &format!("{base}/draft"), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, draft) = call(svc, "POST", &format!("{base}/draft"), None).await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(draft["base_revision"], 1);
        assert_eq!(draft["nodes"][0]["id"], a.id.as_str());
        assert_eq!(draft["nodes"][0]["type"], "SOURCE");
        assert_eq!(draft["nodes"][0]["change"], "unchanged");
        assert_eq!(draft["settings"]["change"], "unchanged");
        let (status, _) = call(svc, "POST", &format!("{base}/draft"), None).await;
        assert_eq!(status, StatusCode::OK);

        // An incomplete node, then an edge to it.
        let (status, node) = call(
            svc,
            "POST",
            &format!("{base}/draft/nodes"),
            Some(json!({"plugin_id": "test-sink", "config": {"launch_args": {"timeout": 0}}})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{node}");
        let sink = node["id"].as_str().unwrap().to_string();
        let (status, _) = call(
            svc,
            "POST",
            &format!("{base}/draft/edges"),
            Some(json!({"source_node_id": a.id, "target_node_id": sink})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let (_, check) = call(svc, "GET", &format!("{base}/draft/check"), None).await;
        assert_eq!(check["launchable"], false);
        let (_, diff) = call(svc, "GET", &format!("{base}/draft/diff"), None).await;
        assert_eq!(
            diff["nodes"],
            json!([{"id": sink, "change": "added", "conflict": null, "parts": []}])
        );
        assert_eq!(diff["edges"][0]["change"], "added");

        // Fix it, and submit.
        let (status, _) = call(
            svc,
            "PUT",
            &format!("{base}/draft/nodes/by-id/{sink}"),
            Some(json!({"plugin_id": "test-sink", "config": {"launch_args": {"timeout": 3}}})),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, scene) = call(
            svc,
            "POST",
            &format!("{base}/draft/submit"),
            Some(json!({"base_revision": 1})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{scene}");
        assert_eq!(scene["revision"], 2);
        let (_, nodes) = call(svc, "GET", &format!("{base}/nodes"), None).await;
        assert_eq!(nodes.as_array().unwrap().len(), 2);

        // A submit without a draft.
        let (status, _) = call(svc, "POST", &format!("{base}/draft/submit"), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        // Resolve needs an id for a node.
        call(svc, "POST", &format!("{base}/draft"), None).await;
        let (status, body) = call(
            svc,
            "POST",
            &format!("{base}/draft/resolve"),
            Some(json!({"target": "node", "keep": "live"})),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        let (status, _) = call(svc, "DELETE", &format!("{base}/draft"), None).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn batches_over_http() {
        let f = fixture().await;
        let svc = f.service;
        let s = f.scene().await;
        let base = format!("/scenes/by-id/{}", s.id);
        let (status, body) = call(
            svc,
            "POST",
            &format!("{base}/graph/batch"),
            Some(json!({"base_revision": 0, "ops": [
                {"op": "create_node", "key": "src", "plugin_id": "test-source"},
                {"op": "create_node", "key": "out", "plugin_id": "test-sink", "config": {"launch_args": {"timeout": 2}}},
                {"op": "create_edge", "source_node_id": "$src", "target_node_id": "$out"},
                {"op": "update_scene", "name": "wired"},
            ]})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["scene"]["revision"], 1);
        assert_eq!(body["scene"]["name"], "wired");
        assert_eq!(body["ids"].as_array().unwrap().len(), 4);
        assert!(body["keys"]["src"].is_string());

        let (status, body) = call(
            svc,
            "POST",
            &format!("{base}/graph/batch"),
            Some(json!({"ops": [{"op": "create_node", "plugin_id": "nope"}]})),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(
            body["details"][0]
                .as_str()
                .unwrap()
                .starts_with("ops[0] (create_node): "),
            "{body}"
        );
        let (status, _) = call(
            svc,
            "POST",
            &format!("{base}/graph/batch"),
            Some(json!({"ops": [{"op": "explode"}]})),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    }
}
