//! Detection on control frames only: the detector runs every n frames (or
//! sooner when the tracker asks), optionally on its own thread, while the
//! tracker carries the objects in between.

use crate::frame::{Frame, Rect};
use crate::graph::config::ControlConfig;
use crate::model::{Model, Timing};
use crate::results::Results;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};

/// A detector's finished run.
#[derive(Debug)]
pub struct Detection {
    pub results: Result<(Results, Timing), String>,
    /// The time of the frame it ran on: a background run is late.
    pub time: f64,
}

struct Job {
    frame: Frame,
    rect: Rect,
    time: f64,
}

enum Engine {
    Here(Box<Model>),
    Background {
        jobs: Sender<Job>,
        done: Receiver<Detection>,
        device: String,
    },
}

fn run(model: &mut Model, job: &Job) -> Detection {
    Detection {
        results: model
            .run(&job.frame, job.rect)
            .map(|r| (r.results, r.timing)),
        time: job.time,
    }
}

/// A detector on control frames.
pub struct Detector {
    engine: Engine,
    tracker_frames: u32,
    /// Frames since the last run started; `None` before the first.
    since: Option<u32>,
    /// A new track waits to be confirmed: run again at once.
    requested: bool,
    /// Immediate runs left before the next regular one, so a flicker that
    /// keeps starting new tracks can't turn every frame into a control frame.
    urgent_left: u32,
    urgent_runs: u32,
    /// A pose looks weak: run early, from halfway through the interval.
    suggested: bool,
    busy: bool,
    /// Wait for each run, so a run on a file repeats exactly.
    deterministic: bool,
}

impl Detector {
    /// With `background`, the model moves to a thread of its own, which ends
    /// when this value is dropped.
    pub fn new(
        model: Box<Model>,
        config: &ControlConfig,
        min_hits: u32,
        background: bool,
    ) -> Result<Self, String> {
        let engine = if background {
            let device = model.session().device().to_string();
            let (jobs, job_rx) = mpsc::channel::<Job>();
            let (done_tx, done) = mpsc::channel();
            std::thread::Builder::new()
                .name("cv-detector".into())
                .spawn(move || {
                    let mut model = model;
                    while let Ok(job) = job_rx.recv() {
                        if done_tx.send(run(&mut model, &job)).is_err() {
                            break;
                        }
                    }
                })
                .map_err(|e| format!("can't start the detector's thread: {e}"))?;
            Engine::Background { jobs, done, device }
        } else {
            Engine::Here(model)
        };
        Ok(Self {
            engine,
            tracker_frames: config.tracker_frames,
            since: None,
            requested: false,
            urgent_left: 0,
            urgent_runs: min_hits.saturating_sub(1),
            suggested: false,
            busy: false,
            deterministic: false,
        })
    }

    pub fn device(&self) -> String {
        match &self.engine {
            Engine::Here(m) => m.session().device().to_string(),
            Engine::Background { device, .. } => device.clone(),
        }
    }

    pub fn set_deterministic(&mut self, on: bool) {
        self.deterministic = on;
    }

    /// A track is waiting to be confirmed: run as soon as possible.
    pub fn request(&mut self) {
        self.requested = true;
    }

    /// Something looks off (a weak pose): run earlier than scheduled.
    pub fn suggest(&mut self) {
        self.suggested = true;
    }

    fn regular(&self) -> bool {
        self.since.is_none_or(|n| n >= self.tracker_frames)
    }

    fn due(&self) -> bool {
        let halfway = self.tracker_frames.div_ceil(2);
        self.regular()
            || (self.requested && self.urgent_left > 0)
            || (self.suggested && self.since.is_some_and(|n| n >= halfway))
    }

