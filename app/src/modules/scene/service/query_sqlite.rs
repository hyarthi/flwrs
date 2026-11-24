use crate::modules::node_config::SceneDefaults;
use crate::modules::scene::service::{EdgeRow, ListFilters, NodeRow, NodeUpload, Scene};
use chrono::Local;
use flwrs_types::groups::Group;
use sqlx::{AssertSqlSafe, Sqlite, SqliteConnection};

/// Which graph a query reads or writes: the live one, or the scene's draft.
/// (The queries splice in only these fixed table names and filters, so the
/// formatted SQL is marked `AssertSqlSafe`.)
/// The draft tables mirror the live ones (plus `base_hash`, `removed`,
/// `conflict`), so the same queries serve both; a draft's removed rows are
/// tombstones, which reads leave out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Store {
    Live,
    Draft,
}

impl Store {
    pub(crate) fn nodes(self) -> &'static str {
        match self {
            Store::Live => "scene_nodes",
            Store::Draft => "scene_draft_nodes",
        }
    }

    pub(crate) fn edges(self) -> &'static str {
        match self {
            Store::Live => "scene_edges",
            Store::Draft => "scene_draft_edges",
        }
    }

    pub(crate) fn uploads(self) -> &'static str {
        match self {
            Store::Live => "scene_node_uploads",
            Store::Draft => "scene_draft_node_uploads",
        }
    }

    /// ` AND <alias>removed = FALSE` for drafts (tombstones don't count).
    fn present(self, alias: &str) -> String {
        match self {
            Store::Live => String::new(),
            Store::Draft => format!(" AND {alias}removed = FALSE"),
        }
    }
}

// --- uploaded node files ---

pub(super) async fn list_uploads(
    conn: &mut SqliteConnection,
    store: Store,
    node_id: &str,
) -> Result<Vec<NodeUpload>, sqlx::Error> {
    sqlx::query_as::<Sqlite, NodeUpload>(AssertSqlSafe(format!(
        "SELECT * FROM {} WHERE node_id = $1 ORDER BY \"name\"",
        store.uploads()
    )))
    .bind(node_id)
    .fetch_all(conn)
    .await
}

pub(super) async fn list_scene_uploads(
    conn: &mut SqliteConnection,
    store: Store,
    scene_id: &str,
) -> Result<Vec<NodeUpload>, sqlx::Error> {
    sqlx::query_as::<Sqlite, NodeUpload>(AssertSqlSafe(format!(
        "SELECT u.* FROM {} u JOIN {} n ON n.id = u.node_id \
         WHERE n.scene_id = $1{} ORDER BY u.node_id, u.\"name\"",
        store.uploads(),
        store.nodes(),
        store.present("n.")
    )))
    .bind(scene_id)
    .fetch_all(conn)
    .await
}

pub(super) async fn get_upload(
    conn: &mut SqliteConnection,
    store: Store,
    node_id: &str,
    name: &str,
) -> Result<NodeUpload, sqlx::Error> {
    sqlx::query_as::<Sqlite, NodeUpload>(AssertSqlSafe(format!(
        "SELECT * FROM {} WHERE node_id = $1 AND \"name\" = $2",
        store.uploads()
    )))
    .bind(node_id)
    .bind(name)
    .fetch_one(conn)
    .await
}

/// Adds or replaces the node's upload named `name`.
pub(super) async fn put_upload(
    conn: &mut SqliteConnection,
    store: Store,
    node_id: &str,
    name: &str,
    sha256: &str,
    size: i64,
) -> Result<NodeUpload, sqlx::Error> {
    sqlx::query_as::<Sqlite, NodeUpload>(AssertSqlSafe(format!(
        "INSERT INTO {} (node_id, \"name\", sha256, \"size\", create_time) \
         VALUES ($1, $2, $3, $4, $5) \
         ON CONFLICT (node_id, \"name\") DO UPDATE SET sha256 = excluded.sha256, \
             \"size\" = excluded.\"size\", create_time = excluded.create_time \
         RETURNING *",
        store.uploads()
    )))
    .bind(node_id)
    .bind(name)
    .bind(sha256)
    .bind(size)
    .bind(Local::now())
    .fetch_one(conn)
    .await
}

pub(super) async fn delete_upload(
    conn: &mut SqliteConnection,
    store: Store,
    node_id: &str,
    name: &str,
) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(AssertSqlSafe(format!(
        "DELETE FROM {} WHERE node_id = $1 AND \"name\" = $2",
        store.uploads()
    )))
    .bind(node_id)
    .bind(name)
    .execute(conn)
    .await?;
    Ok(result.rows_affected())
}

