//! Trellis, the conformance suite for flwrs plugins: runs any plugin executable against a
//! fake hub and checks the rules of the plugin protocol (`docs/protocol.md`).
//! Each case starts the plugin afresh.
//!
//! Use it as a library ([`run`]) or through the `flwrs-trellis` binary.

use flwrs_plugin::plugin::frame::{
    Frame, FrameError, PROTOCOL_VERSION, WireMessage, encode_frame, encode_message, header,
    read_frame,
};
use flwrs_plugin::schema::payload::{
    ArrayValue, Field, FieldDefinition, FieldValue, MapValue, PackedF32, PackedF64, PackedI32,
    PackedI64, PackedU32, PackedU64, PluginPayload, SchemaDefinition,
    field_type::Enum as FieldType, field_value::Value,
};
use flwrs_plugin::schema::protocol::{
    Ack, Credit, Event, EventMetadata, Exit, FrameHeader, Initialize, InitializeResponse, Nack,
    Ping, Pong, Shutdown, capability::Enum as Capability, msg_code::Enum as MsgCode,
    plugin_type::Enum as PluginType,
};
use flwrs_plugin::schema::rules;
use prost::Message;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, Command};

/// The instance id the plugin is started with.
pub const INSTANCE_ID: &str = "conformance-node";

/// A plugin to check.
#[derive(Debug, Clone)]
pub struct Target {
    pub program: PathBuf,
    /// Extra arguments after the bootstrap ones.
    pub args: Vec<String>,
    /// Working directory (a package's folder).
    pub cwd: PathBuf,
    pub plugin_type: PluginType,
    pub version: String,
    /// From the manifest: what the plugin must declare.
    pub in_schema: Option<SchemaDefinition>,
    pub out_schema: Option<SchemaDefinition>,
    pub env: Vec<(String, String)>,
}

impl Target {
    /// From a plugin package folder (`manifest.json` at its root).
    pub fn from_package(dir: &Path) -> Result<Self, String> {
        let read = |file: &str| {
            std::fs::read_to_string(dir.join(file)).map_err(|e| format!("{file}: {e}"))
        };
        let manifest: serde_json::Value =
            serde_json::from_str(&read("manifest.json")?).map_err(|e| format!("manifest.json: {e}"))?;
        let field = |name: &str| {
            manifest[name]
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| format!("manifest.json: no {name:?}"))
        };
        let plugin_type = match field("type")?.as_str() {
            "SOURCE" => PluginType::Source,
            "SINK" => PluginType::Sink,
            "TRANSFORM" => PluginType::Transform,
            other => return Err(format!("manifest.json: unknown type {other:?}")),
        };
        let schema = |side: &str| -> Result<Option<SchemaDefinition>, String> {
            match manifest["payload_schemas"][side].as_str() {
                None => Ok(None),
                Some(file) => rules::parse_json(&read(file)?)
                    .map(Some)
                    .map_err(|e| format!("{file}: {e}")),
            }
        };
        Ok(Self {
            program: dir.join(field("executable")?),
            args: vec![],
            cwd: dir.to_path_buf(),
            plugin_type,
            version: field("version")?,
            in_schema: schema("in")?,
            out_schema: schema("out")?,
            env: vec![],
        })
    }

    fn consumes(&self) -> bool {
        self.plugin_type != PluginType::Source
    }

    fn produces(&self) -> bool {
        self.plugin_type != PluginType::Sink
    }
}

/// How a case went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Pass,
    Fail(String),
    /// Doesn't apply to this plugin, or couldn't be observed.
    Skip(String),
}

#[derive(Debug, Clone)]
pub struct CaseResult {
    pub name: &'static str,
    /// What the case checks, in a few words.
    pub checks: &'static str,
    pub outcome: Outcome,
}

/// The cases, in order.
pub const CASES: &[(&str, &str)] = &[
    ("handshake", "Initialize first, with the right id, token, type, version, protocol range and schemas"),
    ("waits-for-acceptance", "no data, acks or pings before InitializeResponse"),
    ("answers-ping", "PING is answered with PONG and the same nonce"),
    ("data-events", "events flow by the rules: seq numbers, credit, acks, input_seq"),
    ("shuts-down", "SHUTDOWN is answered with EXIT, then the process exits"),
    ("stops-when-rejected", "SHUTDOWN instead of InitializeResponse: exits, no reconnect"),
    ("reconnects", "after a dropped connection: reconnects with the same Initialize"),
    ("closes-on-violation", "a frame with an unknown framing version closes the connection"),
];

