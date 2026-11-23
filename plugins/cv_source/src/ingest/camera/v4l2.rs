//! Linux capture through V4L2 (the `v4l` crate): built-in, USB and any other
//! camera with a video node.

use super::super::config::{CameraConfig, CameraFormat};
use super::super::{Captured, FrameSource, IngestError, convert};
use super::{FRAME_TIMEOUT, Listed, MAX_BAD_FRAMES, pick};
use crate::frame::Frame;
use std::time::SystemTime;
use v4l::buffer::Type;
use v4l::capability::Flags;
use v4l::io::mmap::Stream;
use v4l::io::traits::CaptureStream;
use v4l::video::Capture;
use v4l::{Device, FourCC, Fraction};

const MJPG: FourCC = FourCC { repr: *b"MJPG" };
const JPEG: FourCC = FourCC { repr: *b"JPEG" };
const YUYV: FourCC = FourCC { repr: *b"YUYV" };
const UYVY: FourCC = FourCC { repr: *b"UYVY" };
const NV12: FourCC = FourCC { repr: *b"NV12" };

/// Formats we read, in the order `auto` prefers them: uncompressed is
/// cheaper at low sizes, but USB 2 cameras reach HD rates only in MJPEG.
fn preference(format: CameraFormat, hd: bool) -> Vec<FourCC> {
    match format {
        CameraFormat::Auto if hd => vec![MJPG, JPEG, NV12, YUYV, UYVY],
        CameraFormat::Auto => vec![NV12, YUYV, UYVY, MJPG, JPEG],
        CameraFormat::Mjpeg => vec![MJPG, JPEG],
        CameraFormat::Yuyv => vec![YUYV],
        CameraFormat::Uyvy => vec![UYVY],
        CameraFormat::Nv12 => vec![NV12],
    }
}

pub(super) struct V4l2Camera {
    stream: Stream<'static>,
    fourcc: FourCC,
    width: u32,
    height: u32,
    stride: u32,
    name: String,
}

fn retry(what: &str, e: std::io::Error) -> IngestError {
    IngestError::retryable(format!("{what}: {e}"))
}

pub(super) fn open(config: &CameraConfig) -> Result<V4l2Camera, IngestError> {
    // Video nodes include metadata and output ones: only real capture nodes.
    let mut nodes: Vec<(Listed, std::path::PathBuf)> = v4l::context::enum_devices()
        .into_iter()
        .filter_map(|n| {
            let dev = Device::with_path(n.path()).ok()?;
            let caps = dev.query_caps().ok()?;
            caps.capabilities.contains(Flags::VIDEO_CAPTURE).then(|| {
                (
                    Listed {
                        id: n.path().display().to_string(),
                        name: caps.card,
                    },
                    n.path().to_path_buf(),
                )
            })
        })
        .collect();
    nodes.sort_by_key(|(_, p)| {
        let digits: String = p
            .to_string_lossy()
            .chars()
            .rev()
            .take_while(char::is_ascii_digit)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        digits.parse::<u32>().unwrap_or(u32::MAX)
    });
    let listed: Vec<Listed> = nodes.iter().map(|(l, _)| l.clone()).collect();
    let i = pick(config.device.as_deref(), &listed)?;
    let dev = Device::with_path(&nodes[i].1).map_err(|e| retry(&listed[i].id, e))?;

    let (w, h) = config.width.zip(config.height).unwrap_or((1280, 720));
    let offered: Vec<FourCC> = dev
        .enum_formats()
        .map_err(|e| retry("list formats", e))?
        .into_iter()
        .map(|f| f.fourcc)
        .collect();
    let fourcc = preference(config.format, h >= 720)
        .into_iter()
        .find(|f| offered.contains(f))
        .ok_or_else(|| {
            let list: Vec<String> = offered.iter().map(|f| f.to_string()).collect();
            IngestError::fatal(format!(
                "{}: no usable pixel format for {:?}; it offers {}",
                listed[i].name,
                config.format,
                list.join(", ")
            ))
        })?;
    let mut format = dev.format().map_err(|e| retry("read format", e))?;
    (format.width, format.height, format.fourcc) = (w, h, fourcc);
    let format = dev
        .set_format(&format)
        .map_err(|e| retry("set format", e))?;
    if format.fourcc != fourcc {
        return Err(IngestError::fatal(format!(
            "{}: the driver gave {} instead of {fourcc}",
            listed[i].name, format.fourcc
        )));
    }
    if let Some(fps) = config.fps {
        let mut params = dev.params().map_err(|e| retry("read rate", e))?;
        params.interval = Fraction::new(1, fps);
        if let Err(e) = dev.set_params(&params) {
            log::warn!("Camera: can't set {fps} fps: {e}");
        }
    }
    // The stream shares the device's handle, so it outlives `dev`.
    let mut stream =
        Stream::with_buffers(&dev, Type::VideoCapture, 4).map_err(|e| retry("buffers", e))?;
    stream.set_timeout(FRAME_TIMEOUT);
    Ok(V4l2Camera {
        stream,
        fourcc,
        width: format.width,
        height: format.height,
        stride: format.stride,
        name: listed[i].name.clone(),
    })
}

impl V4l2Camera {
    fn convert(fourcc: FourCC, data: &[u8], w: u32, h: u32, stride: u32) -> Result<Frame, String> {
        let c = convert::Colour::video(h);
        if fourcc == MJPG || fourcc == JPEG {
            convert::jpeg(data)
        } else if fourcc == YUYV {
            convert::yuyv(data, w, h, stride, c)
        } else if fourcc == UYVY {
            convert::uyvy(data, w, h, stride, c)
        } else {
            let split = stride as usize * h as usize;
            if data.len() <= split {
                return Err(format!("a short NV12 frame of {} bytes", data.len()));
            }
            convert::nv12((&data[..split], stride), (&data[split..], stride), w, h, c)
        }
    }
}

impl FrameSource for V4l2Camera {
    fn next_frame(&mut self) -> Result<Option<Captured>, IngestError> {
        let (fourcc, w, h, stride) = (self.fourcc, self.width, self.height, self.stride);
        // A corrupt frame from the camera isn't a broken camera; many are.
        for _ in 0..MAX_BAD_FRAMES {
            let (data, meta) = self.stream.next().map_err(|e| retry("read", e))?;
            let used = (meta.bytesused as usize).min(data.len());
            match Self::convert(fourcc, &data[..used], w, h, stride) {
                Ok(frame) => {
                    return Ok(Some(Captured {
                        frame,
                        capture_time: SystemTime::now(),
                    }));
                }
                Err(e) => log::debug!("Camera: skipped a frame: {e}"),
            }
        }
        Err(IngestError::retryable(format!(
            "{}: {MAX_BAD_FRAMES} corrupt frames in a row",
            self.name
        )))
    }

    fn describe(&self) -> String {
        format!(
            "camera {} ({}x{} {})",
            self.name, self.width, self.height, self.fourcc
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_prefers_mjpeg_for_hd() {
        assert_eq!(preference(CameraFormat::Auto, true)[0], MJPG);
        assert_eq!(preference(CameraFormat::Auto, false)[0], NV12);
        assert_eq!(preference(CameraFormat::Yuyv, true), vec![YUYV]);
    }
}
