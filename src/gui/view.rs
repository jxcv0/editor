//! Native chrome around the shared editor presentation. The canvas is painted
//! as antialiased glyphs and geometry.
use super::font::FontRenderer;
use crate::{
    app::Runtime,
    command::CommandId,
    editor::{Focus, Mode},
    input::{Key, KeyCode},
    ui::{Canvas, Color, InputEvent},
};
use egui::{
    Align, Align2, Color32, FontId, Frame, Layout, Margin, Rect, RichText, Sense, Stroke, Vec2,
    pos2, vec2,
};

const BG: Color32 = Color32::from_rgb(12, 18, 22);
const SURFACE: Color32 = Color32::from_rgb(20, 29, 34);
const BORDER: Color32 = Color32::from_rgb(39, 54, 61);
const TEXT: Color32 = Color32::from_rgb(220, 228, 227);
const MUTED: Color32 = Color32::from_rgb(122, 143, 151);
const ACCENT: Color32 = Color32::from_rgb(139, 213, 182);
const ORANGE: Color32 = Color32::from_rgb(235, 179, 133);

pub fn install_style(ctx: &egui::Context) {
    super::font::install(ctx);
    let mut style = (*ctx.style()).clone();
    style.visuals = egui::Visuals::dark();
    style.visuals.panel_fill = BG;
    style.visuals.window_fill = SURFACE;
    style.visuals.extreme_bg_color = BG;
    style.visuals.override_text_color = Some(TEXT);
    style.visuals.selection.bg_fill = Color32::from_rgb(43, 69, 72);
    style.visuals.selection.stroke = Stroke::new(1.0_f32, ACCENT);
    style.visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, BORDER);
    style.visuals.widgets.inactive.weak_bg_fill = SURFACE;
    style.visuals.widgets.inactive.bg_fill = SURFACE;
    style.visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(35, 53, 60);
    style.visuals.widgets.active.weak_bg_fill = Color32::from_rgb(43, 69, 72);
    style.visuals.widgets.inactive.corner_radius = 6.into();
    style.spacing.button_padding = vec2(12.0, 8.0);
    style.spacing.item_spacing = vec2(8.0, 6.0);
    style
        .text_styles
        .insert(egui::TextStyle::Body, FontId::proportional(14.0));
    style
        .text_styles
        .insert(egui::TextStyle::Button, FontId::proportional(13.0));
    ctx.set_style(style);
}

pub struct View {
    pub font_size: f32,
    pub focused: bool,
    pub preedit: String,
    pub confirm_close: bool,
    pub cursor_rect: Rect,
    last_explorer_selection: Option<usize>,
    wheel: f32,
    drag_anchor: Option<(u64, crate::buffer::Pos)>,
    fonts: FontRenderer,
}
impl Default for View {
    fn default() -> Self {
        Self {
            font_size: 15.0,
            focused: true,
            preedit: String::new(),
            confirm_close: false,
            cursor_rect: Rect::NOTHING,
            last_explorer_selection: None,
            wheel: 0.0,
            drag_anchor: None,
            fonts: FontRenderer::default(),
        }
    }
}

/// Finish an edit before dispatching a toolbar command, just like leaving
/// Insert mode and typing the equivalent Ex/leader command.
pub fn command(runtime: &mut Runtime, command: &str) {
    normal(runtime);
    match command {
        "files" => runtime.editor.execute_command(CommandId::FindFiles),
        "grep" => runtime.editor.execute_command(CommandId::ProjectGrep),
        "explorer" => {
            runtime.editor.explorer.open = !runtime.editor.explorer.open;
            runtime.editor.focus = if runtime.editor.explorer.open {
                Focus::Explorer
            } else {
                Focus::Editor
            };
        }
        _ => runtime.editor.execute_ex(command),
    }
    runtime.handle_input(InputEvent::Tick);
}

fn normal(runtime: &mut Runtime) {
    runtime.editor.focus = Focus::Editor;
    runtime.editor.handle_key(Key::plain(KeyCode::Esc));
    if runtime.editor.mode != Mode::Normal {
        runtime.editor.handle_key(Key::plain(KeyCode::Esc));
    }
}

