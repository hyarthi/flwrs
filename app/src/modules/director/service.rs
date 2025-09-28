use flwrs_core::registry;
use flwrs_core::registry::RegistryError;
use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

pub(crate) struct Service;

#[async_trait]
impl registry::Service for Service {
    fn id(&self) -> String {
        "director-service".to_string()
    }

    async fn start(&self, shutdown_token: CancellationToken) -> Result<(), RegistryError> {
        todo!()
    }
}

pub(crate) struct ScreenSet;
