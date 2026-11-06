//! REST API for a scene's graph: nodes and edges.

use super::api::Svc;
use crate::modules::api_error::{ApiError, ErrorBody};
use crate::modules::scene::service::{self, EdgeInput, NodeInput};
use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::get;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::ToSchema;

#[derive(Serialize, ToSchema)]
pub(crate) struct Node {
    pub id: String,
    pub scene_id: String,
    pub name: Option<String>,
    /// SOURCE, SINK or TRANSFORM (the plugin's type).
    #[serde(rename = "type")]
    pub node_type: String,
    /// The plugin (registry id) this node runs.
    pub plugin_id: String,
    /// The plugin version the node was configured with, if known.
    pub plugin_version: Option<String>,
    /// Node config (`launch`, `runtime`, `launch_args`). The token is never returned.
    #[schema(value_type = Object)]
    pub config: Value,
    /// Whether a pre-shared token is stored (remote nodes).
    pub has_token: bool,
    pub create_time: i64,
    pub update_time: i64,
}

impl From<service::Node> for Node {
    fn from(n: service::Node) -> Self {
        Self {
            has_token: n.config.token.is_some(),
            config: n.config.redacted(),
            id: n.id,
            scene_id: n.scene_id,
            name: n.name,
            node_type: n.node_type,
            plugin_id: n.plugin_id,
            plugin_version: n.plugin_version,
            create_time: n.create_time.timestamp_millis(),
            update_time: n.update_time.timestamp_millis(),
        }
    }
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct NodeRequest {
    pub name: Option<String>,
    /// Registered plugin id.
    pub plugin_id: String,
    /// Optional: taken from the plugin. If given, it must match.
    #[serde(rename = "type")]
    pub node_type: Option<String>,
    /// Node config. On update, omitting `token` keeps the stored one (if the launch mode is unchanged).
    #[schema(value_type = Option<Object>)]
    pub config: Option<Value>,
}

impl From<NodeRequest> for NodeInput {
    fn from(r: NodeRequest) -> Self {
        Self {
            name: r.name,
            plugin_id: r.plugin_id,
            node_type: r.node_type,
            config: r.config.unwrap_or(Value::Null),
        }
    }
}

#[derive(Serialize, ToSchema)]
pub(crate) struct Edge {
    pub id: String,
    pub scene_id: String,
    pub name: Option<String>,
    pub source_node_id: String,
    pub target_node_id: String,
    /// Mapping applied on handover; null = pass through as-is.
    #[schema(value_type = Option<Object>)]
    pub mapping: Option<Value>,
    pub create_time: i64,
    pub update_time: i64,
}

impl From<service::Edge> for Edge {
    fn from(e: service::Edge) -> Self {
        Self {
            id: e.id,
            scene_id: e.scene_id,
            name: e.name,
            source_node_id: e.source_node_id,
            target_node_id: e.target_node_id,
            mapping: e.mapping,
            create_time: e.create_time.timestamp_millis(),
            update_time: e.update_time.timestamp_millis(),
        }
    }
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct EdgeRequest {
    pub name: Option<String>,
    pub source_node_id: String,
    pub target_node_id: String,
    #[schema(value_type = Option<Object>)]
    pub mapping: Option<Value>,
}

impl From<EdgeRequest> for EdgeInput {
    fn from(r: EdgeRequest) -> Self {
        Self {
            name: r.name,
            source_node_id: r.source_node_id,
            target_node_id: r.target_node_id,
            mapping: r.mapping.unwrap_or(Value::Null),
        }
    }
}

#[utoipa::path(
    get,
    path = "/by-id/{id}/nodes",
    operation_id = "list-nodes",
    summary = "List a scene's nodes",
    params(("id" = String, Path, description = "Scene ID")),
    responses(
        (status = 200, description = "Nodes", body = [Node]),
        (status = 404, description = "Scene not found", body = ErrorBody),
    )
)]
pub(crate) async fn list_nodes(
    State(service): State<Svc>,
    Path(id): Path<String>,
) -> Result<Json<Vec<Node>>, ApiError> {
    Ok(Json(
        service
            .list_nodes(&id)
            .await?
            .into_iter()
            .map(Node::from)
            .collect(),
    ))
}

