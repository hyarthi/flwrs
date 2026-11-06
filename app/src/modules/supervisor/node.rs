//! One node's supervision task: launch (local) or wait (remote), the health
//! check, restarts, and the shutdown sequence.

use super::process::{PluginProcess, new_token};
use super::spec::{Launch, LocalLaunch, NodeSpec};
use crate::modules::hub::wire::{DataEvent, RuntimeMsg};
use crate::modules::hub::{Hub, HubEvent, LinkSender, NodeLink};
use flwrs_plugin::schema::common::Exit;
use serde::Serialize;
use std::hash::{BuildHasher, RandomState};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum NodeState {
    /// Local: the process was started; waiting for its `Initialize`.
    Launching,
    /// Remote: waiting for the plugin to connect.
    Waiting,
    /// Connected and accepted by the hub.
    Running,
    /// Lost its connection; waiting for the plugin to reconnect.
    Disconnected,
    /// Local: killed or crashed; waiting to be relaunched.
    Unhealthy,
    Stopping,
    Stopped,
    /// Won't be (re)launched until the scene is reloaded: the plugin declared
    /// the wrong type, version or schemas, or hit `max_restarts`.
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, utoipa::ToSchema)]
pub(crate) struct NodeStatus {
    pub state: NodeState,
    /// Local: processes started so far.
    pub launches: u64,
    /// Local: consecutive launches that never reached `Running`.
    pub failed_launches: u64,
    pub pid: Option<u32>,
    pub last_error: Option<String>,
}

/// A data event from a node's plugin, for the director.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct NodeOutput {
    pub node_id: Arc<str>,
    pub event: DataEvent,
    pub size: usize,
}

/// Controls one supervised node. Dropping it stops the node (in the
/// background); [`NodeHandle::stop`] stops it and waits.
pub(crate) struct NodeHandle {
    status: watch::Receiver<NodeStatus>,
    sender: LinkSender,
    stop: CancellationToken,
    task: Option<JoinHandle<NodeStatus>>,
}

impl NodeHandle {
    pub(crate) fn status(&self) -> NodeStatus {
        self.status.borrow().clone()
    }

    pub(crate) fn watch(&self) -> watch::Receiver<NodeStatus> {
        self.status.clone()
    }

    /// For sending events (and `Shutdown`) to the plugin. Stays valid across
    /// relaunches.
    pub(crate) fn sender(&self) -> LinkSender {
        self.sender.clone()
    }

    /// Runs the shutdown sequence and waits for it. Returns the final status.
    pub(crate) async fn stop(mut self) -> NodeStatus {
        self.stop.cancel();
        let last = self.status();
        match self.task.take() {
            Some(task) => task.await.unwrap_or(last),
            None => last,
        }
    }
}

impl Drop for NodeHandle {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

/// Starts supervising `spec`. Data events from the plugin go to `output`.
pub(crate) fn supervise(spec: NodeSpec, hub: &Hub, output: mpsc::Sender<NodeOutput>) -> NodeHandle {
    let token = match &spec.launch {
        Launch::Local(_) => new_token(),
        Launch::Remote { token } => token.clone(),
    };
    let link = hub.expect(spec.expected(token));
    let sender = link.sender();
    let initial = NodeStatus {
        state: match spec.launch {
            Launch::Local(_) => NodeState::Launching,
            Launch::Remote { .. } => NodeState::Waiting,
        },
        launches: 0,
        failed_launches: 0,
        pid: None,
        last_error: None,
    };
    let (status_tx, status_rx) = watch::channel(initial);
    let stop = CancellationToken::new();
    let node_id: Arc<str> = spec.node_id.as_str().into();
    let supervisor = Supervisor {
        tag: spec.tag(),
        node_id: node_id.clone(),
        spec,
        link,
        output,
        pending: None,
        status: status_tx,
        stop: stop.clone(),
    };
    NodeHandle {
        status: status_rx,
        sender,
        stop,
        task: Some(tokio::spawn(supervisor.run())),
    }
}

/// What the plugin did, as far as supervision cares (data events are
/// forwarded on the way).
#[derive(Debug)]
enum Signal {
    Connected,
    Rejected(Vec<String>),
    Exit(Exit),
    Disconnected(String),
    Stop,
}

/// How waiting for a launched process to come up ended.
enum Startup {
    Up,
    Failed(String),
    Rejected(Vec<String>),
    Stop,
}

/// How a running process's life ended.
enum Ending {
    /// The plugin sent `Exit` on its own: give it `kill_grace` to exit.
    Exited(String),
    /// Crashed, or unhealthy (didn't reconnect in time): kill it if needed.
    Lost(String),
    Rejected(Vec<String>),
    Stop,
}

struct Supervisor {
    spec: NodeSpec,
    tag: String,
    node_id: Arc<str>,
    link: NodeLink,
    output: mpsc::Sender<NodeOutput>,
    /// A data event waiting for room in `output` (kept across cancellation).
    pending: Option<NodeOutput>,
    status: watch::Sender<NodeStatus>,
    stop: CancellationToken,
}

impl Supervisor {
    async fn run(mut self) -> NodeStatus {
        match self.spec.launch.clone() {
            Launch::Local(launch) => self.run_local(&launch).await,
            Launch::Remote { .. } => self.run_remote().await,
        }
        self.status.borrow().clone()
    }

