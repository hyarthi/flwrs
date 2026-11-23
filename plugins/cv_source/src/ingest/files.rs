//! Frames from files: deterministic, for tests and for tuning.

use super::config::FileInput;
use super::{Captured, FrameSource, IngestError, convert};
use crate::frame::Frame;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

/// The largest single JPEG in an MJPEG file.
const MAX_JPEG_BYTES: usize = 64 << 20;

/// A file input without `fps` counts as 30 frames per second on the stream's
/// clock (durations in rules need one).
const DEFAULT_INTERVAL: Duration = Duration::from_nanos(33_333_333);

/// When each frame is due.
struct Pace {
    every: Option<Duration>,
    next: Option<Instant>,
}

impl Pace {
    fn new(fps: Option<f32>) -> Self {
        Self {
            every: fps.map(|f| Duration::from_secs_f32(1.0 / f)),
            next: None,
        }
    }

    fn due(&self) -> Option<Instant> {
        self.every.and(self.next)
    }

    fn advance(&mut self) {
        if let Some(every) = self.every {
            let now = Instant::now();
            // Behind schedule: carry on from now, no burst.
            self.next = Some(self.next.map_or(now, |n| n.max(now - every)) + every);
        }
    }
}

fn fatal(what: impl std::fmt::Display) -> IngestError {
    IngestError::fatal(what.to_string())
}

/// A folder of JPEG / PNG images, in name order.
pub struct ImageFolder {
    files: Vec<PathBuf>,
    at: usize,
    looped: bool,
    pace: Pace,
}

impl ImageFolder {
    pub fn open(input: &FileInput, path: &Path) -> Result<Self, IngestError> {
        let mut files: Vec<PathBuf> = std::fs::read_dir(path)
            .map_err(|e| fatal(format!("{}: {e}", path.display())))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                p.extension()
                    .and_then(|x| x.to_str())
                    .is_some_and(|x| ["jpg", "jpeg", "png"].contains(&&*x.to_lowercase()))
            })
            .collect();
        files.sort();
        if files.is_empty() {
            return Err(fatal(format!("{}: no JPEG or PNG images", path.display())));
        }
        Ok(Self {
            files,
            at: 0,
            looped: input.looped,
            pace: Pace::new(input.fps),
        })
    }
}

fn read_image(path: &Path) -> Result<Frame, IngestError> {
    let at = |e: String| fatal(format!("{}: {e}", path.display()));
    let data = std::fs::read(path).map_err(|e| at(e.to_string()))?;
    if data.starts_with(&[0xFF, 0xD8]) {
        return convert::jpeg(&data).map_err(at);
    }
    let rgb = image::load_from_memory(&data)
        .map_err(|e| at(e.to_string()))?
        .into_rgb8();
    let (w, h) = rgb.dimensions();
    Frame::new(w, h, rgb.into_raw()).map_err(at)
}

impl FrameSource for ImageFolder {
    fn next_frame(&mut self) -> Result<Option<Captured>, IngestError> {
        if self.at == self.files.len() {
            if !self.looped {
                return Ok(None);
            }
            self.at = 0;
        }
        let frame = read_image(&self.files[self.at])?;
        self.at += 1;
        self.pace.advance();
        Ok(Some(Captured {
            frame,
            capture_time: SystemTime::now(),
        }))
    }

    fn due(&self) -> Option<Instant> {
        self.pace.due()
    }

    fn lossless(&self) -> bool {
        self.pace.every.is_none()
    }

    fn frame_interval(&self) -> Option<Duration> {
        Some(self.pace.every.unwrap_or(DEFAULT_INTERVAL))
    }

    fn describe(&self) -> String {
        format!("{} images", self.files.len())
    }
}

