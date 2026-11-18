use crate::http;
use flwrs_core::registry::ServiceRegistry;

pub async fn build_registry() -> ServiceRegistry {
    log::debug!("Building registry");
    let mut registry = ServiceRegistry::default();

    log::debug!("Registering director service");
    registry.register_service(crate::modules::director::service());

    if let Some(telemetry) = crate::modules::telemetry::service() {
        log::debug!("Registering telemetry service");
        registry.register_service(telemetry);
    }

    // HTTP: the management API (and Swagger UI), unless switched off.
    if http::server().config().enabled() {
        log::debug!("Registering HTTP service");
        registry.register_service(http::server());
    } else {
        log::info!(
            "HTTP API off ([http] enabled = false): scenes run, but can't be managed until it's switched on again"
        );
    }

    log::debug!("Registry build completed");
    registry
}
