use crate::config::main_config;
use crate::registry::RegistryError;
use crate::{config, registry};
use async_trait::async_trait;
use axum::extract::Request;
use axum::response::IntoResponse;
use axum::routing::{MethodRouter, Route};
use axum::Router;
use serde::Deserialize;
use std::collections::HashMap;
use std::convert::Infallible;
use std::io;
use thiserror::Error;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tower_layer::Layer;
use tower_service::Service;

#[derive(Deserialize)]
pub struct ServerConfig {
    host: String,
    port: u32,
}

impl ServerConfig {
    pub fn read() -> Self {
        let main_config = main_config();
        config::read_struct(main_config, &["http".to_string()]).unwrap_or(Self {
            host: "".to_string(),
            port: 80,
        })
    }
}

pub struct HttpServer {
    config: ServerConfig,
    api_delegate: Router,
    root_delegate: Router,
}

impl Default for HttpServer {
    fn default() -> Self {
        Self {
            config: ServerConfig::read(),
            api_delegate: Router::new(),
            root_delegate: Router::new(),
        }
    }
}

impl HttpServer {
    pub fn new(apis: Vec<Router>, swagger_ui: Option<Router>) -> Self {
        let mut server = HttpServer::default();

        for api in apis {
            server.merge_api(api);
        }

        match swagger_ui {
            None => {}
            Some(router) => {
                server.merge_root(router);
            }
        }

        server
    }

    pub fn register_api<L>(
        &mut self,
        path: &str,
        handlers: HashMap<String, MethodRouter>,
        middleware: Option<L>,
    ) -> ()
    where
        L: Layer<Route> + Clone + Send + Sync + 'static,
        L::Service: Service<Request> + Clone + Send + Sync + 'static,
        <L::Service as Service<Request>>::Response: IntoResponse + 'static,
        <L::Service as Service<Request>>::Error: Into<Infallible> + 'static,
        <L::Service as Service<Request>>::Future: Send + 'static,
    {
        self.api_delegate = self
            .api_delegate
            .clone()
            .nest(path, Self::build_router(handlers, middleware));
    }

    pub fn merge_api(&mut self, router: Router) -> () {
        self.api_delegate = self.api_delegate.clone().merge(router);
    }

    pub fn merge_root(&mut self, router: Router) -> () {
        self.root_delegate = self.root_delegate.clone().merge(router);
    }

    fn build_router<L>(handlers: HashMap<String, MethodRouter>, middleware: Option<L>) -> Router
    where
        L: Layer<Route> + Clone + Send + Sync + 'static,
        L::Service: Service<Request> + Clone + Send + Sync + 'static,
        <L::Service as Service<Request>>::Response: IntoResponse + 'static,
        <L::Service as Service<Request>>::Error: Into<Infallible> + 'static,
        <L::Service as Service<Request>>::Future: Send + 'static,
    {
        let mut router = Router::new();

        for (path, handler) in handlers.iter() {
            router = router.route(path.as_str(), handler.clone());
        }

        match middleware {
            None => {}
            Some(m) => {
                router = router.layer(m);
            }
        };

        router
    }

    async fn shutdown(token: CancellationToken) -> () {
        token.cancelled().await;
        log::debug!("HTTP server shutting down.");
    }
}

#[derive(Error, Debug)]
pub enum HttpError {
    #[error("IO error: {0}")]
    IoError(
        #[from]
        #[source]
        io::Error,
    ),
}

#[async_trait]
impl registry::Service for HttpServer {
    fn id(&self) -> String {
        "http".to_string()
    }

    async fn start(&self, token: CancellationToken) -> Result<(), RegistryError> {
        let root_router = self.root_delegate.clone();
        let api_router = self.api_delegate.clone();
        let host = self.config.host.clone();
        let port = self.config.port;
        log::debug!("Starting HTTP server.");
        let listener = match TcpListener::bind(format!("{}:{}", host, port)).await {
            Ok(listener) => listener,
            Err(e) => {
                return Err(RegistryError::ServiceError {
                    msg: "Net socket error".to_string(),
                    source: Box::new(HttpError::from(e)),
                });
            }
        };
        log::info!(
            "HTTP server listening on host = [{host}], port = [{port}]",
            host = host,
            port = port
        );
        // TODO add common middlewares
        match axum::serve(listener, root_router.nest("/api", api_router))
            .with_graceful_shutdown(Self::shutdown(token))
            .await
        {
            Ok(()) => {
                log::debug!("HTTP server exited");
                Ok(())
            }
            Err(e) => Err(RegistryError::ServiceError {
                msg: "HTTP runtime error".to_string(),
                source: Box::new(HttpError::from(e)),
            }),
        }
    }
}
