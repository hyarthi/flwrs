//! SQLite queries for drafts' own rows (the graph's rows go through
//! `query_sqlite` with `Store::Draft`). They take `&mut SqliteConnection`,
//! so they work inside transactions.

use crate::modules::node_config::SceneDefaults;
use crate::modules::scene::service::query_sqlite::Store;
use crate::modules::scene::service::{EdgeRow, NodeRow, NodeUpload};
use chrono::Local;
use sqlx::sqlite::SqliteRow;
use sqlx::{AssertSqlSafe, FromRow, Sqlite, SqliteConnection};

/// A draft's own row: its scene settings and where it started from.
#[derive(sqlx::FromRow, Debug, Clone)]
pub(crate) struct DraftRow {
    pub scene_id: String,
    pub base_revision: i64,
    pub name: String,
    pub max_hops: Option<u32>,
    #[sqlx(json)]
    pub defaults: SceneDefaults,
    pub base_hash: String,
    pub conflict: Option<String>,
    pub create_time: chrono::DateTime<Local>,
    pub update_time: chrono::DateTime<Local>,
}

/// A draft node's or edge's own columns.
#[derive(sqlx::FromRow, Debug, Clone, PartialEq, Eq)]
pub(crate) struct RowMeta {
    pub id: String,
    /// The live row's content hash at the base; `None` = added in the draft.
    pub base_hash: Option<String>,
    /// Removed in the draft (a tombstone).
    pub removed: bool,
    pub conflict: Option<String>,
}

pub(crate) async fn get(
    conn: &mut SqliteConnection,
    scene_id: &str,
) -> Result<Option<DraftRow>, sqlx::Error> {
    sqlx::query_as::<Sqlite, DraftRow>("SELECT * FROM scene_drafts WHERE scene_id = $1")
        .bind(scene_id)
        .fetch_optional(conn)
        .await
}

pub(crate) async fn insert(
    conn: &mut SqliteConnection,
    scene_id: &str,
    base_revision: i64,
    name: &str,
    max_hops: Option<u32>,
    defaults: &SceneDefaults,
    base_hash: &str,
) -> Result<DraftRow, sqlx::Error> {
    sqlx::query_as::<Sqlite, DraftRow>(
        "INSERT INTO scene_drafts (scene_id, base_revision, \"name\", max_hops, defaults, base_hash) \
         VALUES ($1, $2, $3, $4, $5, $6) RETURNING *",
    )
    .bind(scene_id)
    .bind(base_revision)
    .bind(name)
    .bind(max_hops)
    .bind(sqlx::types::Json(defaults))
    .bind(base_hash)
    .fetch_one(conn)
    .await
}

/// Marks the draft as changed now.
pub(crate) async fn touch(conn: &mut SqliteConnection, scene_id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE scene_drafts SET update_time = $1 WHERE scene_id = $2")
        .bind(Local::now())
        .bind(scene_id)
        .execute(conn)
        .await?;
    Ok(())
}

pub(crate) async fn set_settings(
    conn: &mut SqliteConnection,
    scene_id: &str,
    name: &str,
    max_hops: Option<u32>,
    defaults: &SceneDefaults,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE scene_drafts SET \"name\" = $1, max_hops = $2, defaults = $3, update_time = $4 \
         WHERE scene_id = $5",
    )
    .bind(name)
    .bind(max_hops)
    .bind(sqlx::types::Json(defaults))
    .bind(Local::now())
    .bind(scene_id)
    .execute(conn)
    .await?;
    Ok(())
}

/// The settings' base and conflict (a rebase or a resolution).
pub(crate) async fn set_settings_meta(
    conn: &mut SqliteConnection,
    scene_id: &str,
    base_hash: &str,
    conflict: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE scene_drafts SET base_hash = $1, conflict = $2 WHERE scene_id = $3")
        .bind(base_hash)
        .bind(conflict)
        .bind(scene_id)
        .execute(conn)
        .await?;
    Ok(())
}

pub(crate) async fn set_base_revision(
    conn: &mut SqliteConnection,
    scene_id: &str,
    revision: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE scene_drafts SET base_revision = $1 WHERE scene_id = $2")
        .bind(revision)
        .bind(scene_id)
        .execute(conn)
        .await?;
    Ok(())
}

/// Deletes the draft (its rows and uploads go by cascade).
pub(crate) async fn delete(
    conn: &mut SqliteConnection,
    scene_id: &str,
) -> Result<u64, sqlx::Error> {
    let result = sqlx::query("DELETE FROM scene_drafts WHERE scene_id = $1")
        .bind(scene_id)
        .execute(conn)
        .await?;
    Ok(result.rows_affected())
}

// --- nodes and edges, tombstones included ---

pub(crate) async fn all_nodes(
    conn: &mut SqliteConnection,
    scene_id: &str,
) -> Result<Vec<(NodeRow, RowMeta)>, sqlx::Error> {
    let rows: Vec<SqliteRow> =
        sqlx::query("SELECT * FROM scene_draft_nodes WHERE scene_id = $1 ORDER BY create_time, id")
            .bind(scene_id)
            .fetch_all(conn)
            .await?;
    rows.iter()
        .map(|r| Ok((NodeRow::from_row(r)?, RowMeta::from_row(r)?)))
        .collect()
}

pub(crate) async fn all_edges(
    conn: &mut SqliteConnection,
    scene_id: &str,
) -> Result<Vec<(EdgeRow, RowMeta)>, sqlx::Error> {
    let rows: Vec<SqliteRow> =
        sqlx::query("SELECT * FROM scene_draft_edges WHERE scene_id = $1 ORDER BY create_time, id")
            .bind(scene_id)
            .fetch_all(conn)
            .await?;
    rows.iter()
        .map(|r| Ok((EdgeRow::from_row(r)?, RowMeta::from_row(r)?)))
        .collect()
}