#[utoipa::path(
    post,
    path = "/by-id/{id}/nodes",
    operation_id = "create-node",
    summary = "Add a node (plugin instance) to a scene",
    params(("id" = String, Path, description = "Scene ID")),
    request_body(content = NodeRequest, content_type = "application/json"),
    responses(
        (status = 201, description = "Created node", body = Node),
        (status = 404, description = "Scene not found", body = ErrorBody),
        (status = 422, description = "Invalid node; details list every problem", body = ErrorBody),
    )
)]
pub(crate) async fn create_node(
    State(service): State<Svc>,
    Path(id): Path<String>,
    Json(req): Json<NodeRequest>,
) -> Result<(StatusCode, Json<Node>), ApiError> {
    let node = service.create_node(&id, req.into()).await?;
    Ok((StatusCode::CREATED, Json(node.into())))
}

#[utoipa::path(
    get,
    path = "/by-id/{id}/nodes/by-id/{node_id}",
    operation_id = "get-node",
    summary = "Get a node",
    params(("id" = String, Path, description = "Scene ID"), ("node_id" = String, Path, description = "Node ID")),
    responses(
        (status = 200, description = "Node", body = Node),
        (status = 404, description = "Not found", body = ErrorBody),
    )
)]
pub(crate) async fn get_node(
    State(service): State<Svc>,
    Path((id, node_id)): Path<(String, String)>,
) -> Result<Json<Node>, ApiError> {
    Ok(Json(service.get_node(&id, &node_id).await?.into()))
}

#[utoipa::path(
    put,
    path = "/by-id/{id}/nodes/by-id/{node_id}",
    operation_id = "update-node",
    summary = "Replace a node",
    description = "The node's edges are re-checked (e.g. if its plugin type changes).",
    params(("id" = String, Path, description = "Scene ID"), ("node_id" = String, Path, description = "Node ID")),
    request_body(content = NodeRequest, content_type = "application/json"),
    responses(
        (status = 200, description = "Updated node", body = Node),
        (status = 404, description = "Not found", body = ErrorBody),
        (status = 422, description = "Invalid node; details list every problem", body = ErrorBody),
    )
)]
pub(crate) async fn update_node(
    State(service): State<Svc>,
    Path((id, node_id)): Path<(String, String)>,
    Json(req): Json<NodeRequest>,
) -> Result<Json<Node>, ApiError> {
    Ok(Json(
        service.update_node(&id, &node_id, req.into()).await?.into(),
    ))
}

#[utoipa::path(
    delete,
    path = "/by-id/{id}/nodes/by-id/{node_id}",
    operation_id = "delete-node",
    summary = "Delete a node and its edges",
    params(("id" = String, Path, description = "Scene ID"), ("node_id" = String, Path, description = "Node ID")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 404, description = "Not found", body = ErrorBody),
    )
)]
pub(crate) async fn delete_node(
    State(service): State<Svc>,
    Path((id, node_id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    service.delete_node(&id, &node_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    get,
    path = "/by-id/{id}/edges",
    operation_id = "list-edges",
    summary = "List a scene's edges",
    params(("id" = String, Path, description = "Scene ID")),
    responses(
        (status = 200, description = "Edges", body = [Edge]),
        (status = 404, description = "Scene not found", body = ErrorBody),
    )
)]
pub(crate) async fn list_edges(
    State(service): State<Svc>,
    Path(id): Path<String>,
) -> Result<Json<Vec<Edge>>, ApiError> {
    Ok(Json(
        service
            .list_edges(&id)
            .await?
            .into_iter()
            .map(Edge::from)
            .collect(),
    ))
}

#[utoipa::path(
    post,
    path = "/by-id/{id}/edges",
    operation_id = "create-edge",
    summary = "Add an edge (data handover) between two nodes",
    description = "A SINK can't be a source and a SOURCE can't be a target. Cycles are allowed, but a node on a cycle can't use queue.when_full \"block\".",
    params(("id" = String, Path, description = "Scene ID")),
    request_body(content = EdgeRequest, content_type = "application/json"),
    responses(
        (status = 201, description = "Created edge", body = Edge),
        (status = 404, description = "Scene not found", body = ErrorBody),
        (status = 409, description = "An edge between these nodes already exists", body = ErrorBody),
        (status = 422, description = "Invalid edge; details list every problem", body = ErrorBody),
    )
)]
pub(crate) async fn create_edge(
    State(service): State<Svc>,
    Path(id): Path<String>,
    Json(req): Json<EdgeRequest>,
) -> Result<(StatusCode, Json<Edge>), ApiError> {
    let edge = service.create_edge(&id, req.into()).await?;
    Ok((StatusCode::CREATED, Json(edge.into())))
}

