//! Explicit GPU validation. Also works with Mesa's software Vulkan driver on
//! headless machines; no window server or user state is touched.
use super::*;
use crate::{
    buffer::Buffer,
    config::Config,
    editor::{Editor, Focus},
    git::{Document, align_diff},
    project::{ProjectEntry, ProjectEntryKind},
};
use std::{fs, io::Write, sync::mpsc, time::Duration};

#[test]
#[ignore = "requires a Vulkan adapter; writes desktop and diff screenshots to target/editor-validation"]
fn offscreen_desktop_and_diff_render() {
    futures_lite::future::block_on(async {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN,
            ..Default::default()
        });
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions::default())
            .await
            .expect("Vulkan adapter");
        eprintln!("offscreen adapter: {:?}", adapter.get_info());
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor::default())
            .await
            .unwrap();
        let errors = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = errors.clone();
        device.on_uncaptured_error(Arc::new(move |error| {
            sink.lock().unwrap().push(error.to_string())
        }));
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("orbit");
        fs::create_dir_all(root.join("src")).unwrap();
        let source = "//! A small space for ambitious ideas.\n\nuse std::time::{Duration, Instant};\n\n#[derive(Debug, Clone)]\nstruct Frame {\n    index: u64,\n    elapsed: Duration,\n}\n\nimpl Frame {\n    fn next(&self, started: Instant) -> Self {\n        Self {\n            index: self.index + 1,\n            elapsed: started.elapsed(),\n        }\n    }\n}\n\nfn main() {\n    let started = Instant::now();\n    let frame = Frame {\n        index: 0,\n        elapsed: Duration::ZERO,\n    };\n\n    // Keep the feedback loop short.\n    let next = frame.next(started);\n    println!(\"frame {} · {:?}\", next.index, next.elapsed);\n}\n";
        fs::write(root.join("src/main.rs"), source).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"orbit\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        let mut config = Config::default();
        config.tools.rust_analyzer.path = "/no-such-editor-test-tool".into();
        let mut editor = Editor::new(config, root.clone());
        editor.open_path(root.join("Cargo.toml")).unwrap();
        editor.open_path(root.join("src/main.rs")).unwrap();
        editor.discard_initial_scratch();
        editor.explorer.open = true;
        editor.focus = Focus::Editor;
        editor.active_pane_mut().cursor = crate::buffer::Pos::new(20, 4);
        editor.rust_analyzer_status = "ready".into();
        editor.git.snapshot = Some(crate::git::Snapshot {
            root: root.clone(),
            branch: "main".into(),
            ..Default::default()
        });
        let entry = |name: &str, directory: bool| ProjectEntry {
            path: root.join(name),
            relative_path: name.into(),
            depth: name.split('/').count(),
            kind: if directory {
                ProjectEntryKind::Directory
            } else {
                ProjectEntryKind::File
            },
        };
        editor.explorer.append_directory(
            &root,
            vec![
                entry("src", true),
                entry("Cargo.toml", false),
                entry("Cargo.lock", false),
                entry("README.md", false),
            ],
        );
        editor.explorer.expanded.insert(root.join("src"));
        editor.explorer.append_directory(
            &root.join("src"),
            vec![entry("src/main.rs", false), entry("src/lib.rs", false)],
        );
        let mut runtime = Runtime::with_state_root(editor, false, None).unwrap();
        let context = egui::Context::default();
        crate::gui::view::install_style(&context);
        let mut view = View::default();
        let size = wgpu::Extent3d {
            width: 1440,
            height: 940,
            depth_or_array_layers: 1,
        };
        let format = wgpu::TextureFormat::Rgba8Unorm;
        let mut renderer =
            egui_wgpu::Renderer::new(&device, format, egui_wgpu::RendererOptions::default());
        fs::create_dir_all("target/editor-validation").unwrap();
        let mut time = 0.0;
        for name in [
            "desktop",
            "diff",
            "ligatures",
            "ligatures-zoom",
            "unsaved-close",
        ] {
            if name == "diff" {
                let lines: Vec<_> = "diff --git a/src/main.rs b/src/main.rs\n--- a/src/main.rs\n+++ b/src/main.rs\n@@ -20,11 +20,14 @@\n fn main() {\n     let started = Instant::now();\n     let frame = Frame {\n-        index: 0,\n+        index: 1,\n         elapsed: Duration::ZERO,\n     };\n \n-    // Keep the feedback loop short.\n+    // Every frame is a new beginning.\n     let next = frame.next(started);\n+    if next.elapsed > Duration::from_millis(16) {\n+        eprintln!(\"frame budget exceeded\");\n+    }\n     println!(\"frame {} · {:?}\", next.index, next.elapsed);\n }".lines().map(str::to_owned).collect();
                runtime.editor.git.visible = true;
                runtime.editor.git.document = Some(Document {
                    title: "Unstaged · src/main.rs".into(),
                    rows: align_diff(&lines),
                    lines,
                });
            }
            if name.starts_with("ligatures") {
                runtime.editor.git.visible = false;
                runtime
                    .editor
                    .active_buffer_mut()
                    .replace_all(concat!(
                        "// FiraCode Nerd Font Mono: embedded, no system font required\n\n",
                        "fn compare(a: usize, b: usize) -> bool {\n",
                        "    a != b && a <= b || a >= b\n",
                        "}\n\n",
                        "// == === != !== <= >= => -> <-> --> <!-- -->\n",
                        "// :: .. ... ..= := ++ ** && || <| |> www\n",
                        "// Nerd symbols: \u{e0b0}  \u{f120}  \u{f07c}  \u{f0001}\n",
                        "// Unicode: café e\u{301} λ →\n\n",
                        "let result = (0..=10).map(|n| n != 5);\n",
                        "match result { Some(value) => value, _ => 0 }\n",
                    ))
                    .unwrap();
                let pane = runtime.editor.active_pane_mut();
                pane.cursor = crate::buffer::Pos::new(3, 6);
                pane.viewport_line = 0;
                pane.viewport_column = 0;
                runtime.editor.config.ui.relative_numbers = false;
                view.font_size = if name == "ligatures-zoom" { 26.0 } else { 18.0 };
            }
            if name == "unsaved-close" {
                runtime.editor.git.visible = false;
                runtime.editor.buffers[0].buffer = Buffer::from_unsaved_text("unsaved text");
                view.request_close(&mut runtime);
            }
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some(name),
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            for _ in 0..3 {
                time += 0.25;
                let output = context.run(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(size.width as f32, size.height as f32),
                        )),
                        time: Some(time),
                        ..Default::default()
                    },
                    |ctx| view.show(ctx, &mut runtime),
                );
                let jobs = context.tessellate(output.shapes, output.pixels_per_point);
                let screen = egui_wgpu::ScreenDescriptor {
                    size_in_pixels: [size.width, size.height],
                    pixels_per_point: output.pixels_per_point,
                };
                for (id, delta) in &output.textures_delta.set {
                    renderer.update_texture(&device, &queue, *id, delta);
                }
                let mut encoder = device.create_command_encoder(&Default::default());
                let commands =
                    renderer.update_buffers(&device, &queue, &mut encoder, &jobs, &screen);
                let target = texture.create_view(&Default::default());
                {
                    let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
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
                    renderer.render(&mut pass.forget_lifetime(), &jobs, &screen);
                }
                queue.submit(commands.into_iter().chain([encoder.finish()]));
                for id in &output.textures_delta.free {
                    renderer.free_texture(id);
                }
            }
            capture(
                &device,
                &queue,
                &texture,
                size,
                &format!("target/editor-validation/{name}.ppm"),
            );
        }
        assert!(
            errors.lock().unwrap().is_empty(),
            "wgpu validation: {:?}",
            errors.lock().unwrap()
        );
    });
}

