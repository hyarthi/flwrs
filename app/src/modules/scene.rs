//! Scenes: workflow definitions (a graph of plugin-instance nodes and
//! data-handover edges), managed at config time.

pub(crate) mod api;
pub(crate) mod api_graph;
pub(crate) mod api_status;
pub(crate) mod api_transfer;
pub(crate) mod graph;
pub(crate) mod scene_file;
pub(crate) mod service;

use crate::db::main_db;
use crate::modules::director::DirectorHandle;
use crate::modules::plugin;
use service::Service;
use tokio::sync::OnceCell;

static SERVICE: OnceCell<Service> = OnceCell::const_new();

/// Creates the scene service. Called once at start-up, after the plugin
/// registry and the director handle exist.
pub(crate) async fn init(director: DirectorHandle) -> &'static Service {
    SERVICE
        .get_or_init(|| async { Service::new(main_db().await, plugin::service(), director) })
        .await
}

/// The scene service. Panics if [`init`] hasn't run.
pub(crate) fn service() -> &'static Service {
    SERVICE
        .get()
        .expect("scene service is initialised at start-up")
}
