//! Optional native Linux frontend: egui widgets and a wgpu/Vulkan surface over
//! the existing foreground runtime. Enable with `--features gui`, then `--gui`.
mod font;
mod graphics;
mod input;
mod view;

use crate::{
    app::Runtime,
    editor::Focus,
    input::{Key, KeyCode},
    ui::InputEvent,
};
use graphics::Graphics;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use view::View;
use winit::{
    application::ApplicationHandler,
    event::{ElementState, Ime, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy},
    keyboard::{Key as NativeKey, ModifiersState, NamedKey},
    window::{Window, WindowId},
};

pub(super) enum UserEvent {
    Repaint(Instant),
    GpuError(String),
}

pub fn run(runtime: Runtime) -> Result<(), Box<dyn std::error::Error>> {
    let events = EventLoop::<UserEvent>::with_user_event().build()?;
    let mut app = App {
        runtime,
        view: View::default(),
        graphics: None,
        proxy: events.create_proxy(),
        modifiers: ModifiersState::empty(),
        repaint: None,
        next_poll: Instant::now(),
        error: None,
    };
    events.run_app(&mut app)?;
    if app.runtime.editor.should_quit {
        app.runtime.save_session();
    }
    if let Some(error) = app.error {
        return Err(error.into());
    }
    Ok(())
}

struct App {
    runtime: Runtime,
    view: View,
    graphics: Option<Graphics>,
    proxy: EventLoopProxy<UserEvent>,
    modifiers: ModifiersState,
    repaint: Option<Instant>,
    next_poll: Instant,
    error: Option<String>,
}