    fn update(&self, f: impl FnOnce(&mut NodeStatus)) {
        self.status.send_modify(f);
    }

    fn set_state(&self, state: NodeState) {
        self.update(|s| s.state = state);
    }

    /// The next signal from the plugin, forwarding data events meanwhile.
    /// With `stoppable`, a stop request ends the wait. Cancel-safe.
    async fn recv(&mut self, stoppable: bool) -> Signal {
        loop {
            if self.pending.is_some() {
                let permit = tokio::select! {
                    _ = self.stop.cancelled(), if stoppable => return Signal::Stop,
                    permit = self.output.reserve() => permit,
                };
                let output = self.pending.take().expect("checked above");
                match permit {
                    Ok(permit) => permit.send(output),
                    Err(_) => log::debug!("{} nobody takes events any more; dropped one", self.tag),
                }
            }
            let event = tokio::select! {
                _ = self.stop.cancelled(), if stoppable => return Signal::Stop,
                event = self.link.recv() => event,
            };
            match event {
                // The expectation was replaced: someone else supervises this node now.
                None => return Signal::Stop,
                Some(HubEvent::Event { event, size }) => {
                    self.pending = Some(NodeOutput {
                        node_id: self.node_id.clone(),
                        event,
                        size,
                    });
                }
                Some(HubEvent::Connected { .. }) => return Signal::Connected,
                Some(HubEvent::Rejected { reasons }) => return Signal::Rejected(reasons),
                Some(HubEvent::Exit { exit, .. }) => return Signal::Exit(exit),
                Some(HubEvent::Disconnected { reason, .. }) => {
                    return Signal::Disconnected(reason);
                }
            }
        }
    }

    // --- local ---