impl View {
    pub(super) fn request_close(&mut self, runtime: &mut Runtime) {
        normal(runtime);
        if runtime.editor.buffers.iter().any(|b| b.buffer.is_dirty()) {
            self.confirm_close = true;
        } else {
            command(runtime, "qa");
        }
    }

    pub fn show(&mut self, ctx: &egui::Context, runtime: &mut Runtime) {
        egui::TopBottomPanel::top("titlebar")
            .exact_height(58.0)
            .frame(
                Frame::new()
                    .fill(BG)
                    .inner_margin(Margin::symmetric(20, 10)),
            )
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.label(
                        RichText::new("e")
                            .font(FontId::monospace(26.0))
                            .strong()
                            .color(ACCENT),
                    );
                    ui.add_space(4.0);
                    ui.menu_button(RichText::new("editor").size(19.0).strong(), |ui| {
                        if ui.button("FiraCode font license").clicked() {
                            normal(runtime);
                            runtime.editor.open_scratch_text(
                                "FiraCode font license",
                                super::font::FONT_LICENSE,
                            );
                            ui.close();
                        }
                    });
                    ui.label(RichText::new("/").color(BORDER).size(22.0));
                    let root = runtime
                        .editor
                        .explorer
                        .root
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy();
                    ui.label(RichText::new(root).color(MUTED));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui
                            .button(RichText::new("Save").color(ACCENT))
                            .on_hover_text("Save current file · Ctrl-S")
                            .clicked()
                        {
                            command(runtime, "w");
                        }
                        if ui
                            .button("Terminal")
                            .on_hover_text("Toggle shell · Ctrl-`")
                            .clicked()
                        {
                            command(runtime, "terminal");
                        }
                        ui.menu_button("Cargo", |ui| {
                            for name in [
                                "run", "check", "test", "build", "clippy", "fmt", "doc", "update",
                                "clean", "cancel",
                            ] {
                                if ui.button(format!("cargo {name}")).clicked() {
                                    command(runtime, &format!("cargo {name}"));
                                    ui.close();
                                }
                            }
                        });
                        if ui.button("Find file   Ctrl P").clicked() {
                            command(runtime, "files");
                        }
                    });
                });
            });
        egui::SidePanel::left("activity")
            .exact_width(54.0)
            .resizable(false)
            .frame(Frame::new().fill(BG).inner_margin(Margin::symmetric(7, 14)))
            .show(ctx, |ui| {
                for (icon, hint, action, selected) in [
                    (
                        0,
                        "Explorer · Space e",
                        "explorer",
                        runtime.editor.explorer.open,
                    ),
                    (1, "Search project · Space /", "grep", false),
                    (
                        2,
                        "Git status · Space g s",
                        "git",
                        runtime.editor.git.visible,
                    ),
                    (
                        3,
                        "Integrated terminal · Ctrl-`",
                        "terminal",
                        runtime.editor.terminal.visible,
                    ),
                ] {
                    let response = ui
                        .add_sized([40.0, 42.0], egui::Button::new("").selected(selected))
                        .on_hover_text(hint);
                    paint_icon(
                        ui.painter(),
                        response.rect.center(),
                        icon,
                        if selected { ACCENT } else { MUTED },
                    );
                    if response.clicked() {
                        command(runtime, action);
                    }
                    ui.add_space(10.0);
                }
                ui.with_layout(Layout::bottom_up(Align::Center), |ui| {
                    if ui
                        .button("?")
                        .on_hover_text("Open command menu · Space")
                        .clicked()
                    {
                        normal(runtime);
                        runtime.handle_input(InputEvent::Key(Key::char(' ')));
                    }
                });
            });
        if runtime.editor.explorer.open {
            self.explorer(ctx, runtime);
        }
        egui::CentralPanel::default()
            .frame(Frame::new().fill(BG).inner_margin(Margin {
                left: 8,
                right: 14,
                top: 0,
                bottom: 12,
            }))
            .show(ctx, |ui| {
                self.tabs(ui, runtime);
                ui.add_space(1.0);
                ui.horizontal(|ui| {
                    let path = runtime
                        .editor
                        .active_buffer()
                        .path()
                        .map(|p| {
                            p.strip_prefix(&runtime.editor.explorer.root)
                                .unwrap_or(p)
                                .display()
                                .to_string()
                        })
                        .unwrap_or_else(|| runtime.editor.active_slot().display_name.clone());
                    ui.label(
                        RichText::new(path.replace('/', "  /  "))
                            .size(12.0)
                            .color(MUTED),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui
                            .small_button("Split")
                            .on_hover_text("Split right · :vsplit")
                            .clicked()
                        {
                            command(runtime, "vsplit");
                        }
                        if ui
                            .small_button("Changes")
                            .on_hover_text("Saved-file changes · Space g d")
                            .clicked()
                        {
                            command(runtime, "git diff");
                        }
                    });
                });
                ui.add_space(8.0);
                self.workspace(ui, runtime);
            });
        if self.confirm_close {
            egui::Modal::new("unsaved-close".into()).show(ctx, |ui| {
                ui.set_width(390.0);
                ui.heading("Keep your changes?");
                ui.add_space(8.0);
                let count = runtime.editor.buffers.iter().filter(|b| b.buffer.is_dirty()).count();
                ui.label(format!("{count} buffer(s) have unsaved changes. Return to the editor to save them, or discard them and close."));
                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    if ui.button(RichText::new("Keep editing").color(ACCENT)).clicked() { self.confirm_close = false; }
                    if ui.button("Discard and close").clicked() { command(runtime, "qa!"); }
                });
            });
        }
    }

    fn explorer(&mut self, ctx: &egui::Context, runtime: &mut Runtime) {
        let initial_width = f32::from(runtime.editor.explorer.width) * 8.0;
        let response = egui::SidePanel::left("project-tree")
            .default_width(initial_width)
            .width_range(160.0..=440.0)
            .resizable(true)
            .frame(Frame::new().fill(BG).inner_margin(Margin {
                left: 10,
                right: 12,
                top: 10,
                bottom: 16,
            }))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("EXPLORER").size(11.0).strong().color(MUTED));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.small_button("×").clicked() {
                            command(runtime, "explorer");
                        }
                    });
                });
                ui.add_space(16.0);
                ui.label(
                    RichText::new(
                        runtime
                            .editor
                            .explorer
                            .root
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy(),
                    )
                    .strong()
                    .color(ACCENT),
                );
                ui.add_space(10.0);
                let count = runtime.editor.explorer.rows().len();
                let selected = runtime.editor.explorer.selected;
                let scroll_to_selected = self.last_explorer_selection != Some(selected);
                self.last_explorer_selection = Some(selected);
                let height = (ui.available_height() - 100.0).max(80.0);
                let mut scroll = egui::ScrollArea::vertical()
                    .max_height(height)
                    .auto_shrink([false, false]);
                if scroll_to_selected {
                    scroll = scroll
                        .vertical_scroll_offset((selected as f32 * 35.0 - height / 2.0).max(0.0));
                }
                scroll.show_rows(ui, 29.0, count, |ui, range| {
                    for i in range {
                        let entry = &runtime.editor.explorer.rows()[i];
                        let directory = entry.is_directory();
                        let expanded = runtime.editor.explorer.expanded.contains(&entry.path);
                        let label = entry.path.file_name().unwrap_or_default().to_string_lossy();
                        let marker = if directory {
                            if expanded { "v" } else { ">" }
                        } else {
                            file_mark(&entry.path)
                        };
                        let text = format!(
                            "{}{}  {}",
                            "  ".repeat(entry.depth.saturating_sub(1).min(20)),
                            marker,
                            label
                        );
                        let active =
                            runtime.editor.active_buffer().path() == Some(entry.path.as_path());
                        let (rect, response) = ui
                            .allocate_exact_size(vec2(ui.available_width(), 29.0), Sense::click());
                        if active || (i == selected && runtime.editor.focus == Focus::Explorer) {
                            ui.painter()
                                .rect_filled(rect, 5.0, Color32::from_rgb(35, 56, 61));
                        } else if response.hovered() {
                            ui.painter().rect_filled(rect, 5.0, SURFACE);
                        }
                        ui.painter().with_clip_rect(rect).text(
                            rect.left_center() + vec2(8.0, 0.0),
                            Align2::LEFT_CENTER,
                            text,
                            FontId::proportional(13.0),
                            if active {
                                ACCENT
                            } else if directory {
                                TEXT
                            } else {
                                MUTED
                            },
                        );
                        if i == selected && scroll_to_selected {
                            response.scroll_to_me(Some(Align::Center));
                        }
                        if response.clicked() {
                            normal(runtime);
                            runtime.editor.explorer.selected = i;
                            runtime.editor.focus = Focus::Explorer;
                            runtime.handle_input(InputEvent::Key(Key::plain(KeyCode::Enter)));
                        }
                    }
                });
            });
        runtime.editor.explorer.width = (response.response.rect.width() / 8.0)
            .round()
            .clamp(16.0, 120.0) as u16;
    }

    fn tabs(&self, ui: &mut egui::Ui, runtime: &mut Runtime) {
        Frame::new()
            .fill(SURFACE)
            .corner_radius(egui::CornerRadius {
                nw: 9,
                ne: 9,
                sw: 0,
                se: 0,
            })
            .inner_margin(Margin::symmetric(7, 5))
            .show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                egui::ScrollArea::horizontal()
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            for i in 0..runtime.editor.buffers.len() {
                                let slot = &runtime.editor.buffers[i];
                                let label = slot
                                    .buffer
                                    .path()
                                    .and_then(|p| p.file_name())
                                    .map(|p| p.to_string_lossy().into_owned())
                                    .unwrap_or_else(|| slot.display_name.clone());
                                let active = runtime.editor.active_pane().buffer == i;
                                let marker = slot.buffer.path().map_or("·", file_mark);
                                let label = format!(
                                    "{marker}  {label}{}",
                                    if slot.buffer.is_dirty() { "  •" } else { "" }
                                );
                                let response = ui.add(
                                    egui::Button::new(RichText::new(label).color(if active {
                                        TEXT
                                    } else {
                                        MUTED
                                    }))
                                    .selected(active)
                                    .min_size(vec2(100.0, 30.0)),
                                );
                                if active {
                                    let rect = response.rect;
                                    ui.painter().line_segment(
                                        [
                                            pos2(rect.left() + 10.0, rect.bottom() + 3.0),
                                            pos2(rect.right() - 10.0, rect.bottom() + 3.0),
                                        ],
                                        Stroke::new(2.0_f32, ACCENT),
                                    );
                                }
                                if response.clicked() {
                                    normal(runtime);
                                    runtime.editor.switch_buffer(i);
                                }
                            }
                        });
                    });
            });
    }

    fn workspace(&mut self, ui: &mut egui::Ui, runtime: &mut Runtime) {
        let font = FontId::monospace(self.font_size);
        let cell = vec2(
            ui.fonts_mut(|f| f.glyph_width(&font, 'M')).max(1.0),
            (self.font_size * 1.55).ceil(),
        );
        let (rect, response) = ui.allocate_exact_size(
            ui.available_size().max(vec2(1.0, 1.0)),
            Sense::click_and_drag(),
        );
        let origin = rect.min + vec2(8.0, 8.0);
        let columns = ((rect.width() - 16.0) / cell.x).floor().clamp(1.0, 512.0) as u16;
        let rows = ((rect.height() - 16.0) / cell.y).floor().clamp(2.0, 256.0) as u16;
        let (canvas, cursor) = runtime.gui_frame(columns, rows);
        let painter = ui.painter().with_clip_rect(rect);
        let theme = &runtime.editor.config.ui.theme;
        let background = crate::config::parse_hex_color(&theme.background)
            .map(|(r, g, b)| Color32::from_rgb(r, g, b))
            .unwrap_or(SURFACE);
        painter.rect_filled(rect, 9.0, background);
        paint_canvas(
            &mut self.fonts,
            &painter,
            &canvas,
            origin,
            cell,
            &font,
            !runtime.editor.git.visible,
        );
        if !runtime.editor.git.visible {
            let status = Rect::from_min_size(
                pos2(origin.x, origin.y + f32::from(rows - 2) * cell.y),
                vec2(f32::from(columns) * cell.x, cell.y),
            );
            painter.rect_filled(status, 3.0, SURFACE);
            let mode = runtime.editor.mode.label();
            let color = if runtime.editor.mode == Mode::Insert {
                ORANGE
            } else {
                ACCENT
            };
            let pill = Rect::from_min_size(status.min + vec2(4.0, 2.0), vec2(83.0, cell.y - 4.0));
            painter.rect_filled(pill, 4.0, color.gamma_multiply(0.15));
            painter.text(
                pill.center(),
                Align2::CENTER_CENTER,
                mode,
                FontId::monospace(11.0),
                color,
            );
            let branch = runtime
                .editor
                .git
                .snapshot
                .as_ref()
                .map_or("local", |s| s.branch.as_str());
            painter.text(
                status.min + vec2(100.0, cell.y / 2.0),
                Align2::LEFT_CENTER,
                branch,
                FontId::proportional(12.0),
                MUTED,
            );
            let pos = runtime.editor.active_pane().cursor;
            let flags = if runtime.editor.active_buffer().is_read_only() {
                "  READ ONLY"
            } else if runtime.editor.active_slot().large_file {
                "  LARGE"
            } else {
                ""
            };
            let info = format!(
                "{}:{}{flags}     UTF-8     {}",
                pos.line + 1,
                pos.grapheme + 1,
                runtime.editor.rust_analyzer_status
            );
            painter.text(
                status.right_center() - vec2(10.0, 0.0),
                Align2::RIGHT_CENTER,
                info,
                FontId::monospace(11.0),
                MUTED,
            );
        }
        self.cursor_rect = Rect::from_min_size(origin, cell);
        if let Some((x, y)) = cursor {
            let caret = Rect::from_min_size(
                origin + vec2(f32::from(x) * cell.x, f32::from(y) * cell.y),
                cell,
            );
            self.cursor_rect = caret;
            if self.focused && !self.confirm_close {
                let thin = matches!(
                    runtime.editor.mode,
                    Mode::Insert | Mode::Command | Mode::Search { .. }
                ) || runtime.editor.focus == Focus::Terminal;
                if thin {
                    painter.rect_filled(
                        Rect::from_min_size(caret.min, vec2(2.0, cell.y)),
                        1.0,
                        ACCENT,
                    );
                } else {
                    painter.rect_filled(caret, 2.0, ACCENT.gamma_multiply(0.3));
                    painter.rect_stroke(
                        caret,
                        2.0,
                        Stroke::new(1.0_f32, ACCENT),
                        egui::StrokeKind::Inside,
                    );
                }
                if !self.preedit.is_empty() {
                    let text = painter.layout_no_wrap(self.preedit.clone(), font.clone(), TEXT);
                    let preedit =
                        Rect::from_min_size(caret.left_bottom(), text.size() + vec2(8.0, 6.0));
                    painter.rect_filled(preedit, 4.0, SURFACE);
                    painter.galley(preedit.min + vec2(4.0, 3.0), text, TEXT);
                    painter.line_segment(
                        [preedit.left_bottom(), preedit.right_bottom()],
                        Stroke::new(1.0_f32, ACCENT),
                    );
                }
            }
        }
        if response.clicked()
            && !self.confirm_close
            && let Some(point) = response.interact_pointer_pos()
        {
            let at = point - origin;
            runtime.pointer(
                (at.x / cell.x).max(0.0) as u16,
                (at.y / cell.y).max(0.0) as u16,
            );
        }
        if response.drag_started()
            && !self.confirm_close
            && let Some(point) = ui.input(|input| input.pointer.press_origin())
        {
            let at = point - origin;
            runtime.pointer(
                (at.x / cell.x).max(0.0) as u16,
                (at.y / cell.y).max(0.0) as u16,
            );
            if runtime.editor.focus == Focus::Editor
                && !runtime.editor.git.visible
                && runtime.editor.picker.is_none()
            {
                self.drag_anchor = Some((
                    runtime.editor.active_pane,
                    runtime.editor.active_pane().cursor,
                ));
            }
        }
        if response.dragged()
            && !self.confirm_close
            && let Some((pane, anchor)) = self.drag_anchor
            && let Some(point) = response.interact_pointer_pos()
        {
            let at = point - origin;
            runtime.drag_pointer(
                (at.x / cell.x).max(0.0) as u16,
                (at.y / cell.y).max(0.0) as u16,
                pane,
                anchor,
            );
        }
        if response.drag_stopped() {
            self.drag_anchor = None;
        }
        if response.hovered() && !self.confirm_close {
            self.wheel += ui.input(|input| input.raw_scroll_delta.y);
            let steps = (self.wheel / cell.y).trunc() as i32;
            if steps != 0 {
                self.wheel -= steps as f32 * cell.y;
                if runtime.editor.focus == Focus::Terminal {
                    for _ in 0..steps.unsigned_abs().min(12) {
                        if steps > 0 {
                            runtime.editor.terminal.scroll_up();
                        } else {
                            runtime.editor.terminal.scroll_down();
                        }
                    }
                } else {
                    let key = if steps > 0 {
                        KeyCode::Up
                    } else {
                        KeyCode::Down
                    };
                    for _ in 0..steps.unsigned_abs().min(32) {
                        runtime.handle_input(InputEvent::Key(Key::plain(key)));
                    }
                }
                ui.ctx().request_repaint();
            }
        }
        if runtime.needs_redraw() {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(16));
        }
    }
}

