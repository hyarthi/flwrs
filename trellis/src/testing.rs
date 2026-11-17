//! `test`: behavioural end-to-end tests. The fake hub drives the plugin
//! through the steps of a test file and checks what comes back.
//!
//! ```toml
//! # tests/uppercase.test.toml
//! name = "uppercases the text"
//! launch_args = "node.json"            # the node's launch_args (JSON file), optional
//! uploads = { "model.onnx" = "model.onnx" }  # files uploaded to the node, optional
//!
//! [[steps]]
//! send = "payloads/hello.json"         # plain JSON shaped by the in schema
//! expect_ack = "ack"
//! expect_outputs = [{ payload = "expected/hello.out.json" }]
//!
//! [[steps]]
//! send = "payloads/empty.json"
//! expect_ack = { nack = { retryable = false } }
//!
//! [[steps]]
//! redeliver = 1                        # step 1's event again, attempt + 1
//! expect_outputs = [{ same_as_step = 1 }]
//! ```
//!
//! Each step does one thing (`send`, `redeliver`, `stimulus`, `wait_ms`,
//! `grant`, `disconnect`, `restart`) and may check the answer (`expect_ack`)
//! and the outputs (`expect_outputs`). Outputs are always checked against the
//! out schema. Paths are relative to the test file; `${port}` in launch args,
//! args and commands is the run's free port (also `TRELLIS_PORT`).

use crate::audit::Outcome;
use crate::hub::{Conn, Fixture, Negotiated, decode};
use crate::payload::{
    at_path, conforms, load, load_with, matches_exact, matches_subset, sample_payload, to_plain,
};
use crate::stimulus::{
    CommandStimulus, Correlation, Correlator, Stimulus, StimulusCtx, Vars, free_port, run_command,
    substitute_json_vars, substitute_vars,
};
use crate::target::Target;
use flwrs_plugin::schema::payload::{PluginPayload, SchemaDefinition};
use flwrs_plugin::schema::protocol::{
    Ack, Credit, ErrorEvent, Event, LogEvent, Nack, Ping, Pong, msg_code::Enum as MsgCode,
};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// A test file.
#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct TestFile {
    /// Defaults to the file name.
    pub name: Option<String>,
    /// A JSON file with the node's `launch_args`.
    pub launch_args: Option<String>,
    /// Files uploaded to the node, by name (paths relative to this file),
    /// on top of `[uploads]` in trellis.toml.
    #[serde(default)]
    pub uploads: BTreeMap<String, String>,
    /// Extra plugin arguments.
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Commands before the plugin starts / after it stopped (e.g. a mock server).
    #[serde(default)]
    pub setup: Vec<Vec<String>>,
    #[serde(default)]
    pub teardown: Vec<Vec<String>>,
    /// Credit granted to a producer after each (re)connect.
    #[serde(default = "default_credit")]
    pub initial_credit: u32,
    /// Give a credit back for each event, like the hub.
    #[serde(default = "yes")]
    pub return_credit: bool,
    /// How long a step waits for its answer and outputs (ms).
    #[serde(default = "default_within")]
    pub within_ms: u64,
    pub steps: Vec<Step>,
    /// The file's folder: relative paths start there.
    #[serde(skip)]
    pub dir: PathBuf,
}

fn default_credit() -> u32 {
    64
}
fn yes() -> bool {
    true
}
fn default_within() -> u64 {
    2000
}

/// One step: one action, then its checks.
#[derive(Deserialize, Debug, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct Step {
    /// Send this payload file (sinks, transforms).
    pub send: Option<String>,
    /// Send step N's event again (1-based), same event id, attempt + 1.
    pub redeliver: Option<usize>,
    /// Make a source emit: a command, or `true` for the stimulus given in code.
    pub stimulus: Option<StimulusSpec>,
    /// Just wait (and collect what comes).
    pub wait_ms: Option<u64>,
    /// Grant this many credits (with `return_credit = false`: manual flow control).
    pub grant: Option<u32>,
    /// Drop the connection and wait for the plugin to reconnect.
    #[serde(default)]
    pub disconnect: bool,
    /// Kill the plugin and start it again.
    #[serde(default)]
    pub restart: bool,
    pub expect_ack: Option<ExpectAck>,
    pub expect_outputs: Option<Vec<OutputExpectation>>,
    /// Overrides the file's `within_ms` for this step.
    pub within_ms: Option<u64>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(untagged)]
