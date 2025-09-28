use crate::config;
use crate::config::main_config;
use crate::db::migrations::{MigrationDef, Version};
use crate::db::DbError;
use chrono::{DateTime, Local};
use rust_embed::Embed;
use serde::Deserialize;
use sqlx::pool::PoolConnection;
use sqlx::sqlite::{SqliteAutoVacuum, SqliteConnectOptions};
use sqlx::{Executor, FromRow, Sqlite, SqlitePool, Type};
use std::str;

#[derive(Deserialize)]
pub struct DbConfig {
    #[serde(alias = "filename")]
    filename: Option<String>,
}

impl DbConfig {
    pub fn read(name: &str) -> Self {
        let main_config = main_config();
        config::read_struct(main_config, &["db".to_string(), name.to_string()])
            .unwrap_or(DbConfig {
                filename: Some(format!("{}.db", name).to_string())
            })
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

pub async fn build_main_db(config: DbConfig) -> Result<Database, DbError> {
    let db = SqlitePool::connect_with(
        SqliteConnectOptions::new()
            .filename(config.filename.unwrap())
            .create_if_missing(true)
            .auto_vacuum(SqliteAutoVacuum::Incremental)
    ).await?;

    Ok(Database {
        id: "main".to_string(),
        delegate: db,
    })
}

#[derive(Embed)]
#[folder = "resources/migrations/sqlite/"]
struct MigrationDefs;

#[derive(Type, Eq, PartialEq)]
#[repr(u32)]
enum MigrationState {
    None = 0,
    Started = 1,
    Done = 2,
    Error = 3,
    Rollback = 4,
}

#[derive(sqlx::FromRow)]
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

pub(crate) struct Migrator {
    up_migrations: Vec<MigrationDef>,
    down_migrations: Vec<MigrationDef>,
    max_version: Version,
}

impl Migrator {
    pub(crate) async fn new(
        db: &Database,
        up: Vec<MigrationDef>,
        down: Vec<MigrationDef>,
    ) -> Result<Self, DbError> {
        log::debug!("Creating new database migrator for [{database}].", database = db.id.clone());

        log::trace!("Migrator [{database}]: getting migrator base DB structure file.", database = db.id.clone());
        let migrations_file = match MigrationDefs::get("migrator.sql") {
            Some(f) => f,
            None => return Err(DbError::MigratorNoFile),
        };
        log::trace!("Migrator [{database}]: establishing base migrator tables.", database = db.id.clone());
        db.get_connection().await?.execute(sqlx::raw_sql(str::from_utf8(migrations_file.data.as_ref())?)).await?;

        log::trace!("Migrator [{database}]: pre-sorting migrations.", database = db.id.clone());
        let mut up_migrations = up.clone();
        up_migrations.sort_by(|one, two| {
            if one.version_major != two.version_major {
                one.version_major.cmp(&two.version_major)
            } else if one.version_minor != two.version_minor {
                one.version_minor.cmp(&two.version_minor)
            } else if one.version_patch != two.version_patch {
                one.version_patch.cmp(&two.version_patch)
            } else {
                one.build_number.cmp(&two.build_number)
            }
        });
        log::trace!("Migrator [{database}]: sorted list of UP migrations:", database = db.id.clone());
        // TODO make this loop optional based on log level
        up_migrations.iter().for_each(|migration| {
            log::trace!(
                "Migrator [{database}]: schema version found {migration_version}", 
                database = db.id.clone(), 
                migration_version = migration.version()
            );
        });
        let mut down_migrations = down.clone();
        down_migrations.sort_by(|one, two| {
            if one.version_major != two.version_major {
                two.version_major.cmp(&one.version_major)
            } else if one.version_minor != two.version_minor {
                two.version_minor.cmp(&one.version_minor)
            } else if one.version_patch != two.version_patch {
                two.version_patch.cmp(&one.version_patch)
            } else {
                two.build_number.cmp(&one.build_number)
            }
        });
        log::trace!("Migrator [{database}]: sorted list of DOWN migrations:", database = db.id.clone());
        // TODO make this loop optional based on log level
        down_migrations.iter().for_each(|migration| {
            log::trace!(
                "Migrator [{database}]: schema version found {migration_version}", 
                database = db.id.clone(), 
                migration_version = migration.version()
            );
        });

        log::debug!("Migrator for database [{database}] created.", database = db.id.clone());
        let max_version = up_migrations
            .last()
            .map(|migration| { migration.version() })
            .unwrap_or(Version::new(0, 0, 0, 0));

        Ok(Self {
            up_migrations,
            down_migrations,
            max_version,
        })
    }

    pub(crate) async fn migrate_up(&self, db: &Database, target_version: Option<Version>) -> Result<(), DbError> {
        log::debug!("Migrator [{database}]: migrating UP.", database = db.id.clone());
        let mut conn = db.get_connection().await?;

        // We don't need worry about locking the DB just yet, 
        // because initially only one process will work with
        // one SQLite DB.

        log::trace!("Migrator [{database}]: grabbing current DB schema version.", database = db.id.clone());
        let current_level = match conn.fetch_optional(
            sqlx::query_as::<_, Migration>(
                "SELECT * FROM migrations \
                    WHERE state = $1 \
                    ORDER BY version_major DESC, version_minor DESC, version_patch DESC, build_number DESC \
                    LIMIT 1"
            ).bind(MigrationState::Done)
        )
            .await? {
            Some(row) => Migration::from_row(&row)?,
            None => Migration {
                id: 0,
                file_name: "".to_string(),
                file_hash: "".to_string(),
                version_major: 0,
                version_minor: 0,
                version_patch: 0,
                build_number: 0,
                start_time: DateTime::default(),
                finish_time: None,
                state: MigrationState::None,
            }
        };
        let base_version = Version::from(&current_level);
        log::trace!(
            "Migrator [{database}]: current schema version: [{version}]", 
            database = db.id.clone(),
            version = base_version.clone(),
        );

        let migrations: Vec<MigrationDef> = self.up_migrations
            .iter()
            .filter(|&migration| { Version::from(migration).is_after(&base_version) })
            .filter(|&migration| {
                let version = Version::from(migration);
                version.is_before(&target_version.unwrap_or(self.max_version)) || version == target_version.unwrap_or(self.max_version)
            })
            .map(|migration| { migration.clone() })
            .collect();

        if migrations.is_empty() {
            log::debug!("Migrator [{database}]: DB schema is up to date.", database = db.id.clone());
            return Ok(());
        }

        for migration in migrations.iter() {
            log::trace!(
                "Migrator [{database}]: migrating UP to [{version}].",
                database = db.id.clone(),
                version = migration.version()
            );
            match self.run_migration(db, migration, MigrationState::Done).await {
                Ok(_) => {
                    log::trace!(
                        "Migrator [{database}]: migration UP to [{version}]: success.",
                        database = db.id.clone(),
                        version = migration.version()
                    );
                }
                Err(e) => {
                    log::error!(
                        "Migrator [{database}]: failed to migrate database [{database}] to version [{version}]: {e}",
                        database = db.id.clone(), 
                        version = target_version.unwrap_or(self.max_version)
                    );
                    log::warn!(
                        "Migrator [{database}]: rolling back to initial version: [{version}].",
                        database = db.id.clone(),
                        version = base_version.clone(),
                    );
                    self.migrate_down(db, base_version.clone()).await?
                }
            }
        }

        Ok(())
    }

    async fn run_migration(
        &self,
        db: &Database,
        migration_def: &MigrationDef,
        new_state: MigrationState,
    ) -> Result<(), DbError> {
        let action = match new_state {
            MigrationState::Done => "migrating",
            MigrationState::Rollback => "rolling back",
            _ => "<undefined operation>"
        };

        log::trace!(
            "Migrator [{database}]: {action} to [{version}]: grabbing connection.",
            database = db.id.clone(),
            version = migration_def.version(),
        );
        let mut conn = db.get_connection().await?;

        log::trace!(
            "Migrator [{database}]: {action} to [{version}]: initialising migration.",
            database = db.id.clone(),
            version = migration_def.version(),
        );
        let row = conn.fetch_one(
            sqlx::query::<Sqlite>("INSERT INTO migrations (\
            version_major, \
            version_minor, \
            version_patch, \
            build_number, \
            file_name, \
            file_hash, \
            start_time, \
            state) \
        VALUES ( $1, $2, $3, $4, $5, $6, $7, $8 ) \
        ON CONFLICT (version_major, version_minor, version_patch, build_number) \
        DO UPDATE \
        SET file_name = $5, file_hash = $6, start_time = $7, state = $8 \
        RETURNING *")
                .bind(migration_def.version_major)
                .bind(migration_def.version_minor)
                .bind(migration_def.version_patch)
                .bind(migration_def.build_number)
                .bind(migration_def.file_name.clone())
                .bind(migration_def.file_hash.clone())
                .bind(Local::now())
                .bind(MigrationState::Started)
        ).await?;

        let migration = Migration::from_row(&row)?;
        log::trace!(
            "Migrator [{database}]: {action} to [{version}]: running migration.",
            database = db.id.clone(),
            version = migration_def.version(),
        );
        match conn.execute(sqlx::raw_sql(str::from_utf8(migration_def.file.data.as_ref())?)).await {
            Ok(_) => (),
            Err(e) => {
                conn.execute(
                    sqlx::query::<Sqlite>("UPDATE migrations \
                    SET state = $1, finish_time = $2 \
                    WHERE id = $3")
                        .bind(MigrationState::Error)
                        .bind(Local::now())
                        .bind(migration.id)
                ).await?;

                return Err(DbError::from(e));
            }
        }

        log::trace!(
            "Migrator [{database}]: {action} to [{version}]: updating migration metadata.",
            database = db.id.clone(),
            version = migration_def.version(),
        );
        conn.execute(
            sqlx::query::<Sqlite>("UPDATE migrations \
            SET state = $1, finish_time = $2 \
            WHERE id = $3")
                .bind(new_state)
                .bind(Local::now())
                .bind(migration.id)
        ).await?;

        Ok(())
    }

    pub(crate) async fn migrate_down(&self, db: &Database, target_version: Version) -> Result<(), DbError> {
        log::debug!("Migrator [{database}]: migrating DOWN.", database = db.id.clone());
        let mut conn = db.get_connection().await?;

        log::trace!("Migrator [{database}]: grabbing current DB schema version.", database = db.id.clone());
        let current_level = match conn.fetch_optional(
            sqlx::query_as::<_, Migration>(
                "SELECT * FROM migrations \
                    WHERE state IN ($1, $2, $3) \
                    ORDER BY version_major ASC, version_minor ASC, version_patch ASC, build_number ASC \
                    LIMIT 1"
            ).bind(MigrationState::Started)
                .bind(MigrationState::Done)
                .bind(MigrationState::Error)
        )
            .await? {
            Some(row) => Migration::from_row(&row)?,
            None => Migration {
                id: 0,
                file_name: "".to_string(),
                file_hash: "".to_string(),
                version_major: 0,
                version_minor: 0,
                version_patch: 0,
                build_number: 0,
                start_time: DateTime::default(),
                finish_time: None,
                state: MigrationState::None,
            }
        };
        let base_version = Version::from(&current_level);
        log::trace!(
            "Migrator [{database}]: current schema version: [{version}]", 
            database = db.id.clone(),
            version = base_version.clone(),
        );

        // grab migration files
        let migrations: Vec<MigrationDef> = self.down_migrations
            .iter()
            .filter(|&migration| {
                Version::from(migration).is_before(&base_version) || current_level.state == MigrationState::Error
            })
            .filter(|&migration| {
                let version = Version::from(migration);
                version.is_after(&target_version)
            })
            .map(|migration| { migration.clone() })
            .collect();

        if migrations.is_empty() {
            log::debug!("Migrator [{database}]: DB schema is up to date.", database = db.id.clone());
            return Ok(());
        }

        for migration in migrations.iter() {
            log::trace!(
                "Migrator [{database}]: migrating DOWN to [{version}].",
                database = db.id.clone(),
                version = migration.version()
            );
            match self.run_migration(db, migration, MigrationState::Rollback).await {
                Ok(_) => {
                    log::trace!(
                        "Migrator [{database}]: migration DOWN to [{version}]: success.",
                        database = db.id.clone(),
                        version = migration.version()
                    );
                }
                Err(e) => {
                    log::error!(
                        "Migrator [{database}]: failed to roll back database [{database}] to version [{version}]: {e}",
                        database = db.id.clone(), 
                        version = target_version.clone()
                    );
                    log::warn!(
                        "Migrator [{database}]: Cancelling rollback. This may leave database in inconsistent state.", 
                        database = db.id.clone()
                    )
                }
            }
        }

        Ok(())
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