fn file_mark(path: &std::path::Path) -> &'static str {
    match path.extension().and_then(|s| s.to_str()) {
        Some("rs") => "rs",
        Some("toml") => "{}",
        Some("md") => "#",
        _ => "·",
    }
}
fn color(Color(r, g, b): Color) -> Color32 {
    Color32::from_rgb(r, g, b)
}

fn paint_canvas(
    fonts: &mut FontRenderer,
    painter: &egui::Painter,
    canvas: &Canvas,
    origin: egui::Pos2,
    cell: Vec2,
    font: &FontId,
    native_status: bool,
) {
    // Merge adjacent background cells into strips; glyphs keep exact Unicode
    // cell advances so split panes, inlays and the PTY share hit coordinates.
    let mut background: Option<(u16, u16, u16, Color32)> = None;
    let flush = |value: Option<(u16, u16, u16, Color32)>| {
        if let Some((x, y, width, bg)) = value {
            painter.rect_filled(
                Rect::from_min_size(
                    origin + vec2(f32::from(x) * cell.x, f32::from(y) * cell.y),
                    vec2(f32::from(width) * cell.x, cell.y),
                ),
                0.0,
                bg,
            );
        }
    };
    for (x, y, _, style, _) in canvas.cells() {
        if native_status && y == canvas.height.saturating_sub(2) {
            continue;
        }
        let bg = color(if style.reverse { style.fg } else { style.bg });
        match &mut background {
            Some((start, row, width, old)) if *row == y && *start + *width == x && *old == bg => {
                *width += 1
            }
            _ => {
                flush(background.take());
                background = Some((x, y, 1, bg));
            }
        }
    }
    flush(background);
    for TextRun { x, y, text, style } in text_runs(canvas, native_status) {
        let at = origin + vec2(f32::from(x) * cell.x, f32::from(y) * cell.y + cell.y / 2.0);
        let fg = color(if style.reverse { style.bg } else { style.fg });
        if text.is_ascii() {
            fonts.paint(painter, &text, at, font, cell.x, fg);
        } else {
            painter.text(at, Align2::LEFT_CENTER, &text, font.clone(), fg);
        }
        if style.underline {
            let width = unicode_width::UnicodeWidthStr::width(text.as_ref()).max(1) as f32;
            painter.line_segment(
                [
                    at + vec2(0.0, cell.y / 2.0 - 2.0),
                    at + vec2(cell.x * width, cell.y / 2.0 - 2.0),
                ],
                Stroke::new(1.0_f32, fg),
            );
        }
    }
}

