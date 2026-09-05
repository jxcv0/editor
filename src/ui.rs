//! Crossterm frontend.  Rendering is cell-diffed, while the editing core only
//! deals in [`crate::input::Key`] values.

use std::{
    borrow::Cow,
    collections::HashMap,
    io::{self, Stdout, Write},
    ops::Deref,
    panic,
    time::Duration,
};

use crossterm::{
    cursor::{Hide, MoveTo, SetCursorStyle, Show},
    event::{self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyEventKind},
    execute, queue,
    style::{
        Attribute, Color as CtColor, Print, SetAttribute, SetBackgroundColor, SetForegroundColor,
    },
    terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::{
    buffer::Buffer,
    command::{self, CommandSource},
    config::parse_hex_color,
    editor::{
        DiagnosticSeverity, Editor, Focus, Layout, Mode, Orientation, Pane, PickerKind, VisualKind,
    },
    input::{Key, KeyCode, Modifiers},
    syntax::{self, Highlight},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Color(pub u8, pub u8, pub u8);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Style {
    pub fg: Color,
    pub bg: Color,
    pub bold: bool,
    pub underline: bool,
    pub reverse: bool,
}

impl Style {
    pub const fn new(fg: Color, bg: Color) -> Self {
        Self {
            fg,
            bg,
            bold: false,
            underline: false,
            reverse: false,
        }
    }
    pub const fn bold(mut self) -> Self {
        self.bold = true;
        self
    }
    pub const fn underline(mut self) -> Self {
        self.underline = true;
        self
    }
    pub const fn reverse(mut self) -> Self {
        self.reverse = true;
        self
    }
}

impl Default for Style {
    fn default() -> Self {
        Self::new(Color(216, 222, 233), Color(17, 19, 24))
    }
}

/// Most terminal cells contain a short grapheme. Keep those bytes in the cell
/// instead of allocating once for every blank and ASCII character in a frame.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CellSymbol {
    Inline { bytes: [u8; 15], len: u8 },
    Extended(Box<str>),
}

impl CellSymbol {
    fn new(text: &str) -> Self {
        if text.len() <= 15 {
            let mut bytes = [0; 15];
            bytes[..text.len()].copy_from_slice(text.as_bytes());
            Self::Inline {
                bytes,
                len: text.len() as u8,
            }
        } else {
            Self::Extended(text.into())
        }
    }

    fn as_str(&self) -> &str {
        match self {
            Self::Inline { bytes, len } => {
                std::str::from_utf8(&bytes[..usize::from(*len)]).expect("stored valid UTF-8")
            }
            Self::Extended(text) => text,
        }
    }
}

impl Deref for CellSymbol {
    type Target = str;

    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl PartialEq<&str> for CellSymbol {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Cell {
    symbol: CellSymbol,
    style: Style,
    continuation: bool,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            symbol: CellSymbol::new(" "),
            style: Style::default(),
            continuation: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

impl Rect {
    pub fn inset(self, horizontal: u16, vertical: u16) -> Self {
        let x = self.x.saturating_add(horizontal);
        let y = self.y.saturating_add(vertical);
        Self {
            x,
            y,
            width: self.width.saturating_sub(horizontal.saturating_mul(2)),
            height: self.height.saturating_sub(vertical.saturating_mul(2)),
        }
    }
}

pub struct Canvas {
    pub width: u16,
    pub height: u16,
    cells: Vec<Cell>,
}

impl Canvas {
    pub fn new(width: u16, height: u16, style: Style) -> Self {
        let cell = Cell {
            style,
            ..Cell::default()
        };
        Self {
            width,
            height,
            cells: vec![cell; usize::from(width) * usize::from(height)],
        }
    }

    fn index(&self, x: u16, y: u16) -> Option<usize> {
        (x < self.width && y < self.height)
            .then(|| usize::from(y) * usize::from(self.width) + usize::from(x))
    }

    pub fn fill(&mut self, rect: Rect, symbol: &str, style: Style) {
        for y in rect.y..rect.y.saturating_add(rect.height).min(self.height) {
            for x in rect.x..rect.x.saturating_add(rect.width).min(self.width) {
                self.put_grapheme(x, y, symbol, style);
            }
        }
    }

    pub fn put_grapheme(&mut self, x: u16, y: u16, grapheme: &str, style: Style) -> u16 {
        let grapheme = terminal_safe_grapheme(grapheme);
        let width = terminal_cell_width(&grapheme) as u16;
        if x.saturating_add(width) > self.width {
            return 0;
        }
        let Some(index) = self.index(x, y) else {
            return 0;
        };
        self.cells[index] = Cell {
            symbol: CellSymbol::new(&grapheme),
            style,
            continuation: false,
        };
        for column in 1..width {
            if let Some(index) = self.index(x + column, y) {
                self.cells[index] = Cell {
                    symbol: CellSymbol::new(""),
                    style,
                    continuation: true,
                };
            }
        }
        width
    }

    pub fn text(&mut self, mut x: u16, y: u16, text: &str, max_width: u16, style: Style) -> u16 {
        let start = x;
        let end = x.saturating_add(max_width).min(self.width);
        for grapheme in text.graphemes(true) {
            let width = UnicodeWidthStr::width(grapheme).max(1) as u16;
            if x.saturating_add(width) > end {
                break;
            }
            x += self.put_grapheme(x, y, grapheme, style);
        }
        x - start
    }

    pub fn hline(&mut self, x: u16, y: u16, width: u16, symbol: &str, style: Style) {
        for column in 0..width {
            self.put_grapheme(x.saturating_add(column), y, symbol, style);
        }
    }

    pub fn vline(&mut self, x: u16, y: u16, height: u16, symbol: &str, style: Style) {
        for row in 0..height {
            self.put_grapheme(x, y.saturating_add(row), symbol, style);
        }
    }

    pub fn border(&mut self, rect: Rect, style: Style) {
        if rect.width < 2 || rect.height < 2 {
            return;
        }
        self.put_grapheme(rect.x, rect.y, "╭", style);
        self.put_grapheme(rect.x + rect.width - 1, rect.y, "╮", style);
        self.put_grapheme(rect.x, rect.y + rect.height - 1, "╰", style);
        self.put_grapheme(
            rect.x + rect.width - 1,
            rect.y + rect.height - 1,
            "╯",
            style,
        );
        self.hline(rect.x + 1, rect.y, rect.width - 2, "─", style);
        self.hline(
            rect.x + 1,
            rect.y + rect.height - 1,
            rect.width - 2,
            "─",
            style,
        );
        for y in rect.y + 1..rect.y + rect.height - 1 {
            self.put_grapheme(rect.x, y, "│", style);
            self.put_grapheme(rect.x + rect.width - 1, y, "│", style);
        }
    }
}

fn terminal_cell_width(symbol: &str) -> usize {
    UnicodeWidthStr::width(symbol).clamp(1, 2)
}

/// A terminal must never receive control bytes as cell contents. Besides
/// allowing escape-sequence injection, a newline rendered on the last row can
/// scroll the physical terminal without scrolling the cached canvas, which
/// corrupts every subsequent differential frame.
fn terminal_safe_grapheme(grapheme: &str) -> Cow<'_, str> {
    if !grapheme.chars().any(char::is_control) {
        return Cow::Borrowed(grapheme);
    }

    let mut characters = grapheme.chars();
    let replacement = match (characters.next(), characters.next()) {
        (Some(character @ '\0'..='\u{1f}'), None) => {
            char::from_u32(0x2400 + u32::from(character)).unwrap_or('\u{fffd}')
        }
        (Some('\u{7f}'), None) => '\u{2421}',
        _ => '\u{fffd}',
    };
    Cow::Owned(replacement.to_string())
}

pub struct Renderer {
    stdout: Stdout,
    previous: Vec<Cell>,
    width: u16,
    height: u16,
    previous_cursor: Option<(u16, u16)>,
    previous_cursor_style: Option<SetCursorStyle>,
    cursor_visible: bool,
}

impl Renderer {
    pub fn new() -> Self {
        Self {
            stdout: io::stdout(),
            previous: Vec::new(),
            width: 0,
            height: 0,
            previous_cursor: None,
            previous_cursor_style: None,
            cursor_visible: false,
        }
    }

    pub fn draw(
        &mut self,
        canvas: &Canvas,
        cursor: Option<(u16, u16)>,
        cursor_style: SetCursorStyle,
    ) -> io::Result<()> {
        let mut changed = false;
        if self.width != canvas.width || self.height != canvas.height {
            queue!(self.stdout, Clear(ClearType::All))?;
            changed = true;
            self.previous.clear();
            self.width = canvas.width;
            self.height = canvas.height;
        }
        let mut index = 0;
        while index < canvas.cells.len() {
            let cell = &canvas.cells[index];
            if self.previous.get(index) == Some(cell) || cell.continuation {
                index += 1;
                continue;
            }
            let row_width = usize::from(canvas.width);
            let x = (index % row_width) as u16;
            let y = (index / row_width) as u16;
            let style = cell.style;
            let row_end = (index / row_width + 1) * row_width;
            let mut run = String::new();
            let mut next = index;
            while next < row_end {
                let candidate = &canvas.cells[next];
                if candidate.continuation {
                    next += 1;
                    continue;
                }
                if candidate.style != style || self.previous.get(next) == Some(candidate) {
                    break;
                }
                debug_assert!(!candidate.symbol.chars().any(char::is_control));
                run.push_str(&candidate.symbol);
                next += terminal_cell_width(candidate.symbol.as_str());
            }
            queue!(
                self.stdout,
                MoveTo(x, y),
                SetForegroundColor(CtColor::Rgb {
                    r: style.fg.0,
                    g: style.fg.1,
                    b: style.fg.2
                }),
                SetBackgroundColor(CtColor::Rgb {
                    r: style.bg.0,
                    g: style.bg.1,
                    b: style.bg.2
                }),
                SetAttribute(if style.bold {
                    Attribute::Bold
                } else {
                    Attribute::NormalIntensity
                }),
                SetAttribute(if style.underline {
                    Attribute::Underlined
                } else {
                    Attribute::NoUnderline
                }),
                SetAttribute(if style.reverse {
                    Attribute::Reverse
                } else {
                    Attribute::NoReverse
                }),
                Print(run)
            )?;
            changed = true;
            index = next.max(index + 1);
        }
        // Unchanged extended graphemes should not be cloned either. The common
        // inline representation needs no per-cell heap allocation on changes.
        if self.previous.len() != canvas.cells.len() {
            self.previous.clone_from(&canvas.cells);
        } else {
            for (previous, current) in self.previous.iter_mut().zip(&canvas.cells) {
                if previous != current {
                    previous.clone_from(current);
                }
            }
        }
        if let Some((x, y)) = cursor.filter(|(x, y)| *x < canvas.width && *y < canvas.height) {
            if self.previous_cursor_style != Some(cursor_style) {
                queue!(self.stdout, cursor_style)?;
                self.previous_cursor_style = Some(cursor_style);
                changed = true;
            }
            if changed || !self.cursor_visible || self.previous_cursor != Some((x, y)) {
                queue!(self.stdout, Show, MoveTo(x, y))?;
                changed = true;
            }
            self.cursor_visible = true;
            self.previous_cursor = Some((x, y));
        } else if self.cursor_visible {
            queue!(self.stdout, Hide)?;
            changed = true;
            self.cursor_visible = false;
            self.previous_cursor = None;
        }
        if changed {
            self.stdout.flush()?;
        }
        Ok(())
    }
}

impl Default for Renderer {
    fn default() -> Self {
        Self::new()
    }
}

type PanicHook = dyn Fn(&panic::PanicHookInfo<'_>) + Send + Sync + 'static;

pub struct TerminalSession {
    previous_hook: Option<Box<PanicHook>>,
}

impl TerminalSession {
    pub fn enter() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        if let Err(error) = execute!(
            io::stdout(),
            EnterAlternateScreen,
            EnableBracketedPaste,
            Hide
        ) {
            let _ = terminal::disable_raw_mode();
            return Err(error);
        }
        let previous_hook = panic::take_hook();
        panic::set_hook(Box::new(|info| {
            let _ = terminal::disable_raw_mode();
            let _ = execute!(
                io::stdout(),
                DisableBracketedPaste,
                SetCursorStyle::DefaultUserShape,
                Show,
                LeaveAlternateScreen
            );
            eprintln!("editor panicked: {info}");
        }));
        Ok(Self {
            previous_hook: Some(previous_hook),
        })
    }

    pub fn size(&self) -> io::Result<(u16, u16)> {
        terminal::size()
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            DisableBracketedPaste,
            SetCursorStyle::DefaultUserShape,
            Show,
            LeaveAlternateScreen
        );
        if let Some(hook) = self.previous_hook.take() {
            panic::set_hook(hook);
        }
    }
}

#[derive(Debug)]
pub enum InputEvent {
    Key(Key),
    Paste(String),
    Resize,
    Focus,
    Tick,
}

pub fn poll_input(timeout: Duration) -> io::Result<InputEvent> {
    if !event::poll(timeout)? {
        return Ok(InputEvent::Tick);
    }
    match event::read()? {
        Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
            Ok(InputEvent::Key(map_key(key)))
        }
        Event::Paste(text) => Ok(InputEvent::Paste(text)),
        Event::Resize(_, _) => Ok(InputEvent::Resize),
        Event::FocusGained | Event::FocusLost => Ok(InputEvent::Focus),
        _ => Ok(InputEvent::Tick),
    }
}

fn map_key(key: event::KeyEvent) -> Key {
    use event::{KeyCode as C, KeyModifiers as M};
    let code = match key.code {
        C::Char(c) => KeyCode::Char(c),
        C::Enter => KeyCode::Enter,
        C::Esc => KeyCode::Esc,
        C::Backspace => KeyCode::Backspace,
        C::Delete => KeyCode::Delete,
        C::Tab => KeyCode::Tab,
        C::BackTab => KeyCode::BackTab,
        C::Left => KeyCode::Left,
        C::Right => KeyCode::Right,
        C::Up => KeyCode::Up,
        C::Down => KeyCode::Down,
        C::Home => KeyCode::Home,
        C::End => KeyCode::End,
        C::PageUp => KeyCode::PageUp,
        C::PageDown => KeyCode::PageDown,
        _ => KeyCode::Unknown,
    };
    let mut modifiers = Modifiers::empty();
    if key.modifiers.contains(M::SHIFT) {
        modifiers = modifiers.union(Modifiers::SHIFT);
    }
    if key.modifiers.contains(M::CONTROL) {
        modifiers = modifiers.union(Modifiers::CONTROL);
    }
    if key.modifiers.contains(M::ALT) {
        modifiers = modifiers.union(Modifiers::ALT);
    }
    Key { code, modifiers }
}

#[derive(Clone, Copy)]
struct Palette {
    background: Color,
    foreground: Color,
    muted: Color,
    accent: Color,
    status: Color,
    error: Color,
    warning: Color,
    info: Color,
    selection: Color,
    keyword: Color,
    string: Color,
    comment: Color,
    type_name: Color,
    number: Color,
}

impl Palette {
    fn from_editor(editor: &Editor) -> Self {
        let theme = &editor.config.ui.theme;
        let color = |value: &str, fallback| {
            parse_hex_color(value)
                .map(|(r, g, b)| Color(r, g, b))
                .unwrap_or(fallback)
        };
        Self {
            background: color(&theme.background, Color(17, 19, 24)),
            foreground: color(&theme.foreground, Color(216, 222, 233)),
            muted: color(&theme.muted, Color(102, 112, 133)),
            accent: color(&theme.accent, Color(122, 162, 247)),
            status: color(&theme.status, Color(36, 40, 59)),
            error: color(&theme.error, Color(247, 118, 142)),
            warning: color(&theme.warning, Color(224, 175, 104)),
            info: color(&theme.info, Color(125, 207, 255)),
            selection: color(&theme.selection, Color(51, 65, 92)),
            keyword: Color(187, 154, 247),
            string: Color(158, 206, 106),
            comment: Color(86, 95, 137),
            type_name: Color(42, 195, 222),
            number: Color(255, 158, 100),
        }
    }
}

/// Select a modal cursor shape without coupling the editing core to terminal
/// escape sequences. Text-entry contexts use a bar; command/navigation modes
/// use a solid block.
pub fn cursor_style(editor: &Editor) -> SetCursorStyle {
    if editor.picker.is_some()
        || matches!(
            editor.mode,
            Mode::Insert | Mode::Command | Mode::Search { .. }
        )
    {
        SetCursorStyle::SteadyBar
    } else {
        SetCursorStyle::SteadyBlock
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct DisplayPoint {
    byte: usize,
    grapheme: usize,
    column: usize,
}

struct CachedLine {
    language: syntax::Language,
    tab_width: usize,
    spans: Vec<syntax::Span>,
    ascii_columns: bool,
    checkpoints: Vec<DisplayPoint>,
    scanned: DisplayPoint,
    used_frame: u64,
}

impl CachedLine {
    fn new(line: &str, language: syntax::Language, tab_width: usize, frame: u64) -> Self {
        Self {
            language,
            tab_width,
            spans: syntax::highlight_line(language, line),
            ascii_columns: line.is_ascii() && !line.contains('\t'),
            checkpoints: vec![DisplayPoint::default()],
            scanned: DisplayPoint::default(),
            used_frame: frame,
        }
    }

    fn advance(&self, point: DisplayPoint, grapheme: &str) -> DisplayPoint {
        DisplayPoint {
            byte: point.byte + grapheme.len(),
            grapheme: point.grapheme + 1,
            column: point.column
                + if grapheme == "\t" {
                    self.tab_width - point.column % self.tab_width
                } else {
                    UnicodeWidthStr::width(grapheme).max(1)
                },
        }
    }

    /// Extend only as far as a requested position. Opening a long Unicode line
    /// at its start must not first build an index of the entire line.
    fn extend(&mut self, line: &str, grapheme: usize, column: usize) {
        for text in line[self.scanned.byte..].graphemes(true) {
            if self.scanned.grapheme >= grapheme && self.scanned.column >= column {
                break;
            }
            self.scanned = self.advance(self.scanned, text);
            if self.scanned.grapheme.is_multiple_of(256) {
                self.checkpoints.push(self.scanned);
            }
        }
    }

    fn at_grapheme(&mut self, line: &str, grapheme: usize) -> DisplayPoint {
        if self.ascii_columns {
            let index = grapheme.min(line.len());
            return DisplayPoint {
                byte: index,
                grapheme: index,
                column: index,
            };
        }
        self.extend(line, grapheme, 0);
        let checkpoint = self
            .checkpoints
            .partition_point(|point| point.grapheme <= grapheme);
        let mut point = self.checkpoints[checkpoint.saturating_sub(1)];
        for text in line[point.byte..].graphemes(true) {
            if point.grapheme >= grapheme {
                break;
            }
            point = self.advance(point, text);
        }
        point
    }

    fn at_column(&mut self, line: &str, column: usize) -> DisplayPoint {
        if self.ascii_columns {
            let index = column.min(line.len());
            return DisplayPoint {
                byte: index,
                grapheme: index,
                column: index,
            };
        }
        self.extend(line, 0, column);
        let checkpoint = self
            .checkpoints
            .partition_point(|point| point.column <= column);
        let mut point = self.checkpoints[checkpoint.saturating_sub(1)];
        for text in line[point.byte..].graphemes(true) {
            let next = self.advance(point, text);
            if next.column > column {
                break;
            }
            point = next;
        }
        point
    }
}

/// Retains line syntax and sparse display indexes across frames. Line keys come
/// from the buffer, so edits invalidate only the text that actually changed.
#[derive(Default)]
pub struct FrameBuilder {
    lines: HashMap<u64, CachedLine>,
    frame: u64,
}

impl FrameBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn draw_editor(
        &mut self,
        editor: &mut Editor,
        width: u16,
        height: u16,
    ) -> (Canvas, Option<(u16, u16)>) {
        self.frame = self.frame.wrapping_add(1);
        let result = build_frame(self, editor, width, height);
        if self.lines.len() > 256 {
            self.lines.retain(|_, line| line.used_frame == self.frame);
        }
        result
    }

    fn line(&mut self, buffer: &Buffer, number: usize, tab_width: usize) -> &mut CachedLine {
        let key = buffer.line_cache_key(number).expect("visible line exists");
        let language = syntax::language_for_path(buffer.path());
        let text = buffer.line(number).expect("visible line exists");
        let line = self
            .lines
            .entry(key)
            .or_insert_with(|| CachedLine::new(text, language, tab_width, self.frame));
        if line.tab_width != tab_width || line.language != language {
            *line = CachedLine::new(text, language, tab_width, self.frame);
        }
        line.used_frame = self.frame;
        line
    }
}

/// Build a standalone deterministic frame. Interactive frontends should retain
/// a `FrameBuilder` to reuse unchanged line data between frames.
pub fn draw_editor(editor: &mut Editor, width: u16, height: u16) -> (Canvas, Option<(u16, u16)>) {
    FrameBuilder::new().draw_editor(editor, width, height)
}

fn build_frame(
    builder: &mut FrameBuilder,
    editor: &mut Editor,
    width: u16,
    height: u16,
) -> (Canvas, Option<(u16, u16)>) {
    let palette = Palette::from_editor(editor);
    let base = Style::new(palette.foreground, palette.background);
    let mut canvas = Canvas::new(width, height, base);
    if width == 0 || height == 0 {
        return (canvas, None);
    }

    let chrome_height = height.min(2);
    let content = Rect {
        x: 0,
        y: 0,
        width,
        height: height.saturating_sub(chrome_height),
    };
    let (mut content, terminal_rect) = terminal_layout(content, editor.terminal.visible);
    if editor.explorer.open && content.width >= 40 {
        let explorer_width = editor
            .explorer
            .width
            .min(content.width.saturating_sub(20))
            .max(16);
        let explorer_rect = Rect {
            x: 0,
            y: 0,
            width: explorer_width,
            height: content.height,
        };
        render_explorer(&mut canvas, editor, explorer_rect, palette);
        if explorer_width < width {
            canvas.vline(
                explorer_width,
                0,
                content.height,
                "│",
                Style::new(palette.muted, palette.background),
            );
        }
        content.x = explorer_width.saturating_add(1);
        content.width = content
            .width
            .saturating_sub(explorer_width.saturating_add(1));
    }

    let mut pane_rects = Vec::new();
    layout_rects(&editor.layout, content, &mut pane_rects);
    sync_viewports(builder, editor, &pane_rects);
    let mut editor_cursor = None;
    for (pane_id, rect) in &pane_rects {
        if let Some(pane) = editor.panes.iter().find(|pane| pane.id == *pane_id) {
            let cursor = render_pane(builder, &mut canvas, editor, pane, *rect, palette);
            if *pane_id == editor.active_pane {
                editor_cursor = cursor;
            }
        }
    }
    render_split_lines(&mut canvas, &editor.layout, content, palette);
    let terminal_cursor =
        terminal_rect.and_then(|rect| render_terminal(&mut canvas, editor, rect, palette));

    if height >= 2 {
        render_status(
            &mut canvas,
            editor,
            Rect {
                x: 0,
                y: height - 2,
                width,
                height: 1,
            },
            palette,
        );
        render_command_line(
            &mut canvas,
            editor,
            Rect {
                x: 0,
                y: height - 1,
                width,
                height: 1,
            },
            palette,
        );
    } else {
        render_status(
            &mut canvas,
            editor,
            Rect {
                x: 0,
                y: 0,
                width,
                height: 1,
            },
            palette,
        );
    }

    if matches!(editor.mode, Mode::Leader) {
        render_leader(&mut canvas, editor, palette);
    }
    if editor.picker.is_some() {
        render_picker(&mut canvas, editor, palette);
    }

    let cursor = if let Some(picker) = &editor.picker {
        let rect = overlay_rect(width, height, 72, 18);
        let prompt_width = UnicodeWidthStr::width(picker.query.as_str()) as u16;
        Some((
            rect.x
                .saturating_add(3)
                .saturating_add(prompt_width)
                .min(rect.x.saturating_add(rect.width.saturating_sub(1)))
                .min(width.saturating_sub(1)),
            rect.y
                .saturating_add(2)
                .min(rect.y.saturating_add(rect.height.saturating_sub(1)))
                .min(height.saturating_sub(1)),
        ))
    } else {
        match editor.mode {
            Mode::Command | Mode::Search { .. } if height > 0 => Some((
                (1 + UnicodeWidthStr::width(editor.prompt.as_str()) as u16)
                    .min(width.saturating_sub(1)),
                height - 1,
            )),
            _ if editor.focus == Focus::Terminal => terminal_cursor,
            _ if editor.focus == Focus::Editor => editor_cursor,
            _ => None,
        }
    };
    (canvas, cursor)
}

fn terminal_layout(content: Rect, visible: bool) -> (Rect, Option<Rect>) {
    if !visible || content.height < 2 || content.width == 0 {
        return (content, None);
    }
    let terminal_height = (content.height / 3)
        .max(3)
        .min(content.height.saturating_sub(1));
    let editor = Rect {
        height: content.height.saturating_sub(terminal_height),
        ..content
    };
    let terminal = Rect {
        x: content.x,
        y: editor.y.saturating_add(editor.height),
        width: content.width,
        height: terminal_height,
    };
    (editor, Some(terminal))
}

fn render_terminal(
    canvas: &mut Canvas,
    editor: &mut Editor,
    rect: Rect,
    palette: Palette,
) -> Option<(u16, u16)> {
    if rect.height == 0 || rect.width == 0 {
        return None;
    }

    let header = Rect { height: 1, ..rect };
    let body = Rect {
        y: rect.y.saturating_add(1),
        height: rect.height.saturating_sub(1),
        ..rect
    };
    canvas.fill(header, " ", Style::new(palette.foreground, palette.status));
    let status = editor.terminal.status.label();
    let scrollback = editor.terminal.screen().scrollback();
    let suffix = if scrollback > 0 {
        format!("  scrollback:{scrollback}  Shift-PgDn returns")
    } else if editor.focus == Focus::Terminal {
        "  Ctrl-\\ editor  Shift-PgUp scroll".into()
    } else {
        "  Ctrl-W j focus  <Space>t hide".into()
    };
    canvas.text(
        header.x.saturating_add(1),
        header.y,
        &format!("TERMINAL  {status}{suffix}"),
        header.width.saturating_sub(2),
        Style::new(
            if editor.focus == Focus::Terminal {
                palette.accent
            } else {
                palette.muted
            },
            palette.status,
        )
        .bold(),
    );

    if body.height == 0 {
        return None;
    }
    editor.terminal.resize(body.height, body.width);
    canvas.fill(
        body,
        " ",
        Style::new(palette.foreground, palette.background),
    );
    let screen = editor.terminal.screen();
    for row in 0..body.height {
        for column in 0..body.width {
            let Some(cell) = screen.cell(row, column) else {
                continue;
            };
            if cell.is_wide_continuation() {
                continue;
            }
            let symbol = if cell.has_contents() {
                cell.contents()
            } else {
                " "
            };
            let style = Style {
                fg: terminal_color(cell.fgcolor(), palette.foreground),
                bg: terminal_color(cell.bgcolor(), palette.background),
                bold: cell.bold(),
                underline: cell.underline(),
                reverse: cell.inverse(),
            };
            canvas.put_grapheme(
                body.x.saturating_add(column),
                body.y.saturating_add(row),
                symbol,
                style,
            );
        }
    }

    if editor.focus != Focus::Terminal || screen.hide_cursor() || screen.scrollback() > 0 {
        return None;
    }
    let (row, column) = screen.cursor_position();
    Some((
        body.x
            .saturating_add(column.min(body.width.saturating_sub(1))),
        body.y
            .saturating_add(row.min(body.height.saturating_sub(1))),
    ))
}

fn terminal_color(color: vt100::Color, default: Color) -> Color {
    match color {
        vt100::Color::Default => default,
        vt100::Color::Rgb(red, green, blue) => Color(red, green, blue),
        vt100::Color::Idx(index) => indexed_terminal_color(index),
    }
}

fn indexed_terminal_color(index: u8) -> Color {
    const ANSI: [Color; 16] = [
        Color(0, 0, 0),
        Color(205, 49, 49),
        Color(13, 188, 121),
        Color(229, 229, 16),
        Color(36, 114, 200),
        Color(188, 63, 188),
        Color(17, 168, 205),
        Color(229, 229, 229),
        Color(102, 102, 102),
        Color(241, 76, 76),
        Color(35, 209, 139),
        Color(245, 245, 67),
        Color(59, 142, 234),
        Color(214, 112, 214),
        Color(41, 184, 219),
        Color(255, 255, 255),
    ];
    match index {
        0..=15 => ANSI[usize::from(index)],
        16..=231 => {
            const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
            let value = index - 16;
            Color(
                LEVELS[usize::from(value / 36)],
                LEVELS[usize::from((value % 36) / 6)],
                LEVELS[usize::from(value % 6)],
            )
        }
        232..=255 => {
            let value = 8_u8.saturating_add((index - 232).saturating_mul(10));
            Color(value, value, value)
        }
    }
}

fn layout_rects(layout: &Layout, rect: Rect, output: &mut Vec<(u64, Rect)>) {
    match layout {
        Layout::Leaf(id) => output.push((*id, rect)),
        Layout::Split {
            orientation,
            ratio,
            first,
            second,
        } => match orientation {
            Orientation::Vertical => {
                let available = rect.width.saturating_sub(1);
                let first_width = ((u32::from(available) * u32::from(*ratio)) / 1000) as u16;
                let first_rect = Rect {
                    width: first_width,
                    ..rect
                };
                let second_rect = Rect {
                    x: rect.x.saturating_add(first_width).saturating_add(1),
                    width: available.saturating_sub(first_width),
                    ..rect
                };
                layout_rects(first, first_rect, output);
                layout_rects(second, second_rect, output);
            }
            Orientation::Horizontal => {
                let available = rect.height.saturating_sub(1);
                let first_height = ((u32::from(available) * u32::from(*ratio)) / 1000) as u16;
                let first_rect = Rect {
                    height: first_height,
                    ..rect
                };
                let second_rect = Rect {
                    y: rect.y.saturating_add(first_height).saturating_add(1),
                    height: available.saturating_sub(first_height),
                    ..rect
                };
                layout_rects(first, first_rect, output);
                layout_rects(second, second_rect, output);
            }
        },
    }
}

fn render_split_lines(canvas: &mut Canvas, layout: &Layout, rect: Rect, palette: Palette) {
    let style = Style::new(palette.muted, palette.background);
    if let Layout::Split {
        orientation,
        ratio,
        first,
        second,
    } = layout
    {
        match orientation {
            Orientation::Vertical => {
                let available = rect.width.saturating_sub(1);
                let first_width = ((u32::from(available) * u32::from(*ratio)) / 1000) as u16;
                let x = rect.x.saturating_add(first_width);
                for y in rect.y..rect.y.saturating_add(rect.height) {
                    canvas.put_grapheme(x, y, "│", style);
                }
                let first_rect = Rect {
                    width: first_width,
                    ..rect
                };
                let second_rect = Rect {
                    x: x.saturating_add(1),
                    width: available.saturating_sub(first_width),
                    ..rect
                };
                render_split_lines(canvas, first, first_rect, palette);
                render_split_lines(canvas, second, second_rect, palette);
            }
            Orientation::Horizontal => {
                let available = rect.height.saturating_sub(1);
                let first_height = ((u32::from(available) * u32::from(*ratio)) / 1000) as u16;
                let y = rect.y.saturating_add(first_height);
                canvas.hline(rect.x, y, rect.width, "─", style);
                let first_rect = Rect {
                    height: first_height,
                    ..rect
                };
                let second_rect = Rect {
                    y: y.saturating_add(1),
                    height: available.saturating_sub(first_height),
                    ..rect
                };
                render_split_lines(canvas, first, first_rect, palette);
                render_split_lines(canvas, second, second_rect, palette);
            }
        }
    }
}

fn sync_viewports(builder: &mut FrameBuilder, editor: &mut Editor, rects: &[(u64, Rect)]) {
    for (id, rect) in rects {
        let Some(pane_index) = editor.panes.iter().position(|pane| pane.id == *id) else {
            continue;
        };
        let buffer_index = editor.panes[pane_index].buffer;
        let buffer = &editor.buffers[buffer_index].buffer;
        let line_count = buffer.line_count();
        let cursor = buffer.clamp_pos(editor.panes[pane_index].cursor);
        let anchor = editor.panes[pane_index]
            .anchor
            .map(|anchor| buffer.clamp_pos(anchor));
        let line = buffer.line(cursor.line).unwrap_or("");
        let tab_width = editor.config.editor.tab_width.max(1);
        let cursor_column = builder
            .line(buffer, cursor.line, tab_width)
            .at_grapheme(line, cursor.grapheme)
            .column;
        let gutter = (buffer.line_count().max(1).to_string().len() as u16 + 3)
            .min(rect.width.saturating_sub(1));
        let visible_lines = usize::from(rect.height.max(1));
        let visible_columns = usize::from(rect.width.saturating_sub(gutter).max(1));
        let pane = &mut editor.panes[pane_index];
        pane.cursor = cursor;
        pane.anchor = anchor;
        if cursor.line < pane.viewport_line {
            pane.viewport_line = cursor.line;
        } else if cursor.line >= pane.viewport_line.saturating_add(visible_lines) {
            pane.viewport_line = cursor.line.saturating_sub(visible_lines.saturating_sub(1));
        }
        pane.viewport_line = pane.viewport_line.min(line_count.saturating_sub(1));

        if cursor_column < pane.viewport_column {
            pane.viewport_column = cursor_column;
        } else if cursor_column >= pane.viewport_column.saturating_add(visible_columns) {
            pane.viewport_column = cursor_column.saturating_sub(visible_columns.saturating_sub(1));
        }
    }
}

fn render_pane(
    builder: &mut FrameBuilder,
    canvas: &mut Canvas,
    editor: &Editor,
    pane: &Pane,
    rect: Rect,
    palette: Palette,
) -> Option<(u16, u16)> {
    if rect.width == 0 || rect.height == 0 {
        return None;
    }
    let slot = &editor.buffers[pane.buffer];
    let buffer = &slot.buffer;
    let digits = buffer.line_count().max(1).to_string().len() as u16;
    let gutter = (digits + 3).min(rect.width.saturating_sub(1));
    let content_x = rect.x.saturating_add(gutter);
    let content_width = rect.width.saturating_sub(gutter);
    let working_lines = buffer
        .path()
        .and_then(|path| editor.codex_working_lines.get(path));
    let diagnostics = editor
        .diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic.path.as_deref() == buffer.path() && diagnostic.version == buffer.revision()
        })
        .collect::<Vec<_>>();
    let mut cursor = None;

    for row in 0..rect.height {
        let line_number = pane.viewport_line + usize::from(row);
        if line_number >= buffer.line_count() {
            canvas.text(
                rect.x + gutter.saturating_sub(2),
                rect.y + row,
                "~",
                1,
                Style::new(palette.muted, palette.background),
            );
            continue;
        }
        let active_line = line_number == pane.cursor.line;
        let number = if active_line || !editor.config.ui.relative_numbers {
            line_number + 1
        } else {
            line_number.abs_diff(pane.cursor.line)
        };
        let number_text = format!("{number:>width$}", width = usize::from(digits));
        let number_style = Style::new(
            if active_line {
                palette.accent
            } else {
                palette.muted
            },
            palette.background,
        );
        canvas.text(rect.x, rect.y + row, &number_text, digits, number_style);
        if gutter > digits && working_lines.is_some_and(|lines| lines.contains(&line_number)) {
            const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
            canvas.text(
                rect.x + digits,
                rect.y + row,
                FRAMES[editor.codex_spinner_frame % FRAMES.len()],
                1,
                Style::new(palette.info, palette.background),
            );
        }
        let severity = diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.line == line_number)
            .map(|diagnostic| diagnostic.severity)
            .min_by_key(|severity| diagnostic_severity_rank(*severity));
        if let Some(severity) = severity {
            let (symbol, color) = diagnostic_decoration(severity, palette);
            canvas.text(
                rect.x + digits + 1,
                rect.y + row,
                symbol,
                1,
                Style::new(color, palette.background),
            );
        }
        let line = buffer.line(line_number).unwrap_or("");
        let tab_width = editor.config.editor.tab_width.max(1);
        let cached = builder.line(buffer, line_number, tab_width);
        let start = cached.at_column(line, pane.viewport_column);
        let cursor_column =
            active_line.then(|| cached.at_grapheme(line, pane.cursor.grapheme).column);
        let mut spans = cached.spans.iter().peekable();
        let mut display_column = start.column;
        for (offset, (byte, grapheme)) in line[start.byte..].grapheme_indices(true).enumerate() {
            let grapheme_index = start.grapheme + offset;
            let byte = start.byte + byte;
            let width = if grapheme == "\t" {
                tab_width - (display_column % tab_width)
            } else {
                UnicodeWidthStr::width(grapheme).max(1)
            };
            let next_column = display_column + width;
            if next_column <= pane.viewport_column {
                display_column = next_column;
                continue;
            }
            let screen_column = display_column.saturating_sub(pane.viewport_column);
            if screen_column >= usize::from(content_width) {
                break;
            }
            let selected = is_selected(editor, pane, line_number, grapheme_index);
            while spans.peek().is_some_and(|span| span.end <= byte) {
                spans.next();
            }
            let kind = spans
                .peek()
                .filter(|span| byte >= span.start && byte < span.end)
                .map(|span| span.kind)
                .unwrap_or(Highlight::Plain);
            let mut style = syntax_style(kind, palette);
            if selected {
                style.bg = palette.selection;
            }
            if grapheme == "\t" {
                for offset in 0..width.min(usize::from(content_width).saturating_sub(screen_column))
                {
                    canvas.put_grapheme(
                        content_x + (screen_column + offset) as u16,
                        rect.y + row,
                        " ",
                        style,
                    );
                }
            } else {
                canvas.put_grapheme(
                    content_x + screen_column as u16,
                    rect.y + row,
                    grapheme,
                    style,
                );
            }
            display_column = next_column;
        }
        let mut inline_column = display_column
            .saturating_sub(pane.viewport_column)
            .saturating_add(2);
        let mut rendered_diagnostic = false;
        for severity in [
            DiagnosticSeverity::Error,
            DiagnosticSeverity::Warning,
            DiagnosticSeverity::Information,
            DiagnosticSeverity::Hint,
        ] {
            for diagnostic in diagnostics.iter().filter(|diagnostic| {
                diagnostic.line == line_number && diagnostic.severity == severity
            }) {
                if inline_column >= usize::from(content_width) {
                    break;
                }
                let (symbol, color) = diagnostic_decoration(severity, palette);
                let summary = diagnostic
                    .message
                    .lines()
                    .next()
                    .filter(|line| !line.is_empty())
                    .unwrap_or("diagnostic");
                let separator = if rendered_diagnostic { "  " } else { "" };
                let label = format!("{separator}{symbol} {summary}");
                let written = canvas.text(
                    content_x.saturating_add(inline_column as u16),
                    rect.y + row,
                    &label,
                    content_width.saturating_sub(inline_column as u16),
                    Style::new(color, palette.background),
                );
                inline_column = inline_column.saturating_add(usize::from(written));
                rendered_diagnostic = true;
            }
        }
        if pane.id == editor.active_pane && active_line {
            let before = cursor_column.expect("active line has cursor column");
            let x = content_x.saturating_add(before.saturating_sub(pane.viewport_column) as u16);
            if x < rect.x.saturating_add(rect.width) {
                cursor = Some((x, rect.y + row));
            }
        }
    }
    cursor
}

