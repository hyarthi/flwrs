//! Plugin registry: plugin packages (ZIP with `manifest.json`) uploaded over
//! REST, validated, unpacked into `[plugins] dir`, and tracked in `plugins`.

pub(crate) mod api;
pub(crate) mod config;
pub(crate) mod manifest;
pub(crate) mod package;
pub(crate) mod service;

use crate::db::main_db;
use crate::modules::director::DirectorHandle;
use service::Service;
use tokio::sync::OnceCell;

static SERVICE: OnceCell<Service> = OnceCell::const_new();

/// Creates the plugin service, prepares its folders and checks registered
/// packages. Called once at start-up, before the HTTP server is built.
pub(crate) async fn init(director: DirectorHandle) -> &'static Service {
    SERVICE
        .get_or_init(|| async {
            let config = config::PluginsConfig::read();
            let service = Service::new(main_db().await, config, director);
            if let Err(e) = service.prepare_dirs().await {
                log::error!(
                    "Plugin registry: failed to prepare [{}]: {e}",
                    service.config().dir.display()
                );
                std::process::exit(1);
            }
            if let Err(e) = service.check_installations().await {
                log::error!("Plugin registry: start-up check failed: {e}");
            }
            service
        })
        .await
}

/// The plugin service. Panics if [`init`] hasn't run.
pub(crate) fn service() -> &'static Service {
    SERVICE
        .get()
        .expect("plugin service is initialised at start-up")
}