struct TextRun<'a> {
    x: u16,
    y: u16,
    text: std::borrow::Cow<'a, str>,
    style: crate::ui::Style,
}

/// Shape adjacent ASCII cells together, but never across rows, Unicode/wide
/// cells, colors, selections or decorations. All origins remain canvas cells.
fn text_runs(canvas: &Canvas, native_status: bool) -> Vec<TextRun<'_>> {
    let mut cells = canvas.cells().peekable();
    let mut runs = Vec::new();
    let ascii_cell = |s: &str| s.len() == 1 && (s.as_bytes()[0].is_ascii_graphic() || s == " ");
    while let Some((x, y, text, style, continuation)) = cells.next() {
        if continuation || (native_status && y == canvas.height.saturating_sub(2)) {
            continue;
        }
        let text = if ascii_cell(text) {
            let mut text = text.to_owned();
            while let Some(&(next_x, next_y, next_text, next_style, next_continuation)) =
                cells.peek()
            {
                if next_y != y
                    || usize::from(next_x) != usize::from(x) + text.len()
                    || next_continuation
                    || next_style != style
                    || !ascii_cell(next_text)
                {
                    break;
                }
                text.push_str(next_text);
                cells.next();
            }
            std::borrow::Cow::Owned(text)
        } else {
            std::borrow::Cow::Borrowed(text)
        };
        if text.trim().is_empty() && !style.underline {
            continue;
        }
        runs.push(TextRun { x, y, text, style });
    }
    runs
}

