//! macOS capture through AVFoundation: built-in, USB, Continuity and virtual
//! cameras. Frames arrive on a dispatch queue, are converted there, and
//! wait in a one-frame slot for the reader.

use super::super::config::{CameraConfig, CameraFormat};
use super::super::{Captured, FrameSource, IngestError, convert};
use super::{Delivery, DeliverySlot, FRAME_TIMEOUT, Listed, closest_mode, pick};
use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool, NSObject, ProtocolObject};
use objc2::{AllocAnyThread, DefinedClass, define_class, msg_send};
use objc2_av_foundation::{
    AVAuthorizationStatus, AVCaptureConnection, AVCaptureDevice, AVCaptureDeviceDiscoverySession,
    AVCaptureDeviceInput, AVCaptureDevicePosition, AVCaptureDeviceTypeBuiltInWideAngleCamera,
    AVCaptureOutput, AVCaptureSession, AVCaptureVideoDataOutput,
    AVCaptureVideoDataOutputSampleBufferDelegate, AVMediaTypeVideo,
};
use objc2_core_media::{CMSampleBuffer, CMTime, CMVideoFormatDescriptionGetDimensions};
use objc2_core_video::{
    CVPixelBufferGetBaseAddress, CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRow,
    CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetHeight, CVPixelBufferGetHeightOfPlane,
    CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress,
    CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
};
use objc2_foundation::{
    NSArray, NSDictionary, NSNumber, NSObjectProtocol, NSProcessInfo, NSString,
};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Instant, SystemTime};

const fn fourcc(code: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*code)
}

const NV12_VIDEO: u32 = fourcc(b"420v");
const NV12_FULL: u32 = fourcc(b"420f");
const YUYV: u32 = fourcc(b"yuvs");
const UYVY: u32 = fourcc(b"2vuy");

/// Copies a sample's pixels to an RGB frame.
fn convert_sample(sample: &CMSampleBuffer) -> Result<Captured, String> {
    // SAFETY: `sample` is the live buffer AVFoundation passed to the callback.
    let image = unsafe { sample.image_buffer() }.ok_or("a sample without an image")?;
    let flags = CVPixelBufferLockFlags::ReadOnly;
    // SAFETY: `image` is a retained pixel buffer; locked once, unlocked below.
    if unsafe { CVPixelBufferLockBaseAddress(&image, flags) } != 0 {
        return Err("can't lock the pixel buffer".into());
    }
    let (w, h) = (
        CVPixelBufferGetWidth(&image) as u32,
        CVPixelBufferGetHeight(&image) as u32,
    );
    let format = CVPixelBufferGetPixelFormatType(&image);
    let colour = convert::Colour {
        full_range: format == NV12_FULL,
        hd: h >= 720,
    };
    let plane = |i: usize| -> Result<(&[u8], u32), String> {
        let base = CVPixelBufferGetBaseAddressOfPlane(&image, i) as *const u8;
        let stride = CVPixelBufferGetBytesPerRowOfPlane(&image, i);
        let rows = CVPixelBufferGetHeightOfPlane(&image, i);
        if base.is_null() {
            return Err("an unmapped plane".into());
        }
        // SAFETY: while the buffer is locked, the plane is `stride * rows`
        // readable bytes at `base`, and the slice doesn't outlive the lock.
        Ok((
            unsafe { std::slice::from_raw_parts(base, stride * rows) },
            stride as u32,
        ))
    };
    let packed = || -> Result<(&[u8], u32), String> {
        let base = CVPixelBufferGetBaseAddress(&image) as *const u8;
        let stride = CVPixelBufferGetBytesPerRow(&image);
        if base.is_null() {
            return Err("an unmapped buffer".into());
        }
        // SAFETY: as above, for a single-plane buffer.
        Ok((
            unsafe { std::slice::from_raw_parts(base, stride * h as usize) },
            stride as u32,
        ))
    };
    let frame = match format {
        NV12_VIDEO | NV12_FULL => {
            plane(0).and_then(|y| plane(1).and_then(|uv| convert::nv12(y, uv, w, h, colour)))
        }
        YUYV => packed().and_then(|(d, s)| convert::yuyv(d, w, h, s, colour)),
        UYVY => packed().and_then(|(d, s)| convert::uyvy(d, w, h, s, colour)),
        other => Err(format!("unexpected pixel format {other:#x}")),
    };
    // SAFETY: locked above with the same flags; no slice outlives this.
    unsafe { CVPixelBufferUnlockBaseAddress(&image, flags) };
    Ok(Captured {
        frame: frame?,
        capture_time: SystemTime::now(),
    })
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and this type
    // doesn't implement Drop.
    #[unsafe(super(NSObject))]
    #[name = "FlwrsCameraDelegate"]
    #[ivars = Arc<DeliverySlot>]
    struct Delegate;

    // SAFETY: NSObjectProtocol has no extra requirements.
    unsafe impl NSObjectProtocol for Delegate {}

    // SAFETY: the signature matches the protocol's method.
    unsafe impl AVCaptureVideoDataOutputSampleBufferDelegate for Delegate {
        #[unsafe(method(captureOutput:didOutputSampleBuffer:fromConnection:))]
        fn did_output(
            &self,
            _output: &AVCaptureOutput,
            sample: &CMSampleBuffer,
            _connection: &AVCaptureConnection,
        ) {
            self.ivars().put(match convert_sample(sample) {
                Ok(captured) => Delivery::Frame(captured),
                Err(e) => Delivery::Bad(e),
            });
        }
    }
);

