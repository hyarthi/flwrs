use crate::config;
use crate::config::main_config;
use crate::db::DbError;
use crate::db::migrations::{MigrationDef, Version};
use chrono::{DateTime, Local};
use rust_embed::Embed;
use serde::Deserialize;
use sqlx::pool::PoolConnection;
use sqlx::sqlite::{SqliteAutoVacuum, SqliteConnectOptions};
use sqlx::{AssertSqlSafe, Connection, Executor, FromRow, Sqlite, SqlitePool, Type};
use std::str;

#[derive(Deserialize, Default)]
pub struct DbConfig {
    filename: Option<String>,
}

impl DbConfig {
    /// A config for a database file at `filename`, bypassing the main config.
    pub(crate) fn at(filename: &str) -> Self {
        Self {
            filename: Some(filename.to_string()),
        }
    }

    /// Reads `[db.<name>]` from the main config. The file defaults to `<name>.db`.
    pub fn read(name: &str) -> Self {
        let config: DbConfig =
            config::read_struct(main_config(), &["db".to_string(), name.to_string()])
                .unwrap_or_default();
        Self {
            filename: Some(config.filename.unwrap_or_else(|| format!("{name}.db"))),
        }
    }
}

pub struct Database {
    id: String,
    pub delegate: SqlitePool,
}

impl Database {
    pub async fn get_connection(&self) -> Result<PoolConnection<Sqlite>, DbError> {
        let conn = self.delegate.acquire().await?;
        Ok(conn)
    }
}

pub async fn build_db(name: &str, config: DbConfig) -> Result<Database, DbError> {
    let filename = config.filename.unwrap_or_else(|| format!("{name}.db"));
    // SQLite creates the file but not its folder (e.g. `.data/main.db` on a fresh checkout).
    if let Some(parent) = std::path::Path::new(&filename).parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(DbError::CreateDir)?;
    }
    let db = SqlitePool::connect_with(
        SqliteConnectOptions::new()
            .filename(filename)
            .create_if_missing(true)
            .auto_vacuum(SqliteAutoVacuum::Incremental),
    )
    .await?;

    Ok(Database {
        id: name.to_string(),
        delegate: db,
    })
}

#[derive(Embed)]
#[folder = "resources/migrations/sqlite/"]
struct MigrationDefs;

#[derive(Type, Eq, PartialEq, Debug)]
#[repr(u32)]
enum MigrationState {
    None = 0,
    Started = 1,
    Done = 2,
    Error = 3,
    Rollback = 4,
}

/// Row of the `migrations` tracking table.
#[derive(sqlx::FromRow)]
#[allow(dead_code)] // full row mapping; not every column is read yet
struct Migration {
    id: u32,
    version_major: u32,
    version_minor: u32,
    version_patch: u32,
    build_number: u32,
    file_name: String,
    file_hash: String,
    start_time: DateTime<Local>,
    finish_time: Option<DateTime<Local>>,
    state: MigrationState,
}

/// Applies versioned migrations and records them in the `migrations` table.
///
/// Each migration runs in its own transaction, so a failed migration leaves no
/// partial changes. If an upgrade fails, the migrations already applied in that
/// run are rolled back with their `down` scripts.
pub(crate) struct Migrator {
    /// Sorted ascending by version.
    up_migrations: Vec<MigrationDef>,
    /// Sorted descending by version.
    down_migrations: Vec<MigrationDef>,
    max_version: Version,
}