fn diagnostic_severity_rank(severity: DiagnosticSeverity) -> u8 {
    match severity {
        DiagnosticSeverity::Error => 0,
        DiagnosticSeverity::Warning => 1,
        DiagnosticSeverity::Information => 2,
        DiagnosticSeverity::Hint => 3,
    }
}

fn diagnostic_decoration(severity: DiagnosticSeverity, palette: Palette) -> (&'static str, Color) {
    match severity {
        DiagnosticSeverity::Error => ("●", palette.error),
        DiagnosticSeverity::Warning => ("▲", palette.warning),
        DiagnosticSeverity::Information => ("●", palette.info),
        DiagnosticSeverity::Hint => ("·", palette.muted),
    }
}

fn is_selected(editor: &Editor, pane: &Pane, line: usize, grapheme: usize) -> bool {
    if pane.id != editor.active_pane {
        return false;
    }
    let Mode::Visual(kind) = editor.mode else {
        return false;
    };
    let Some(anchor) = pane.anchor else {
        return false;
    };
    let cursor = pane.cursor;
    match kind {
        VisualKind::Character => {
            let pos = crate::buffer::Pos::new(line, grapheme);
            let (start, end) = if anchor <= cursor {
                (anchor, cursor)
            } else {
                (cursor, anchor)
            };
            pos >= start && pos <= end
        }
        VisualKind::Line => {
            line >= anchor.line.min(cursor.line) && line <= anchor.line.max(cursor.line)
        }
        VisualKind::Block => {
            line >= anchor.line.min(cursor.line)
                && line <= anchor.line.max(cursor.line)
                && grapheme >= anchor.grapheme.min(cursor.grapheme)
                && grapheme <= anchor.grapheme.max(cursor.grapheme)
        }
    }
}