pub enum StimulusSpec {
    InCode(bool),
    Command(Vec<String>),
}

/// The plugin's answer to an event it received.
#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ExpectAck {
    Ack,
    /// `nack = {}` for any NACK, `nack = { retryable = false }` to check the flag.
    Nack {
        #[serde(default)]
        retryable: Option<bool>,
    },
    /// No answer within the step's time.
    None,
    /// An answer of either kind.
    Any,
}

/// What an output must look like. `count` checks how many outputs came;
/// `same_as_step` compares all outputs with another step's; every other item
/// checks the output at its position (first item: first output). Without a
/// `count`, the number of positional items is the expected number of outputs.
#[derive(Deserialize, Debug, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct OutputExpectation {
    pub count: Option<usize>,
    pub same_as_step: Option<usize>,
    /// A payload file to compare with.
    pub payload: Option<String>,
    /// `exact` (default) or `subset` (only the file's fields, recursively).
    #[serde(rename = "match")]
    pub mode: Option<MatchMode>,
    /// A dot path into the output (plain JSON form) for the checks below.
    pub path: Option<String>,
    pub eq: Option<Json>,
    pub ne: Option<Json>,
    pub gt: Option<Json>,
    pub ge: Option<Json>,
    pub lt: Option<Json>,
    pub le: Option<Json>,
    pub contains: Option<Json>,
    pub exists: Option<bool>,
}

#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum MatchMode {
    #[default]
    Exact,
    Subset,
}

impl TestFile {
    pub fn read(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut file: TestFile =
            toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        file.dir = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        if file.name.is_none() {
            file.name = path.file_name().map(|n| {
                n.to_string_lossy()
                    .trim_end_matches(".test.toml")
                    .to_string()
            });
        }
        Ok(file)
    }
}

/// How a test went.
#[derive(Debug, Clone, Serialize)]
pub struct TestResult {
    pub name: String,
    pub file: Option<PathBuf>,
    pub outcome: Outcome,
    pub steps: Vec<StepResult>,
    /// The plugin's log and error records, in order.
    pub logs: Vec<String>,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct StepResult {
    /// 1-based, as in `redeliver` / `same_as_step`.
    pub index: usize,
    pub action: String,
    pub outcome: Outcome,
}

/// Runs test files against a plugin.
pub struct Tester {
    target: Target,
    stimulus: Option<Box<dyn Stimulus>>,
    correlator: Option<Correlator>,
    update: bool,
    vars: Vars,
}

impl Tester {
    pub fn new(target: Target) -> Self {
        Self {
            target,
            stimulus: None,
            correlator: None,
            update: false,
            vars: Vars::new(),
        }
    }

    /// `${name}` → `value` in payload files, launch args, args and commands
    /// (e.g. the port of a mock server the test started).
    pub fn var(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.vars.insert(name.into(), value.into());
        self
    }

    /// The stimulus for `stimulus = true` steps (and the smoke test).
    pub fn stimulus(mut self, stimulus: impl Stimulus + 'static) -> Self {
        self.stimulus = Some(Box::new(stimulus));
        self
    }

    /// Ties a stimulus to its events: then a stimulus step only counts the
    /// outputs that carry the id it returned.
    pub fn correlate(
        mut self,
        correlate: impl Fn(&PluginPayload) -> Option<Correlation> + Send + Sync + 'static,
    ) -> Self {
        self.correlator = Some(std::sync::Arc::new(correlate));
        self
    }

    /// Writes the actual outputs into the expected payload files (snapshots).
    pub fn update(mut self, update: bool) -> Self {
        self.update = update;
        self
    }

    pub async fn run_file(&mut self, path: &Path) -> TestResult {
        match TestFile::read(path) {
            Ok(file) => {
                let mut result = self.run(&file).await;
                result.file = Some(path.to_path_buf());
                result
            }
            Err(e) => TestResult {
                name: path.display().to_string(),
                file: Some(path.to_path_buf()),
                outcome: Outcome::Fail(e),
                steps: vec![],
                logs: vec![],
                duration_ms: 0,
            },
        }
    }

