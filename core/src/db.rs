use crate::db::migrations::{MigrationDef, Version};
use crate::db::sqlite::Migrator;
use std::str::Utf8Error;
use thiserror::Error;

pub mod migrations;
mod sqlite;

#[derive(Error, Debug)]
pub enum DbError {
    #[error("SQLite db error: {0}")]
    SQLite(#[from] sqlx::Error),
    #[error("failed to get migrator file: no file found")]
    MigratorNoFile,
    #[error("migration malformed: {0}")]
    MigrationMalformed(String),
    #[error("failed to parse UTF8: {0}")]
    UTF8ParseFailed(#[from] Utf8Error),
    #[error("failed to create database folder: {0}")]
    CreateDir(std::io::Error),
}

pub enum Database {
    SQLite(sqlite::Database),
}

/// Opens the database configured under `[db.<name>]` and migrates it up to
/// `schema_version` (default: the latest migration in `up`).
pub async fn build_db(
    name: &str,
    up: Vec<MigrationDef>,
    down: Vec<MigrationDef>,
    schema_version: Option<Version>,
) -> Result<Database, DbError> {
    let config = sqlite::DbConfig::read(name);
    let db = sqlite::build_db(name, config).await?;
    let migrator = Migrator::new(&db, up, down).await?;
    migrator.migrate_up(&db, schema_version).await?;
    Ok(Database::SQLite(db))
}

/// Like [`build_db`], but opens the SQLite file at `filename` instead of reading
/// `[db.<name>]` from the main config. Used by tests and tools.
pub async fn build_db_at(
    name: &str,
    filename: &str,
    up: Vec<MigrationDef>,
    down: Vec<MigrationDef>,
    schema_version: Option<Version>,
) -> Result<Database, DbError> {
    let db = sqlite::build_db(name, sqlite::DbConfig::at(filename)).await?;
    let migrator = Migrator::new(&db, up, down).await?;
    migrator.migrate_up(&db, schema_version).await?;
    Ok(Database::SQLite(db))
}
