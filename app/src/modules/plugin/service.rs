mod query_sqlite;

use super::config::PluginsConfig;
use super::manifest::{self, Manifest};
use super::package::{self, Limits, Package, PackageError};
use crate::modules::director::DirectorHandle;
use chrono::Local;
use flwrs_core::db::{Database, DbError};
use std::path::{Path, PathBuf};
use thiserror::Error;
use tokio::sync::Mutex;
use ulid::Ulid;

/// A registered plugin (a row of `plugins`).
#[derive(sqlx::FromRow, Debug, Clone)]
pub(crate) struct Plugin {
    pub id: String,
    pub name: String,
    pub version: String,
    #[sqlx(rename = "type")]
    pub plugin_type: String,
    pub description: Option<String>,
    pub manifest: String,
    pub launch_args_schema: Option<String>,
    pub in_schema: Option<String>,
    pub out_schema: Option<String>,
    pub install_path: String,
    pub archive_sha256: String,
    pub create_time: chrono::DateTime<Local>,
    pub update_time: chrono::DateTime<Local>,
}

impl Plugin {
    pub(crate) fn parsed_manifest(&self) -> Option<Manifest> {
        serde_json::from_str(&self.manifest).ok()
    }
}

pub(crate) struct NewPlugin {
    pub id: String,
    pub name: String,
    pub version: String,
    pub plugin_type: &'static str,
    pub description: Option<String>,
    pub manifest: String,
    pub launch_args_schema: Option<String>,
    pub in_schema: Option<String>,
    pub out_schema: Option<String>,
    pub install_path: String,
    pub archive_sha256: String,
}

impl NewPlugin {
    fn from_package(p: Package, install_path: &Path, archive_sha256: String) -> Self {
        Self {
            id: p.manifest.id.clone(),
            name: p.manifest.name.clone(),
            version: p.manifest.version.clone(),
            plugin_type: p.manifest.plugin_type.as_str(),
            description: p.manifest.description.clone(),
            manifest: p.manifest_json,
            launch_args_schema: p.launch_args_schema,
            in_schema: p.in_schema,
            out_schema: p.out_schema,
            install_path: install_path.to_string_lossy().into_owned(),
            archive_sha256,
        }
    }
}

#[derive(Error, Debug)]
pub(crate) enum ServiceError {
    #[error("plugin not found")]
    NotFound,
    #[error("plugin {0:?} is already registered")]
    AlreadyExists(String),
    #[error("invalid plugin package")]
    InvalidPackage(Vec<String>),
    #[error("plugin is used by scene nodes")]
    InUse(Vec<String>),
    #[error("the new package would make existing scene nodes invalid")]
    IncompatibleNodes(Vec<String>),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to execute query: {0}")]
    Query(sqlx::Error),
    #[error("failed to get connection: {0}")]
    Connection(#[from] DbError),
    #[error("internal error: {0}")]
    Internal(String),
}

impl From<PackageError> for ServiceError {
    fn from(e: PackageError) -> Self {
        match e {
            PackageError::Invalid(errors) => ServiceError::InvalidPackage(errors),
            PackageError::Io(e) => ServiceError::Io(e),
        }
    }
}

/// Plugin registry: installs, upgrades and removes plugin packages under
/// `config.dir`, and keeps the `plugins` table in sync with the folders.
pub(crate) struct Service {
    db: &'static Database,
    config: PluginsConfig,
    platform: String,
    /// Told to reload enabled scenes after a plugin upgrade.
    director: DirectorHandle,
    /// Serializes registry changes, so folder swaps and DB writes never interleave.
    lock: Mutex<()>,
}

/// Removes a file or folder when dropped (unless disarmed).
struct Cleanup(Option<PathBuf>);

impl Cleanup {
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = if path.is_dir() {
                std::fs::remove_dir_all(path)
            } else {
                std::fs::remove_file(path)
            };
        }
    }
}

impl Service {
    pub(crate) fn new(
        db: &'static Database,
        config: PluginsConfig,
        director: DirectorHandle,
    ) -> Self {
        Self::with_platform(db, config, manifest::current_platform(), director)
    }