/// Every stored file some node still has, live or in a draft (tombstones
/// included: a restore brings their files back): (name, sha256).
pub(super) async fn stored_files(
    conn: &mut SqliteConnection,
) -> Result<Vec<(String, String)>, sqlx::Error> {
    sqlx::query_as::<Sqlite, (String, String)>(
        "SELECT \"name\", sha256 FROM scene_node_uploads \
         UNION SELECT \"name\", sha256 FROM scene_draft_node_uploads \
         UNION SELECT \"name\", sha256 FROM scene_draft_history_uploads",
    )
    .fetch_all(conn)
    .await
}

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
    defaults: &SceneDefaults,
    groups: &[Group],
) -> Result<Scene, sqlx::Error> {
    sqlx::query_as::<Sqlite, Scene>(
        "INSERT INTO scenes (id, name, max_hops, defaults, \"groups\") VALUES ($1, $2, $3, $4, $5) RETURNING *",
    )
    .bind(id)
    .bind(name)
    .bind(max_hops)
    .bind(sqlx::types::Json(defaults))
    .bind(sqlx::types::Json(groups))
    .fetch_one(conn)
    .await
}

pub(super) async fn update_scene(
    conn: &mut SqliteConnection,
    id: &str,
    name: &str,
    max_hops: Option<u32>,
    defaults: &SceneDefaults,
    groups: &[Group],
) -> Result<Scene, sqlx::Error> {
    sqlx::query_as::<Sqlite, Scene>(
        "UPDATE scenes SET name = $1, max_hops = $2, defaults = $3, \"groups\" = $4, update_time = $5 WHERE id = $6 RETURNING *",
    )
    .bind(name)
    .bind(max_hops)
    .bind(sqlx::types::Json(defaults))
    .bind(sqlx::types::Json(groups))
    .bind(Local::now())
    .bind(id)
    .fetch_one(conn)
    .await
}