    /// Without test files: a sample event (sinks, transforms) or a stimulus
    /// (sources), checking that an answer comes and outputs fit the schema.
    pub async fn smoke(&mut self) -> TestResult {
        let in_schema = self.target.in_schema.clone().unwrap_or_default();
        let step = if self.target.consumes() {
            let sample = sample_payload(&in_schema);
            Step {
                send: Some(String::new()),
                expect_ack: Some(ExpectAck::Any),
                ..Default::default()
            }
            .with_inline(sample)
        } else {
            // Until the first event (or the step's time is over).
            Step {
                stimulus: Some(StimulusSpec::InCode(self.stimulus.is_some())),
                expect_outputs: Some(vec![OutputExpectation::default()]),
                ..Default::default()
            }
            .with_inline_none()
        };
        let file = TestFile {
            name: Some("smoke".into()),
            launch_args: None,
            uploads: BTreeMap::new(),
            args: vec![],
            env: BTreeMap::new(),
            setup: vec![],
            teardown: vec![],
            initial_credit: default_credit(),
            return_credit: true,
            within_ms: 5000,
            steps: vec![step.0],
            dir: PathBuf::from("."),
        };
        let mut result = self.run_steps(&file, step.1).await;
        if !self.target.consumes()
            && result
                .steps
                .first()
                .is_some_and(|s| s.action.ends_with(", 0 outputs"))
        {
            result.outcome =
                Outcome::Skip("the source emitted nothing (give it a stimulus)".into());
        }
        result
    }

    /// Runs one test.
    pub async fn run(&mut self, file: &TestFile) -> TestResult {
        self.run_steps(file, None).await
    }

    async fn run_steps(&mut self, file: &TestFile, inline: Option<PluginPayload>) -> TestResult {
        let started = Instant::now();
        let name = file.name.clone().unwrap_or_else(|| "test".into());
        let port = free_port();
        let mut result = TestResult {
            name,
            file: None,
            outcome: Outcome::Pass,
            steps: vec![],
            logs: vec![],
            duration_ms: 0,
        };
        let target = match self.prepare(file, port) {
            Ok(target) => target,
            Err(e) => {
                result.outcome = Outcome::Fail(e);
                return result;
            }
        };
        for command in &file.setup {
            let argv: Vec<String> = command
                .iter()
                .map(|a| substitute_vars(a, port, &self.vars))
                .collect();
            if let Err(e) = run_command(&argv, &file.dir, port).await {
                result.outcome = Outcome::Fail(format!("setup: {e}"));
                return result;
            }
        }
        let outcome = match Session::open(&target, file, port).await {
            Err(e) => Err(e),
            Ok(mut session) => {
                let outcome = self.drive(&mut session, file, inline, &mut result).await;
                result.logs = std::mem::take(&mut session.logs);
                session.fixture.stop().await;
                outcome
            }
        };
        for command in &file.teardown {
            let argv: Vec<String> = command
                .iter()
                .map(|a| substitute_vars(a, port, &self.vars))
                .collect();
            if let Err(e) = run_command(&argv, &file.dir, port).await
                && outcome.is_ok()
            {
                result.outcome = Outcome::Fail(format!("teardown: {e}"));
            }
        }
        if let Err(e) = outcome {
            result.outcome = Outcome::Fail(e);
        }
        result.duration_ms = started.elapsed().as_millis() as u64;
        result
    }

    fn prepare(&self, file: &TestFile, port: u16) -> Result<Target, String> {
        let mut target = self.target.clone();
        for (name, path) in &file.uploads {
            target = target.with_upload(name, file.dir.join(path))?;
        }
        if let Some(path) = &file.launch_args {
            let path = file.dir.join(path);
            let text =
                std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            let args: Json =
                serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
            target = target.with_launch_args(substitute_json_vars(&args, port, &self.vars))?;
        } else if !target.launch_args.is_null() {
            let args = substitute_json_vars(&target.launch_args, port, &self.vars);
            target = target.with_launch_args(args)?;
        }
        target.args.extend(
            file.args
                .iter()
                .map(|a| substitute_vars(a, port, &self.vars)),
        );
        target.env.push(("TRELLIS_PORT".into(), port.to_string()));
        target.env.extend(
            file.env
                .iter()
                .map(|(k, v)| (k.clone(), substitute_vars(v, port, &self.vars))),
        );
        Ok(target)
    }