/// Runs every case (or those named in `only`) against `target`.
pub async fn run(target: &Target, only: &[String]) -> Vec<CaseResult> {
    let mut results = vec![];
    for (name, checks) in CASES {
        if !only.is_empty() && !only.iter().any(|o| o == name) {
            continue;
        }
        let outcome = match run_case(target, name).await {
            Ok(outcome) => outcome,
            Err(failure) => Outcome::Fail(failure),
        };
        results.push(CaseResult {
            name,
            checks,
            outcome,
        });
    }
    results
}

type CaseResultInner = Result<Outcome, String>;

async fn run_case(target: &Target, name: &str) -> CaseResultInner {
    let mut fixture = Fixture::start(target).await?;
    let outcome = match name {
        "handshake" => handshake(&mut fixture).await,
        "waits-for-acceptance" => waits_for_acceptance(&mut fixture).await,
        "answers-ping" => answers_ping(&mut fixture).await,
        "data-events" => data_events(&mut fixture).await,
        "shuts-down" => shuts_down(&mut fixture).await,
        "stops-when-rejected" => stops_when_rejected(&mut fixture).await,
        "reconnects" => reconnects(&mut fixture).await,
        "closes-on-violation" => closes_on_violation(&mut fixture).await,
        other => Err(format!("unknown case {other}")),
    };
    fixture.stop().await;
    outcome
}

/// A fake hub with the plugin running against it.
struct Fixture {
    target: Target,
    listener: TcpListener,
    child: Child,
    token: String,
}

impl Fixture {
    async fn start(target: &Target) -> Result<Self, String> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| format!("fake hub: {e}"))?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        let token = random_token();
        let child = Command::new(&target.program)
            .args([
                "--id",
                INSTANCE_ID,
                "--host",
                "127.0.0.1",
                "--port",
                &port.to_string(),
                "--log-level",
                "debug",
            ])
            .args(&target.args)
            .current_dir(&target.cwd)
            .env("FLWRS_TOKEN", &token)
            .envs(target.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("couldn't start {}: {e}", target.program.display()))?;
        Ok(Self {
            target: target.clone(),
            listener,
            child,
            token,
        })
    }

    /// The plugin's next connection.
    async fn accept(&self, wait: Duration) -> Result<Conn, String> {
        match tokio::time::timeout(wait, self.listener.accept()).await {
            Ok(Ok((stream, _))) => Ok(Conn {
                stream,
                next_seq: 1,
            }),
            Ok(Err(e)) => Err(format!("accept: {e}")),
            Err(_) => Err(format!("the plugin didn't connect within {} s", wait.as_secs())),
        }
    }

    /// Accepts a connection and reads its Initialize (the first frame).
    async fn handshake(&self) -> Result<(Conn, Initialize), String> {
        let mut conn = self.accept(Duration::from_secs(10)).await?;
        let first = conn.frame(Duration::from_secs(10)).await?.ok_or("the plugin closed the connection before Initialize")?;
        if first.code() != MsgCode::Initialize {
            return Err(format!("the first frame is {:?}, not Initialize", first.code()));
        }
        let init = decode::<Initialize>(&first)?;
        Ok((conn, init))
    }

    /// Handshake, then accept it with every capability it offered.
    async fn accepted(&self) -> Result<(Conn, Initialize, Negotiated), String> {
        let (mut conn, init) = self.handshake().await?;
        let has = |c: Capability| init.capabilities.contains(&(c as i32));
        let negotiated = Negotiated {
            acks: has(Capability::Acks),
            credits: has(Capability::Credits) && self.target.produces(),
            max_unacked: if init.max_unacked == 0 { 64 } else { init.max_unacked.min(1024) },
        };
        let mut caps = vec![];
        if negotiated.acks {
            caps.push(Capability::Acks as i32);
        }
        if negotiated.credits {
            caps.push(Capability::Credits as i32);
        }
        conn.send(&InitializeResponse {
            protocol_version: PROTOCOL_VERSION,
            capabilities: caps,
            heartbeat_interval_ms: 0,
            max_unacked: if negotiated.acks && self.target.consumes() {
                negotiated.max_unacked
            } else {
                0
            },
        })
        .await?;
        Ok((conn, init, negotiated))
    }

    async fn exits(&mut self, wait: Duration) -> bool {
        tokio::time::timeout(wait, self.child.wait()).await.is_ok()
    }

    async fn stop(mut self) {
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
    }
}

