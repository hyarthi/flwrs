//! A frame with what's known about it, drawn on a pixel buffer of any size:
//! the frame fitted inside, the overlay in display pixels so labels stay
//! readable at any camera resolution.

use super::canvas::{Canvas, mix, rgb, text_size};
use super::facts::{FrameFacts, MapLayer, ObjectFacts, RuleShape};
use crate::frame::Frame;
use crate::results::MapSummary;
use std::sync::Arc;

/// What the overlay shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct View {
    pub boxes: bool,
    pub labels: bool,
    pub keypoints: bool,
    pub map: bool,
    pub panel: bool,
    /// The keyboard guide (closed: a hint saying how to open it).
    pub guide: bool,
    /// Flip the display left to right, like a mirror. Only the display: the
    /// models always get the true image, so left and right stay right.
    pub mirror: bool,
    /// The rules' lines and zones.
    pub rules: bool,
}

impl Default for View {
    fn default() -> Self {
        Self {
            boxes: true,
            labels: true,
            keypoints: true,
            map: false,
            panel: true,
            guide: false,
            mirror: false,
            rules: true,
        }
    }
}

/// A frame ready to show.
#[derive(Clone, Debug)]
pub struct Scene {
    pub frame: Frame,
    pub seq: u64,
    pub facts: FrameFacts,
    /// fps, per-node times, devices.
    pub status: Vec<String>,
    /// Seconds into a recording, while one runs.
    pub recording: Option<f32>,
    /// The latest events, oldest first.
    pub recent: Vec<String>,
    /// The rules' lines and zones.
    pub shapes: Arc<Vec<RuleShape>>,
}

const PALETTE: [u32; 12] = [
    rgb(255, 87, 87),
    rgb(64, 196, 255),
    rgb(105, 240, 124),
    rgb(255, 214, 0),
    rgb(224, 124, 255),
    rgb(255, 145, 0),
    rgb(29, 233, 182),
    rgb(255, 128, 171),
    rgb(140, 158, 255),
    rgb(198, 255, 0),
    rgb(0, 229, 255),
    rgb(255, 171, 145),
];
const INK: u32 = rgb(240, 240, 240);
const DARK: u32 = rgb(16, 16, 16);
const ERROR: u32 = rgb(255, 110, 110);
const GUIDE_TITLE: u32 = rgb(255, 214, 0);
const EVENT: u32 = rgb(120, 230, 160);
const RECORDING: u32 = rgb(240, 40, 40);
const SHAPE: u32 = rgb(255, 255, 255);
/// Keypoints below this aren't drawn.
const MIN_KEYPOINT: f32 = 0.3;
/// Of 256.
const MAP_ALPHA: u32 = 115;
const LABEL_ALPHA: u32 = 190;

/// A stable colour for a key (FNV-1a).
pub fn colour_of(key: &str) -> u32 {
    let hash = key.bytes().fold(0x811c_9dc5u32, |h, b| {
        (h ^ b as u32).wrapping_mul(0x0100_0193)
    });
    PALETTE[hash as usize % PALETTE.len()]
}

/// Where the frame sits in the buffer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Fit {
    pub scale: f32,
    pub x: f32,
    pub y: f32,
    /// Set to the frame's width when the display is mirrored.
    pub mirror_width: Option<f32>,
}

impl Fit {
    pub fn of(fw: u32, fh: u32, w: usize, h: usize, mirror: bool) -> Self {
        let scale = (w as f32 / fw as f32).min(h as f32 / fh as f32);
        Self {
            scale,
            x: ((w as f32 - fw as f32 * scale) / 2.0).floor(),
            y: ((h as f32 - fh as f32 * scale) / 2.0).floor(),
            mirror_width: mirror.then_some(fw as f32),
        }
    }

    /// A frame x as shown, and a shown x as a frame x (the same flip).
    fn flip(&self, x: f32) -> f32 {
        self.mirror_width.map_or(x, |w| w - x)
    }