impl Migrator {
    pub(crate) async fn new(
        db: &Database,
        mut up_migrations: Vec<MigrationDef>,
        mut down_migrations: Vec<MigrationDef>,
    ) -> Result<Self, DbError> {
        let database = &db.id;
        log::debug!("Creating new database migrator for [{database}].");

        let migrations_file = MigrationDefs::get("migrator.sql").ok_or(DbError::MigratorNoFile)?;
        log::trace!("Migrator [{database}]: establishing base migrator tables.");
        db.get_connection()
            .await?
            // Embedded at compile time, so trusted.
            .execute(sqlx::raw_sql(AssertSqlSafe(
                str::from_utf8(migrations_file.data.as_ref())?.to_string(),
            )))
            .await?;

        up_migrations.sort_by_key(MigrationDef::version);
        down_migrations.sort_by_key(|m| std::cmp::Reverse(m.version()));
        if log::log_enabled!(log::Level::Trace) {
            for migration in &up_migrations {
                log::trace!(
                    "Migrator [{database}]: UP migration found {}",
                    migration.version()
                );
            }
            for migration in &down_migrations {
                log::trace!(
                    "Migrator [{database}]: DOWN migration found {}",
                    migration.version()
                );
            }
        }

        let max_version = up_migrations
            .last()
            .map(MigrationDef::version)
            .unwrap_or_default();
        log::debug!("Migrator for database [{database}] created.");

        Ok(Self {
            up_migrations,
            down_migrations,
            max_version,
        })
    }

    /// Highest version whose migration completed successfully, or `0.0.0-0`.
    async fn current_version(&self, db: &Database) -> Result<Version, DbError> {
        let mut conn = db.get_connection().await?;
        let row = conn
            .fetch_optional(
                sqlx::query_as::<_, Migration>(
                    "SELECT * FROM migrations \
                     WHERE state = $1 \
                     ORDER BY version_major DESC, version_minor DESC, version_patch DESC, build_number DESC \
                     LIMIT 1",
                )
                .bind(MigrationState::Done),
            )
            .await?;
        Ok(match row {
            Some(row) => Version::from(&Migration::from_row(&row)?),
            None => Version::default(),
        })
    }

    /// Migrates up to `target_version` (default: the latest available).
    pub(crate) async fn migrate_up(
        &self,
        db: &Database,
        target_version: Option<Version>,
    ) -> Result<(), DbError> {
        let database = &db.id;
        let target = target_version.unwrap_or(self.max_version);
        log::debug!("Migrator [{database}]: migrating UP to [{target}].");

        // No locking needed yet: one process owns one SQLite DB.
        let base_version = self.current_version(db).await?;
        log::trace!("Migrator [{database}]: current schema version: [{base_version}]");

        let pending: Vec<&MigrationDef> = self
            .up_migrations
            .iter()
            .filter(|m| m.version() > base_version && m.version() <= target)
            .collect();
        if pending.is_empty() {
            log::debug!("Migrator [{database}]: DB schema is up to date.");
            return Ok(());
        }

        for migration in pending {
            let version = migration.version();
            log::trace!("Migrator [{database}]: migrating UP to [{version}].");
            if let Err(e) = self
                .run_migration(db, migration, MigrationState::Done)
                .await
            {
                log::error!("Migrator [{database}]: failed to migrate to version [{version}]: {e}");
                log::warn!(
                    "Migrator [{database}]: rolling back to initial version: [{base_version}]."
                );
                if let Err(rollback_err) = self.migrate_down(db, base_version).await {
                    log::error!("Migrator [{database}]: rollback failed: {rollback_err}");
                }
                return Err(e);
            }
            log::trace!("Migrator [{database}]: migration UP to [{version}]: success.");
        }

        Ok(())
    }

    /// Rolls back completed migrations newer than `target_version`, newest first.
    pub(crate) async fn migrate_down(
        &self,
        db: &Database,
        target_version: Version,
    ) -> Result<(), DbError> {
        let database = &db.id;
        log::debug!("Migrator [{database}]: migrating DOWN to [{target_version}].");

        let current = self.current_version(db).await?;
        log::trace!("Migrator [{database}]: current schema version: [{current}]");

        let pending: Vec<&MigrationDef> = self
            .down_migrations
            .iter()
            .filter(|m| m.version() > target_version && m.version() <= current)
            .collect();
        if pending.is_empty() {
            log::debug!("Migrator [{database}]: nothing to roll back.");
            return Ok(());
        }

        for migration in pending {
            let version = migration.version();
            log::trace!("Migrator [{database}]: migrating DOWN from [{version}].");
            if let Err(e) = self
                .run_migration(db, migration, MigrationState::Rollback)
                .await
            {
                log::error!(
                    "Migrator [{database}]: failed to roll back version [{version}]: {e}. \
                     Cancelling rollback; the database stays at [{version}]."
                );
                return Err(e);
            }
            log::trace!("Migrator [{database}]: migration DOWN from [{version}]: success.");
        }

        Ok(())
    }