    async fn run_local(&mut self, launch: &LocalLaunch) {
        let timings = self.spec.timings.clone();
        let mut delay = timings.restart_delay;
        loop {
            let token = new_token();
            self.link.renew_token(token.clone());
            self.update(|s| {
                s.state = NodeState::Launching;
                s.launches += 1;
                s.pid = None;
            });
            let started = Instant::now();
            let (startup, mut process) =
                match PluginProcess::spawn(launch, &self.spec.node_id, &token, &self.tag) {
                    Ok(mut process) => {
                        log::info!(
                            "{} launched plugin [{}] (pid {})",
                            self.tag,
                            self.spec.plugin_id,
                            process.pid().map_or("?".into(), |p| p.to_string())
                        );
                        self.update(|s| s.pid = process.pid());
                        (self.until_running(&mut process).await, Some(process))
                    }
                    Err(err) => (
                        Startup::Failed(format!(
                            "couldn't start {}: {err}",
                            launch.program.display()
                        )),
                        None,
                    ),
                };

            let mut ran_for = Duration::ZERO;
            match startup {
                Startup::Up => {
                    let up_since = Instant::now();
                    self.update(|s| {
                        s.state = NodeState::Running;
                        s.failed_launches = 0;
                    });
                    let ending = match process.as_mut() {
                        Some(process) => self.while_running(process).await,
                        None => Ending::Lost("no process".into()),
                    };
                    ran_for = up_since.elapsed();
                    match ending {
                        Ending::Stop => return self.stop_local(process).await,
                        Ending::Rejected(reasons) => {
                            return self.fail_local(process, reasons.join("; ")).await;
                        }
                        Ending::Exited(reason) => {
                            log::warn!("{} {reason}; relaunching", self.tag);
                            self.update(|s| s.last_error = Some(reason));
                            if let Some(process) = process.as_mut()
                                && process.wait_for(timings.kill_grace).await.is_none()
                            {
                                process.terminate(timings.kill_grace).await;
                            }
                        }
                        Ending::Lost(reason) => {
                            log::warn!("{} {reason}; relaunching", self.tag);
                            self.update(|s| s.last_error = Some(reason));
                            if let Some(process) = process.as_mut() {
                                process.terminate(timings.kill_grace).await;
                            }
                        }
                    }
                }
                Startup::Stop => return self.stop_local(process).await,
                Startup::Rejected(reasons) => {
                    return self.fail_local(process, reasons.join("; ")).await;
                }
                Startup::Failed(reason) => {
                    log::warn!(
                        "{} unhealthy after {} ms: {reason}",
                        self.tag,
                        started.elapsed().as_millis()
                    );
                    if let Some(process) = process.as_mut() {
                        process.terminate(timings.kill_grace).await;
                    }
                    self.update(|s| {
                        s.failed_launches += 1;
                        s.last_error = Some(reason.clone());
                    });
                    let failed = self.status.borrow().failed_launches;
                    if let Some(max) = timings.max_restarts
                        && failed > max
                    {
                        let reason = format!(
                            "gave up after {failed} consecutive failed launches (max_restarts = {max}); last: {reason}"
                        );
                        return self.fail_local(None, reason).await;
                    }
                }
            }

            // Wait, then relaunch.
            self.update(|s| {
                s.state = NodeState::Unhealthy;
                s.pid = None;
            });
            if let Some(backoff) = &timings.backoff
                && ran_for >= backoff.reset_after
            {
                delay = timings.restart_delay;
            }
            let wait = match &timings.backoff {
                Some(_) => jitter(delay),
                None => delay,
            };
            tokio::select! {
                _ = self.stop.cancelled() => return self.stop_local(None).await,
                _ = tokio::time::sleep(wait) => {}
            }
            if let Some(backoff) = &timings.backoff {
                delay = delay.mul_f64(backoff.multiplier).min(backoff.max_delay);
            }
        }
    }

