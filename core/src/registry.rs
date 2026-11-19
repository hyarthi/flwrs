use async_trait::async_trait;
use std::error::Error;
use std::fmt::Debug;
use thiserror::Error;
use tokio::signal;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

/// Resolves on Ctrl+C or (on Unix) SIGTERM.
async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(e) = signal::ctrl_c().await {
            log::error!("Failed to listen for Ctrl+C: {e}");
            std::future::pending::<()>().await;
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match signal::unix::signal(signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(e) => {
                log::error!("Failed to listen for SIGTERM: {e}");
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => log::debug!("Received Ctrl+C, shutting down"),
        _ = terminate => log::debug!("Received SIGTERM, shutting down"),
    }
}

/// Asked to stop from inside (e.g. a restart from the management API).
static STOP: std::sync::LazyLock<CancellationToken> =
    std::sync::LazyLock::new(CancellationToken::new);

/// Stops the registry as a shutdown signal would: every service is cancelled
/// and [`ServiceRegistry::start`] returns once they've finished.
pub fn request_stop() {
    STOP.cancel();
}

/// Whether [`request_stop`] was called.
pub fn stop_requested() -> bool {
    STOP.is_cancelled()
}

/// Runs a set of long-lived services and stops them together.
#[derive(Default)]
pub struct ServiceRegistry {
    services: Vec<&'static dyn Service>,
    /// Stops it like a signal (default: the process-wide [`request_stop`]).
    stop: Option<CancellationToken>,
}

impl ServiceRegistry {
    /// Stopped by `token` instead of [`request_stop`] (tests).
    pub fn with_stop(mut self, token: CancellationToken) -> Self {
        self.stop = Some(token);
        self
    }

    pub fn register_service(&mut self, service: &'static dyn Service) {
        self.services.push(service);
    }

    /// Starts every service and waits until a shutdown signal arrives or a
    /// service fails. Then cancels the rest and waits for them to finish.
    /// Returns the first service error, if any.
    pub async fn start(&self) -> Result<(), RegistryError> {
        let token = CancellationToken::new();
        let mut tasks = JoinSet::new();
        for &service in &self.services {
            let id = service.id();
            log::debug!("Starting service: [{id}]");
            let token = token.clone();
            tasks.spawn(async move { (id, service.start(token).await) });
        }

        let mut first_error = None;
        let signal = shutdown_signal();
        tokio::pin!(signal);
        let stop = self.stop.clone().unwrap_or_else(|| STOP.clone());
        loop {
            tokio::select! {
                _ = &mut signal => break,
                () = stop.cancelled() => {
                    log::info!("Stop requested");
                    break;
                }
                joined = tasks.join_next() => match joined {
                    None => {
                        log::info!("All services stopped");
                        break;
                    }
                    Some(joined) => {
                        if let Some(err) = Self::handle_exit(joined) {
                            first_error = Some(err);
                            break;
                        }
                    }
                },
            }
        }

        log::info!("Service registry shutting down");
        token.cancel();
        while let Some(joined) = tasks.join_next().await {
            if let Some(err) = Self::handle_exit(joined) {
                first_error.get_or_insert(err);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    fn handle_exit(
        joined: Result<(String, Result<(), RegistryError>), tokio::task::JoinError>,
    ) -> Option<RegistryError> {
        match joined {
            Ok((id, Ok(()))) => {
                log::debug!("Service [{id}] stopped");
                None
            }
            Ok((id, Err(e))) => {
                log::error!("Service [{id}] failed: {e}");
                Some(e)
            }
            Err(e) => {
                log::error!("Service task panicked: {e}");
                Some(RegistryError::ServiceError {
                    msg: "Service task panicked".to_string(),
                    source: Box::new(e),
                })
            }
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

type RegistrySourceError = Box<dyn Error + Send + Sync>;

#[async_trait]
pub trait Service: Sync + Send {
    fn id(&self) -> String;
    /// Runs until `shutdown_token` is cancelled or the service fails.
    async fn start(&self, shutdown_token: CancellationToken) -> Result<(), RegistryError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct Failing;
    #[async_trait]
    impl Service for Failing {
        fn id(&self) -> String {
            "failing".into()
        }
        async fn start(&self, _: CancellationToken) -> Result<(), RegistryError> {
            Err(RegistryError::ServiceError {
                msg: "boom".into(),
                source: "bind failed".into(),
            })
        }
    }

    struct Waiting(AtomicBool);
    #[async_trait]
    impl Service for Waiting {
        fn id(&self) -> String {
            "waiting".into()
        }
        async fn start(&self, token: CancellationToken) -> Result<(), RegistryError> {
            token.cancelled().await;
            self.0.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test]
    async fn failing_service_stops_the_others() {
        static WAITING: Waiting = Waiting(AtomicBool::new(false));
        static FAILING: Failing = Failing;
        let mut registry = ServiceRegistry::default();
        registry.register_service(&WAITING);
        registry.register_service(&FAILING);

        let result = tokio::time::timeout(std::time::Duration::from_secs(5), registry.start())
            .await
            .expect("registry should not hang when a service fails");
        assert!(matches!(result, Err(RegistryError::ServiceError { msg, .. }) if msg == "boom"));
        assert!(
            WAITING.0.load(Ordering::SeqCst),
            "other services get cancelled"
        );
    }

    #[tokio::test]
    async fn a_stop_request_stops_every_service() {
        let stop = CancellationToken::new();
        let mut registry = ServiceRegistry::default().with_stop(stop.clone());
        let waiting: &'static Waiting = Box::leak(Box::new(Waiting(AtomicBool::new(false))));
        registry.register_service(waiting);
        let run = tokio::spawn(async move { registry.start().await });
        tokio::task::yield_now().await;
        stop.cancel();
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), run)
            .await
            .expect("stopped")
            .unwrap();
        assert!(result.is_ok());
    }
}