fn syntax_style(kind: Highlight, palette: Palette) -> Style {
    let color = match kind {
        Highlight::Keyword => palette.keyword,
        Highlight::Type => palette.type_name,
        Highlight::String => palette.string,
        Highlight::Comment => palette.comment,
        Highlight::Number => palette.number,
        Highlight::Function | Highlight::Macro => palette.info,
        Highlight::Heading => palette.accent,
        Highlight::Punctuation | Highlight::Plain => palette.foreground,
    };
    let mut style = Style::new(color, palette.background);
    if matches!(kind, Highlight::Keyword | Highlight::Heading) {
        style.bold = true;
    }
    style
}

fn render_explorer(canvas: &mut Canvas, editor: &Editor, rect: Rect, palette: Palette) {
    canvas.fill(rect, " ", Style::new(palette.foreground, palette.status));
    let title = editor
        .explorer
        .root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("/");
    canvas.text(
        rect.x + 1,
        rect.y,
        &format!("▾ {title}"),
        rect.width.saturating_sub(2),
        Style::new(palette.accent, palette.status).bold(),
    );
    let visible = usize::from(rect.height.saturating_sub(2));
    let start = editor
        .explorer
        .selected
        .saturating_sub(visible.saturating_sub(1));
    for (row, entry) in editor
        .explorer
        .rows()
        .iter()
        .enumerate()
        .skip(start)
        .take(visible)
    {
        let y = rect.y + 1 + (row - start) as u16;
        let selected = row == editor.explorer.selected && editor.focus == Focus::Explorer;
        let style = Style::new(
            if selected {
                palette.background
            } else if entry.is_directory() {
                palette.accent
            } else {
                palette.foreground
            },
            if selected {
                palette.accent
            } else {
                palette.status
            },
        );
        let marker = if entry.is_directory() {
            if editor.explorer.expanded.contains(&entry.path) {
                "▾"
            } else {
                "▸"
            }
        } else {
            " "
        };
        let name = entry.path.file_name().unwrap_or_default().to_string_lossy();
        let indent = entry.depth.saturating_sub(1).saturating_mul(2);
        let label = format!(
            " {}{marker} {name}{}",
            " ".repeat(indent.min(usize::from(rect.width))),
            if entry.is_directory() { "/" } else { "" },
        );
        canvas.fill(
            Rect {
                x: rect.x,
                y,
                width: rect.width,
                height: 1,
            },
            " ",
            style,
        );
        canvas.text(rect.x, y, &label, rect.width, style);
    }
    let flags = format!(
        "{}hidden {}ignored",
        if editor.explorer.show_hidden {
            "✓ "
        } else {
            "· "
        },
        if editor.explorer.show_ignored {
            "✓ "
        } else {
            "· "
        }
    );
    if rect.height > 1 {
        canvas.text(
            rect.x + 1,
            rect.y + rect.height - 1,
            &flags,
            rect.width.saturating_sub(2),
            Style::new(palette.muted, palette.status),
        );
    }
}