fn capture(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    size: wgpu::Extent3d,
    path: &str,
) {
    let stride = (size.width * 4).div_ceil(256) * 256;
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("screenshot"),
        size: u64::from(stride) * u64::from(size.height),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&Default::default());
    encoder.copy_texture_to_buffer(
        texture.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(stride),
                rows_per_image: None,
            },
        },
        size,
    );
    queue.submit([encoder.finish()]);
    let (sender, receiver) = mpsc::channel();
    buffer
        .slice(..)
        .map_async(wgpu::MapMode::Read, move |result| {
            let _ = sender.send(result);
        });
    device
        .poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: Some(Duration::from_secs(20)),
        })
        .unwrap();
    receiver
        .recv_timeout(Duration::from_secs(20))
        .unwrap()
        .unwrap();
    let bytes = buffer.slice(..).get_mapped_range();
    let mut file = std::io::BufWriter::new(fs::File::create(path).unwrap());
    writeln!(file, "P6\n{} {}\n255", size.width, size.height).unwrap();
    let mut visible = 0;
    for row in bytes.chunks_exact(stride as usize) {
        for pixel in row[..size.width as usize * 4].as_chunks::<4>().0 {
            file.write_all(&pixel[..3]).unwrap();
            visible += usize::from(pixel[0] > 100 || pixel[1] > 100 || pixel[2] > 100);
        }
    }
    assert!(visible > 5000, "render must contain visible text");
}
