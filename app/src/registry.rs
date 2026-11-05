use crate::http;
use flwrs_core::registry::ServiceRegistry;

pub async fn build_registry() -> ServiceRegistry {
    log::debug!("Building registry");
    let mut registry = ServiceRegistry::default();

    log::debug!("Registering director service");
    registry.register_service(crate::modules::director::service());

    // HTTP
    log::debug!("Registering HTTP service");
    registry.register_service(http::server());

    log::debug!("Registry build completed");
    registry
}
