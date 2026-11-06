//! The hub: the TCP endpoint plugins connect to (handshake, per-connection
//! reader and writer). The director runs it (so it outlives every scene) and
//! uses it through [`Hub::expect`] and [`NodeLink`].

mod config;
mod service;
#[cfg(test)]
mod tests;
pub(crate) mod wire;

pub(crate) use config::HubConfig;
pub(crate) use service::{ExpectedNode, Hub, HubError, HubEvent, LinkSender, NodeLink};
use std::sync::OnceLock;

static HUB: OnceLock<Hub> = OnceLock::new();

/// Binds the hub listener from the `[hub]` config. Called once at start-up,
/// so the actual port is known before plugins are launched.
pub(crate) async fn init() -> std::io::Result<&'static Hub> {
    if let Some(hub) = HUB.get() {
        return Ok(hub);
    }
    let hub = Hub::bind(HubConfig::read()).await?;
    Ok(HUB.get_or_init(|| hub))
}

/// The hub bound at start-up. Panics if [`init`] hasn't run.
pub(crate) fn get() -> &'static Hub {
    HUB.get().expect("hub is initialised at start-up")
}