/// Counts a committed change to the scene's live graph.
pub(super) async fn bump_revision(
    conn: &mut SqliteConnection,
    scene_id: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE scenes SET revision = revision + 1 WHERE id = $1")
        .bind(scene_id)
        .execute(conn)
        .await?;
    Ok(())
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

/// Every scene, newest first (ids are ULIDs: within a second, too; the live
/// list; no paging).
pub(super) async fn all_scenes(conn: &mut SqliteConnection) -> Result<Vec<Scene>, sqlx::Error> {
    sqlx::query_as::<Sqlite, Scene>("SELECT * FROM scenes ORDER BY create_time DESC, id DESC")
        .fetch_all(conn)
        .await
}

/// `(scene id, nodes, edges)` for every scene.
pub(super) async fn graph_sizes(
    conn: &mut SqliteConnection,
) -> Result<Vec<(String, i64, i64)>, sqlx::Error> {
    sqlx::query_as::<Sqlite, (String, i64, i64)>(
        "SELECT s.id, \
             (SELECT COUNT(*) FROM scene_nodes n WHERE n.scene_id = s.id), \
             (SELECT COUNT(*) FROM scene_edges e WHERE e.scene_id = s.id) \
         FROM scenes s",
    )
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
    store: Store,
    scene_id: &str,
) -> Result<Vec<NodeRow>, sqlx::Error> {
    sqlx::query_as::<Sqlite, NodeRow>(AssertSqlSafe(format!(
        "SELECT * FROM {} WHERE scene_id = $1{} ORDER BY create_time, id",
        store.nodes(),
        store.present("")
    )))
    .bind(scene_id)
    .fetch_all(conn)
    .await
}

pub(super) async fn get_node(
    conn: &mut SqliteConnection,
    store: Store,
    scene_id: &str,
    id: &str,
) -> Result<NodeRow, sqlx::Error> {
    sqlx::query_as::<Sqlite, NodeRow>(AssertSqlSafe(format!(
        "SELECT * FROM {} WHERE scene_id = $1 AND id = $2{}",
        store.nodes(),
        store.present("")
    )))
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
    store: Store,
    scene_id: &str,
    id: &str,
    v: &NodeValues<'_>,
) -> Result<NodeRow, sqlx::Error> {
    sqlx::query_as::<Sqlite, NodeRow>(AssertSqlSafe(format!(
        "INSERT INTO {} (id, name, scene_id, \"type\", plugin_id, plugin_version, config) \
         VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING *",
        store.nodes()
    )))
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
    store: Store,
    scene_id: &str,
    id: &str,
    v: &NodeValues<'_>,
) -> Result<NodeRow, sqlx::Error> {
    sqlx::query_as::<Sqlite, NodeRow>(AssertSqlSafe(format!(
        "UPDATE {} SET name = $1, \"type\" = $2, plugin_id = $3, plugin_version = $4, config = $5, update_time = $6 \
         WHERE scene_id = $7 AND id = $8{} RETURNING *",
        store.nodes(),
        store.present("")
    )))
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

/// A node's layout (`ui`), tombstones included: `None` = no such node.
pub(super) async fn get_ui(
    conn: &mut SqliteConnection,
    store: Store,
    scene_id: &str,
    id: &str,
) -> Result<Option<Option<String>>, sqlx::Error> {
    sqlx::query_scalar(AssertSqlSafe(format!(
        "SELECT ui FROM {} WHERE scene_id = $1 AND id = $2",
        store.nodes()
    )))
    .bind(scene_id)
    .bind(id)
    .fetch_optional(conn)
    .await
}

/// Sets a node's layout (`ui`), tombstones included; no update time: layout
/// isn't content. Returns whether the node is there.
pub(super) async fn set_ui(
    conn: &mut SqliteConnection,
    store: Store,
    scene_id: &str,
    id: &str,
    ui: Option<&str>,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(AssertSqlSafe(format!(
        "UPDATE {} SET ui = $1 WHERE scene_id = $2 AND id = $3",
        store.nodes()
    )))
    .bind(ui)
    .bind(scene_id)
    .bind(id)
    .execute(conn)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// As [`set_ui`], for an edge.
pub(super) async fn set_edge_ui(
    conn: &mut SqliteConnection,
    store: Store,
    scene_id: &str,
    id: &str,
    ui: Option<&str>,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(AssertSqlSafe(format!(
        "UPDATE {} SET ui = $1 WHERE scene_id = $2 AND id = $3",
        store.edges()
    )))
    .bind(ui)
    .bind(scene_id)
    .bind(id)
    .execute(conn)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Deletes the row (a draft's tombstone too: drafts mark removals with
/// `draft::query::remove_node` instead).
pub(super) async fn delete_node(
    conn: &mut SqliteConnection,
    store: Store,
    scene_id: &str,
    id: &str,
) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(AssertSqlSafe(format!(
        "DELETE FROM {} WHERE scene_id = $1 AND id = $2",
        store.nodes()
    )))
    .bind(scene_id)
    .bind(id)
    .execute(conn)
    .await?;
    Ok(result.rows_affected())
}

// --- edges ---

pub(super) async fn list_edges(
    conn: &mut SqliteConnection,
    store: Store,
    scene_id: &str,
) -> Result<Vec<EdgeRow>, sqlx::Error> {
    sqlx::query_as::<Sqlite, EdgeRow>(AssertSqlSafe(format!(
        "SELECT * FROM {} WHERE scene_id = $1{} ORDER BY create_time, id",
        store.edges(),
        store.present("")
    )))
    .bind(scene_id)
    .fetch_all(conn)
    .await
}

pub(super) async fn get_edge(
    conn: &mut SqliteConnection,
    store: Store,
    scene_id: &str,
    id: &str,
) -> Result<EdgeRow, sqlx::Error> {
    sqlx::query_as::<Sqlite, EdgeRow>(AssertSqlSafe(format!(
        "SELECT * FROM {} WHERE scene_id = $1 AND id = $2{}",
        store.edges(),
        store.present("")
    )))
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
    store: Store,
    scene_id: &str,
    id: &str,
    v: &EdgeValues<'_>,
) -> Result<EdgeRow, sqlx::Error> {
    sqlx::query_as::<Sqlite, EdgeRow>(AssertSqlSafe(format!(
        "INSERT INTO {} (id, name, scene_id, source_node_id, target_node_id, mapping) \
         VALUES ($1, $2, $3, $4, $5, $6) RETURNING *",
        store.edges()
    )))
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
    store: Store,
    scene_id: &str,
    id: &str,
    v: &EdgeValues<'_>,
) -> Result<EdgeRow, sqlx::Error> {
    sqlx::query_as::<Sqlite, EdgeRow>(AssertSqlSafe(format!(
        "UPDATE {} SET name = $1, source_node_id = $2, target_node_id = $3, mapping = $4, update_time = $5 \
         WHERE scene_id = $6 AND id = $7{} RETURNING *",
        store.edges(),
        store.present("")
    )))
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
    store: Store,
    scene_id: &str,
    id: &str,
) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(AssertSqlSafe(format!(
        "DELETE FROM {} WHERE scene_id = $1 AND id = $2",
        store.edges()
    )))
    .bind(scene_id)
    .bind(id)
    .execute(conn)
    .await?;
    Ok(result.rows_affected())
}
