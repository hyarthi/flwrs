//! Pre-processing settings of a model node: how a frame (or a region of it)
//! becomes the model's input tensor. Every resize, crop and rotation is one
//! affine warp from model-input pixels to frame pixels, followed by a
//! per-pixel conversion; the warp's affine maps the decoded results back, so
//! every box and point leaves a model in frame pixels.

use crate::frame::{Affine, Frame, Rect};
use crate::tensor::{Tensor, TensorData};
use serde::{Deserialize, Serialize};

/// How the region fills the model's input.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Resize {
    /// Both sides scaled to fit exactly (the aspect ratio changes).
    Stretch,
    /// Scaled to fit inside, the rest padded with `pad`.
    Letterbox,
    /// Scaled so the shorter side fits, the rest cropped (centre crop).
    Cover,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Color {
    Rgb,
    Bgr,
}

/// The input tensor's element type.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DType {
    F32,
    I32,
    U8,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Layout {
    Nchw,
    Nhwc,
}

/// The pre-processing settings: `value = (pixel × scale − mean) / std` per
/// channel (in the model's channel order), after the warp. `scale`, `mean`
/// and `std` apply to `f32` only; `i32` and `u8` take the pixels as they are.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Pre {
    pub width: u32,
    pub height: u32,
    pub resize: Resize,
    /// The padding value (letterbox, and outside the frame).
    #[serde(default)]
    pub pad: u8,
    pub color: Color,
    pub dtype: DType,
    #[serde(default = "one")]
    pub scale: f32,
    #[serde(default = "zeros")]
    pub mean: [f32; 3],
    #[serde(default = "ones")]
    pub std: [f32; 3],
    pub layout: Layout,
}

fn one() -> f32 {
    1.0
}

fn zeros() -> [f32; 3] {
    [0.0; 3]
}

fn ones() -> [f32; 3] {
    [1.0; 3]
}

/// The largest model input side accepted.
pub const MAX_SIDE: u32 = 8192;

impl Pre {
    /// The settings' own problems.
    pub fn problems(&self) -> Vec<String> {
        let mut problems = vec![];
        for (name, v) in [("width", self.width), ("height", self.height)] {
            if v == 0 || v > MAX_SIDE {
                problems.push(format!("pre.{name} must be between 1 and {MAX_SIDE}"));
            }
        }
        if !self.scale.is_finite() || self.mean.iter().any(|v| !v.is_finite()) {
            problems.push("pre.scale and pre.mean must be finite numbers".into());
        }
        if self.std.iter().any(|v| !v.is_finite() || *v == 0.0) {
            problems.push("pre.std must be finite and not 0".into());
        }
        problems
    }
}

/// A model input cut from a frame, with the affine from its pixels to the
/// frame's.
#[derive(Clone, Debug, PartialEq)]
pub struct Prepared {
    pub tensor: Tensor,
    pub affine: Affine,
}

/// The affine that shows `rect` at `width` x `height` with `resize`.
pub fn plan(rect: Rect, width: u32, height: u32, resize: Resize) -> Affine {
    let (iw, ih) = (width as f32, height as f32);
    let (sx, sy) = match resize {
        Resize::Stretch => (rect.w / iw, rect.h / ih),
        Resize::Letterbox => {
            let s = (rect.w / iw).max(rect.h / ih);
            (s, s)
        }
        Resize::Cover => {
            let s = (rect.w / iw).min(rect.h / ih);
            (s, s)
        }
    };
    let (sin, cos) = rect.rotation.sin_cos();
    let (a, b, d, e) = (sx * cos, -sy * sin, sx * sin, sy * cos);
    Affine {
        a,
        b,
        c: rect.cx - a * iw / 2.0 - b * ih / 2.0,
        d,
        e,
        f: rect.cy - d * iw / 2.0 - e * ih / 2.0,
    }
}

/// Warps `rect` of the frame into the model's input (bilinear, `pad` outside
/// the frame), then converts it. The rectangle must be usable
/// ([`Rect::is_usable`]) and the settings valid ([`Pre::problems`]).
pub fn prepare(frame: &Frame, rect: Rect, pre: &Pre) -> Result<Prepared, String> {
    if !rect.is_usable() {
        return Err(format!("the region {rect:?} has no area"));
    }
    let affine = plan(rect, pre.width, pre.height, pre.resize);
    let rgb = warp(
        frame,
        &affine,
        pre.width as usize,
        pre.height as usize,
        pre.pad as f32,
    );
    Ok(Prepared {
        tensor: convert(&rgb, pre),
        affine,
    })
}

/// One axis of an axis-aligned warp: per output pixel, the two source
/// indices (None outside the frame) and the weight of the second.
fn axis(
    len: usize,
    scale: f32,
    offset: f32,
    size: u32,
) -> Vec<(Option<usize>, Option<usize>, f32)> {
    (0..len)
        .map(|i| {
            let s = scale * (i as f32 + 0.5) + offset - 0.5;
            let i0 = s.floor();
            let t = s - i0;
            let at = |v: f32| (v >= 0.0 && v < size as f32).then_some(v as usize);
            (at(i0), at(i0 + 1.0), t)
        })
        .collect()
}