#[utoipa::path(
    get,
    path = "/by-id/{id}/edges/by-id/{edge_id}",
    operation_id = "get-edge",
    summary = "Get an edge",
    params(("id" = String, Path, description = "Scene ID"), ("edge_id" = String, Path, description = "Edge ID")),
    responses(
        (status = 200, description = "Edge", body = Edge),
        (status = 404, description = "Not found", body = ErrorBody),
    )
)]
pub(crate) async fn get_edge(
    State(service): State<Svc>,
    Path((id, edge_id)): Path<(String, String)>,
) -> Result<Json<Edge>, ApiError> {
    Ok(Json(service.get_edge(&id, &edge_id).await?.into()))
}

#[utoipa::path(
    put,
    path = "/by-id/{id}/edges/by-id/{edge_id}",
    operation_id = "update-edge",
    summary = "Replace an edge",
    params(("id" = String, Path, description = "Scene ID"), ("edge_id" = String, Path, description = "Edge ID")),
    request_body(content = EdgeRequest, content_type = "application/json"),
    responses(
        (status = 200, description = "Updated edge", body = Edge),
        (status = 404, description = "Not found", body = ErrorBody),
        (status = 409, description = "An edge between these nodes already exists", body = ErrorBody),
        (status = 422, description = "Invalid edge", body = ErrorBody),
    )
)]
pub(crate) async fn update_edge(
    State(service): State<Svc>,
    Path((id, edge_id)): Path<(String, String)>,
    Json(req): Json<EdgeRequest>,
) -> Result<Json<Edge>, ApiError> {
    Ok(Json(
        service.update_edge(&id, &edge_id, req.into()).await?.into(),
    ))
}