    /// Runs one script in a transaction and records the outcome.
    async fn run_migration(
        &self,
        db: &Database,
        migration_def: &MigrationDef,
        new_state: MigrationState,
    ) -> Result<(), DbError> {
        let database = &db.id;
        let version = migration_def.version();
        let mut conn = db.get_connection().await?;

        log::trace!("Migrator [{database}]: [{version}]: recording start.");
        let row = conn
            .fetch_one(
                sqlx::query::<Sqlite>(
                    "INSERT INTO migrations (\
                        version_major, version_minor, version_patch, build_number, \
                        file_name, file_hash, start_time, state) \
                     VALUES ( $1, $2, $3, $4, $5, $6, $7, $8 ) \
                     ON CONFLICT (version_major, version_minor, version_patch, build_number) \
                     DO UPDATE SET file_name = $5, file_hash = $6, start_time = $7, finish_time = NULL, state = $8 \
                     RETURNING *",
                )
                .bind(version.major)
                .bind(version.minor)
                .bind(version.patch)
                .bind(version.build_number)
                .bind(migration_def.file_name.clone())
                .bind(migration_def.file_hash.clone())
                .bind(Local::now())
                .bind(MigrationState::Started),
            )
            .await?;
        let migration = Migration::from_row(&row)?;
        let script = str::from_utf8(migration_def.data.as_ref())?;

        log::trace!("Migrator [{database}]: [{version}]: running script.");
        let mut tx = conn.begin().await?;
        let applied = async {
            // Migration scripts are embedded in the binary, so they're trusted.
            tx.execute(sqlx::raw_sql(AssertSqlSafe(script.to_string())))
                .await?;
            tx.execute(
                sqlx::query::<Sqlite>(
                    "UPDATE migrations SET state = $1, finish_time = $2 WHERE id = $3",
                )
                .bind(new_state)
                .bind(Local::now())
                .bind(migration.id),
            )
            .await?;
            Ok::<_, sqlx::Error>(())
        }
        .await;

        match applied {
            Ok(()) => {
                tx.commit().await?;
                Ok(())
            }
            Err(e) => {
                tx.rollback().await?;
                conn.execute(
                    sqlx::query::<Sqlite>(
                        "UPDATE migrations SET state = $1, finish_time = $2 WHERE id = $3",
                    )
                    .bind(MigrationState::Error)
                    .bind(Local::now())
                    .bind(migration.id),
                )
                .await?;
                Err(DbError::from(e))
            }
        }
    }
}

