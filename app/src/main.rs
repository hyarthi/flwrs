mod db;
mod http;
mod modules;
mod registry;

use crate::registry::build_registry;
use std::env;

/// `[runtime]`: the server's own threads.
#[derive(serde::Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct RuntimeSection {
    /// Cores to use: 1 = single-threaded, N = the main thread + N - 1
    /// workers; unset or 0 = a worker per core.
    threads: Option<usize>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let section: RuntimeSection = flwrs_core::config::read_struct(
        flwrs_core::config::main_config(),
        &["runtime".to_string()],
    )
    .unwrap_or_default();
    flwrs_plugin::rt::runtime(section.threads)?.block_on(run())
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    flwrs_core::logging::init_main_logger()?;
    log::info!("Application is starting up...");
    log::debug!("Working directory: {}", env::current_dir()?.display());

    // Order matters: the hub is bound first (the director will launch plugins
    // pointing at its address), the director's handle is used by the plugin
    // and scene services, and the scene service uses the plugin registry.
    let hub = modules::hub::init().await?;
    log::info!("Hub bound to {}", hub.local_addr());
    let director = modules::director::init();
    modules::plugin::init(director.clone()).await;
    modules::scene::init(director.clone()).await;

    let registry = build_registry().await;
    registry.start().await?;

    log::info!("Application stopped");
    Ok(())
}
