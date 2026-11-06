//! Telemetry: optional sinks for the server's own stats (`[telemetry]`).
//! Every sink reads the director's stats on its own schedule, as the
//! neutral [`samples`] list, and writes them in its format:
//!
//! - `[telemetry.otlp]`: an OpenTelemetry collector, over OTLP/HTTP
//!   (protobuf); needs the `otel` feature (on by default).
//! - `[[telemetry.file]]`: local time-partitioned files, one entry per
//!   format; needs the `telemetry-file` feature (on by default).
//!
//! No sink configured: nothing runs.

pub(crate) mod config;
#[cfg(feature = "telemetry-file")]
pub(crate) mod file;
#[cfg(feature = "otel")]
pub(crate) mod otlp;
pub(crate) mod samples;

use async_trait::async_trait;
use config::TelemetryConfig;
use flwrs_core::registry::{self, RegistryError};
use std::sync::OnceLock;
use tokio_util::sync::CancellationToken;

pub(crate) struct Service {
    config: TelemetryConfig,
}

static SERVICE: OnceLock<Service> = OnceLock::new();

/// The telemetry service, if any sink is configured.
pub(crate) fn service() -> Option<&'static dyn registry::Service> {
    let config = TelemetryConfig::read();
    if config == TelemetryConfig::default() {
        return None;
    }
    Some(SERVICE.get_or_init(|| Service { config }))
}

#[async_trait]
impl registry::Service for Service {
    fn id(&self) -> String {
        "telemetry".to_string()
    }

    async fn start(&self, shutdown: CancellationToken) -> Result<(), RegistryError> {
        #[cfg_attr(
            not(any(feature = "otel", feature = "telemetry-file")),
            allow(unused_mut)
        )]
        let mut sinks: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();
        #[cfg(feature = "telemetry-file")]
        {
            let mut seen = std::collections::HashSet::new();
            for (i, file) in self.config.files.iter().enumerate() {
                let sink = match file.resolve() {
                    Ok(sink) => sink,
                    Err(err) => {
                        log::error!(
                            "Telemetry: [[telemetry.file]] #{}: {err}; not writing",
                            i + 1
                        );
                        continue;
                    }
                };
                if !seen.insert((sink.dir.clone(), sink.format)) {
                    log::error!(
                        "Telemetry: [[telemetry.file]] #{}: another entry already writes {} to {}; not writing",
                        i + 1,
                        sink.format.name(),
                        sink.dir.display()
                    );
                    continue;
                }
                sinks.spawn(file::run(
                    crate::modules::director::init(),
                    sink,
                    shutdown.clone(),
                ));
            }
        }
        #[cfg(not(feature = "telemetry-file"))]
        if !self.config.files.is_empty() {
            log::warn!(
                "Telemetry: [[telemetry.file]] is set, but this server was built without the telemetry-file feature; not writing"
            );
        }
        if let Some(otlp) = &self.config.otlp {
            #[cfg(feature = "otel")]
            {
                let resolved = otlp.resolve(|var| std::env::var(var).ok());
                sinks.spawn(otlp::run(
                    crate::modules::director::init(),
                    resolved,
                    shutdown.clone(),
                ));
            }
            #[cfg(not(feature = "otel"))]
            {
                let _ = otlp;
                log::warn!(
                    "Telemetry: [telemetry.otlp] is set, but this server was built without the otel feature; not exporting"
                );
            }
        }
        sinks.join_all().await;
        shutdown.cancelled().await;
        Ok(())
    }
}
