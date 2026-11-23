//! Camera pixels to RGB frames.

use crate::frame::{Frame, MAX_FRAME_SIDE};
use yuvutils_rs::{
    YuvBiPlanarImage, YuvConversionMode, YuvPackedImage, YuvRange, YuvStandardMatrix,
};
use zune_core::bytestream::ZCursor;
use zune_core::colorspace::ColorSpace;
use zune_core::options::DecoderOptions;
use zune_jpeg::JpegDecoder;

/// How a camera's YUV values are coded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Colour {
    pub full_range: bool,
    pub hd: bool,
}

impl Colour {
    /// Video range, with the matrix conventional for the height.
    pub fn video(height: u32) -> Self {
        Self {
            full_range: false,
            hd: height >= 720,
        }
    }

    fn range(self) -> YuvRange {
        if self.full_range {
            YuvRange::Full
        } else {
            YuvRange::Limited
        }
    }

    fn matrix(self) -> YuvStandardMatrix {
        if self.hd {
            YuvStandardMatrix::Bt709
        } else {
            YuvStandardMatrix::Bt601
        }
    }
}

fn buffer(width: u32, height: u32) -> Result<Vec<u8>, String> {
    if width == 0 || height == 0 || width > MAX_FRAME_SIDE || height > MAX_FRAME_SIDE {
        return Err(format!("a frame of {width}x{height}"));
    }
    Ok(vec![0; width as usize * height as usize * 3])
}

/// One JPEG image.
pub fn jpeg(data: &[u8]) -> Result<Frame, String> {
    let options = DecoderOptions::default()
        .set_max_width(MAX_FRAME_SIDE as usize)
        .set_max_height(MAX_FRAME_SIDE as usize)
        .jpeg_set_out_colorspace(ColorSpace::RGB);
    let mut decoder = JpegDecoder::new_with_options(ZCursor::new(data), options);
    let rgb = decoder.decode().map_err(|e| format!("JPEG: {e:?}"))?;
    let (w, h) = decoder
        .dimensions()
        .ok_or_else(|| "JPEG: no dimensions".to_string())?;
    Frame::new(w as u32, h as u32, rgb)
}

/// Packed 4:2:2, Y0 U Y1 V.
pub fn yuyv(data: &[u8], w: u32, h: u32, stride: u32, c: Colour) -> Result<Frame, String> {
    let mut rgb = buffer(w, h)?;
    let image = YuvPackedImage {
        yuy: data,
        yuy_stride: stride,
        width: w,
        height: h,
    };
    yuvutils_rs::yuyv422_to_rgb(&image, &mut rgb, w * 3, c.range(), c.matrix())
        .map_err(|e| format!("YUYV: {e}"))?;
    Frame::new(w, h, rgb)
}

/// Packed 4:2:2, U Y0 V Y1.
pub fn uyvy(data: &[u8], w: u32, h: u32, stride: u32, c: Colour) -> Result<Frame, String> {
    let mut rgb = buffer(w, h)?;
    let image = YuvPackedImage {
        yuy: data,
        yuy_stride: stride,
        width: w,
        height: h,
    };
    yuvutils_rs::uyvy422_to_rgb(&image, &mut rgb, w * 3, c.range(), c.matrix())
        .map_err(|e| format!("UYVY: {e}"))?;
    Frame::new(w, h, rgb)
}

/// Two planes, 4:2:0: Y, then interleaved U V.
pub fn nv12(y: (&[u8], u32), uv: (&[u8], u32), w: u32, h: u32, c: Colour) -> Result<Frame, String> {
    let mut rgb = buffer(w, h)?;
    // The converter takes only a tightly packed chroma plane of the exact
    // size, and drivers pad their rows: repack when they do.
    let row = w.div_ceil(2) as usize * 2;
    let rows = h.div_ceil(2) as usize;
    let stride = uv.1 as usize;
    if stride < row || uv.0.len() < stride * (rows - 1) + row {
        return Err(format!("NV12: a chroma plane of {} bytes", uv.0.len()));
    }
    let packed: Vec<u8>;
    let uv_plane = if stride == row {
        &uv.0[..row * rows]
    } else {
        packed = (0..rows)
            .flat_map(|r| &uv.0[r * stride..r * stride + row])
            .copied()
            .collect();
        &packed
    };
    let luma = y.1 as usize;
    if luma < w as usize || y.0.len() < luma * (h as usize - 1) + w as usize {
        return Err(format!("NV12: a luma plane of {} bytes", y.0.len()));
    }
    let tight: Vec<u8>;
    // The last row of a plane may stop short of its stride.
    let y_plane = if y.0.len() >= luma * h as usize {
        &y.0[..luma * h as usize]
    } else {
        tight =
            y.0.iter()
                .copied()
                .chain(std::iter::repeat(0))
                .take(luma * h as usize)
                .collect();
        &tight
    };
    let image = YuvBiPlanarImage {
        y_plane,
        y_stride: y.1,
        uv_plane,
        uv_stride: row as u32,
        width: w,
        height: h,
    };
    yuvutils_rs::yuv_nv12_to_rgb(
        &image,
        &mut rgb,
        w * 3,
        c.range(),
        c.matrix(),
        YuvConversionMode::Balanced,
    )
    .map_err(|e| format!("NV12: {e}"))?;
    Frame::new(w, h, rgb)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn near(a: &[u8], b: [u8; 3]) -> bool {
        a.iter().zip(b).all(|(&x, y)| x.abs_diff(y) <= 3)
    }

    // Video-range BT.601: pure red is Y 81, U 90, V 240.
    const RED: (u8, u8, u8) = (81, 90, 240);

    #[test]
    fn packed_formats_give_the_colour() {
        let c = Colour::video(2);
        let (y, u, v) = RED;
        let f = yuyv(&[y, u, y, v].repeat(4), 2, 4, 4, c).unwrap();
        assert!(near(&f.rgb()[..3], [255, 0, 0]), "{:?}", &f.rgb()[..3]);
        let f = uyvy(&[u, y, v, y].repeat(4), 2, 4, 4, c).unwrap();
        assert!(near(&f.rgb()[..3], [255, 0, 0]), "{:?}", &f.rgb()[..3]);
    }

    #[test]
    fn nv12_honours_strides() {
        // Rows padded by 2 bytes, as drivers do.
        let (y, u, v) = RED;
        let luma = [y, y, 0, 0].repeat(2);
        let chroma = [u, v, 0, 0];
        let f = nv12((&luma, 4), (&chroma, 4), 2, 2, Colour::video(2)).unwrap();
        assert!(near(&f.rgb()[9..12], [255, 0, 0]), "{:?}", f.rgb());
    }

    #[test]
    fn short_buffers_are_errors() {
        let c = Colour::video(4);
        assert!(yuyv(&[0; 3], 2, 4, 4, c).is_err());
        assert!(nv12((&[0; 2], 2), (&[0; 1], 2), 2, 2, c).is_err());
        assert!(jpeg(&[0xFF, 0xD8, 0, 1]).is_err());
    }
}
