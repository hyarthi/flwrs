//! Frames, the regions of them a model looks at, and the affine that maps a
//! model's input pixels back to frame pixels.

use std::sync::Arc;

/// An RGB frame, 8 bits per channel, rows without padding. Cheap to clone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    width: u32,
    height: u32,
    rgb: Arc<[u8]>,
}

/// The largest side of a frame.
pub const MAX_FRAME_SIDE: u32 = 16_384;

impl Frame {
    /// A frame from packed RGB bytes (`width * height * 3`).
    pub fn new(width: u32, height: u32, rgb: impl Into<Arc<[u8]>>) -> Result<Self, String> {
        let rgb = rgb.into();
        if width == 0 || height == 0 || width > MAX_FRAME_SIDE || height > MAX_FRAME_SIDE {
            return Err(format!(
                "a frame of {width}x{height}: each side must be 1 to {MAX_FRAME_SIDE}"
            ));
        }
        let expected = width as usize * height as usize * 3;
        if rgb.len() != expected {
            return Err(format!(
                "a {width}x{height} RGB frame has {expected} bytes, not {}",
                rgb.len()
            ));
        }
        Ok(Self { width, height, rgb })
    }

    /// A frame of one colour.
    pub fn filled(width: u32, height: u32, rgb: [u8; 3]) -> Result<Self, String> {
        let n = width as usize * height as usize;
        Self::new(width, height, rgb.repeat(n))
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// The packed RGB bytes.
    pub fn rgb(&self) -> &[u8] {
        &self.rgb
    }

    /// The whole frame as a region.
    pub fn full(&self) -> Rect {
        Rect::of_box([0.0, 0.0, self.width as f32, self.height as f32])
    }
}

/// A (possibly rotated) rectangle of the frame: centre, size, rotation
/// (radians, clockwise in image coordinates).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub cx: f32,
    pub cy: f32,
    pub w: f32,
    pub h: f32,
    pub rotation: f32,
}

impl Rect {
    /// An axis-aligned box `[x1, y1, x2, y2]`.
    pub fn of_box(b: [f32; 4]) -> Self {
        Self {
            cx: (b[0] + b[2]) / 2.0,
            cy: (b[1] + b[3]) / 2.0,
            w: b[2] - b[0],
            h: b[3] - b[1],
            rotation: 0.0,
        }
    }

    /// Whether a model input can be cut from it: finite, with a positive size.
    pub fn is_usable(&self) -> bool {
        [self.cx, self.cy, self.w, self.h, self.rotation]
            .iter()
            .all(|v| v.is_finite())
            && self.w > 0.0
            && self.h > 0.0
    }
}

/// Maps a model-input point to a frame point: `x' = a·x + b·y + c`,
/// `y' = d·x + e·y + f`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Affine {
    pub a: f32,
    pub b: f32,
    pub c: f32,
    pub d: f32,
    pub e: f32,
    pub f: f32,
}

impl Affine {
    pub const IDENTITY: Affine = Affine {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 0.0,
        e: 1.0,
        f: 0.0,
    };

    pub fn apply(&self, x: f32, y: f32) -> (f32, f32) {
        (
            self.a * x + self.b * y + self.c,
            self.d * x + self.e * y + self.f,
        )
    }

    /// Whether it only scales and shifts (no rotation or shear).
    pub fn is_axis_aligned(&self) -> bool {
        self.b == 0.0 && self.d == 0.0
    }

    /// The axis-aligned box around a model-input box once mapped (a rotated
    /// region's box grows to hold it).
    pub fn map_box(&self, b: [f32; 4]) -> [f32; 4] {
        let corners =
            [(b[0], b[1]), (b[2], b[1]), (b[0], b[3]), (b[2], b[3])].map(|(x, y)| self.apply(x, y));
        let xs = corners.map(|c| c.0);
        let ys = corners.map(|c| c.1);
        [
            xs.into_iter().fold(f32::INFINITY, f32::min),
            ys.into_iter().fold(f32::INFINITY, f32::min),
            xs.into_iter().fold(f32::NEG_INFINITY, f32::max),
            ys.into_iter().fold(f32::NEG_INFINITY, f32::max),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_check_their_size() {
        assert!(Frame::new(2, 2, vec![0u8; 12]).is_ok());
        assert!(
            Frame::new(2, 2, vec![0u8; 11])
                .unwrap_err()
                .contains("12 bytes")
        );
        assert!(Frame::new(0, 2, vec![]).is_err());
        let f = Frame::filled(3, 2, [1, 2, 3]).unwrap();
        assert_eq!(&f.rgb()[..6], &[1, 2, 3, 1, 2, 3]);
        assert_eq!(
            f.full(),
            Rect {
                cx: 1.5,
                cy: 1.0,
                w: 3.0,
                h: 2.0,
                rotation: 0.0
            }
        );
    }

    #[test]
    fn boxes_map_through_a_rotation() {
        // A quarter turn about the origin: (x, y) -> (-y, x).
        let quarter = Affine {
            a: 0.0,
            b: -1.0,
            c: 0.0,
            d: 1.0,
            e: 0.0,
            f: 0.0,
        };
        assert_eq!(
            quarter.map_box([1.0, 2.0, 3.0, 4.0]),
            [-4.0, 1.0, -2.0, 3.0]
        );
        assert!(!quarter.is_axis_aligned());
        assert!(Affine::IDENTITY.is_axis_aligned());
        assert!(!Rect::of_box([0.0, 0.0, 0.0, 5.0]).is_usable());
        assert!(
            !Rect {
                rotation: f32::NAN,
                ..Rect::of_box([0.0, 0.0, 1.0, 1.0])
            }
            .is_usable()
        );
    }
}