impl Delegate {
    fn new(slot: Arc<DeliverySlot>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(slot);
        // SAFETY: NSObject's `init` on a freshly allocated instance.
        unsafe { msg_send![super(this), init] }
    }
}

pub(super) struct MacCamera {
    session: Retained<AVCaptureSession>,
    output: Retained<AVCaptureVideoDataOutput>,
    // Kept alive: the output holds its delegate weakly.
    _delegate: Retained<Delegate>,
    _queue: DispatchRetained<DispatchQueue>,
    slot: Arc<DeliverySlot>,
    name: String,
}

/// Waits for the user's answer when the OS hasn't asked yet.
fn check_permission() -> Result<(), IngestError> {
    let denied = || {
        IngestError::retryable(
            "camera access is denied: allow it for the app that started the server (System Settings → Privacy & Security → Camera)",
        )
    };
    // SAFETY: the media type constant is valid for the process's life.
    let media =
        unsafe { AVMediaTypeVideo }.ok_or_else(|| IngestError::fatal("no video media type"))?;
    // SAFETY: as above.
    let status = unsafe { AVCaptureDevice::authorizationStatusForMediaType(media) };
    if status == AVAuthorizationStatus::Authorized {
        return Ok(());
    }
    if status != AVAuthorizationStatus::NotDetermined {
        return Err(denied());
    }
    let answer = Arc::new((Mutex::new(None::<bool>), Condvar::new()));
    let inner = answer.clone();
    let block = RcBlock::new(move |granted: Bool| {
        *inner.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(granted.as_bool());
        inner.1.notify_all();
    });
    // SAFETY: the block is valid, and AVFoundation copies it.
    unsafe { AVCaptureDevice::requestAccessForMediaType_completionHandler(media, &block) };
    let until = Instant::now() + std::time::Duration::from_secs(60);
    let mut got = answer.0.lock().unwrap_or_else(|e| e.into_inner());
    while got.is_none() {
        let now = Instant::now();
        if now >= until {
            return Err(denied());
        }
        got = answer
            .1
            .wait_timeout(got, until - now)
            .unwrap_or_else(|e| e.into_inner())
            .0;
    }
    if *got == Some(true) {
        Ok(())
    } else {
        Err(denied())
    }
}

fn devices() -> Result<Retained<NSArray<AVCaptureDevice>>, IngestError> {
    // SAFETY: the constants are valid for the process's life; the array and
    // session are retained by the bindings.
    unsafe {
        let media = AVMediaTypeVideo.ok_or_else(|| IngestError::fatal("no video media type"))?;
        // By value, not the framework's constants: those added in macOS 14
        // would stop the binary from loading on older systems.
        let modern = NSProcessInfo::processInfo()
            .operatingSystemVersion()
            .majorVersion
            >= 14;
        let names: &[&str] = if modern {
            &[
                "AVCaptureDeviceTypeExternal",
                "AVCaptureDeviceTypeContinuityCamera",
            ]
        } else {
            &["AVCaptureDeviceTypeExternalUnknown"]
        };
        let extra: Vec<Retained<NSString>> = names.iter().map(|n| NSString::from_str(n)).collect();
        let mut device_types: Vec<&NSString> = vec![AVCaptureDeviceTypeBuiltInWideAngleCamera];
        device_types.extend(extra.iter().map(|k| &**k));
        let types = NSArray::from_slice(&device_types);
        let session =
            AVCaptureDeviceDiscoverySession::discoverySessionWithDeviceTypes_mediaType_position(
                &types,
                Some(media),
                AVCaptureDevicePosition::Unspecified,
            );
        Ok(session.devices())
    }
}

fn names(list: &NSArray<AVCaptureDevice>) -> Vec<Listed> {
    // SAFETY: reading names and ids of live, retained devices.
    list.iter()
        .map(|d| unsafe {
            Listed {
                id: d.uniqueID().to_string(),
                name: d.localizedName().to_string(),
            }
        })
        .collect()
}

/// The cameras the OS lists; none is opened and no permission is asked for.
pub(super) fn list() -> Result<Vec<Listed>, IngestError> {
    Ok(names(&*devices()?))
}

