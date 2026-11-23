//! The decoders' implementations. Each checks the shapes it reads, so any
//! output (wrong model, wrong settings, NaN) gives an error or results,
//! never a panic; results are sanitized by the caller.

use super::Decoder;
use super::util::*;
use crate::results::{Class, Keypoint, Map, MapSummary, Object, Results, Text};
use crate::tensor::Outputs;

fn object(class_id: u32, class_name: Option<String>, confidence: f32, bbox: [f32; 4]) -> Object {
    Object {
        tracker_id: None,
        class_id,
        class_name,
        confidence,
        bbox,
        keypoints: vec![],
        children: vec![],
    }
}

fn kp(x: f32, y: f32, confidence: f32) -> Keypoint {
    Keypoint { x, y, confidence }
}

/// A whole-input pose (one object holding a person crop's keypoints), or
/// nothing below the cutoff.
fn pose(keypoints: Vec<Keypoint>, cx: &Context) -> Results {
    let confidence = mean_confidence(&keypoints);
    if !reaches(confidence, cx.min_confidence) {
        return Results::Objects(vec![]);
    }
    let mut o = object(0, cx.label(0), confidence, [0.0, 0.0, cx.width, cx.height]);
    o.keypoints = keypoints;
    Results::Objects(vec![o])
}

/// The mean confidence of a set of keypoints.
fn mean_confidence(kps: &[Keypoint]) -> f32 {
    if kps.is_empty() {
        0.0
    } else {
        kps.iter().map(|k| k.confidence).sum::<f32>() / kps.len() as f32
    }
}

impl Decoder {
    /// Decodes a run's outputs (coordinates in model-input pixels).
    pub fn decode(&self, out: &Outputs, cx: &Context) -> Result<Results, String> {
        let mut results = self.decode_raw(out, cx)?;
        results.sanitize();
        Ok(results)
    }

