//! The command line (the node's launch args) and the settings read from it.

use clap::Parser;
use flwrs_cv_source::graph::config::Execution;
use flwrs_cv_source::graph::{self, Checked, GraphConfig};
use flwrs_cv_source::ingest::config::InputConfig;
use flwrs_cv_source::live::LiveConfig;
use flwrs_plugin::cli::{Bootstrap, read_json};
use flwrs_plugin::schema::payload::SchemaDefinition;
use flwrs_plugin::schema::rules;
use std::collections::BTreeMap;
use std::path::PathBuf;

/// flwrs source plugin that runs computer-vision models on a camera or video
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
pub struct Args {
    #[command(flatten)]
    pub bootstrap: Bootstrap,

    /// JSON file: the model graph (nodes and links)
    #[arg(long)]
    pub graph: PathBuf,

    /// A file the graph names (a model, labels, a charset); once per file
    #[arg(long = "file")]
    pub files: Vec<PathBuf>,

    /// JSON file: where models run (runtime, device, provider, fallback);
    /// a model node's own `execution` overrides it field by field
    #[arg(long)]
    pub execution: Option<PathBuf>,

    /// JSON file: where frames come from (a camera, an image folder, an
    /// MJPEG file)
    #[arg(long)]
    pub input: Option<PathBuf>,

    /// JSON file: the events' payload schema (default: the detections schema)
    #[arg(long)]
    pub out_schema: Option<PathBuf>,

    /// JSON file: what the configurator read from the model files, for
    /// editors. The plugin doesn't use it.
    #[arg(long)]
    #[allow(dead_code, reason = "a configured value for editors")]
    pub model_info: Option<PathBuf>,

    /// JSON file: the live view (per-node stats, an annotated preview). Off
    /// by default; either only works while someone is watching
    #[arg(long)]
    pub live: Option<PathBuf>,
}

/// What the plugin runs with.
pub struct Settings {
    pub graph: GraphConfig,
    pub checked: Checked,
    /// The node's files, by name (how the graph refers to them).
    pub files: BTreeMap<String, PathBuf>,
    pub execution: Execution,
    /// Where frames come from; none: the plugin loads its graph and idles.
    pub input: Option<InputConfig>,
    pub out_schema: SchemaDefinition,
    pub live: LiveConfig,
}

pub use flwrs_cv_source::emit::DEFAULT_OUT_SCHEMA;

impl Args {
    /// The settings, or every problem found.
    pub fn settings(&self) -> Result<Settings, Vec<String>> {
        let mut problems = vec![];
        let parsed = read_json::<serde_json::Value>(&self.graph)
            .map_err(|e| vec![format!("--graph {e}")])
            .and_then(|json| graph::parse(&json));
        let out_text = match &self.out_schema {
            Some(path) => flwrs_plugin::cli::read_text(path)
                .map_err(|e| problems.push(format!("--out-schema {e}")))
                .ok(),
            None => Some(DEFAULT_OUT_SCHEMA.to_string()),
        };
        let out_schema = out_text.and_then(|text| match rules::parse_json(&text) {
            Ok(s) => {
                let errors = rules::check_well_formed(&s);
                if errors.is_empty() {
                    Some(s)
                } else {
                    problems.push(format!("out_schema: {}", errors.join("; ")));
                    None
                }
            }
            Err(e) => {
                problems.push(format!("out_schema: {e}"));
                None
            }
        });
        let execution = match &self.execution {
            None => Some(Execution::default()),
            Some(path) => match read_json::<Execution>(path) {
                Ok(e) => {
                    let own = e.problems();
                    if own.is_empty() {
                        Some(e)
                    } else {
                        problems.extend(own);
                        None
                    }
                }
                Err(e) => {
                    problems.push(format!("--execution {e}"));
                    None
                }
            },
        };
        let input = match &self.input {
            None => None,
            Some(path) => match read_json::<InputConfig>(path) {
                Ok(input) => {
                    problems.extend(input.problems());
                    Some(input)
                }
                Err(e) => {
                    problems.push(format!("--input {e}"));
                    None
                }
            },
        };
        let live = match &self.live {
            None => Some(LiveConfig::default()),
            Some(path) => match read_json::<LiveConfig>(path) {
                Ok(live) => {
                    problems.extend(live.problems());
                    Some(live)
                }
                Err(e) => {
                    problems.push(format!("--live {e}"));
                    None
                }
            },
        };
        let mut files = BTreeMap::new();
        for path in &self.files {
            match path.file_name().and_then(|n| n.to_str()) {
                Some(name) => {
                    if files.insert(name.to_string(), path.clone()).is_some() {
                        problems.push(format!("--file: two files named {name:?}"));
                    }
                }
                None => problems.push(format!("--file {}: not a file name", path.display())),
            }
        }
        let graph = parsed.map_err(|p| problems.extend(p)).ok();
        if let Some((graph, _)) = &graph {
            for name in graph::uploads(graph) {
                if !files.contains_key(&name) {
                    problems.push(format!(
                        "the graph names the file {name:?}, which isn't among the node's files (upload it and add it to files)"
                    ));
                }
            }
        }
        match (graph, out_schema, execution, live) {
            (Some((graph, checked)), Some(out_schema), Some(execution), Some(live))
                if problems.is_empty() =>
            {
                Ok(Settings {
                    graph,
                    checked,
                    files,
                    execution,
                    input,
                    out_schema,
                    live,
                })
            }
            _ => Err(problems),
        }
    }
}