/// The warped region as interleaved RGB `f32` (0..255).
fn warp(frame: &Frame, affine: &Affine, w: usize, h: usize, pad: f32) -> Vec<f32> {
    let (fw, fh) = (frame.width() as usize, frame.height() as usize);
    let raw = frame.rgb();
    let mut out = vec![0f32; w * h * 3];
    let px = |x: Option<usize>, y: Option<usize>, c: usize| match (x, y) {
        (Some(x), Some(y)) => raw[(y * fw + x) * 3 + c] as f32,
        _ => pad,
    };
    if affine.is_axis_aligned() {
        // Separable: each output column and row samples the same source
        // columns and rows.
        let xs = axis(w, affine.a, affine.c, frame.width());
        let ys = axis(h, affine.e, affine.f, frame.height());
        for (y, &(y0, y1, ty)) in ys.iter().enumerate() {
            for (x, &(x0, x1, tx)) in xs.iter().enumerate() {
                let o = (y * w + x) * 3;
                for c in 0..3 {
                    out[o + c] = px(x0, y0, c) * (1.0 - tx) * (1.0 - ty)
                        + px(x1, y0, c) * tx * (1.0 - ty)
                        + px(x0, y1, c) * (1.0 - tx) * ty
                        + px(x1, y1, c) * tx * ty;
                }
            }
        }
        return out;
    }
    let at = |v: f32, size: usize| (v >= 0.0 && v < size as f32).then_some(v as usize);
    for y in 0..h {
        for x in 0..w {
            let (sx, sy) = affine.apply(x as f32 + 0.5, y as f32 + 0.5);
            let (sx, sy) = (sx - 0.5, sy - 0.5);
            let (fx, fy) = (sx.floor(), sy.floor());
            let (tx, ty) = (sx - fx, sy - fy);
            let (x0, x1) = (at(fx, fw), at(fx + 1.0, fw));
            let (y0, y1) = (at(fy, fh), at(fy + 1.0, fh));
            let o = (y * w + x) * 3;
            for c in 0..3 {
                out[o + c] = px(x0, y0, c) * (1.0 - tx) * (1.0 - ty)
                    + px(x1, y0, c) * tx * (1.0 - ty)
                    + px(x0, y1, c) * (1.0 - tx) * ty
                    + px(x1, y1, c) * tx * ty;
            }
        }
    }
    out
}