    fn point(&self, x: f32, y: f32) -> (i32, i32) {
        let x = self.flip(x);
        (
            (x * self.scale + self.x).round() as i32,
            (y * self.scale + self.y).round() as i32,
        )
    }
}

fn blit(c: &mut Canvas, frame: &Frame, fit: Fit) {
    let (w, h) = (c.width(), c.height());
    let (fw, fh) = (frame.width() as usize, frame.height() as usize);
    let rgb_data = frame.rgb();
    let source = |pos: usize, offset: f32, len: usize| -> Option<usize> {
        let v = (pos as f32 + 0.5 - offset) / fit.scale;
        (v >= 0.0 && (v as usize) < len).then_some(v as usize)
    };
    let columns: Vec<Option<usize>> = (0..w)
        .map(|x| {
            let column = source(x, fit.x, fw)?;
            Some(if fit.mirror_width.is_some() {
                fw - 1 - column
            } else {
                column
            })
        })
        .collect();
    let pixels = c.pixels_mut();
    for y in 0..h {
        let row = &mut pixels[y * w..(y + 1) * w];
        match source(y, fit.y, fh) {
            None => row.fill(0),
            Some(fy) => {
                for (out, col) in row.iter_mut().zip(&columns) {
                    *out = col.map_or(0, |fx| {
                        let p = &rgb_data[(fy * fw + fx) * 3..(fy * fw + fx) * 3 + 3];
                        rgb(p[0], p[1], p[2])
                    });
                }
            }
        }
    }
}

fn value_colour(t: f32) -> u32 {
    // Dark blue → teal → yellow.
    let t = t.clamp(0.0, 1.0);
    let lerp = |a: f32, b: f32, t: f32| (a + (b - a) * t) as u8;
    if t < 0.5 {
        let s = t * 2.0;
        rgb(
            lerp(40.0, 30.0, s),
            lerp(20.0, 160.0, s),
            lerp(120.0, 140.0, s),
        )
    } else {
        let s = (t - 0.5) * 2.0;
        rgb(
            lerp(30.0, 250.0, s),
            lerp(160.0, 230.0, s),
            lerp(140.0, 30.0, s),
        )
    }
}

/// Lays a map over the frame, through the inverse of its model → frame affine.
fn overlay_map(c: &mut Canvas, layer: &MapLayer, frame: &Frame, fit: Fit) {
    let (ox, oy) = layer.affine.apply(0.0, 0.0);
    let (ux, uy) = layer.affine.apply(1.0, 0.0);
    let (vx, vy) = layer.affine.apply(0.0, 1.0);
    let (a, b, d, e) = (ux - ox, vx - ox, uy - oy, vy - oy);
    let det = a * e - b * d;
    if det.abs() < 1e-9 {
        return;
    }
    let map = &layer.map;
    let by_class = matches!(map.summary, MapSummary::Classes(_));
    let (low, span) = match map.summary {
        MapSummary::Values { min, max, .. } if max > min => (min, max - min),
        _ => (0.0, 1.0),
    };
    let (w, h) = (c.width(), c.height());
    let (fw, fh) = (frame.width() as f32, frame.height() as f32);
    let pixels = c.pixels_mut();
    for sy in 0..h {
        for sx in 0..w {
            let fx = fit.flip((sx as f32 + 0.5 - fit.x) / fit.scale);
            let fy = (sy as f32 + 0.5 - fit.y) / fit.scale;
            if fx < 0.0 || fy < 0.0 || fx >= fw || fy >= fh {
                continue;
            }
            let (px, py) = (fx - ox, fy - oy);
            let mx = (e * px - b * py) / det;
            let my = (a * py - d * px) / det;
            if mx < 0.0 || my < 0.0 || mx >= map.width as f32 || my >= map.height as f32 {
                continue;
            }
            let Some(&value) = map.values.get(my as usize * map.width + mx as usize) else {
                continue;
            };
            let colour = if by_class {
                PALETTE[value.max(0.0) as usize % PALETTE.len()]
            } else {
                value_colour((value - low) / span)
            };
            let i = sy * w + sx;
            pixels[i] = mix(pixels[i], colour, MAP_ALPHA);
        }
    }
}

