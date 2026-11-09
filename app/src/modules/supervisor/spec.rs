//! Everything needed to run one node, resolved from the node's config and its
//! plugin's registry entry.

use crate::modules::hub::{ExpectedNode, NodeAuth};
use crate::modules::node_config::{
    EffectiveAuth, SceneDefaults,
    LaunchMode, NodeConfig, QueueConfig, RuntimeConfig, SpillConfig, WhenFull,
};
use crate::modules::plugin::manifest::{self, PluginType};
use crate::modules::plugin::service::Plugin;
use flwrs_plugin::schema::rules;
use flwrs_plugin::schema::payload::SchemaDefinition;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Built-in defaults. Precedence: the node's
/// `runtime` → the manifest's `runtime_defaults` → these.
pub(crate) const DEFAULT_INIT_TIMEOUT_MS: u64 = 10_000;
pub(crate) const DEFAULT_RECONNECT_TIMEOUT_MS: u64 = 60_000;
pub(crate) const DEFAULT_RESTART_DELAY_MS: u64 = 5_000;
pub(crate) const DEFAULT_KILL_GRACE_MS: u64 = 2_000;
pub(crate) const DEFAULT_SHUTDOWN_GRACE_MS: u64 = 10_000;
pub(crate) const DEFAULT_BACKOFF_MULTIPLIER: f64 = 2.0;
pub(crate) const DEFAULT_BACKOFF_MAX_DELAY_MS: u64 = 300_000;
pub(crate) const DEFAULT_BACKOFF_RESET_AFTER_MS: u64 = 300_000;

/// One node, ready to run.
#[derive(Debug, Clone)]
pub(crate) struct NodeSpec {
    pub scene_id: String,
    pub node_id: String,
    /// What the plugin sends in `Initialize` (and gets in events): the node
    /// id, or a remote node's `remote_id`.
    pub instance_id: String,
    pub plugin_id: String,
    pub plugin_type: PluginType,
    pub plugin_version: String,
    pub in_schema: Option<SchemaDefinition>,
    pub out_schema: Option<SchemaDefinition>,
    pub launch: Launch,
    pub timings: Timings,
    /// The node's inbound queue (sinks and transforms).
    pub queue: QueueSpec,
    pub delivery: DeliverySpec,
    pub max_event_bytes: Option<usize>,
}

#[derive(Debug, Clone)]
pub(crate) enum Launch {
    /// The supervisor starts the plugin as a child process.
    Local(LocalLaunch),
    /// The plugin runs elsewhere and connects with these credentials.
    Remote { auth: NodeAuth },
}

#[derive(Debug, Clone)]
pub(crate) struct LocalLaunch {
    /// Absolute path of the executable.
    pub program: PathBuf,
    /// Working directory: the plugin's install folder.
    pub cwd: PathBuf,
    /// The hub address the plugin connects to.
    pub hub_addr: SocketAddr,
    /// `--log-level` value.
    pub log_level: String,
    /// The node's `launch_args`, converted; appended after the bootstrap args.
    pub args: Vec<String>,
}

impl LocalLaunch {
    /// The full argument list: bootstrap args, then the node's own.
    pub(crate) fn argv(&self, node_id: &str) -> Vec<String> {
        let mut argv = vec![
            "--id".to_string(),
            node_id.to_string(),
            "--host".to_string(),
            self.hub_addr.ip().to_string(),
            "--port".to_string(),
            self.hub_addr.port().to_string(),
            "--log-level".to_string(),
            self.log_level.clone(),
        ];
        argv.extend(self.args.iter().cloned());
        argv
    }
}

pub(crate) const DEFAULT_QUEUE_CAPACITY: usize = 1024;
pub(crate) const DEFAULT_QUEUE_MAX_BYTES: usize = 64 * 1024 * 1024;
pub(crate) const DEFAULT_BLOCK_TIMEOUT_MS: u64 = 30_000;

/// A node's inbound queue settings, with defaults applied (same precedence
/// as the timings).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct QueueSpec {
    pub capacity: usize,
    pub max_bytes: usize,
    /// `None`: the director decides (`block`, or `drop_newest` on a cycle).
    pub when_full: Option<WhenFull>,
    pub block_timeout: Duration,
    /// `drop_oldest`: the most events one push may evict; `None` = no limit.
    pub max_evictions: Option<usize>,
    /// `runtime.queue.spill.enabled`; `None` = the director's `spill_default`.
    pub spill_enabled: Option<bool>,
    /// Limits that apply when spill is on.
    pub spill: SpillSpec,
    /// `runtime.queue.durable`; `None` = the scene's default.
    pub durable: Option<bool>,
}