    /// Runs the steps; a failed step ends the test (the rest are skipped).
    async fn drive(
        &mut self,
        s: &mut Session,
        file: &TestFile,
        inline: Option<PluginPayload>,
        result: &mut TestResult,
    ) -> Result<(), String> {
        let mut failed = false;
        for (i, step) in file.steps.iter().enumerate() {
            let index = i + 1;
            if failed {
                result.steps.push(StepResult {
                    index,
                    action: describe(step),
                    outcome: Outcome::Skip("an earlier step failed".into()),
                });
                continue;
            }
            let (action, outcome) = match self.step(s, file, index, step, inline.clone()).await {
                Ok(action) => (action, Outcome::Pass),
                Err((action, why)) => (action, Outcome::Fail(why)),
            };
            if matches!(outcome, Outcome::Fail(_)) {
                failed = true;
                result.outcome = Outcome::Fail(format!("step {index} failed"));
            }
            result.steps.push(StepResult {
                index,
                action,
                outcome,
            });
        }
        Ok(())
    }

    async fn step(
        &mut self,
        s: &mut Session,
        file: &TestFile,
        index: usize,
        step: &Step,
        inline: Option<PluginPayload>,
    ) -> Result<String, (String, String)> {
        let mut action = describe(step);
        let fail = |action: &str, why: String| (action.to_string(), why);
        // Outputs that come from here on belong to this step.
        s.step = index;
        let within = Duration::from_millis(step.within_ms.unwrap_or(file.within_ms));
        let in_schema = s.target.in_schema.clone().unwrap_or_default();
        let positional = step
            .expect_outputs
            .iter()
            .flatten()
            .filter(|e| e.count.is_none() && e.same_as_step.is_none())
            .count();
        let wanted_outputs = step
            .expect_outputs
            .iter()
            .flatten()
            .find_map(|e| e.count)
            .or((positional > 0).then_some(positional));
        let mut sent_seq = None;
        let mut correlation = None;

        if let Some(send) = &step.send {
            if !s.target.consumes() {
                return Err(fail(&action, "a source doesn't receive events".into()));
            }
            let payload = match inline.clone() {
                Some(p) => p,
                None => load_with(&file.dir.join(send), &in_schema, |t| {
                    substitute_vars(t, s.port, &self.vars)
                })
                .map_err(|e| fail(&action, e))?,
            };
            let event_id = format!("step-{index}");
            let seq = s
                .send(payload.clone(), &event_id, 1)
                .await
                .map_err(|e| fail(&action, e))?;
            s.sent.insert(index, (payload, event_id, 1));
            sent_seq = Some(seq);
        } else if let Some(k) = step.redeliver {
            let Some((payload, event_id, attempt)) = s.sent.get(&k).cloned() else {
                return Err(fail(&action, format!("step {k} didn't send an event")));
            };
            let seq = s
                .send(payload.clone(), &event_id, attempt + 1)
                .await
                .map_err(|e| fail(&action, e))?;
            s.sent.insert(index, (payload, event_id, attempt + 1));
            sent_seq = Some(seq);
        } else if let Some(spec) = &step.stimulus {
            let ctx = StimulusCtx {
                iteration: index as u64 - 1,
                intended: Instant::now(),
                launch_args: s.target.launch_args.clone(),
                port: s.port,
            };
            correlation = match spec {
                StimulusSpec::InCode(false) => None,
                StimulusSpec::InCode(true) => match self.stimulus.as_mut() {
                    Some(stimulus) => stimulus.fire(ctx).await.map_err(|e| fail(&action, e))?,
                    None => return Err(fail(&action, "no stimulus was given in code".into())),
                },
                StimulusSpec::Command(argv) => CommandStimulus {
                    argv: argv.clone(),
                    cwd: file.dir.clone(),
                    vars: self.vars.clone(),
                }
                .fire(ctx)
                .await
                .map_err(|e| fail(&action, e))?,
            };
        } else if let Some(ms) = step.wait_ms {
            s.collect(Duration::from_millis(ms), |_| false)
                .await
                .map_err(|e| fail(&action, e))?;
        } else if let Some(n) = step.grant {
            s.conn
                .writer
                .send(&Credit { events: n })
                .await
                .map_err(|e| fail(&action, e))?;
        } else if step.disconnect {
            s.reconnect(false).await.map_err(|e| fail(&action, e))?;
        } else if step.restart {
            s.reconnect(true).await.map_err(|e| fail(&action, e))?;
        } else {
            return Err(fail(&action, "the step does nothing".into()));
        }

        // Wait for the answer and the outputs.
        let answer_expected = sent_seq.is_some() && s.negotiated.acks;
        let expect_none = step.expect_ack == Some(ExpectAck::None);
        let correlator = self.correlator.clone();
        let relevant = |s: &Session, o: &Output| match (sent_seq, &correlation, &correlator) {
            (Some(seq), _, _) => o.input_seq == seq,
            (None, Some(id), Some(c)) => c(&o.payload).as_ref() == Some(id),
            _ => o.step == s.step,
        };
        if step.wait_ms.is_none() {
            let wait = s
                .collect(within, |s| {
                    if expect_none {
                        return false;
                    }
                    let answered = sent_seq.is_none_or(|seq| s.answers.contains_key(&seq));
                    if answer_expected {
                        return answered;
                    }
                    match wanted_outputs {
                        Some(n) => s.outputs.iter().filter(|o| relevant(s, o)).count() >= n,
                        None => sent_seq.is_none() && step.stimulus.is_none(),
                    }
                })
                .await;
            wait.map_err(|e| fail(&action, e))?;
        }
        let outputs: Vec<PluginPayload> = s
            .outputs
            .iter()
            .filter(|o| relevant(s, o))
            .map(|o| o.payload.clone())
            .collect();
        s.step_outputs.insert(index, outputs.clone());
        action.push_str(&format!(", {} outputs", outputs.len()));

        let mut problems = vec![];
        // The answer.
        let answer = sent_seq.and_then(|seq| s.answers.get(&seq).cloned());
        match (&step.expect_ack, &answer) {
            (None, _) | (Some(ExpectAck::Any), Some(_)) | (Some(ExpectAck::None), None) => {}
            (Some(ExpectAck::Any), None) if !answer_expected => {}
            (Some(ExpectAck::Any), None) => problems.push("no ACK or NACK".to_string()),
            (Some(ExpectAck::None), Some(a)) => {
                problems.push(format!("expected no answer, got {a:?}"))
            }
            (Some(ExpectAck::Ack), Some(Answer::Ack)) => {}
            (Some(ExpectAck::Ack), other) => problems.push(format!("expected ACK, got {other:?}")),
            (Some(ExpectAck::Nack { retryable }), Some(Answer::Nack { retry, reason })) => {
                if retryable.is_some_and(|r| r != *retry) {
                    problems.push(format!(
                        "NACK with retry = {retry} ({reason}), expected {}",
                        !retry
                    ));
                }
            }
            (Some(ExpectAck::Nack { .. }), other) => {
                problems.push(format!("expected NACK, got {other:?}"))
            }
        }
        if step.expect_ack.is_some() && sent_seq.is_some() && !s.negotiated.acks {
            problems.push("the plugin didn't negotiate ACKS, so there's no answer to check".into());
        }
        // The outputs.
        if let Some(out_schema) = &s.target.out_schema {
            for (i, o) in outputs.iter().enumerate() {
                for p in conforms(o, out_schema) {
                    problems.push(format!("output {}: {p}", i + 1));
                }
            }
        }
        let out_schema = s.target.out_schema.clone().unwrap_or_default();
        let mut position = 0;
        for expectation in step.expect_outputs.iter().flatten() {
            if let Some(n) = expectation.count {
                if outputs.len() != n {
                    problems.push(format!("{} outputs, expected {n}", outputs.len()));
                }
                continue;
            }
            if let Some(k) = expectation.same_as_step {
                match s.step_outputs.get(&k) {
                    None => problems.push(format!("step {k} has no outputs to compare with")),
                    Some(earlier) if earlier != &outputs => {
                        problems.push(format!("the outputs differ from step {k}'s"))
                    }
                    Some(_) => {}
                }
                continue;
            }
            position += 1;
            let Some(actual) = outputs.get(position - 1) else {
                continue; // counted below
            };
            if let Err(e) = check_output(expectation, actual, &out_schema, &file.dir, self.update) {
                problems.push(format!("output {position}: {e}"));
            }
        }
        if positional > 0
            && !step
                .expect_outputs
                .iter()
                .flatten()
                .any(|e| e.count.is_some())
            && outputs.len() != positional
        {
            problems.push(format!("{} outputs, expected {positional}", outputs.len()));
        }
        if let Some(problem) = &s.problem {
            problems.push(problem.clone());
        }
        if problems.is_empty() {
            Ok(action)
        } else {
            Err((action, problems.join("; ")))
        }
    }
}

impl Step {
    /// The smoke test's step, with its payload given directly.
    fn with_inline(self, payload: PluginPayload) -> (Step, Option<PluginPayload>) {
        (self, Some(payload))
    }
    fn with_inline_none(self) -> (Step, Option<PluginPayload>) {
        (self, None)
    }
}

fn describe(step: &Step) -> String {
    if let Some(send) = &step.send {
        if send.is_empty() {
            "send a sample event".into()
        } else {
            format!("send {send}")
        }
    } else if let Some(k) = step.redeliver {
        format!("redeliver step {k}")
    } else if let Some(spec) = &step.stimulus {
        match spec {
            StimulusSpec::Command(argv) => format!("stimulus {}", argv.join(" ")),
            StimulusSpec::InCode(true) => "stimulus".into(),
            StimulusSpec::InCode(false) => "wait for events".into(),
        }
    } else if let Some(ms) = step.wait_ms {
        format!("wait {ms} ms")
    } else if let Some(n) = step.grant {
        format!("grant {n} credits")
    } else if step.disconnect {
        "disconnect".into()
    } else if step.restart {
        "restart".into()
    } else {
        "nothing".into()
    }
}

fn check_output(
    e: &OutputExpectation,
    actual: &PluginPayload,
    schema: &SchemaDefinition,
    dir: &Path,
    update: bool,
) -> Result<(), String> {
    if let Some(file) = &e.payload {
        let path = dir.join(file);
        let compared = if path.is_file() {
            load(&path, schema).and_then(|expected| match e.mode.unwrap_or_default() {
                MatchMode::Exact => matches_exact(&expected, actual),
                MatchMode::Subset => matches_subset(&expected, actual),
            })
        } else {
            Err(format!("{file} doesn't exist"))
        };
        if let Err(problem) = compared {
            if !update {
                return Err(problem);
            }
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let text = serde_json::to_string_pretty(&to_plain(actual)).unwrap_or_default();
            std::fs::write(&path, text + "\n").map_err(|e| format!("{}: {e}", path.display()))?;
        }
    }
    if let Some(path) = &e.path {
        let plain = to_plain(actual);
        let value = at_path(&plain, path);
        if let Some(exists) = e.exists
            && value.is_some() != exists
        {
            return Err(format!(
                "{path}: {}",
                if exists { "missing" } else { "present" }
            ));
        }
        let checks: [(&str, &Option<Json>); 7] = [
            ("eq", &e.eq),
            ("ne", &e.ne),
            ("gt", &e.gt),
            ("ge", &e.ge),
            ("lt", &e.lt),
            ("le", &e.le),
            ("contains", &e.contains),
        ];
        for (op, wanted) in checks {
            let Some(wanted) = wanted else { continue };
            let Some(value) = value else {
                return Err(format!("{path}: missing"));
            };
            if !compare(op, value, wanted) {
                return Err(format!("{path}: {value} isn't {op} {wanted}"));
            }
        }
    }
    Ok(())
}

fn compare(op: &str, value: &Json, wanted: &Json) -> bool {
    let order = || -> Option<std::cmp::Ordering> {
        match (value, wanted) {
            (Json::Number(a), Json::Number(b)) => a.as_f64()?.partial_cmp(&b.as_f64()?),
            (Json::String(a), Json::String(b)) => Some(a.cmp(b)),
            _ => None,
        }
    };
    let equal = || match (value, wanted) {
        (Json::Number(a), Json::Number(b)) => a.as_f64() == b.as_f64(),
        _ => value == wanted,
    };
    use std::cmp::Ordering::*;
    match op {
        "eq" => equal(),
        "ne" => !equal(),
        "gt" => order() == Some(Greater),
        "ge" => matches!(order(), Some(Greater | Equal)),
        "lt" => order() == Some(Less),
        "le" => matches!(order(), Some(Less | Equal)),
        "contains" => match (value, wanted) {
            (Json::String(s), Json::String(w)) => s.contains(w.as_str()),
            (Json::Array(items), w) => items.contains(w),
            (Json::Object(map), Json::String(k)) => map.contains_key(k),
            _ => false,
        },
        _ => false,
    }
}

/// The plugin's answer to an event.
#[derive(Debug, Clone, PartialEq)]
enum Answer {
    Ack,
    Nack { retry: bool, reason: String },
}

struct Output {
    /// The step that was running when it came (0 before the first).
    step: usize,
    input_seq: u64,
    payload: PluginPayload,
}

/// A running test: the plugin, its connection and what it sent.
struct Session {
    fixture: Fixture,
    conn: Conn,
    negotiated: Negotiated,
    target: Target,
    port: u16,
    initial_credit: u32,
    return_credit: bool,
    /// The step running now.
    step: usize,
    /// step → (payload, event id, attempt) of the event it sent.
    sent: BTreeMap<usize, (PluginPayload, String, u32)>,
    answers: BTreeMap<u64, Answer>,
    outputs: Vec<Output>,
    step_outputs: BTreeMap<usize, Vec<PluginPayload>>,
    logs: Vec<String>,
    /// A protocol problem seen while reading (reported with the current step).
    problem: Option<String>,
}

impl Session {
    async fn open(target: &Target, file: &TestFile, port: u16) -> Result<Self, String> {
        let fixture = Fixture::start(target).await?;
        let (conn, _, negotiated) = fixture.accepted().await?;
        let mut session = Self {
            fixture,
            conn,
            negotiated,
            target: target.clone(),
            port,
            initial_credit: file.initial_credit,
            return_credit: file.return_credit,
            step: 0,
            sent: BTreeMap::new(),
            answers: BTreeMap::new(),
            outputs: vec![],
            step_outputs: BTreeMap::new(),
            logs: vec![],
            problem: None,
        };
        session.grant_initial().await?;
        Ok(session)
    }

