use super::config::DirectorConfig;
use super::engine::Engine;
use super::handle::DirectorCommand;
use super::queue::{Budget, SpillShared};
use super::running::Context;
use crate::modules::supervisor::server_log_level;
use async_trait::async_trait;
use flwrs_core::registry;
use flwrs_core::registry::RegistryError;
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;

/// The director as a registry service: runs the [`Engine`] with the hub and
/// the scene service from start-up.
pub(crate) struct Service {
    commands: Mutex<Option<mpsc::Receiver<DirectorCommand>>>,
}

impl Service {
    pub(crate) fn new(commands: mpsc::Receiver<DirectorCommand>) -> Self {
        Self {
            commands: Mutex::new(Some(commands)),
        }
    }
}

#[async_trait]
impl registry::Service for Service {
    fn id(&self) -> String {
        "director".to_string()
    }

    async fn start(&self, shutdown_token: CancellationToken) -> Result<(), RegistryError> {
        let Some(commands) = self.commands.lock().await.take() else {
            log::error!("Director: started twice");
            return Ok(());
        };
        let hub = crate::modules::hub::get().clone();
        // The director owns the hub's lifetime: plugins stay reachable until
        // every scene has stopped (and got its Shutdown).
        let hub_stop = CancellationToken::new();
        let hub_task = {
            let (hub, token) = (hub.clone(), hub_stop.clone());
            tokio::spawn(async move { hub.run(token).await })
        };
        let config = DirectorConfig::read();
        let ctx = Context {
            budget: Budget::new(hub.config().max_inflight_bytes),
            spill: SpillShared::new(
                config.spool_dir.clone(),
                config.spill_default,
                config.memory_soft_inflight_bytes,
                config.spill_max_bytes,
                config.spill_min_free_bytes,
            ),
            hub,
            max_hops: config.max_hops,
            log_level: server_log_level(),
        };
        Engine::new(ctx, crate::modules::scene::service())
            .run(commands, shutdown_token)
            .await;
        hub_stop.cancel();
        match hub_task.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(err)) => Err(RegistryError::ServiceError {
                msg: "hub failed".into(),
                source: Box::new(err),
            }),
            Err(err) => Err(RegistryError::ServiceError {
                msg: "hub task failed".into(),
                source: Box::new(err),
            }),
        }
    }
}