#[derive(Debug, Clone, Copy)]
struct Negotiated {
    acks: bool,
    credits: bool,
    max_unacked: u32,
}

struct Conn {
    stream: TcpStream,
    /// Next seq for an event the fake hub sends.
    next_seq: u64,
}

impl Conn {
    async fn send<M: WireMessage>(&mut self, msg: &M) -> Result<(), String> {
        self.raw(&encode_message(msg)).await
    }

    async fn raw(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.stream
            .write_all(bytes)
            .await
            .map_err(|e| format!("writing to the plugin: {e}"))
    }

    async fn send_event(&mut self, payload: PluginPayload, n: usize) -> Result<u64, String> {
        let seq = self.next_seq;
        self.next_seq += 1;
        let header = FrameHeader {
            code: MsgCode::Event as i32,
            seq,
            metadata: Some(EventMetadata {
                event_id: format!("conformance-{n}"),
                trace_id: format!("trace-{n}"),
                hops: 1,
                origin_node_id: "conformance-source".into(),
                create_time: None,
            }),
            attempt: 1,
            ..Default::default()
        };
        let body = Event {
            payload: Some(payload),
        }
        .encode_to_vec();
        self.raw(&encode_frame(&header, &body)).await?;
        Ok(seq)
    }

    /// The next frame; `None` if the plugin closed the connection.
    async fn frame(&mut self, wait: Duration) -> Result<Option<Frame>, String> {
        match tokio::time::timeout(wait, read_frame(&mut self.stream)).await {
            Ok(Ok(frame)) => Ok(frame),
            Ok(Err(e)) => Err(format!("invalid frame from the plugin: {e}")),
            Err(_) => Err(format!("nothing from the plugin within {} ms", wait.as_millis())),
        }
    }

    /// The next frame that isn't a log or error record.
    async fn next(&mut self, wait: Duration) -> Result<Option<Frame>, String> {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            let Some(frame) = self.frame(left).await? else {
                return Ok(None);
            };
            if !matches!(frame.code(), MsgCode::Log | MsgCode::Error) {
                return Ok(Some(frame));
            }
        }
    }

    /// Reads until the plugin closes the connection.
    async fn closed(&mut self, wait: Duration) -> Result<(), String> {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(left, read_frame(&mut self.stream)).await {
                Ok(Ok(None)) | Ok(Err(_)) => return Ok(()),
                Ok(Ok(Some(_))) => continue,
                Err(_) => {
                    return Err(format!(
                        "the plugin kept the connection open for {} s",
                        wait.as_secs()
                    ));
                }
            }
        }
    }
}

fn decode<M: WireMessage>(frame: &Frame) -> Result<M, String> {
    frame.decode::<M>().map_err(|e| match e {
        FrameError::Decode(e) => format!("undecodable {:?}: {e}", frame.code()),
        other => other.to_string(),
    })
}

fn random_token() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("the OS has random numbers");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn type_name(t: PluginType) -> &'static str {
    t.as_str_name()
}

// --- the cases ---

