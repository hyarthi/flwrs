//! Tiny ONNX models for tests, written as protobuf by hand (no Python, no
//! files in the repo): an identity model, and models whose outputs are
//! fixed tensors (so a decoder can be checked through each runtime).

use crate::tensor::Output;

/// A protobuf message being written.
#[derive(Default)]
struct Message(Vec<u8>);

impl Message {
    fn varint(&mut self, mut v: u64) {
        loop {
            let byte = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                self.0.push(byte);
                break;
            }
            self.0.push(byte | 0x80);
        }
    }

    fn key(&mut self, field: u32, wire: u8) {
        self.varint(((field as u64) << 3) | wire as u64);
    }

    fn int(mut self, field: u32, v: i64) -> Self {
        self.key(field, 0);
        self.varint(v as u64);
        self
    }

    fn bytes(mut self, field: u32, v: &[u8]) -> Self {
        self.key(field, 2);
        self.varint(v.len() as u64);
        self.0.extend_from_slice(v);
        self
    }

    fn str(self, field: u32, v: &str) -> Self {
        self.bytes(field, v.as_bytes())
    }

    fn msg(self, field: u32, m: Message) -> Self {
        self.bytes(field, &m.0)
    }
}

/// ONNX element types.
pub const FLOAT: i64 = 1;
pub const UINT8: i64 = 2;
pub const INT8: i64 = 3;
pub const INT32: i64 = 6;

fn value_info(name: &str, elem: i64, shape: &[usize]) -> Message {
    let mut dims = Message::default();
    for d in shape {
        dims = dims.msg(1, Message::default().int(1, *d as i64));
    }
    let tensor_type = Message::default().int(1, elem).msg(2, dims);
    Message::default()
        .str(1, name)
        .msg(2, Message::default().msg(1, tensor_type))
}

fn tensor(name: &str, shape: &[usize], values: &[f32]) -> Message {
    let mut t = Message::default();
    for d in shape {
        t = t.int(1, *d as i64);
    }
    let raw: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
    t.int(2, FLOAT).str(8, name).bytes(9, &raw)
}

fn node(op: &str, inputs: &[&str], output: &str, attributes: Vec<Message>) -> Message {
    let mut n = Message::default();
    for i in inputs {
        n = n.str(1, i);
    }
    n = n.str(2, output).str(3, output).str(4, op);
    for a in attributes {
        n = n.msg(5, a);
    }
    n
}

fn model(graph: Message, metadata: &[(&str, &str)]) -> Vec<u8> {
    let mut m = Message::default()
        .int(1, 7) // IR version 7
        .str(2, "flwrs-cv-tests")
        .msg(7, graph)
        .msg(8, Message::default().str(1, "").int(2, 13)); // opset 13
    for (k, v) in metadata {
        m = m.msg(14, Message::default().str(1, k).str(2, v));
    }
    m.0
}

/// `y = x` for an input of `shape` and element type `elem` (output `float`
/// through a Cast when `elem` isn't).
pub fn identity(shape: &[usize], elem: i64) -> Vec<u8> {
    let mut graph = Message::default().str(2, "identity");
    if elem == FLOAT {
        graph = graph.msg(1, node("Identity", &["x"], "y", vec![]));
    } else {
        let to = Message::default().str(1, "to").int(3, FLOAT).int(20, 2);
        graph = graph.msg(1, node("Cast", &["x"], "y", vec![to]));
    }
    let graph = graph
        .msg(11, value_info("x", elem, shape))
        .msg(12, value_info("y", FLOAT, shape));
    model(graph, &[])
}

