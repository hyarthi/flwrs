use super::handle::{DirectorCommand, DirectorError};
use async_trait::async_trait;
use flwrs_core::registry;
use flwrs_core::registry::RegistryError;
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;

/// The workflow engine. For now it only receives commands and logs them;
/// loading and running scenes comes in a later phase (DIRECTOR_PLAN.tmp.md).
pub(crate) struct Service {
    commands: Mutex<Option<mpsc::Receiver<DirectorCommand>>>,
}

impl Service {
    pub(crate) fn new(commands: mpsc::Receiver<DirectorCommand>) -> Self {
        Self {
            commands: Mutex::new(Some(commands)),
        }
    }

    fn handle(command: DirectorCommand) {
        match command {
            DirectorCommand::Reload { scene_id, done } => {
                log::info!(
                    "Director: reload requested for scene [{scene_id}] (scene execution isn't implemented yet)"
                );
                if let Some(done) = done {
                    let _ = done.send(Ok(()));
                }
            }
            DirectorCommand::Stop { scene_id, done } => {
                log::info!(
                    "Director: stop requested for scene [{scene_id}] (scene execution isn't implemented yet)"
                );
                if let Some(done) = done {
                    let _ = done.send(Ok::<(), DirectorError>(()));
                }
            }
            DirectorCommand::Status { scene_id, reply } => {
                log::debug!("Director: status requested for scene [{scene_id}]");
                let _ = reply.send(None);
            }
        }
    }
}

#[async_trait]
impl registry::Service for Service {
    fn id(&self) -> String {
        "director".to_string()
    }

    async fn start(&self, shutdown_token: CancellationToken) -> Result<(), RegistryError> {
        let Some(mut commands) = self.commands.lock().await.take() else {
            log::error!("Director: started twice");
            return Ok(());
        };
        loop {
            tokio::select! {
                _ = shutdown_token.cancelled() => break,
                command = commands.recv() => match command {
                    Some(command) => Self::handle(command),
                    None => break,
                },
            }
        }
        log::debug!("Director: stopped");
        Ok(())
    }
}