/// The end of the JPEG at the start of `data`, walking its segments (so a
/// thumbnail inside its metadata isn't taken for the end). `None`: more
/// bytes are needed.
fn jpeg_end(data: &[u8]) -> Result<Option<usize>, String> {
    if data.len() < 2 {
        return Ok(None);
    }
    if data[..2] != [0xFF, 0xD8] {
        return Err("not a JPEG frame".into());
    }
    let mut i = 2;
    loop {
        let Some(&[a, marker]) = data.get(i..i + 2).and_then(|s| s.first_chunk::<2>()) else {
            return Ok(None);
        };
        if a != 0xFF {
            return Err(format!("bad JPEG marker at {i}"));
        }
        match marker {
            0xFF => i += 1, // fill byte
            0xD9 => return Ok(Some(i + 2)),
            0x01 | 0xD0..=0xD7 => i += 2,
            _ => {
                let Some(len) = data.get(i + 2..i + 4) else {
                    return Ok(None);
                };
                let len = u16::from_be_bytes([len[0], len[1]]) as usize;
                if len < 2 {
                    return Err(format!("bad JPEG segment at {i}"));
                }
                i += 2 + len;
                if marker == 0xDA {
                    // Entropy-coded data, up to the next real marker (a
                    // progressive JPEG has several scans).
                    loop {
                        let Some(off) = data
                            .get(i..)
                            .and_then(|d| d.iter().position(|&b| b == 0xFF))
                        else {
                            return Ok(None);
                        };
                        i += off;
                        match data.get(i + 1) {
                            None => return Ok(None),
                            Some(0x00 | 0xD0..=0xD7) => i += 2,
                            Some(_) => break,
                        }
                    }
                }
            }
        }
    }
}

/// A file of concatenated JPEG frames, read as it goes.
pub struct MjpegFile {
    path: PathBuf,
    file: File,
    buf: Vec<u8>,
    start: usize,
    eof: bool,
    frames: u64,
    looped: bool,
    pace: Pace,
}

impl MjpegFile {
    pub fn open(input: &FileInput, path: &Path) -> Result<Self, IngestError> {
        let file = File::open(path).map_err(|e| fatal(format!("{}: {e}", path.display())))?;
        Ok(Self {
            path: path.to_path_buf(),
            file,
            buf: vec![],
            start: 0,
            eof: false,
            frames: 0,
            looped: input.looped,
            pace: Pace::new(input.fps),
        })
    }

    fn error(&self, e: String) -> IngestError {
        fatal(format!("{}: {e}", self.path.display()))
    }

    fn fill(&mut self) -> Result<(), IngestError> {
        if self.start > 0 {
            self.buf.drain(..self.start);
            self.start = 0;
        }
        let have = self.buf.len();
        self.buf.resize(have + (1 << 20), 0);
        let n = self
            .file
            .read(&mut self.buf[have..])
            .map_err(|e| fatal(format!("{}: {e}", self.path.display())))?;
        self.buf.truncate(have + n);
        self.eof = n == 0;
        Ok(())
    }

    fn rewind(&mut self) -> Result<bool, IngestError> {
        if !self.looped || self.frames == 0 {
            return Ok(false);
        }
        self.file = File::open(&self.path).map_err(fatal)?;
        self.buf.clear();
        self.start = 0;
        self.eof = false;
        Ok(true)
    }
}

impl FrameSource for MjpegFile {
    fn next_frame(&mut self) -> Result<Option<Captured>, IngestError> {
        loop {
            match jpeg_end(&self.buf[self.start..]).map_err(|e| self.error(e))? {
                Some(len) => {
                    let range = self.start..self.start + len;
                    let frame =
                        convert::jpeg(&self.buf[range.clone()]).map_err(|e| self.error(e))?;
                    self.start = range.end;
                    self.frames += 1;
                    self.pace.advance();
                    return Ok(Some(Captured {
                        frame,
                        capture_time: SystemTime::now(),
                    }));
                }
                None if self.eof => {
                    if self.start < self.buf.len() {
                        log::warn!("{}: ends inside a frame", self.path.display());
                    }
                    if !self.rewind()? {
                        return Ok(None);
                    }
                }
                None if self.buf.len() - self.start > MAX_JPEG_BYTES => {
                    return Err(fatal(format!(
                        "{}: a frame over {MAX_JPEG_BYTES} bytes",
                        self.path.display()
                    )));
                }
                None => self.fill()?,
            }
        }
    }

