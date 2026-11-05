//! Helpers shared by the source, transform and sink runners.

use crate::plugin::core::{MAX_RECONNECT_DELAY, ReconnectPolicy};
use crate::plugin::error::{Error, ShutdownError};
use crate::plugin::frame::WireMessage;
use crate::plugin::logger;
use crate::plugin::msg_client::{HubClient, Session, Stage};
use crate::schema::common::{
    ErrorEvent, Exit, exit_code::Enum as ExitCode, plugin_type::Enum as PluginType,
};
use std::hash::{BuildHasher, RandomState};
use std::time::Duration;

/// Delay before restarting a source whose `run` failed.
pub(crate) const RESTART_BACKOFF: Duration = Duration::from_secs(1);

/// How long to wait for queued logs to reach the hub before sending `Exit`.
const LOG_FLUSH_TIMEOUT: Duration = Duration::from_secs(1);

pub(crate) fn error_event(plugin_id: &str, plugin_type: PluginType, message: String) -> ErrorEvent {
    ErrorEvent {
        plugin_id: plugin_id.to_string(),
        plugin_type: plugin_type as i32,
        message,
        details: vec![],
    }
}

/// How a session with the hub ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionEnd {
    /// The hub sent `Shutdown`. `accepted` is false if it came before
    /// `InitializeResponse`, i.e. the hub rejected this plugin.
    Shutdown { accepted: bool },
    /// The connection was lost; reconnect.
    Disconnected,
}

/// Reconnect delays: exponential, capped, with ±10 % jitter.
pub(crate) struct Backoff {
    policy: ReconnectPolicy,
    next: Duration,
}

impl Backoff {
    pub(crate) fn new(policy: ReconnectPolicy) -> Self {
        let policy = ReconnectPolicy {
            initial_delay: policy.initial_delay.min(MAX_RECONNECT_DELAY),
            max_delay: policy.max_delay.min(MAX_RECONNECT_DELAY),
        };
        Self {
            next: policy.initial_delay,
            policy,
        }
    }

    pub(crate) fn reset(&mut self) {
        self.next = self.policy.initial_delay;
    }

    pub(crate) fn next_delay(&mut self) -> Duration {
        let delay = self.next;
        self.next = (self.next * 2).min(self.policy.max_delay);
        // Jitter must not push past the protocol maximum the hub relies on.
        jitter(delay).min(MAX_RECONNECT_DELAY)
    }
}

/// Spreads `delay` by up to ±10 %, so many plugins don't retry in lockstep.
fn jitter(delay: Duration) -> Duration {
    // RandomState is seeded randomly per instance: good enough for jitter,
    // without pulling in a random number crate.
    let random = RandomState::new().hash_one(std::time::SystemTime::now());
    let factor = 0.9 + (random % 2001) as f64 / 10_000.0; // 0.9 ..= 1.1
    delay.mul_f64(factor)
}

/// Connects (retrying with backoff until it works) and sends `Initialize`.
pub(crate) async fn open_session<Out: WireMessage, In: WireMessage + Send + 'static>(
    client: &HubClient,
    initialize: &Out,
    backoff: &mut Backoff,
) -> Session<In> {
    let mut attempt: u32 = 0;
    loop {
        attempt += 1;
        let error = match client.connect().await {
            Ok((id, reader)) => {
                let session = Session::new(client, id, reader);
                match client.send_initialize(initialize).await {
                    Ok(()) => {
                        if attempt > 1 {
                            log::info!("Connected to the hub after {attempt} attempts");
                        }
                        return session;
                    }
                    Err(err) => err,
                }
            }
            Err(err) => err,
        };
        let delay = backoff.next_delay();
        // The hub can't receive logs right now, so tell the operator directly.
        eprintln!(
            "Can't connect to the hub at {} (attempt {attempt}): {error}; retrying in {} ms",
            client.addr(),
            delay.as_millis()
        );
        tokio::time::sleep(delay).await;
    }
}

/// Marks the session as accepted after `InitializeResponse`.
pub(crate) async fn accept<M>(client: &HubClient, session: &mut Session<M>, backoff: &mut Backoff) {
    if !session.accepted {
        session.accepted = true;
        client.set_ready(session.id).await;
        backoff.reset();
        log::debug!("The hub accepted the plugin");
    }
}

pub(crate) fn exit_message(shutdown: &Result<(), ShutdownError>) -> Exit {
    match shutdown {
        Ok(()) => Exit {
            code: ExitCode::Ok as i32,
            message: String::new(),
        },
        Err(err) => Exit {
            code: ExitCode::Error as i32,
            message: err.to_string(),
        },
    }
}

/// The end of a run: flush logs, send `Exit`, close the connection and send
/// further logs to stderr. Returns the runner's result.
pub(crate) async fn finish<M: WireMessage>(
    client: &HubClient,
    plugin_id: &str,
    end: SessionEnd,
    shutdown: Result<(), ShutdownError>,
    exit: impl FnOnce(Exit) -> M,
) -> Result<(), Error> {
    if let Err(err) = &shutdown {
        log::error!("Error shutting down: {err}");
    }
    let rejected = end == (SessionEnd::Shutdown { accepted: false });
    if rejected {
        let msg = "The hub rejected this plugin (Shutdown before InitializeResponse); \
                   check its instance id and token";
        log::error!("{msg}");
        eprintln!("{msg}");
    }
    log::info!("Plugin shutdown: {plugin_id}");
    logger::flush(LOG_FLUSH_TIMEOUT).await;
    let _ = client
        .send(&exit(exit_message(&shutdown)), Stage::Initializing)
        .await;
    client.disconnect(None).await;
    logger::close();

    if rejected {
        return Err(Error::Rejected);
    }
    shutdown.map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_up_to_the_cap_and_resets() {
        let mut backoff = Backoff::new(ReconnectPolicy {
            initial_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(5),
        });
        let delays: Vec<u128> = (0..5).map(|_| backoff.next_delay().as_millis()).collect();
        for (delay, expected) in delays.iter().zip([1000u128, 2000, 4000, 5000, 5000]) {
            assert!(
                (expected * 9 / 10..=expected * 11 / 10).contains(delay),
                "{delays:?}"
            );
        }
        backoff.reset();
        assert!(backoff.next_delay() <= Duration::from_millis(1100));
    }

    #[test]
    fn backoff_never_exceeds_the_protocol_maximum() {
        let mut backoff = Backoff::new(ReconnectPolicy {
            initial_delay: Duration::from_secs(100),
            max_delay: Duration::from_secs(600),
        });
        for _ in 0..3 {
            assert!(backoff.next_delay() <= MAX_RECONNECT_DELAY);
        }
    }
}