pub(crate) const DEFAULT_MAX_ATTEMPTS: u32 = 5;
pub(crate) const DEFAULT_RETRY_DELAY_MS: u64 = 1_000;

/// Redelivery to a plugin with ACKS (`runtime.delivery`).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DeliverySpec {
    /// Deliveries of one event (the first included) before it's dead-lettered.
    pub max_attempts: u32,
    /// Wait before redelivering an event the plugin rejected with retry.
    pub retry_delay: Duration,
}

impl DeliverySpec {
    pub(crate) fn resolve(node: &RuntimeConfig, defaults: &RuntimeConfig) -> Self {
        let (n, d) = (node.delivery.as_ref(), defaults.delivery.as_ref());
        Self {
            max_attempts: n
                .and_then(|x| x.max_attempts)
                .or(d.and_then(|x| x.max_attempts))
                .map_or(DEFAULT_MAX_ATTEMPTS, |v| u32::try_from(v).unwrap_or(u32::MAX)),
            retry_delay: Duration::from_millis(
                n.and_then(|x| x.retry_delay_ms)
                    .or(d.and_then(|x| x.retry_delay_ms))
                    .unwrap_or(DEFAULT_RETRY_DELAY_MS),
            ),
        }
    }
}

pub(crate) const DEFAULT_SPILL_MEMORY_MAX_EVENTS: usize = 256;
pub(crate) const DEFAULT_SPILL_MEMORY_MAX_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const DEFAULT_SPILL_DISK_MAX_EVENTS: u64 = 1_000_000;
pub(crate) const DEFAULT_SPILL_DISK_MAX_BYTES: u64 = 1024 * 1024 * 1024;
pub(crate) const DEFAULT_SPILL_REFILL_WATERMARK: f64 = 0.25;
pub(crate) const DEFAULT_SPILL_WRITE_BATCH: usize = 256;

/// A spilling queue's limits (`runtime.queue.spill`).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SpillSpec {
    /// Memory soft limit: beyond it, new events go to disk.
    pub memory_max_events: usize,
    pub memory_max_bytes: usize,
    /// Disk limit: beyond it, the queue is full (`when_full` applies).
    pub disk_max_events: u64,
    pub disk_max_bytes: u64,
    /// Refill from disk once memory is below this share of its soft limit.
    pub refill_watermark: f64,
    /// Events per disk write transaction.
    pub write_batch: usize,
}

impl QueueSpec {
    pub(crate) fn resolve(node: &RuntimeConfig, defaults: &RuntimeConfig) -> Self {
        let (n, d) = (node.queue.as_ref(), defaults.queue.as_ref());
        let size = |f: fn(&QueueConfig) -> Option<u64>, builtin: usize| {
            n.and_then(f)
                .or(d.and_then(f))
                .map_or(builtin, |v| usize::try_from(v).unwrap_or(usize::MAX))
        };
        Self {
            capacity: size(|q| q.capacity, DEFAULT_QUEUE_CAPACITY),
            max_bytes: size(|q| q.max_bytes, DEFAULT_QUEUE_MAX_BYTES),
            when_full: n.and_then(|q| q.when_full).or(d.and_then(|q| q.when_full)),
            block_timeout: Duration::from_millis(
                n.and_then(|q| q.block_timeout_ms)
                    .or(d.and_then(|q| q.block_timeout_ms))
                    .unwrap_or(DEFAULT_BLOCK_TIMEOUT_MS),
            ),
            max_evictions: n
                .and_then(|q| q.max_evictions)
                .or(d.and_then(|q| q.max_evictions))
                .map(to_usize),
            spill_enabled: spill_field(n, d, |s| s.enabled),
            durable: n.and_then(|q| q.durable).or(d.and_then(|q| q.durable)),
            spill: SpillSpec {
                memory_max_events: spill_field(n, d, |s| s.memory_max_events)
                    .map_or(DEFAULT_SPILL_MEMORY_MAX_EVENTS, to_usize),
                memory_max_bytes: spill_field(n, d, |s| s.memory_max_bytes)
                    .map_or(DEFAULT_SPILL_MEMORY_MAX_BYTES, to_usize),
                disk_max_events: spill_field(n, d, |s| s.disk_max_events)
                    .unwrap_or(DEFAULT_SPILL_DISK_MAX_EVENTS),
                disk_max_bytes: spill_field(n, d, |s| s.disk_max_bytes)
                    .unwrap_or(DEFAULT_SPILL_DISK_MAX_BYTES),
                refill_watermark: spill_field(n, d, |s| s.refill_watermark)
                    .unwrap_or(DEFAULT_SPILL_REFILL_WATERMARK),
                write_batch: spill_field(n, d, |s| s.write_batch)
                    .map_or(DEFAULT_SPILL_WRITE_BATCH, to_usize),
            },
        }
    }
}