/// Interleaved RGB → the model's channel order, layout and type.
fn convert(rgb: &[f32], pre: &Pre) -> Tensor {
    let (w, h) = (pre.width as usize, pre.height as usize);
    let order = match pre.color {
        Color::Rgb => [0, 1, 2],
        Color::Bgr => [2, 1, 0],
    };
    let shape = match pre.layout {
        Layout::Nchw => vec![1, 3, h, w],
        Layout::Nhwc => vec![1, h, w, 3],
    };
    let index = |c: usize, i: usize| match pre.layout {
        Layout::Nchw => c * w * h + i,
        Layout::Nhwc => i * 3 + c,
    };
    fn fill<T: Copy + Default>(n: usize, mut f: impl FnMut(&mut [T])) -> Vec<T> {
        let mut v = vec![T::default(); n];
        f(&mut v);
        v
    }
    let n = w * h * 3;
    let data = match pre.dtype {
        DType::F32 => TensorData::F32(fill(n, |out| {
            for i in 0..w * h {
                for (c, &src) in order.iter().enumerate() {
                    out[index(c, i)] = (rgb[i * 3 + src] * pre.scale - pre.mean[c]) / pre.std[c];
                }
            }
        })),
        DType::I32 => TensorData::I32(fill(n, |out| {
            for i in 0..w * h {
                for (c, &src) in order.iter().enumerate() {
                    out[index(c, i)] = rgb[i * 3 + src].round() as i32;
                }
            }
        })),
        DType::U8 => TensorData::U8(fill(n, |out| {
            for i in 0..w * h {
                for (c, &src) in order.iter().enumerate() {
                    out[index(c, i)] = rgb[i * 3 + src].round().clamp(0.0, 255.0) as u8;
                }
            }
        })),
    };
    Tensor { shape, data }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pre(width: u32, height: u32, resize: Resize) -> Pre {
        Pre {
            width,
            height,
            resize,
            pad: 0,
            color: Color::Rgb,
            dtype: DType::F32,
            scale: 1.0,
            mean: [0.0; 3],
            std: [1.0; 3],
            layout: Layout::Nchw,
        }
    }

    /// A frame whose red channel is x, green y, blue 7.
    fn ramp(w: u32, h: u32) -> Frame {
        let mut rgb = vec![];
        for y in 0..h {
            for x in 0..w {
                rgb.extend([x as u8, y as u8, 7]);
            }
        }
        Frame::new(w, h, rgb).unwrap()
    }

    #[test]
    fn the_same_size_copies_the_pixels() {
        let f = ramp(4, 3);
        let p = prepare(&f, f.full(), &pre(4, 3, Resize::Stretch)).unwrap();
        assert_eq!(p.tensor.shape, [1, 3, 3, 4]);
        let TensorData::F32(d) = &p.tensor.data else {
            panic!()
        };
        assert_eq!(&d[..4], &[0.0, 1.0, 2.0, 3.0]); // red = x
        assert_eq!(&d[12..16], &[0.0, 0.0, 0.0, 0.0]); // green, row 0
        assert!(d[24..].iter().all(|v| *v == 7.0));
        assert_eq!(p.affine, Affine::IDENTITY);
    }

    #[test]
    fn letterbox_pads_and_maps_back() {
        // 8x4 into 4x4: scale 2, two padded rows above and below the content.
        let f = Frame::filled(8, 4, [200, 100, 50]).unwrap();
        let mut settings = pre(4, 4, Resize::Letterbox);
        settings.pad = 114;
        settings.layout = Layout::Nhwc;
        settings.color = Color::Bgr;
        settings.dtype = DType::U8;
        let p = prepare(&f, f.full(), &settings).unwrap();
        assert_eq!(p.tensor.shape, [1, 4, 4, 3]);
        let TensorData::U8(d) = &p.tensor.data else {
            panic!()
        };
        assert_eq!(&d[..3], &[114, 114, 114]); // top row: padding
        assert_eq!(&d[4 * 3 + 3..4 * 3 + 6], &[50, 100, 200]); // row 1: BGR
        assert_eq!(p.affine.apply(0.0, 1.0), (0.0, 0.0));
        assert_eq!(p.affine.apply(4.0, 3.0), (8.0, 4.0));
    }

    #[test]
    fn cover_crops_the_long_side() {
        let f = ramp(8, 4);
        let p = prepare(&f, f.full(), &pre(2, 2, Resize::Cover)).unwrap();
        // The centre 4x4, halved: x from 2 to 6.
        assert_eq!(p.affine.apply(0.0, 0.0), (2.0, 0.0));
        assert_eq!(p.affine.apply(2.0, 2.0), (6.0, 4.0));
    }

    #[test]
    fn a_rotated_region_samples_along_its_axes() {
        let f = ramp(9, 9);
        // A 3x3 region around (4, 4), a quarter turn: the model's x runs down the frame.
        let rect = Rect {
            cx: 4.5,
            cy: 4.5,
            w: 3.0,
            h: 3.0,
            rotation: std::f32::consts::FRAC_PI_2,
        };
        let p = prepare(&f, rect, &pre(3, 3, Resize::Stretch)).unwrap();
        let TensorData::F32(d) = &p.tensor.data else {
            panic!()
        };
        let green_row0: Vec<f32> = d[9..12].iter().map(|v| v.round()).collect();
        assert_eq!(green_row0, [3.0, 4.0, 5.0]);
        let (x, y) = p.affine.apply(1.5, 1.5);
        assert!((x - 4.5).abs() < 1e-4 && (y - 4.5).abs() < 1e-4);
    }

    #[test]
    fn values_are_scaled_and_normalised() {
        let f = Frame::filled(2, 2, [255, 0, 51]).unwrap();
        let mut settings = pre(2, 2, Resize::Stretch);
        settings.scale = 1.0 / 255.0;
        settings.mean = [0.5, 0.5, 0.5];
        settings.std = [0.5, 0.5, 0.5];
        let p = prepare(&f, f.full(), &settings).unwrap();
        let TensorData::F32(d) = &p.tensor.data else {
            panic!()
        };
        assert!(
            (d[0] - 1.0).abs() < 1e-6 && (d[4] + 1.0).abs() < 1e-6 && (d[8] + 0.6).abs() < 1e-6
        );
        let mut ints = pre(2, 2, Resize::Stretch);
        ints.dtype = DType::I32;
        let p = prepare(&f, f.full(), &ints).unwrap();
        assert_eq!(
            p.tensor.data,
            TensorData::I32(vec![255, 255, 255, 255, 0, 0, 0, 0, 51, 51, 51, 51])
        );
    }

    #[test]
    fn regions_outside_the_frame_are_padding() {
        let f = Frame::filled(4, 4, [9, 9, 9]).unwrap();
        let mut settings = pre(2, 2, Resize::Stretch);
        settings.pad = 1;
        let p = prepare(&f, Rect::of_box([100.0, 100.0, 104.0, 104.0]), &settings).unwrap();
        let TensorData::F32(d) = &p.tensor.data else {
            panic!()
        };
        assert!(d.iter().all(|v| *v == 1.0));
        assert!(prepare(&f, Rect::of_box([1.0, 1.0, 1.0, 3.0]), &settings).is_err());
    }
}