    pub(crate) fn with_platform(
        db: &'static Database,
        config: PluginsConfig,
        platform: String,
        director: DirectorHandle,
    ) -> Self {
        Self {
            db,
            config,
            platform,
            director,
            lock: Mutex::new(()),
        }
    }

    #[cfg(test)]
    pub(crate) fn db(&self) -> &'static Database {
        self.db
    }

    pub(crate) fn config(&self) -> &PluginsConfig {
        &self.config
    }

    fn staging_dir(&self) -> PathBuf {
        self.config.dir.join(".staging")
    }

    fn trash_dir(&self) -> PathBuf {
        self.config.dir.join(".trash")
    }

    fn install_dir(&self, id: &str) -> PathBuf {
        self.config.dir.join(id)
    }

    fn limits(&self) -> Limits {
        Limits {
            max_unpacked_bytes: self.config.max_unpacked_bytes,
            max_entries: self.config.max_entries,
            max_compression_ratio: self.config.max_compression_ratio,
        }
    }

    /// Creates the plugin folders and removes leftovers of interrupted uploads.
    pub(crate) async fn prepare_dirs(&self) -> Result<(), ServiceError> {
        for dir in [self.staging_dir(), self.trash_dir()] {
            if dir.exists() {
                tokio::fs::remove_dir_all(&dir).await?;
            }
            tokio::fs::create_dir_all(&dir).await?;
        }
        Ok(())
    }

    /// A fresh path for an incoming upload, inside the staging folder (same
    /// filesystem as the install folders, so moves are atomic renames).
    pub(crate) fn new_upload_path(&self) -> PathBuf {
        self.staging_dir().join(format!("{}.zip", Ulid::generate()))
    }

    pub(crate) async fn get(&self, id: &str) -> Result<Plugin, ServiceError> {
        let mut conn = self.conn().await?;
        Ok(query_sqlite::get(&mut conn, id).await?)
    }

    pub(crate) async fn list(&self) -> Result<Vec<Plugin>, ServiceError> {
        let mut conn = self.conn().await?;
        Ok(query_sqlite::list(&mut conn).await?)
    }

    /// Whether the plugin's folder and executable are present on disk.
    pub(crate) fn is_installed(&self, plugin: &Plugin) -> bool {
        plugin
            .parsed_manifest()
            .is_some_and(|m| Path::new(&plugin.install_path).join(m.executable).is_file())
    }

    /// Start-up check: logs registered plugins whose files are missing, and
    /// folders that don't belong to any registered plugin. Returns the ids of
    /// broken plugins.
    pub(crate) async fn check_installations(&self) -> Result<Vec<String>, ServiceError> {
        let plugins = self.list().await?;
        let mut broken = vec![];
        for plugin in &plugins {
            if !self.is_installed(plugin) {
                log::error!(
                    "Plugin registry: plugin [{}] is registered but its files are missing at [{}]; nodes using it can't start",
                    plugin.id,
                    plugin.install_path
                );
                broken.push(plugin.id.clone());
            }
        }
        if let Ok(mut entries) = tokio::fs::read_dir(&self.config.dir).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                let name = entry.file_name().to_string_lossy().into_owned();
                if !name.starts_with('.') && !plugins.iter().any(|p| p.id == name) {
                    log::warn!(
                        "Plugin registry: folder [{}] doesn't belong to a registered plugin; ignoring it",
                        entry.path().display()
                    );
                }
            }
        }
        log::info!(
            "Plugin registry: {} plugin(s) registered, {} unavailable",
            plugins.len(),
            broken.len()
        );
        Ok(broken)
    }

    /// Validates the uploaded ZIP at `upload` and registers it. The upload file
    /// is removed afterwards in every case.
    pub(crate) async fn install(&self, upload: &Path) -> Result<Plugin, ServiceError> {
        let _upload_cleanup = Cleanup(Some(upload.to_path_buf()));
        let _guard = self.lock.lock().await;
        let (package, staged, sha) = self.unpack(upload).await?;
        let mut staged_cleanup = Cleanup(Some(staged.clone()));

        let id = package.manifest.id.clone();
        match self.get(&id).await {
            Ok(_) => return Err(ServiceError::AlreadyExists(id)),
            Err(ServiceError::NotFound) => {}
            Err(e) => return Err(e),
        }
        let target = self.install_dir(&id);
        if target.exists() {
            let parked = self.trash_dir().join(format!("{id}-{}", Ulid::generate()));
            log::warn!(
                "Plugin registry: folder [{}] exists but isn't registered; moving it aside",
                target.display()
            );
            tokio::fs::rename(&target, &parked).await?;
            let _ = tokio::fs::remove_dir_all(&parked).await;
        }
        tokio::fs::rename(&staged, &target).await?;
        staged_cleanup.disarm();
        let mut installed_cleanup = Cleanup(Some(target.clone()));

        let new = NewPlugin::from_package(package, &target, sha);
        let mut conn = self.conn().await?;
        let plugin = match query_sqlite::insert(&mut conn, &new).await {
            Ok(p) => p,
            Err(e) => {
                return Err(match ServiceError::from(e) {
                    ServiceError::AlreadyExists(_) => ServiceError::AlreadyExists(id),
                    other => other,
                });
            }
        };
        installed_cleanup.disarm();
        log::info!(
            "Plugin registry: installed [{}] {} ({})",
            plugin.id,
            plugin.version,
            plugin.plugin_type
        );
        Ok(plugin)
    }

    /// Replaces the package of plugin `id` with the uploaded ZIP. The id and
    /// type must stay the same, and every node using the plugin must still have
    /// valid `launch_args` under the new manifest.
    pub(crate) async fn upgrade(&self, id: &str, upload: &Path) -> Result<Plugin, ServiceError> {
        let _upload_cleanup = Cleanup(Some(upload.to_path_buf()));
        let _guard = self.lock.lock().await;
        let existing = self.get(id).await?;
        let (package, staged, sha) = self.unpack(upload).await?;
        let _staged_cleanup = Cleanup(Some(staged.clone()));

        let mut errors = vec![];
        if package.manifest.id != id {
            errors.push(format!(
                "manifest id {:?} doesn't match plugin {id:?}",
                package.manifest.id
            ));
        }
        if package.manifest.plugin_type.as_str() != existing.plugin_type {
            errors.push(format!(
                "type can't change on upgrade ({} → {})",
                existing.plugin_type,
                package.manifest.plugin_type.as_str()
            ));
        }
        if !errors.is_empty() {
            return Err(ServiceError::InvalidPackage(errors));
        }
        self.check_nodes_against(&package).await?;

        // Swap folders: current → trash, staged → current. Undo on failure.
        let target = PathBuf::from(&existing.install_path);
        let parked = self.trash_dir().join(format!("{id}-{}", Ulid::generate()));
        let had_folder = target.exists();
        if had_folder {
            tokio::fs::rename(&target, &parked).await?;
        }
        if let Err(e) = tokio::fs::rename(&staged, &target).await {
            if had_folder {
                let _ = tokio::fs::rename(&parked, &target).await;
            }
            return Err(e.into());
        }
        let new = NewPlugin::from_package(package, &target, sha);
        let mut conn = self.conn().await?;
        let plugin = match query_sqlite::update_package(&mut conn, &new).await {
            Ok(p) => p,
            Err(e) => {
                let _ = tokio::fs::remove_dir_all(&target).await;
                if had_folder {
                    let _ = tokio::fs::rename(&parked, &target).await;
                }
                return Err(e.into());
            }
        };
        query_sqlite::set_nodes_plugin_version(&mut conn, &plugin.id, &plugin.version).await?;
        if had_folder && let Err(e) = tokio::fs::remove_dir_all(&parked).await {
            log::warn!(
                "Plugin registry: couldn't remove old package [{}]: {e}",
                parked.display()
            );
        }
        let scenes = query_sqlite::enabled_scenes_using(&mut conn, id).await?;
        for scene_id in &scenes {
            self.director.request_reload(scene_id);
        }
        log::info!(
            "Plugin registry: upgraded [{}] {} → {}; reloading {} enabled scene(s) that use it",
            plugin.id,
            existing.version,
            plugin.version,
            scenes.len()
        );
        Ok(plugin)
    }

    /// Deregisters plugin `id` and removes its files. Fails while any scene
    /// node uses it.
    pub(crate) async fn delete(&self, id: &str) -> Result<(), ServiceError> {
        let _guard = self.lock.lock().await;
        let plugin = self.get(id).await?;
        let mut conn = self.conn().await?;
        // Nodes don't reference plugins with a foreign key (imported scenes
        // may name plugins this server lacks), so the check and the delete
        // share one write transaction: no node can be added in between.
        let mut tx = sqlx::Connection::begin_with(&mut *conn, "BEGIN IMMEDIATE").await?;
        let nodes = query_sqlite::nodes_using(&mut tx, id).await?;
        if !nodes.is_empty() {
            return Err(ServiceError::InUse(
                nodes.into_iter().map(|(node, _)| node).collect(),
            ));
        }
        if query_sqlite::delete(&mut tx, id).await? == 0 {
            return Err(ServiceError::NotFound);
        }
        tx.commit().await?;
        let folder = PathBuf::from(&plugin.install_path);
        if folder.exists() {
            let parked = self.trash_dir().join(format!("{id}-{}", Ulid::generate()));
            let removed = match tokio::fs::rename(&folder, &parked).await {
                Ok(()) => tokio::fs::remove_dir_all(&parked).await,
                Err(e) => Err(e),
            };
            if let Err(e) = removed {
                log::warn!(
                    "Plugin registry: deregistered [{id}] but couldn't remove [{}]: {e}",
                    folder.display()
                );
            }
        }
        log::info!("Plugin registry: removed [{id}]");
        Ok(())
    }

    /// Hashes and unpacks the upload into a fresh staging folder. Runs on a
    /// blocking thread (ZIP handling and hashing are synchronous).
    async fn unpack(&self, upload: &Path) -> Result<(Package, PathBuf, String), ServiceError> {
        let staged = self.staging_dir().join(Ulid::generate().to_string());
        let (upload, dest, limits, platform) = (
            upload.to_path_buf(),
            staged.clone(),
            self.limits(),
            self.platform.clone(),
        );
        let result =
            tokio::task::spawn_blocking(move || -> Result<(Package, String), ServiceError> {
                let sha = sha256::try_digest(upload.as_path())?;
                let package = package::extract_and_validate(&upload, &dest, &limits, &platform)?;
                Ok((package, sha))
            })
            .await
            .map_err(|e| ServiceError::Internal(format!("unpack task failed: {e}")))?;
        match result {
            Ok((package, sha)) => Ok((package, staged, sha)),
            Err(e) => {
                let _ = tokio::fs::remove_dir_all(&staged).await;
                Err(e)
            }
        }
    }

    /// Re-validates the `launch_args` of every node using the plugin against
    /// the new package's manifest.
    async fn check_nodes_against(&self, package: &Package) -> Result<(), ServiceError> {
        let mut conn = self.conn().await?;
        let nodes = query_sqlite::nodes_using(&mut conn, &package.manifest.id).await?;
        if nodes.is_empty() {
            return Ok(());
        }
        let validator = match &package.launch_args_schema {
            Some(schema) => {
                let value: serde_json::Value = serde_json::from_str(schema).map_err(|e| {
                    ServiceError::Internal(format!("stored schema isn't JSON: {e}"))
                })?;
                Some(
                    jsonschema::validator_for(&value)
                        .map_err(|e| ServiceError::Internal(e.to_string()))?,
                )
            }
            None => None,
        };
        let mut problems = vec![];
        for (node_id, config) in nodes {
            let config: serde_json::Value = serde_json::from_str(&config).unwrap_or_default();
            let args = config
                .get("launch_args")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            for error in manifest::validate_launch_args(
                validator.as_ref(),
                package.manifest.launch_args_style,
                &args,
            ) {
                problems.push(format!("node {node_id}: {error}"));
            }
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(ServiceError::IncompatibleNodes(problems))
        }
    }

    async fn conn(&self) -> Result<sqlx::pool::PoolConnection<sqlx::Sqlite>, ServiceError> {
        match self.db {
            Database::SQLite(db) => Ok(db.get_connection().await?),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::modules::plugin::package::tests::{PLATFORM, valid_package};

    pub(crate) struct Fixture {
        pub dir: tempfile::TempDir,
        pub service: &'static Service,
        pub director: tokio::sync::Mutex<
            tokio::sync::mpsc::Receiver<crate::modules::director::handle::DirectorCommand>,
        >,
    }

    pub(crate) async fn fixture(config: impl FnOnce(&mut PluginsConfig)) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::test_db(&dir.path().join("main.db")).await;
        let mut cfg = PluginsConfig {
            dir: dir.path().join("plugins"),
            ..PluginsConfig::default()
        };
        config(&mut cfg);
        let (director, rx) = DirectorHandle::channel();
        let service: &'static Service = Box::leak(Box::new(Service::with_platform(
            db,
            cfg,
            PLATFORM.to_string(),
            director,
        )));
        service.prepare_dirs().await.unwrap();
        Fixture {
            dir,
            service,
            director: tokio::sync::Mutex::new(rx),
        }
    }

    impl Fixture {
        pub(crate) fn upload(&self, id: &str, version: &str) -> PathBuf {
            let path = self.service.new_upload_path();
            valid_package(&path, id, version);
            path
        }

        async fn add_node(&self, node_id: &str, plugin_id: &str, config: &str) {
            let Database::SQLite(db) = self.service.db;
            let mut conn = db.get_connection().await.unwrap();
            sqlx::query("INSERT OR IGNORE INTO scenes (id, name) VALUES ('s1', 'scene')")
                .execute(&mut *conn)
                .await
                .unwrap();
            sqlx::query("INSERT INTO scene_nodes (id, scene_id, type, plugin_id, config) VALUES ($1, 's1', 'SINK', $2, $3)")
                .bind(node_id)
                .bind(plugin_id)
                .bind(config)
                .execute(&mut *conn)
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn install_get_list_and_files() {
        let f = fixture(|_| {}).await;
        let upload = f.upload("test-sink", "1.0.0");
        let plugin = f.service.install(&upload).await.unwrap();
        assert_eq!(
            (
                plugin.id.as_str(),
                plugin.plugin_type.as_str(),
                plugin.version.as_str()
            ),
            ("test-sink", "SINK", "1.0.0")
        );
        assert!(f.service.is_installed(&plugin));
        assert!(f.dir.path().join("plugins/test-sink/bin/run").is_file());
        assert!(!upload.exists(), "upload file removed");
        assert_eq!(f.service.list().await.unwrap().len(), 1);
        assert!(f.service.check_installations().await.unwrap().is_empty());
        // staging is left clean
        assert_eq!(
            std::fs::read_dir(f.dir.path().join("plugins/.staging"))
                .unwrap()
                .count(),
            0
        );
    }

    #[tokio::test]
    async fn duplicate_install_is_rejected_and_leaves_no_files() {
        let f = fixture(|_| {}).await;
        f.service
            .install(&f.upload("test-sink", "1.0.0"))
            .await
            .unwrap();
        let err = f
            .service
            .install(&f.upload("test-sink", "1.0.1"))
            .await
            .unwrap_err();
        assert!(matches!(err, ServiceError::AlreadyExists(id) if id == "test-sink"));
        assert_eq!(f.service.get("test-sink").await.unwrap().version, "1.0.0");
        assert_eq!(
            std::fs::read_dir(f.dir.path().join("plugins/.staging"))
                .unwrap()
                .count(),
            0
        );
    }

    #[tokio::test]
    async fn invalid_upload_is_rejected() {
        let f = fixture(|_| {}).await;
        let path = f.service.new_upload_path();
        std::fs::write(&path, b"not a zip").unwrap();
        let err = f.service.install(&path).await.unwrap_err();
        assert!(matches!(err, ServiceError::InvalidPackage(e) if e[0].contains("not a valid ZIP")));
        assert!(f.service.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn upgrade_replaces_the_package() {
        let f = fixture(|_| {}).await;
        let first = f
            .service
            .install(&f.upload("test-sink", "1.0.0"))
            .await
            .unwrap();
        f.add_node("n1", "test-sink", r#"{"launch_args": {"timeout": 5}}"#)
            .await;
        let upgraded = f
            .service
            .upgrade("test-sink", &f.upload("test-sink", "1.1.0"))
            .await
            .unwrap();
        assert_eq!(upgraded.version, "1.1.0");
        assert_eq!(upgraded.create_time, first.create_time);
        assert!(f.service.is_installed(&upgraded));
        assert_eq!(
            std::fs::read_dir(f.dir.path().join("plugins/.trash"))
                .unwrap()
                .count(),
            0
        );
    }

    #[tokio::test]
    async fn upgrade_reloads_enabled_scenes_using_the_plugin() {
        use crate::modules::director::handle::DirectorCommand;
        let f = fixture(|_| {}).await;
        f.service
            .install(&f.upload("test-sink", "1.0.0"))
            .await
            .unwrap();
        f.add_node("n1", "test-sink", "{}").await; // scene s1, disabled
        f.service
            .upgrade("test-sink", &f.upload("test-sink", "1.0.1"))
            .await
            .unwrap();
        assert!(
            f.director.lock().await.try_recv().is_err(),
            "disabled scene isn't reloaded"
        );

        let Database::SQLite(db) = f.service.db;
        let mut conn = db.get_connection().await.unwrap();
        sqlx::query("UPDATE scenes SET enabled = TRUE")
            .execute(&mut *conn)
            .await
            .unwrap();
        f.service
            .upgrade("test-sink", &f.upload("test-sink", "1.0.2"))
            .await
            .unwrap();
        match f.director.lock().await.try_recv() {
            Ok(DirectorCommand::Reload { scene_id, .. }) => assert_eq!(scene_id, "s1"),
            other => panic!("expected a reload, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn upgrade_blocked_by_invalid_node_or_id_change() {
        let f = fixture(|_| {}).await;
        f.service
            .install(&f.upload("test-sink", "1.0.0"))
            .await
            .unwrap();
        // valid under no schema, but the package's schema rejects unknown keys
        f.add_node("n1", "test-sink", r#"{"launch_args": {"unknown": 1}}"#)
            .await;
        let err = f
            .service
            .upgrade("test-sink", &f.upload("test-sink", "2.0.0"))
            .await
            .unwrap_err();
        match err {
            ServiceError::IncompatibleNodes(problems) => {
                assert!(
                    problems.iter().all(|p| p.starts_with("node n1:")),
                    "{problems:?}"
                )
            }
            other => panic!("expected IncompatibleNodes, got {other:?}"),
        }
        assert_eq!(f.service.get("test-sink").await.unwrap().version, "1.0.0");

        let err = f
            .service
            .upgrade("test-sink", &f.upload("other-id", "2.0.0"))
            .await
            .unwrap_err();
        assert!(matches!(err, ServiceError::InvalidPackage(e) if e[0].contains("doesn't match")));
        assert!(matches!(
            f.service
                .upgrade("missing", &f.upload("missing", "1.0.0"))
                .await
                .unwrap_err(),
            ServiceError::NotFound
        ));
    }

    #[tokio::test]
    async fn delete_blocked_while_used_then_allowed() {
        let f = fixture(|_| {}).await;
        f.service
            .install(&f.upload("test-sink", "1.0.0"))
            .await
            .unwrap();
        f.add_node("n1", "test-sink", "{}").await;
        let err = f.service.delete("test-sink").await.unwrap_err();
        assert!(matches!(err, ServiceError::InUse(nodes) if nodes == ["n1"]));

        let Database::SQLite(db) = f.service.db;
        let mut conn = db.get_connection().await.unwrap();
        sqlx::query("DELETE FROM scene_nodes")
            .execute(&mut *conn)
            .await
            .unwrap();
        f.service.delete("test-sink").await.unwrap();
        assert!(matches!(
            f.service.get("test-sink").await.unwrap_err(),
            ServiceError::NotFound
        ));
        assert!(!f.dir.path().join("plugins/test-sink").exists());
        assert!(matches!(
            f.service.delete("test-sink").await.unwrap_err(),
            ServiceError::NotFound
        ));
    }

    #[tokio::test]
    async fn startup_check_reports_missing_files() {
        let f = fixture(|_| {}).await;
        let plugin = f
            .service
            .install(&f.upload("test-sink", "1.0.0"))
            .await
            .unwrap();
        std::fs::remove_dir_all(&plugin.install_path).unwrap();
        assert_eq!(
            f.service.check_installations().await.unwrap(),
            ["test-sink"]
        );
    }
}
