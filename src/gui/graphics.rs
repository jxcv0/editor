//! Wgpu owns only presentation. No terminal raw mode or escape output is used.
use super::{UserEvent, view::View};
use crate::app::Runtime;
use std::{sync::Arc, time::Instant};
use winit::{dpi::PhysicalSize, event_loop::EventLoopProxy, window::Window};

pub struct Graphics {
    pub window: Arc<Window>,
    pub input: egui_winit::State,
    pub context: egui::Context,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    renderer: egui_wgpu::Renderer,
    size: PhysicalSize<u32>,
}

impl Graphics {
    pub async fn new(
        window: Arc<Window>,
        proxy: EventLoopProxy<UserEvent>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN,
            ..Default::default()
        });
        let surface = instance.create_surface(window.clone())?;
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                compatible_surface: Some(&surface),
                ..Default::default()
            })
            .await?;
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("editor desktop"),
                ..Default::default()
            })
            .await?;
        let error_proxy = proxy.clone();
        device.on_uncaptured_error(Arc::new(move |error| {
            let _ = error_proxy.send_event(UserEvent::GpuError(error.to_string()));
        }));
        let lost_proxy = proxy.clone();
        device.set_device_lost_callback(move |reason, message| {
            if reason != wgpu::DeviceLostReason::Destroyed {
                let _ =
                    lost_proxy.send_event(UserEvent::GpuError(format!("device lost: {message}")));
            }
        });
        let size = window.inner_size();
        let mut config = surface
            .get_default_config(&adapter, size.width.max(1), size.height.max(1))
            .ok_or("no supported window surface configuration")?;
        if let Some(format) = surface
            .get_capabilities(&adapter)
            .formats
            .into_iter()
            .find(|f| !f.is_srgb())
        {
            config.format = format;
        }
        config.present_mode = wgpu::PresentMode::AutoVsync;
        surface.configure(&device, &config);
        let context = egui::Context::default();
        super::view::install_style(&context);
        context.options_mut(|options| options.zoom_with_keyboard = false);
        context.set_request_repaint_callback(move |request| {
            if let Some(deadline) = Instant::now().checked_add(request.delay) {
                let _ = proxy.send_event(UserEvent::Repaint(deadline));
            }
        });
        let input = egui_winit::State::new(
            context.clone(),
            egui::ViewportId::ROOT,
            window.as_ref(),
            Some(window.scale_factor() as f32),
            window.theme(),
            Some(device.limits().max_texture_dimension_2d as usize),
        );
        let renderer = egui_wgpu::Renderer::new(
            &device,
            config.format,
            egui_wgpu::RendererOptions::default(),
        );
        Ok(Self {
            window,
            input,
            context,
            surface,
            device,
            queue,
            config,
            renderer,
            size,
        })
    }

    pub fn resize(&mut self, size: PhysicalSize<u32>) {
        self.size = size;
        if size.width > 0 && size.height > 0 {
            self.config.width = size.width;
            self.config.height = size.height;
            self.surface.configure(&self.device, &self.config);
        }
    }

    /// False requests a retry after a transient surface error.
    pub fn draw(&mut self, runtime: &mut Runtime, view: &mut View) -> Result<bool, String> {
        if self.size.width == 0 || self.size.height == 0 {
            return Ok(true);
        }
        let frame = match self.surface.get_current_texture() {
            Ok(frame) => frame,
            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                self.resize(self.window.inner_size());
                return Ok(false);
            }
            Err(wgpu::SurfaceError::Timeout) => return Ok(false),
            Err(error) => return Err(format!("cannot acquire window surface: {error}")),
        };
        let _ = self.device.poll(wgpu::PollType::Poll);
        let mut raw = self.input.take_egui_input(&self.window);
        // Winit already sent these to the modal core. In particular Ctrl-V must
        // remain block selection, and Tab must never focus a toolbar button.
        raw.events.retain(|event| {
            !matches!(
                event,
                egui::Event::Key { .. }
                    | egui::Event::Text(_)
                    | egui::Event::Ime(_)
                    | egui::Event::Copy
                    | egui::Event::Cut
                    | egui::Event::Paste(_)
            )
        });
        let output = self.context.run(raw, |ctx| view.show(ctx, runtime));
        self.input
            .handle_platform_output(&self.window, output.platform_output);
        let jobs = self
            .context
            .tessellate(output.shapes, output.pixels_per_point);
        let screen = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [self.size.width, self.size.height],
            pixels_per_point: output.pixels_per_point,
        };
        for (id, delta) in &output.textures_delta.set {
            self.renderer
                .update_texture(&self.device, &self.queue, *id, delta);
        }
        let target = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("editor frame"),
            });
        let commands =
            self.renderer
                .update_buffers(&self.device, &self.queue, &mut encoder, &jobs, &screen);
        {
            let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("editor egui"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &target,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            self.renderer
                .render(&mut pass.forget_lifetime(), &jobs, &screen);
        }
        self.queue
            .submit(commands.into_iter().chain([encoder.finish()]));
        self.window.pre_present_notify();
        frame.present();
        for id in &output.textures_delta.free {
            self.renderer.free_texture(id);
        }
        Ok(true)
    }
}

#[cfg(test)]
#[path = "render_tests.rs"]
mod tests;
