mod db;
mod http;
mod modules;
mod registry;

use crate::registry::build_registry;
use std::env;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    flwrs_core::logging::init_main_logger()?;
    log::info!("Application is starting up...");
    log::debug!("Working directory: {}", env::current_dir()?.display());

    // Order matters: the director's handle is used by the plugin and scene
    // services, and the scene service uses the plugin registry.
    let director = modules::director::init();
    modules::plugin::init(director.clone()).await;
    modules::scene::init(director.clone()).await;

    let registry = build_registry().await;
    registry.start().await?;

    log::info!("Application stopped");
    Ok(())
}