    async fn grant_initial(&mut self) -> Result<(), String> {
        if self.negotiated.credits && self.initial_credit > 0 {
            self.conn
                .writer
                .send(&Credit {
                    events: self.initial_credit,
                })
                .await?;
        }
        Ok(())
    }

    async fn send(
        &mut self,
        payload: PluginPayload,
        event_id: &str,
        attempt: u32,
    ) -> Result<u64, String> {
        self.conn
            .writer
            .send_event(payload, event_id, attempt)
            .await
    }

    async fn reconnect(&mut self, restart: bool) -> Result<(), String> {
        // A restart kills the plugin before the old connection closes, so the
        // dying process can't queue a reconnect we'd accept instead.
        if restart {
            self.fixture.restart().await?;
        }
        self.conn.writer.close().await;
        // Replace the connection: dropping the old one closes it.
        let (conn, _, negotiated) = self.fixture.accepted().await.map_err(|e| {
            format!("{e} (after a disconnect the SDK reconnects within its backoff, at most 30 s)")
        })?;
        self.conn = conn;
        self.negotiated = negotiated;
        self.grant_initial().await
    }

    /// Reads frames until `done` says so or `wait` is over (not an error).
    async fn collect(
        &mut self,
        wait: Duration,
        done: impl Fn(&Session) -> bool,
    ) -> Result<(), String> {
        let deadline = tokio::time::Instant::now() + wait;
        while !done(self) {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                break;
            }
            let frame = match self.conn.reader.frame(left).await {
                Err(e) if e.starts_with("nothing from the plugin") => break,
                Err(e) => return Err(e),
                Ok(None) => return Err("the plugin closed the connection".into()),
                Ok(Some(frame)) => frame,
            };
            match frame.code() {
                MsgCode::Event if self.target.produces() => {
                    let event: Event = decode(&frame)?;
                    self.outputs.push(Output {
                        step: self.step,
                        input_seq: frame.header.input_seq,
                        payload: event.payload.unwrap_or_default(),
                    });
                    if self.negotiated.credits && self.return_credit {
                        self.conn.writer.send(&Credit { events: 1 }).await?;
                    }
                    if self.negotiated.acks {
                        self.conn
                            .writer
                            .send(&Ack {
                                up_to: frame.header.seq,
                                seqs: vec![],
                            })
                            .await?;
                    }
                }
                MsgCode::Ack if self.target.consumes() => {
                    let ack: Ack = decode(&frame)?;
                    let seqs: Vec<u64> = self
                        .sent_seqs()
                        .filter(|s| *s <= ack.up_to || ack.seqs.contains(s))
                        .collect();
                    for seq in seqs.into_iter().chain(ack.seqs.iter().copied()) {
                        self.answers.entry(seq).or_insert(Answer::Ack);
                    }
                }
                MsgCode::Nack if self.target.consumes() => {
                    let nack: Nack = decode(&frame)?;
                    self.answers.insert(
                        nack.seq,
                        Answer::Nack {
                            retry: nack.retry,
                            reason: nack.reason,
                        },
                    );
                }
                MsgCode::Log => {
                    let log: LogEvent = decode(&frame)?;
                    self.logs
                        .push(format!("{:?}: {}", log.level(), log.message));
                }
                MsgCode::Error => {
                    let error: ErrorEvent = decode(&frame)?;
                    self.logs.push(format!("ERROR EVENT: {}", error.message));
                }
                MsgCode::Ping => {
                    let ping: Ping = decode(&frame)?;
                    self.conn.writer.send(&Pong { nonce: ping.nonce }).await?;
                }
                MsgCode::Pong | MsgCode::Credit => {}
                MsgCode::Exit => return Err("the plugin sent EXIT".into()),
                other => {
                    self.problem = Some(format!("unexpected {other:?} from the plugin"));
                }
            }
        }
        Ok(())
    }

