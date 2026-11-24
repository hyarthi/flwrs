//! Windows capture through Media Foundation's source reader: built-in, USB
//! and virtual cameras. The reader runs in its asynchronous mode (its
//! synchronous read can block forever on a camera that stops), so every read
//! waits with a deadline.

use super::super::config::{CameraConfig, CameraFormat};
use super::super::{Captured, FrameSource, IngestError, convert};
use super::{Delivery, DeliverySlot, FRAME_TIMEOUT, Listed, MAX_BAD_FRAMES, closest_mode, pick};
use crate::frame::Frame;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{
    COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use windows::core::{GUID, HRESULT, PWSTR, Ref, implement};

// The stream index constant, as the API takes it.
const FIRST_VIDEO: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

fn retry(what: &str, e: windows::core::Error) -> IngestError {
    IngestError::retryable(format!("{what}: {e}"))
}

/// COM and Media Foundation for this thread, released last.
struct Runtime;

impl Runtime {
    fn start() -> Result<Self, IngestError> {
        // SAFETY: paired with `CoUninitialize` in `drop`, on this thread
        // (sources stay on the thread that opened them).
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
            .ok()
            .map_err(|e| retry("COM", e))?;
        // SAFETY: paired with `MFShutdown` in `drop`.
        if let Err(e) = unsafe { MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET) } {
            // SAFETY: undoes the initialisation above.
            unsafe { CoUninitialize() };
            return Err(retry("Media Foundation", e));
        }
        Ok(Self)
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        // SAFETY: both were started in `start`, on this thread.
        unsafe {
            let _ = MFShutdown();
            CoUninitialize();
        }
    }
}

fn subtype_rank(subtype: GUID, format: CameraFormat, hd: bool) -> Option<u8> {
    let of = |list: &[GUID]| list.iter().position(|g| *g == subtype).map(|i| i as u8);
    match format {
        CameraFormat::Auto if hd => of(&[
            MFVideoFormat_MJPG,
            MFVideoFormat_NV12,
            MFVideoFormat_YUY2,
            MFVideoFormat_UYVY,
        ]),
        CameraFormat::Auto => of(&[
            MFVideoFormat_NV12,
            MFVideoFormat_YUY2,
            MFVideoFormat_UYVY,
            MFVideoFormat_MJPG,
        ]),
        CameraFormat::Mjpeg => of(&[MFVideoFormat_MJPG]),
        CameraFormat::Yuyv => of(&[MFVideoFormat_YUY2]),
        CameraFormat::Uyvy => of(&[MFVideoFormat_UYVY]),
        CameraFormat::Nv12 => of(&[MFVideoFormat_NV12]),
    }
}

/// A string attribute of an activation object.
fn string_of(device: &IMFActivate, key: &GUID) -> Option<String> {
    let mut text = PWSTR::null();
    let mut len = 0;
    // SAFETY: on success the callee allocated `text`, freed below.
    unsafe {
        device.GetAllocatedString(key, &mut text, &mut len).ok()?;
        let out = text.to_string().ok();
        CoTaskMemFree(Some(text.0 as *const _));
        out
    }
}

/// How long closing waits for reads in flight.
const FLUSH_TIMEOUT: Duration = Duration::from_secs(2);

/// How a sample's bytes are laid out.
#[derive(Clone, Copy)]
struct Layout {
    subtype: GUID,
    width: u32,
    height: u32,
    stride: u32,
}

impl Layout {
    fn convert(self, data: &[u8]) -> Result<Frame, String> {
        let (w, h, stride) = (self.width, self.height, self.stride);
        let c = convert::Colour::video(h);
        if self.subtype == MFVideoFormat_MJPG {
            convert::jpeg(data)
        } else if self.subtype == MFVideoFormat_YUY2 {
            convert::yuyv(data, w, h, stride, c)
        } else if self.subtype == MFVideoFormat_UYVY {
            convert::uyvy(data, w, h, stride, c)
        } else {
            let split = stride as usize * h as usize;
            if data.len() <= split {
                return Err(format!("a short NV12 frame of {} bytes", data.len()));
            }
            convert::nv12((&data[..split], stride), (&data[split..], stride), w, h, c)
        }
    }

    fn convert_sample(self, sample: &IMFSample) -> Result<Frame, String> {
        // SAFETY: the buffer is locked for the slice's life and unlocked
        // before leaving the block.
        unsafe {
            let buffer = sample
                .ConvertToContiguousBuffer()
                .map_err(|e| format!("buffer: {e}"))?;
            let mut base = std::ptr::null_mut();
            let mut len = 0;
            buffer
                .Lock(&mut base, None, Some(&mut len))
                .map_err(|e| format!("lock: {e}"))?;
            let out = self.convert(std::slice::from_raw_parts(base, len as usize));
            let _ = buffer.Unlock();
            out
        }
    }
}

/// What the reader's callback shares with the camera.
#[derive(Default)]
struct Shared {
    slot: DeliverySlot,
    layout: OnceLock<Layout>,
    flushed: Mutex<bool>,
    flush_done: Condvar,
}

