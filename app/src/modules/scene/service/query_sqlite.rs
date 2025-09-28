use crate::modules::scene::service::{ListFilters, Scene, ServiceError};
use sqlx::error::ErrorKind;
use sqlx::pool::PoolConnection;
use sqlx::{Executor, FromRow, Sqlite};

impl From<sqlx::Error> for ServiceError {
    fn from(error: sqlx::Error) -> Self {
        match error {
            sqlx::Error::RowNotFound => ServiceError::NotFound,
            _ => match error.as_database_error() {
                None => ServiceError::Query(error),
                Some(e) => match e.kind() {
                    ErrorKind::UniqueViolation => ServiceError::Conflict,
                    ErrorKind::ForeignKeyViolation => ServiceError::Conflict,
                    ErrorKind::CheckViolation => ServiceError::Conflict,
                    _ => ServiceError::Query(error),
                },
            },
        }
    }
}

pub(super) async fn get_scene(
    conn: &mut PoolConnection<Sqlite>,
    id: &str,
) -> Result<Scene, sqlx::Error> {
    let row = conn
        .fetch_one(sqlx::query_as::<Sqlite, Scene>("SELECT * FROM scenes WHERE id = $1").bind(id))
        .await?;
    let scene = Scene::from_row(&row)?;
    Ok(scene)
}

pub(super) async fn create_scene(
    conn: &mut PoolConnection<Sqlite>,
    scene: Scene,
) -> Result<Scene, sqlx::Error> {
    let row = conn
        .fetch_one(
            sqlx::query_as::<Sqlite, Scene>(
                "INSERT INTO scenes (id, name) VALUES ($1, $2) RETURNING *",
            )
            .bind(scene.id)
            .bind(scene.name),
        )
        .await?;
    let scene = Scene::from_row(&row)?;
    Ok(scene)
}

pub(super) async fn update_scene(
    conn: &mut PoolConnection<Sqlite>,
    scene: Scene,
) -> Result<Scene, sqlx::Error> {
    let row = conn
        .fetch_one(
            sqlx::query_as::<Sqlite, Scene>(
                "UPDATE scenes SET name = $1, update_time = $2 WHERE id = $3 RETURNING *",
            )
            .bind(scene.name)
            .bind(scene.update_time)
            .bind(scene.id),
        )
        .await?;
    let scene = Scene::from_row(&row)?;
    Ok(scene)
}

pub(super) async fn delete_scene(
    conn: &mut PoolConnection<Sqlite>,
    id: &str,
) -> Result<u64, sqlx::Error> {
    match conn
        .execute(sqlx::query("DELETE FROM scenes WHERE id = $1").bind(id.to_string()))
        .await
    {
        Ok(result) => Ok(result.rows_affected()),
        Err(e) => Err(e),
    }
}

pub(super) async fn list_scenes(
    conn: &mut PoolConnection<Sqlite>,
    filters: ListFilters,
) -> Result<Vec<Scene>, sqlx::Error> {
    let rows = conn
        .fetch_all(
            sqlx::query_as::<Sqlite, Scene>(
                "SELECT * FROM scenes ORDER BY create_time DESC LIMIT $1 OFFSET $2",
            )
            .bind(filters.limit)
            .bind(filters.offset),
        )
        .await?;
    let mut scenes = vec![];
    for row in rows {
        scenes.push(Scene::from_row(&row)?);
    }

    Ok(scenes)
}