impl App {
    fn redraw(&self) {
        if let Some(g) = &self.graphics {
            g.window.request_redraw();
        }
    }
    fn fail(&mut self, event_loop: &ActiveEventLoop, error: String) {
        self.error = Some(error);
        event_loop.exit();
    }
    fn key(&mut self, key: &NativeKey, text: Option<&str>) {
        if !self.view.focused || !self.view.preedit.is_empty() {
            return;
        }
        if self.view.confirm_close {
            if *key == NativeKey::Named(NamedKey::Escape) {
                self.view.confirm_close = false;
            }
            self.redraw();
            return;
        }
        let letter = match key {
            NativeKey::Character(s) => s.to_lowercase(),
            _ => String::new(),
        };
        let ctrl = self.modifiers.control_key();
        if ctrl && self.modifiers.shift_key() && letter == "v" {
            if let Some(text) = self
                .graphics
                .as_mut()
                .and_then(|g| g.input.clipboard_text())
            {
                self.runtime.handle_input(InputEvent::Paste(text));
            }
        } else if ctrl && self.modifiers.shift_key() && letter == "c" {
            // Modal yanks are available even after leaving a Visual selection.
            if let Some(text) = self.runtime.editor.clipboard_text()
                && let Some(graphics) = &mut self.graphics
            {
                graphics.input.set_clipboard_text(text);
                self.runtime.editor.message("Copied to system clipboard");
            }
        } else if ctrl && letter == "p" && self.runtime.editor.focus != Focus::Terminal {
            view::command(&mut self.runtime, "files");
        } else if ctrl && letter == "s" && self.runtime.editor.focus != Focus::Terminal {
            view::command(&mut self.runtime, "w");
        } else if ctrl
            && matches!(letter.as_str(), "=" | "+" | "-" | "0")
            && self.runtime.editor.focus != Focus::Terminal
        {
            self.view.font_size = match letter.as_str() {
                "-" => (self.view.font_size - 1.0).max(11.0),
                "0" => 15.0,
                _ => (self.view.font_size + 1.0).min(28.0),
            };
        } else {
            for key in input::keys(key, text, self.modifiers) {
                self.runtime.handle_input(InputEvent::Key(key));
            }
        }
        self.redraw();
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.graphics.is_some() {
            return;
        }
        let result = (|| -> Result<Graphics, Box<dyn std::error::Error>> {
            let window = Arc::new(
                event_loop.create_window(
                    Window::default_attributes()
                        .with_title("editor")
                        .with_inner_size(winit::dpi::LogicalSize::new(1440.0, 940.0))
                        .with_min_inner_size(winit::dpi::LogicalSize::new(640.0, 420.0)),
                )?,
            );
            window.set_ime_allowed(true);
            futures_lite::future::block_on(Graphics::new(window, self.proxy.clone()))
        })();
        match result {
            Ok(graphics) => {
                self.graphics = Some(graphics);
                self.redraw();
            }
            Err(error) => self.fail(event_loop, format!("cannot start GUI: {error}")),
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        let Some(graphics) = &mut self.graphics else {
            return;
        };
        if graphics.window.id() != id {
            return;
        }
        if graphics
            .input
            .on_window_event(&graphics.window, &event)
            .repaint
        {
            graphics.window.request_redraw();
        }
        match event {
            WindowEvent::CloseRequested => {
                self.view.request_close(&mut self.runtime);
                self.redraw();
            }
            WindowEvent::RedrawRequested => {
                self.repaint = None;
                match graphics.draw(&mut self.runtime, &mut self.view) {
                    Ok(true) => {
                        let label = &self.runtime.editor.active_slot().display_name;
                        graphics.window.set_title(&format!("{label} — editor"));
                        let rect = self.view.cursor_rect;
                        graphics.window.set_ime_cursor_area(
                            winit::dpi::LogicalPosition::new(rect.min.x as f64, rect.min.y as f64),
                            winit::dpi::LogicalSize::new(rect.width() as f64, rect.height() as f64),
                        );
                        self.runtime.start_background();
                    }
                    Ok(false) => self.repaint = Some(Instant::now() + Duration::from_millis(16)),
                    Err(error) => self.fail(event_loop, error),
                }
            }
            WindowEvent::Resized(size) => {
                graphics.resize(size);
                self.redraw();
            }
            WindowEvent::ScaleFactorChanged { .. } => {
                graphics.resize(graphics.window.inner_size());
                self.redraw();
            }
            WindowEvent::ModifiersChanged(modifiers) => self.modifiers = modifiers.state(),
            WindowEvent::KeyboardInput {
                event,
                is_synthetic: false,
                ..
            } if event.state == ElementState::Pressed => {
                self.key(&event.logical_key, event.text.as_deref());
            }
            WindowEvent::Ime(Ime::Preedit(text, _)) => {
                self.view.preedit = text;
                self.redraw();
            }
            WindowEvent::Ime(Ime::Commit(text)) if !self.view.confirm_close => {
                self.view.preedit.clear();
                for ch in text.chars() {
                    self.runtime.handle_input(InputEvent::Key(Key::char(ch)));
                }
                self.redraw();
            }
            WindowEvent::Ime(Ime::Disabled) => {
                self.view.preedit.clear();
                self.redraw();
            }
            WindowEvent::Focused(focused) => {
                self.view.focused = focused;
                self.view.preedit.clear();
                self.modifiers = ModifiersState::empty();
                self.redraw();
            }
            WindowEvent::DroppedFile(path) if !self.view.confirm_close => {
                self.runtime.editor.handle_key(Key::plain(KeyCode::Esc));
                match self.runtime.editor.open_path(&path) {
                    Ok(_) => self.runtime.editor.focus = Focus::Editor,
                    Err(error) => self.runtime.editor.message(error.to_string()),
                }
                self.redraw();
            }
            _ => {}
        }
        if self.runtime.editor.should_quit {
            event_loop.exit();
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Repaint(deadline) => {
                self.repaint = Some(self.repaint.map_or(deadline, |old| old.min(deadline)))
            }
            UserEvent::GpuError(error) => self.fail(event_loop, error),
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let now = Instant::now();
        if now >= self.next_poll {
            self.runtime.handle_input(InputEvent::Tick);
            self.next_poll = now + Duration::from_millis(25);
            if self.runtime.needs_redraw() {
                self.redraw();
            }
        }
        if self.repaint.is_some_and(|time| time <= now) {
            self.repaint = None;
            self.redraw();
        }
        if self.runtime.editor.should_quit {
            event_loop.exit();
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(
            self.repaint
                .map_or(self.next_poll, |time| time.min(self.next_poll)),
        ));
    }
}
