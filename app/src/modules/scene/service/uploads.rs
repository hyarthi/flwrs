//! Files uploaded to nodes (manifest v2 `"source": "upload"` launch args).
//!
//! The bytes are stored once per name and content in `[director] files_dir`
//! as `<stem>_<sha256><.ext>` (readable names; the same content under two
//! names is stored twice). `scene_node_uploads` records which node has which;
//! a stored file goes once no row names it. A node's `config/` links to its
//! uploads when it launches (see `supervisor::workdir`).

use super::*;
use crate::modules::supervisor::workdir;
use flwrs_plugin::package::launch::check_file_name;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Where nodes' files go.
#[derive(Clone, Debug)]
pub(crate) struct NodeFiles {
    /// `[director] nodes_dir`: local nodes' folders.
    pub nodes_dir: PathBuf,
    /// `[director] files_dir`: uploaded files.
    pub files_dir: PathBuf,
    /// `[scenes] max_upload_bytes`.
    pub max_upload_bytes: u64,
    /// `[scenes] max_import_bytes` / `max_import_entries`: scene imports.
    pub max_import_bytes: u64,
    pub max_import_entries: usize,
}

/// A file uploaded to a node.
#[derive(sqlx::FromRow, Debug, Clone, PartialEq, Eq)]
pub(crate) struct NodeUpload {
    pub node_id: String,
    pub name: String,
    /// Lowercase hex SHA-256 of the content.
    pub sha256: String,
    pub size: i64,
    pub create_time: chrono::DateTime<Local>,
}

impl NodeUpload {
    /// Its file's name in `files_dir`.
    pub(crate) fn stored_name(&self) -> String {
        stored_name(&self.name, &self.sha256)
    }
}

/// `resnet50.onnx` + hash → `resnet50_<hash>.onnx`; `README` → `README_<hash>`.
pub(crate) fn stored_name(name: &str, sha256: &str) -> String {
    match name.rfind('.') {
        Some(dot) if dot > 0 => format!("{}_{sha256}{}", &name[..dot], &name[dot..]),
        _ => format!("{name}_{sha256}"),
    }
}

/// Uploads in progress, inside `files_dir` (so storing one is a rename).
const INCOMING: &str = ".incoming";

/// Removes an incoming file when dropped (it was stored or refused).
struct Incoming(PathBuf);

impl Drop for Incoming {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

impl Service {
    pub(crate) fn files(&self) -> &NodeFiles {
        &self.files
    }

    /// A fresh path to stream an upload to; hand it to [`Service::put_upload`].
    pub(crate) async fn incoming_path(&self) -> Result<PathBuf, ServiceError> {
        let dir = self.files.files_dir.join(INCOMING);
        tokio::fs::create_dir_all(&dir).await.map_err(io_error)?;
        Ok(dir.join(Ulid::generate().to_string()))
    }

    /// Stores the file at `incoming` (from [`Service::incoming_path`]) as the
    /// node's upload `name`, replacing one with that name. Returns it, and
    /// whether it's new. `incoming` is removed in every case.
    pub(crate) async fn put_upload(
        &self,
        scene_id: &str,
        node_id: &str,
        name: &str,
        incoming: &Path,
    ) -> Result<(NodeUpload, bool), ServiceError> {
        self.put_upload_in(Store::Live, scene_id, node_id, name, incoming)
            .await
    }