/// The device's best format for the settings, and the rate to set.
fn configure(device: &AVCaptureDevice, config: &CameraConfig) -> Result<(), IngestError> {
    let size = config.width.zip(config.height);
    // SAFETY: the formats and ranges are read from a live, retained device.
    let (formats, modes): (Vec<_>, Vec<(u32, u32, f64)>) = unsafe {
        let formats = device.formats().to_vec();
        let modes = formats
            .iter()
            .map(|f| {
                let d = CMVideoFormatDescriptionGetDimensions(&f.formatDescription());
                let max = f
                    .videoSupportedFrameRateRanges()
                    .iter()
                    .map(|r| r.maxFrameRate())
                    .fold(0.0, f64::max);
                (d.width.max(0) as u32, d.height.max(0) as u32, max)
            })
            .collect();
        (formats, modes)
    };
    let Some(best) = closest_mode(&modes, size, config.fps) else {
        return Err(IngestError::retryable("the camera lists no formats"));
    };
    let format = &formats[best];
    // A rate outside the format's ranges would raise an Objective-C
    // exception, which aborts the process: only set one that's in range.
    // SAFETY: as above.
    let rate = config.fps.and_then(|fps| unsafe {
        let wanted = f64::from(fps);
        let ok = format
            .videoSupportedFrameRateRanges()
            .iter()
            .any(|r| r.minFrameRate() <= wanted && wanted <= r.maxFrameRate());
        if !ok {
            log::warn!("Camera: {fps} fps isn't offered at this size; using the camera's rate");
        }
        ok.then(|| CMTime::new(1, fps as i32))
    });
    // SAFETY: the device is locked for configuration around the setters, the
    // format comes from this device, and the rate is one it supports.
    unsafe {
        device
            .lockForConfiguration()
            .map_err(|e| IngestError::retryable(format!("the camera is busy: {e}")))?;
        device.setActiveFormat(format);
        if let Some(duration) = rate {
            device.setActiveVideoMinFrameDuration(duration);
            device.setActiveVideoMaxFrameDuration(duration);
        }
        device.unlockForConfiguration();
    }
    Ok(())
}

pub(super) fn open(config: &CameraConfig) -> Result<MacCamera, IngestError> {
    let pixel = match config.format {
        CameraFormat::Auto | CameraFormat::Nv12 => NV12_FULL,
        CameraFormat::Yuyv => YUYV,
        CameraFormat::Uyvy => UYVY,
        CameraFormat::Mjpeg => {
            return Err(IngestError::fatal(
                "AVFoundation delivers nv12, yuyv or uyvy here, not mjpeg",
            ));
        }
    };
    check_permission()?;
    let list = devices()?;
    let listed = names(&list);
    let i = pick(config.device.as_deref(), &listed)?;
    let device = list.objectAtIndex(i);
    configure(&device, config)?;

    let slot = Arc::new(DeliverySlot::default());
    let delegate = Delegate::new(slot.clone());
    let queue = DispatchQueue::new("flwrs.camera", None);
    // SAFETY: the objects are retained here; the session is configured
    // between begin / commit and each add is guarded by its `canAdd`, since
    // a refused add would raise an exception. The video settings dictionary
    // uses the value of `kCVPixelBufferPixelFormatTypeKey`.
    let (session, output) = unsafe {
        let input = AVCaptureDeviceInput::deviceInputWithDevice_error(&device)
            .map_err(|e| IngestError::retryable(format!("can't open the camera: {e}")))?;
        let session = AVCaptureSession::new();
        let output = AVCaptureVideoDataOutput::new();
        let key = NSString::from_str("PixelFormatType");
        let value = NSNumber::new_u32(pixel);
        let settings: Retained<NSDictionary<NSString, AnyObject>> =
            NSDictionary::from_slices(&[&*key], &[value.as_ref() as &AnyObject]);
        output.setVideoSettings(Some(&settings));
        output.setAlwaysDiscardsLateVideoFrames(true);
        output.setSampleBufferDelegate_queue(
            Some(ProtocolObject::from_ref(&*delegate)),
            Some(&queue),
        );
        session.beginConfiguration();
        let added = session.canAddInput(&input) && {
            session.addInput(&input);
            session.canAddOutput(&output)
        };
        if added {
            session.addOutput(&output);
        }
        session.commitConfiguration();
        if !added {
            return Err(IngestError::retryable(
                "the camera can't be added to a capture session",
            ));
        }
        // The session resets the active format when it's configured.
        configure(&device, config)?;
        session.startRunning();
        (session, output)
    };
    Ok(MacCamera {
        session,
        output,
        _delegate: delegate,
        _queue: queue,
        slot,
        name: listed[i].name.clone(),
    })
}

impl FrameSource for MacCamera {
    fn next_frame(&mut self) -> Result<Option<Captured>, IngestError> {
        let until = Instant::now() + FRAME_TIMEOUT;
        loop {
            match self.slot.wait(until) {
                Some(Delivery::Frame(captured)) => return Ok(Some(captured)),
                Some(Delivery::Bad(e)) => log::debug!("Camera: skipped a frame: {e}"),
                Some(Delivery::Again) => {}
                Some(Delivery::Failed(e)) => return Err(IngestError::retryable(e)),
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
        format!("camera {} (AVFoundation)", self.name)
    }
}

impl Drop for MacCamera {
    fn drop(&mut self) {
        // SAFETY: stops the session and detaches the delegate of objects
        // this value still retains.
        unsafe {
            self.session.stopRunning();
            self.output.setSampleBufferDelegate_queue(None, None);
        }
    }
}