/// Runs on Media Foundation's work queue, one read at a time.
#[implement(IMFSourceReaderCallback)]
struct ReadCallback {
    shared: Arc<Shared>,
}

impl IMFSourceReaderCallback_Impl for ReadCallback_Impl {
    fn OnReadSample(
        &self,
        status: HRESULT,
        _stream: u32,
        flags: u32,
        _timestamp: i64,
        sample: Ref<IMFSample>,
    ) -> windows::core::Result<()> {
        let broken = MF_SOURCE_READERF_ERROR.0 as u32 | MF_SOURCE_READERF_ENDOFSTREAM.0 as u32;
        let delivery = if status.is_err() {
            Delivery::Failed(format!("read: {}", windows::core::Error::from(status)))
        } else if flags & broken != 0 {
            Delivery::Failed("the stream ended".into())
        } else {
            match (sample.as_ref(), self.shared.layout.get()) {
                (Some(sample), Some(layout)) => match layout.convert_sample(sample) {
                    Ok(frame) => Delivery::Frame(Captured {
                        frame,
                        capture_time: SystemTime::now(),
                    }),
                    Err(e) => Delivery::Bad(e),
                },
                _ => Delivery::Again,
            }
        };
        self.shared.slot.put(delivery);
        Ok(())
    }

    fn OnFlush(&self, _stream: u32) -> windows::core::Result<()> {
        *self
            .shared
            .flushed
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = true;
        self.shared.flush_done.notify_all();
        Ok(())
    }

    fn OnEvent(&self, _stream: u32, _event: Ref<IMFMediaEvent>) -> windows::core::Result<()> {
        Ok(())
    }
}

pub(super) struct MfCamera {
    reader: IMFSourceReader,
    source: IMFMediaSource,
    shared: Arc<Shared>,
    width: u32,
    height: u32,
    name: String,
    // Dropped last (fields drop in order).
    _runtime: Runtime,
}

fn split(pair: u64) -> (u32, u32) {
    ((pair >> 32) as u32, pair as u32)
}

/// The video capture devices, not activated. Needs a started [`Runtime`].
fn enumerate() -> Result<(Vec<IMFActivate>, Vec<Listed>), IngestError> {
    // SAFETY: every interface is a live COM reference from the call before;
    // the activation array is released and freed before leaving the block.
    unsafe {
        let mut attributes = None;
        MFCreateAttributes(&mut attributes, 1).map_err(|e| retry("attributes", e))?;
        let attributes = attributes.ok_or_else(|| IngestError::fatal("no attributes"))?;
        attributes
            .SetGUID(
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
            )
            .map_err(|e| retry("attributes", e))?;
        let mut raw = std::ptr::null_mut();
        let mut count = 0;
        MFEnumDeviceSources(&attributes, &mut raw, &mut count)
            .map_err(|e| retry("list cameras", e))?;
        let devices: Vec<IMFActivate> = if raw.is_null() {
            vec![]
        } else {
            let items = std::slice::from_raw_parts_mut(raw, count as usize);
            let owned = items.iter_mut().filter_map(Option::take).collect();
            CoTaskMemFree(Some(raw as *const _));
            owned
        };
        let listed: Vec<Listed> = devices
            .iter()
            .map(|d| Listed {
                id: string_of(d, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK)
                    .unwrap_or_default(),
                name: string_of(d, &MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME).unwrap_or_default(),
            })
            .collect();
        Ok((devices, listed))
    }
}

/// The cameras the OS lists; none is activated.
pub(super) fn list() -> Result<Vec<Listed>, IngestError> {
    let _runtime = Runtime::start()?;
    enumerate().map(|(_, listed)| listed)
}