    /// Seqs answered by an ACK's `up_to` are only the ones still waiting.
    fn sent_seqs(&self) -> impl Iterator<Item = u64> + '_ {
        let max = self.conn.writer.last_seq();
        (1..=max).filter(|s| !self.answers.contains_key(s))
    }
}

/// One line per test (and per step for failures), for people.
pub fn report(results: &[TestResult]) -> String {
    let mut out = String::new();
    for r in results {
        let status = match &r.outcome {
            Outcome::Pass => "PASS",
            Outcome::Fail(_) => "FAIL",
            Outcome::Skip(_) => "SKIP",
        };
        out.push_str(&format!("{status}  {} ({} ms)\n", r.name, r.duration_ms));
        match &r.outcome {
            Outcome::Pass => {}
            Outcome::Skip(why) => out.push_str(&format!("       {why}\n")),
            Outcome::Fail(why) => {
                if r.steps.is_empty() {
                    out.push_str(&format!("       {why}\n"));
                }
                for s in &r.steps {
                    if let Outcome::Fail(why) = &s.outcome {
                        out.push_str(&format!(
                            "       step {}: {}\n         {why}\n",
                            s.index, s.action
                        ));
                    }
                }
                if !r.logs.is_empty() {
                    out.push_str("       plugin logs (last 10):\n");
                    for line in r.logs.iter().rev().take(10).rev() {
                        out.push_str(&format!("         {line}\n"));
                    }
                }
            }
        }
    }
    let count = |f: fn(&Outcome) -> bool| results.iter().filter(|r| f(&r.outcome)).count();
    out.push_str(&format!(
        "\n{} tests, {} passed, {} failed, {} skipped\n",
        results.len(),
        count(|o| *o == Outcome::Pass),
        count(|o| matches!(o, Outcome::Fail(_))),
        count(|o| matches!(o, Outcome::Skip(_))),
    ));
    out
}