async fn handshake(f: &mut Fixture) -> CaseResultInner {
    let (_, init) = f.handshake().await?;
    let mut problems = vec![];
    if init.instance_id != INSTANCE_ID {
        problems.push(format!("instance_id {:?}, expected {INSTANCE_ID:?} (from --id)", init.instance_id));
    }
    if init.token != f.token {
        problems.push("token isn't FLWRS_TOKEN".to_string());
    }
    if init.plugin_type != f.target.plugin_type as i32 {
        problems.push(format!(
            "plugin_type {}, the manifest says {}",
            init.plugin_type,
            type_name(f.target.plugin_type)
        ));
    }
    if init.plugin_version != f.target.version {
        problems.push(format!(
            "plugin_version {:?}, the manifest says {:?}",
            init.plugin_version, f.target.version
        ));
    }
    if !(init.min_protocol_version..=init.max_protocol_version).contains(&PROTOCOL_VERSION) {
        problems.push(format!(
            "protocol versions {}..{} don't include {PROTOCOL_VERSION}",
            init.min_protocol_version, init.max_protocol_version
        ));
    }
    for (side, manifest, declared, needed) in [
        ("in", &f.target.in_schema, &init.in_schema, f.target.consumes()),
        ("out", &f.target.out_schema, &init.out_schema, f.target.produces()),
    ] {
        match (needed, manifest, declared) {
            (false, _, _) => {}
            (true, _, None) => problems.push(format!("{side}_schema is missing")),
            (true, manifest, Some(declared)) => {
                problems.extend(rules::check_well_formed(declared).into_iter().map(|p| format!("{side}_schema: {p}")));
                if let Some(manifest) = manifest {
                    problems.extend(rules::diff(manifest, declared).into_iter().map(|d| format!("{side}_schema.{d}")));
                }
            }
        }
    }
    if problems.is_empty() {
        Ok(Outcome::Pass)
    } else {
        Err(problems.join("; "))
    }
}

async fn waits_for_acceptance(f: &mut Fixture) -> CaseResultInner {
    let (mut conn, _) = f.handshake().await?;
    match conn.next(Duration::from_millis(700)).await {
        Err(_) => Ok(Outcome::Pass),
        Ok(None) => Err("the plugin closed the connection while waiting for InitializeResponse".into()),
        Ok(Some(frame)) if frame.code() == MsgCode::Exit => Ok(Outcome::Pass),
        Ok(Some(frame)) => Err(format!("sent {:?} before InitializeResponse", frame.code())),
    }
}

async fn answers_ping(f: &mut Fixture) -> CaseResultInner {
    let (mut conn, _, negotiated) = f.accepted().await?;
    if negotiated.credits {
        // A source shouldn't be blocked from answering by having no credit.
        conn.send(&Credit { events: 0 }).await?;
    }
    conn.send(&Ping { nonce: 4242 }).await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        let frame = conn
            .next(left)
            .await
            .map_err(|_| "no PONG within 3 s".to_string())?
            .ok_or("the plugin closed the connection")?;
        match frame.code() {
            MsgCode::Pong => {
                let pong: Pong = decode(&frame)?;
                return if pong.nonce == 4242 {
                    Ok(Outcome::Pass)
                } else {
                    Err(format!("PONG with nonce {}, expected 4242", pong.nonce))
                };
            }
            MsgCode::Event if f.target.produces() => continue, // a source's events
            other => return Err(format!("unexpected {other:?} while waiting for PONG")),
        }
    }
}