fn render_status(canvas: &mut Canvas, editor: &Editor, rect: Rect, palette: Palette) {
    canvas.fill(rect, " ", Style::new(palette.foreground, palette.status));
    let mode_color = match editor.mode {
        Mode::Normal => palette.accent,
        Mode::Insert => Color(158, 206, 106),
        Mode::Visual(_) => Color(187, 154, 247),
        Mode::OperatorPending => palette.warning,
        _ => palette.info,
    };
    let mode = format!(" {} ", editor.mode.label());
    let mode_width = canvas.text(
        rect.x,
        rect.y,
        &mode,
        rect.width,
        Style::new(palette.background, mode_color).bold(),
    );
    let slot = editor.active_slot();
    let flags = format!(
        "{}{}{}",
        if slot.buffer.is_dirty() { " [+]" } else { "" },
        if slot.buffer.is_read_only() {
            " [RO]"
        } else {
            ""
        },
        if slot.large_file { " [LARGE]" } else { "" }
    );
    let path = format!(" {}{}", slot.display_name, flags);
    canvas.text(
        rect.x + mode_width,
        rect.y,
        &path,
        rect.width.saturating_sub(mode_width),
        Style::new(palette.foreground, palette.status),
    );
    let pane = editor.active_pane();
    let right = if rect.width > 90 {
        format!(
            "RA:{}  CODEX:{}  {}:{}",
            editor.rust_analyzer_status,
            editor.codex_watch_status,
            pane.cursor.line + 1,
            pane.cursor.grapheme + 1
        )
    } else if rect.width > 55 {
        format!(
            "RA:{}  {}:{}",
            editor.rust_analyzer_status,
            pane.cursor.line + 1,
            pane.cursor.grapheme + 1
        )
    } else {
        format!("{}:{}", pane.cursor.line + 1, pane.cursor.grapheme + 1)
    };
    let right_width = UnicodeWidthStr::width(right.as_str()) as u16;
    if right_width < rect.width {
        canvas.text(
            rect.x + rect.width - right_width - 1,
            rect.y,
            &right,
            right_width,
            Style::new(palette.muted, palette.status),
        );
    }
}