    fn decode_raw(&self, out: &Outputs, cx: &Context) -> Result<Results, String> {
        Ok(match self {
            Decoder::Classify {
                output,
                softmax: soft,
                topk,
            } => {
                let t = get(out, output)?;
                let [_] = dims::<1>(t, output, "[1, C]")?;
                let mut p = t.data().to_vec();
                if *soft {
                    softmax(&mut p);
                }
                let mut idx: Vec<usize> = (0..p.len()).collect();
                let key = |v: f32| if v.is_nan() { f32::NEG_INFINITY } else { v };
                idx.sort_by(|a, b| key(p[*b]).total_cmp(&key(p[*a])));
                Results::Classes(
                    idx.into_iter()
                        .take(*topk)
                        .filter(|&i| reaches(p[i], cx.min_confidence))
                        .map(|i| Class {
                            class_id: i as u32,
                            class_name: cx.label(i as u32),
                            confidence: p[i],
                        })
                        .collect(),
                )
            }
            Decoder::Yolo {
                output,
                channels_first,
                classes,
                objectness,
                keypoints,
                grid,
                iou,
            } => {
                let t = get(out, output)?;
                let [a, b] = dims::<2>(t, output, "[1, C, N] or [1, N, C]")?;
                let (c, n) = if *channels_first { (a, b) } else { (b, a) };
                let first = 4 + *objectness as usize;
                let need = first + classes + keypoints * 3;
                ensure(c >= need, || {
                    format!(
                        "output {output:?} has {c} values per row; the settings need {need} (4 box + objectness + {classes} classes + {keypoints} keypoints x 3)"
                    )
                })?;
                let data = t.data();
                let v = |ch: usize, i: usize| {
                    if *channels_first {
                        data[ch * n + i]
                    } else {
                        data[i * c + ch]
                    }
                };
                let cells: Vec<(f32, f32, f32)> = grid
                    .iter()
                    .flat_map(|&s| {
                        let (gw, gh) = (cx.width as u32 / s.max(1), cx.height as u32 / s.max(1));
                        (0..gh)
                            .flat_map(move |y| (0..gw).map(move |x| (x as f32, y as f32, s as f32)))
                    })
                    .collect();
                ensure(grid.is_empty() || cells.len() == n, || {
                    format!(
                        "output {output:?} has {n} rows, but the grid strides {grid:?} give {} cells",
                        cells.len()
                    )
                })?;
                let mut objects = vec![];
                for i in 0..n {
                    let obj = if *objectness { v(4, i) } else { 1.0 };
                    let (mut k, mut best) = (0, f32::NEG_INFINITY);
                    for class in 0..*classes {
                        let s = v(first + class, i);
                        if s > best {
                            (k, best) = (class, s);
                        }
                    }
                    let s = best * obj;
                    if !reaches(s, cx.min_confidence) {
                        continue;
                    }
                    let (mut bx, mut by, mut bw, mut bh) = (v(0, i), v(1, i), v(2, i), v(3, i));
                    if let Some(&(gx, gy, st)) = cells.get(i) {
                        bx = (bx + gx) * st;
                        by = (by + gy) * st;
                        bw = bw.exp() * st;
                        bh = bh.exp() * st;
                    }
                    let kb = first + classes;
                    let mut o = object(k as u32, cx.label(k as u32), s, cxcywh(bx, by, bw, bh));
                    o.keypoints = (0..*keypoints)
                        .map(|j| kp(v(kb + j * 3, i), v(kb + j * 3 + 1, i), v(kb + j * 3 + 2, i)))
                        .collect();
                    objects.push(o);
                }
                Results::Objects(nms(objects, *iou, MAX_DETECTIONS))
            }
            Decoder::DetectionsE2e { output } => {
                let t = get(out, output)?;
                let [_, w] = dims::<2>(t, output, "[1, N, 6]")?;
                ensure(w >= 6, || {
                    format!(
                        "output {output:?} has rows of {w} values; expected x1 y1 x2 y2 score class"
                    )
                })?;
                let objects = t
                    .data()
                    .chunks(w)
                    .filter(|r| r[4] >= cx.min_confidence)
                    .filter_map(|r| {
                        let class = class_of(r[5])?;
                        Some(object(
                            class,
                            cx.label(class),
                            r[4],
                            [r[0], r[1], r[2], r[3]],
                        ))
                    })
                    .take(MAX_DETECTIONS)
                    .collect();
                Results::Objects(objects)
            }
            Decoder::Detr {
                logits,
                boxes,
                activation,
            } => {
                use super::DetrActivation as A;
                let (l, b) = (get(out, logits)?, get(out, boxes)?);
                let [q, c] = dims::<2>(l, logits, "[1, Q, C]")?;
                let [qb, four] = dims::<2>(b, boxes, "[1, Q, 4]")?;
                ensure(qb == q && four == 4, || {
                    format!(
                        "outputs {logits:?} and {boxes:?} have shapes {:?} and {:?}; expected [1, Q, C] and [1, Q, 4]",
                        l.shape(),
                        b.shape()
                    )
                })?;
                ensure(*activation != A::SoftmaxNoObject || c >= 2, || {
                    "softmax_no_object needs at least two classes (the last is \"no object\")"
                        .into()
                })?;
                let mut objects = vec![];
                for i in 0..q {
                    let row = &l.data()[i * c..(i + 1) * c];
                    let best = match activation {
                        A::Sigmoid => argmax(row).map(|(k, v)| (k, sigmoid(v))),
                        A::None => argmax(row),
                        A::SoftmaxNoObject => {
                            let mut p = row.to_vec();
                            softmax(&mut p);
                            argmax(&p[..c - 1])
                        }
                    };
                    let Some((k, s)) = best else { continue };
                    if !reaches(s, cx.min_confidence) {
                        continue;
                    }
                    let bb = &b.data()[i * 4..i * 4 + 4];
                    let bbox = cxcywh(
                        bb[0] * cx.width,
                        bb[1] * cx.height,
                        bb[2] * cx.width,
                        bb[3] * cx.height,
                    );
                    objects.push(object(k as u32, cx.label(k as u32), s, bbox));
                }
                // No NMS is needed; this only merges duplicate queries.
                Results::Objects(nms(objects, 0.7, MAX_DETECTIONS))
            }
            Decoder::Gfl {
                cls,
                reg,
                strides,
                reg_max,
                iou,
            } => {
                let bins = reg_max + 1;
                let mut objects = vec![];
                for ((cn, rn), &s) in cls.iter().zip(reg).zip(strides) {
                    let (c, r) = (get(out, cn)?, get(out, rn)?);
                    let [n, nc] = dims::<2>(c, cn, "[1, N, classes]")?;
                    let [nr, w] = dims::<2>(r, rn, "[1, N, 4 x (reg_max + 1)]")?;
                    let (cols, rows) = (
                        (cx.width / s.max(1) as f32).ceil() as usize,
                        (cx.height / s.max(1) as f32).ceil() as usize,
                    );
                    ensure(nr == n && w == 4 * bins && n == cols * rows, || {
                        format!(
                            "outputs {cn:?} {:?} and {rn:?} {:?} don't fit stride {s} on a {}x{} input with reg_max {reg_max} (expected [1, {}, C] and [1, {}, {}])",
                            c.shape(),
                            r.shape(),
                            cx.width,
                            cx.height,
                            cols * rows,
                            cols * rows,
                            4 * bins
                        )
                    })?;
                    for i in 0..n {
                        let Some((k, sc)) = argmax(&c.data()[i * nc..(i + 1) * nc]) else {
                            continue;
                        };
                        if !reaches(sc, cx.min_confidence) {
                            continue;
                        }
                        let d: Vec<f32> = (0..4)
                            .map(|side| {
                                let start = i * 4 * bins + side * bins;
                                let mut p = r.data()[start..start + bins].to_vec();
                                softmax(&mut p);
                                p.iter().enumerate().map(|(j, p)| j as f32 * p).sum::<f32>()
                                    * s as f32
                            })
                            .collect();
                        let (px, py) = ((i % cols) as f32 * s as f32, (i / cols) as f32 * s as f32);
                        objects.push(object(
                            k as u32,
                            cx.label(k as u32),
                            sc,
                            [px - d[0], py - d[1], px + d[2], py + d[3]],
                        ));
                    }
                }
                Results::Objects(nms(objects, *iou, MAX_DETECTIONS))
            }
            Decoder::SsdAnchors {
                boxes,
                scores,
                strides,
                keypoints,
                iou,
            } => {
                let (b, s) = (get(out, boxes)?, get(out, scores)?);
                let n = s.data().len();
                // MediaPipe's SSD anchors with a fixed anchor size: layers
                // sharing a stride are merged, 2 anchors per layer.
                let mut anchors = vec![];
                let mut i = 0;
                while i < strides.len() {
                    let mut j = i;
                    while j < strides.len() && strides[j] == strides[i] {
                        j += 1;
                    }
                    let per_cell = 2 * (j - i);
                    let st = strides[i].max(1) as f32;
                    let (fw, fh) = (
                        (cx.width / st).ceil() as usize,
                        (cx.height / st).ceil() as usize,
                    );
                    for y in 0..fh {
                        for x in 0..fw {
                            for _ in 0..per_cell {
                                anchors.push((
                                    (x as f32 + 0.5) / fw as f32,
                                    (y as f32 + 0.5) / fh as f32,
                                ));
                            }
                        }
                    }
                    i = j;
                }
                ensure(anchors.len() == n && n > 0, || {
                    format!(
                        "output {scores:?} has {n} scores, but the strides {strides:?} give {} anchors",
                        anchors.len()
                    )
                })?;
                let per = b.data().len() / n;
                ensure(
                    b.data().len() == per * n && per >= 4 + 2 * keypoints,
                    || {
                        format!(
                            "output {boxes:?} has shape {:?}; expected [1, {n}, at least {}]",
                            b.shape(),
                            4 + 2 * keypoints
                        )
                    },
                )?;
                let mut objects = vec![];
                for (i, &(ax, ay)) in anchors.iter().enumerate() {
                    let sc = sigmoid(s.data()[i]);
                    if !reaches(sc, cx.min_confidence) {
                        continue;
                    }
                    let r = &b.data()[i * per..(i + 1) * per];
                    let (bx, by) = (r[0] + ax * cx.width, r[1] + ay * cx.height);
                    let mut o = object(0, cx.label(0), sc, cxcywh(bx, by, r[2], r[3]));
                    o.keypoints = (0..*keypoints)
                        .map(|k| {
                            kp(
                                r[4 + k * 2] + ax * cx.width,
                                r[5 + k * 2] + ay * cx.height,
                                1.0,
                            )
                        })
                        .collect();
                    objects.push(o);
                }
                Results::Objects(nms(objects, *iou, MAX_DETECTIONS))
            }
            Decoder::Yunet {
                strides,
                keypoints,
                iou,
            } => {
                let mut objects = vec![];
                for &s in strides {
                    let names = ["cls", "obj", "bbox", "kps"].map(|p| format!("{p}_{s}"));
                    let [cls, obj, bb, kps] = [0, 1, 2, 3].map(|i| get(out, &names[i]));
                    let (cls, obj, bb, kps) = (cls?, obj?, bb?, kps?);
                    let st = s.max(1) as usize;
                    let (cols, rows) = (cx.width as usize / st, cx.height as usize / st);
                    let n = cols * rows;
                    ensure(
                        cls.data().len() == n
                            && obj.data().len() == n
                            && bb.data().len() == 4 * n
                            && kps.data().len() == 2 * keypoints * n,
                        || {
                            format!(
                                "YuNet outputs for stride {s} don't fit a {}x{} input with {keypoints} keypoints ({n} cells)",
                                cx.width, cx.height
                            )
                        },
                    )?;
                    let sf = s as f32;
                    for i in 0..n {
                        let sc =
                            (cls.data()[i].clamp(0.0, 1.0) * obj.data()[i].clamp(0.0, 1.0)).sqrt();
                        if !reaches(sc, cx.min_confidence) {
                            continue;
                        }
                        let (c, r) = ((i % cols) as f32, (i / cols) as f32);
                        let b = &bb.data()[i * 4..i * 4 + 4];
                        let mut o = object(
                            0,
                            cx.label(0),
                            sc,
                            cxcywh(
                                (c + b[0]) * sf,
                                (r + b[1]) * sf,
                                b[2].exp() * sf,
                                b[3].exp() * sf,
                            ),
                        );
                        let k = &kps.data()[i * keypoints * 2..(i + 1) * keypoints * 2];
                        o.keypoints = (0..*keypoints)
                            .map(|j| kp((c + k[j * 2]) * sf, (r + k[j * 2 + 1]) * sf, 1.0))
                            .collect();
                        objects.push(o);
                    }
                }
                Results::Objects(nms(objects, *iou, MAX_DETECTIONS))
            }
            Decoder::Movenet { output, multi } => {
                let t = get(out, output)?;
                let row = |r: &[f32]| -> Vec<Keypoint> {
                    (0..17)
                        .map(|k| kp(r[k * 3 + 1] * cx.width, r[k * 3] * cx.height, r[k * 3 + 2]))
                        .collect()
                };
                let mut objects = vec![];
                if *multi {
                    let [_, w] = dims::<2>(t, output, "[1, 6, 56]")?;
                    ensure(w == 56, || {
                        format!(
                            "output {output:?} has shape {:?}; expected [1, 6, 56]",
                            t.shape()
                        )
                    })?;
                    for r in t.data().chunks(56) {
                        if !reaches(r[55], cx.min_confidence) {
                            continue;
                        }
                        let bbox = [
                            r[52] * cx.width,
                            r[51] * cx.height,
                            r[54] * cx.width,
                            r[53] * cx.height,
                        ];
                        let mut o = object(0, cx.label_or(0, "person"), r[55], bbox);
                        o.keypoints = row(r);
                        objects.push(o);
                    }
                } else {
                    let shape = dims::<2>(t, output, "[1, 1, 17, 3]")?;
                    ensure(shape == [17, 3], || {
                        format!(
                            "output {output:?} has shape {:?}; expected [1, 1, 17, 3]",
                            t.shape()
                        )
                    })?;
                    let kps = row(t.data());
                    let s = mean_confidence(&kps);
                    let seen: Vec<&Keypoint> = kps.iter().filter(|k| k.confidence > 0.2).collect();
                    if s >= cx.min_confidence && !seen.is_empty() {
                        let bbox = seen.iter().fold(
                            [
                                f32::INFINITY,
                                f32::INFINITY,
                                f32::NEG_INFINITY,
                                f32::NEG_INFINITY,
                            ],
                            |b, k| [b[0].min(k.x), b[1].min(k.y), b[2].max(k.x), b[3].max(k.y)],
                        );
                        let mut o = object(0, cx.label_or(0, "person"), s, bbox);
                        o.keypoints = kps;
                        objects.push(o);
                    }
                }
                Results::Objects(objects)
            }
            Decoder::Rtmo { dets, keypoints } => {
                let (d, k) = (get(out, dets)?, get(out, keypoints)?);
                let [n, five] = dims::<2>(d, dets, "[1, N, 5]")?;
                let [nk, kn, three] = dims::<3>(k, keypoints, "[1, N, K, 3]")?;
                ensure(five == 5 && three == 3 && nk == n, || {
                    format!(
                        "outputs {dets:?} {:?} and {keypoints:?} {:?}; expected [1, N, 5] and [1, N, K, 3]",
                        d.shape(),
                        k.shape()
                    )
                })?;
                let objects = (0..n)
                    .filter(|&i| d.data()[i * 5 + 4] >= cx.min_confidence)
                    .take(MAX_DETECTIONS)
                    .map(|i| {
                        let r = &d.data()[i * 5..i * 5 + 5];
                        let mut o =
                            object(0, cx.label_or(0, "person"), r[4], [r[0], r[1], r[2], r[3]]);
                        o.keypoints = (0..kn)
                            .map(|j| {
                                let at = (i * kn + j) * 3;
                                kp(k.data()[at], k.data()[at + 1], k.data()[at + 2])
                            })
                            .collect();
                        o
                    })
                    .collect();
                Results::Objects(objects)
            }
            Decoder::Simcc { x, y, split } => {
                let (tx, ty) = (get(out, x)?, get(out, y)?);
                let [k, wx] = dims::<2>(tx, x, "[1, K, W x split]")?;
                let [ky, wy] = dims::<2>(ty, y, "[1, K, H x split]")?;
                ensure(k == ky && wx > 0 && wy > 0, || {
                    format!(
                        "outputs {x:?} {:?} and {y:?} {:?} don't have the same keypoints",
                        tx.shape(),
                        ty.shape()
                    )
                })?;
                let kps: Vec<Keypoint> = (0..k)
                    .filter_map(|j| {
                        let (ix, sx) = argmax(&tx.data()[j * wx..(j + 1) * wx])?;
                        let (iy, sy) = argmax(&ty.data()[j * wy..(j + 1) * wy])?;
                        Some(kp(ix as f32 / split, iy as f32 / split, sx.min(sy)))
                    })
                    .collect();
                pose(kps, cx)
            }
            Decoder::Heatmap { output } => {
                let t = get(out, output)?;
                let [k, h, w] = dims::<3>(t, output, "[1, K, H, W]")?;
                ensure(w > 0 && h > 0, || {
                    format!("output {output:?} has an empty heatmap")
                })?;
                let (sx, sy) = (cx.width / w as f32, cx.height / h as f32);
                let kps: Vec<Keypoint> = (0..k)
                    .filter_map(|j| {
                        let m = &t.data()[j * w * h..(j + 1) * w * h];
                        let (i, s) = argmax(m)?;
                        let (px, py) = ((i % w) as i64, (i / w) as i64);
                        let at = |x: i64, y: i64| {
                            if x < 0 || y < 0 || x >= w as i64 || y >= h as i64 {
                                0.0
                            } else {
                                m[y as usize * w + x as usize]
                            }
                        };
                        // A quarter pixel towards the higher neighbour (none
                        // when they're equal: `signum` would say 1 for 0).
                        let sign = |d: f32| {
                            if d > 0.0 {
                                1.0
                            } else if d < 0.0 {
                                -1.0
                            } else {
                                0.0
                            }
                        };
                        let dx = 0.25 * sign(at(px + 1, py) - at(px - 1, py));
                        let dy = 0.25 * sign(at(px, py + 1) - at(px, py - 1));
                        Some(kp(
                            (px as f32 + dx + 0.5) * sx,
                            (py as f32 + dy + 0.5) * sy,
                            s,
                        ))
                    })
                    .collect();
                pose(kps, cx)
            }
            Decoder::Landmarks {
                output,
                points,
                dims: per,
                visibility_index,
                presence,
                presence_sigmoid,
            } => {
                let t = get(out, output)?;
                ensure(t.data().len() >= points * per, || {
                    format!(
                        "output {output:?} has {} values; {points} points x {per} need {}",
                        t.data().len(),
                        points * per
                    )
                })?;
                let kps: Vec<Keypoint> = (0..*points)
                    .map(|k| {
                        let r = &t.data()[k * per..(k + 1) * per];
                        let visible = visibility_index
                            .and_then(|v| r.get(v))
                            .map(|v| sigmoid(*v))
                            .unwrap_or(1.0);
                        kp(r[0], r.get(1).copied().unwrap_or(0.0), visible)
                    })
                    .collect();
                let score = match presence {
                    Some(p) => {
                        let v = get(out, p)?.data().first().copied().unwrap_or(0.0);
                        if *presence_sigmoid { sigmoid(v) } else { v }
                    }
                    None => 1.0,
                };
                if !reaches(score, cx.min_confidence) {
                    return Ok(Results::Objects(vec![]));
                }
                let mut o = object(0, cx.label(0), score, [0.0, 0.0, cx.width, cx.height]);
                o.keypoints = kps;
                Results::Objects(vec![o])
            }
            Decoder::Semantic { output } => {
                let t = get(out, output)?;
                let [c, h, w] = dims::<3>(t, output, "[1, C, H, W]")?;
                ensure(c > 0 && w * h > 0, || {
                    format!("output {output:?} has an empty map")
                })?;
                let mut ids = vec![0f32; w * h];
                let mut counts = vec![0usize; c];
                for (i, id) in ids.iter_mut().enumerate() {
                    let (mut best, mut bk) = (f32::NEG_INFINITY, 0);
                    for k in 0..c {
                        let v = t.data()[k * w * h + i];
                        if v > best {
                            best = v;
                            bk = k;
                        }
                    }
                    *id = bk as f32;
                    counts[bk] += 1;
                }
                let mut top: Vec<(usize, usize)> = counts
                    .iter()
                    .copied()
                    .enumerate()
                    .filter(|x| x.1 > 0)
                    .collect();
                top.sort_by_key(|&(_, n)| std::cmp::Reverse(n));
                let total = (w * h) as f32;
                let summary = top
                    .iter()
                    .take(6)
                    .map(|&(k, n)| {
                        (
                            Class {
                                class_id: k as u32,
                                class_name: cx.label(k as u32),
                                confidence: 1.0,
                            },
                            n as f32 / total,
                        )
                    })
                    .collect();
                Results::Map(Map {
                    width: w,
                    height: h,
                    values: ids,
                    summary: MapSummary::Classes(summary),
                })
            }
            Decoder::DenseMap { output, threshold } => {
                let t = get(out, output)?;
                let [h, w] = dims::<2>(t, output, "[1, 1, H, W] or [1, H, W]")?;
                ensure(w * h > 0, || format!("output {output:?} has an empty map"))?;
                let finite: Vec<f32> = t.data().iter().copied().filter(|v| v.is_finite()).collect();
                let n = finite.len().max(1) as f32;
                let (min, max) = finite
                    .iter()
                    .fold((f32::INFINITY, f32::NEG_INFINITY), |(a, b), &v| {
                        (a.min(v), b.max(v))
                    });
                let mean = finite.iter().sum::<f32>() / n;
                let above_threshold = threshold
                    .map(|th| t.data().iter().filter(|&&v| v > th).count() as f32 / (w * h) as f32);
                Results::Map(Map {
                    width: w,
                    height: h,
                    values: t.data().to_vec(),
                    summary: MapSummary::Values {
                        min,
                        max,
                        mean,
                        above_threshold,
                    },
                })
            }
            Decoder::OcrDet {
                output,
                threshold,
                min_area,
            } => {
                let t = get(out, output)?;
                let [h, w] = dims::<2>(t, output, "[1, 1, H, W]")?;
                let data = t.data();
                let mut seen = vec![false; w * h];
                let mut objects = vec![];
                for start in 0..w * h {
                    if seen[start] || !reaches(data[start], *threshold) {
                        continue;
                    }
                    let (mut stack, mut b, mut n, mut sum) =
                        (vec![start], [usize::MAX, usize::MAX, 0, 0], 0usize, 0f32);
                    seen[start] = true;
                    while let Some(i) = stack.pop() {
                        let (x, y) = (i % w, i / w);
                        b = [b[0].min(x), b[1].min(y), b[2].max(x), b[3].max(y)];
                        n += 1;
                        sum += data[i];
                        let neighbours = [
                            (x > 0).then(|| i - 1),
                            (x + 1 < w).then(|| i + 1),
                            (y > 0).then(|| i - w),
                            (y + 1 < h).then(|| i + w),
                        ];
                        for j in neighbours.into_iter().flatten() {
                            if !seen[j] && data[j] >= *threshold {
                                seen[j] = true;
                                stack.push(j);
                            }
                        }
                    }
                    if n >= *min_area && reaches(sum / n as f32, cx.min_confidence) {
                        // DB shrinks text regions: grow the box back (ratio 1.5).
                        let (bw, bh) = ((b[2] - b[0] + 1) as f32, (b[3] - b[1] + 1) as f32);
                        let grow = (bw * bh * 1.5) / (2.0 * (bw + bh));
                        let bbox = [
                            b[0] as f32 - grow,
                            b[1] as f32 - grow,
                            b[2] as f32 + 1.0 + grow,
                            b[3] as f32 + 1.0 + grow,
                        ];
                        let (sx, sy) = (cx.width / w as f32, cx.height / h as f32);
                        objects.push(object(
                            0,
                            cx.label_or(0, "text"),
                            sum / n as f32,
                            [bbox[0] * sx, bbox[1] * sy, bbox[2] * sx, bbox[3] * sy],
                        ));
                        if objects.len() == MAX_DETECTIONS {
                            break;
                        }
                    }
                }
                Results::Objects(objects)
            }
            Decoder::Ctc { output } => {
                let t = get(out, output)?;
                let [steps, c] = dims::<2>(t, output, "[1, T, C]")?;
                let (mut text, mut last, mut conf, mut n) = (String::new(), 0usize, 0f32, 0);
                for i in 0..steps {
                    let Some((k, p)) = argmax(&t.data()[i * c..(i + 1) * c]) else {
                        break;
                    };
                    if k != 0 && k != last {
                        text.push_str(cx.charset.get(k - 1).map(String::as_str).unwrap_or(" "));
                        conf += p;
                        n += 1;
                    }
                    last = k;
                }
                let confidence = if n > 0 { conf / n as f32 } else { 0.0 };
                let kept = !text.is_empty() && reaches(confidence, cx.min_confidence);
                Results::Text(kept.then_some(Text { text, confidence }))
            }
            Decoder::Embedding { output } => {
                let t = get(out, output)?;
                let norm = t.data().iter().map(|x| x * x).sum::<f32>().sqrt();
                let v = if norm > 0.0 && norm.is_finite() {
                    t.data().iter().map(|x| x / norm).collect()
                } else {
                    vec![0.0; t.data().len()]
                };
                Results::Vector(v)
            }
            Decoder::Raw { outputs } => Results::Tensor(
                outputs
                    .iter()
                    .map(|name| {
                        get(out, name)
                            .map(|t| (name.clone(), t.shape().to_vec(), t.data().to_vec()))
                    })
                    .collect::<Result<_, _>>()?,
            ),
        })
    }
}
