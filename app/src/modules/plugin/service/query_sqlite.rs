use crate::modules::plugin::service::{NewPlugin, Plugin, ServiceError};
use chrono::Local;
use sqlx::error::ErrorKind;
use sqlx::pool::PoolConnection;
use sqlx::{Executor, FromRow, Sqlite};

impl From<sqlx::Error> for ServiceError {
    fn from(error: sqlx::Error) -> Self {
        match &error {
            sqlx::Error::RowNotFound => ServiceError::NotFound,
            _ => match error.as_database_error().map(|e| e.kind()) {
                Some(ErrorKind::UniqueViolation) => ServiceError::AlreadyExists(String::new()),
                Some(ErrorKind::ForeignKeyViolation) => ServiceError::InUse(vec![]),
                _ => ServiceError::Query(error),
            },
        }
    }
}

pub(super) async fn get(
    conn: &mut PoolConnection<Sqlite>,
    id: &str,
) -> Result<Plugin, sqlx::Error> {
    let row = conn
        .fetch_one(sqlx::query_as::<Sqlite, Plugin>("SELECT * FROM plugins WHERE id = $1").bind(id))
        .await?;
    Plugin::from_row(&row)
}

pub(super) async fn list(conn: &mut PoolConnection<Sqlite>) -> Result<Vec<Plugin>, sqlx::Error> {
    let rows = conn
        .fetch_all(sqlx::query_as::<Sqlite, Plugin>(
            "SELECT * FROM plugins ORDER BY id",
        ))
        .await?;
    rows.iter().map(Plugin::from_row).collect()
}

pub(super) async fn insert(
    conn: &mut PoolConnection<Sqlite>,
    p: &NewPlugin,
) -> Result<Plugin, sqlx::Error> {
    let row = conn
        .fetch_one(
            sqlx::query_as::<Sqlite, Plugin>(
                "INSERT INTO plugins (id, name, version, \"type\", description, manifest, launch_args_schema, \
                     in_schema, out_schema, install_path, archive_sha256) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) RETURNING *",
            )
            .bind(&p.id)
            .bind(&p.name)
            .bind(&p.version)
            .bind(p.plugin_type)
            .bind(&p.description)
            .bind(&p.manifest)
            .bind(&p.launch_args_schema)
            .bind(&p.in_schema)
            .bind(&p.out_schema)
            .bind(&p.install_path)
            .bind(&p.archive_sha256),
        )
        .await?;
    Plugin::from_row(&row)
}

/// Replaces the package-derived columns of an existing plugin. `type` and
/// `install_path` never change on upgrade.
pub(super) async fn update_package(
    conn: &mut PoolConnection<Sqlite>,
    p: &NewPlugin,
) -> Result<Plugin, sqlx::Error> {
    let row = conn
        .fetch_one(
            sqlx::query_as::<Sqlite, Plugin>(
                "UPDATE plugins SET name = $1, version = $2, description = $3, manifest = $4, \
                     launch_args_schema = $5, in_schema = $6, out_schema = $7, archive_sha256 = $8, \
                     update_time = $9 \
                 WHERE id = $10 RETURNING *",
            )
            .bind(&p.name)
            .bind(&p.version)
            .bind(&p.description)
            .bind(&p.manifest)
            .bind(&p.launch_args_schema)
            .bind(&p.in_schema)
            .bind(&p.out_schema)
            .bind(&p.archive_sha256)
            .bind(Local::now())
            .bind(&p.id),
        )
        .await?;
    Plugin::from_row(&row)
}

pub(super) async fn delete(
    conn: &mut PoolConnection<Sqlite>,
    id: &str,
) -> Result<u64, sqlx::Error> {
    let result = conn
        .execute(sqlx::query("DELETE FROM plugins WHERE id = $1").bind(id))
        .await?;
    Ok(result.rows_affected())
}

/// `(node id, node config JSON)` of every scene node that uses the plugin.
pub(super) async fn nodes_using(
    conn: &mut PoolConnection<Sqlite>,
    plugin_id: &str,
) -> Result<Vec<(String, String)>, sqlx::Error> {
    sqlx::query_as::<Sqlite, (String, String)>(
        "SELECT id, config FROM scene_nodes WHERE plugin_id = $1 ORDER BY id",
    )
    .bind(plugin_id)
    .fetch_all(&mut **conn)
    .await
}

/// Ids of enabled scenes with at least one node using the plugin.
pub(super) async fn enabled_scenes_using(
    conn: &mut PoolConnection<Sqlite>,
    plugin_id: &str,
) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar::<Sqlite, String>(
        "SELECT DISTINCT s.id FROM scenes s JOIN scene_nodes n ON n.scene_id = s.id \
         WHERE n.plugin_id = $1 AND s.enabled ORDER BY s.id",
    )
    .bind(plugin_id)
    .fetch_all(&mut **conn)
    .await
}