    /// Waits for the hub to accept the launched plugin, within `init_timeout`.
    async fn until_running(&mut self, process: &mut PluginProcess) -> Startup {
        let timeout = self.spec.timings.init_timeout;
        let deadline = tokio::time::sleep(timeout);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                signal = self.recv(true) => match signal {
                    Signal::Connected => return Startup::Up,
                    Signal::Rejected(reasons) => return Startup::Rejected(reasons),
                    Signal::Stop => return Startup::Stop,
                    // Not accepted yet, so these can't be about this launch.
                    Signal::Exit(_) | Signal::Disconnected(_) => {}
                },
                status = process.wait() => {
                    return Startup::Failed(match status {
                        Ok(status) => format!("process exited ({status}) before connecting"),
                        Err(err) => format!("process lost: {err}"),
                    });
                }
                _ = &mut deadline => {
                    return Startup::Failed(format!(
                        "no Initialize within {} ms (init_timeout_ms)",
                        timeout.as_millis()
                    ));
                }
            }
        }
    }

    async fn while_running(&mut self, process: &mut PluginProcess) -> Ending {
        let reconnect_timeout = self.spec.timings.reconnect_timeout;
        let mut reconnect_deadline: Option<std::pin::Pin<Box<tokio::time::Sleep>>> = None;
        loop {
            let deadline = async {
                match reconnect_deadline.as_mut() {
                    Some(deadline) => deadline.await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                signal = self.recv(true) => match signal {
                    Signal::Connected => {
                        reconnect_deadline = None;
                        self.set_state(NodeState::Running);
                    }
                    Signal::Disconnected(reason) => {
                        self.update(|s| {
                            s.state = NodeState::Disconnected;
                            s.last_error = Some(reason);
                        });
                        reconnect_deadline = Some(Box::pin(tokio::time::sleep(reconnect_timeout)));
                    }
                    Signal::Exit(exit) => {
                        return Ending::Exited(format!(
                            "unexpected Exit (code {}{})",
                            exit.code().as_str_name(),
                            if exit.message.is_empty() { String::new() } else { format!(": {}", exit.message) }
                        ));
                    }
                    Signal::Rejected(reasons) => return Ending::Rejected(reasons),
                    Signal::Stop => return Ending::Stop,
                },
                status = process.wait() => {
                    return Ending::Lost(match status {
                        Ok(status) => format!("process exited ({status})"),
                        Err(err) => format!("process lost: {err}"),
                    });
                }
                _ = deadline => {
                    return Ending::Lost(format!(
                        "didn't reconnect within {} ms (reconnect_timeout_ms)",
                        reconnect_timeout.as_millis()
                    ));
                }
            }
        }
    }

    /// `Shutdown` (if connected), up to `shutdown_grace` to exit, then
    /// SIGTERM / SIGKILL.
    async fn stop_local(&mut self, mut process: Option<PluginProcess>) {
        self.set_state(NodeState::Stopping);
        let timings = self.spec.timings.clone();
        if let Some(process) = process.as_mut() {
            let mut exited = false;
            if self.link.is_connected() && self.link.send(RuntimeMsg::Shutdown).await.is_ok() {
                let deadline = tokio::time::sleep(timings.shutdown_grace);
                tokio::pin!(deadline);
                // Keep forwarding events while the plugin drains.
                exited = loop {
                    tokio::select! {
                        _ = process.wait() => break true,
                        _ = self.recv(false) => {}
                        _ = &mut deadline => break false,
                    }
                };
            }
            if !exited {
                process.terminate(timings.kill_grace).await;
            }
        }
        log::info!("{} stopped", self.tag);
        self.update(|s| {
            s.state = NodeState::Stopped;
            s.pid = None;
        });
    }

    /// Marks the node `Failed` (no more launches) and waits to be stopped.
    async fn fail_local(&mut self, mut process: Option<PluginProcess>, reason: String) {
        log::error!("{} failed, not relaunching: {reason}", self.tag);
        if let Some(process) = process.as_mut() {
            process.terminate(self.spec.timings.kill_grace).await;
        }
        self.update(|s| {
            s.state = NodeState::Failed;
            s.pid = None;
            s.last_error = Some(reason);
        });
        self.stop.cancelled().await;
        self.update(|s| s.state = NodeState::Stopped);
    }

    // --- remote ---

    async fn run_remote(&mut self) {
        loop {
            match self.recv(true).await {
                Signal::Connected => self.set_state(NodeState::Running),
                Signal::Disconnected(reason) => {
                    log::warn!("{} remote plugin disconnected: {reason}", self.tag);
                    self.update(|s| {
                        s.state = NodeState::Disconnected;
                        s.last_error = Some(reason);
                    });
                }
                Signal::Exit(exit) => {
                    let reason = format!(
                        "remote plugin sent Exit (code {}); waiting for it to reconnect",
                        exit.code().as_str_name()
                    );
                    log::warn!("{} {reason}", self.tag);
                    self.update(|s| {
                        s.state = NodeState::Disconnected;
                        s.last_error = Some(reason);
                    });
                }
                Signal::Rejected(reasons) => self.update(|s| {
                    s.state = NodeState::Failed;
                    s.last_error = Some(reasons.join("; "));
                }),
                Signal::Stop => break,
            }
        }
        self.set_state(NodeState::Stopping);
        if self.link.is_connected() && self.link.send(RuntimeMsg::Shutdown).await.is_ok() {
            let grace = self.spec.timings.shutdown_grace;
            let _ = tokio::time::timeout(grace, async {
                loop {
                    if let Signal::Exit(_) | Signal::Disconnected(_) = self.recv(false).await {
                        return;
                    }
                }
            })
            .await;
        }
        log::info!("{} stopped", self.tag);
        self.set_state(NodeState::Stopped);
    }
}

/// ±10 %, so many failing nodes don't restart in lockstep.
fn jitter(delay: Duration) -> Duration {
    let random = RandomState::new().hash_one(Instant::now());
    delay.mul_f64(0.9 + (random % 2001) as f64 / 10_000.0)
}