    /// [`Service::put_upload`], live or in the scene's draft.
    pub(crate) async fn put_upload_in(
        &self,
        store: Store,
        scene_id: &str,
        node_id: &str,
        name: &str,
        incoming: &Path,
    ) -> Result<(NodeUpload, bool), ServiceError> {
        let _incoming = Incoming(incoming.to_path_buf());
        check_file_name(name)
            .map_err(|e| ServiceError::Invalid(vec![format!("upload name {name:?}: {e}")]))?;
        let path = incoming.to_path_buf();
        let (sha256, size) = tokio::task::spawn_blocking(move || {
            let size = std::fs::metadata(&path)?.len();
            sha256::try_digest(path.as_path()).map(|sha| (sha, size))
        })
        .await
        .map_err(|e| ServiceError::Unknown(format!("hashing an upload: {e}")))?
        .map_err(io_error)?;

        let _guard = self.lock.lock().await;
        let mut conn = self.conn().await?;
        let scene = query_sqlite::get_scene(&mut conn, scene_id)
            .await
            .map_err(not_found("scene"))?;
        if store == Store::Draft {
            self.ensure_draft(&mut conn, &scene).await?;
        }
        let node: Node = query_sqlite::get_node(&mut conn, store, scene_id, node_id)
            .await
            .map_err(not_found("node"))?
            .try_into()?;
        let existed = query_sqlite::get_upload(&mut conn, store, node_id, name)
            .await
            .is_ok();
        // A file the settings name feeds the node's configurator (if its
        // plugin has one): it runs on the new file first, and a failure
        // refuses the upload.
        let reconfigured = self
            .configure_for_upload(&mut conn, store, &node, name, incoming)
            .await?;
        let stored = self.files.files_dir.join(stored_name(name, &sha256));
        if !stored.exists() {
            tokio::fs::rename(incoming, &stored)
                .await
                .map_err(io_error)?;
            read_only(&stored);
        }
        let mut tx = conn.begin_with(WRITE_TX).await?;
        let upload = query_sqlite::put_upload(
            &mut tx,
            store,
            node_id,
            name,
            &sha256,
            i64::try_from(size).unwrap_or(i64::MAX),
        )
        .await?;
        if let Some(prepared) = &reconfigured {
            query_sqlite::update_node(&mut tx, store, scene_id, node_id, &prepared.values())
                .await?;
            self.store_configured(&mut tx, store, node_id, prepared)
                .await?;
        }
        self.committed(&mut tx, store, scene_id).await?;
        tx.commit().await?;
        self.collect_garbage(&mut conn).await?;
        // A running node gets the new file when it restarts (the director's
        // diff restarts nodes whose uploads changed).
        if store == Store::Live {
            self.changed(&scene);
        }
        log::info!(
            "Scene service: node [{node_id}] got upload {name:?} ({size} bytes, sha256 {sha256})"
        );
        Ok((upload, !existed))
    }

    /// The node with its configurator run on a replaced upload `name` (at
    /// `incoming`), if its plugin has one and its settings name the file.
    async fn configure_for_upload(
        &self,
        conn: &mut sqlx::SqliteConnection,
        store: Store,
        node: &Node,
        name: &str,
        incoming: &Path,
    ) -> Result<Option<PreparedNode>, ServiceError> {
        let remote = match node.config.launch {
            crate::modules::node_config::LaunchMode::Local => None,
            crate::modules::node_config::LaunchMode::Remote => match &node.config.configurator {
                None => return Ok(None),
                Some(c) => Some(c.clone()),
            },
        };
        let Ok(plugin) = self.plugins.get(&node.plugin_id).await else {
            return Ok(None);
        };
        let Some(package) = configurator::PackageRef::of(&plugin).filter(|p| p.has_configurator())
        else {
            return Ok(None);
        };
        let rules = NodeRules::of(&plugin).map_err(ServiceError::Unknown)?;
        let named = rules.uploads_referenced(&node.config.launch_args);
        let endpoint_files = remote.as_ref().map(|r| r.uploads()).unwrap_or_default();
        // A file the settings name changes them: it runs. An endpoint file
        // (CA, client certificate) only matters while it never ran.
        let never_ran = !rules
            .spec
            .configured_fields()
            .iter()
            .any(|f| node.config.launch_args.get(f.as_str()).is_some());
        if !named.iter().any(|(_, n, _)| n == name)
            && !(never_ran && endpoint_files.contains(&name))
        {
            return Ok(None);
        }
        let mut uploads =
            configurator::uploads(conn, store, &self.files.files_dir, &node.id).await?;
        uploads.insert(name.to_string(), incoming.to_path_buf());
        // Still waiting for another file: it runs once that one arrives.
        if named.iter().any(|(_, n, _)| !uploads.contains_key(n))
            || endpoint_files.iter().any(|n| !uploads.contains_key(*n))
        {
            return Ok(None);
        }
        let ran = self
            .run_configurator(
                &package,
                &node.id,
                &node.config.launch_args,
                uploads,
                remote.as_ref(),
            )
            .await;
        let configured = match ran {
            Ok(configured) => configured,
            // A draft's unfinished node (nothing configured to lose) takes
            // the file; its check says what's still wrong.
            Err(errors) if store == Store::Draft && never_ran => {
                return Ok(Some(PreparedNode {
                    name: node.name.clone(),
                    node_type: plugin.plugin_type.clone(),
                    plugin_id: plugin.id.clone(),
                    plugin_version: Some(plugin.version.clone()),
                    config_json: serde_json::to_string(&node.config).expect("config serializes"),
                    configured: None,
                    refusal: Some((node.config.launch_args.clone(), errors)),
                }));
            }
            Err(mut errors) => {
                errors.insert(
                    0,
                    format!("the node's configurator refused the new {name:?}"),
                );
                return Err(ServiceError::Invalid(errors));
            }
        };
        let mut config = node.config.clone();
        config.launch_args = configured.launch_args.clone();
        Ok(Some(PreparedNode {
            name: node.name.clone(),
            node_type: plugin.plugin_type.clone(),
            plugin_id: plugin.id.clone(),
            plugin_version: Some(plugin.version.clone()),
            config_json: serde_json::to_string(&config).expect("config serializes"),
            configured: Some(configured),
            refusal: None,
        }))
    }