/// A model with an input `x` of `shape` whose outputs are these tensors
/// whatever the input (each is `constant + 0 · mean(x)`, so the input is
/// used). `cast` turns an output into another element type (e.g. `UINT8`).
pub fn constant_outputs(
    input: &[usize],
    outputs: &[(&str, &Output)],
    cast: &[(&str, i64)],
    metadata: &[(&str, &str)],
) -> Vec<u8> {
    let keepdims = Message::default().str(1, "keepdims").int(3, 0).int(20, 2);
    let mut graph = Message::default()
        .str(2, "constants")
        .msg(1, node("ReduceMean", &["x"], "mean", vec![keepdims]))
        .msg(1, node("Mul", &["mean", "zero"], "nothing", vec![]))
        .msg(5, tensor("zero", &[], &[0.0]))
        .msg(11, value_info("x", FLOAT, input));
    for (name, out) in outputs {
        let constant = format!("{name}_value");
        let elem = cast
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, e)| *e)
            .unwrap_or(FLOAT);
        graph = graph.msg(5, tensor(&constant, out.shape(), out.data()));
        if elem == FLOAT {
            graph = graph.msg(1, node("Add", &[&constant, "nothing"], name, vec![]));
        } else {
            let sum = format!("{name}_sum");
            let to = Message::default().str(1, "to").int(3, elem).int(20, 2);
            graph = graph
                .msg(1, node("Add", &[&constant, "nothing"], &sum, vec![]))
                .msg(1, node("Cast", &[&sum], name, vec![to]));
        }
        graph = graph.msg(12, value_info(name, elem, out.shape()));
    }
    model(graph, metadata)
}

/// The mean colour of an image: `y` of shape `[batch, 3]` from `x` of shape
/// `[batch, 3, height, width]`. With `batch` `None` the batch is a dynamic
/// dimension, so one run can take several images.
pub fn mean_colour(batch: Option<usize>, height: usize, width: usize) -> Vec<u8> {
    let dim = |d: Option<usize>| match d {
        Some(d) => Message::default().int(1, d as i64),
        None => Message::default().str(2, "batch"),
    };
    let info = |name: &str, dims: &[Option<usize>]| {
        let mut shape = Message::default();
        for d in dims {
            shape = shape.msg(1, dim(*d));
        }
        let tensor_type = Message::default().int(1, FLOAT).msg(2, shape);
        Message::default()
            .str(1, name)
            .msg(2, Message::default().msg(1, tensor_type))
    };
    let axis = Message::default().str(1, "axis").int(3, 1).int(20, 2);
    let graph = Message::default()
        .str(2, "mean_colour")
        .msg(1, node("GlobalAveragePool", &["x"], "pooled", vec![]))
        .msg(1, node("Flatten", &["pooled"], "y", vec![axis]))
        .msg(11, info("x", &[batch, Some(3), Some(height), Some(width)]))
        .msg(12, info("y", &[batch, Some(3)]));
    model(graph, &[])
}

/// A gesture embedder's shape: inputs `hand`, `handedness`, `world_hand`,
/// and a fixed output `embedding` of `len` values (all inputs used).
#[cfg(feature = "ort")]
pub fn hand_embedder(len: usize) -> Vec<u8> {
    let keepdims = || Message::default().str(1, "keepdims").int(3, 0).int(20, 2);
    let graph = Message::default()
        .str(2, "embedder")
        .msg(1, node("ReduceMean", &["hand"], "m1", vec![keepdims()]))
        .msg(
            1,
            node("ReduceMean", &["handedness"], "m2", vec![keepdims()]),
        )
        .msg(
            1,
            node("ReduceMean", &["world_hand"], "m3", vec![keepdims()]),
        )
        .msg(1, node("Add", &["m1", "m2"], "m12", vec![]))
        .msg(1, node("Add", &["m12", "m3"], "sum", vec![]))
        .msg(1, node("Mul", &["sum", "zero"], "nothing", vec![]))
        .msg(1, node("Add", &["fixed", "nothing"], "embedding", vec![]))
        .msg(5, tensor("zero", &[], &[0.0]))
        .msg(5, tensor("fixed", &[1, len], &vec![0.5; len]))
        .msg(11, value_info("hand", FLOAT, &[1, 21, 3]))
        .msg(11, value_info("handedness", FLOAT, &[1, 1]))
        .msg(11, value_info("world_hand", FLOAT, &[1, 21, 3]))
        .msg(12, value_info("embedding", FLOAT, &[1, len]));
    model(graph, &[])
}

/// A deterministic pseudo-random generator (xorshift), for fuzz tests.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }

    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// 0..n.
    pub fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }

    /// A value from a mix: ordinary numbers, extremes, NaN and infinities.
    pub fn value(&mut self) -> f32 {
        match self.below(20) {
            0 => f32::NAN,
            1 => f32::INFINITY,
            2 => f32::NEG_INFINITY,
            3 => f32::MAX,
            4 => -1e30,
            5 => 0.0,
            _ => (self.next() % 2_000_000) as f32 / 1_000_000.0 * 4.0 - 4.0,
        }
    }
}