fn draw_object(c: &mut Canvas, o: &ObjectFacts, view: &View, fit: Fit, scale: i32) {
    let colour = colour_of(&o.colour_key);
    let (ax, y0) = fit.point(o.bbox[0], o.bbox[1]);
    let (bx, y1) = fit.point(o.bbox[2], o.bbox[3]);
    // Mirrored, the box's left edge is its right one.
    let (x0, x1) = (ax.min(bx), ax.max(bx));
    if view.boxes {
        c.outline(x0, y0, x1, y1, colour, scale.max(2));
    }
    if view.keypoints {
        let visible = |i: usize| o.keypoints.get(i).filter(|k| k.confidence >= MIN_KEYPOINT);
        for &(a, b) in &o.skeleton {
            if let (Some(p), Some(q)) = (visible(a), visible(b)) {
                c.line(fit.point(p.x, p.y), fit.point(q.x, q.y), colour, scale);
            }
        }
        for k in o.keypoints.iter().filter(|k| k.confidence >= MIN_KEYPOINT) {
            let (x, y) = fit.point(k.x, k.y);
            c.dot(x, y, scale + 1, colour);
        }
    }
    if view.labels {
        let mut lines = vec![format!("{} {}", o.id, o.label)];
        lines.extend(o.lines.iter().cloned());
        let (w, h) = text_size(&lines, scale);
        // Above the box where there's room, else inside it.
        let top = if y0 - h >= 0 { y0 - h } else { y0.max(0) };
        let left = x0.min(c.width() as i32 - w).max(0);
        c.fill(left, top, w, h, DARK, LABEL_ALPHA);
        for (n, line) in lines.iter().enumerate() {
            let ink = if n == 0 { colour } else { INK };
            c.text(
                left + 2 * scale,
                top + (1 + 10 * n as i32) * scale,
                line,
                ink,
                scale,
            );
        }
    }
    for child in &o.children {
        draw_object(c, child, view, fit, scale);
    }
}

fn draw_shapes(c: &mut Canvas, scene: &Scene, fit: Fit, scale: i32) {
    let (fw, fh) = (scene.frame.width() as f32, scene.frame.height() as f32);
    for shape in scene.shapes.iter() {
        let points: Vec<(i32, i32)> = shape
            .points
            .iter()
            .map(|p| fit.point(p[0] * fw, p[1] * fh))
            .collect();
        for pair in points.windows(2) {
            c.line(pair[0], pair[1], SHAPE, scale);
        }
        if shape.closed
            && let (Some(first), Some(last)) = (points.first(), points.last())
        {
            c.line(*last, *first, SHAPE, scale);
        }
        if let Some(&(x, y)) = points.first() {
            let lines = [shape.name.clone()];
            let (w, h) = text_size(&lines, scale);
            let left = x.min(c.width() as i32 - w).max(0);
            let top = y.min(c.height() as i32 - h).max(0);
            c.fill(left, top, w, h, DARK, LABEL_ALPHA);
            c.text(left + 2 * scale, top + scale, &shape.name, SHAPE, scale);
        }
    }
}

fn draw_panel(c: &mut Canvas, scene: &Scene, scale: i32) {
    let f = &scene.facts;
    let mut lines = scene.status.clone();
    lines.extend(f.notes.iter().cloned());
    let notes = lines.len();
    lines.extend(f.errors.iter().map(|e| format!("error: {e}")));
    let errors = lines.len();
    lines.extend(scene.recent.iter().cloned());
    if lines.is_empty() {
        return;
    }
    let (w, h) = text_size(&lines, scale);
    c.fill(0, 0, w, h, DARK, LABEL_ALPHA);
    for (n, line) in lines.iter().enumerate() {
        let ink = if n < notes {
            INK
        } else if n < errors {
            ERROR
        } else {
            EVENT
        };
        c.text(2 * scale, (1 + 10 * n as i32) * scale, line, ink, scale);
    }
}

