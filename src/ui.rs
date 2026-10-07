//! Crossterm frontend.  Rendering is cell-diffed, while the editing core only
//! deals in [`crate::input::Key`] values.

use std::{
    borrow::Cow,
    collections::HashMap,
    io::{self, Stdout, Write},
    ops::Deref,
    panic,
    time::{Duration, Instant},
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
    animation::Motion,
    buffer::Buffer,
    check::{CheckEntry, CheckLevel, CheckStatus},
    command::{self, CommandSource},
    config::parse_hex_color,
    editor::{
        DiagnosticSeverity, Editor, Focus, InlayHint, Layout, Mode, Orientation, Pane, PickerKind,
        VisualKind,
    },
    input::{Key, KeyCode, Modifiers},
    syntax::{self, Highlight},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Color(pub u8, pub u8, pub u8);

impl Color {
    /// Derive quiet surfaces from the user's theme, including custom light themes.
    fn mix(self, other: Self, percent: u16) -> Self {
        let channel =
            |a: u8, b: u8| ((u16::from(a) * (100 - percent) + u16::from(b) * percent) / 100) as u8;
        Self(
            channel(self.0, other.0),
            channel(self.1, other.1),
            channel(self.2, other.2),
        )
    }
}

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
        Self::new(Color(220, 228, 227), Color(16, 22, 25))
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
    /// Backend-neutral cells, including wide-glyph continuation cells.
    #[cfg(feature = "gui")]
    pub(crate) fn cells(&self) -> impl Iterator<Item = (u16, u16, &str, Style, bool)> {
        self.cells.iter().enumerate().map(|(i, cell)| {
            (
                (i % usize::from(self.width)) as u16,
                (i / usize::from(self.width)) as u16,
                cell.symbol.as_str(),
                cell.style,
                cell.continuation,
            )
        })
    }

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
    surface: Color,
    border: Color,
    cursor_line: Color,
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
        let background = color(&theme.background, Color(16, 22, 25));
        let status = color(&theme.status, Color(27, 37, 43));
        let muted = color(&theme.muted, Color(119, 133, 138));
        Self {
            background,
            foreground: color(&theme.foreground, Color(220, 228, 227)),
            muted,
            accent: color(&theme.accent, Color(139, 213, 182)),
            status,
            error: color(&theme.error, Color(247, 118, 142)),
            warning: color(&theme.warning, Color(224, 175, 104)),
            info: color(&theme.info, Color(139, 191, 216)),
            selection: color(&theme.selection, Color(43, 69, 72)),
            surface: background.mix(status, 55),
            border: background.mix(muted, 35),
            cursor_line: background.mix(status, 65),
            keyword: Color(195, 166, 221),
            string: Color(186, 216, 154),
            comment: muted,
            type_name: Color(226, 194, 141),
            number: Color(223, 169, 143),
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
    motions: HashMap<u64, PaneMotion>,
    animation_time: Option<Instant>,
    animating: bool,
    native_chrome: bool,
    #[cfg(feature = "gui")]
    hit_panes: Vec<(Pane, Rect)>,
    #[cfg(feature = "gui")]
    hit_terminal: Option<Rect>,
    #[cfg(feature = "gui")]
    hit_check: Option<Rect>,
}

struct PaneMotion {
    identity: (u64, u64),
    rect: Rect,
    viewport: Motion,
    cursor: Option<Motion>,
    last_viewport: (usize, usize),
    scrolled: bool,
    used_frame: u64,
}

impl FrameBuilder {
    #[cfg(feature = "gui")]
    pub(crate) fn draw_workspace(
        &mut self,
        editor: &mut Editor,
        width: u16,
        height: u16,
    ) -> (Canvas, Option<(u16, u16)>) {
        self.native_chrome = true;
        let frame = self.draw_animated_at(editor, width, height, Instant::now());
        self.native_chrome = false;
        frame
    }

    #[cfg(feature = "gui")]
    pub(crate) fn place_cursor(&mut self, editor: &mut Editor, x: u16, y: u16) {
        if editor.git.visible || editor.picker.is_some() || editor.mode == Mode::Leader {
            return;
        }
        let contains = |r: Rect| x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height;
        if self.hit_terminal.is_some_and(contains) || self.hit_check.is_some_and(contains) {
            let terminal = self.hit_terminal.is_some_and(contains);
            editor.focus = Focus::Editor;
            editor.handle_key(Key::plain(KeyCode::Esc));
            editor.focus = if terminal {
                Focus::Terminal
            } else {
                Focus::Check
            };
            return;
        }
        let Some((shown, rect)) = self
            .hit_panes
            .iter()
            .find(|(_, r)| x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height)
            .cloned()
        else {
            return;
        };
        // Finish transactions before switching panes or moving the insertion point.
        editor.focus = Focus::Editor;
        editor.handle_key(Key::plain(KeyCode::Esc));
        editor.active_pane = shown.id;
        editor.focus = Focus::Editor;
        let buffer = editor.active_buffer();
        let line = (shown.viewport_line + usize::from(y - rect.y))
            .min(buffer.line_count().saturating_sub(1));
        let digits = buffer.line_count().max(1).to_string().len() as u16;
        let gutter = (digits + 3 + git_gutter(editor, buffer)).min(rect.width.saturating_sub(1));
        let target = usize::from(x.saturating_sub(rect.x + gutter)) + shown.viewport_column;
        let text = buffer.line(line).unwrap_or("");
        let hints = line_hints(editor, buffer, line);
        let (mut column, mut source, mut index) = (0usize, 0usize, 0usize);
        for (i, g) in text.graphemes(true).enumerate() {
            column += hints
                .iter()
                .filter(|h| h.position.grapheme == i)
                .map(|h| UnicodeWidthStr::width(h.label.as_str()))
                .sum::<usize>();
            let width = if g == "\t" {
                editor.config.editor.tab_width - source % editor.config.editor.tab_width
            } else {
                UnicodeWidthStr::width(g).max(1)
            };
            if target < column + width {
                index = i;
                break;
            }
            source += width;
            column += width;
            index = i + 1;
        }
        editor.active_pane_mut().cursor = crate::buffer::Pos::new(line, index);
        editor.active_pane_mut().desired_column = index;
    }

    pub fn new() -> Self {
        Self::default()
    }

    pub fn draw_editor(
        &mut self,
        editor: &mut Editor,
        width: u16,
        height: u16,
    ) -> (Canvas, Option<(u16, u16)>) {
        self.animation_time = None;
        self.draw_frame(editor, width, height)
    }

    /// Advance presentation without delaying input or changing logical panes.
    pub fn draw_animated_at(
        &mut self,
        editor: &mut Editor,
        width: u16,
        height: u16,
        now: Instant,
    ) -> (Canvas, Option<(u16, u16)>) {
        self.animation_time = Some(now);
        self.draw_frame(editor, width, height)
    }

    pub fn is_animating(&self) -> bool {
        self.animating
    }

    fn draw_frame(
        &mut self,
        editor: &mut Editor,
        width: u16,
        height: u16,
    ) -> (Canvas, Option<(u16, u16)>) {
        self.animating = false;
        #[cfg(feature = "gui")]
        self.hit_panes.clear();
        self.frame = self.frame.wrapping_add(1);
        let result = build_frame(self, editor, width, height);
        if self.lines.len() > 256 {
            self.lines.retain(|_, line| line.used_frame == self.frame);
        }
        self.motions
            .retain(|_, motion| motion.used_frame == self.frame);
        result
    }

    fn presented_pane(&mut self, editor: &Editor, pane: &Pane, rect: Rect) -> Pane {
        let mut shown = pane.clone();
        let Some(now) = self.animation_time.filter(|_| {
            editor.mode == Mode::Normal
                && editor.focus == Focus::Editor
                && editor.picker.is_none()
                && editor.hover.is_none()
                && !editor.git.visible
        }) else {
            self.motions.remove(&pane.id);
            return shown;
        };
        let buffer = &editor.buffers[pane.buffer].buffer;
        let identity = (buffer.line_cache_key(0).unwrap_or(0), buffer.revision());
        let target = (pane.viewport_column, pane.viewport_line);
        let motion = self.motions.entry(pane.id).or_insert_with(|| PaneMotion {
            identity,
            rect,
            viewport: Motion::new(target, now),
            cursor: None,
            last_viewport: target,
            scrolled: false,
            used_frame: self.frame,
        });
        if motion.identity != identity || motion.rect != rect {
            motion.identity = identity;
            motion.rect = rect;
            motion.viewport = Motion::new(target, now);
            motion.cursor = None;
        }
        motion.used_frame = self.frame;
        if editor.config.ui.smooth_scroll {
            motion.viewport.retarget(
                target,
                now,
                Duration::from_millis(100),
                (usize::from(rect.width / 2), usize::from(rect.height / 2)),
            );
        } else {
            motion.viewport = Motion::new(target, now);
        }
        let (column, line) = motion.viewport.position(now);
        motion.scrolled = motion.viewport.active(now) || motion.last_viewport != (column, line);
        motion.last_viewport = (column, line);
        shown.viewport_column = column;
        shown.viewport_line = line;
        self.animating |= motion.viewport.active(now);
        shown
    }

    fn presented_cursor(
        &mut self,
        canvas: &Canvas,
        editor: &Editor,
        id: u64,
        cursor: Option<(u16, u16)>,
    ) -> Option<(u16, u16)> {
        let Some(now) = self.animation_time else {
            return cursor;
        };
        let Some(motion) = self.motions.get_mut(&id) else {
            return cursor;
        };
        let Some((x, y)) = cursor else {
            if motion.viewport.active(now) {
                return motion.cursor.as_ref().map(|cursor| {
                    let (x, y) = cursor.position(now);
                    let (mut x, y) = (x as u16, y as u16);
                    while x > motion.rect.x
                        && canvas
                            .index(x, y)
                            .is_some_and(|i| canvas.cells[i].continuation)
                    {
                        x -= 1;
                    }
                    (x, y)
                });
            }
            motion.cursor = None;
            return None;
        };
        let target = (usize::from(x), usize::from(y));
        if !editor.config.ui.cursor_animation || motion.scrolled {
            motion.cursor = Some(Motion::new(target, now));
            return cursor;
        }
        let tween = motion
            .cursor
            .get_or_insert_with(|| Motion::new(target, now));
        tween.retarget(
            target,
            now,
            Duration::from_millis(70),
            (
                usize::from(motion.rect.width),
                usize::from(motion.rect.height),
            ),
        );
        let (x, y) = tween.position(now);
        self.animating |= tween.active(now);
        let (mut x, y) = (x as u16, y as u16);
        // Never place the terminal cursor on the trailing cell of a wide glyph.
        while x > motion.rect.x
            && canvas
                .index(x, y)
                .is_some_and(|i| canvas.cells[i].continuation)
        {
            x -= 1;
        }
        Some((x, y))
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
    if editor.explorer.open && content.width >= 40 && !builder.native_chrome {
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
                Style::new(palette.border, palette.surface),
            );
        }
        content.x = explorer_width.saturating_add(1);
        content.width = content
            .width
            .saturating_sub(explorer_width.saturating_add(1));
    }
    let check_rect = if editor.check.visible {
        let (panes, panel) = check_panel_layout(content);
        content = panes;
        panel
    } else {
        None
    };
    #[cfg(feature = "gui")]
    {
        builder.hit_terminal = terminal_rect;
        builder.hit_check = check_rect;
    }

    let mut pane_rects = Vec::new();
    layout_rects(&editor.layout, content, &mut pane_rects);
    sync_viewports(builder, editor, &pane_rects);
    let mut editor_cursor = None;
    for (pane_id, rect) in &pane_rects {
        if let Some(pane) = editor.panes.iter().find(|pane| pane.id == *pane_id) {
            let shown = builder.presented_pane(editor, pane, *rect);
            #[cfg(feature = "gui")]
            builder.hit_panes.push((shown.clone(), *rect));
            let cursor = render_pane(builder, &mut canvas, editor, &shown, *rect, palette);
            if *pane_id == editor.active_pane {
                editor_cursor = builder.presented_cursor(&canvas, editor, *pane_id, cursor);
            }
        }
    }
    render_split_lines(&mut canvas, &editor.layout, content, palette);
    if let Some(rect) = check_rect {
        canvas.vline(
            rect.x.saturating_sub(1),
            rect.y,
            rect.height,
            "│",
            Style::new(palette.border, palette.surface),
        );
        render_check_panel(&mut canvas, editor, rect, palette);
    }
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

    if editor.mode == Mode::Normal
        && editor.focus == Focus::Editor
        && editor.picker.is_none()
        && let Some(cursor) = editor_cursor
        && let Some((_, rect)) = pane_rects.iter().find(|(id, _)| *id == editor.active_pane)
    {
        render_hover(&mut canvas, editor, *rect, cursor, palette);
    }
    if matches!(editor.mode, Mode::Leader) {
        render_leader(&mut canvas, editor, palette);
    }
    if editor.picker.is_some() {
        render_picker(&mut canvas, editor, palette);
    }
    if editor.git.visible {
        render_git(&mut canvas, editor, palette);
        return (canvas, None);
    }

    let cursor = if let Some(picker) = &editor.picker {
        let rect = picker_rect(width, height, picker.kind);
        let query = text_tail(&picker.query, rect.width.saturating_sub(7));
        let prompt_width = UnicodeWidthStr::width(query.as_ref()) as u16;
        (rect.width >= 8 && rect.height >= 6).then_some((
            rect.x
                .saturating_add(4)
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
            Mode::Leader => None,
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

fn render_git(canvas: &mut Canvas, editor: &mut Editor, palette: Palette) {
    let rect = Rect {
        x: 0,
        y: 0,
        width: canvas.width,
        height: canvas.height.saturating_sub(1),
    };
    if rect.height == 0 {
        return;
    }
    canvas.fill(rect, " ", Style::new(palette.foreground, palette.surface));
    let panel = &mut editor.git;
    let title = if panel.loading {
        "Git · loading…".to_owned()
    } else if let Some(doc) = &panel.document {
        doc.title.clone()
    } else {
        format!(
            "Git · {} · staged / unstaged",
            panel
                .snapshot
                .as_ref()
                .map_or("unavailable", |s| s.branch.as_str())
        )
    };
    canvas.text(
        0,
        0,
        &title,
        rect.width,
        Style::new(palette.accent, palette.status).bold(),
    );
    let body = usize::from(rect.height.saturating_sub(2));
    if let Some(error) = &panel.error {
        for (i, row) in wrap_text(error, usize::from(rect.width), 0)
            .iter()
            .take(body)
            .enumerate()
        {
            canvas.text(
                0,
                1 + i as u16,
                row,
                rect.width,
                Style::new(palette.error, palette.surface),
            );
        }
    } else if let Some(doc) = &panel.document {
        if rect.width >= 64
            && doc
                .rows
                .iter()
                .any(|row| matches!(row, crate::git::DiffRow::Lines { .. }))
        {
            render_split_diff(
                canvas,
                doc,
                &mut panel.scroll,
                panel.horizontal,
                rect,
                palette,
            );
        } else {
            panel.scroll = panel
                .scroll
                .min(doc.lines.len().saturating_sub(body.max(1)));
            for (i, line) in doc.lines.iter().skip(panel.scroll).take(body).enumerate() {
                let color = if line.starts_with('+') {
                    palette.accent
                } else if line.starts_with('-') {
                    palette.error
                } else if line.starts_with("@@") {
                    palette.info
                } else {
                    palette.foreground
                };
                let text: String = line
                    .graphemes(true)
                    .skip(panel.horizontal)
                    .take(usize::from(rect.width))
                    .collect();
                canvas.text(
                    0,
                    1 + i as u16,
                    &text,
                    rect.width,
                    Style::new(color, palette.surface),
                );
            }
        }
    } else if let Some(snapshot) = &panel.snapshot {
        panel.scroll = panel.scroll.min(panel.selected);
        if panel.selected >= panel.scroll.saturating_add(body) {
            panel.scroll = panel.selected.saturating_sub(body.saturating_sub(1));
        }
        if snapshot.entries.is_empty() && body > 0 {
            canvas.text(
                1,
                1,
                "Working tree clean",
                rect.width.saturating_sub(1),
                Style::new(palette.muted, palette.surface),
            );
        }
        for (i, entry) in snapshot
            .entries
            .iter()
            .enumerate()
            .skip(panel.scroll)
            .take(body)
        {
            let y = 1 + (i - panel.scroll) as u16;
            let bg = if i == panel.selected {
                palette.selection
            } else {
                palette.surface
            };
            canvas.fill(
                Rect {
                    y,
                    height: 1,
                    ..rect
                },
                " ",
                Style::new(palette.foreground, bg),
            );
            let label = format!("{}{} {}", entry.index, entry.worktree, entry.path.display());
            let text: String = label
                .graphemes(true)
                .skip(panel.horizontal)
                .take(usize::from(rect.width))
                .collect();
            canvas.text(0, y, &text, rect.width, Style::new(palette.foreground, bg));
        }
    }
    if rect.height >= 2 {
        canvas.text(0, rect.height - 1, "j/k move  d/D/H diff  s stage  u unstage  o open  r refresh  Backspace status  q close",
            rect.width, Style::new(palette.muted, palette.status));
    }
}

fn render_split_diff(
    canvas: &mut Canvas,
    doc: &crate::git::Document,
    scroll: &mut usize,
    horizontal: usize,
    rect: Rect,
    palette: Palette,
) {
    let middle = rect.width / 2;
    let right = middle + 1;
    let body = usize::from(rect.height.saturating_sub(3));
    if rect.height < 3 {
        return;
    }
    canvas.text(
        0,
        1,
        "  BEFORE",
        middle,
        Style::new(palette.error, palette.status).bold(),
    );
    canvas.text(
        right,
        1,
        "  AFTER",
        rect.width - right,
        Style::new(palette.accent, palette.status).bold(),
    );
    *scroll = (*scroll).min(doc.rows.len().saturating_sub(body.max(1)));
    for (i, row) in doc.rows.iter().skip(*scroll).take(body).enumerate() {
        let y = 2 + i as u16;
        match row {
            crate::git::DiffRow::Header(text) => {
                canvas.text(
                    0,
                    y,
                    text,
                    rect.width,
                    Style::new(palette.info, palette.status),
                );
            }
            crate::git::DiffRow::Lines { old, new } => {
                for (line, x, width, color, marker) in [
                    (old, 0, middle, palette.error, '-'),
                    (new, right, rect.width - right, palette.accent, '+'),
                ] {
                    let bg = if line.as_ref().is_some_and(|l| l.changed) {
                        palette.surface.mix(color, 12)
                    } else {
                        palette.surface
                    };
                    canvas.fill(
                        Rect {
                            x,
                            y,
                            width,
                            height: 1,
                        },
                        " ",
                        Style::new(palette.foreground, bg),
                    );
                    if let Some(line) = line {
                        let number = format!(
                            "{:>5} {} ",
                            line.number,
                            if line.changed { marker } else { ' ' }
                        );
                        let gutter = (number.len() as u16).min(width);
                        canvas.text(
                            x,
                            y,
                            &number,
                            gutter,
                            Style::new(if line.changed { color } else { palette.muted }, bg),
                        );
                        let text = if line.no_newline {
                            Cow::Owned(format!("{}  [no newline]", line.text))
                        } else {
                            Cow::Borrowed(line.text.as_str())
                        };
                        draw_diff_text(
                            canvas,
                            x + gutter,
                            y,
                            &text,
                            width - gutter,
                            horizontal,
                            Style::new(palette.foreground, bg),
                        );
                    }
                }
                canvas.put_grapheme(middle, y, "│", Style::new(palette.border, palette.surface));
            }
        }
    }
}

/// Horizontal offsets are display cells on both sides. Tabs have fixed stops;
/// clipping a wide grapheme leaves a blank instead of breaking alignment.
fn draw_diff_text(
    canvas: &mut Canvas,
    x: u16,
    y: u16,
    text: &str,
    width: u16,
    offset: usize,
    style: Style,
) {
    let mut column = 0usize;
    for g in text.graphemes(true) {
        let cells = if g == "\t" {
            4 - column % 4
        } else {
            UnicodeWidthStr::width(g).max(1)
        };
        if column >= offset {
            let local = column - offset;
            if local.saturating_add(cells) > usize::from(width) {
                break;
            }
            if g != "\t" {
                canvas.put_grapheme(x + local as u16, y, g, style);
            }
        }
        column = column.saturating_add(cells);
    }
}

/// Dock the cargo check panel right of the panes, only when both keep a
/// usable width.
fn check_panel_layout(content: Rect) -> (Rect, Option<Rect>) {
    const MIN_PANEL: u16 = 28;
    const MIN_PANES: u16 = 30;
    if content.width < MIN_PANEL + MIN_PANES + 1 || content.height == 0 {
        return (content, None);
    }
    let preferred = u16::try_from(u32::from(content.width) * 2 / 5).unwrap_or(u16::MAX);
    let width = preferred
        .clamp(MIN_PANEL, 80)
        .min(content.width - MIN_PANES - 1);
    let panel = Rect {
        x: content.x + content.width - width,
        width,
        ..content
    };
    let panes = Rect {
        width: content.width - width - 1,
        ..content
    };
    (panes, Some(panel))
}

fn render_check_panel(canvas: &mut Canvas, editor: &mut Editor, rect: Rect, palette: Palette) {
    let focused = editor.focus == Focus::Check;
    canvas.fill(rect, " ", Style::new(palette.foreground, palette.surface));
    let header = Rect { height: 1, ..rect };
    canvas.fill(header, " ", Style::new(palette.foreground, palette.status));
    let title_width = canvas.text(
        header.x.saturating_add(1),
        header.y,
        &format!("CARGO {}", editor.check.command.name().to_ascii_uppercase()),
        header.width.saturating_sub(2),
        Style::new(
            if focused {
                palette.accent
            } else {
                palette.muted
            },
            palette.status,
        )
        .bold(),
    );
    let (errors, warnings) = editor.check.counts();
    let status_color = match &editor.check.status {
        CheckStatus::Running => palette.info,
        CheckStatus::Failed(_) => palette.error,
        CheckStatus::Finished { success, .. } if errors > 0 || !success => palette.error,
        CheckStatus::Finished { .. } if warnings > 0 => palette.warning,
        CheckStatus::Finished { .. } => palette.accent,
        CheckStatus::Idle | CheckStatus::Cancelled => palette.muted,
    };
    let status_width = header.width.saturating_sub(title_width).saturating_sub(4);
    let mut status = editor.check.summary();
    if let CheckStatus::Finished { elapsed, .. } = editor.check.status {
        let timed = format!("{status} · {:.1}s", elapsed.as_secs_f64());
        if UnicodeWidthStr::width(timed.as_str()) <= usize::from(status_width) {
            status = timed;
        }
    }
    canvas.text(
        header.x.saturating_add(title_width).saturating_add(3),
        header.y,
        &status,
        status_width,
        Style::new(status_color, palette.status),
    );

    let footer_height = u16::from(rect.height >= 3);
    let body = Rect {
        y: rect.y.saturating_add(1),
        height: rect.height.saturating_sub(1).saturating_sub(footer_height),
        ..rect
    };
    if footer_height > 0 {
        let omitted = editor.check.omitted();
        let mut hints = if omitted > 0 {
            format!("{omitted} more not shown · ")
        } else {
            String::new()
        };
        hints.push_str(if focused {
            "Enter open · r rerun · s stop · q back"
        } else {
            "Ctrl-W l focus · <Space>cw hide"
        });
        canvas.text(
            rect.x.saturating_add(1),
            rect.y + rect.height - 1,
            &hints,
            rect.width.saturating_sub(2),
            Style::new(palette.muted, palette.surface),
        );
    }
    if body.height == 0 || body.width < 4 {
        return;
    }

    let text_width = usize::from(body.width.saturating_sub(2));
    let entries = editor.check.entries();
    if entries.is_empty() {
        let (text, color) = match &editor.check.status {
            CheckStatus::Idle => ("Not run yet".into(), palette.muted),
            CheckStatus::Running => (
                format!("Running cargo {}…", editor.check.command.name()),
                palette.muted,
            ),
            CheckStatus::Cancelled => ("Cancelled".into(), palette.muted),
            CheckStatus::Failed(error) => (error.clone(), palette.error),
            CheckStatus::Finished { success: false, .. } => (
                format!(
                    "cargo {} failed without diagnostics",
                    editor.check.command.name()
                ),
                palette.error,
            ),
            CheckStatus::Finished { .. } => ("✓ No errors or warnings".into(), palette.accent),
        };
        for (row, line) in wrap_text(&text, text_width, 0)
            .iter()
            .take(usize::from(body.height))
            .enumerate()
        {
            canvas.text(
                body.x.saturating_add(1),
                body.y.saturating_add(row as u16),
                line,
                body.width.saturating_sub(2),
                Style::new(color, palette.surface),
            );
        }
        return;
    }

    // Keep the whole selected entry visible, measuring only the entries
    // between the selection and the earliest row that can stay on screen.
    let selected = editor.check.selected.min(entries.len() - 1);
    let mut scroll = editor.check.scroll.min(selected);
    let room = usize::from(body.height).saturating_add(1);
    let mut used = 0;
    for index in (scroll..=selected).rev() {
        used += check_entry_rows(&entries[index], text_width).len() + 1;
        if used > room {
            scroll = (index + 1).min(selected);
            break;
        }
    }

    let mut y = body.y;
    let bottom = body.y.saturating_add(body.height);
    'entries: for (index, entry) in entries.iter().enumerate().skip(scroll) {
        let highlighted = focused && index == selected;
        let background = if highlighted {
            palette.selection
        } else {
            palette.surface
        };
        for (text, kind) in check_entry_rows(entry, text_width) {
            if y >= bottom {
                break 'entries;
            }
            let foreground = match kind {
                CheckRow::Title => match entry.level {
                    Some(CheckLevel::Error) => palette.error,
                    Some(CheckLevel::Warning) => palette.warning,
                    None if entry.title.starts_with("error") => palette.error,
                    None if entry.title.starts_with("warning") => palette.warning,
                    None => palette.foreground,
                },
                CheckRow::Origin => palette.info,
                CheckRow::Label => palette.foreground,
                CheckRow::Note => palette.muted,
            };
            let mut style = Style::new(foreground, background);
            if kind == CheckRow::Title && entry.level.is_some() {
                style = style.bold();
            }
            canvas.fill(
                Rect {
                    y,
                    height: 1,
                    ..body
                },
                " ",
                Style::new(palette.foreground, background),
            );
            canvas.text(
                body.x.saturating_add(1),
                y,
                &text,
                body.width.saturating_sub(2),
                style,
            );
            y += 1;
        }
        y = y.saturating_add(1);
    }
    editor.check.selected = selected;
    editor.check.scroll = scroll;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CheckRow {
    Title,
    Origin,
    Label,
    Note,
}

/// Display rows for one check entry, wrapped to `width` cells.
fn check_entry_rows(entry: &CheckEntry, width: usize) -> Vec<(String, CheckRow)> {
    let marker = match entry.level {
        Some(CheckLevel::Error) => "● ",
        Some(CheckLevel::Warning) => "▲ ",
        None => "",
    };
    let mut rows = Vec::new();
    let mut push = |text: &str, kind: CheckRow, indent: usize| {
        for line in text.lines() {
            rows.extend(
                wrap_text(line, width, indent)
                    .into_iter()
                    .map(|row| (row, kind)),
            );
        }
    };
    push(&format!("{marker}{}", entry.title), CheckRow::Title, 2);
    if let Some(origin) = &entry.origin {
        push(&format!("  {origin}"), CheckRow::Origin, 4);
    }
    if let Some(label) = &entry.label {
        push(&format!("  {label}"), CheckRow::Label, 4);
    }
    for note in &entry.notes {
        push(&format!("  {note}"), CheckRow::Note, 4);
    }
    rows
}

/// Word-wrap `text` to `width` cells, indenting continuation rows. Words
/// longer than a row are split between graphemes.
fn wrap_text(text: &str, width: usize, indent: usize) -> Vec<String> {
    let width = width.max(1);
    let indent = indent.min(width / 2);
    let mut rows = Vec::new();
    let mut row = String::new();
    let mut content_start = 0;
    let mut break_at = None;
    for grapheme in text.graphemes(true) {
        let cells = UnicodeWidthStr::width(grapheme);
        if UnicodeWidthStr::width(row.as_str()) + cells > width && row.len() > content_start {
            if grapheme == " " {
                rows.push(row.trim_end().to_owned());
                row = " ".repeat(indent);
                content_start = indent;
                break_at = None;
                continue;
            }
            let carried = break_at
                .filter(|&at| at < row.len())
                .map(|at| row.split_off(at))
                .unwrap_or_default();
            rows.push(row.trim_end().to_owned());
            row = format!("{}{carried}", " ".repeat(indent));
            content_start = indent;
            break_at = None;
        }
        row.push_str(grapheme);
        if grapheme == " " && !row[content_start..].trim().is_empty() {
            break_at = Some(row.len());
        }
    }
    rows.push(row);
    rows
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
        "  Ctrl-W j focus  Ctrl-` hide".into()
    };
    let title_width = canvas.text(
        header.x.saturating_add(1),
        header.y,
        "TERMINAL",
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
    let status_x = header.x.saturating_add(title_width).saturating_add(1);
    let status_width = canvas.text(
        status_x,
        header.y,
        &format!("  · {status}"),
        header.width.saturating_sub(title_width).saturating_sub(2),
        Style::new(palette.muted, palette.status),
    );
    let hint_width = UnicodeWidthStr::width(suffix.as_str()) as u16;
    if title_width
        .saturating_add(status_width)
        .saturating_add(hint_width)
        .saturating_add(3)
        <= header.width
    {
        canvas.text(
            header.x + header.width - hint_width - 1,
            header.y,
            &suffix,
            hint_width,
            Style::new(palette.muted, palette.status),
        );
    }

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
    let style = Style::new(palette.border, palette.background);
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
            .column
            + line_hints(editor, buffer, cursor.line)
                .iter()
                .filter(|hint| hint.position.grapheme <= cursor.grapheme)
                .map(|hint| UnicodeWidthStr::width(hint.label.as_str()))
                .sum::<usize>();
        let gutter =
            (buffer.line_count().max(1).to_string().len() as u16 + 3 + git_gutter(editor, buffer))
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
    let git_width = git_gutter(editor, buffer);
    let gutter = (digits + 3 + git_width).min(rect.width.saturating_sub(1));
    let content_x = rect.x.saturating_add(gutter);
    let content_width = rect.width.saturating_sub(gutter);
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
            continue;
        }
        let active_line = line_number == pane.cursor.line;
        let focused_line =
            active_line && pane.id == editor.active_pane && editor.focus == Focus::Editor;
        let line_background = if focused_line {
            palette.cursor_line
        } else {
            palette.background
        };
        if focused_line {
            canvas.fill(
                Rect {
                    y: rect.y + row,
                    height: 1,
                    ..rect
                },
                " ",
                Style::new(palette.foreground, line_background),
            );
        }
        let number = if active_line || !editor.config.ui.relative_numbers {
            line_number + 1
        } else {
            line_number.abs_diff(pane.cursor.line)
        };
        let number_text = format!("{number:>width$}", width = usize::from(digits));
        let number_style = Style::new(
            if focused_line {
                palette.accent
            } else {
                palette.muted
            },
            line_background,
        );
        canvas.text(
            rect.x,
            rect.y + row,
            &number_text,
            digits,
            if focused_line {
                number_style.bold()
            } else {
                number_style
            },
        );
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
                Style::new(color, line_background),
            );
        }
        let line = buffer.line(line_number).unwrap_or("");
        if git_width > 0 && rect.width > digits + 4 {
            let changes = editor
                .git
                .snapshot
                .as_ref()
                .and_then(|s| s.file.as_ref())
                .unwrap();
            let (staged, unstaged) = changes.markers(line_number);
            for (offset, marker) in [staged, unstaged].into_iter().enumerate() {
                if let Some(marker) = marker {
                    let color = match marker {
                        '+' => palette.accent,
                        '-' => palette.error,
                        _ => palette.warning,
                    };
                    canvas.text(
                        rect.x + digits + 2 + offset as u16,
                        rect.y + row,
                        &marker.to_string(),
                        1,
                        Style::new(color, line_background),
                    );
                }
            }
        }
        let tab_width = editor.config.editor.tab_width.max(1);
        let cached = builder.line(buffer, line_number, tab_width);
        let hints = line_hints(editor, buffer, line_number);
        let hint_width: usize = hints
            .iter()
            .map(|hint| UnicodeWidthStr::width(hint.label.as_str()))
            .sum();
        let start = cached.at_column(line, pane.viewport_column.saturating_sub(hint_width));
        let cursor_column = active_line.then(|| {
            cached.at_grapheme(line, pane.cursor.grapheme).column
                + hints
                    .iter()
                    .filter(|hint| hint.position.grapheme <= pane.cursor.grapheme)
                    .map(|hint| UnicodeWidthStr::width(hint.label.as_str()))
                    .sum::<usize>()
        });
        let mut spans = cached.spans.iter().peekable();
        let mut display_column = start.column
            + hints
                .iter()
                .filter(|hint| hint.position.grapheme < start.grapheme)
                .map(|hint| UnicodeWidthStr::width(hint.label.as_str()))
                .sum::<usize>();
        let mut hints = hints
            .iter()
            .filter(|hint| hint.position.grapheme >= start.grapheme)
            .peekable();
        let mut source_column = start.column;
        let line_rect = Rect {
            x: content_x,
            y: rect.y + row,
            width: content_width,
            height: 1,
        };
        let mut reached_end = true;
        for (offset, (byte, grapheme)) in line[start.byte..].grapheme_indices(true).enumerate() {
            let grapheme_index = start.grapheme + offset;
            let byte = start.byte + byte;
            while hints
                .peek()
                .is_some_and(|hint| hint.position.grapheme == grapheme_index)
            {
                let hint = hints.next().unwrap();
                render_virtual_text(
                    canvas,
                    line_rect,
                    &hint.label,
                    &mut display_column,
                    pane.viewport_column,
                    Style::new(palette.muted, line_background),
                );
            }
            let width = if grapheme == "\t" {
                tab_width - (source_column % tab_width)
            } else {
                UnicodeWidthStr::width(grapheme).max(1)
            };
            source_column += width;
            let next_column = display_column + width;
            if next_column <= pane.viewport_column {
                display_column = next_column;
                continue;
            }
            let screen_column = display_column.saturating_sub(pane.viewport_column);
            if screen_column >= usize::from(content_width) {
                reached_end = false;
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
            style.bg = line_background;
            if selected {
                style.bg = palette.selection;
            }
            if grapheme == "\t" {
                for offset in 0..width
                    .min(next_column.saturating_sub(pane.viewport_column))
                    .min(usize::from(content_width).saturating_sub(screen_column))
                {
                    canvas.put_grapheme(
                        content_x + (screen_column + offset) as u16,
                        rect.y + row,
                        " ",
                        style,
                    );
                }
            } else if display_column >= pane.viewport_column
                && screen_column + width <= usize::from(content_width)
            {
                canvas.put_grapheme(
                    content_x + screen_column as u16,
                    rect.y + row,
                    grapheme,
                    style,
                );
            }
            display_column = next_column;
        }
        if reached_end {
            for hint in hints {
                render_virtual_text(
                    canvas,
                    line_rect,
                    &hint.label,
                    &mut display_column,
                    pane.viewport_column,
                    Style::new(palette.muted, line_background),
                );
            }
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
                editor.mode == Mode::Normal
                    && diagnostic.line == line_number
                    && diagnostic.severity == severity
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
                    Style::new(color, line_background),
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

fn git_gutter(editor: &Editor, buffer: &Buffer) -> u16 {
    if editor
        .git
        .snapshot
        .as_ref()
        .and_then(|s| s.file.as_ref())
        .is_some_and(|file| {
            Some(file.path.as_path()) == buffer.path()
                && file.revision == buffer.revision()
                && !buffer.is_dirty()
        })
    {
        2
    } else {
        0
    }
}

fn line_hints<'a>(editor: &'a Editor, buffer: &Buffer, line: usize) -> &'a [InlayHint] {
    if editor.mode != Mode::Normal || !editor.inlay_hints {
        return &[];
    }
    let Some(snapshot) = buffer
        .path()
        .and_then(|path| editor.inlay_snapshots.get(path))
    else {
        return &[];
    };
    if snapshot.revision != buffer.revision() {
        return &[];
    }
    let start = snapshot
        .hints
        .partition_point(|hint| hint.position.line < line);
    let end = snapshot
        .hints
        .partition_point(|hint| hint.position.line <= line);
    &snapshot.hints[start..end]
}

fn render_virtual_text(
    canvas: &mut Canvas,
    rect: Rect,
    text: &str,
    column: &mut usize,
    viewport: usize,
    style: Style,
) {
    for grapheme in text.graphemes(true) {
        let width = UnicodeWidthStr::width(grapheme).max(1);
        if *column >= viewport && column.saturating_sub(viewport) + width <= usize::from(rect.width)
        {
            canvas.put_grapheme(
                rect.x + (*column - viewport) as u16,
                rect.y,
                grapheme,
                style,
            );
        }
        *column += width;
    }
}

fn render_hover(
    canvas: &mut Canvas,
    editor: &mut Editor,
    pane: Rect,
    cursor: (u16, u16),
    palette: Palette,
) {
    let Some(hover) = &editor.hover else {
        return;
    };
    if pane.width < 8 || pane.height < 4 {
        return;
    }
    let width = pane.width.min(80);
    let rows: Vec<_> = hover
        .text
        .lines()
        .flat_map(|line| wrap_text(line, usize::from(width.saturating_sub(4)), 0))
        .collect();
    let above = cursor.1.saturating_sub(pane.y);
    let below = (pane.y + pane.height).saturating_sub(cursor.1 + 1);
    // Prefer the requested location above the cursor, but keep the popup
    // usable on the first rows of a pane by placing it below when necessary.
    let place_above = above >= 4 || above >= below;
    let available = if place_above { above } else { below };
    if available < 3 {
        return;
    }
    let height = available.min(16).min(rows.len().saturating_add(2) as u16);
    let body_height = usize::from(height.saturating_sub(2));
    let scroll = hover.scroll.min(rows.len().saturating_sub(body_height));
    editor.hover.as_mut().unwrap().scroll = scroll;
    let rect = Rect {
        x: cursor
            .0
            .saturating_sub(1)
            .min(pane.x + pane.width - width)
            .max(pane.x),
        y: if place_above {
            cursor.1 - height
        } else {
            cursor.1 + 1
        },
        width,
        height,
    };
    render_popup(canvas, rect, palette);
    canvas.text(
        rect.x + 2,
        rect.y,
        " Hover ",
        width.saturating_sub(4),
        Style::new(palette.accent, palette.surface),
    );
    for (index, line) in rows.iter().skip(scroll).take(body_height).enumerate() {
        canvas.text(
            rect.x + 2,
            rect.y + 1 + index as u16,
            line,
            width.saturating_sub(4),
            Style::new(palette.foreground, palette.surface),
        );
    }
    if rows.len() > body_height {
        canvas.text(
            rect.x + 2,
            rect.y + height - 1,
            " C-f/C-b scroll · Esc close ",
            width.saturating_sub(4),
            Style::new(palette.muted, palette.surface),
        );
    }
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
    if matches!(kind, Highlight::Heading) {
        style.bold = true;
    }
    style
}

fn render_explorer(canvas: &mut Canvas, editor: &Editor, rect: Rect, palette: Palette) {
    canvas.fill(rect, " ", Style::new(palette.foreground, palette.surface));
    canvas.fill(
        Rect { height: 1, ..rect },
        " ",
        Style::new(palette.foreground, palette.status),
    );
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
        let active_file = editor.active_buffer().path() == Some(entry.path.as_path());
        let style = Style::new(
            if selected || active_file {
                palette.accent
            } else if entry.is_directory() {
                palette.info
            } else {
                palette.foreground
            },
            if selected {
                palette.selection
            } else if active_file {
                palette.status
            } else {
                palette.surface
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
        canvas.text(rect.x, y, &label, rect.width.saturating_sub(1), style);
        if active_file && rect.width > 1 {
            canvas.text(
                rect.x + rect.width - 1,
                y,
                "●",
                1,
                Style::new(palette.accent, style.bg),
            );
        }
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
            Style::new(palette.muted, palette.surface),
        );
    }
}

/// Keep the end of a path or query visible without splitting a grapheme.
fn text_tail(text: &str, width: u16) -> Cow<'_, str> {
    if UnicodeWidthStr::width(text) <= usize::from(width) {
        return Cow::Borrowed(text);
    }
    if width == 0 {
        return Cow::Borrowed("");
    }
    let mut used = 1;
    let mut start = text.len();
    for (index, grapheme) in text.grapheme_indices(true).rev() {
        let cells = UnicodeWidthStr::width(grapheme).max(1);
        if used + cells > usize::from(width) {
            break;
        }
        used += cells;
        start = index;
    }
    Cow::Owned(format!("…{}", &text[start..]))
}

fn render_status(canvas: &mut Canvas, editor: &Editor, rect: Rect, palette: Palette) {
    canvas.fill(rect, " ", Style::new(palette.foreground, palette.status));
    let mode_color = match editor.mode {
        Mode::Normal => palette.accent,
        Mode::Insert => palette.info,
        Mode::Visual(_) => palette.keyword,
        Mode::OperatorPending => palette.warning,
        Mode::Replace => palette.error,
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
    let flags_width = UnicodeWidthStr::width(flags.as_str()) as u16;
    let left = rect.x.saturating_add(mode_width).saturating_add(1);
    let mut right = rect.x.saturating_add(rect.width);
    let pane = editor.active_pane();
    let position = format!(" {}:{} ", pane.cursor.line + 1, pane.cursor.grapheme + 1);
    let position_width = UnicodeWidthStr::width(position.as_str()) as u16;
    if position_width <= right.saturating_sub(left) {
        right -= position_width;
        canvas.text(
            right,
            rect.y,
            &position,
            position_width,
            Style::new(palette.info, palette.surface),
        );
    }
    // Reserve space for the path and safety flags before adding optional tools.
    // Long tool failures must never overwrite the mode, filename, or position.
    let git_status = editor.git.snapshot.as_ref().map(|s| {
        let staged = s
            .entries
            .iter()
            .filter(|e| e.index != ' ' && e.index != '?')
            .count();
        let unstaged = s.entries.iter().filter(|e| e.worktree != ' ').count();
        format!("{} S:{staged} U:{unstaged}", s.branch)
    });
    for (name, status, minimum_width) in [
        ("Git", git_status.as_deref().unwrap_or(""), 75),
        ("RA", editor.rust_analyzer_status.as_str(), 55),
    ] {
        if status.is_empty() {
            continue;
        }
        let label = format!("  {name} · {status} ");
        let label_width = UnicodeWidthStr::width(label.as_str()).min(usize::from(u16::MAX)) as u16;
        if rect.width < minimum_width
            || label_width.saturating_add(flags_width).saturating_add(14)
                > right.saturating_sub(left)
        {
            continue;
        }
        right -= label_width;
        let color = if status.starts_with("failed") {
            palette.error
        } else if status == "ready" {
            palette.accent
        } else {
            palette.muted
        };
        canvas.text(
            right,
            rect.y,
            &label,
            label_width,
            Style::new(color, palette.status),
        );
    }
    let available = right.saturating_sub(left).saturating_sub(1);
    let path_width = available.saturating_sub(flags_width);
    let path = text_tail(&slot.display_name, path_width);
    let written = canvas.text(
        left,
        rect.y,
        &path,
        path_width,
        Style::new(palette.foreground, palette.status),
    );
    canvas.text(
        left.saturating_add(written),
        rect.y,
        &flags,
        available.saturating_sub(written),
        Style::new(palette.warning, palette.status),
    );
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
    if text.is_empty() && rect.width >= 40 {
        canvas.text(
            rect.x + 1,
            rect.y,
            "Space  commands",
            rect.width.saturating_sub(2),
            Style::new(palette.muted, palette.background),
        );
        return;
    }
    let style = if text.to_ascii_lowercase().contains("error")
        || text.starts_with("Unknown")
        || text.contains("unavailable")
    {
        Style::new(palette.error, palette.background)
    } else {
        Style::new(palette.foreground, palette.background)
    };
    let prompt = matches!(editor.mode, Mode::Command | Mode::Search { .. });
    let padding = u16::from(!prompt);
    canvas.text(
        rect.x.saturating_add(padding),
        rect.y,
        &text,
        rect.width.saturating_sub(padding),
        style,
    );
    if prompt {
        canvas.text(
            rect.x,
            rect.y,
            &text[..1],
            1,
            Style::new(palette.accent, palette.background).bold(),
        );
    }
}

fn render_popup(canvas: &mut Canvas, rect: Rect, palette: Palette) {
    let shadow = Rect {
        x: rect.x.saturating_add(2),
        y: rect.y.saturating_add(1),
        ..rect
    };
    canvas.fill(
        shadow,
        " ",
        Style::new(palette.muted, palette.background.mix(Color(0, 0, 0), 25)),
    );
    canvas.fill(rect, " ", Style::new(palette.foreground, palette.status));
    canvas.border(rect, Style::new(palette.border, palette.status));
}

fn render_leader(canvas: &mut Canvas, editor: &Editor, palette: Palette) {
    let entries = command::menu_entries(&editor.leader_prefix);
    if entries.is_empty() {
        return;
    }
    let desired_height = u16::try_from(entries.len())
        .unwrap_or(u16::MAX)
        .saturating_add(3);
    let rect = overlay_rect(canvas.width, canvas.height, 50, desired_height);
    if rect.width == 0 || rect.height == 0 {
        return;
    }
    render_popup(canvas, rect, palette);
    if rect.width < 8 || rect.height < 4 {
        return;
    }
    canvas.text(
        rect.x.saturating_add(2),
        rect.y,
        &format!(" Commands · Space{} ", editor.leader_prefix),
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
            CommandSource::Editor => false,
        });
        let color = if unavailable {
            palette.muted
        } else {
            palette.foreground
        };
        let mut label = if entry.group {
            format!("{} ›", entry.label)
        } else {
            entry.label.into()
        };
        if unavailable {
            label.push_str(&format!("  [{}]", editor.rust_analyzer_status));
        }
        let y = rect.y.saturating_add(1).saturating_add(row as u16);
        canvas.text(
            rect.x.saturating_add(2),
            y,
            &format!(" {} ", if entry.key == ' ' { '␣' } else { entry.key }),
            3,
            Style::new(
                if unavailable {
                    palette.muted
                } else {
                    palette.accent
                },
                palette.surface,
            )
            .bold(),
        );
        canvas.text(
            rect.x.saturating_add(7),
            y,
            &label,
            rect.width.saturating_sub(9),
            Style::new(color, palette.status),
        );
    }
    canvas.text(
        rect.x + 2,
        rect.y + rect.height - 1,
        " Esc close ",
        rect.width.saturating_sub(4),
        Style::new(palette.muted, palette.status),
    );
}