/// JUnit XML, for CI.
pub fn junit(results: &[TestResult]) -> String {
    let esc = |s: &str| {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    };
    let failures = results
        .iter()
        .filter(|r| matches!(r.outcome, Outcome::Fail(_)))
        .count();
    let skipped = results
        .iter()
        .filter(|r| matches!(r.outcome, Outcome::Skip(_)))
        .count();
    let mut out = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<testsuite name=\"trellis\" tests=\"{}\" failures=\"{failures}\" skipped=\"{skipped}\">\n",
        results.len()
    );
    for r in results {
        out.push_str(&format!(
            "  <testcase name=\"{}\" time=\"{:.3}\">\n",
            esc(&r.name),
            r.duration_ms as f64 / 1000.0
        ));
        match &r.outcome {
            Outcome::Pass => {}
            Outcome::Skip(why) => {
                out.push_str(&format!("    <skipped message=\"{}\"/>\n", esc(why)))
            }
            Outcome::Fail(why) => {
                let detail: Vec<String> = r
                    .steps
                    .iter()
                    .filter_map(|s| match &s.outcome {
                        Outcome::Fail(w) => Some(format!("step {} ({}): {w}", s.index, s.action)),
                        _ => None,
                    })
                    .chain(r.logs.iter().map(|l| format!("log: {l}")))
                    .collect();
                out.push_str(&format!(
                    "    <failure message=\"{}\">{}</failure>\n",
                    esc(why),
                    esc(&detail.join("\n"))
                ));
            }
        }
        out.push_str("  </testcase>\n");
    }
    out.push_str("</testsuite>\n");
    out
}
