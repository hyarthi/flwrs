//! The debug window (`winit` + `softbuffer`): the newest scene, drawn by
//! our own code at the window's real pixel size.

use super::compose::{Scene, View, compose};
use super::run::Control;
use softbuffer::{Context, Surface};
use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowId};

/// The newest scene from the frame loop; older unread ones are replaced.
#[derive(Default)]
pub(super) struct Mailbox(Mutex<Option<Scene>>);

impl Mailbox {
    pub(super) fn put(&self, scene: Scene) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(scene);
    }

    fn take(&self) -> Option<Scene> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

pub(super) fn event_loop() -> Result<EventLoop<()>, String> {
    EventLoop::<()>::with_user_event()
        .build()
        .map_err(|e| e.to_string())
}

struct Display {
    window: Rc<Window>,
    surface: Surface<Rc<Window>, Rc<Window>>,
}

struct App {
    mailbox: Arc<Mailbox>,
    control: Arc<Control>,
    display: Option<Display>,
    scene: Option<Scene>,
    view: View,
    error: Option<String>,
}

impl App {
    fn fail(&mut self, event_loop: &ActiveEventLoop, error: String) {
        self.error = Some(error);
        event_loop.exit();
    }

    fn title(&self) -> String {
        let paused = self.control.paused.load(Ordering::Acquire);
        format!(
            "flwrs vision debug{}",
            if paused { " (paused)" } else { "" }
        )
    }

    fn refresh(&self) {
        if let Some(d) = &self.display {
            d.window.set_title(&self.title());
            d.window.request_redraw();
        }
    }

    fn key(&mut self, event_loop: &ActiveEventLoop, key: &Key) {
        let toggle = |flag: &mut bool| *flag = !*flag;
        match key.as_ref() {
            Key::Named(NamedKey::Escape) => event_loop.exit(),
            Key::Named(NamedKey::Space) => {
                let paused = self.control.paused.load(Ordering::Acquire);
                self.control.paused.store(!paused, Ordering::Release);
            }
            Key::Named(NamedKey::ArrowRight) => self.step(),
            Key::Character(c) => match c.to_ascii_lowercase().as_str() {
                "q" => event_loop.exit(),
                "n" | "." => self.step(),
                "b" => toggle(&mut self.view.boxes),
                "l" => toggle(&mut self.view.labels),
                "k" => toggle(&mut self.view.keypoints),
                "m" => toggle(&mut self.view.map),
                "p" => toggle(&mut self.view.panel),
                "f" => toggle(&mut self.view.mirror),
                "z" => toggle(&mut self.view.rules),
                "r" => {
                    let on = self.control.record.load(Ordering::Acquire);
                    self.control.record.store(!on, Ordering::Release);
                }
                "h" | "?" => toggle(&mut self.view.guide),
                _ => return,
            },
            _ => return,
        }
        self.refresh();
    }

    fn step(&self) {
        self.control.paused.store(true, Ordering::Release);
        self.control.steps.fetch_add(1, Ordering::AcqRel);
    }

    fn draw(&mut self) -> Result<(), String> {
        let (Some(display), Some(scene)) = (&mut self.display, &self.scene) else {
            return Ok(());
        };
        let size = display.window.inner_size();
        let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else {
            return Ok(());
        };
        display.surface.resize(w, h).map_err(|e| e.to_string())?;
        let mut buffer = display.surface.buffer_mut().map_err(|e| e.to_string())?;
        let scale = display.window.scale_factor().round().max(1.0) as i32;
        compose(
            scene,
            &self.view,
            &mut buffer,
            w.get() as usize,
            h.get() as usize,
            scale,
        );
        buffer.present().map_err(|e| e.to_string())
    }
}

impl ApplicationHandler<()> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.display.is_some() {
            return;
        }
        let attributes = Window::default_attributes()
            .with_title(self.title())
            .with_inner_size(LogicalSize::new(1280.0, 720.0));
        let window = match event_loop.create_window(attributes) {
            Ok(window) => Rc::new(window),
            Err(e) => return self.fail(event_loop, format!("can't open a window: {e}")),
        };
        let surface =
            Context::new(window.clone()).and_then(|context| Surface::new(&context, window.clone()));
        match surface {
            Ok(surface) => self.display = Some(Display { window, surface }),
            Err(e) => self.fail(event_loop, format!("can't draw in the window: {e}")),
        }
    }

    fn user_event(&mut self, _: &ActiveEventLoop, _: ()) {
        if let Some(scene) = self.mailbox.take() {
            self.scene = Some(scene);
            if let Some(d) = &self.display {
                d.window.request_redraw();
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        logical_key,
                        state: ElementState::Pressed,
                        ..
                    },
                ..
            } => self.key(event_loop, &logical_key),
            WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => self.refresh(),
            WindowEvent::RedrawRequested => {
                if let Err(e) = self.draw() {
                    self.fail(event_loop, e);
                }
            }
            _ => {}
        }
    }
}

/// Runs the window on this (the main) thread until it's closed.
pub(super) fn show(
    event_loop: EventLoop<()>,
    mailbox: Arc<Mailbox>,
    control: Arc<Control>,
) -> Result<(), String> {
    event_loop.set_control_flow(ControlFlow::Wait);
    let mut app = App {
        mailbox,
        control,
        display: None,
        scene: None,
        view: View::default(),
        error: None,
    };
    event_loop.run_app(&mut app).map_err(|e| e.to_string())?;
    app.error.map_or(Ok(()), Err)
}