#[utoipa::path(
    delete,
    path = "/by-id/{id}/edges/by-id/{edge_id}",
    operation_id = "delete-edge",
    summary = "Delete an edge",
    params(("id" = String, Path, description = "Scene ID"), ("edge_id" = String, Path, description = "Edge ID")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 404, description = "Not found", body = ErrorBody),
    )
)]
pub(crate) async fn delete_edge(
    State(service): State<Svc>,
    Path((id, edge_id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    service.delete_edge(&id, &edge_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub(super) fn routes() -> Router<Svc> {
    Router::new()
        .route(
            "/scenes/by-id/{id}/nodes",
            get(list_nodes).post(create_node),
        )
        .route(
            "/scenes/by-id/{id}/nodes/by-id/{node_id}",
            get(get_node).put(update_node).delete(delete_node),
        )
        .route(
            "/scenes/by-id/{id}/edges",
            get(list_edges).post(create_edge),
        )
        .route(
            "/scenes/by-id/{id}/edges/by-id/{edge_id}",
            get(get_edge).put(update_edge).delete(delete_edge),
        )
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
        let mut req = Request::builder().method(method).uri(uri);
        if body.is_some() {
            req = req.header(header::CONTENT_TYPE, "application/json");
        }
        let req = req
            .body(
                body.map(|b| Body::from(b.to_string()))
                    .unwrap_or_else(Body::empty),
            )
            .unwrap();
        let response = Api::build_router(service).oneshot(req).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    #[tokio::test]
    async fn graph_over_http() {
        let f = fixture().await;
        let svc = f.service;
        let (status, scene) = call(svc, "POST", "/scenes", Some(json!({"name": "demo"}))).await;
        assert_eq!((status, &scene["enabled"]), (StatusCode::OK, &json!(false)));
        assert_eq!(scene["max_hops"], Value::Null, "server default");
        let sid = scene["id"].as_str().unwrap();
        let (status, updated) = call(
            svc,
            "PUT",
            &format!("/scenes/by-id/{sid}"),
            Some(json!({"name": "demo", "max_hops": 64})),
        )
        .await;
        assert_eq!((status, &updated["max_hops"]), (StatusCode::OK, &json!(64)));
        let (status, err) = call(
            svc,
            "PUT",
            &format!("/scenes/by-id/{sid}"),
            Some(json!({"name": "demo", "max_hops": 0})),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{err}");

        let (status, src) = call(
            svc,
            "POST",
            &format!("/scenes/by-id/{sid}/nodes"),
            Some(json!({"plugin_id": "test-source"})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{src}");
        assert_eq!(src["type"], "SOURCE");
        let (status, sink) = call(
            svc,
            "POST",
            &format!("/scenes/by-id/{sid}/nodes"),
            Some(json!({"plugin_id": "test-sink", "name": "out",
                        "config": {"launch": "remote", "token": "a-long-enough-shared-secret"}})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{sink}");
        assert_eq!(sink["has_token"], true);
        assert!(
            sink["config"].get("token").is_none(),
            "token is redacted: {sink}"
        );

        let (status, edge) = call(
            svc,
            "POST",
            &format!("/scenes/by-id/{sid}/edges"),
            Some(
                json!({"source_node_id": src["id"], "target_node_id": sink["id"],
                        "mapping": {"rules": [{"target": "url", "value": "http://x"}]}}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{edge}");
        let (status, err) = call(
            svc,
            "POST",
            &format!("/scenes/by-id/{sid}/edges"),
            Some(json!({"source_node_id": sink["id"], "target_node_id": src["id"]})),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{err}");
        assert_eq!(
            err["details"].as_array().unwrap().len(),
            2,
            "both direction problems reported: {err}"
        );

        let (_, edges) = call(svc, "GET", &format!("/scenes/by-id/{sid}/edges"), None).await;
        assert_eq!(edges.as_array().unwrap().len(), 1);
        let (status, _) = call(
            svc,
            "GET",
            &format!("/scenes/by-id/{sid}/nodes/by-id/nope"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // test-source has no files, so the scene can't be enabled
        let (status, err) = call(svc, "POST", &format!("/scenes/by-id/{sid}/enable"), None).await;
        assert_eq!(status, StatusCode::CONFLICT, "{err}");
        let src_id = src["id"].as_str().unwrap();
        let (status, _) = call(
            svc,
            "DELETE",
            &format!("/scenes/by-id/{sid}/nodes/by-id/{src_id}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (status, scene) = call(svc, "POST", &format!("/scenes/by-id/{sid}/enable"), None).await;
        assert_eq!((status, &scene["enabled"]), (StatusCode::OK, &json!(true)));
        let (status, scene) =
            call(svc, "POST", &format!("/scenes/by-id/{sid}/disable"), None).await;
        assert_eq!((status, &scene["enabled"]), (StatusCode::OK, &json!(false)));
    }

    #[tokio::test]
    async fn unknown_request_fields_are_rejected() {
        let f = fixture().await;
        let scene = f.scene().await;
        let (status, _) = call(
            f.service,
            "POST",
            &format!("/scenes/by-id/{}/nodes", scene.id),
            Some(json!({"plugin_id": "test-sink", "plugin": "typo"})),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    }
}
