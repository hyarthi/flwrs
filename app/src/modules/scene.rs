//! Scenes: workflow definitions (a graph of plugin-instance nodes and
//! data-handover edges), managed at config time.

pub(crate) mod api;
pub(crate) mod api_graph;
pub(crate) mod api_status;
pub(crate) mod api_transfer;
pub(crate) mod api_uploads;
pub(crate) mod archive;
pub(crate) mod config;
pub(crate) mod graph;
pub(crate) mod scene_file;
pub(crate) mod service;

use crate::db::main_db;
use crate::modules::director::DirectorHandle;
use crate::modules::director::config::DirectorConfig;
use crate::modules::plugin;
use service::Service;
use tokio::sync::OnceCell;

static SERVICE: OnceCell<Service> = OnceCell::const_new();

/// Creates the scene service. Called once at start-up, after the plugin
/// registry and the director handle exist.
pub(crate) async fn init(director: DirectorHandle) -> &'static Service {
    SERVICE
        .get_or_init(|| async {
            let paths = DirectorConfig::read();
            let limits = config::ScenesConfig::read();
            let files = service::NodeFiles {
                nodes_dir: paths.nodes_dir,
                files_dir: paths.files_dir,
                max_upload_bytes: limits.max_upload_bytes,
                max_import_bytes: limits.max_import_bytes,
                max_import_entries: limits.max_import_entries,
            };
            plugin::service().set_configure_env(crate::modules::configurator::Env {
                files_dir: files.files_dir.clone(),
                limits: crate::modules::configurator::Limits {
                    timeout: std::time::Duration::from_millis(
                        plugin::service().config().configure_timeout_ms,
                    ),
                    max_response_bytes: plugin::service().config().configure_max_response_bytes,
                    max_file_bytes: files.max_upload_bytes,
                },
            });
            let service = Service::new(main_db().await, plugin::service(), director, files);
            if let Err(e) = service.clean_files().await {
                log::warn!("Scene service: couldn't clean up uploaded files: {e}");
            }
            service
        })
        .await
}

/// The scene service. Panics if [`init`] hasn't run.
pub(crate) fn service() -> &'static Service {
    SERVICE
        .get()
        .expect("scene service is initialised at start-up")
}