fn render_command_line(canvas: &mut Canvas, editor: &Editor, rect: Rect, palette: Palette) {
    canvas.fill(
        rect,
        " ",
        Style::new(palette.foreground, palette.background),
    );
    let text = match editor.mode {
        Mode::Command => format!(":{}", editor.prompt),
        Mode::Search { backward: true } => format!("?{}", editor.prompt),
        Mode::Search { backward: false } => format!("/{}", editor.prompt),
        Mode::Leader => format!("<Space>{}", editor.leader_prefix),
        _ => editor.current_message().unwrap_or("").to_owned(),
    };
    let style = if text.to_ascii_lowercase().contains("error")
        || text.starts_with("Unknown")
        || text.contains("unavailable")
    {
        Style::new(palette.error, palette.background)
    } else {
        Style::new(palette.foreground, palette.background)
    };
    canvas.text(rect.x, rect.y, &text, rect.width, style);
}

fn render_leader(canvas: &mut Canvas, editor: &Editor, palette: Palette) {
    let mut entries = command::menu_entries(&editor.leader_prefix);
    if editor.leader_prefix == "a"
        && !["stopped", "disabled", "completed", "failed"]
            .iter()
            .any(|state| editor.codex_watch_status.starts_with(state))
    {
        entries.retain(|entry| !matches!(entry.key, 'd' | 'w'));
    }
    if entries.is_empty() {
        return;
    }
    let desired_height = u16::try_from(entries.len())
        .unwrap_or(u16::MAX)
        .saturating_add(3);
    let rect = overlay_rect(canvas.width, canvas.height, 44, desired_height);
    if rect.width == 0 || rect.height == 0 {
        return;
    }
    canvas.fill(rect, " ", Style::new(palette.foreground, palette.status));
    canvas.border(rect, Style::new(palette.muted, palette.status));
    canvas.text(
        rect.x.saturating_add(2),
        rect.y,
        " which key ",
        rect.width.saturating_sub(4),
        Style::new(palette.accent, palette.status).bold(),
    );
    for (row, entry) in entries
        .iter()
        .take(usize::from(rect.height.saturating_sub(2)))
        .enumerate()
    {
        let sequence = format!("{}{}", editor.leader_prefix, entry.key);
        let command = command::by_sequence(&sequence);
        let unavailable = command.is_some_and(|command| match command.source {
            CommandSource::RustAnalyzer
                if command.id == crate::command::CommandId::RustAnalyzerRestart =>
            {
                false
            }
            CommandSource::RustAnalyzer => editor.rust_analyzer_status != "ready",
            CommandSource::CodexWatch => false,
            CommandSource::Editor => false,
        });
        let color = if unavailable {
            palette.muted
        } else {
            palette.foreground
        };
        let mut label = if entry.group {
            format!("+ {}", entry.label)
        } else {
            entry.label.into()
        };
        if unavailable {
            label.push_str(&format!("  [{}]", editor.rust_analyzer_status));
        }
        canvas.text(
            rect.x.saturating_add(2),
            rect.y.saturating_add(1).saturating_add(row as u16),
            &format!("{}  {}", entry.key, label),
            rect.width.saturating_sub(4),
            Style::new(color, palette.status),
        );
    }
}

