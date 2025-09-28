mod query_sqlite;

use flwrs_core::db::{Database, DbError};
use chrono::Local;
use thiserror::Error;
use ulid::Ulid;

#[derive(sqlx::FromRow, Debug)]
pub(crate) struct Scene {
    pub id: String,
    pub name: String,
    pub create_time: chrono::DateTime<Local>,
    pub update_time: chrono::DateTime<Local>,
}

#[derive(Error, Debug)]
pub(crate) enum ServiceError {
    #[error("no scene found")]
    NotFound,
    #[error("conflict")]
    Conflict,
    #[error("failed to execute query: {0}")]
    Query(sqlx::Error),
    #[error("failed to get connection: {0}")]
    Connection(#[from] DbError),
    #[error("unknown error occurred: {0}")]
    Unknown(String),
}

pub(crate) struct ListFilters {
    pub(self) offset: i64,
    pub(self) limit: i64,
}

impl ListFilters {
    pub(crate) fn new(offset: i64, limit: i64) -> Self {
        Self { offset, limit }
    }
}

pub(crate) struct Service {
    db: &'static Database,
}

impl Service {
    pub(crate) fn new(db: &'static Database) -> Self {
        Self { db }
    }

    pub(crate) async fn get_scene(&self, id: &str) -> Result<Scene, ServiceError> {
        match self.db {
            Database::SQLite(db) => {
                let mut conn = db.get_connection().await?;
                let scene = query_sqlite::get_scene(&mut conn, id).await?;
                Ok(scene)
            }
        }
    }

    pub(crate) async fn create_scene(&self, scene: Scene) -> Result<Scene, ServiceError> {
        log::debug!("Scene Service: creating scene");
        match self.db {
            Database::SQLite(db) => {
                let mut conn = db.get_connection().await?;
                let input = Scene {
                    id: Ulid::new().to_string(),
                    name: scene.name,
                    create_time: Local::now(),
                    update_time: Local::now(),
                };
                match query_sqlite::create_scene(&mut conn, input).await {
                    Ok(output) => Ok(output),
                    Err(e) => {
                        log::error!("Scene Service: failed to create scene: {e}");
                        let svc_error = ServiceError::from(e);
                        if matches!(svc_error, ServiceError::NotFound) {
                            Err(ServiceError::Unknown(
                                "insert returned empty result".to_string(),
                            ))
                        } else {
                            Err(svc_error)
                        }
                    }
                }
            }
        }
    }

    pub(crate) async fn update_scene(&self, scene: Scene) -> Result<Scene, ServiceError> {
        match self.db {
            Database::SQLite(db) => {
                let mut conn = db.get_connection().await?;
                let input = Scene {
                    id: scene.id,
                    name: scene.name,
                    create_time: scene.create_time,
                    update_time: Local::now(),
                };
                Ok(query_sqlite::update_scene(&mut conn, input).await?)
            }
        }
    }

    pub(crate) async fn delete_scene(&self, id: &str) -> Result<(), ServiceError> {
        match self.db {
            Database::SQLite(db) => {
                let mut conn = db.get_connection().await?;
                match query_sqlite::delete_scene(&mut conn, id).await {
                    Ok(_) => Ok(()),
                    Err(e) => Err(ServiceError::from(e)),
                }
            }
        }
    }

    pub(crate) async fn list_scenes(
        &self,
        filters: ListFilters,
    ) -> Result<(Vec<Scene>, bool), ServiceError> {
        let input = ListFilters {
            offset: filters.offset,
            limit: filters.limit + 1,
        };
        match self.db {
            Database::SQLite(db) => {
                let mut conn = db.get_connection().await?;
                match query_sqlite::list_scenes(&mut conn, input).await {
                    Ok(scenes) => {
                        let has_more = scenes.len() > filters.limit as usize;
                        Ok((
                            scenes.into_iter().take(filters.limit as usize).collect(),
                            has_more,
                        ))
                    }
                    Err(e) => Err(ServiceError::from(e)),
                }
            }
        }
    }
}
