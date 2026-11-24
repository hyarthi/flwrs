//! `audit`: the conformance cases. Each case starts the plugin afresh against
//! the fake hub and checks one part of the plugin protocol (`docs/protocol.md`).

use crate::hub::{Fixture, decode};
use crate::payload::sample_payload;
use crate::stimulus::{Stimulus, StimulusCtx};
use crate::target::Target;
use flwrs_plugin::plugin::frame::{PROTOCOL_VERSION, encode_frame, header};
use flwrs_plugin::schema::protocol::{
    Ack, Credit, Event, Exit, Nack, Ping, Pong, Shutdown, capability::Enum as Capability,
    msg_code::Enum as MsgCode, plugin_type::Enum as PluginType,
};
use flwrs_plugin::schema::rules;
use prost::Message;
use std::collections::HashMap;
use std::time::Duration;

/// How a case, test or step went.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "status", content = "reason", rename_all = "lowercase")]
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

/// The cases, in order: (name, what it checks).
pub const CASES: &[(&str, &str)] = &[
    (
        "handshake",
        "Initialize first, with the right id, token, type, version, protocol range and schemas",
    ),
    (
        "waits-for-acceptance",
        "no data, acks or pings before InitializeResponse",
    ),
    (
        "answers-ping",
        "PING is answered with PONG and the same nonce",
    ),
    (
        "data-events",
        "events flow by the rules: seq numbers, credit, acks, input_seq",
    ),
    (
        "shuts-down",
        "SHUTDOWN is answered with EXIT, then the process exits",
    ),
    (
        "stops-when-rejected",
        "SHUTDOWN instead of InitializeResponse: exits, no reconnect",
    ),
    (
        "reconnects",
        "after a dropped connection: reconnects with the same Initialize",
    ),
    (
        "closes-on-violation",
        "a frame with an unknown framing version closes the connection",
    ),
    (
        "live-feed",
        "a plugin offering LIVE_FEED sends none while the hub hasn't enabled it",
    ),
];

/// A conformance run against one plugin.
///
/// ```rust,ignore
/// let results = Audit::new(target).run().await;
/// println!("{}", report(&results));
/// ```
pub struct Audit {
    target: Target,
    only: Vec<String>,
    stimulus: Option<Box<dyn Stimulus>>,
    port: u16,
}

impl Audit {
    pub fn new(target: Target) -> Self {
        Self {
            target,
            only: vec![],
            stimulus: None,
            port: 0,
        }
    }

    /// Runs only these cases (empty: all).
    pub fn only(mut self, cases: &[String]) -> Self {
        self.only = cases.to_vec();
        self
    }

    /// Fired once in `data-events`, after the plugin is accepted: for sources
    /// that only emit when something outside happens.
    pub fn stimulus(mut self, stimulus: impl Stimulus + 'static) -> Self {
        self.stimulus = Some(Box::new(stimulus));
        self
    }