    pub(crate) async fn list_uploads(
        &self,
        scene_id: &str,
        node_id: &str,
    ) -> Result<Vec<NodeUpload>, ServiceError> {
        self.list_uploads_in(Store::Live, scene_id, node_id).await
    }

    pub(crate) async fn list_uploads_in(
        &self,
        store: Store,
        scene_id: &str,
        node_id: &str,
    ) -> Result<Vec<NodeUpload>, ServiceError> {
        let mut conn = self.conn().await?;
        query_sqlite::get_node(&mut conn, store, scene_id, node_id)
            .await
            .map_err(not_found("node"))?;
        Ok(query_sqlite::list_uploads(&mut conn, store, node_id).await?)
    }

    pub(crate) async fn get_upload(
        &self,
        scene_id: &str,
        node_id: &str,
        name: &str,
    ) -> Result<NodeUpload, ServiceError> {
        self.get_upload_in(Store::Live, scene_id, node_id, name)
            .await
    }

    pub(crate) async fn get_upload_in(
        &self,
        store: Store,
        scene_id: &str,
        node_id: &str,
        name: &str,
    ) -> Result<NodeUpload, ServiceError> {
        let mut conn = self.conn().await?;
        query_sqlite::get_node(&mut conn, store, scene_id, node_id)
            .await
            .map_err(not_found("node"))?;
        query_sqlite::get_upload(&mut conn, store, node_id, name)
            .await
            .map_err(not_found("upload"))
    }

    /// An upload and the path of its bytes, for download. Refused for a
    /// file a secret launch arg names.
    pub(crate) async fn upload_content(
        &self,
        scene_id: &str,
        node_id: &str,
        name: &str,
    ) -> Result<(NodeUpload, PathBuf), ServiceError> {
        self.upload_content_in(Store::Live, scene_id, node_id, name)
            .await
    }

    pub(crate) async fn upload_content_in(
        &self,
        store: Store,
        scene_id: &str,
        node_id: &str,
        name: &str,
    ) -> Result<(NodeUpload, PathBuf), ServiceError> {
        let upload = self.get_upload_in(store, scene_id, node_id, name).await?;
        let node = self.get_node_in(store, scene_id, node_id).await?;
        if let Ok(plugin) = self.plugins.get(&node.plugin_id).await
            && let Ok(rules) = NodeRules::of(&plugin)
            && let Some((field, _, _)) = rules
                .uploads_referenced(&node.config.launch_args)
                .into_iter()
                .find(|(_, n, secret)| *secret && n == name)
        {
            return Err(ServiceError::Forbidden(format!(
                "upload {name:?} is the node's secret launch_args/{field}; it can't be downloaded"
            )));
        }
        if let Some(crate::modules::node_config::ConfiguratorAuth::Mtls { key, .. }) = node
            .config
            .configurator
            .as_ref()
            .and_then(|c| c.auth.as_ref())
            && key == name
        {
            return Err(ServiceError::Forbidden(format!(
                "upload {name:?} is the key of the node's configurator client certificate; it can't be downloaded"
            )));
        }
        let path = self.files.files_dir.join(upload.stored_name());
        Ok((upload, path))
    }

    /// Deletes an upload. Refused while the node's launch args name it.
    pub(crate) async fn delete_upload(
        &self,
        scene_id: &str,
        node_id: &str,
        name: &str,
    ) -> Result<(), ServiceError> {
        self.delete_upload_in(Store::Live, scene_id, node_id, name)
            .await
    }