fn picker_rect(width: u16, height: u16, kind: PickerKind) -> Rect {
    if kind == PickerKind::References {
        overlay_rect(width, height, 96, 22)
    } else {
        overlay_rect(width, height, 72, 18)
    }
}

fn render_picker(canvas: &mut Canvas, editor: &Editor, palette: Palette) {
    let Some(picker) = &editor.picker else { return };
    let rect = picker_rect(canvas.width, canvas.height, picker.kind);
    if rect.width == 0 || rect.height == 0 {
        return;
    }
    render_popup(canvas, rect, palette);
    if rect.width < 8 || rect.height < 6 {
        return;
    }
    let title = match picker.kind {
        PickerKind::Files => " Project files ",
        PickerKind::Buffers => " Buffers ",
        PickerKind::Grep => " Project grep ",
        PickerKind::Messages => " Messages ",
        PickerKind::Diagnostics => " Diagnostics ",
        PickerKind::Symbols => " Symbols ",
        PickerKind::References => " References ",
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
        &text_tail(&picker.query, rect.width.saturating_sub(7)),
        rect.width.saturating_sub(7),
        Style::new(palette.foreground, palette.status),
    );
    if rect.height > 4 {
        canvas.hline(
            rect.x.saturating_add(1),
            rect.y.saturating_add(3),
            rect.width.saturating_sub(2),
            "─",
            Style::new(palette.border, palette.status),
        );
    }
    let item_height = if picker.kind == PickerKind::References {
        2
    } else {
        1
    };
    let rows = usize::from(rect.height.saturating_sub(5) / item_height);
    let start = picker.selected.saturating_sub(rows.saturating_sub(1));
    for (index, item) in picker.items.iter().enumerate().skip(start).take(rows) {
        let y = rect
            .y
            .saturating_add(4)
            .saturating_add((index - start) as u16 * item_height);
        let selected = index == picker.selected;
        let style = Style::new(
            palette.foreground,
            if selected {
                palette.selection
            } else {
                palette.status
            },
        );
        canvas.fill(
            Rect {
                x: rect.x.saturating_add(1),
                y,
                width: rect.width.saturating_sub(2),
                height: item_height,
            },
            " ",
            style,
        );
        if selected {
            canvas.text(
                rect.x + 1,
                y,
                "▎",
                1,
                Style::new(palette.accent, palette.selection),
            );
        }
        canvas.text(
            rect.x.saturating_add(3),
            y,
            &item.label,
            rect.width.saturating_sub(5),
            if picker.kind == PickerKind::References && !selected {
                Style::new(palette.muted, palette.status)
            } else {
                style
            },
        );
        if picker.kind == PickerKind::References {
            canvas.text(
                rect.x.saturating_add(4),
                y.saturating_add(1),
                &item.detail,
                rect.width.saturating_sub(6),
                style,
            );
        }
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
    let count = if picker.items.is_empty() {
        " 0 results ".into()
    } else {
        format!(" {}/{} ", picker.selected + 1, picker.items.len())
    };
    let count_width = UnicodeWidthStr::width(count.as_str()) as u16;
    let footer_width = rect.width.saturating_sub(4);
    if count_width <= footer_width {
        let y = rect.y + rect.height - 1;
        canvas.text(
            rect.x + 2,
            y,
            " ↑↓ select  Enter confirm  Esc close ",
            footer_width.saturating_sub(count_width).saturating_sub(1),
            Style::new(palette.muted, palette.status),
        );
        canvas.text(
            rect.x + rect.width - count_width - 2,
            y,
            &count,
            count_width,
            Style::new(palette.accent, palette.status),
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
    fn animation_frames_preserve_logical_positions_and_settle_without_more_input() {
        let mut editor = Editor::new(Config::default(), PathBuf::from("/work"));
        editor.buffers[0].buffer = Buffer::from_text("abcdefghijklmnop\n".repeat(100));
        let mut builder = FrameBuilder::new();
        let now = Instant::now();
        let (_, initial) = builder.draw_animated_at(&mut editor, 40, 12, now);
        editor.active_pane_mut().cursor = crate::buffer::Pos::new(0, 12);
        let (_, first) = builder.draw_animated_at(&mut editor, 40, 12, now);
        assert_eq!(first, initial);
        assert!(builder.is_animating());
        assert_eq!(editor.active_pane().cursor.grapheme, 12);
        let (_, middle) =
            builder.draw_animated_at(&mut editor, 40, 12, now + Duration::from_millis(20));
        assert!(middle.unwrap().0 > initial.unwrap().0);
        let (_, final_cursor) =
            builder.draw_animated_at(&mut editor, 40, 12, now + Duration::from_millis(120));
        assert_eq!(final_cursor, draw_editor(&mut editor, 40, 12).1);
        assert!(!builder.is_animating());

        let jump = now + Duration::from_millis(200);
        editor.active_pane_mut().cursor = crate::buffer::Pos::new(70, 0);
        let (first, _) = builder.draw_animated_at(&mut editor, 40, 12, jump);
        let viewport = editor.active_pane().viewport_line;
        assert_eq!(viewport, 61);
        assert!(builder.is_animating());
        let (last, _) =
            builder.draw_animated_at(&mut editor, 40, 12, jump + Duration::from_millis(120));
        assert_ne!(row(&first, 0), row(&last, 0));
        assert_eq!(editor.active_pane().viewport_line, viewport);
        assert_eq!(editor.active_pane().cursor.line, 70);
        assert!(!builder.is_animating());
        assert_eq!(row(&last, 0), row(&draw_editor(&mut editor, 40, 12).0, 0));
    }

    #[test]
    fn animations_snap_for_insert_resize_disable_and_do_not_land_on_wide_cell_tails() {
        let mut editor = Editor::new(Config::default(), PathBuf::from("/work"));
        editor.buffers[0].buffer = Buffer::from_text("界界界界界界\n".repeat(20));
        let now = Instant::now();
        let mut builder = FrameBuilder::new();
        builder.draw_animated_at(&mut editor, 40, 10, now);
        editor.active_pane_mut().cursor = crate::buffer::Pos::new(0, 5);
        builder.draw_animated_at(&mut editor, 40, 10, now);
        for ms in [10, 20, 30, 50] {
            let (canvas, cursor) =
                builder.draw_animated_at(&mut editor, 40, 10, now + Duration::from_millis(ms));
            let (x, y) = cursor.unwrap();
            assert!(!canvas.cells[canvas.index(x, y).unwrap()].continuation);
        }
        editor.mode = Mode::Insert;
        let actual = builder.draw_animated_at(&mut editor, 40, 10, now).1;
        assert_eq!(actual, draw_editor(&mut editor, 40, 10).1);
        assert!(!builder.is_animating());
        editor.mode = Mode::Normal;
        builder.draw_animated_at(&mut editor, 40, 10, now);
        editor.active_pane_mut().cursor = crate::buffer::Pos::new(15, 0);
        builder.draw_animated_at(&mut editor, 40, 10, now);
        assert!(builder.is_animating());
        builder.draw_animated_at(&mut editor, 50, 12, now);
        assert!(!builder.is_animating());
        editor.config.ui.smooth_scroll = false;
        editor.config.ui.cursor_animation = false;
        editor.active_pane_mut().cursor = crate::buffer::Pos::new(0, 0);
        let actual = builder.draw_animated_at(&mut editor, 50, 12, now).1;
        assert_eq!(actual, draw_editor(&mut editor, 50, 12).1);
        assert!(!builder.is_animating());
    }

    #[test]
    fn git_gutters_are_separate_and_hidden_for_dirty_or_stale_buffers() {
        use crate::git::{FileChanges, Hunk, Snapshot};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.rs");
        std::fs::write(&path, "one\ntwo\nthree\n").unwrap();
        let mut editor = Editor::new(Config::default(), dir.path().to_owned());
        editor.open_path(&path).unwrap();
        let hunk = Hunk {
            old_start: 0,
            old_len: 0,
            new_start: 0,
            new_len: 1,
        };
        editor.git.snapshot = Some(Snapshot {
            root: dir.path().to_owned(),
            branch: "main".into(),
            file: Some(FileChanges {
                path,
                revision: editor.active_buffer().revision(),
                staged: vec![hunk],
                unstaged: vec![Hunk {
                    new_start: 2,
                    old_start: 2,
                    ..hunk
                }],
            }),
            ..Snapshot::default()
        });
        let (canvas, _) = draw_editor(&mut editor, 80, 10);
        assert_eq!(canvas.cells[3].symbol, "+");
        assert_eq!(canvas.cells[2 * 80 + 4].symbol, "+");
        editor.handle_key(Key::char('i'));
        editor.handle_key(Key::char('x'));
        assert_eq!(git_gutter(&editor, editor.active_buffer()), 0);
        editor.git.visible = true;
        for width in 0..10 {
            for height in 0..6 {
                assert!(draw_editor(&mut editor, width, height).1.is_none());
            }
        }
    }

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
    fn status_reserves_filename_flags_and_position_before_long_tool_errors() {
        let mut editor = Editor::new(Config::default(), PathBuf::from("/work"));
        editor
            .active_buffer_mut()
            .insert(crate::buffer::Pos::ZERO, "x")
            .unwrap();
        editor.buffers[0].display_name = format!("{}界面.rs", "long/e\u{301}/".repeat(30));
        editor.buffers[0].large_file = true;
        editor.rust_analyzer_status = format!("failed: {}", "unavailable ".repeat(50));
        for width in [40, 60, 100, 140] {
            let (canvas, _) = draw_editor(&mut editor, width, 10);
            let status = row(&canvas, 8);
            assert!(status.starts_with(" NORMAL "));
            assert!(status.contains("界面.rs [+] [LARGE]"), "{status}");
            assert!(status.ends_with(" 1:1 "));
            assert_eq!(UnicodeWidthStr::width(status.as_str()), usize::from(width));
        }
    }

    #[test]
    fn active_line_respects_focus_and_visual_selection() {
        let mut editor = Editor::new(Config::default(), PathBuf::from("/work"));
        editor.buffers[0].buffer = Buffer::from_text("abc\ndef");
        let palette = Palette::from_editor(&editor);
        let (normal, _) = draw_editor(&mut editor, 40, 8);
        assert_eq!(normal.cells[4].style.bg, palette.cursor_line);
        assert_eq!(normal.cells[39].style.bg, palette.cursor_line);
        assert_eq!(normal.cells[44].style.bg, palette.background);

        editor.handle_key(Key::char('v'));
        let (visual, _) = draw_editor(&mut editor, 40, 8);
        assert_eq!(visual.cells[4].style.bg, palette.selection);
        assert_eq!(visual.cells[5].style.bg, palette.cursor_line);

        editor.handle_key(Key::plain(KeyCode::Esc));
        editor.focus = Focus::Explorer;
        let (unfocused, _) = draw_editor(&mut editor, 40, 8);
        assert_eq!(unfocused.cells[4].style.bg, palette.background);
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

    fn check_entry(level: CheckLevel, title: &str, line: usize) -> CheckEntry {
        CheckEntry {
            level: Some(level),
            title: title.into(),
            location: Some(crate::check::CheckLocation {
                path: PathBuf::from("/work/src/main.rs"),
                line,
                column: 4,
            }),
            origin: Some(format!("src/main.rs:{}:5", line + 1)),
            label: Some("expected `u32`, found `&str`".into()),
            notes: vec!["help: consider removing this call to keep the types aligned".into()],
        }
    }

    fn finished_check(editor: &mut Editor, entries: Vec<CheckEntry>) {
        use crate::check::CheckEvent;
        editor.check.visible = true;
        editor.check.begin();
        for entry in entries {
            editor.check.apply(CheckEvent::Entry(entry));
        }
        editor.check.apply(CheckEvent::Finished {
            success: false,
            code: Some(101),
        });
    }

    #[test]
    fn check_panel_docks_right_wraps_entries_and_follows_the_selection() {
        let mut editor = Editor::new(Config::default(), PathBuf::from("/work"));
        finished_check(
            &mut editor,
            (0..6)
                .map(|line| check_entry(CheckLevel::Error, "error[E0308]: mismatched types", line))
                .chain([check_entry(
                    CheckLevel::Warning,
                    "warning: unused variable",
                    9,
                )])
                .collect(),
        );
        let (canvas, _) = draw_editor(&mut editor, 100, 24);
        // 2/5 of 100 columns, after a one-column divider.
        let panel_x = 60;
        for y in 0_u16..22 {
            let index = usize::from(y) * usize::from(canvas.width) + panel_x - 1;
            assert_eq!(canvas.cells[index].symbol, "│", "row {y}");
        }
        let panel_rows = (0..24)
            .map(|y| row(&canvas, y).chars().skip(panel_x).collect::<String>())
            .collect::<Vec<_>>();
        assert_eq!(
            panel_rows[0].trim_end(),
            " CARGO CHECK  6 errors, 1 warning"
        );
        assert_eq!(
            panel_rows[1].trim_end(),
            " ● error[E0308]: mismatched types"
        );
        assert_eq!(panel_rows[2].trim_end(), "   src/main.rs:1:5");
        assert_eq!(panel_rows[3].trim_end(), "   expected `u32`, found `&str`");
        assert_eq!(
            panel_rows[4].trim_end(),
            "   help: consider removing this call to"
        );
        assert_eq!(panel_rows[5].trim_end(), "     keep the types aligned");
        assert_eq!(panel_rows[6].trim_end(), "");
        assert!(panel_rows[21].contains("Ctrl-W l focus · <Space>cw hide"));
        assert!(
            row(&canvas, 0).starts_with("1 "),
            "panes stay left of the panel"
        );

        editor.focus = Focus::Check;
        editor.check.select_last();
        let (canvas, _) = draw_editor(&mut editor, 100, 24);
        let frame = (0..24)
            .map(|y| row(&canvas, y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(frame.contains("▲ warning: unused variable"));
        assert!(frame.contains("Enter open · r rerun · s stop"));
        assert!(editor.check.scroll > 0);
        let selected_row = (0..24)
            .find(|&y| row(&canvas, y).contains("▲ warning"))
            .unwrap();
        let index = usize::from(selected_row) * usize::from(canvas.width) + panel_x + 1;
        assert_eq!(
            canvas.cells[index].style.bg,
            Palette::from_editor(&editor).selection
        );
    }

    #[test]
    fn check_panel_reports_empty_states_and_yields_narrow_terminals() {
        let mut editor = Editor::new(Config::default(), PathBuf::from("/work"));
        editor.check.visible = true;
        editor.check.begin();
        let (canvas, _) = draw_editor(&mut editor, 90, 12);
        let frame = (0..12)
            .map(|y| row(&canvas, y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(frame.contains("CARGO CHECK  running…"));
        assert!(frame.contains("Running cargo check…"));

        finished_check(&mut editor, Vec::new());
        editor.check.begin();
        editor.check.apply(crate::check::CheckEvent::Finished {
            success: true,
            code: Some(0),
        });
        let (canvas, _) = draw_editor(&mut editor, 90, 12);
        let frame = (0..12)
            .map(|y| row(&canvas, y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(frame.contains("✓ No errors or warnings"));
        assert!(frame.contains("CARGO CHECK  no errors or warnings"));
        let (canvas, _) = draw_editor(&mut editor, 160, 12);
        assert!(row(&canvas, 0).contains("CARGO CHECK  no errors or warnings · 0.0s"));

        let (canvas, _) = draw_editor(&mut editor, 50, 12);
        let frame = (0..12)
            .map(|y| row(&canvas, y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!frame.contains("CARGO CHECK"));
    }

    #[test]
    fn wrapping_prefers_spaces_and_indents_continuations() {
        assert_eq!(wrap_text("short", 10, 2), ["short"]);
        assert_eq!(
            wrap_text("one two three four", 10, 2),
            ["one two", "  three", "  four"]
        );
        assert_eq!(wrap_text("abcdefghij", 4, 1), ["abcd", " efg", " hij"]);
        assert_eq!(
            wrap_text("fits exactly here", 12, 2),
            ["fits exactly", "  here"]
        );
        assert_eq!(wrap_text("界界界", 4, 0), ["界界", "界"]);
        assert_eq!(wrap_text("", 4, 0), [""]);
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
        assert!(frame.contains("Commands · Space"));
        assert!(frame.contains("buffers ›"));
        assert!(!frame.contains("Terminal"));
        assert!(frame.contains("cargo"));
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

        assert!(frame.contains("TERMINAL  · running"));
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
                    column: None,
                    insert_text: None,
                }],
            );
            let (canvas, _) = draw_editor(&mut picker, width, height);
            assert_eq!((canvas.width, canvas.height), (width, height));
        }
    }

    #[test]
    fn long_picker_queries_keep_their_tail_and_cursor_inside_the_border() {
        let mut editor = Editor::new(Config::default(), PathBuf::from("/work"));
        editor.show_picker_items(PickerKind::Files, Vec::new());
        editor.picker.as_mut().unwrap().query = format!("{}界e\u{301}", "long/".repeat(30));
        for width in [20, 40, 100] {
            let (canvas, cursor) = draw_editor(&mut editor, width, 20);
            let rect = picker_rect(width, 20, PickerKind::Files);
            let query_row = row(&canvas, rect.y + 2);
            assert!(query_row.contains("界e\u{301}"), "{query_row}");
            let (x, y) = cursor.unwrap();
            assert!(x < rect.x + rect.width - 1);
            assert_eq!(y, rect.y + 2);
            assert_eq!(canvas.cells[usize::from(y * width + x)].symbol, " ");
        }
    }

    #[test]
    fn reference_picker_shows_code_beneath_locations_and_scrolls_whole_results() {
        let mut editor = Editor::new(Config::default(), PathBuf::from("/work"));
        editor.show_picker_items(
            PickerKind::References,
            (0..20)
                .map(|index| crate::editor::PickerItem {
                    label: format!("src/file{index}.rs:42:5"),
                    detail: format!("let value{index} = symbol(\"😀\");"),
                    path: Some(PathBuf::from(format!("/work/src/file{index}.rs"))),
                    line: Some(41),
                    column: Some(4),
                    insert_text: None,
                })
                .collect(),
        );
        let rect = overlay_rect(100, 24, 96, 22);
        let (canvas, cursor) = draw_editor(&mut editor, 100, 24);
        assert_eq!(cursor, Some((rect.x + 4, rect.y + 2)));
        assert!(row(&canvas, rect.y).contains("References"));
        assert!(row(&canvas, rect.y + 4).contains("src/file0.rs:42:5"));
        assert!(row(&canvas, rect.y + 5).contains("let value0 = symbol(\"😀\");"));
        for ch in "value".chars() {
            editor.handle_key(Key::char(ch));
        }
        let (_, cursor) = draw_editor(&mut editor, 100, 24);
        assert_eq!(cursor, Some((rect.x + 9, rect.y + 2)));
        editor.handle_key(Key::ctrl('p'));
        let (scrolled, _) = draw_editor(&mut editor, 100, 24);
        assert!(row(&scrolled, rect.y + 18).contains("src/file19.rs:42:5"));
        assert!(row(&scrolled, rect.y + 19).contains("let value19 = symbol(\"😀\");"));
        assert!(!row(&scrolled, rect.y + 21).contains("symbol"));
        for (width, height) in [(0, 0), (1, 1), (10, 4), (20, 8)] {
            let (small, _) = draw_editor(&mut editor, width, height);
            assert_eq!((small.width, small.height), (width, height));
        }
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
    fn diagnostics_hide_inline_text_in_every_mode_except_normal_even_when_current() {
        let mut editor = Editor::new(Config::default(), PathBuf::from("/work"));
        editor.diagnostics.push(crate::editor::Diagnostic {
            path: None,
            line: 0,
            column: 0,
            severity: DiagnosticSeverity::Error,
            message: "current diagnostic".into(),
            version: editor.active_buffer().revision(),
        });
        for mode in [
            Mode::Insert,
            Mode::Command,
            Mode::Visual(VisualKind::Character),
            Mode::Normal,
        ] {
            editor.mode = mode.clone();
            let (frame, _) = draw_editor(&mut editor, 60, 10);
            assert_eq!(
                row(&frame, 0).contains("current diagnostic"),
                mode == Mode::Normal
            );
            assert!(row(&frame, 0).contains('●'), "gutter marker stays visible");
        }
    }

    #[test]
    fn inlays_preserve_unicode_cursor_positions_scroll_and_never_change_buffer_text() {
        use crate::{
            buffer::Pos,
            editor::{InlayHint, InlaySnapshot},
        };
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("main.rs");
        let source = "let 名 = 1;\n";
        std::fs::write(&path, source).unwrap();
        let mut editor = Editor::new(Config::default(), directory.path().to_owned());
        editor.open_path(&path).unwrap();
        editor.inlay_snapshots.insert(
            path.clone(),
            InlaySnapshot {
                revision: editor.active_buffer().revision(),
                hints: vec![InlayHint {
                    position: Pos::new(0, 5),
                    label: ": i32".into(),
                }],
            },
        );
        editor.active_pane_mut().cursor = Pos::new(0, 8);
        for width in [60, 15, 8] {
            let (frame, cursor) = draw_editor(&mut editor, width, 6);
            let (x, y) = cursor.unwrap();
            assert!(x < width);
            assert_eq!(frame.cells[frame.index(x, y).unwrap()].symbol, "1");
            if width == 60 {
                assert!(row(&frame, 0).contains("名: i32 = 1;"));
            }
        }
        editor.active_pane_mut().viewport_column = 0;
        for mode in [Mode::Insert, Mode::Normal] {
            editor.mode = mode.clone();
            let (frame, _) = draw_editor(&mut editor, 60, 6);
            assert_eq!(row(&frame, 0).contains(": i32"), mode == Mode::Normal);
        }
        editor.inlay_hints = false;
        assert!(!row(&draw_editor(&mut editor, 60, 6).0, 0).contains(": i32"));
        assert_eq!(editor.active_buffer().text(), source);
        assert!(!editor.active_buffer().is_dirty());
        editor.inlay_hints = true;
        editor.active_buffer_mut().insert(Pos::ZERO, "x").unwrap();
        assert!(!row(&draw_editor(&mut editor, 60, 6).0, 0).contains(": i32"));
    }

    #[test]
    fn hover_box_prefers_above_cursor_falls_below_and_handles_small_panes() {
        use crate::{buffer::Pos, editor::HoverPopup};
        let mut editor = Editor::new(Config::default(), PathBuf::from("/work"));
        editor
            .active_buffer_mut()
            .insert(Pos::ZERO, &"source\n".repeat(20))
            .unwrap();
        editor.hover = Some(HoverPopup {
            text: "fn example()\nUnicode 名 documentation".into(),
            scroll: 0,
        });
        editor.active_pane_mut().cursor = Pos::new(10, 2);
        let (frame, cursor) = draw_editor(&mut editor, 60, 24);
        let top = (0..24).find(|y| row(&frame, *y).contains("Hover")).unwrap();
        assert!(top < cursor.unwrap().1);
        assert!(row(&frame, top + 1).contains("fn example()"));
        assert!(row(&frame, top).contains('╭'));
        editor.active_pane_mut().cursor = Pos::ZERO;
        editor.active_pane_mut().viewport_line = 0;
        let (frame, cursor) = draw_editor(&mut editor, 60, 24);
        let top = (0..24).find(|y| row(&frame, *y).contains("Hover")).unwrap();
        assert!(top > cursor.unwrap().1);
        for width in 0..12 {
            for height in 0..8 {
                draw_editor(&mut editor, width, height);
            }
        }
        editor.handle_key(Key::plain(KeyCode::Esc));
        assert!(editor.hover.is_none());
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
    #[test]
    fn side_by_side_diff_keeps_context_aligned_and_falls_back_when_narrow() {
        let mut editor = Editor::new(Config::default(), PathBuf::from("/work"));
        let lines: Vec<String> = "@@ -1,4 +1,3 @@\n context\n-old\n-extra\n+new\n tail"
            .lines()
            .map(str::to_owned)
            .collect();
        editor.git.visible = true;
        editor.git.document = Some(crate::git::Document {
            title: "Unstaged".into(),
            rows: crate::git::align_diff(&lines),
            lines,
        });
        let (canvas, _) = draw_editor(&mut editor, 100, 14);
        assert!(row(&canvas, 1).contains("BEFORE"));
        assert!(row(&canvas, 1).contains("AFTER"));
        let context = row(&canvas, 3);
        assert_eq!(context.matches("context").count(), 2);
        let extra = row(&canvas, 5);
        assert!(extra[..50].contains("extra"));
        assert!(extra[53..].trim().is_empty());
        assert_eq!(row(&canvas, 6).matches("tail").count(), 2);
        editor.git.scroll = usize::MAX;
        editor.git.horizontal = 8;
        let (narrow, _) = draw_editor(&mut editor, 30, 6);
        assert!(!row(&narrow, 1).contains("BEFORE"));
        // All geometries and scroll extremes stay bounded, including tiny frames.
        for width in [0, 1, 30, 63, 64, 65, 100] {
            for height in [0, 1, 2, 3, 4] {
                draw_editor(&mut editor, width, height);
            }
        }
    }

    #[test]
    fn diff_clipping_uses_cells_for_tabs_and_wide_graphemes() {
        let mut canvas = Canvas::new(12, 1, Style::default());
        draw_diff_text(
            &mut canvas,
            0,
            0,
            "\t界a\u{301}tail",
            12,
            5,
            Style::default(),
        );
        assert!(row(&canvas, 0).starts_with(" a\u{301}tail"));
    }

    #[cfg(feature = "gui")]
    #[test]
    fn desktop_hit_testing_uses_graphemes_tabs_and_the_clicked_pane() {
        let mut editor = Editor::new(Config::default(), PathBuf::from("/work"));
        editor.buffers[0].buffer = Buffer::from_text("\t界a\u{301}bc");
        editor.explorer.open = true; // Native tree is outside the workspace grid.
        let mut builder = FrameBuilder::new();
        builder.draw_workspace(&mut editor, 80, 20);
        builder.place_cursor(&mut editor, 9, 0); // second cell of 界 after a four-cell tab
        assert_eq!(editor.active_pane().cursor, crate::buffer::Pos::new(0, 1));
        builder.place_cursor(&mut editor, 10, 0);
        assert_eq!(editor.active_pane().cursor, crate::buffer::Pos::new(0, 2));
        editor.split(Orientation::Vertical);
        builder.draw_workspace(&mut editor, 80, 20);
        let first = editor.panes[0].id;
        builder.place_cursor(&mut editor, 4, 0);
        assert_eq!(editor.active_pane, first);
    }
}