    /// The port a stimulus gets in its context (`${port}`).
    pub fn port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    /// Runs the cases in order.
    pub async fn run(mut self) -> Vec<CaseResult> {
        let mut results = vec![];
        for (name, checks) in CASES {
            if !self.only.is_empty() && !self.only.iter().any(|o| o == name) {
                continue;
            }
            let outcome = match self.case(name).await {
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

    async fn case(&mut self, name: &str) -> CaseResultInner {
        let mut fixture = Fixture::start(&self.target).await?;
        let outcome = match name {
            "handshake" => handshake(&mut fixture).await,
            "waits-for-acceptance" => waits_for_acceptance(&mut fixture).await,
            "answers-ping" => answers_ping(&mut fixture).await,
            "data-events" => data_events(&mut fixture, self.stimulus.as_mut(), self.port).await,
            "shuts-down" => shuts_down(&mut fixture).await,
            "stops-when-rejected" => stops_when_rejected(&mut fixture).await,
            "reconnects" => reconnects(&mut fixture).await,
            "closes-on-violation" => closes_on_violation(&mut fixture).await,
            "live-feed" => live_feed(&mut fixture).await,
            other => Err(format!("unknown case {other}")),
        };
        fixture.stop().await;
        outcome
    }
}

type CaseResultInner = Result<Outcome, String>;

// --- the cases ---

async fn handshake(f: &mut Fixture) -> CaseResultInner {
    let (_, init) = f.handshake().await?;
    let mut problems = vec![];
    if init.instance_id != f.target.instance_id() {
        problems.push(format!(
            "instance_id {:?}, expected {:?} (from --id{})",
            init.instance_id,
            f.target.instance_id(),
            if f.target.remote.is_some() {
                ", or [remote] id"
            } else {
                ""
            }
        ));
    }
    if init.token != f.token {
        problems.push(if f.target.remote.is_some() {
            "token isn't the one it was given (${token} / FLWRS_TOKEN, or [remote] token)"
                .to_string()
        } else {
            "token isn't FLWRS_TOKEN".to_string()
        });
    }
    if init.plugin_type != f.target.plugin_type as i32 {
        problems.push(format!(
            "plugin_type {}, the manifest says {}",
            init.plugin_type,
            f.target.plugin_type.as_str_name()
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
        (
            "in",
            &f.target.in_schema,
            &init.in_schema,
            f.target.consumes(),
        ),
        (
            "out",
            &f.target.out_schema,
            &init.out_schema,
            f.target.produces(),
        ),
    ] {
        match (needed, manifest, declared) {
            (false, _, _) => {}
            (true, _, None) => problems.push(format!("{side}_schema is missing")),
            (true, manifest, Some(declared)) => {
                problems.extend(
                    rules::check_well_formed(declared)
                        .into_iter()
                        .map(|p| format!("{side}_schema: {p}")),
                );
                if let Some(manifest) = manifest {
                    problems.extend(
                        rules::diff(manifest, declared)
                            .into_iter()
                            .map(|d| format!("{side}_schema.{d}")),
                    );
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
    match conn.reader.next(Duration::from_millis(700)).await {
        Err(_) => Ok(Outcome::Pass),
        Ok(None) => {
            Err("the plugin closed the connection while waiting for InitializeResponse".into())
        }
        Ok(Some(frame)) if frame.code() == MsgCode::Exit => Ok(Outcome::Pass),
        Ok(Some(frame)) => Err(format!("sent {:?} before InitializeResponse", frame.code())),
    }
}

async fn answers_ping(f: &mut Fixture) -> CaseResultInner {
    let (mut conn, _, negotiated) = f.accepted().await?;
    if negotiated.credits {
        // A source shouldn't be blocked from answering by having no credit.
        conn.writer.send(&Credit { events: 0 }).await?;
    }
    conn.writer.send(&Ping { nonce: 4242 }).await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        let frame = conn
            .reader
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

/// Credit given back for a source's first events (beyond the initial 3):
/// then it has used it all.
const MORE_CREDIT: u64 = 2;
/// How long to watch a source with no credit left, for events beyond it.
const OVERRUN_WATCH: Duration = Duration::from_millis(300);
/// How long a stimulated source may be quiet after its first events.
const STIMULUS_QUIET: Duration = Duration::from_millis(500);

async fn data_events(
    f: &mut Fixture,
    stimulus: Option<&mut Box<dyn Stimulus>>,
    port: u16,
) -> CaseResultInner {
    let (mut conn, _, negotiated) = f.accepted().await?;
    // Like a hub: an initial grant, and a credit back for each event routed.
    let mut granted: u64 = if negotiated.credits { 3 } else { u64::MAX };
    if negotiated.credits {
        conn.writer.send(&Credit { events: 3 }).await?;
    }
    let mut problems = vec![];
    let stimulated = stimulus.is_some();
    if let Some(stimulus) = stimulus {
        let ctx = StimulusCtx {
            iteration: 0,
            intended: std::time::Instant::now(),
            launch_args: f.target.launch_args.clone(),
            port,
        };
        if let Err(e) = stimulus.fire(ctx).await {
            problems.push(format!("the stimulus failed: {e}"));
        }
    }
    let mut sent: Vec<u64> = vec![];
    if f.target.consumes() {
        let schema = f.target.in_schema.clone().unwrap_or_default();
        let window = if negotiated.acks {
            negotiated.max_unacked.min(5)
        } else {
            5
        };
        for n in 0..window as usize {
            sent.push(
                conn.writer
                    .send_event(sample_payload(&schema), &format!("audit-{n}"), 1)
                    .await?,
            );
        }
    }
    // Watch for up to 5 s: outputs (producers), acks (consumers with ACKS).
    // It ends sooner once there's nothing left to see: every input answered
    // (a transform's acks come after its outputs), or a source's credit used
    // up and a short while more to catch it sending beyond it.
    let mut outputs = 0u64;
    let mut next_output_seq = 1u64;
    let mut answered: HashMap<u64, bool> = HashMap::new();
    let mut watch = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let mut left = watch.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            break;
        }
        // A source driven by a stimulus emits what it causes, promptly: once
        // it has, a quiet spell ends the watch.
        if stimulated && !f.target.consumes() && outputs > 0 {
            left = left.min(STIMULUS_QUIET);
        }
        let frame = match conn.reader.next(left).await {
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
                    problems.push(format!(
                        "event seq {}, expected {next_output_seq}",
                        frame.header.seq
                    ));
                }
                next_output_seq = frame.header.seq + 1;
                if outputs > granted {
                    problems.push(format!("sent {outputs} events with {granted} credit"));
                }
                if negotiated.credits && outputs <= MORE_CREDIT {
                    conn.writer.send(&Credit { events: 1 }).await?;
                    granted += 1;
                }
                if frame.header.flags != 0 {
                    problems.push("an event with flags set".to_string());
                }
                let input = frame.header.input_seq;
                if input != 0
                    && (f.target.plugin_type != PluginType::Transform || !sent.contains(&input))
                {
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
        if f.target.consumes() && negotiated.acks && answered.len() == sent.len() {
            break;
        }
        if !f.target.consumes() {
            if negotiated.credits && outputs >= granted && outputs > MORE_CREDIT {
                watch = watch.min(tokio::time::Instant::now() + OVERRUN_WATCH);
            } else if !negotiated.credits && outputs >= 5 {
                break;
            }
        }
    }
    if negotiated.acks && f.target.consumes() {
        let missing: Vec<u64> = sent
            .iter()
            .copied()
            .filter(|s| !answered.contains_key(s))
            .collect();
        if !missing.is_empty() {
            problems.push(format!("events never acked or nacked: {missing:?}"));
        }
    }
    if !problems.is_empty() {
        return Err(problems.join("; "));
    }
    if f.target.plugin_type == PluginType::Source && outputs == 0 {
        return Ok(Outcome::Skip(
            "the source emitted nothing within 5 s (give it a stimulus if it waits for the outside world)".into(),
        ));
    }
    Ok(Outcome::Pass)
}

async fn shuts_down(f: &mut Fixture) -> CaseResultInner {
    let (mut conn, _, _) = f.accepted().await?;
    conn.writer
        .send(&Shutdown {
            reason: "trellis audit".into(),
        })
        .await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match conn.reader.next(left).await {
            Err(_) => return Err("no EXIT within 10 s of SHUTDOWN".into()),
            Ok(None) => return Err("closed the connection without EXIT".into()),
            Ok(Some(frame)) if frame.code() == MsgCode::Exit => {
                decode::<Exit>(&frame)?;
                break;
            }
            Ok(Some(_)) => {} // in-flight events, acks
        }
    }
    // A remote plugin's process isn't ours to watch: its connection closes.
    if f.target.launched_remotely() {
        return conn
            .reader
            .closed(Duration::from_secs(10))
            .await
            .map(|_| Outcome::Pass)
            .map_err(|e| format!("after EXIT: {e}"));
    }
    if f.exits(Duration::from_secs(10)).await {
        Ok(Outcome::Pass)
    } else {
        Err("sent EXIT but didn't exit within 10 s".into())
    }
}

async fn stops_when_rejected(f: &mut Fixture) -> CaseResultInner {
    let (mut conn, _) = f.handshake().await?;
    conn.writer
        .send(&Shutdown {
            reason: "rejected by trellis audit".into(),
        })
        .await?;
    let _ = conn.reader.closed(Duration::from_secs(10)).await;
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
    let (_, again) = f.handshake().await.map_err(|e| {
        format!("after the connection dropped: {e} (the reconnect backoff may be at most 30 s)")
    })?;
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
    conn.writer.raw(&frame).await?;
    conn.reader.closed(Duration::from_secs(5)).await?;
    Ok(Outcome::Pass)
}

/// The fake hub never enables `LIVE_FEED`, so a plugin offering it must keep
/// quiet about it: not even an announcement.
async fn live_feed(f: &mut Fixture) -> CaseResultInner {
    let (mut conn, init, _) = f.accepted().await?;
    if !init.capabilities.contains(&(Capability::LiveFeed as i32)) {
        return Ok(Outcome::Pass); // nothing to break
    }
    let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return Ok(Outcome::Pass);
        }
        match conn.reader.next(left).await {
            Ok(Some(frame)) if frame.code() == MsgCode::LiveFeed => {
                return Err("LIVE_FEED sent though the hub didn't enable it".into());
            }
            Ok(Some(_)) => {}
            Ok(None) => return Err("the plugin closed the connection".into()),
            Err(_) => return Ok(Outcome::Pass), // quiet until the deadline
        }
    }
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
    let failed = results
        .iter()
        .filter(|r| matches!(r.outcome, Outcome::Fail(_)))
        .count();
    out.push_str(&format!(
        "\n{} cases, {} passed, {} failed, {} skipped\n",
        results.len(),
        results
            .iter()
            .filter(|r| r.outcome == Outcome::Pass)
            .count(),
        failed,
        results
            .iter()
            .filter(|r| matches!(r.outcome, Outcome::Skip(_)))
            .count()
    ));
    out
}
