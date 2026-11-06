//! The director: runs scene workflows. Other services talk to it through
//! [`DirectorHandle`].

mod config;
mod engine;
pub(crate) mod handle;
pub(crate) mod queue;
pub(crate) mod running;
mod service;
// The tests run real plugin processes through shell scripts.
#[cfg(all(test, unix))]
mod tests;

pub(crate) use handle::DirectorHandle;
use std::sync::OnceLock;

static DIRECTOR: OnceLock<(DirectorHandle, service::Service)> = OnceLock::new();

/// Creates the director and its command channel. Called once at start-up,
/// before the services that send it commands.
pub(crate) fn init() -> &'static DirectorHandle {
    let (handle, _) = DIRECTOR.get_or_init(|| {
        let (handle, commands) = DirectorHandle::channel();
        (handle, service::Service::new(commands))
    });
    handle
}

/// The director as a registry service. Panics if [`init`] hasn't run.
pub(crate) fn service() -> &'static dyn flwrs_core::registry::Service {
    &DIRECTOR
        .get()
        .expect("director is initialised at start-up")
        .1
}