/// A node's or edge's own columns, tombstone or not.
pub(crate) async fn meta(
    conn: &mut SqliteConnection,
    table: &str,
    scene_id: &str,
    id: &str,
) -> Result<Option<RowMeta>, sqlx::Error> {
    sqlx::query_as::<Sqlite, RowMeta>(AssertSqlSafe(format!(
        "SELECT id, base_hash, removed, conflict FROM {table} WHERE scene_id = $1 AND id = $2"
    )))
    .bind(scene_id)
    .bind(id)
    .fetch_optional(conn)
    .await
}

pub(crate) async fn set_meta(
    conn: &mut SqliteConnection,
    table: &str,
    id: &str,
    meta: &RowMeta,
) -> Result<(), sqlx::Error> {
    sqlx::query(AssertSqlSafe(format!(
        "UPDATE {table} SET base_hash = $1, removed = $2, conflict = $3, update_time = $4 WHERE id = $5"
    )))
    .bind(meta.base_hash.as_deref())
    .bind(meta.removed)
    .bind(meta.conflict.as_deref())
    .bind(Local::now())
    .bind(id)
    .execute(conn)
    .await?;
    Ok(())
}

/// Deletes a node's or edge's row (a node's edges and uploads go with it).
pub(crate) async fn delete_row(
    conn: &mut SqliteConnection,
    table: &str,
    id: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(AssertSqlSafe(format!("DELETE FROM {table} WHERE id = $1")))
        .bind(id)
        .execute(conn)
        .await?;
    Ok(())
}

/// Writes a live node's content into the draft (a new row, or over the
/// draft's row with the same id), with its own columns.
pub(crate) async fn put_node(
    conn: &mut SqliteConnection,
    row: &NodeRow,
    meta: &RowMeta,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO scene_draft_nodes (id, \"name\", scene_id, \"type\", plugin_id, plugin_version, config, \
             create_time, update_time, base_hash, removed, conflict, ui) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13) \
         ON CONFLICT (id) DO UPDATE SET \"name\" = excluded.\"name\", \"type\" = excluded.\"type\", \
             plugin_id = excluded.plugin_id, plugin_version = excluded.plugin_version, \
             config = excluded.config, update_time = excluded.update_time, \
             base_hash = excluded.base_hash, removed = excluded.removed, conflict = excluded.conflict, \
             ui = COALESCE(excluded.ui, scene_draft_nodes.ui)",
    )
    .bind(&row.id)
    .bind(&row.name)
    .bind(&row.scene_id)
    .bind(&row.node_type)
    .bind(&row.plugin_id)
    .bind(&row.plugin_version)
    .bind(&row.config)
    .bind(row.create_time)
    .bind(row.update_time)
    .bind(meta.base_hash.as_deref())
    .bind(meta.removed)
    .bind(meta.conflict.as_deref())
    .bind(&row.ui)
    .execute(conn)
    .await?;
    Ok(())
}

/// As [`put_node`], for an edge.
pub(crate) async fn put_edge(
    conn: &mut SqliteConnection,
    row: &EdgeRow,
    meta: &RowMeta,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO scene_draft_edges (id, \"name\", scene_id, source_node_id, target_node_id, mapping, \
             create_time, update_time, base_hash, removed, conflict) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) \
         ON CONFLICT (id) DO UPDATE SET \"name\" = excluded.\"name\", \
             source_node_id = excluded.source_node_id, target_node_id = excluded.target_node_id, \
             mapping = excluded.mapping, update_time = excluded.update_time, \
             base_hash = excluded.base_hash, removed = excluded.removed, conflict = excluded.conflict",
    )
    .bind(&row.id)
    .bind(&row.name)
    .bind(&row.scene_id)
    .bind(&row.source_node_id)
    .bind(&row.target_node_id)
    .bind(&row.mapping)
    .bind(row.create_time)
    .bind(row.update_time)
    .bind(meta.base_hash.as_deref())
    .bind(meta.removed)
    .bind(meta.conflict.as_deref())
    .execute(conn)
    .await?;
    Ok(())
}

/// Makes `node_id`'s uploads in `store` exactly `uploads`.
pub(crate) async fn replace_uploads(
    conn: &mut SqliteConnection,
    store: Store,
    node_id: &str,
    uploads: &[NodeUpload],
) -> Result<(), sqlx::Error> {
    sqlx::query(AssertSqlSafe(format!(
        "DELETE FROM {} WHERE node_id = $1",
        store.uploads()
    )))
    .bind(node_id)
    .execute(&mut *conn)
    .await?;
    for u in uploads {
        sqlx::query(AssertSqlSafe(format!(
            "INSERT INTO {} (node_id, \"name\", sha256, \"size\", create_time) VALUES ($1, $2, $3, $4, $5)",
            store.uploads()
        )))
        .bind(node_id)
        .bind(&u.name)
        .bind(&u.sha256)
        .bind(u.size)
        .bind(u.create_time)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// Every upload of the scene's draft nodes, tombstones included.
pub(crate) async fn all_uploads(
    conn: &mut SqliteConnection,
    scene_id: &str,
) -> Result<Vec<NodeUpload>, sqlx::Error> {
    sqlx::query_as::<Sqlite, NodeUpload>(
        "SELECT u.* FROM scene_draft_node_uploads u JOIN scene_draft_nodes n ON n.id = u.node_id \
         WHERE n.scene_id = $1 ORDER BY u.node_id, u.\"name\"",
    )
    .bind(scene_id)
    .fetch_all(conn)
    .await
}
