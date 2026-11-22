//! The vision source as an SDK `Source`. For now it loads its graph's
//! models, connects and emits nothing: there's no camera input yet.

use crate::settings::Settings;
use flwrs_cv_source::graph::exec::Plan;
use flwrs_plugin::plugin::core::{InitializeRequest, SchemaDefinition};
use flwrs_plugin::plugin::error::{InitializeError, ShutdownError, SourceError};
use flwrs_plugin::schema::protocol::log_level::Enum as LogLevel;
use flwrs_plugin::source::local_sink::LocalSink;
use flwrs_plugin::source::plugin::Source;

pub(crate) struct VisionSource {
    pub settings: Settings,
    /// The graph, ready to run on frames.
    pub plan: std::sync::Mutex<Plan>,
    /// From loading (models that fell back to the CPU).
    pub warnings: Vec<String>,
    pub sink: Option<LocalSink>,
}

impl Source for VisionSource {
    fn initialize(
        &mut self,
        plugin_id: String,
        _: LogLevel,
        sink: LocalSink,
    ) -> Result<InitializeRequest, InitializeError> {
        self.sink = Some(sink);
        Ok(InitializeRequest::new()
            .with_id(plugin_id)
            .with_version(self.version())
            .with_out_schema(SchemaDefinition::from(self.settings.out_schema.clone())))
    }

    fn shutdown(&mut self) -> Result<(), ShutdownError> {
        Ok(())
    }

    fn version(&self) -> String {
        env!("CARGO_PKG_VERSION").into()
    }

    async fn run(&self) -> Result<(), SourceError> {
        for w in &self.warnings {
            log::warn!("{w}");
        }
        let devices: Vec<String> = match self.plan.lock() {
            Ok(plan) => plan
                .devices()
                .into_iter()
                .map(|(id, d)| format!("{id} on {d}"))
                .collect(),
            Err(_) => vec![],
        };
        log::info!(
            "Graph of {} nodes loaded ({}); models: {}; no camera yet, so no events",
            self.settings.graph.nodes.len(),
            self.settings.checked.order.join(" → "),
            if devices.is_empty() {
                "none".to_string()
            } else {
                devices.join(", ")
            }
        );
        // Cancelled by the runner on shutdown or a disconnect.
        std::future::pending::<()>().await;
        Ok(())
    }
}