async fn data_events(f: &mut Fixture) -> CaseResultInner {
    let (mut conn, _, negotiated) = f.accepted().await?;
    // Like a hub: an initial grant, and a credit back for each event routed.
    let mut granted: u64 = if negotiated.credits { 3 } else { u64::MAX };
    if negotiated.credits {
        conn.send(&Credit { events: 3 }).await?;
    }
    let mut problems = vec![];
    let mut sent: Vec<u64> = vec![];
    if f.target.consumes() {
        let schema = f.target.in_schema.clone().unwrap_or_default();
        let window = if negotiated.acks { negotiated.max_unacked.min(5) } else { 5 };
        for n in 0..window as usize {
            sent.push(conn.send_event(sample_payload(&schema), n).await?);
        }
    }
    // Watch for a while: outputs (producers), acks (consumers with ACKS).
    let mut outputs = 0u64;
    let mut next_output_seq = 1u64;
    let mut answered: HashMap<u64, bool> = HashMap::new();
    let watch = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let left = watch.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            break;
        }
        let frame = match conn.next(left).await {
            Err(_) => break,
            Ok(None) => {
                problems.push("the plugin closed the connection".to_string());
                break;
            }
            Ok(Some(frame)) => frame,
        };
        match frame.code() {
            MsgCode::Event if f.target.produces() => {
                outputs += 1;
                if frame.header.seq != next_output_seq {
                    problems.push(format!("event seq {}, expected {next_output_seq}", frame.header.seq));
                }
                next_output_seq = frame.header.seq + 1;
                if outputs > granted {
                    problems.push(format!("sent {outputs} events with {granted} credit"));
                }
                if negotiated.credits && outputs <= 20 {
                    conn.send(&Credit { events: 1 }).await?;
                    granted += 1;
                }
                if frame.header.flags != 0 {
                    problems.push("an event with flags set".to_string());
                }
                let input = frame.header.input_seq;
                if input != 0 && (f.target.plugin_type != PluginType::Transform || !sent.contains(&input)) {
                    problems.push(format!("input_seq {input} isn't an input it got"));
                }
                if decode::<Event>(&frame).is_err() {
                    problems.push("an undecodable event".to_string());
                }
            }
            MsgCode::Ack if negotiated.acks && f.target.consumes() => {
                let ack: Ack = decode(&frame)?;
                for seq in &sent {
                    if *seq <= ack.up_to || ack.seqs.contains(seq) {
                        answered.entry(*seq).or_insert(true);
                    }
                }
            }
            MsgCode::Nack if negotiated.acks && f.target.consumes() => {
                let nack: Nack = decode(&frame)?;
                answered.insert(nack.seq, false);
            }
            MsgCode::Exit => {
                problems.push(format!("sent Exit: {:?}", decode::<Exit>(&frame)?.message));
                break;
            }
            other => problems.push(format!("unexpected {other:?}")),
        }
        if f.target.consumes() && negotiated.acks && answered.len() == sent.len() && !f.target.produces() {
            break;
        }
    }
    if negotiated.acks && f.target.consumes() {
        let missing: Vec<u64> = sent.iter().copied().filter(|s| !answered.contains_key(s)).collect();
        if !missing.is_empty() {
            problems.push(format!("events never acked or nacked: {missing:?}"));
        }
    }
    if !problems.is_empty() {
        return Err(problems.join("; "));
    }
    if f.target.plugin_type == PluginType::Source && outputs == 0 {
        return Ok(Outcome::Skip("the source emitted nothing within 5 s".into()));
    }
    Ok(Outcome::Pass)
}

async fn shuts_down(f: &mut Fixture) -> CaseResultInner {
    let (mut conn, _, _) = f.accepted().await?;
    conn.send(&Shutdown {
        reason: "conformance".into(),
    })
    .await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match conn.next(left).await {
            Err(_) => return Err("no EXIT within 10 s of SHUTDOWN".into()),
            Ok(None) => return Err("closed the connection without EXIT".into()),
            Ok(Some(frame)) if frame.code() == MsgCode::Exit => {
                decode::<Exit>(&frame)?;
                break;
            }
            Ok(Some(_)) => {} // in-flight events, acks
        }
    }
    if f.exits(Duration::from_secs(10)).await {
        Ok(Outcome::Pass)
    } else {
        Err("sent EXIT but didn't exit within 10 s".into())
    }
}

async fn stops_when_rejected(f: &mut Fixture) -> CaseResultInner {
    let (mut conn, _) = f.handshake().await?;
    conn.send(&Shutdown {
        reason: "rejected by the conformance suite".into(),
    })
    .await?;
    let _ = conn.closed(Duration::from_secs(10)).await;
    if !f.exits(Duration::from_secs(10)).await {
        return Err("didn't exit within 10 s of being rejected".into());
    }
    match tokio::time::timeout(Duration::from_secs(2), f.listener.accept()).await {
        Err(_) => Ok(Outcome::Pass),
        Ok(_) => Err("reconnected after being rejected".into()),
    }
}

async fn reconnects(f: &mut Fixture) -> CaseResultInner {
    let (conn, first, _) = f.accepted().await?;
    drop(conn);
    let (_, again) = f
        .handshake()
        .await
        .map_err(|e| format!("after the connection dropped: {e} (the reconnect backoff may be at most 30 s)"))?;
    if again != first {
        return Err("the Initialize after reconnecting differs from the first".into());
    }
    Ok(Outcome::Pass)
}

