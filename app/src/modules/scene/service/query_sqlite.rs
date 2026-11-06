use crate::modules::scene::service::{EdgeRow, ListFilters, NodeRow, Scene};
use chrono::Local;
use sqlx::{Sqlite, SqliteConnection};

// --- scenes ---

pub(super) async fn get_scene(conn: &mut SqliteConnection, id: &str) -> Result<Scene, sqlx::Error> {
    sqlx::query_as::<Sqlite, Scene>("SELECT * FROM scenes WHERE id = $1")
        .bind(id)
        .fetch_one(conn)
        .await
}

pub(super) async fn create_scene(
    conn: &mut SqliteConnection,
    id: &str,
    name: &str,
    max_hops: Option<u32>,
) -> Result<Scene, sqlx::Error> {
    sqlx::query_as::<Sqlite, Scene>(
        "INSERT INTO scenes (id, name, max_hops) VALUES ($1, $2, $3) RETURNING *",
    )
    .bind(id)
    .bind(name)
    .bind(max_hops)
    .fetch_one(conn)
    .await
}

pub(super) async fn update_scene(
    conn: &mut SqliteConnection,
    id: &str,
    name: &str,
    max_hops: Option<u32>,
) -> Result<Scene, sqlx::Error> {
    sqlx::query_as::<Sqlite, Scene>(
        "UPDATE scenes SET name = $1, max_hops = $2, update_time = $3 WHERE id = $4 RETURNING *",
    )
    .bind(name)
    .bind(max_hops)
    .bind(Local::now())
    .bind(id)
    .fetch_one(conn)
    .await
}

pub(super) async fn set_scene_enabled(
    conn: &mut SqliteConnection,
    id: &str,
    enabled: bool,
) -> Result<Scene, sqlx::Error> {
    sqlx::query_as::<Sqlite, Scene>(
        "UPDATE scenes SET enabled = $1, update_time = $2 WHERE id = $3 RETURNING *",
    )
    .bind(enabled)
    .bind(Local::now())
    .bind(id)
    .fetch_one(conn)
    .await
}

pub(super) async fn delete_scene(
    conn: &mut SqliteConnection,
    id: &str,
) -> Result<u64, sqlx::Error> {
    let result = sqlx::query("DELETE FROM scenes WHERE id = $1")
        .bind(id)
        .execute(conn)
        .await?;
    Ok(result.rows_affected())
}

pub(super) async fn list_scenes(
    conn: &mut SqliteConnection,
    filters: &ListFilters,
) -> Result<Vec<Scene>, sqlx::Error> {
    sqlx::query_as::<Sqlite, Scene>(
        "SELECT * FROM scenes ORDER BY create_time DESC LIMIT $1 OFFSET $2",
    )
    .bind(filters.limit)
    .bind(filters.offset)
    .fetch_all(conn)
    .await
}

pub(super) async fn enabled_scene_ids(
    conn: &mut SqliteConnection,
) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar::<Sqlite, String>(
        "SELECT id FROM scenes WHERE enabled = TRUE ORDER BY create_time, id",
    )
    .fetch_all(conn)
    .await
}

// --- nodes ---

pub(super) async fn list_nodes(
    conn: &mut SqliteConnection,
    scene_id: &str,
) -> Result<Vec<NodeRow>, sqlx::Error> {
    sqlx::query_as::<Sqlite, NodeRow>(
        "SELECT * FROM scene_nodes WHERE scene_id = $1 ORDER BY create_time, id",
    )
    .bind(scene_id)
    .fetch_all(conn)
    .await
}

pub(super) async fn get_node(
    conn: &mut SqliteConnection,
    scene_id: &str,
    id: &str,
) -> Result<NodeRow, sqlx::Error> {
    sqlx::query_as::<Sqlite, NodeRow>("SELECT * FROM scene_nodes WHERE scene_id = $1 AND id = $2")
        .bind(scene_id)
        .bind(id)
        .fetch_one(conn)
        .await
}

pub(super) struct NodeValues<'a> {
    pub name: Option<&'a str>,
    pub node_type: &'a str,
    pub plugin_id: &'a str,
    pub plugin_version: Option<&'a str>,
    pub config: &'a str,
}

