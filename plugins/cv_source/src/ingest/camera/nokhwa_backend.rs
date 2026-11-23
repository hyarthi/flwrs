//! macOS fallback capture through `nokhwa`. Always asks for YUYV: its
//! binding mislabels NV12 and requests a 10-bit format for it.

use super::super::config::CameraConfig;
use super::super::{Captured, FrameSource, IngestError, convert};
use super::{Listed, pick};
use nokhwa::Camera;
use nokhwa::pixel_format::RgbFormat;
use nokhwa::utils::{
    ApiBackend, CameraFormat as NkFormat, CameraIndex, FrameFormat, RequestedFormat,
    RequestedFormatType, Resolution,
};
use std::time::SystemTime;

pub(super) struct NokhwaCamera {
    camera: Camera,
    name: String,
}

pub(super) fn open(config: &CameraConfig) -> Result<NokhwaCamera, IngestError> {
    let retry =
        |what: &str, e: nokhwa::NokhwaError| IngestError::retryable(format!("nokhwa {what}: {e}"));
    let infos = nokhwa::query(ApiBackend::AVFoundation).map_err(|e| retry("list", e))?;
    let listed: Vec<Listed> = infos
        .iter()
        .map(|c| Listed {
            id: c.misc(),
            name: c.human_name(),
        })
        .collect();
    let i = pick(config.device.as_deref(), &listed)?;
    let (w, h) = config.width.zip(config.height).unwrap_or((1280, 720));
    let wanted = NkFormat::new(
        Resolution::new(w, h),
        FrameFormat::YUYV,
        config.fps.unwrap_or(30),
    );
    let requested = RequestedFormat::new::<RgbFormat>(RequestedFormatType::Closest(wanted));
    let mut camera = Camera::with_backend(
        infos[i].index().clone(),
        requested,
        ApiBackend::AVFoundation,
    )
    .or_else(|_| {
        Camera::with_backend(
            CameraIndex::Index(i as u32),
            requested,
            ApiBackend::AVFoundation,
        )
    })
    .map_err(|e| retry("open", e))?;
    camera.open_stream().map_err(|e| retry("start", e))?;
    Ok(NokhwaCamera {
        camera,
        name: listed[i].name.clone(),
    })
}

impl FrameSource for NokhwaCamera {
    fn next_frame(&mut self) -> Result<Option<Captured>, IngestError> {
        let buffer = self
            .camera
            .frame()
            .map_err(|e| IngestError::retryable(format!("nokhwa read: {e}")))?;
        let res = buffer.resolution();
        let (w, h) = (res.width(), res.height());
        let frame = convert::yuyv(buffer.buffer(), w, h, w * 2, convert::Colour::video(h))
            .map_err(IngestError::retryable)?;
        Ok(Some(Captured {
            frame,
            capture_time: SystemTime::now(),
        }))
    }

    fn describe(&self) -> String {
        format!("camera {} (nokhwa)", self.name)
    }
}

impl Drop for NokhwaCamera {
    fn drop(&mut self) {
        let _ = self.camera.stop_stream();
    }
}