fn render_picker(canvas: &mut Canvas, editor: &Editor, palette: Palette) {
    let Some(picker) = &editor.picker else { return };
    let rect = overlay_rect(canvas.width, canvas.height, 72, 18);
    if rect.width == 0 || rect.height == 0 {
        return;
    }
    canvas.fill(rect, " ", Style::new(palette.foreground, palette.status));
    canvas.border(rect, Style::new(palette.accent, palette.status));
    let title = match picker.kind {
        PickerKind::Files => " Project files ",
        PickerKind::Buffers => " Buffers ",
        PickerKind::Grep => " Project grep ",
        PickerKind::Messages => " Messages ",
        PickerKind::Diagnostics => " Diagnostics ",
        PickerKind::Symbols => " Symbols ",
        PickerKind::Recent => " Recent files ",
    };
    canvas.text(
        rect.x.saturating_add(2),
        rect.y,
        title,
        rect.width.saturating_sub(4),
        Style::new(palette.accent, palette.status).bold(),
    );
    canvas.text(
        rect.x.saturating_add(2),
        rect.y.saturating_add(2),
        "› ",
        2,
        Style::new(palette.accent, palette.status).bold(),
    );
    canvas.text(
        rect.x.saturating_add(4),
        rect.y.saturating_add(2),
        &picker.query,
        rect.width.saturating_sub(6),
        Style::new(palette.foreground, palette.status),
    );
    if rect.height > 4 {
        canvas.hline(
            rect.x.saturating_add(1),
            rect.y.saturating_add(3),
            rect.width.saturating_sub(2),
            "─",
            Style::new(palette.muted, palette.status),
        );
    }
    let rows = usize::from(rect.height.saturating_sub(5));
    let start = picker.selected.saturating_sub(rows.saturating_sub(1));
    for (index, item) in picker.items.iter().enumerate().skip(start).take(rows) {
        let y = rect
            .y
            .saturating_add(4)
            .saturating_add((index - start) as u16);
        let selected = index == picker.selected;
        let style = Style::new(
            if selected {
                palette.background
            } else {
                palette.foreground
            },
            if selected {
                palette.accent
            } else {
                palette.status
            },
        );
        canvas.fill(
            Rect {
                x: rect.x.saturating_add(1),
                y,
                width: rect.width.saturating_sub(2),
                height: 1,
            },
            " ",
            style,
        );
        canvas.text(
            rect.x.saturating_add(2),
            y,
            &item.label,
            rect.width.saturating_sub(4),
            style,
        );
    }
    if picker.items.is_empty() {
        canvas.text(
            rect.x.saturating_add(2),
            rect.y.saturating_add(4),
            if picker.query.is_empty() {
                "Type to search"
            } else {
                "No matches"
            },
            rect.width.saturating_sub(4),
            Style::new(palette.muted, palette.status),
        );
    }
}