/// One spill setting, node first, then the manifest's defaults.
fn spill_field<T>(
    node: Option<&QueueConfig>,
    defaults: Option<&QueueConfig>,
    f: impl Fn(&SpillConfig) -> Option<T>,
) -> Option<T> {
    node.and_then(|q| q.spill.as_ref())
        .and_then(&f)
        .or_else(|| defaults.and_then(|q| q.spill.as_ref()).and_then(&f))
}

fn to_usize(v: u64) -> usize {
    usize::try_from(v).unwrap_or(usize::MAX)
}

/// Supervision timings, with every default applied.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Timings {
    pub init_timeout: Duration,
    pub reconnect_timeout: Duration,
    pub restart_delay: Duration,
    /// `None`: every relaunch waits `restart_delay`.
    pub backoff: Option<Backoff>,
    pub kill_grace: Duration,
    /// Consecutive failed launches before the node goes `Failed`; `None` = no limit.
    pub max_restarts: Option<u64>,
    pub shutdown_grace: Duration,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Backoff {
    pub multiplier: f64,
    pub max_delay: Duration,
    pub reset_after: Duration,
}

impl Timings {
    /// Applies the precedence node → manifest defaults → built-in.
    pub(crate) fn resolve(node: &RuntimeConfig, defaults: &RuntimeConfig) -> Self {
        let ms = |node: Option<u64>, manifest: Option<u64>, builtin: u64| {
            Duration::from_millis(node.or(manifest).unwrap_or(builtin))
        };
        let backoff = node
            .restart_backoff
            .as_ref()
            .or(defaults.restart_backoff.as_ref())
            .map(|b| Backoff {
                multiplier: b.multiplier.unwrap_or(DEFAULT_BACKOFF_MULTIPLIER),
                max_delay: Duration::from_millis(
                    b.max_delay_ms.unwrap_or(DEFAULT_BACKOFF_MAX_DELAY_MS),
                ),
                reset_after: Duration::from_millis(
                    b.reset_after_ms.unwrap_or(DEFAULT_BACKOFF_RESET_AFTER_MS),
                ),
            });
        Self {
            init_timeout: ms(
                node.init_timeout_ms,
                defaults.init_timeout_ms,
                DEFAULT_INIT_TIMEOUT_MS,
            ),
            reconnect_timeout: ms(
                node.reconnect_timeout_ms,
                defaults.reconnect_timeout_ms,
                DEFAULT_RECONNECT_TIMEOUT_MS,
            ),
            restart_delay: ms(
                node.restart_delay_ms,
                defaults.restart_delay_ms,
                DEFAULT_RESTART_DELAY_MS,
            ),
            backoff,
            kill_grace: ms(
                node.kill_grace_ms,
                defaults.kill_grace_ms,
                DEFAULT_KILL_GRACE_MS,
            ),
            max_restarts: node.max_restarts.or(defaults.max_restarts),
            shutdown_grace: ms(
                node.shutdown_grace_ms,
                defaults.shutdown_grace_ms,
                DEFAULT_SHUTDOWN_GRACE_MS,
            ),
        }
    }
}

/// Where a local plugin should connect: the hub's address, with an
/// unspecified IP (`0.0.0.0` / `::`) replaced by loopback.
pub(crate) fn local_hub_addr(hub: SocketAddr) -> SocketAddr {
    let ip = match hub.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        ip => ip,
    };
    SocketAddr::new(ip, hub.port())
}

/// The `--log-level` for plugins: the server's own level.
pub(crate) fn server_log_level() -> String {
    match log::max_level().to_level() {
        Some(level) => level.as_str().to_ascii_lowercase(),
        None => "error".into(),
    }
}

/// What resolving a node needs from the server and its scene.
pub(crate) struct Surroundings<'a> {
    /// The hub's plain listener (local plugins connect to it).
    pub hub_addr: SocketAddr,
    /// The hub has a TLS listener.
    pub tls_available: bool,
    pub log_level: &'a str,
    pub scene_defaults: &'a SceneDefaults,
}

