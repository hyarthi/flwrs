//! The process supervisor: runs one plugin instance per node. Local nodes are
//! launched as child processes (with a fresh token per launch), health-checked
//! and relaunched; remote nodes are waited for. Used by the director.

mod node;
mod process;
mod spec;
#[cfg(test)]
mod tests;
pub(crate) mod workdir;

pub(crate) use node::{NodeHandle, NodeOutput, NodeState, NodeStatus, Output, supervise};
pub(crate) use spec::{
    DeliverySpec, Launch, NodeSpec, QueueSpec, SpillSpec, Surroundings, runtime_defaults,
    server_log_level,
};