/// The guide's lines: each key, what it does, and a toggle's state.
fn guide_lines(view: &View, has_map: bool, recording: bool) -> Vec<String> {
    let recording = if recording { "on" } else { "off" };
    let state = |on: bool| if on { "on" } else { "off" };
    let rows = [
        ("space", "pause / resume", ""),
        ("n", "step one frame", ""),
        ("b", "boxes", state(view.boxes)),
        ("l", "labels", state(view.labels)),
        ("k", "keypoints", state(view.keypoints)),
        if has_map {
            ("m", "map overlay", state(view.map))
        } else {
            ("m", "no map in this graph", "")
        },
        ("p", "status panel", state(view.panel)),
        ("f", "mirror display", state(view.mirror)),
        ("z", "rule lines, zones", state(view.rules)),
        ("r", "record video", recording),
        ("h", "hide this guide", ""),
        ("q", "quit", ""),
    ];
    let mut lines = vec!["Keys".to_string()];
    lines.extend(
        rows.iter()
            .map(|(key, what, on)| format!("{key:<6}{what:<16}{on:>3}").trim_end().to_string()),
    );
    lines
}

/// The guide, or the hint that opens it, in the bottom-right corner.
fn draw_guide(c: &mut Canvas, view: &View, has_map: bool, recording: bool, scale: i32) {
    let lines = if view.guide {
        guide_lines(view, has_map, recording)
    } else {
        vec!["h: keys".to_string()]
    };
    let (w, h) = text_size(&lines, scale);
    let margin = 4 * scale;
    let left = (c.width() as i32 - w - margin).max(0);
    let top = (c.height() as i32 - h - margin).max(0);
    c.fill(left, top, w, h, DARK, LABEL_ALPHA);
    for (n, line) in lines.iter().enumerate() {
        let ink = if view.guide && n == 0 {
            GUIDE_TITLE
        } else {
            INK
        };
        c.text(
            left + 2 * scale,
            top + (1 + 10 * n as i32) * scale,
            line,
            ink,
            scale,
        );
    }
}

/// A red dot and the time, top right.
fn draw_recording(c: &mut Canvas, seconds: f32, scale: i32) {
    let whole = seconds as u32;
    let lines = [format!("  REC {}:{:02}", whole / 60, whole % 60)];
    let (w, h) = text_size(&lines, scale);
    let left = (c.width() as i32 - w - 4 * scale).max(0);
    let top = 4 * scale;
    c.fill(left, top, w, h, DARK, LABEL_ALPHA);
    c.dot(left + 6 * scale, top + 6 * scale, 3 * scale, RECORDING);
    c.text(left + 2 * scale, top + scale, &lines[0], INK, scale);
}

/// The frame and what's known about it, without the debug window's own marks
/// (the keyboard hint, the recording light). `scale`: pixels per font pixel.
pub fn annotate(scene: &Scene, view: &View, pixels: &mut [u32], w: usize, h: usize, scale: i32) {
    if w == 0 || h == 0 {
        return;
    }
    let mut c = Canvas::new(pixels, w, h);
    let fit = Fit::of(scene.frame.width(), scene.frame.height(), w, h, view.mirror);
    blit(&mut c, &scene.frame, fit);
    if view.map {
        for layer in &scene.facts.maps {
            overlay_map(&mut c, layer, &scene.frame, fit);
        }
    }
    if view.rules {
        draw_shapes(&mut c, scene, fit, scale);
    }
    for o in &scene.facts.objects {
        draw_object(&mut c, o, view, fit, scale);
    }
    if view.panel {
        draw_panel(&mut c, scene, scale);
    }
}

