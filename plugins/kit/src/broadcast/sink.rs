//! A push sink's `consume_event`: an event into its channel, answered by
//! the ack mode, with the refusals mapped to NACKs.

use super::{AckMode, Broadcaster, Refused, Undelivered, message_of};
use flwrs_plugin::plugin::core::InboundEvent;
use flwrs_plugin::plugin::error::SinkError;

/// Publishes an event and waits for its ack (channel and size refusals are final;
/// limits and undelivered events retryable).
pub async fn publish(
    broadcaster: &Broadcaster,
    ack_mode: AckMode,
    event: &InboundEvent,
) -> Result<(), SinkError> {
    let (channel, message) = message_of(&event.payload);
    let delivery = match broadcaster.publish(channel.as_deref(), message, ack_mode) {
        Ok(d) => d,
        Err(e @ (Refused::TooManyChannels | Refused::TooManySubscribers)) => {
            return Err(SinkError::retryable(e.to_string()));
        }
        Err(e) => return Err(SinkError::new(e.to_string())),
    };
    delivery.done().await.map_err(|u| {
        SinkError::retryable(match u {
            Undelivered::Timeout => "no subscriber took it within delivery_timeout_ms",
            Undelivered::Evicted => {
                "newer events pushed it out of the replay ring before a subscriber took it"
            }
            Undelivered::Closed => "the plugin is stopping",
        })
    })
}