pub(super) async fn insert_node(
    conn: &mut SqliteConnection,
    scene_id: &str,
    id: &str,
    v: &NodeValues<'_>,
) -> Result<NodeRow, sqlx::Error> {
    sqlx::query_as::<Sqlite, NodeRow>(
        "INSERT INTO scene_nodes (id, name, scene_id, \"type\", plugin_id, plugin_version, config) \
         VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING *",
    )
    .bind(id)
    .bind(v.name)
    .bind(scene_id)
    .bind(v.node_type)
    .bind(v.plugin_id)
    .bind(v.plugin_version)
    .bind(v.config)
    .fetch_one(conn)
    .await
}

pub(super) async fn update_node(
    conn: &mut SqliteConnection,
    scene_id: &str,
    id: &str,
    v: &NodeValues<'_>,
) -> Result<NodeRow, sqlx::Error> {
    sqlx::query_as::<Sqlite, NodeRow>(
        "UPDATE scene_nodes SET name = $1, \"type\" = $2, plugin_id = $3, plugin_version = $4, config = $5, update_time = $6 \
         WHERE scene_id = $7 AND id = $8 RETURNING *",
    )
    .bind(v.name)
    .bind(v.node_type)
    .bind(v.plugin_id)
    .bind(v.plugin_version)
    .bind(v.config)
    .bind(Local::now())
    .bind(scene_id)
    .bind(id)
    .fetch_one(conn)
    .await
}

pub(super) async fn delete_node(
    conn: &mut SqliteConnection,
    scene_id: &str,
    id: &str,
) -> Result<u64, sqlx::Error> {
    let result = sqlx::query("DELETE FROM scene_nodes WHERE scene_id = $1 AND id = $2")
        .bind(scene_id)
        .bind(id)
        .execute(conn)
        .await?;
    Ok(result.rows_affected())
}

// --- edges ---

pub(super) async fn list_edges(
    conn: &mut SqliteConnection,
    scene_id: &str,
) -> Result<Vec<EdgeRow>, sqlx::Error> {
    sqlx::query_as::<Sqlite, EdgeRow>(
        "SELECT * FROM scene_edges WHERE scene_id = $1 ORDER BY create_time, id",
    )
    .bind(scene_id)
    .fetch_all(conn)
    .await
}

pub(super) async fn get_edge(
    conn: &mut SqliteConnection,
    scene_id: &str,
    id: &str,
) -> Result<EdgeRow, sqlx::Error> {
    sqlx::query_as::<Sqlite, EdgeRow>("SELECT * FROM scene_edges WHERE scene_id = $1 AND id = $2")
        .bind(scene_id)
        .bind(id)
        .fetch_one(conn)
        .await
}

pub(super) struct EdgeValues<'a> {
    pub name: Option<&'a str>,
    pub source_node_id: &'a str,
    pub target_node_id: &'a str,
    pub mapping: Option<&'a str>,
}

pub(super) async fn insert_edge(
    conn: &mut SqliteConnection,
    scene_id: &str,
    id: &str,
    v: &EdgeValues<'_>,
) -> Result<EdgeRow, sqlx::Error> {
    sqlx::query_as::<Sqlite, EdgeRow>(
        "INSERT INTO scene_edges (id, name, scene_id, source_node_id, target_node_id, mapping) \
         VALUES ($1, $2, $3, $4, $5, $6) RETURNING *",
    )
    .bind(id)
    .bind(v.name)
    .bind(scene_id)
    .bind(v.source_node_id)
    .bind(v.target_node_id)
    .bind(v.mapping)
    .fetch_one(conn)
    .await
}

pub(super) async fn update_edge(
    conn: &mut SqliteConnection,
    scene_id: &str,
    id: &str,
    v: &EdgeValues<'_>,
) -> Result<EdgeRow, sqlx::Error> {
    sqlx::query_as::<Sqlite, EdgeRow>(
        "UPDATE scene_edges SET name = $1, source_node_id = $2, target_node_id = $3, mapping = $4, update_time = $5 \
         WHERE scene_id = $6 AND id = $7 RETURNING *",
    )
    .bind(v.name)
    .bind(v.source_node_id)
    .bind(v.target_node_id)
    .bind(v.mapping)
    .bind(Local::now())
    .bind(scene_id)
    .bind(id)
    .fetch_one(conn)
    .await
}

pub(super) async fn delete_edge(
    conn: &mut SqliteConnection,
    scene_id: &str,
    id: &str,
) -> Result<u64, sqlx::Error> {
    let result = sqlx::query("DELETE FROM scene_edges WHERE scene_id = $1 AND id = $2")
        .bind(scene_id)
        .bind(id)
        .execute(conn)
        .await?;
    Ok(result.rows_affected())
}