/// Small vector icons avoid dependence on a user's installed symbol fonts.
fn paint_icon(painter: &egui::Painter, center: egui::Pos2, icon: u8, color: Color32) {
    let stroke = Stroke::new(1.5_f32, color);
    let p = |x, y| center + vec2(x, y);
    let line = |a, b| {
        painter.line_segment([a, b], stroke);
    };
    match icon {
        0 => {
            painter.rect_stroke(
                Rect::from_center_size(center, vec2(17.0, 20.0)),
                2.0,
                stroke,
                egui::StrokeKind::Inside,
            );
            for y in [-4.0, 0.0, 4.0] {
                line(p(-4.0, y), p(4.0, y));
            }
        }
        1 => {
            painter.circle_stroke(p(-2.0, -2.0), 6.0, stroke);
            line(p(3.0, 3.0), p(9.0, 9.0));
        }
        2 => {
            line(p(-5.0, -6.0), p(-5.0, 6.0));
            line(p(-5.0, 2.0), p(5.0, -2.0));
            line(p(5.0, -2.0), p(5.0, -6.0));
            for (x, y) in [(-5.0, -8.0), (-5.0, 8.0), (5.0, -8.0)] {
                painter.circle_stroke(p(x, y), 2.0, stroke);
            }
        }
        _ => {
            line(p(-7.0, -5.0), p(-2.0, 0.0));
            line(p(-2.0, 0.0), p(-7.0, 5.0));
            line(p(1.0, 5.0), p(8.0, 5.0));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        buffer::Buffer,
        config::Config,
        editor::{Editor, VisualKind},
        input::Modifiers,
    };

    fn runtime(root: &std::path::Path) -> Runtime {
        let mut config = Config::default();
        config.tools.rust_analyzer.path = "/no-such-editor-test-rust-analyzer".into();
        Runtime::with_state_root(Editor::new(config, root.to_owned()), false, None).unwrap()
    }

    #[test]
    fn ligature_runs_respect_styles_unicode_and_canvas_boundaries() {
        let style = crate::ui::Style::new(Color(255, 255, 255), Color(0, 0, 0));
        let mut canvas = Canvas::new(8, 4, style);
        canvas.text(0, 0, "a!=b", 8, style);
        canvas.text(0, 1, "!=", 8, style);
        canvas.put_grapheme(1, 1, "=", style.reverse());
        canvas.text(0, 2, "!=界->", 8, style);
        canvas.text(0, 3, "e\u{301}->", 8, style);
        let runs = text_runs(&canvas, false);
        let actual: Vec<_> = runs.iter().map(|r| (r.x, r.y, r.text.trim_end())).collect();
        assert_eq!(
            actual,
            vec![
                (0, 0, "a!=b"),
                (0, 1, "!"),
                (1, 1, "="),
                (0, 2, "!="),
                (2, 2, "界"),
                (4, 2, "->"),
                (0, 3, "e\u{301}"),
                (1, 3, "->"),
            ]
        );
        assert!(text_runs(&canvas, true).iter().all(|r| r.y != 2));
    }

    #[test]
    fn ligatures_keep_individual_cursor_and_mouse_cells() {
        let root = tempfile::tempdir().unwrap();
        let mut runtime = runtime(root.path());
        runtime.editor.config.ui.cursor_animation = false;
        runtime.editor.config.ui.smooth_scroll = false;
        runtime.editor.buffers[0].buffer = Buffer::from_text("a!=b -> c");
        let ctx = egui::Context::default();
        install_style(&ctx);
        let mut view = View::default();
        let raw = || egui::RawInput {
            screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, vec2(1000.0, 640.0))),
            ..Default::default()
        };
        let mut first = Rect::NOTHING;
        for column in 0..4 {
            let _ = ctx.run(raw(), |ctx| view.show(ctx, &mut runtime));
            if column == 0 {
                first = view.cursor_rect;
            }
            assert_eq!(runtime.editor.active_pane().cursor.grapheme, column);
            assert!(
                (view.cursor_rect.left() - first.left() - column as f32 * first.width()).abs()
                    < 0.01
            );
            runtime.handle_input(InputEvent::Key(Key::char('l')));
        }
        // Click the second character of !=, not just the start of its glyph.
        let point = first.center() + vec2(first.width() * 2.0, 0.0);
        for pressed in [true, false] {
            let mut input = raw();
            input.events = vec![
                egui::Event::PointerMoved(point),
                egui::Event::PointerButton {
                    pos: point,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                },
            ];
            let _ = ctx.run(input, |ctx| view.show(ctx, &mut runtime));
        }
        assert_eq!(runtime.editor.active_pane().cursor.grapheme, 2);
        assert_eq!(runtime.editor.active_buffer().text(), "a!=b -> c");
    }

    #[test]
    fn window_close_finishes_insert_and_protects_all_dirty_buffers() {
        let root = tempfile::tempdir().unwrap();
        let mut runtime = runtime(root.path());
        let mut view = View::default();
        for ch in "iunsaved".chars() {
            runtime.editor.handle_key(Key::char(ch));
        }
        view.request_close(&mut runtime);
        assert!(view.confirm_close);
        assert!(!runtime.editor.should_quit);
        assert!(!runtime.editor.active_buffer().in_transaction());
        assert_eq!(runtime.editor.active_buffer().text(), "unsaved");
        // Cancelling the dialog and opening another buffer must still protect
        // the now-hidden modified buffer on a subsequent close request.
        view.confirm_close = false;
        runtime.editor.open_scratch_text("clean", "");
        view.request_close(&mut runtime);
        assert!(view.confirm_close && !runtime.editor.should_quit);
        command(&mut runtime, "qa!");
        assert!(runtime.editor.should_quit);
    }

    #[test]
    fn toolbar_save_uses_core_transactions_and_conflict_checks() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("test.rs");
        std::fs::write(&path, "original").unwrap();
        let mut runtime = runtime(root.path());
        runtime.editor.open_path(&path).unwrap();
        for ch in "A edited".chars() {
            runtime.editor.handle_key(Key::char(ch));
        }
        command(&mut runtime, "w");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "original edited");
        runtime.editor.handle_key(Key::char('u'));
        assert_eq!(runtime.editor.active_buffer().text(), "original");
        std::fs::write(&path, "external update").unwrap();
        command(&mut runtime, "w");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "external update");
        assert!(runtime.editor.active_buffer().is_dirty());
    }

    #[test]
    fn clipboard_handles_block_selection_without_changing_modal_state() {
        let root = tempfile::tempdir().unwrap();
        let mut runtime = runtime(root.path());
        runtime.editor.buffers[0].buffer = Buffer::from_text("abc\ndef");
        runtime.editor.handle_key(Key::ctrl('v'));
        runtime.editor.handle_key(Key::char('l'));
        runtime.editor.handle_key(Key::char('j'));
        assert_eq!(runtime.editor.clipboard_text().as_deref(), Some("ab\nde"));
        assert_eq!(runtime.editor.mode, Mode::Visual(VisualKind::Block));
        assert_eq!(Modifiers::CONTROL, Key::ctrl('v').modifiers);
    }

    #[test]
    fn native_ui_tessellates_at_small_large_and_scaled_sizes() {
        let root = tempfile::tempdir().unwrap();
        let mut runtime = runtime(root.path());
        runtime.editor.buffers[0].buffer =
            Buffer::from_text("fn main() {\n    println!(\"Hello, 界\");\n}\n");
        let ctx = egui::Context::default();
        install_style(&ctx);
        let mut view = View::default();
        for (width, height, scale) in [
            (640.0, 420.0, 1.0),
            (1440.0, 940.0, 1.0),
            (900.0, 640.0, 2.0),
        ] {
            ctx.set_pixels_per_point(scale);
            let raw = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, vec2(width, height))),
                ..Default::default()
            };
            let output = ctx.run(raw, |ctx| view.show(ctx, &mut runtime));
            let meshes = ctx.tessellate(output.shapes, output.pixels_per_point);
            assert!(!meshes.is_empty());
            assert!(view.cursor_rect.is_finite());
        }
    }
}