    fn due(&self) -> Option<Instant> {
        self.pace.due()
    }

    fn lossless(&self) -> bool {
        self.pace.every.is_none()
    }

    fn frame_interval(&self) -> Option<Duration> {
        Some(self.pace.every.unwrap_or(DEFAULT_INTERVAL))
    }

    fn describe(&self) -> String {
        format!("MJPEG file {}", self.path.display())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn encoded(colour: [u8; 3]) -> Vec<u8> {
        let image = image::RgbImage::from_pixel(16, 8, image::Rgb(colour));
        let mut out = std::io::Cursor::new(vec![]);
        image.write_to(&mut out, image::ImageFormat::Jpeg).unwrap();
        out.into_inner()
    }

    fn input(path: &Path, looped: bool) -> FileInput {
        FileInput {
            path: path.to_str().unwrap().into(),
            fps: None,
            looped,
        }
    }

    #[test]
    fn jpeg_end_skips_a_thumbnail_in_metadata() {
        let inner = encoded([1, 2, 3]);
        let outer = encoded([200, 10, 10]);
        // An APP1 segment holding a whole JPEG, right after SOI.
        let mut data = vec![0xFF, 0xD8, 0xFF, 0xE1];
        data.extend(((inner.len() + 2) as u16).to_be_bytes());
        data.extend(&inner);
        data.extend(&outer[2..]);
        assert_eq!(jpeg_end(&data).unwrap(), Some(data.len()));
        assert_eq!(jpeg_end(&data[..data.len() - 1]).unwrap(), None);
    }

    #[test]
    fn an_mjpeg_file_gives_its_frames_and_loops() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v.mjpeg");
        let colours = [[250, 0, 0], [0, 250, 0], [0, 0, 250]];
        let data: Vec<u8> = colours.iter().flat_map(|c| encoded(*c)).collect();
        std::fs::write(&path, data).unwrap();
        let mut s = MjpegFile::open(&input(&path, true), &path).unwrap();
        for i in 0..7 {
            let f = s.next_frame().unwrap().unwrap().frame;
            let want = colours[i % 3];
            assert!(
                f.rgb()[..3]
                    .iter()
                    .zip(want)
                    .all(|(a, b)| a.abs_diff(b) < 12),
                "frame {i}: {:?}",
                &f.rgb()[..3]
            );
        }
        let mut once = MjpegFile::open(&input(&path, false), &path).unwrap();
        let mut n = 0;
        while once.next_frame().unwrap().is_some() {
            n += 1;
        }
        assert_eq!(n, 3);
    }

    #[test]
    fn a_folder_is_read_in_name_order() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("b.jpg"), encoded([0, 250, 0])).unwrap();
        std::fs::write(dir.path().join("notes.txt"), "x").unwrap();
        image::RgbImage::from_pixel(4, 2, image::Rgb([9, 8, 7]))
            .save(dir.path().join("a.png"))
            .unwrap();
        let mut s = ImageFolder::open(&input(dir.path(), false), dir.path()).unwrap();
        let first = s.next_frame().unwrap().unwrap().frame;
        assert_eq!((first.width(), &first.rgb()[..3]), (4, &[9, 8, 7][..]));
        assert_eq!(s.next_frame().unwrap().unwrap().frame.width(), 16);
        assert!(s.next_frame().unwrap().is_none());
    }

    #[test]
    fn an_empty_folder_and_garbage_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        assert!(ImageFolder::open(&input(dir.path(), false), dir.path()).is_err());
        let path = dir.path().join("x.mjpeg");
        std::fs::write(&path, b"not a jpeg").unwrap();
        let mut s = MjpegFile::open(&input(&path, false), &path).unwrap();
        s.fill().unwrap();
        assert!(s.next_frame().is_err());
    }
}