impl From<&Migration> for Version {
    fn from(value: &Migration) -> Self {
        Self {
            major: value.version_major,
            minor: value.version_minor,
            patch: value.version_patch,
            build_number: value.build_number,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(name: &str, sql: &'static str) -> MigrationDef {
        MigrationDef::from_bytes(name.to_string(), sql.as_bytes()).unwrap()
    }

    async fn temp_db(dir: &tempfile::TempDir) -> Database {
        let path = dir.path().join("test.db");
        build_db(
            "test",
            DbConfig {
                filename: Some(path.to_string_lossy().into_owned()),
            },
        )
        .await
        .unwrap()
    }

    async fn tables(db: &Database) -> Vec<String> {
        let mut conn = db.get_connection().await.unwrap();
        sqlx::query_scalar::<_, String>(
            "SELECT name FROM sqlite_master WHERE type = 'table' \
             AND name NOT IN ('migrations', 'sqlite_sequence') ORDER BY name",
        )
        .fetch_all(&mut *conn)
        .await
        .unwrap()
    }

    fn ups() -> Vec<MigrationDef> {
        // Deliberately unsorted, and 0.10 vs 0.9 exercises version ordering:
        // b is created from a, so it fails if run first.
        vec![
            def("0.10.0-1.up.sql", "CREATE TABLE b AS SELECT * FROM a;"),
            def("0.9.0-1.up.sql", "CREATE TABLE a (id INTEGER);"),
        ]
    }

    fn downs() -> Vec<MigrationDef> {
        vec![
            def("0.9.0-1.down.sql", "DROP TABLE a;"),
            def("0.10.0-1.down.sql", "DROP TABLE b;"),
        ]
    }

    #[tokio::test]
    async fn creates_missing_parent_folders() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/deeper/test.db");
        build_db(
            "test",
            DbConfig {
                filename: Some(path.to_string_lossy().into_owned()),
            },
        )
        .await
        .unwrap();
        assert!(path.exists());
    }

    #[tokio::test]
    async fn migrates_up_in_version_order_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let db = temp_db(&dir).await;
        let migrator = Migrator::new(&db, ups(), downs()).await.unwrap();
        migrator.migrate_up(&db, None).await.unwrap();
        assert_eq!(tables(&db).await, vec!["a", "b"]);
        assert_eq!(
            migrator.current_version(&db).await.unwrap(),
            Version::new(0, 10, 0, 1)
        );

        // running again is a no-op
        migrator.migrate_up(&db, None).await.unwrap();
        assert_eq!(tables(&db).await, vec!["a", "b"]);
    }

    #[tokio::test]
    async fn migrates_up_to_target_then_down() {
        let dir = tempfile::tempdir().unwrap();
        let db = temp_db(&dir).await;
        let migrator = Migrator::new(&db, ups(), downs()).await.unwrap();

        migrator
            .migrate_up(&db, Some(Version::new(0, 9, 0, 1)))
            .await
            .unwrap();
        assert_eq!(tables(&db).await, vec!["a"]);

        migrator.migrate_up(&db, None).await.unwrap();
        assert_eq!(tables(&db).await, vec!["a", "b"]);

        migrator
            .migrate_down(&db, Version::new(0, 9, 0, 1))
            .await
            .unwrap();
        assert_eq!(tables(&db).await, vec!["a"]);
        assert_eq!(
            migrator.current_version(&db).await.unwrap(),
            Version::new(0, 9, 0, 1)
        );

        migrator
            .migrate_down(&db, Version::default())
            .await
            .unwrap();
        assert!(tables(&db).await.is_empty());
    }

    #[tokio::test]
    async fn failed_migration_rolls_back_the_whole_run() {
        let dir = tempfile::tempdir().unwrap();
        let db = temp_db(&dir).await;
        let mut up = ups();
        up.push(def(
            "1.0.0-1.up.sql",
            "CREATE TABLE c (id INTEGER); THIS IS NOT SQL;",
        ));
        let mut down = downs();
        down.push(def("1.0.0-1.down.sql", "DROP TABLE c;"));

        let migrator = Migrator::new(&db, up, down).await.unwrap();
        assert!(migrator.migrate_up(&db, None).await.is_err());
        // c never committed (transaction); a and b were rolled back by their downs
        assert!(tables(&db).await.is_empty());
        assert_eq!(
            migrator.current_version(&db).await.unwrap(),
            Version::default()
        );
    }

    #[tokio::test]
    async fn failed_migration_keeps_earlier_runs() {
        let dir = tempfile::tempdir().unwrap();
        let db = temp_db(&dir).await;
        Migrator::new(&db, ups(), downs())
            .await
            .unwrap()
            .migrate_up(&db, None)
            .await
            .unwrap();

        let mut up = ups();
        up.push(def("1.0.0-1.up.sql", "NOT SQL;"));
        let migrator = Migrator::new(&db, up, downs()).await.unwrap();
        assert!(migrator.migrate_up(&db, None).await.is_err());
        // only this run is rolled back; the schema from the earlier run stays
        assert_eq!(tables(&db).await, vec!["a", "b"]);
        assert_eq!(
            migrator.current_version(&db).await.unwrap(),
            Version::new(0, 10, 0, 1)
        );
    }
}