pub(super) fn open(config: &CameraConfig) -> Result<MfCamera, IngestError> {
    let runtime = Runtime::start()?;
    let (devices, listed) = enumerate()?;
    // SAFETY: the devices are live COM references.
    let (source, name) = unsafe {
        let i = pick(config.device.as_deref(), &listed)?;
        let source: IMFMediaSource = devices[i]
            .ActivateObject()
            .map_err(|e| retry("open the camera", e))?;
        (source, listed[i].name.clone())
    };
    // SAFETY: as above; `GetNativeMediaType` ends with an error past the last.
    unsafe {
        let shared = Arc::new(Shared::default());
        let callback: IMFSourceReaderCallback = ReadCallback {
            shared: shared.clone(),
        }
        .into();
        let mut reader_attributes = None;
        MFCreateAttributes(&mut reader_attributes, 1).map_err(|e| retry("attributes", e))?;
        let reader_attributes =
            reader_attributes.ok_or_else(|| IngestError::fatal("no attributes"))?;
        reader_attributes
            .SetUnknown(&MF_SOURCE_READER_ASYNC_CALLBACK, &callback)
            .map_err(|e| retry("attributes", e))?;
        let reader = MFCreateSourceReaderFromMediaSource(&source, &reader_attributes)
            .map_err(|e| retry("source reader", e))?;
        let hd = config.height.unwrap_or(720) >= 720;
        let mut types = vec![];
        let mut modes = vec![];
        let mut index = 0;
        while let Ok(media) = reader.GetNativeMediaType(FIRST_VIDEO, index) {
            index += 1;
            let Ok(subtype) = media.GetGUID(&MF_MT_SUBTYPE) else {
                continue;
            };
            let Some(rank) = subtype_rank(subtype, config.format, hd) else {
                continue;
            };
            let Ok(size) = media.GetUINT64(&MF_MT_FRAME_SIZE) else {
                continue;
            };
            let (w, h) = split(size);
            let rate = media
                .GetUINT64(&MF_MT_FRAME_RATE)
                .map(split)
                .unwrap_or((30, 1));
            let fps = f64::from(rate.0) / f64::from(rate.1.max(1));
            types.push((media, subtype, rank));
            modes.push((w, h, fps));
        }
        // The best format's modes only, so a size isn't traded for a format.
        let Some(best_rank) = types.iter().map(|t| t.2).min() else {
            return Err(IngestError::fatal(format!(
                "{name}: no usable pixel format"
            )));
        };
        let keep: Vec<usize> = (0..types.len())
            .filter(|&i| types[i].2 == best_rank)
            .collect();
        let kept: Vec<_> = keep.iter().map(|&i| modes[i]).collect();
        let at = closest_mode(&kept, config.width.zip(config.height), config.fps)
            .map(|i| keep[i])
            .ok_or_else(|| IngestError::fatal(format!("{name}: no usable mode")))?;
        let (media, subtype, _) = &types[at];
        let (width, height, _) = modes[at];
        reader
            .SetCurrentMediaType(FIRST_VIDEO, None, media)
            .map_err(|e| retry("set the format", e))?;
        // A negative stride marks a bottom-up image, which these formats
        // aren't; the default is the tight row.
        let packed = *subtype == MFVideoFormat_YUY2 || *subtype == MFVideoFormat_UYVY;
        let tight = if packed { width * 2 } else { width };
        let stride = media
            .GetUINT32(&MF_MT_DEFAULT_STRIDE)
            .ok()
            .map(|s| s as i32)
            .filter(|s| *s > 0)
            .map_or(tight, |s| s as u32);
        let _ = shared.layout.set(Layout {
            subtype: *subtype,
            width,
            height,
            stride,
        });
        Ok(MfCamera {
            reader,
            source,
            shared,
            width,
            height,
            name,
            _runtime: runtime,
        })
    }
}

impl FrameSource for MfCamera {
    fn next_frame(&mut self) -> Result<Option<Captured>, IngestError> {
        let until = Instant::now() + FRAME_TIMEOUT;
        let mut bad = 0;
        loop {
            // SAFETY: in asynchronous mode every out-pointer must be null;
            // the result comes to the callback.
            unsafe {
                self.reader
                    .ReadSample(FIRST_VIDEO, 0, None, None, None, None)
            }
            .map_err(|e| retry("read", e))?;
            match self.shared.slot.wait(until) {
                Some(Delivery::Frame(captured)) => return Ok(Some(captured)),
                Some(Delivery::Again) => {}
                Some(Delivery::Bad(e)) => {
                    log::debug!("Camera: skipped a frame: {e}");
                    bad += 1;
                    if bad >= MAX_BAD_FRAMES {
                        return Err(IngestError::retryable(format!(
                            "{}: {MAX_BAD_FRAMES} corrupt frames in a row",
                            self.name
                        )));
                    }
                }
                Some(Delivery::Failed(e)) => {
                    return Err(IngestError::retryable(format!("{}: {e}", self.name)));
                }
                None => {
                    return Err(IngestError::retryable(format!(
                        "no frame from {} for {FRAME_TIMEOUT:?}",
                        self.name
                    )));
                }
            }
        }
    }

    fn describe(&self) -> String {
        format!("camera {} ({}x{})", self.name, self.width, self.height)
    }
}

impl Drop for MfCamera {
    fn drop(&mut self) {
        // Reads in flight finish (or are cancelled) before the runtime
        // stops, with a limit: a stuck driver mustn't hold up the plugin.
        // SAFETY: the reader is live; `Flush` completes through `OnFlush`.
        if unsafe { self.reader.Flush(FIRST_VIDEO) }.is_ok() {
            let until = Instant::now() + FLUSH_TIMEOUT;
            let mut flushed = self
                .shared
                .flushed
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            while !*flushed {
                let now = Instant::now();
                if now >= until {
                    log::warn!("Camera: {} didn't stop in time", self.name);
                    break;
                }
                flushed = self
                    .shared
                    .flush_done
                    .wait_timeout(flushed, until - now)
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
            }
        }
        // SAFETY: the source is live until its fields drop.
        let _ = unsafe { self.source.Shutdown() };
    }
}
