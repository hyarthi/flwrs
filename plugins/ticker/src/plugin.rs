//! The ticker as an SDK `Source`.

use crate::schema;
use crate::ticker::{Output, Tick, Ticker};
use flwrs_plugin::plugin::core::InitializeRequest;
use flwrs_plugin::plugin::error::{InitializeError, ShutdownError, SourceError};
use flwrs_plugin::schema::protocol::log_level::Enum as LogLevel;
use flwrs_plugin::source::local_sink::LocalSink;
use flwrs_plugin::source::plugin::Source;

pub(crate) struct TickerSource {
    pub ticker: Ticker,
    pub label: String,
    pub confirmed: bool,
    pub sink: Option<LocalSink>,
}

struct Hub<'a> {
    sink: &'a LocalSink,
    label: &'a str,
    confirmed: bool,
}

impl Output for Hub<'_> {
    async fn ready(&self) -> Result<(), String> {
        self.sink.ready().await.map_err(|e| e.to_string())
    }

    async fn emit(&self, tick: &Tick) -> Result<(), String> {
        let payload = schema::payload(tick, self.label);
        let sent = if self.confirmed {
            self.sink.emit_confirmed(payload).await
        } else {
            self.sink.emit(payload).await
        };
        sent.map_err(|e| e.to_string())
    }
}

impl Source for TickerSource {
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
            .with_out_schema(schema::build_schema()))
    }

    fn shutdown(&mut self) -> Result<(), ShutdownError> {
        Ok(())
    }

    fn version(&self) -> String {
        env!("CARGO_PKG_VERSION").into()
    }

    async fn run(&self) -> Result<(), SourceError> {
        let sink = self
            .sink
            .as_ref()
            .ok_or_else(|| SourceError::new("run before initialize"))?;
        let out = Hub {
            sink,
            label: &self.label,
            confirmed: self.confirmed,
        };
        self.ticker.run(&out).await.map_err(SourceError::new)?;
        log::info!("The schedule has ended; no more ticks");
        Ok(())
    }
}
