use async_trait::async_trait;
use lazy_static::lazy_static;
use std::error::Error;
use std::fmt::Debug;
use std::sync::Arc;
use thiserror::Error;
use tokio::signal;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

lazy_static! {
    static ref SHUTDOWN: Arc<CancellationToken> = Arc::new(CancellationToken::new());
}

async fn shutdown() -> Result<(), RegistryError> {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {
            log::debug!("received Ctrl+C, shutting down");
            SHUTDOWN.cancel();
        },
        _ = terminate => {
            log::debug!("received SIGINT, shutting down");
            SHUTDOWN.cancel();
        }
    }

    Ok(())
}

pub struct ServiceRegistry {
    services: Vec<Arc<&'static dyn Service>>,
    tracker: TaskTracker,
}

impl ServiceRegistry {
    pub fn register_service(&mut self, service: &'static dyn Service) {
        self.services.push(Arc::new(service));
    }

    pub async fn start(&self) -> Result<(), RegistryError> {
        self.services.iter().for_each(|service| {
            log::debug!("Starting service: [{id}]", id = service.id());
            self.tracker.spawn(service.start(SHUTDOWN.as_ref().clone()));
        });

        log::debug!("Registry waiting on tasks to complete");
        match shutdown().await {
            Ok(_) => {}
            Err(e) => {
                log::error!("Service error: {e}");
                return Err(e);
            }
        };
        log::info!("Service registry shutting down");
        // TODO replace with cancellation token
        SHUTDOWN.as_ref().cancel();
        self.tracker.close();
        self.tracker.wait().await;
        Ok(())
    }
}

impl Default for ServiceRegistry {
    fn default() -> Self {
        Self {
            services: vec![],
            tracker: TaskTracker::new(),
        }
    }
}

#[derive(Error, Debug)]
pub enum RegistryError {
    #[error("{msg}: {source}")]
    ServiceError {
        msg: String,
        #[source]
        source: RegistrySourceError,
    },
}

type RegistrySourceError = Box<dyn Error + Send>;

#[async_trait]
pub trait Service: Sync + Send {
    fn id(&self) -> String;
    async fn start(&self, shutdown_token: CancellationToken) -> Result<(), RegistryError>;
}
