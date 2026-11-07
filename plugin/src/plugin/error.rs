use prost::DecodeError;
use std::fmt::{Display, Formatter};

/// Boxed error that can cross task boundaries.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("I/O error: {0}")]
    IOError(#[from] std::io::Error),
    #[error("Initialize error: {0}")]
    InitError(#[from] InitializeError),
    #[error("Shutdown error: {0}")]
    ShutdownError(#[from] ShutdownError),
    #[error("Sink error: {0}")]
    SinkError(#[from] SinkError),
    #[error("Source error: {0}")]
    SourceError(#[from] SourceError),
    #[error("Transform error: {0}")]
    TransformError(#[from] TransformError),
    #[error("Failed to set logger: {0}")]
    SetLoggerError(#[from] log::SetLoggerError),
    #[error("Invalid message: {0}")]
    InvalidMessage(#[from] DecodeError),
    #[error("Hub closed the connection")]
    Disconnected,
    #[error("The hub rejected the plugin (Shutdown before InitializeResponse)")]
    Rejected,
}

macro_rules! plugin_error {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Debug)]
        pub struct $name {
            pub source: BoxError,
            /// Whether trying again later may succeed. For an event a sink or
            /// transform couldn't handle, the hub then redelivers it (up to
            /// its attempt limit) instead of giving up on it.
            pub retry: bool,
        }

        impl $name {
            /// Wraps any error, or a message (`&str` / `String`). Not retried.
            pub fn new(source: impl Into<BoxError>) -> Self {
                Self { source: source.into(), retry: false }
            }

            /// Like [`Self::new`], for a failure that may go away (a timeout,
            /// a service that's down): the event is delivered again later.
            pub fn retryable(source: impl Into<BoxError>) -> Self {
                Self { source: source.into(), retry: true }
            }
        }

        impl Display for $name {
            fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
                Display::fmt(&self.source, f)
            }
        }

        impl std::error::Error for $name {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&*self.source)
            }
        }
    };
}

plugin_error!(
    /// Plugin failed to initialize.
    InitializeError
);
plugin_error!(
    /// Plugin failed to shut down cleanly.
    ShutdownError
);
plugin_error!(
    /// Sink failed to consume an event.
    SinkError
);
plugin_error!(
    /// Source failed while producing events.
    SourceError
);
plugin_error!(
    /// Transform failed to process an event.
    TransformError
);