    pub(crate) async fn delete_upload_in(
        &self,
        store: Store,
        scene_id: &str,
        node_id: &str,
        name: &str,
    ) -> Result<(), ServiceError> {
        let _guard = self.lock.lock().await;
        let mut conn = self.conn().await?;
        let scene = query_sqlite::get_scene(&mut conn, scene_id)
            .await
            .map_err(not_found("scene"))?;
        if store == Store::Draft {
            self.ensure_draft(&mut conn, &scene).await?;
        }
        let node: Node = query_sqlite::get_node(&mut conn, store, scene_id, node_id)
            .await
            .map_err(not_found("node"))?
            .try_into()?;
        if let Ok(plugin) = self.plugins.get(&node.plugin_id).await
            && let Ok(rules) = NodeRules::of(&plugin)
        {
            let fields: Vec<String> = rules
                .uploads_referenced(&node.config.launch_args)
                .into_iter()
                .filter(|(_, n, _)| n == name)
                .map(|(field, _, _)| format!("launch_args/{field}"))
                .collect();
            let mut fields = fields;
            if node
                .config
                .configurator
                .as_ref()
                .is_some_and(|c| c.uploads().contains(&name))
            {
                fields.push("configurator".into());
            }
            if !fields.is_empty() {
                return Err(ServiceError::Conflict(
                    format!("the node's launch args name upload {name:?}; change them first"),
                    fields,
                ));
            }
        }
        if query_sqlite::delete_upload(&mut conn, store, node_id, name).await? == 0 {
            return Err(ServiceError::NotFound("upload"));
        }
        self.committed(&mut conn, store, scene_id).await?;
        self.collect_garbage(&mut conn).await?;
        if store == Store::Live {
            self.changed(&scene);
        }
        Ok(())
    }

    /// Empties a node's folder (the plugin's own files: its state, caches),
    /// so it starts over on its next launch. Only while its scene is disabled.
    pub(crate) async fn reset_workdir(
        &self,
        scene_id: &str,
        node_id: &str,
    ) -> Result<(), ServiceError> {
        let _guard = self.lock.lock().await;
        let mut conn = self.conn().await?;
        let scene = query_sqlite::get_scene(&mut conn, scene_id)
            .await
            .map_err(not_found("scene"))?;
        query_sqlite::get_node(&mut conn, Store::Live, scene_id, node_id)
            .await
            .map_err(not_found("node"))?;
        if scene.enabled {
            return Err(ServiceError::Conflict(
                "the scene is running; disable it first".into(),
                vec![],
            ));
        }
        let dir = workdir::node_dir(&self.files.nodes_dir, scene_id, node_id);
        tokio::task::spawn_blocking(move || workdir::remove(&dir))
            .await
            .map_err(|e| ServiceError::Unknown(format!("removing a node folder: {e}")))?
            .map_err(io_error)?;
        log::info!("Scene service: emptied the folder of node [{node_id}]");
        Ok(())
    }

    /// Removes stored files no node has any more, and unfinished uploads.
    /// At start-up, before any upload can be in progress.
    pub(crate) async fn clean_files(&self) -> Result<(), ServiceError> {
        let _guard = self.lock.lock().await;
        let _ = tokio::fs::remove_dir_all(self.files.files_dir.join(INCOMING)).await;
        let mut conn = self.conn().await?;
        self.collect_garbage(&mut conn).await
    }

    /// Removes stored files no row names (and gone nodes' configurator
    /// warnings). The caller holds the service lock
    /// (so no upload is between its rename and its row).
    pub(super) async fn collect_garbage(
        &self,
        conn: &mut sqlx::SqliteConnection,
    ) -> Result<(), ServiceError> {
        configurator::forget_warnings(conn).await?;
        let keep: HashSet<String> = query_sqlite::stored_files(conn)
            .await?
            .into_iter()
            .map(|(name, sha)| stored_name(&name, &sha))
            .collect();
        let Ok(mut entries) = tokio::fs::read_dir(&self.files.files_dir).await else {
            return Ok(()); // nothing stored yet
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || keep.contains(&name) {
                continue;
            }
            let path = entry.path();
            writable(&path);
            match tokio::fs::remove_file(&path).await {
                Ok(()) => log::debug!("Scene service: removed unused upload {name}"),
                Err(e) => log::warn!("Scene service: couldn't remove {}: {e}", path.display()),
            }
        }
        Ok(())
    }
}

fn io_error(e: std::io::Error) -> ServiceError {
    ServiceError::Unknown(format!("file error: {e}"))
}

/// Stored files are read-only: nothing changes them in place.
fn read_only(path: &Path) {
    if let Ok(meta) = std::fs::metadata(path) {
        let mut perms = meta.permissions();
        perms.set_readonly(true);
        let _ = std::fs::set_permissions(path, perms);
    }
}

/// Windows can't remove a read-only file.
fn writable(path: &Path) {
    #[cfg(windows)]
    if let Ok(meta) = std::fs::metadata(path) {
        let mut perms = meta.permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        let _ = std::fs::set_permissions(path, perms);
    }
    #[cfg(not(windows))]
    let _ = path;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_names_keep_the_extension() {
        assert_eq!(stored_name("resnet50.onnx", "ab"), "resnet50_ab.onnx");
        assert_eq!(stored_name("a.tar.gz", "ab"), "a.tar_ab.gz");
        assert_eq!(stored_name("README", "ab"), "README_ab");
    }
}