async fn closes_on_violation(f: &mut Fixture) -> CaseResultInner {
    let (mut conn, _, _) = f.accepted().await?;
    // A Ping frame with framing version 9.
    let mut frame = encode_frame(&header(MsgCode::Ping), &Ping { nonce: 1 }.encode_to_vec());
    frame[4] = 9;
    conn.raw(&frame).await?;
    conn.closed(Duration::from_secs(5)).await?;
    Ok(Outcome::Pass)
}

/// A payload with a sample value for every field of `schema`.
pub fn sample_payload(schema: &SchemaDefinition) -> PluginPayload {
    PluginPayload {
        fields: schema
            .fields
            .iter()
            .map(|def| Field {
                key: def.key.clone(),
                value: Some(sample_value(def, 0)),
            })
            .collect(),
    }
}

fn sample_value(def: &FieldDefinition, depth: usize) -> FieldValue {
    let value = match def.r#type() {
        FieldType::String if def.key == "url" => Some(Value::String("http://127.0.0.1:9/conformance".into())),
        FieldType::String if def.key == "method" => Some(Value::String("POST".into())),
        FieldType::String => Some(Value::String("conformance".into())),
        FieldType::I32 => Some(Value::I32(-1)),
        FieldType::I64 => Some(Value::I64(-1)),
        FieldType::U32 => Some(Value::U32(1)),
        FieldType::U64 => Some(Value::U64(1)),
        FieldType::F32 => Some(Value::F32(0.5)),
        FieldType::F64 => Some(Value::F64(0.5)),
        FieldType::Bool => Some(Value::Bool(true)),
        FieldType::Datetime => Some(Value::DateTime(Default::default())),
        FieldType::Bytes => Some(Value::Bytes(b"conformance".to_vec())),
        FieldType::Array => def
            .nested_type_definition
            .as_deref()
            .filter(|_| depth < 8)
            .map(|nested| {
                Value::Array(ArrayValue {
                    value: vec![sample_value(nested, depth + 1)],
                })
            }),
        FieldType::Map => def
            .nested_type_definition
            .as_deref()
            .filter(|_| depth < 8)
            .map(|nested| {
                Value::Map(MapValue {
                    value: [("key".to_string(), sample_value(nested, depth + 1))].into(),
                })
            }),
        FieldType::Object => Some(Value::Object(MapValue {
            value: def
                .object_fields
                .iter()
                .filter(|_| depth < 8)
                .map(|f| (f.key.clone(), sample_value(f, depth + 1)))
                .collect(),
        })),
        FieldType::PackedF32 => Some(Value::PackedF32(PackedF32 { values: vec![0.5, 1.5] })),
        FieldType::PackedF64 => Some(Value::PackedF64(PackedF64 { values: vec![0.5, 1.5] })),
        FieldType::PackedI32 => Some(Value::PackedI32(PackedI32 { values: vec![-1, 1] })),
        FieldType::PackedI64 => Some(Value::PackedI64(PackedI64 { values: vec![-1, 1] })),
        FieldType::PackedU32 => Some(Value::PackedU32(PackedU32 { values: vec![1, 2] })),
        FieldType::PackedU64 => Some(Value::PackedU64(PackedU64 { values: vec![1, 2] })),
        FieldType::Undefined => None,
    };
    FieldValue { value }
}

/// A report for people: one line per case.
pub fn report(results: &[CaseResult]) -> String {
    let mut out = String::new();
    for r in results {
        let (status, detail) = match &r.outcome {
            Outcome::Pass => ("PASS", String::new()),
            Outcome::Fail(why) => ("FAIL", format!("\n       {why}")),
            Outcome::Skip(why) => ("SKIP", format!("\n       {why}")),
        };
        out.push_str(&format!("{status}  {:<22} {}{detail}\n", r.name, r.checks));
    }
    let failed = results.iter().filter(|r| matches!(r.outcome, Outcome::Fail(_))).count();
    out.push_str(&format!(
        "\n{} cases, {} passed, {} failed, {} skipped\n",
        results.len(),
        results.iter().filter(|r| r.outcome == Outcome::Pass).count(),
        failed,
        results.iter().filter(|r| matches!(r.outcome, Outcome::Skip(_))).count()
    ));
    out
}
