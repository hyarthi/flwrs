//! Drawing on a pixel buffer (`0x00RRGGBB`, the format window surfaces take).
//! Everything clips to the buffer.

use super::font::{ADVANCE, GLYPH_HEIGHT, GLYPH_WIDTH, glyph};

pub const fn rgb(r: u8, g: u8, b: u8) -> u32 {
    (r as u32) << 16 | (g as u32) << 8 | b as u32
}

/// `over` on `under`, `alpha` of 256 parts.
pub fn mix(under: u32, over: u32, alpha: u32) -> u32 {
    let channel = |shift: u32| {
        let (u, o) = ((under >> shift) & 255, (over >> shift) & 255);
        ((u * (256 - alpha) + o * alpha) >> 8) << shift
    };
    channel(16) | channel(8) | channel(0)
}

pub struct Canvas<'a> {
    pixels: &'a mut [u32],
    width: usize,
    height: usize,
}

impl<'a> Canvas<'a> {
    /// `pixels` holds `width * height` values.
    pub fn new(pixels: &'a mut [u32], width: usize, height: usize) -> Self {
        assert_eq!(pixels.len(), width * height);
        Self {
            pixels,
            width,
            height,
        }
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    pub fn pixels_mut(&mut self) -> &mut [u32] {
        self.pixels
    }

    fn index(&self, x: i32, y: i32) -> Option<usize> {
        (x >= 0 && y >= 0 && (x as usize) < self.width && (y as usize) < self.height)
            .then(|| y as usize * self.width + x as usize)
    }

    pub fn set(&mut self, x: i32, y: i32, colour: u32) {
        if let Some(i) = self.index(x, y) {
            self.pixels[i] = colour;
        }
    }

    /// A filled rectangle, blended (`alpha` of 256).
    pub fn fill(&mut self, x: i32, y: i32, w: i32, h: i32, colour: u32, alpha: u32) {
        let (x0, y0) = (x.max(0), y.max(0));
        let x1 = x.saturating_add(w).min(self.width as i32);
        let y1 = y.saturating_add(h).min(self.height as i32);
        for py in y0..y1 {
            for px in x0..x1 {
                let i = py as usize * self.width + px as usize;
                self.pixels[i] = if alpha >= 256 {
                    colour
                } else {
                    mix(self.pixels[i], colour, alpha)
                };
            }
        }
    }

    /// A box outline, no fill; `thickness` grows inwards.
    pub fn outline(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, colour: u32, thickness: i32) {
        let (w, h) = (x1 - x0 + 1, y1 - y0 + 1);
        if w <= 0 || h <= 0 {
            return;
        }
        let t = thickness.max(1).min(w).min(h);
        self.fill(x0, y0, w, t, colour, 256);
        self.fill(x0, y1 - t + 1, w, t, colour, 256);
        self.fill(x0, y0, t, h, colour, 256);
        self.fill(x1 - t + 1, y0, t, h, colour, 256);
    }

    /// A straight line.
    pub fn line(&mut self, a: (i32, i32), b: (i32, i32), colour: u32, thickness: i32) {
        let (mut x, mut y) = a;
        let (dx, dy) = ((b.0 - x).abs(), -(b.1 - y).abs());
        let (sx, sy) = ((b.0 - x).signum(), (b.1 - y).signum());
        let mut err = dx + dy;
        let t = thickness.max(1);
        // Bounded by the longer side, whatever the ends are.
        for _ in 0..=dx.max(-dy) {
            self.fill(x - (t - 1) / 2, y - (t - 1) / 2, t, t, colour, 256);
            let e = 2 * err;
            if e >= dy {
                err += dy;
                x += sx;
            }
            if e <= dx {
                err += dx;
                y += sy;
            }
        }
    }

    /// A filled dot.
    pub fn dot(&mut self, x: i32, y: i32, radius: i32, colour: u32) {
        for py in -radius..=radius {
            for px in -radius..=radius {
                if px * px + py * py <= radius * radius {
                    self.set(x + px, y + py, colour);
                }
            }
        }
    }

    /// Text with its top-left at (x, y), `scale` pixels per font pixel.
    pub fn text(&mut self, x: i32, y: i32, text: &str, colour: u32, scale: i32) {
        let scale = scale.max(1);
        for (n, c) in text.chars().enumerate() {
            let left = x + (n * ADVANCE) as i32 * scale;
            if left >= self.width as i32 {
                break;
            }
            for (cx, column) in glyph(c).iter().enumerate() {
                for cy in 0..GLYPH_HEIGHT {
                    if column >> cy & 1 == 1 {
                        self.fill(
                            left + cx as i32 * scale,
                            y + cy as i32 * scale,
                            scale,
                            scale,
                            colour,
                            256,
                        );
                    }
                }
            }
        }
    }
}

/// The pixel size of lines of text (with a 2-pixel margin, per scale).
pub fn text_size(lines: &[String], scale: i32) -> (i32, i32) {
    let longest = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0);
    let w = if longest == 0 {
        0
    } else {
        longest * ADVANCE - (ADVANCE - GLYPH_WIDTH)
    };
    let h = lines.len() * (GLYPH_HEIGHT + 2);
    ((w as i32 + 4) * scale, (h as i32 + 2) * scale)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RED: u32 = rgb(255, 0, 0);

    #[test]
    fn an_outline_is_on_the_edges_and_not_inside() {
        let mut p = vec![0; 100];
        let mut c = Canvas::new(&mut p, 10, 10);
        c.outline(2, 3, 7, 8, RED, 1);
        for (x, y) in [
            (2, 3),
            (7, 3),
            (2, 8),
            (7, 8),
            (5, 3),
            (2, 5),
            (7, 5),
            (5, 8),
        ] {
            assert_eq!(p[y * 10 + x], RED, "({x}, {y})");
        }
        for (x, y) in [(3, 4), (5, 5), (6, 7), (1, 3), (8, 8), (5, 9)] {
            assert_eq!(p[y * 10 + x], 0, "({x}, {y})");
        }
    }

    #[test]
    fn drawing_clips_to_the_buffer() {
        let mut p = vec![0; 16];
        let mut c = Canvas::new(&mut p, 4, 4);
        c.outline(-5, -5, 100, 100, RED, 2);
        c.line((-10, -10), (20, 20), RED, 3);
        c.dot(-1, 2, 3, RED);
        c.text(3, 3, "clipped", RED, 2);
        c.fill(i32::MAX - 1, 0, i32::MAX, 5, RED, 256);
    }

    #[test]
    fn a_line_reaches_both_ends() {
        let mut p = vec![0; 100];
        let mut c = Canvas::new(&mut p, 10, 10);
        c.line((1, 1), (8, 5), RED, 1);
        assert_eq!((p[11], p[58]), (RED, RED));
    }

    #[test]
    fn text_lands_where_asked() {
        let mut p = vec![0; 20 * 10];
        let mut c = Canvas::new(&mut p, 20, 10);
        c.text(2, 1, "I", RED, 1);
        // `I` has its stem in the third column.
        for y in 1..=7 {
            assert_eq!(p[y * 20 + 4], RED, "row {y}");
        }
        assert_eq!(p[20 * 8 + 4], 0);
        assert_eq!(p[20 + 1], 0);
    }

    #[test]
    fn blending_goes_from_one_colour_to_the_other() {
        assert_eq!(mix(0, RED, 0), 0);
        assert_eq!(mix(0, rgb(200, 100, 0), 128), rgb(100, 50, 0));
    }
}