/// Draws a scene on a `w` x `h` buffer, as the debug window shows it;
/// `scale`: pixels per font pixel.
pub fn compose(scene: &Scene, view: &View, pixels: &mut [u32], w: usize, h: usize, scale: i32) {
    if w == 0 || h == 0 {
        return;
    }
    annotate(scene, view, pixels, w, h, scale);
    let mut c = Canvas::new(pixels, w, h);
    draw_guide(
        &mut c,
        view,
        !scene.facts.maps.is_empty(),
        scene.recording.is_some(),
        scale,
    );
    if let Some(seconds) = scene.recording {
        draw_recording(&mut c, seconds, scale);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::Affine;
    use crate::results::Map;

    fn object() -> ObjectFacts {
        ObjectFacts {
            id: "det#0".into(),
            label: "person 0.93".into(),
            class: "person".into(),
            colour_key: "person".into(),
            bbox: [20.0, 40.0, 60.0, 80.0],
            lines: vec![],
            keypoints: vec![],
            skeleton: vec![],
            children: vec![],
        }
    }

    fn scene(facts: FrameFacts) -> Scene {
        Scene {
            frame: Frame::filled(100, 100, [10, 20, 30]).unwrap(),
            seq: 0,
            facts,
            status: vec![],
            recording: None,
            recent: vec![],
            shapes: Arc::new(vec![]),
        }
    }

    fn draw(s: &Scene, view: View, w: usize, h: usize) -> Vec<u32> {
        let mut p = vec![0; w * h];
        compose(s, &view, &mut p, w, h, 1);
        p
    }

    const BOXES: View = View {
        boxes: true,
        labels: false,
        keypoints: false,
        map: false,
        panel: false,
        guide: false,
        mirror: false,
        rules: false,
    };

    #[test]
    fn the_frame_is_fitted_with_bars() {
        let s = scene(FrameFacts::default());
        let p = draw(&s, BOXES, 200, 100);
        assert_eq!(p[0], 0, "left bar");
        assert_eq!(p[49], 0);
        assert_eq!(p[50], rgb(10, 20, 30));
        assert_eq!(p[149], rgb(10, 20, 30));
        assert_eq!(p[150], 0, "right bar");
    }

    #[test]
    fn a_box_is_an_outline_at_its_scaled_place() {
        let s = scene(FrameFacts {
            objects: vec![object()],
            ..FrameFacts::default()
        });
        let p = draw(&s, BOXES, 200, 200);
        let colour = colour_of("person");
        let at = |x: usize, y: usize| p[y * 200 + x];
        assert_eq!(at(40, 80), colour);
        assert_eq!(at(80, 80), colour, "top edge");
        assert_eq!(at(40, 120), colour, "left edge");
        assert_eq!(at(80, 120), rgb(10, 20, 30), "no fill");
        assert_eq!(at(39, 80), rgb(10, 20, 30), "outside");
    }

    #[test]
    fn a_label_sits_above_its_box_or_inside_at_the_top() {
        let mut o = object();
        let s = scene(FrameFacts {
            objects: vec![o.clone()],
            ..FrameFacts::default()
        });
        let view = View {
            boxes: false,
            labels: true,
            ..BOXES
        };
        let p = draw(&s, view, 200, 200);
        assert_ne!(p[75 * 200 + 40], rgb(10, 20, 30), "above the box");
        o.bbox = [20.0, 0.0, 60.0, 40.0];
        let s = scene(FrameFacts {
            objects: vec![o],
            ..FrameFacts::default()
        });
        let p = draw(&s, view, 200, 200);
        assert_ne!(p[2 * 200 + 40], rgb(10, 20, 30), "inside, at the top");
    }

    #[test]
    fn low_confidence_keypoints_are_left_out() {
        use crate::results::Keypoint;
        let mut o = object();
        o.keypoints = vec![
            Keypoint {
                x: 30.0,
                y: 50.0,
                confidence: 0.9,
            },
            Keypoint {
                x: 50.0,
                y: 70.0,
                confidence: 0.1,
            },
        ];
        o.skeleton = vec![(0, 1)];
        let s = scene(FrameFacts {
            objects: vec![o],
            ..FrameFacts::default()
        });
        let view = View {
            boxes: false,
            keypoints: true,
            ..BOXES
        };
        let p = draw(&s, view, 100, 100);
        assert_eq!(p[50 * 100 + 30], colour_of("person"));
        assert_eq!(p[70 * 100 + 50], rgb(10, 20, 30));
        assert_eq!(
            p[60 * 100 + 40],
            rgb(10, 20, 30),
            "no bone to a hidden point"
        );
    }

    #[test]
    fn a_map_is_placed_by_its_affine_and_only_when_on() {
        let map = Map {
            width: 2,
            height: 1,
            values: vec![0.0, 1.0],
            summary: MapSummary::Values {
                min: 0.0,
                max: 1.0,
                mean: 0.5,
                above_threshold: None,
            },
        };
        // The model's two pixels cover the frame's right half, side by side.
        let affine = Affine {
            a: 25.0,
            b: 0.0,
            c: 50.0,
            d: 0.0,
            e: 100.0,
            f: 0.0,
        };
        let s = scene(FrameFacts {
            maps: vec![MapLayer {
                node: "m".into(),
                map,
                affine,
            }],
            ..FrameFacts::default()
        });
        let base = rgb(10, 20, 30);
        let off = draw(&s, BOXES, 100, 100);
        assert_eq!(off[50 * 100 + 60], base);
        let p = draw(&s, View { map: true, ..BOXES }, 100, 100);
        assert_eq!(p[50 * 100 + 10], base, "left half untouched");
        assert_ne!(p[50 * 100 + 60], base);
        assert_ne!(
            p[50 * 100 + 60],
            p[50 * 100 + 90],
            "two values, two colours"
        );
    }

    #[test]
    fn the_panel_shows_status_and_errors() {
        let mut s = scene(FrameFacts {
            errors: vec!["pose failed".into()],
            ..FrameFacts::default()
        });
        s.status = vec!["30.0 fps".into()];
        let view = View {
            panel: true,
            ..BOXES
        };
        let p = draw(&s, view, 200, 100);
        assert!(p[..200 * 10].contains(&INK));
        assert!(p[200 * 10..200 * 22].contains(&ERROR));
    }

    #[test]
    fn the_hint_opens_into_a_guide_in_the_corner() {
        let s = scene(FrameFacts::default());
        let (w, h) = (300, 200);
        let inked = |p: &[u32], y0: usize, y1: usize| {
            (y0..y1).any(|y| p[y * w + w / 2..(y + 1) * w].contains(&INK))
        };
        let closed = draw(&s, BOXES, w, h);
        assert!(inked(&closed, h - 16, h), "the hint is at the bottom");
        assert!(!inked(&closed, h - 60, h - 20), "and nothing above it");
        let open = draw(
            &s,
            View {
                guide: true,
                ..BOXES
            },
            w,
            h,
        );
        assert!(inked(&open, h - 60, h - 20), "the guide is taller");
        assert!(
            (0..h - 140).all(|y| open[y * w + w - 60] == rgb(10, 20, 30)),
            "only in its corner"
        );
    }

    #[test]
    fn the_guide_says_what_is_on() {
        let lines = guide_lines(
            &View {
                map: true,
                boxes: false,
                ..View::default()
            },
            true,
            true,
        );
        let row = |k: &str| lines.iter().find(|l| l.starts_with(k)).unwrap().clone();
        assert!(row("b ").ends_with("off"), "{lines:?}");
        assert!(row("m ").ends_with("on"), "{lines:?}");
        assert!(row("h ").contains("hide"), "{lines:?}");
        assert!(lines.iter().all(|l| l.is_ascii()));
    }

    #[test]
    fn the_guide_says_when_the_graph_has_no_map() {
        let lines = guide_lines(&View::default(), false, false);
        assert!(
            lines.iter().any(|l| l.contains("no map in this graph")),
            "{lines:?}"
        );
        assert!(
            !lines.iter().any(|l| l.contains("map overlay")),
            "{lines:?}"
        );
        let lines = guide_lines(&View::default(), true, false);
        assert!(
            lines
                .iter()
                .any(|l| l.contains("map overlay") && l.ends_with("off")),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("f ") && l.ends_with("off")),
            "{lines:?}"
        );
    }

    #[test]
    fn mirroring_flips_the_picture_and_the_overlay_together() {
        let (fw, fh) = (100usize, 100usize);
        let mut rgb_data = vec![10u8; fw * fh * 3];
        for y in 0..fh {
            for x in 0..10 {
                rgb_data[(y * fw + x) * 3] = 250;
            }
        }
        let mut o = object();
        o.bbox = [10.0, 40.0, 30.0, 60.0];
        let mut s = scene(FrameFacts {
            objects: vec![o],
            ..FrameFacts::default()
        });
        s.frame = Frame::new(100, 100, rgb_data).unwrap();
        let colour = colour_of("person");
        let plain = draw(&s, BOXES, fw, fh);
        assert_eq!(
            plain[50 * fw + 2] >> 16,
            250,
            "the bright strip on the left"
        );
        assert_eq!(plain[50 * fw + 10], colour, "the box's left edge");
        let flipped = draw(
            &s,
            View {
                mirror: true,
                ..BOXES
            },
            fw,
            fh,
        );
        assert_eq!(flipped[50 * fw + 97] >> 16, 250, "now on the right");
        assert_eq!(flipped[50 * fw + 2] >> 16, 10);
        assert_eq!(flipped[50 * fw + 90], colour, "the box followed it");
        assert_eq!(flipped[50 * fw + 70], colour);
        assert_ne!(flipped[50 * fw + 10], colour);
    }

    #[test]
    fn a_mirrored_map_follows_the_picture() {
        let map = Map {
            width: 2,
            height: 1,
            values: vec![0.0, 1.0],
            summary: MapSummary::Values {
                min: 0.0,
                max: 1.0,
                mean: 0.5,
                above_threshold: None,
            },
        };
        // The model's two pixels cover the frame's right half.
        let affine = Affine {
            a: 25.0,
            b: 0.0,
            c: 50.0,
            d: 0.0,
            e: 100.0,
            f: 0.0,
        };
        let s = scene(FrameFacts {
            maps: vec![MapLayer {
                node: "m".into(),
                map,
                affine,
            }],
            ..FrameFacts::default()
        });
        let base = rgb(10, 20, 30);
        let p = draw(
            &s,
            View {
                map: true,
                mirror: true,
                ..BOXES
            },
            100,
            100,
        );
        assert_ne!(p[50 * 100 + 30], base, "now on the left");
        assert_eq!(p[50 * 100 + 70], base);
    }

    #[test]
    fn a_tiny_window_doesnt_break_the_guide() {
        let s = scene(FrameFacts::default());
        draw(
            &s,
            View {
                guide: true,
                ..BOXES
            },
            8,
            6,
        );
    }

    #[test]
    fn rule_shapes_and_recent_events_are_drawn() {
        let mut s = scene(FrameFacts::default());
        s.shapes = Arc::new(vec![RuleShape {
            name: "x".into(),
            points: vec![[0.5, 0.0], [0.5, 1.0]],
            closed: false,
        }]);
        s.recent = vec!["1.0s door".into()];
        let off = draw(&s, BOXES, 100, 100);
        assert_eq!(off[80 * 100 + 50], rgb(10, 20, 30));
        let on = draw(
            &s,
            View {
                rules: true,
                ..BOXES
            },
            100,
            100,
        );
        assert_eq!(on[60 * 100 + 50], SHAPE);
        let panel = draw(
            &s,
            View {
                panel: true,
                ..BOXES
            },
            100,
            100,
        );
        assert!(panel[..100 * 12].contains(&EVENT));
    }

    #[test]
    fn a_recording_shows_in_the_corner_and_the_guide() {
        let mut s = scene(FrameFacts::default());
        assert!(!draw(&s, BOXES, 200, 100).contains(&RECORDING));
        s.recording = Some(75.0);
        let p = draw(&s, BOXES, 200, 100);
        assert!(p[..200 * 20].contains(&RECORDING));
        let lines = guide_lines(&View::default(), false, true);
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("r ") && l.ends_with("on")),
            "{lines:?}"
        );
    }

    #[test]
    fn colours_are_stable_per_key() {
        assert_eq!(colour_of("person"), colour_of("person"));
        assert!(PALETTE.contains(&colour_of("anything")));
    }
}