impl NodeSpec {
    /// Resolves a node (its parsed `config`) running `plugin`. Validation is
    /// repeated here: the supervisor doesn't trust that the database is valid.
    pub(crate) fn resolve(
        scene_id: &str,
        node_id: &str,
        config: &NodeConfig,
        plugin: &Plugin,
        at: &Surroundings,
    ) -> Result<Self, Vec<String>> {
        let Surroundings {
            hub_addr,
            tls_available,
            log_level,
            scene_defaults,
        } = *at;
        let mut errors = vec![];
        let manifest = plugin
            .parsed_manifest()
            .ok_or_else(|| vec![format!("plugin [{}] has an invalid manifest", plugin.id)])?;
        let defaults = match &manifest.runtime_defaults {
            Some(value) => RuntimeConfig::parse(value).unwrap_or_else(|e| {
                errors.push(format!("manifest runtime_defaults: {e}"));
                RuntimeConfig::default()
            }),
            None => RuntimeConfig::default(),
        };
        errors.extend(config.runtime.validate("config.runtime"));

        let parse_schema = |json: &Option<String>, side: &str, errors: &mut Vec<String>| {
            json.as_deref().and_then(|j| {
                rules::parse_json(j)
                    .map_err(|e| errors.push(format!("plugin {side} schema: {e}")))
                    .ok()
            })
        };
        let in_schema = parse_schema(&plugin.in_schema, "in", &mut errors);
        let out_schema = parse_schema(&plugin.out_schema, "out", &mut errors);

        let launch = match config.launch {
            LaunchMode::Remote => {
                if let Some(unsupported) = config.unsupported() {
                    errors.push(unsupported);
                }
                match config.effective_auth(scene_defaults, tls_available) {
                    EffectiveAuth::Token { require_tls } => match &config.token {
                        Some(token) if !token.is_empty() => Launch::Remote {
                            auth: NodeAuth::Token {
                                token: token.clone(),
                                require_tls,
                            },
                        },
                        _ => {
                            errors.push(
                                "config.token is required for remote nodes with token auth".into(),
                            );
                            Launch::Remote {
                                auth: NodeAuth::local(String::new()),
                            }
                        }
                    },
                    EffectiveAuth::Mtls { pin } => {
                        if !tls_available {
                            errors.push(
                                "auth mtls needs the hub's TLS listener ([hub.tls])".into(),
                            );
                        }
                        Launch::Remote {
                            auth: NodeAuth::Mtls { pin },
                        }
                    }
                }
            }
            LaunchMode::Local => {
                let args =
                    manifest::launch_args_to_argv(manifest.launch_args_style, &config.launch_args)
                        .unwrap_or_else(|e| {
                            errors.extend(e.into_iter().map(|e| format!("config.{e}")));
                            vec![]
                        });
                let cwd = std::path::absolute(Path::new(&plugin.install_path))
                    .unwrap_or_else(|_| PathBuf::from(&plugin.install_path));
                Launch::Local(LocalLaunch {
                    program: cwd.join(&manifest.executable),
                    cwd,
                    hub_addr: local_hub_addr(hub_addr),
                    log_level: log_level.to_string(),
                    args,
                })
            }
        };
        if !errors.is_empty() {
            return Err(errors);
        }

        let max_event_bytes = config
            .runtime
            .queue
            .as_ref()
            .and_then(|q| q.max_event_bytes)
            .or(defaults.queue.as_ref().and_then(|q| q.max_event_bytes))
            .map(|n| usize::try_from(n).unwrap_or(usize::MAX));
        Ok(Self {
            scene_id: scene_id.to_string(),
            node_id: node_id.to_string(),
            instance_id: config.instance_id(node_id).to_string(),
            plugin_id: plugin.id.clone(),
            plugin_type: manifest.plugin_type,
            plugin_version: plugin.version.clone(),
            in_schema,
            out_schema,
            timings: Timings::resolve(&config.runtime, &defaults),
            queue: QueueSpec::resolve(&config.runtime, &defaults),
            delivery: DeliverySpec::resolve(&config.runtime, &defaults),
            launch,
            max_event_bytes,
        })
    }

    /// What the hub should accept for this node, with `token`.
    pub(crate) fn expected(&self, auth: NodeAuth) -> ExpectedNode {
        ExpectedNode {
            scene_id: self.scene_id.clone(),
            node_id: self.node_id.clone(),
            instance_id: self.instance_id.clone(),
            plugin_id: self.plugin_id.clone(),
            plugin_type: self.plugin_type,
            plugin_version: self.plugin_version.clone(),
            in_schema: self.in_schema.clone(),
            out_schema: self.out_schema.clone(),
            auth,
            max_event_bytes: self.max_event_bytes,
        }
    }

    /// `[scene/node]`, for log lines.
    pub(crate) fn tag(&self) -> String {
        format!("[{}/{}]", self.scene_id, self.node_id)
    }
}
