use std::fmt::{Display, Formatter};
use prost::DecodeError;

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
    #[error("Failed to set logger: {0}")]
    SetLoggerError(#[from] log::SetLoggerError),
    #[error("Invalid message: {0}")]
    InvalidMessage(#[from] DecodeError),
}

#[derive(Debug)]
pub struct InitializeError {
    pub source: Box<dyn std::error::Error>,
}

impl Display for InitializeError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.source.to_string())
    }
}

impl std::error::Error for InitializeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&*self.source)
    }
}

#[derive(Debug)]
pub struct ShutdownError {
    pub source: Box<dyn std::error::Error>,
}

impl Display for ShutdownError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.source.to_string())
    }
}

impl std::error::Error for ShutdownError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&*self.source)
    }
}

#[derive(Debug)]
pub struct SinkError {
    pub source: Box<dyn std::error::Error>,
}

impl Display for SinkError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.source.to_string())
    }
}

impl std::error::Error for SinkError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&*self.source)
    }
}

#[derive(Debug)]
pub struct SourceError {
    pub source: Box<dyn std::error::Error>,
}

impl Display for SourceError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.source.to_string())
    }
}

impl std::error::Error for SourceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&*self.source)
    }
}