fn overlay_rect(
    screen_width: u16,
    screen_height: u16,
    desired_width: u16,
    desired_height: u16,
) -> Rect {
    let dimension = |screen: u16, desired: u16| {
        if screen == 0 || desired == 0 {
            0
        } else if screen <= 2 {
            desired.min(screen)
        } else {
            desired.min(screen - 2).max(1)
        }
    };
    let width = dimension(screen_width, desired_width);
    let height = dimension(screen_height, desired_height);
    Rect {
        x: screen_width.saturating_sub(width) / 2,
        y: screen_height.saturating_sub(height) / 2,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{buffer::Buffer, config::Config, editor::BufferSlot};
    use std::path::PathBuf;

    #[test]
    fn canvas_marks_wide_grapheme_continuation() {
        let mut canvas = Canvas::new(8, 1, Style::default());
        assert_eq!(canvas.text(0, 0, "a界b", 8, Style::default()), 4);
        assert_eq!(canvas.cells[1].symbol, "界");
        assert!(canvas.cells[2].continuation);
        assert_eq!(canvas.cells[3].symbol, "b");
    }

    #[test]
    fn canvas_preserves_graphemes_larger_than_inline_cell_storage() {
        let mut canvas = Canvas::new(4, 1, Style::default());
        let family = "👩‍👩‍👧‍👦";
        assert_eq!(canvas.put_grapheme(0, 0, family, Style::default()), 2);
        assert_eq!(canvas.cells[0].symbol.as_str(), family);
        assert!(canvas.cells[1].continuation);
        assert_eq!(canvas.cells[0].clone().symbol.as_str(), family);
    }

    #[test]
    fn display_index_preserves_tabs_wide_and_combining_graphemes() {
        let text = "a界\t e\u{301}👩‍💻".repeat(700);
        let mut line = CachedLine::new(&text, syntax::Language::Plain, 4, 0);
        assert_eq!(
            line.at_column(&text, 2),
            DisplayPoint {
                byte: 1,
                grapheme: 1,
                column: 1
            }
        );
        assert!(
            line.scanned.byte < 20,
            "first lookup must not index an entire long line"
        );
        assert_eq!(
            line.at_column(&text, 7),
            DisplayPoint {
                byte: 9,
                grapheme: 5,
                column: 6
            }
        );
        assert_eq!(
            line.at_grapheme(&text, 3500),
            DisplayPoint {
                byte: 11664,
                grapheme: 3500,
                column: 4667
            }
        );
        assert_eq!(
            line.at_column(&text, 5000),
            DisplayPoint {
                byte: 12500,
                grapheme: 3750,
                column: 5000
            }
        );
        assert_eq!(line.at_grapheme(&text, 0), DisplayPoint::default());
        assert_eq!(
            line.at_column(&text, 3),
            DisplayPoint {
                byte: 4,
                grapheme: 2,
                column: 3
            }
        );
    }

    #[test]
    fn cached_syntax_updates_before_insert_transaction_is_committed() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cache.rs");
        std::fs::write(&path, "let value = 1;").unwrap();
        let mut editor = Editor::new(Config::default(), directory.path().to_owned());
        editor.open_path(&path).unwrap();
        let mut builder = FrameBuilder::new();
        let (before, _) = builder.draw_editor(&mut editor, 40, 5);
        assert_eq!(
            before.cells[4].style.fg,
            Palette::from_editor(&editor).keyword
        );
        let version = editor.active_buffer().version();
        editor.active_buffer_mut().begin_transaction().unwrap();
        editor
            .active_buffer_mut()
            .insert(crate::buffer::Pos::ZERO, "//")
            .unwrap();
        let (after, _) = builder.draw_editor(&mut editor, 40, 5);
        assert_eq!(editor.active_buffer().version(), version);
        assert!(row(&after, 0).contains("//let value"));
        assert_eq!(
            after.cells[4].style.fg,
            Palette::from_editor(&editor).comment
        );
    }

    #[test]
    fn display_cache_tracks_tab_width_and_replaced_scratch_buffers() {
        let mut editor = Editor::new(Config::default(), PathBuf::from("/work"));
        editor.buffers[0].buffer = Buffer::from_text("\tx");
        editor.config.editor.tab_width = 4;
        editor.active_pane_mut().cursor = crate::buffer::Pos::new(0, 1);
        let mut builder = FrameBuilder::new();
        assert_eq!(builder.draw_editor(&mut editor, 40, 5).1, Some((8, 0)));
        editor.config.editor.tab_width = 8;
        assert_eq!(builder.draw_editor(&mut editor, 40, 5).1, Some((12, 0)));
        editor.buffers[0].buffer = Buffer::from_text("replacement");
        let (canvas, cursor) = builder.draw_editor(&mut editor, 40, 5);
        assert_eq!(cursor, Some((5, 0)));
        assert!(row(&canvas, 0).contains("replacement"));
    }

    #[test]
    fn scrolling_releases_old_offscreen_line_indexes() {
        let mut editor = Editor::new(Config::default(), PathBuf::from("/work"));
        editor.buffers[0].buffer = Buffer::from_text("text\n".repeat(1000));
        let mut builder = FrameBuilder::new();
        for line in (0..1000).step_by(50) {
            editor.active_pane_mut().cursor = crate::buffer::Pos::new(line, 0);
            builder.draw_editor(&mut editor, 120, 40);
            assert!(builder.lines.len() <= 256);
        }
    }

    #[test]
    fn long_line_scrolling_keeps_bounded_syntax_and_visible_text() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("long.rs");
        let text = format!("{}END", "let value = 1; ".repeat(70_000));
        std::fs::write(&path, &text).unwrap();
        let mut editor = Editor::new(Config::default(), directory.path().to_owned());
        editor.open_path(&path).unwrap();
        let mut builder = FrameBuilder::new();
        let (first, _) = builder.draw_editor(&mut editor, 120, 40);
        assert!(row(&first, 0).contains("let value"));
        editor.active_pane_mut().cursor = crate::buffer::Pos::new(0, text.len() - 1);
        let (last, cursor) = builder.draw_editor(&mut editor, 120, 40);
        assert!(row(&last, 0).contains("END"));
        assert_eq!(cursor, Some((119, 0)));
        assert!(
            builder
                .lines
                .values()
                .flat_map(|line| &line.spans)
                .all(|span| span.end <= syntax::MAX_HIGHLIGHT_BYTES)
        );
    }

    #[test]
    fn canvas_never_stores_terminal_control_characters() {
        let mut canvas = Canvas::new(10, 1, Style::default());
        assert_eq!(
            canvas.text(0, 0, "a\n\r\t\u{1b}\u{7f}\u{9b}b", 10, Style::default()),
            8
        );
        assert_eq!(row(&canvas, 0), "a␊␍␉␛␡�b  ");
        assert!(
            canvas
                .cells
                .iter()
                .all(|cell| !cell.symbol.chars().any(char::is_control))
        );
    }

    #[test]
    fn external_message_newline_cannot_reach_the_terminal_frame() {
        let mut editor = Editor::new(Config::default(), PathBuf::from("/work"));
        editor.message("rust-analyzer: notify error\n");

        let (canvas, _) = draw_editor(&mut editor, 60, 8);

        assert!(row(&canvas, 7).contains("notify error␊"));
        assert!(
            canvas
                .cells
                .iter()
                .all(|cell| !cell.symbol.chars().any(char::is_control))
        );
    }

    #[test]
    fn vertical_lines_occupy_one_column() {
        let mut canvas = Canvas::new(5, 4, Style::default());
        canvas.vline(2, 0, 4, "│", Style::default());

        for y in 0_u16..4 {
            assert_eq!(canvas.cells[usize::from(y) * 5 + 2].symbol, "│");
        }
        assert_eq!(
            canvas
                .cells
                .iter()
                .filter(|cell| cell.symbol == "│")
                .count(),
            4
        );
    }

    #[test]
    fn unsupported_terminal_keys_are_not_treated_as_escape() {
        let mapped = map_key(event::KeyEvent::new(
            event::KeyCode::F(12),
            event::KeyModifiers::NONE,
        ));
        assert_eq!(mapped.code, KeyCode::Unknown);
    }

    #[test]
    fn cursor_shape_distinguishes_navigation_from_text_entry() {
        let mut editor = Editor::new(Config::default(), PathBuf::from("/work"));
        assert_eq!(cursor_style(&editor), SetCursorStyle::SteadyBlock);

        editor.handle_key(Key::char('i'));
        assert_eq!(cursor_style(&editor), SetCursorStyle::SteadyBar);
        editor.handle_key(Key::plain(KeyCode::Esc));
        assert_eq!(cursor_style(&editor), SetCursorStyle::SteadyBlock);

        editor.handle_key(Key::char(':'));
        assert_eq!(cursor_style(&editor), SetCursorStyle::SteadyBar);

        let mut picker = Editor::new(Config::default(), PathBuf::from("/work"));
        picker.show_picker_items(PickerKind::Files, Vec::new());
        assert_eq!(cursor_style(&picker), SetCursorStyle::SteadyBar);

        let mut visual = Editor::new(Config::default(), PathBuf::from("/work"));
        visual.handle_key(Key::char('v'));
        assert_eq!(cursor_style(&visual), SetCursorStyle::SteadyBlock);
    }

    fn row(canvas: &Canvas, y: u16) -> String {
        (0..canvas.width)
            .filter_map(|x| {
                let cell =
                    &canvas.cells[usize::from(y) * usize::from(canvas.width) + usize::from(x)];
                (!cell.continuation).then_some(cell.symbol.as_str())
            })
            .collect()
    }

    #[test]
    fn editor_frame_has_stable_gutter_and_status_chrome() {
        let mut editor = Editor::new(Config::default(), PathBuf::from("/work"));
        editor.buffers[0] = BufferSlot {
            buffer: Buffer::from_text("fn main() {}\n"),
            display_name: "src/main.rs".into(),
            large_file: false,
        };
        let (canvas, cursor) = draw_editor(&mut editor, 60, 10);
        assert!(row(&canvas, 0).starts_with("1   fn main()"));
        assert!(row(&canvas, 8).contains("NORMAL"));
        assert!(row(&canvas, 8).contains("src/main.rs"));
        assert_eq!(cursor, Some((4, 0)));
    }

    #[test]
    fn explorer_divider_is_vertical() {
        let mut editor = Editor::new(Config::default(), PathBuf::from("/work"));
        editor.explorer.open = true;

        let (canvas, _) = draw_editor(&mut editor, 60, 10);
        let divider = editor.explorer.width;

        for y in 0_u16..8 {
            let index = usize::from(y) * usize::from(canvas.width) + usize::from(divider);
            assert_eq!(canvas.cells[index].symbol, "│");
        }
        assert_ne!(
            canvas.cells[usize::from(divider.saturating_add(1))].symbol,
            "│"
        );
    }

    #[test]
    fn explorer_renders_tree_markers_indentation_and_scrolls_to_selection() {
        use crate::project::{ProjectEntry, ProjectEntryKind};
        use std::path::Path;
        let mut editor = Editor::new(Config::default(), PathBuf::from("/work"));
        editor.explorer.open = true;
        editor.focus = Focus::Explorer;
        editor.explorer.append_directory(
            Path::new("/work"),
            vec![ProjectEntry {
                path: PathBuf::from("/work/src"),
                relative_path: "src".into(),
                kind: ProjectEntryKind::Directory,
                depth: 1,
            }],
        );
        let (canvas, _) = draw_editor(&mut editor, 60, 10);
        assert!(row(&canvas, 1).starts_with(" ▸ src/"));
        editor.handle_key(Key::plain(KeyCode::Right));
        editor.explorer.append_directory(
            Path::new("/work/src"),
            (0..20)
                .map(|index| ProjectEntry {
                    path: PathBuf::from(format!("/work/src/file{index:02}.rs")),
                    relative_path: PathBuf::new(),
                    kind: ProjectEntryKind::File,
                    depth: 1,
                })
                .collect(),
        );
        let (canvas, _) = draw_editor(&mut editor, 60, 10);
        assert!(row(&canvas, 1).starts_with(" ▾ src/"));
        assert!(row(&canvas, 2).starts_with("     file00.rs"));
        editor.explorer.move_selection(20);
        let (canvas, _) = draw_editor(&mut editor, 60, 10);
        assert!(row(&canvas, 6).starts_with("     file19.rs"));
        editor.handle_key(Key::plain(KeyCode::Left));
        editor.handle_key(Key::plain(KeyCode::Enter));
        let (canvas, _) = draw_editor(&mut editor, 60, 10);
        assert!(row(&canvas, 1).starts_with(" ▸ src/"));
        assert!(!row(&canvas, 2).contains("file"));
    }

    #[test]
    fn leader_overlay_is_discoverable_and_non_destructive() {
        let mut editor = Editor::new(Config::default(), PathBuf::from("/work"));
        editor.handle_key(Key::char(' '));
        let (canvas, _) = draw_editor(&mut editor, 70, 20);
        let frame = (0..canvas.height)
            .map(|y| row(&canvas, y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(frame.contains("which key"));
        assert!(frame.contains("+ buffers"));
        assert!(frame.contains("t  Terminal"));
        assert!(matches!(editor.mode, Mode::Leader));
    }

    #[test]
    fn integrated_terminal_uses_a_bottom_panel_and_owns_the_cursor_when_focused() {
        let mut editor = Editor::new(Config::default(), PathBuf::from("/work"));
        editor.terminal.visible = true;
        editor.terminal.status = crate::terminal::TerminalStatus::Running;
        editor.terminal.process(b"prompt> \x1b[31mred\x1b[0m");
        editor.focus = Focus::Terminal;

        let (canvas, cursor) = draw_editor(&mut editor, 60, 20);
        let frame = (0..canvas.height)
            .map(|y| row(&canvas, y))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(frame.contains("TERMINAL  running"));
        assert!(frame.contains("prompt> red"));
        assert!(cursor.is_some_and(|(_, y)| (12..18).contains(&y)));
        assert_eq!(editor.terminal.screen().size(), (5, 60));
    }

    #[test]
    fn drawing_scrolls_horizontally_to_keep_the_cursor_visible() {
        let mut editor = Editor::new(Config::default(), PathBuf::from("/work"));
        editor.buffers[0] = BufferSlot {
            buffer: Buffer::from_text("abcdefghijklmnopqrstuvwxyz"),
            display_name: "long.rs".into(),
            large_file: false,
        };
        editor.active_pane_mut().cursor = crate::buffer::Pos::new(0, 20);

        let (canvas, cursor) = draw_editor(&mut editor, 12, 6);

        assert!(editor.active_pane().viewport_column > 0);
        let (x, y) = cursor.expect("active cursor should remain visible");
        assert!(x < canvas.width);
        assert!(y < canvas.height.saturating_sub(2));
    }

    #[test]
    fn overlays_are_safe_at_degenerate_terminal_sizes() {
        let sizes = [(0, 0), (0, 8), (8, 0), (1, 8), (8, 1), (1, 1)];
        for (width, height) in sizes {
            let mut leader = Editor::new(Config::default(), PathBuf::from("/work"));
            leader.mode = Mode::Leader;
            let (canvas, _) = draw_editor(&mut leader, width, height);
            assert_eq!((canvas.width, canvas.height), (width, height));

            let mut picker = Editor::new(Config::default(), PathBuf::from("/work"));
            picker.show_picker_items(
                PickerKind::Messages,
                vec![crate::editor::PickerItem {
                    label: "item".into(),
                    detail: String::new(),
                    path: None,
                    line: None,
                    insert_text: None,
                }],
            );
            let (canvas, _) = draw_editor(&mut picker, width, height);
            assert_eq!((canvas.width, canvas.height), (width, height));
        }
    }

    #[test]
    fn codex_spinners_render_beside_working_lines_without_displacing_diagnostics_or_text() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("source.rs");
        std::fs::write(&path, "first\n// @codex fix this\nlast\n").unwrap();
        let mut editor = Editor::new(Config::default(), directory.path().to_owned());
        editor.open_path(&path).unwrap();
        editor.codex_working_lines.insert(
            path.canonicalize().unwrap(),
            std::collections::BTreeSet::from([1]),
        );
        editor.diagnostics.push(crate::editor::Diagnostic {
            path: editor.active_buffer().path().map(std::path::Path::to_owned),
            line: 1,
            column: 0,
            severity: DiagnosticSeverity::Error,
            message: "problem".into(),
            version: editor.active_buffer().revision(),
        });
        let (frame, cursor) = draw_editor(&mut editor, 60, 10);
        assert!(row(&frame, 1).starts_with("1⠋● // @codex fix this"));
        assert!(!row(&frame, 0).contains('⠋'));
        assert!(!row(&frame, 2).contains('⠋'));
        editor.codex_spinner_frame = 1;
        let (next, next_cursor) = draw_editor(&mut editor, 60, 10);
        assert!(row(&next, 1).starts_with("1⠙● // @codex fix this"));
        assert_eq!(cursor, next_cursor);
        editor.split(Orientation::Horizontal);
        let (split, _) = draw_editor(&mut editor, 60, 12);
        assert!(row(&split, 1).contains('⠙'));
        assert!(row(&split, 6).contains('⠙'));
        for width in 0..4 {
            let (narrow, _) = draw_editor(&mut editor, width, 12);
            assert_eq!(narrow.width, width);
        }
        editor.codex_working_lines.clear();
        editor.codex_working_lines.insert(
            directory.path().join("other.rs"),
            std::collections::BTreeSet::from([1]),
        );
        let (other, _) = draw_editor(&mut editor, 60, 12);
        assert!(!row(&other, 1).contains('⠙'));
        assert!(!row(&other, 6).contains('⠙'));
    }

    #[test]
    fn gutter_and_inline_text_render_only_current_version_diagnostics() {
        let mut editor = Editor::new(Config::default(), PathBuf::from("/work"));
        editor
            .active_buffer_mut()
            .insert(crate::buffer::Pos::ZERO, "x")
            .unwrap();
        let current = editor.active_buffer().revision();
        editor.diagnostics.push(crate::editor::Diagnostic {
            path: None,
            line: 0,
            column: 0,
            severity: DiagnosticSeverity::Error,
            message: "stale".into(),
            version: current.saturating_sub(1),
        });

        let (stale, _) = draw_editor(&mut editor, 20, 5);
        assert_eq!(stale.cells[2].symbol, " ");
        assert!(!row(&stale, 0).contains("stale"));

        editor.diagnostics[0].version = current;
        let (current_frame, _) = draw_editor(&mut editor, 20, 5);
        assert_eq!(current_frame.cells[2].symbol, "●");
        assert!(row(&current_frame, 0).contains("x  ● stale"));
    }

    #[test]
    fn uncommitted_insert_immediately_hides_the_previous_diagnostic_revision() {
        let mut editor = Editor::new(Config::default(), PathBuf::from("/work"));
        editor.buffers[0] = BufferSlot {
            buffer: Buffer::from_text("x"),
            display_name: "src/main.rs".into(),
            large_file: false,
        };
        editor.diagnostics.push(crate::editor::Diagnostic {
            path: None,
            line: 0,
            column: 0,
            severity: DiagnosticSeverity::Error,
            message: "old parse".into(),
            version: editor.active_buffer().revision(),
        });
        assert!(row(&draw_editor(&mut editor, 30, 5).0, 0).contains("old parse"));

        let committed_version = editor.active_buffer().version();
        editor.active_buffer_mut().begin_transaction().unwrap();
        editor
            .active_buffer_mut()
            .insert(crate::buffer::Pos::new(0, 1), "y")
            .unwrap();

        assert_eq!(editor.active_buffer().version(), committed_version);
        assert!(!row(&draw_editor(&mut editor, 30, 5).0, 0).contains("old parse"));
    }

    #[test]
    fn inline_diagnostics_put_errors_first_and_use_severity_colors() {
        let mut editor = Editor::new(Config::default(), PathBuf::from("/work"));
        editor.buffers[0] = BufferSlot {
            buffer: Buffer::from_text("let value = missing;"),
            display_name: "src/main.rs".into(),
            large_file: false,
        };
        let version = editor.active_buffer().revision();
        editor.diagnostics = vec![
            crate::editor::Diagnostic {
                path: None,
                line: 0,
                column: 0,
                severity: DiagnosticSeverity::Warning,
                message: "warning text".into(),
                version,
            },
            crate::editor::Diagnostic {
                path: None,
                line: 0,
                column: 12,
                severity: DiagnosticSeverity::Error,
                message: "error text".into(),
                version,
            },
        ];

        let (canvas, _) = draw_editor(&mut editor, 80, 5);
        let rendered = row(&canvas, 0);
        let error_byte = rendered.find("● error text").unwrap();
        let warning_byte = rendered.find("▲ warning text").unwrap();
        let error_x = UnicodeWidthStr::width(&rendered[..error_byte]);
        let warning_x = UnicodeWidthStr::width(&rendered[..warning_byte]);

        assert!(error_x < warning_x);
        assert_eq!(canvas.cells[error_x].style.fg, Color(247, 118, 142));
        assert_eq!(canvas.cells[warning_x].style.fg, Color(224, 175, 104));
    }
}
