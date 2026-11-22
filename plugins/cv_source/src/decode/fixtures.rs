//! Decoder test fixtures: for each decoder, outputs whose decoding is known.
//! Shared by the decoder tests, the fuzz test and the runtime tests (which
//! run them through generated models).

use super::{Context, Decoder};
use crate::results::{MapSummary, Results};
use crate::tensor::{Output, Outputs};
use serde_json::json;

pub struct Fixture {
    pub name: &'static str,
    pub decoder: Decoder,
    pub width: f32,
    pub height: f32,
    pub outputs: Vec<(&'static str, Vec<usize>, Vec<f32>)>,
    pub labels: Vec<String>,
    pub charset: Vec<String>,
    /// The model node's cutoff (default: the decoder's).
    pub min_confidence: Option<f32>,
    pub check: fn(&Results),
}

impl Fixture {
    pub fn outputs(&self) -> Outputs {
        self.outputs
            .iter()
            .map(|(n, s, d)| (n.to_string(), Output::new(s.clone(), d.clone()).unwrap()))
            .collect()
    }

    pub fn decode(&self, outputs: &Outputs) -> Result<Results, String> {
        let cx = Context {
            width: self.width,
            height: self.height,
            labels: &self.labels,
            charset: &self.charset,
            min_confidence: self
                .min_confidence
                .or(self.decoder.default_min_confidence())
                .unwrap_or(0.0),
        };
        self.decoder.decode(outputs, &cx)
    }
}

fn decoder(v: serde_json::Value) -> Decoder {
    serde_json::from_value(v).unwrap()
}

fn objects(r: &Results) -> &[crate::results::Object] {
    match r {
        Results::Objects(o) => o,
        other => panic!("not objects: {other:?}"),
    }
}

fn close(a: f32, b: f32) -> bool {
    (a - b).abs() < 1e-3
}

fn close_box(a: [f32; 4], b: [f32; 4]) -> bool {
    a.iter().zip(b).all(|(x, y)| close(*x, y))
}

/// `[1, C, N]` data from rows (channels first).
fn channels_first(rows: &[&[f32]]) -> Vec<f32> {
    let c = rows[0].len();
    (0..c)
        .flat_map(|ch| rows.iter().map(move |r| r[ch]))
        .collect()
}

fn base(
    name: &'static str,
    decoder: Decoder,
    size: f32,
    outputs: Vec<(&'static str, Vec<usize>, Vec<f32>)>,
    check: fn(&Results),
) -> Fixture {
    Fixture {
        name,
        decoder,
        width: size,
        height: size,
        outputs,
        labels: vec![],
        charset: vec![],
        min_confidence: None,
        check,
    }
}

pub fn all() -> Vec<Fixture> {
    vec![
        Fixture {
            labels: vec!["a".into(), "b".into(), "c".into(), "d".into()],
            ..base(
                "classify",
                decoder(json!({"type": "classify", "output": "probs", "softmax": true, "topk": 2})),
                8.0,
                vec![("probs", vec![1, 4], vec![1.0, 3.0, 2.0, 0.0])],
                |r| {
                    let Results::Classes(c) = r else { panic!() };
                    assert_eq!(c.iter().map(|c| c.class_id).collect::<Vec<_>>(), [1, 2]);
                    assert_eq!(c[0].class_name.as_deref(), Some("b"));
                    assert!(close(c[0].confidence, 0.6439));
                },
            )
        },
        base(
            "yolo",
            decoder(json!({"type": "yolo", "output": "out", "channels_first": true, "classes": 2})),
            32.0,
            vec![(
                "out",
                vec![1, 6, 3],
                channels_first(&[
                    &[16.0, 16.0, 8.0, 8.0, 0.1, 0.9],
                    &[17.0, 16.0, 8.0, 8.0, 0.1, 0.8], // overlaps the first: suppressed
                    &[5.0, 5.0, 4.0, 4.0, 0.6, 0.1],
                ]),
            )],
            |r| {
                let o = objects(r);
                assert_eq!(o.len(), 2);
                assert_eq!((o[0].class_id, o[1].class_id), (1, 0));
                assert!(
                    close(o[0].confidence, 0.9) && close_box(o[0].bbox, [12.0, 12.0, 20.0, 20.0])
                );
                assert!(close_box(o[1].bbox, [3.0, 3.0, 7.0, 7.0]));
            },
        ),
        base(
            "yolo_grid",
            decoder(
                json!({"type": "yolo", "output": "out", "channels_first": false, "classes": 1, "objectness": true, "grid": [32]}),
            ),
            64.0,
            vec![(
                "out",
                vec![1, 4, 6],
                [vec![0.0; 18], vec![0.5, 0.5, 0.0, 0.0, 0.9, 0.8]].concat(),
            )],
            |r| {
                let o = objects(r);
                assert_eq!(o.len(), 1);
                assert!(
                    close(o[0].confidence, 0.72) && close_box(o[0].bbox, [32.0, 32.0, 64.0, 64.0])
                );
            },
        ),
        base(
            "yolo_pose",
            decoder(
                json!({"type": "yolo", "output": "out", "channels_first": true, "classes": 1, "keypoints": 2}),
            ),
            32.0,
            vec![(
                "out",
                vec![1, 11, 1],
                vec![10.0, 10.0, 4.0, 4.0, 0.7, 9.0, 8.0, 0.5, 11.0, 12.0, 2.0],
            )],
            |r| {
                let o = objects(r);
                assert_eq!(o[0].keypoints.len(), 2);
                assert_eq!(
                    (
                        o[0].keypoints[0].x,
                        o[0].keypoints[0].y,
                        o[0].keypoints[0].confidence
                    ),
                    (9.0, 8.0, 0.5)
                );
                assert_eq!(o[0].keypoints[1].confidence, 1.0); // clamped
            },
        ),
        Fixture {
            labels: vec!["a".into(), "b".into(), "c".into(), "d".into()],
            ..base(
                "detections_e2e",
                decoder(json!({"type": "detections_e2e", "output": "rows"})),
                32.0,
                vec![(
                    "rows",
                    vec![1, 2, 6],
                    vec![1.0, 2.0, 11.0, 12.0, 0.9, 3.0, 0.0, 0.0, 1.0, 1.0, 0.1, 0.0],
                )],
                |r| {
                    let o = objects(r);
                    assert_eq!(o.len(), 1);
                    assert_eq!((o[0].class_id, o[0].class_name.as_deref()), (3, Some("d")));
                    assert_eq!(o[0].bbox, [1.0, 2.0, 11.0, 12.0]);
                },
            )
        },
        base(
            "detr",
            decoder(
                json!({"type": "detr", "logits": "logits", "boxes": "boxes", "activation": "sigmoid"}),
            ),
            32.0,
            vec![
                ("logits", vec![1, 2, 2], vec![3.0, -3.0, -5.0, -5.0]),
                (
                    "boxes",
                    vec![1, 2, 4],
                    vec![0.5, 0.5, 0.5, 0.5, 0.1, 0.1, 0.1, 0.1],
                ),
            ],
            |r| {
                let o = objects(r);
                assert_eq!(o.len(), 1);
                assert!(
                    close(o[0].confidence, 0.9526) && close_box(o[0].bbox, [8.0, 8.0, 24.0, 24.0])
                );
            },
        ),
        base(
            "detr_softmax",
            decoder(
                json!({"type": "detr", "logits": "logits", "boxes": "boxes", "activation": "softmax_no_object"}),
            ),
            32.0,
            vec![
                // Query 0: class 1; query 1: mostly "no object".
                ("logits", vec![1, 2, 3], vec![0.0, 5.0, 0.0, 0.0, 0.0, 9.0]),
                (
                    "boxes",
                    vec![1, 2, 4],
                    vec![0.5, 0.5, 1.0, 1.0, 0.5, 0.5, 1.0, 1.0],
                ),
            ],
            |r| {
                let o = objects(r);
                assert_eq!((o.len(), o[0].class_id), (1, 1));
                assert!(close_box(o[0].bbox, [0.0, 0.0, 32.0, 32.0]));
            },
        ),
        base(
            "gfl",
            decoder(
                json!({"type": "gfl", "cls": ["cls"], "reg": ["reg"], "strides": [32], "reg_max": 1}),
            ),
            64.0,
            vec![
                ("cls", vec![1, 4, 1], vec![0.9, 0.0, 0.0, 0.0]),
                // Cell 0: every side all in bin 1 (one stride).
                (
                    "reg",
                    vec![1, 4, 8],
                    [
                        vec![-50.0, 50.0, -50.0, 50.0, -50.0, 50.0, -50.0, 50.0],
                        vec![0.0; 24],
                    ]
                    .concat(),
                ),
            ],
            |r| {
                let o = objects(r);
                assert_eq!(o.len(), 1);
                assert!(close_box(o[0].bbox, [-32.0, -32.0, 32.0, 32.0]));
            },
        ),
        base(
            "ssd_anchors",
            decoder(
                json!({"type": "ssd_anchors", "boxes": "boxes", "scores": "scores", "strides": [32], "keypoints": 1}),
            ),
            64.0,
            vec![
                (
                    "scores",
                    vec![1, 8, 1],
                    [vec![5.0], vec![-10.0; 7]].concat(),
                ),
                (
                    "boxes",
                    vec![1, 8, 6],
                    [vec![1.0, 2.0, 10.0, 10.0, 3.0, 4.0], vec![0.0; 42]].concat(),
                ),
            ],
            |r| {
                let o = objects(r);
                assert_eq!(o.len(), 1);
                assert!(close_box(o[0].bbox, [12.0, 13.0, 22.0, 23.0]));
                assert_eq!((o[0].keypoints[0].x, o[0].keypoints[0].y), (19.0, 20.0));
            },
        ),
        base(
            "yunet",
            decoder(json!({"type": "yunet", "strides": [32], "keypoints": 1})),
            64.0,
            vec![
                ("cls_32", vec![1, 4, 1], vec![0.81, 0.0, 0.0, 0.0]),
                ("obj_32", vec![1, 4, 1], vec![1.0, 0.0, 0.0, 0.0]),
                (
                    "bbox_32",
                    vec![1, 4, 4],
                    [vec![0.5, 0.5, 0.0, 0.0], vec![0.0; 12]].concat(),
                ),
                (
                    "kps_32",
                    vec![1, 4, 2],
                    [vec![0.25, 0.25], vec![0.0; 6]].concat(),
                ),
            ],
            |r| {
                let o = objects(r);
                assert_eq!(o.len(), 1);
                assert!(
                    close(o[0].confidence, 0.9) && close_box(o[0].bbox, [0.0, 0.0, 32.0, 32.0])
                );
                assert_eq!((o[0].keypoints[0].x, o[0].keypoints[0].y), (8.0, 8.0));
            },
        ),
        base(
            "movenet",
            decoder(json!({"type": "movenet", "output": "kps", "multi": false})),
            32.0,
            // Keypoint k at y = k / 32 (normalised), x = 0.5, score 0.9.
            vec![(
                "kps",
                vec![1, 1, 17, 3],
                (0..17).flat_map(|k| [k as f32 / 32.0, 0.5, 0.9]).collect(),
            )],
            |r| {
                let o = objects(r);
                assert_eq!(
                    (o.len(), o[0].keypoints.len(), o[0].class_name.as_deref()),
                    (1, 17, Some("person"))
                );
                assert!(close_box(o[0].bbox, [16.0, 0.0, 16.0, 16.0]));
                assert_eq!((o[0].keypoints[3].x, o[0].keypoints[3].y), (16.0, 3.0));
            },
        ),
        base(
            "movenet_multi",
            decoder(json!({"type": "movenet", "output": "people", "multi": true})),
            100.0,
            vec![(
                "people",
                vec![1, 6, 56],
                [
                    vec![0.0; 51],
                    vec![0.1, 0.2, 0.5, 0.6, 0.8],
                    vec![0.0; 56 * 5],
                ]
                .concat(),
            )],
            |r| {
                let o = objects(r);
                assert_eq!(o.len(), 1);
                assert!(
                    close_box(o[0].bbox, [20.0, 10.0, 60.0, 50.0]) && close(o[0].confidence, 0.8)
                );
            },
        ),
        base(
            "rtmo",
            decoder(json!({"type": "rtmo", "dets": "dets", "keypoints": "kps"})),
            32.0,
            vec![
                (
                    "dets",
                    vec![1, 2, 5],
                    vec![1.0, 2.0, 3.0, 4.0, 0.9, 0.0, 0.0, 0.0, 0.0, 0.1],
                ),
                ("kps", vec![1, 2, 1, 3], vec![2.0, 3.0, 0.5, 0.0, 0.0, 0.0]),
            ],
            |r| {
                let o = objects(r);
                assert_eq!(o.len(), 1);
                assert_eq!(o[0].bbox, [1.0, 2.0, 3.0, 4.0]);
                assert_eq!(
                    (o[0].keypoints[0].x, o[0].keypoints[0].confidence),
                    (2.0, 0.5)
                );
            },
        ),
        base(
            "simcc",
            decoder(json!({"type": "simcc", "x": "sx", "y": "sy", "split": 2.0})),
            4.0,
            vec![
                (
                    "sx",
                    vec![1, 1, 8],
                    vec![0.0, 0.0, 0.0, 0.0, 0.8, 0.0, 0.0, 0.0],
                ),
                (
                    "sy",
                    vec![1, 1, 8],
                    vec![0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.6, 0.0],
                ),
            ],
            |r| {
                let o = objects(r);
                let k = o[0].keypoints[0];
                assert_eq!((k.x, k.y, k.confidence), (2.0, 3.0, 0.6));
                assert!(close(o[0].confidence, 0.6));
            },
        ),
        base(
            "heatmap",
            decoder(json!({"type": "heatmap", "output": "hm"})),
            16.0,
            vec![(
                "hm",
                vec![1, 1, 4, 4],
                vec![
                    0.0, 0.0, 0.0, 0.0, 0.0, 0.1, 0.9, 0.3, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0,
                ],
            )],
            |r| {
                let k = objects(r)[0].keypoints[0];
                assert_eq!((k.x, k.y, k.confidence), (11.0, 6.0, 0.9));
            },
        ),
        base(
            "landmarks",
            decoder(
                json!({"type": "landmarks", "output": "lm", "points": 2, "dims": 3, "visibility_index": 2, "presence": "p", "presence_sigmoid": true}),
            ),
            32.0,
            vec![
                ("lm", vec![1, 6], vec![1.0, 2.0, 0.0, 3.0, 4.0, 100.0]),
                ("p", vec![1, 1], vec![2.0]),
            ],
            |r| {
                let o = objects(r);
                assert!(close(o[0].confidence, 0.8808));
                assert_eq!(
                    (o[0].keypoints[0].confidence, o[0].keypoints[1].x),
                    (0.5, 3.0)
                );
            },
        ),
        Fixture {
            labels: vec!["sky".into(), "road".into()],
            ..base(
                "semantic",
                decoder(json!({"type": "semantic", "output": "seg"})),
                2.0,
                vec![(
                    "seg",
                    vec![1, 2, 2, 2],
                    vec![1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 1.0, 1.0],
                )],
                |r| {
                    let Results::Map(m) = r else { panic!() };
                    assert_eq!(m.values, [0.0, 1.0, 1.0, 1.0]);
                    let MapSummary::Classes(c) = &m.summary else {
                        panic!()
                    };
                    assert_eq!(
                        (c[0].0.class_name.as_deref(), c[0].1, c[1].1),
                        (Some("road"), 0.75, 0.25)
                    );
                },
            )
        },
        base(
            "dense_map",
            decoder(json!({"type": "dense_map", "output": "depth", "threshold": 1.5})),
            2.0,
            vec![("depth", vec![1, 1, 2, 2], vec![0.0, 1.0, 2.0, 3.0])],
            |r| {
                let Results::Map(m) = r else { panic!() };
                assert_eq!(
                    m.summary,
                    MapSummary::Values {
                        min: 0.0,
                        max: 3.0,
                        mean: 1.5,
                        above_threshold: Some(0.5)
                    }
                );
            },
        ),
        base(
            "ocr_det",
            decoder(json!({"type": "ocr_det", "output": "prob", "threshold": 0.5, "min_area": 2})),
            8.0,
            vec![(
                "prob",
                vec![1, 1, 4, 4],
                vec![
                    0.9, 0.9, 0.0, 0.0, 0.9, 0.9, 0.0, 0.0, 0.0, 0.0, 0.0, 0.9, 0.0, 0.0, 0.0, 0.0,
                ],
            )],
            |r| {
                let o = objects(r);
                // The 2x2 block (the single pixel is below min_area), grown
                // by 0.75 and scaled to the 8x8 input.
                assert_eq!(o.len(), 1);
                assert!(
                    close_box(o[0].bbox, [-1.5, -1.5, 5.5, 5.5]) && close(o[0].confidence, 0.9)
                );
            },
        ),
        Fixture {
            charset: vec!["a".into(), "b".into()],
            ..base(
                "ctc",
                decoder(json!({"type": "ctc", "output": "seq"})),
                8.0,
                // Steps: a, a (repeat), blank, b.
                vec![(
                    "seq",
                    vec![1, 4, 3],
                    vec![0.0, 0.9, 0.1, 0.0, 0.9, 0.1, 0.9, 0.0, 0.1, 0.1, 0.0, 0.7],
                )],
                |r| {
                    let Results::Text(Some(t)) = r else { panic!() };
                    assert_eq!(t.text, "ab");
                    assert!(close(t.confidence, 0.8));
                },
            )
        },
        base(
            "embedding",
            decoder(json!({"type": "embedding", "output": "emb"})),
            8.0,
            vec![("emb", vec![1, 2], vec![3.0, 4.0])],
            |r| assert_eq!(r, &Results::Vector(vec![0.6, 0.8])),
        ),
        base(
            "raw",
            decoder(json!({"type": "raw", "outputs": ["t"]})),
            8.0,
            vec![("t", vec![1, 2], vec![1.0, 2.0])],
            |r| {
                assert_eq!(
                    r,
                    &Results::Tensor(vec![("t".into(), vec![1, 2], vec![1.0, 2.0])])
                )
            },
        ),
    ]
}