    /// One frame: starts a run if it's due, and gives a finished one.
    pub fn step(&mut self, frame: &Frame, rect: Rect, time: f64) -> Option<Detection> {
        let mut finished = None;
        if let Engine::Background { done, .. } = &self.engine {
            match done.try_recv() {
                Ok(d) => {
                    self.busy = false;
                    finished = Some(d);
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    self.busy = false;
                    finished = Some(Detection {
                        results: Err("the detector's thread stopped".into()),
                        time,
                    });
                }
            }
        }
        if !self.due() || self.busy {
            self.since = self.since.map(|n| n.saturating_add(1));
            return finished;
        }
        if self.regular() {
            self.urgent_left = self.urgent_runs;
        } else if self.requested {
            self.urgent_left = self.urgent_left.saturating_sub(1);
        }
        self.since = Some(1);
        self.requested = false;
        self.suggested = false;
        let job = Job {
            frame: frame.clone(),
            rect,
            time,
        };
        match &mut self.engine {
            Engine::Here(model) => Some(run(model, &job)),
            Engine::Background { jobs, done, .. } => {
                if jobs.send(job).is_err() {
                    return Some(Detection {
                        results: Err("the detector's thread stopped".into()),
                        time,
                    });
                }
                if self.deterministic {
                    return done.recv().ok();
                }
                self.busy = true;
                finished
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Runtime;
    use crate::graph::config::{Execution, ModelConfig};
    use crate::tensor::Output;
    use crate::testing;
    use std::collections::BTreeMap;

    fn detector(tracker_frames: u32) -> (tempfile::TempDir, Detector) {
        let dir = tempfile::tempdir().unwrap();
        let output = Output::new(vec![1, 2], vec![0.3, 0.7]).unwrap();
        let path = dir.path().join("m.onnx");
        std::fs::write(
            &path,
            testing::constant_outputs(&[1, 3, 8, 8], &[("o", &output)], &[], &[]),
        )
        .unwrap();
        let config: ModelConfig = serde_json::from_value(serde_json::json!({
            "model": "m.onnx",
            "pre": {"width": 8, "height": 8, "resize": "stretch", "color": "rgb", "dtype": "f32", "layout": "nchw"},
            "decode": {"type": "classify", "output": "o", "softmax": false, "topk": 1}
        }))
        .unwrap();
        let files = BTreeMap::from([("m.onnx".to_string(), path)]);
        let execution = Execution {
            runtime: Some(Runtime::available()[0]),
            ..Execution::default()
        };
        let (model, _) = Model::load(&config, &files, &execution, 1, None).unwrap();
        let control =
            serde_json::from_value(serde_json::json!({"tracker_frames": tracker_frames})).unwrap();
        (
            dir,
            Detector::new(Box::new(model), &control, 3, false).unwrap(),
        )
    }

    fn runs(d: &mut Detector, frames: u32, each: impl Fn(&mut Detector)) -> Vec<u32> {
        let frame = Frame::filled(8, 8, [1; 3]).unwrap();
        let mut out = vec![];
        for n in 0..frames {
            each(d);
            if d.step(&frame, frame.full(), f64::from(n)).is_some() {
                out.push(n);
            }
        }
        out
    }

    #[test]
    fn it_runs_every_interval() {
        let (_dir, mut d) = detector(5);
        assert_eq!(runs(&mut d, 13, |_| {}), [0, 5, 10]);
    }

    #[test]
    fn zero_tracker_frames_is_every_frame() {
        let (_dir, mut d) = detector(0);
        assert_eq!(runs(&mut d, 4, |_| {}), [0, 1, 2, 3]);
    }

    #[test]
    fn endless_requests_cant_make_every_frame_a_control_frame() {
        let (_dir, mut d) = detector(5);
        assert_eq!(
            runs(&mut d, 16, Detector::request),
            [0, 1, 2, 7, 8, 9, 14, 15]
        );
    }

    #[test]
    fn a_weak_pose_brings_the_run_forward_by_half() {
        let (_dir, mut d) = detector(6);
        assert_eq!(runs(&mut d, 14, Detector::suggest), [0, 3, 6, 9, 12]);
    }
}
