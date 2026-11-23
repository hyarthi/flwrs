//! The model graph inside the plugin: nodes with typed ports, linked like a
//! small hub. The settings (`config`), and their check (`check`).

mod check;
pub mod config;
pub mod crop;
pub mod exec;

pub use check::{Checked, check};
pub use config::{GraphConfig, NodeConfig};

/// What travels on a link. Coordinates are always frame pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum PortType {
    /// The frame (or a region of it, with its affine).
    Frame,
    /// Regions of the frame, one per object (a crop node's output); linked
    /// to a `frame` input, the node runs once per region.
    Regions,
    /// Objects: class, score, box, keypoints, children.
    Objects,
    /// Top-k classes with scores.
    Classes,
    /// A per-pixel map (segmentation ids, depth) + summary statistics.
    Map,
    /// Text + score (OCR).
    Text,
    /// An embedding.
    Vector,
    /// Raw tensors.
    Tensor,
    /// Rule events.
    Events,
}

impl PortType {
    pub fn name(self) -> &'static str {
        match self {
            PortType::Frame => "frame",
            PortType::Regions => "regions",
            PortType::Objects => "objects",
            PortType::Classes => "classes",
            PortType::Map => "map",
            PortType::Text => "text",
            PortType::Vector => "vector",
            PortType::Tensor => "tensor",
            PortType::Events => "events",
        }
    }
}

/// A node's ports, by name. An output port is named after its type.
#[derive(Debug)]
pub struct Ports {
    pub inputs: Vec<(&'static str, PortType)>,
    pub outputs: Vec<(&'static str, PortType)>,
}

impl Ports {
    pub fn of(node: &NodeConfig) -> Self {
        let out = |t: PortType| vec![(t.name(), t)];
        match node {
            NodeConfig::Source {} => Ports {
                inputs: vec![],
                outputs: out(PortType::Frame),
            },
            NodeConfig::Model(m) => Ports {
                inputs: if m.tensor_input.is_some() {
                    vec![("objects", PortType::Objects)]
                } else {
                    vec![("frame", PortType::Frame)]
                },
                outputs: out(m.decode.result_type()),
            },
            NodeConfig::Crop(_) => Ports {
                inputs: vec![("frame", PortType::Frame), ("objects", PortType::Objects)],
                outputs: out(PortType::Regions),
            },
            NodeConfig::Filter(_) | NodeConfig::Tracker(_) => Ports {
                inputs: vec![("objects", PortType::Objects)],
                outputs: out(PortType::Objects),
            },
            NodeConfig::Rule(r) => Ports {
                inputs: match r.input {
                    config::RuleInput::Objects => vec![("objects", PortType::Objects)],
                    config::RuleInput::Classes => vec![("classes", PortType::Classes)],
                },
                outputs: out(PortType::Events),
            },
            NodeConfig::Lua(_) => Ports {
                inputs: vec![("objects", PortType::Objects)],
                outputs: out(PortType::Events),
            },
        }
    }

    pub fn input(&self, name: &str) -> Option<PortType> {
        self.inputs
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, t)| *t)
    }

    pub fn output(&self, name: &str) -> Option<PortType> {
        self.outputs
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, t)| *t)
    }

    pub fn input_names(&self) -> String {
        names(&self.inputs)
    }

    pub fn output_names(&self) -> String {
        names(&self.outputs)
    }
}

fn names(ports: &[(&str, PortType)]) -> String {
    if ports.is_empty() {
        "none".into()
    } else {
        ports.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(", ")
    }
}

/// Parses and checks a graph's JSON.
pub fn parse(json: &serde_json::Value) -> Result<(GraphConfig, Checked), Vec<String>> {
    let graph: GraphConfig =
        serde_json::from_value(json.clone()).map_err(|e| vec![format!("graph: {e}")])?;
    let checked = check(&graph)?;
    Ok((graph, checked))
}

/// The upload names a graph refers to (models, label files, charsets).
pub fn uploads(graph: &GraphConfig) -> Vec<String> {
    use config::{LabelSource, Labels, TextSource};
    let mut names = std::collections::BTreeSet::new();
    for node in graph.nodes.values() {
        if let NodeConfig::Model(m) = node {
            names.insert(m.model.clone());
            if let Some(Labels::From(LabelSource::File(f) | LabelSource::HfConfig(f))) = &m.labels {
                names.insert(f.clone());
            }
            if let Some(TextSource::File(f)) = &m.charset {
                names.insert(f.clone());
            }
        }
    }
    names.into_iter().collect()
}
