//! Terminal-independent modal editor state machine.

use std::{
    collections::{BTreeSet, HashMap, VecDeque},
    fs,
    path::{Path, PathBuf},
};

use unicode_segmentation::UnicodeSegmentation;

use crate::{
    buffer::{Buffer, BufferError, Pos, TextRange},
    command::{self, CommandId},
    config::{Config, parse_hex_color},
    input::{Key, KeyCode, Modifiers},
    terminal::TerminalPanel,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisualKind {
    Character,
    Line,
    Block,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Insert,
    Visual(VisualKind),
    OperatorPending,
    Command,
    Search { backward: bool },
    Leader,
    Replace,
    Find,
}

impl Mode {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Normal => "NORMAL",
            Self::Insert => "INSERT",
            Self::Visual(VisualKind::Character) => "VISUAL",
            Self::Visual(VisualKind::Line) => "V-LINE",
            Self::Visual(VisualKind::Block) => "V-BLOCK",
            Self::OperatorPending => "OPERATOR",
            Self::Command => "COMMAND",
            Self::Search { .. } => "SEARCH",
            Self::Leader => "LEADER",
            Self::Replace => "REPLACE",
            Self::Find => "FIND",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Orientation {
    Horizontal,
    Vertical,
}

pub type PaneId = u64;

#[derive(Debug, Clone)]
pub struct Pane {
    pub id: PaneId,
    pub buffer: usize,
    pub cursor: Pos,
    pub anchor: Option<Pos>,
    pub viewport_line: usize,
    pub viewport_column: usize,
    pub desired_column: usize,
}

#[derive(Debug, Clone)]
pub enum Layout {
    Leaf(PaneId),
    Split {
        orientation: Orientation,
        ratio: u16,
        first: Box<Layout>,
        second: Box<Layout>,
    },
}

impl Layout {
    pub fn leaves(&self, output: &mut Vec<PaneId>) {
        match self {
            Self::Leaf(id) => output.push(*id),
            Self::Split { first, second, .. } => {
                first.leaves(output);
                second.leaves(output);
            }
        }
    }

    fn split(&mut self, target: PaneId, new: PaneId, orientation: Orientation) -> bool {
        match self {
            Self::Leaf(id) if *id == target => {
                *self = Self::Split {
                    orientation,
                    ratio: 500,
                    first: Box::new(Self::Leaf(target)),
                    second: Box::new(Self::Leaf(new)),
                };
                true
            }
            Self::Leaf(_) => false,
            Self::Split { first, second, .. } => {
                first.split(target, new, orientation) || second.split(target, new, orientation)
            }
        }
    }

    fn remove(self, target: PaneId) -> Option<Self> {
        match self {
            Self::Leaf(id) => (id != target).then_some(Self::Leaf(id)),
            Self::Split {
                orientation,
                ratio,
                first,
                second,
            } => match (first.remove(target), second.remove(target)) {
                (Some(first), Some(second)) => Some(Self::Split {
                    orientation,
                    ratio,
                    first: Box::new(first),
                    second: Box::new(second),
                }),
                (Some(only), None) | (None, Some(only)) => Some(only),
                (None, None) => None,
            },
        }
    }
}

#[derive(Debug, Clone)]
pub struct BufferSlot {
    pub buffer: Buffer,
    pub display_name: String,
    pub large_file: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegisterKind {
    Character,
    Line,
    Block,
}

#[derive(Debug, Clone)]
pub struct Register {
    pub text: String,
    pub kind: RegisterKind,
}

impl Default for Register {
    fn default() -> Self {
        Self {
            text: String::new(),
            kind: RegisterKind::Character,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Operator {
    Delete,
    Change,
    Yank,
    Indent,
    Dedent,
    Reindent,
    Comment,
}

#[derive(Debug, Clone, Copy)]
enum Motion {
    Left,
    Down,
    Up,
    Right,
    Word,
    BackWord,
    EndWord,
    LineStart,
    FirstNonBlank,
    LineEnd,
    FileStart,
    FileEnd,
    MatchPair,
}

#[derive(Debug, Clone, Copy)]
struct PendingOperator {
    operator: Operator,
    count: usize,
    count_explicit: bool,
}

#[derive(Debug, Clone, Copy)]
struct FindSpec {
    character: char,
    backward: bool,
    till: bool,
}

#[derive(Debug, Clone, Copy)]
enum Awaiting {
    None,
    WindowPrefix,
    Find {
        backward: bool,
        till: bool,
        operator: bool,
    },
    TextObject {
        around: bool,
    },
    Register,
    MacroRecord,
    MacroPlay,
    GPrefix,
    BracketPrefix(char),
}

#[derive(Debug, Clone)]
enum LastChange {
    Insert(String),
    DeleteMotion(Motion, usize),
    DeleteLines(usize),
    DeleteChars(usize),
    Paste { before: bool },
    Replace { ch: char, count: usize },
    Join(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Editor,
    Explorer,
    Terminal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerKind {
    Files,
    Buffers,
    Grep,
    Messages,
    Diagnostics,
    Symbols,
    Recent,
}

#[derive(Debug, Clone)]
pub struct PickerItem {
    pub label: String,
    pub detail: String,
    pub path: Option<PathBuf>,
    pub line: Option<usize>,
    pub insert_text: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Picker {
    pub kind: PickerKind,
    pub query: String,
    pub items: Vec<PickerItem>,
    pub all_items: Vec<PickerItem>,
    pub selected: usize,
    pub return_mode: Mode,
    pub current_buffer_diagnostics: bool,
}

#[derive(Debug, Clone)]
pub struct Explorer {
    pub open: bool,
    pub root: PathBuf,
    pub files: Vec<PathBuf>,
    pub selected: usize,
    pub expanded: BTreeSet<PathBuf>,
    pub show_hidden: bool,
    pub show_ignored: bool,
    pub width: u16,
}

#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub path: Option<PathBuf>,
    pub line: usize,
    pub column: usize,
    pub severity: DiagnosticSeverity,
    pub message: String,
    pub version: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Information,
    Hint,
}

#[derive(Debug, Clone)]
pub enum EditorRequest {
    None,
    RefreshProject,
    DocumentSaved(PathBuf),
    TerminalToggle(bool),
    TerminalInput(Vec<u8>),
    RustAnalyzer(CommandId),
    RustAnalyzerWithArgument(CommandId, String),
    CodexWatch(CommandId),
    CheckHealth,
}

#[derive(Debug)]
pub enum OpenError {
    Io(std::io::Error),
    Buffer(BufferError),
    TooLarge {
        path: PathBuf,
        size: u64,
        limit: u64,
    },
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => e.fmt(f),
            Self::Buffer(e) => e.fmt(f),
            Self::TooLarge { path, size, limit } => write!(
                f,
                "{} is {size} bytes; safety limit is {limit} bytes",
                path.display()
            ),
        }
    }
}

impl From<std::io::Error> for OpenError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}
impl From<BufferError> for OpenError {
    fn from(value: BufferError) -> Self {
        Self::Buffer(value)
    }
}

pub struct Editor {
    pub config: Config,
    pub buffers: Vec<BufferSlot>,
    pub panes: Vec<Pane>,
    pub layout: Layout,
    pub active_pane: PaneId,
    pub mode: Mode,
    pub focus: Focus,
    pub explorer: Explorer,
    pub terminal: TerminalPanel,
    pub picker: Option<Picker>,
    pub prompt: String,
    pub leader_prefix: String,
    pub messages: VecDeque<String>,
    pub diagnostics: Vec<Diagnostic>,
    pub rust_analyzer_status: String,
    pub codex_watch_status: String,
    pub inlay_hints: bool,
    pub should_quit: bool,
    pub request: EditorRequest,
    count: Option<usize>,
    pending_operator: Option<PendingOperator>,
    awaiting: Awaiting,
    selected_register: char,
    registers: HashMap<char, Register>,
    last_find: Option<FindSpec>,
    last_find_match: Option<Pos>,
    last_search: Option<(String, bool)>,
    last_change: Option<LastChange>,
    insert_recording: String,
    macro_recording: Option<(char, Vec<Key>)>,
    macros: HashMap<char, Vec<Key>>,
    last_macro: Option<char>,
    macro_depth: usize,
    next_pane_id: PaneId,
    file_finder: Option<crate::project::FileFinder>,
    file_finder_active: bool,
    project_search_results: Vec<PickerItem>,
    recent_files: Vec<PathBuf>,
}

impl Editor {
    pub fn new(config: Config, project_root: PathBuf) -> Self {
        let width = config.ui.explorer_width;
        let show_hidden = config.ui.show_hidden;
        let show_ignored = config.ui.show_ignored;
        let terminal_background =
            parse_hex_color(&config.ui.theme.background).unwrap_or((0x11, 0x13, 0x18));
        let mut scratch = Buffer::new();
        scratch
            .set_undo_limit(config.limits.undo_steps)
            .expect("new buffer has no transaction");
        scratch
            .set_undo_byte_limit(config.limits.undo_bytes)
            .expect("new buffer has no transaction");
        Self {
            config,
            buffers: vec![BufferSlot {
                buffer: scratch,
                display_name: "[scratch]".into(),
                large_file: false,
            }],
            panes: vec![Pane {
                id: 1,
                buffer: 0,
                cursor: Pos::ZERO,
                anchor: None,
                viewport_line: 0,
                viewport_column: 0,
                desired_column: 0,
            }],
            layout: Layout::Leaf(1),
            active_pane: 1,
            mode: Mode::Normal,
            focus: Focus::Editor,
            explorer: Explorer {
                open: false,
                root: project_root,
                files: Vec::new(),
                selected: 0,
                expanded: BTreeSet::new(),
                show_hidden,
                show_ignored,
                width,
            },
            terminal: TerminalPanel::with_background(terminal_background),
            picker: None,
            prompt: String::new(),
            leader_prefix: String::new(),
            messages: VecDeque::new(),
            diagnostics: Vec::new(),
            rust_analyzer_status: "starting".into(),
            codex_watch_status: "stopped".into(),
            inlay_hints: true,
            should_quit: false,
            request: EditorRequest::None,
            count: None,
            pending_operator: None,
            awaiting: Awaiting::None,
            selected_register: '"',
            registers: HashMap::new(),
            last_find: None,
            last_find_match: None,
            last_search: None,
            last_change: None,
            insert_recording: String::new(),
            macro_recording: None,
            macros: HashMap::new(),
            last_macro: None,
            macro_depth: 0,
            next_pane_id: 2,
            file_finder: None,
            file_finder_active: false,
            project_search_results: Vec::new(),
            recent_files: Vec::new(),
        }
    }

    pub fn active_pane(&self) -> &Pane {
        self.panes
            .iter()
            .find(|p| p.id == self.active_pane)
            .expect("active pane exists")
    }
    pub fn active_pane_mut(&mut self) -> &mut Pane {
        self.panes
            .iter_mut()
            .find(|p| p.id == self.active_pane)
            .expect("active pane exists")
    }
    pub fn active_buffer(&self) -> &Buffer {
        &self.buffers[self.active_pane().buffer].buffer
    }
    pub fn active_buffer_mut(&mut self) -> &mut Buffer {
        let buffer = self.active_pane().buffer;
        &mut self.buffers[buffer].buffer
    }
    pub fn active_slot(&self) -> &BufferSlot {
        &self.buffers[self.active_pane().buffer]
    }

    pub fn set_project_files(&mut self, mut files: Vec<PathBuf>) {
        files.sort();
        files.dedup();
        self.explorer.files.clear();
        if let Some(finder) = &self.file_finder {
            finder.reset(self.explorer.root.clone());
        }
        self.append_project_files(files);
        self.explorer.selected = self
            .explorer
            .selected
            .min(self.explorer.files.len().saturating_sub(1));
        if self
            .picker
            .as_ref()
            .is_some_and(|p| p.kind == PickerKind::Files)
        {
            self.refresh_picker();
        }
    }

    /// Incorporate only the new scan batch. The ranking worker owns its index;
    /// the explorer keeps the traversal order without repeatedly sorting or
    /// cloning paths discovered by earlier batches.
    pub fn append_project_files(&mut self, files: Vec<PathBuf>) {
        if files.is_empty() {
            return;
        }
        self.explorer.files.extend(files.iter().cloned());
        self.file_finder
            .get_or_insert_with(|| crate::project::FileFinder::new(self.explorer.root.clone()))
            .append(files);
    }

    /// Publish results only for the current query and current project index.
    /// Returns whether visible picker contents changed.
    pub fn poll_file_finder(&mut self) -> bool {
        let Some(finder) = &self.file_finder else {
            return false;
        };
        if !self
            .picker
            .as_ref()
            .is_some_and(|picker| picker.kind == PickerKind::Files)
        {
            if self.file_finder_active {
                finder.cancel();
                self.file_finder_active = false;
            }
            return false;
        }
        let Some(result) = finder.try_recv() else {
            return false;
        };
        let Some(picker) = self
            .picker
            .as_mut()
            .filter(|picker| picker.kind == PickerKind::Files && picker.query == result.query)
        else {
            return false;
        };
        if result.generation != finder.generation() {
            return false;
        }
        picker.items = result
            .files
            .into_iter()
            .map(|matched| PickerItem {
                label: matched.relative_path.display().to_string(),
                detail: format!("score {}", matched.score),
                path: Some(matched.path),
                line: None,
                insert_text: None,
            })
            .collect();
        picker.selected = picker.selected.min(picker.items.len().saturating_sub(1));
        true
    }

    pub fn set_project_search_results(&mut self, items: Vec<PickerItem>) {
        self.project_search_results = items;
        if self
            .picker
            .as_ref()
            .is_some_and(|picker| picker.kind == PickerKind::Grep)
        {
            self.refresh_picker();
        }
    }

    pub fn show_completion(&mut self, items: Vec<PickerItem>) {
        let return_mode = self.mode.clone();
        self.picker = Some(Picker {
            kind: PickerKind::Symbols,
            query: String::new(),
            items: items.clone(),
            all_items: items,
            selected: 0,
            return_mode,
            current_buffer_diagnostics: false,
        });
    }

    pub fn show_picker_items(&mut self, kind: PickerKind, items: Vec<PickerItem>) {
        let return_mode = self.mode.clone();
        self.picker = Some(Picker {
            kind,
            query: String::new(),
            items: items.clone(),
            all_items: items,
            selected: 0,
            return_mode,
            current_buffer_diagnostics: false,
        });
    }

    pub fn jump_to(
        &mut self,
        path: impl AsRef<Path>,
        position: crate::buffer::Utf16Pos,
    ) -> Result<(), OpenError> {
        self.open_path(path)?;
        let cursor = self
            .active_buffer()
            .utf16_to_pos(position)
            .unwrap_or(Pos::new(position.line, 0));
        self.set_cursor(cursor, false);
        Ok(())
    }

    pub fn replace_diagnostics(&mut self, path: &Path, diagnostics: Vec<Diagnostic>) {
        self.diagnostics
            .retain(|diagnostic| diagnostic.path.as_deref() != Some(path));
        self.diagnostics.extend(diagnostics);
        self.refresh_picker();
    }

    pub fn apply_lsp_edits(
        &mut self,
        edits: Vec<(crate::buffer::Utf16Pos, crate::buffer::Utf16Pos, String)>,
    ) -> Result<(), BufferError> {
        let mut converted = edits
            .into_iter()
            .map(|(start, end, text)| {
                Ok((
                    TextRange::new(
                        self.active_buffer().utf16_to_pos(start)?,
                        self.active_buffer().utf16_to_pos(end)?,
                    ),
                    text,
                ))
            })
            .collect::<Result<Vec<_>, BufferError>>()?;
        converted.sort_by_key(|item| std::cmp::Reverse(item.0.start));
        self.active_buffer_mut().begin_transaction()?;
        for (range, text) in converted {
            if let Err(error) = self.active_buffer_mut().replace(range, &text) {
                let _ = self.active_buffer_mut().rollback_transaction();
                return Err(error);
            }
        }
        self.active_buffer_mut().commit_transaction()?;
        self.clamp_all_cursors_for_active_buffer();
        Ok(())
    }

    pub fn open_path(&mut self, path: impl AsRef<Path>) -> Result<usize, OpenError> {
        let path = path.as_ref();
        let metadata = match fs::metadata(path) {
            Ok(metadata) => Some(metadata),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        if let Some(metadata) = &metadata
            && metadata.len() > self.config.limits.max_file_bytes
        {
            return Err(OpenError::TooLarge {
                path: path.to_owned(),
                size: metadata.len(),
                limit: self.config.limits.max_file_bytes,
            });
        }
        let canonical = fs::canonicalize(path).unwrap_or_else(|_| {
            if path.is_absolute() {
                path.to_owned()
            } else {
                std::env::current_dir().unwrap_or_default().join(path)
            }
        });
        if let Some(index) = self
            .buffers
            .iter()
            .position(|slot| slot.buffer.path() == Some(canonical.as_path()))
        {
            self.active_pane_mut().buffer = index;
            self.clamp_active_cursor();
            return Ok(index);
        }
        let mut buffer = if metadata.is_some() {
            Buffer::open(&canonical)?
        } else {
            Buffer::new_file(&canonical)?
        };
        buffer.set_undo_limit(self.config.limits.undo_steps)?;
        buffer.set_undo_byte_limit(self.config.limits.undo_bytes)?;
        let display_name = canonical
            .strip_prefix(&self.explorer.root)
            .unwrap_or(&canonical)
            .display()
            .to_string();
        let large_file = metadata
            .as_ref()
            .is_some_and(|metadata| metadata.len() > self.config.limits.large_file_bytes);
        self.buffers.push(BufferSlot {
            buffer,
            display_name,
            large_file,
        });
        let index = self.buffers.len() - 1;
        self.active_pane_mut().buffer = index;
        let pane = self.active_pane_mut();
        pane.cursor = Pos::ZERO;
        pane.viewport_line = 0;
        pane.desired_column = 0;
        self.recent_files.retain(|item| item != &canonical);
        self.recent_files.insert(0, canonical);
        self.recent_files.truncate(100);
        Ok(index)
    }

    pub fn open_scratch_text(&mut self, name: impl Into<String>, text: &str) -> usize {
        let mut buffer = Buffer::from_unsaved_text(text);
        buffer
            .set_undo_limit(self.config.limits.undo_steps)
            .expect("new buffer has no transaction");
        buffer
            .set_undo_byte_limit(self.config.limits.undo_bytes)
            .expect("new buffer has no transaction");
        self.buffers.push(BufferSlot {
            buffer,
            display_name: name.into(),
            large_file: false,
        });
        let index = self.buffers.len() - 1;
        self.active_pane_mut().buffer = index;
        let pane = self.active_pane_mut();
        pane.cursor = Pos::ZERO;
        pane.viewport_line = 0;
        index
    }

    pub fn discard_initial_scratch(&mut self) {
        if self.buffers.len() <= 1
            || self.buffers[0].buffer.path().is_some()
            || !self.buffers[0].buffer.is_empty()
            || self.buffers[0].buffer.is_dirty()
        {
            return;
        }
        self.buffers.remove(0);
        for pane in &mut self.panes {
            pane.buffer = pane.buffer.saturating_sub(1).min(self.buffers.len() - 1);
        }
    }

    pub fn message(&mut self, message: impl Into<String>) {
        self.messages.push_back(message.into());
        while self.messages.len() > self.config.limits.message_history {
            self.messages.pop_front();
        }
    }

    pub fn current_message(&self) -> Option<&str> {
        self.messages.back().map(String::as_str)
    }

    pub fn take_request(&mut self) -> EditorRequest {
        std::mem::replace(&mut self.request, EditorRequest::None)
    }

    pub fn split(&mut self, orientation: Orientation) {
        let source = self.active_pane().clone();
        let id = self.next_pane_id;
        self.next_pane_id += 1;
        let mut pane = source;
        pane.id = id;
        self.panes.push(pane);
        let _ = self.layout.split(self.active_pane, id, orientation);
        self.active_pane = id;
    }

    pub fn close_pane(&mut self) {
        if self.panes.len() == 1 {
            self.message("Cannot close the last pane");
            return;
        }
        let mut leaves = Vec::new();
        self.layout.leaves(&mut leaves);
        let position = leaves
            .iter()
            .position(|id| *id == self.active_pane)
            .unwrap_or(0);
        let target = self.active_pane;
        self.layout = self
            .layout
            .clone()
            .remove(target)
            .expect("another pane remains");
        self.panes.retain(|pane| pane.id != target);
        let mut remaining = Vec::new();
        self.layout.leaves(&mut remaining);
        self.active_pane = remaining[position.min(remaining.len() - 1)];
    }

    pub fn only_pane(&mut self) {
        let pane = self.active_pane().clone();
        self.panes.clear();
        self.panes.push(pane);
        self.layout = Layout::Leaf(self.active_pane);
    }

    pub fn cycle_pane(&mut self, delta: isize) {
        let mut leaves = Vec::new();
        self.layout.leaves(&mut leaves);
        if leaves.len() < 2 {
            return;
        }
        let current = leaves
            .iter()
            .position(|id| *id == self.active_pane)
            .unwrap_or(0);
        let next = (current as isize + delta).rem_euclid(leaves.len() as isize) as usize;
        self.active_pane = leaves[next];
    }

    pub fn handle_paste(&mut self, text: &str) {
        if self.focus == Focus::Terminal {
            let bytes = self.terminal.encode_paste(text);
            self.request = EditorRequest::TerminalInput(bytes);
            return;
        }
        if !matches!(self.mode, Mode::Insert) {
            self.enter_insert(false);
        }
        let cursor = self.active_pane().cursor;
        match self.active_buffer_mut().insert(cursor, text) {
            Ok(pos) => {
                self.active_pane_mut().cursor = pos;
                self.insert_recording.push_str(text);
            }
            Err(error) => self.message(error.to_string()),
        }
    }

    pub fn handle_key(&mut self, key: Key) {
        if self.focus == Focus::Terminal {
            self.handle_terminal_key(key);
            return;
        }
        if let Some((_, keys)) = self.macro_recording.as_mut()
            && !matches!(self.awaiting, Awaiting::MacroRecord)
        {
            keys.push(key);
        }
        if self.handle_window_prefix_key(key) {
            return;
        }
        if key == Key::ctrl('w')
            && self.picker.is_none()
            && (self.focus == Focus::Explorer
                || matches!(
                    self.mode,
                    Mode::Normal
                        | Mode::Visual(_)
                        | Mode::OperatorPending
                        | Mode::Leader
                        | Mode::Replace
                        | Mode::Find
                ))
        {
            self.cancel_pending();
            self.awaiting = Awaiting::WindowPrefix;
            return;
        }
        if self.picker.is_some() {
            self.handle_picker_key(key);
            return;
        }
        if self.focus == Focus::Explorer {
            self.handle_explorer_key(key);
            return;
        }
        match self.mode.clone() {
            Mode::Insert => self.handle_insert_key(key),
            Mode::Command => self.handle_prompt_key(key, false),
            Mode::Search { backward } => self.handle_prompt_key(key, backward),
            Mode::Leader => self.handle_leader_key(key),
            Mode::Visual(kind) => self.handle_visual_key(key, kind),
            _ => self.handle_normal_key(key),
        }
    }

    fn handle_terminal_key(&mut self, key: Key) {
        // Legacy terminal input represents Ctrl-\\ as byte 0x1c. Crossterm
        // decodes that byte as Ctrl-4 unless an enhanced keyboard protocol is
        // active, where it can preserve the physical backslash key.
        if key == Key::ctrl('\\') || key == Key::ctrl('4') {
            self.focus = Focus::Editor;
            self.message("Terminal unfocused; <Space>t hides it");
            return;
        }
        if key.modifiers.contains(Modifiers::SHIFT) {
            match key.code {
                KeyCode::PageUp => {
                    self.terminal.scroll_up();
                    return;
                }
                KeyCode::PageDown => {
                    self.terminal.scroll_down();
                    return;
                }
                KeyCode::Home => {
                    self.terminal.scroll_to_top();
                    return;
                }
                KeyCode::End => {
                    self.terminal.scroll_to_bottom();
                    return;
                }
                _ => {}
            }
        }
        if let Some(bytes) = self.terminal.encode_key(key) {
            self.request = EditorRequest::TerminalInput(bytes);
        }
    }

    fn handle_insert_key(&mut self, key: Key) {
        if matches!(key.code, KeyCode::Esc) || key == Key::ctrl('[') {
            self.leave_insert();
            return;
        }
        if key.modifiers.contains(Modifiers::CONTROL) {
            match key.code {
                KeyCode::Char('n' | 'p') => {
                    self.message("Completion is unavailable while rust-analyzer is starting")
                }
                KeyCode::Char('e') => self.message("Completion dismissed"),
                KeyCode::Char(' ') => {
                    self.request = EditorRequest::RustAnalyzer(CommandId::Completion)
                }
                _ => {}
            }
            return;
        }
        let cursor = self.active_pane().cursor;
        match key.code {
            KeyCode::Char(ch) => {
                let mut encoded = [0; 4];
                let text = ch.encode_utf8(&mut encoded);
                match self.active_buffer_mut().insert(cursor, text) {
                    Ok(pos) => {
                        self.active_pane_mut().cursor = pos;
                        self.insert_recording.push(ch);
                    }
                    Err(error) => self.message(error.to_string()),
                }
            }
            KeyCode::Enter => {
                let indent = self
                    .active_buffer()
                    .line(cursor.line)
                    .unwrap_or("")
                    .chars()
                    .take_while(|ch| matches!(ch, ' ' | '\t'))
                    .collect::<String>();
                let text = format!("\n{indent}");
                match self.active_buffer_mut().insert(cursor, &text) {
                    Ok(pos) => {
                        self.active_pane_mut().cursor = pos;
                        self.insert_recording.push_str(&text);
                    }
                    Err(error) => self.message(error.to_string()),
                }
            }
            KeyCode::Tab => {
                let text = if self.config.editor.insert_spaces {
                    " ".repeat(self.config.editor.tab_width)
                } else {
                    "\t".into()
                };
                match self.active_buffer_mut().insert(cursor, &text) {
                    Ok(pos) => {
                        self.active_pane_mut().cursor = pos;
                        self.insert_recording.push_str(&text);
                    }
                    Err(error) => self.message(error.to_string()),
                }
            }
            KeyCode::Backspace => match self.active_buffer_mut().delete_grapheme_backward(cursor) {
                Ok(Some((pos, _))) => {
                    self.active_pane_mut().cursor = pos;
                }
                Ok(None) => {}
                Err(error) => self.message(error.to_string()),
            },
            KeyCode::Delete => {
                if let Err(error) = self.active_buffer_mut().delete_grapheme_forward(cursor) {
                    self.message(error.to_string());
                }
            }
            KeyCode::Left => self.move_cursor(Motion::Left, 1, true),
            KeyCode::Right => self.move_cursor(Motion::Right, 1, true),
            KeyCode::Up => self.move_cursor(Motion::Up, 1, true),
            KeyCode::Down => self.move_cursor(Motion::Down, 1, true),
            KeyCode::Home => self.set_cursor(Pos::new(cursor.line, 0), true),
            KeyCode::End => {
                let end = self.active_buffer().line_end(cursor.line).unwrap_or(cursor);
                self.set_cursor(end, true);
            }
            _ => {}
        }
    }

    fn handle_prompt_key(&mut self, key: Key, backward: bool) {
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.prompt.clear();
            }
            KeyCode::Backspace => {
                self.prompt.pop();
            }
            KeyCode::Enter => {
                let prompt = std::mem::take(&mut self.prompt);
                let was_search = matches!(self.mode, Mode::Search { .. });
                self.mode = Mode::Normal;
                if was_search {
                    if !prompt.is_empty() {
                        self.run_search(&prompt, backward, true);
                    }
                } else {
                    self.execute_ex(prompt.trim());
                }
            }
            KeyCode::Char(ch) if !key.modifiers.contains(Modifiers::CONTROL) => {
                self.prompt.push(ch)
            }
            _ => {}
        }
    }

    fn handle_normal_key(&mut self, key: Key) {
        if self.handle_awaiting(key) {
            return;
        }
        if self.pending_operator.is_some() {
            self.handle_operator_key(key);
            return;
        }
        if matches!(key.code, KeyCode::Esc) {
            self.cancel_pending();
            return;
        }
        if key.modifiers.contains(Modifiers::CONTROL) {
            let count = self.take_count();
            match key.code {
                KeyCode::Char('r') => {
                    for _ in 0..count {
                        self.redo();
                    }
                }
                KeyCode::Char('d') => {
                    self.move_cursor(Motion::Down, count.saturating_mul(12), false)
                }
                KeyCode::Char('u') => self.move_cursor(Motion::Up, count.saturating_mul(12), false),
                KeyCode::Char('f') => {
                    self.move_cursor(Motion::Down, count.saturating_mul(24), false)
                }
                KeyCode::Char('b') => self.move_cursor(Motion::Up, count.saturating_mul(24), false),
                KeyCode::Char(direction @ ('h' | 'j' | 'k' | 'l')) => {
                    self.focus_window_direction(direction)
                }
                KeyCode::Char('v') => self.enter_visual(VisualKind::Block),
                _ => self.message("Unsupported control key"),
            }
            return;
        }
        let KeyCode::Char(ch) = key.code else {
            match key.code {
                KeyCode::Left => self.move_cursor(Motion::Left, 1, false),
                KeyCode::Right => self.move_cursor(Motion::Right, 1, false),
                KeyCode::Up => self.move_cursor(Motion::Up, 1, false),
                KeyCode::Down => self.move_cursor(Motion::Down, 1, false),
                KeyCode::PageUp => self.move_cursor(Motion::Up, 24, false),
                KeyCode::PageDown => self.move_cursor(Motion::Down, 24, false),
                _ => {}
            }
            return;
        };
        if ch.is_ascii_digit() && (ch != '0' || self.count.is_some()) {
            self.push_count(ch);
            return;
        }
        let count_explicit = self.count.is_some();
        let count = self.take_count();
        match ch {
            'h' => self.move_cursor(Motion::Left, count, false),
            'j' => self.move_cursor(Motion::Down, count, false),
            'k' => self.move_cursor(Motion::Up, count, false),
            'l' => self.move_cursor(Motion::Right, count, false),
            'w' => self.move_cursor(Motion::Word, count, false),
            'b' => self.move_cursor(Motion::BackWord, count, false),
            'e' => self.move_cursor(Motion::EndWord, count, false),
            '0' => self.move_cursor(Motion::LineStart, 1, false),
            '^' => self.move_cursor(Motion::FirstNonBlank, 1, false),
            '$' => self.move_cursor(Motion::LineEnd, count, false),
            'G' => self.move_cursor(
                if count_explicit {
                    Motion::FileStart
                } else {
                    Motion::FileEnd
                },
                count,
                false,
            ),
            '%' => self.move_cursor(Motion::MatchPair, 1, false),
            'g' => {
                self.count = (count != 1).then_some(count);
                self.awaiting = Awaiting::GPrefix;
            }
            'f' => self.await_find(false, false, false, count),
            'F' => self.await_find(true, false, false, count),
            't' => self.await_find(false, true, false, count),
            'T' => self.await_find(true, true, false, count),
            ';' => self.repeat_find(false, count),
            ',' => self.repeat_find(true, count),
            'i' => self.enter_insert(false),
            'I' => {
                self.move_cursor(Motion::FirstNonBlank, 1, true);
                self.enter_insert(false);
            }
            'a' => {
                self.move_insert_after();
                self.enter_insert(false);
            }
            'A' => {
                self.move_cursor(Motion::LineEnd, 1, true);
                self.enter_insert(false);
            }
            'o' => self.open_line_below(),
            'O' => self.open_line_above(),
            'd' => self.begin_operator(Operator::Delete, count, count_explicit),
            'c' => self.begin_operator(Operator::Change, count, count_explicit),
            'y' => self.begin_operator(Operator::Yank, count, count_explicit),
            '>' => self.begin_operator(Operator::Indent, count, count_explicit),
            '<' => self.begin_operator(Operator::Dedent, count, count_explicit),
            '=' => self.begin_operator(Operator::Reindent, count, count_explicit),
            'x' => self.delete_chars(count),
            'r' => {
                self.count = Some(count);
                self.mode = Mode::Replace;
                self.awaiting = Awaiting::Find {
                    backward: false,
                    till: false,
                    operator: false,
                };
            }
            'J' => self.join_lines(count),
            'p' => self.paste(false, count),
            'P' => self.paste(true, count),
            'u' => {
                for _ in 0..count {
                    self.undo();
                }
            }
            '.' => self.repeat_last_change(count),
            '/' => self.enter_search(false),
            '?' => self.enter_search(true),
            'n' => self.repeat_search(false, count),
            'N' => self.repeat_search(true, count),
            '*' => self.search_word(false),
            '#' => self.search_word(true),
            'v' => self.enter_visual(VisualKind::Character),
            'V' => self.enter_visual(VisualKind::Line),
            ':' => {
                self.clear_visual_anchors();
                self.mode = Mode::Command;
                self.prompt.clear();
            }
            ' ' => {
                self.clear_visual_anchors();
                self.mode = Mode::Leader;
                self.leader_prefix.clear();
            }
            '"' => self.awaiting = Awaiting::Register,
            'q' => {
                if self.macro_recording.is_some() {
                    self.stop_macro_recording();
                } else {
                    self.awaiting = Awaiting::MacroRecord;
                }
            }
            '@' => {
                self.count = Some(count);
                self.awaiting = Awaiting::MacroPlay;
            }
            '[' | ']' => self.awaiting = Awaiting::BracketPrefix(ch),
            'K' => self.request = EditorRequest::RustAnalyzer(CommandId::Hover),
            _ => self.message(format!("Unsupported Normal command: {ch}")),
        }
    }

    fn handle_visual_key(&mut self, key: Key, kind: VisualKind) {
        if matches!(key.code, KeyCode::Esc) {
            self.leave_visual();
            return;
        }
        if key.modifiers.contains(Modifiers::CONTROL) {
            match key.code {
                KeyCode::Char('v') => self.enter_visual(VisualKind::Block),
                KeyCode::Char(direction @ ('h' | 'j' | 'k' | 'l')) => {
                    self.focus_window_direction(direction)
                }
                _ => {}
            }
            return;
        }
        let KeyCode::Char(ch) = key.code else { return };
        if ch.is_ascii_digit() && (ch != '0' || self.count.is_some()) {
            self.push_count(ch);
            return;
        }
        let count_explicit = self.count.is_some();
        let count = self.take_count();
        match ch {
            'h' => self.move_cursor(Motion::Left, count, false),
            'j' => self.move_cursor(Motion::Down, count, false),
            'k' => self.move_cursor(Motion::Up, count, false),
            'l' => self.move_cursor(Motion::Right, count, false),
            'w' => self.move_cursor(Motion::Word, count, false),
            'b' => self.move_cursor(Motion::BackWord, count, false),
            'e' => self.move_cursor(Motion::EndWord, count, false),
            '0' => self.move_cursor(Motion::LineStart, 1, false),
            '^' => self.move_cursor(Motion::FirstNonBlank, 1, false),
            '$' => self.move_cursor(Motion::LineEnd, 1, false),
            'G' => self.move_cursor(
                if count_explicit {
                    Motion::FileStart
                } else {
                    Motion::FileEnd
                },
                count,
                false,
            ),
            '%' => self.move_cursor(Motion::MatchPair, 1, false),
            'g' => {
                self.count = (count != 1).then_some(count);
                self.awaiting = Awaiting::GPrefix;
            }
            'f' => self.await_find(false, false, false, count),
            'F' => self.await_find(true, false, false, count),
            't' => self.await_find(false, true, false, count),
            'T' => self.await_find(true, true, false, count),
            'd' | 'x' => self.apply_visual_operator(Operator::Delete, kind),
            'c' => self.apply_visual_operator(Operator::Change, kind),
            'y' => self.apply_visual_operator(Operator::Yank, kind),
            '>' => self.apply_visual_operator(Operator::Indent, kind),
            '<' => self.apply_visual_operator(Operator::Dedent, kind),
            '=' => self.apply_visual_operator(Operator::Reindent, kind),
            'v' => {
                if kind == VisualKind::Character {
                    self.leave_visual()
                } else {
                    self.enter_visual(VisualKind::Character)
                }
            }
            'V' => {
                if kind == VisualKind::Line {
                    self.leave_visual()
                } else {
                    self.enter_visual(VisualKind::Line)
                }
            }
            ':' => {
                self.clear_visual_anchors();
                self.mode = Mode::Command;
                self.prompt.clear();
            }
            ' ' => {
                self.clear_visual_anchors();
                self.mode = Mode::Leader;
                self.leader_prefix.clear();
            }
            _ => self.message(format!("Unsupported Visual command: {ch}")),
        }
    }

    fn handle_awaiting(&mut self, key: Key) -> bool {
        let awaiting = self.awaiting;
        if matches!(awaiting, Awaiting::None) {
            return false;
        }
        if matches!(key.code, KeyCode::Esc) {
            self.cancel_pending();
            return true;
        }
        let KeyCode::Char(ch) = key.code else {
            self.message("Expected a character");
            self.cancel_pending();
            return true;
        };
        match awaiting {
            Awaiting::None => false,
            // Window-prefix input is intercepted by `handle_key` before focus or
            // mode-specific dispatch, so its second key cannot leak into the
            // explorer or the active buffer.
            Awaiting::WindowPrefix => unreachable!("window prefix handled at top level"),
            Awaiting::Register => {
                if ch == '"' || ch == '+' || ch == '*' || ch.is_ascii_alphabetic() {
                    self.selected_register = ch.to_ascii_lowercase();
                } else {
                    self.message(format!("Invalid register: {ch}"));
                }
                self.awaiting = Awaiting::None;
                true
            }
            Awaiting::MacroRecord => {
                self.awaiting = Awaiting::None;
                if ch.is_ascii_alphabetic() {
                    self.macro_recording = Some((ch.to_ascii_lowercase(), Vec::new()));
                    self.message(format!("Recording macro @{ch}"));
                } else {
                    self.message("Macro register must be a-z");
                }
                true
            }
            Awaiting::MacroPlay => {
                self.awaiting = Awaiting::None;
                let register = if ch == '@' {
                    self.last_macro
                } else {
                    Some(ch.to_ascii_lowercase())
                };
                let count = self.take_count();
                if let Some(register) = register {
                    self.play_macro(register, count);
                } else {
                    self.message("No previous macro");
                }
                true
            }
            Awaiting::GPrefix => {
                self.awaiting = Awaiting::None;
                match ch {
                    'g' => {
                        let count = self.take_count();
                        if self.pending_operator.is_some() {
                            self.apply_operator_motion(Motion::FileStart, count);
                        } else {
                            self.move_cursor(Motion::FileStart, count, false);
                        }
                    }
                    'd' => self.request = EditorRequest::RustAnalyzer(CommandId::Definition),
                    'D' => self.request = EditorRequest::RustAnalyzer(CommandId::Declaration),
                    'y' => self.request = EditorRequest::RustAnalyzer(CommandId::TypeDefinition),
                    'I' => self.request = EditorRequest::RustAnalyzer(CommandId::Implementation),
                    'r' => self.request = EditorRequest::RustAnalyzer(CommandId::References),
                    'l' => self.show_line_diagnostic(),
                    '-' => {
                        let count = self.take_count();
                        for _ in 0..count {
                            self.undo();
                        }
                    }
                    '+' => {
                        let count = self.take_count();
                        for _ in 0..count {
                            self.redo();
                        }
                    }
                    'c' => {
                        let count_explicit = self.count.is_some();
                        let count = self.take_count();
                        self.begin_operator(Operator::Comment, count, count_explicit);
                    }
                    _ => {
                        self.message(format!("Unsupported g command: g{ch}"));
                        self.cancel_pending();
                    }
                }
                true
            }
            Awaiting::BracketPrefix(prefix) => {
                self.awaiting = Awaiting::None;
                if ch == 'd' {
                    let count = self.take_count();
                    self.navigate_diagnostic(prefix == '[', count);
                } else {
                    self.message(format!("Unsupported command: {prefix}{ch}"));
                }
                true
            }
            Awaiting::Find {
                backward,
                till,
                operator,
            } => {
                self.awaiting = Awaiting::None;
                if matches!(self.mode, Mode::Replace) {
                    self.mode = Mode::Normal;
                    let count = self.take_count();
                    self.replace_chars(ch, count);
                    return true;
                }
                let spec = FindSpec {
                    character: ch,
                    backward,
                    till,
                };
                self.last_find = Some(spec);
                let count = self.take_count();
                if let Some((target, matched)) = self.find_target(spec, count, None) {
                    self.last_find_match = Some(matched);
                    if operator {
                        self.apply_operator_target(target, !till || !backward, false);
                    } else {
                        self.set_cursor(target, false);
                    }
                } else {
                    self.last_find_match = None;
                    self.message(format!("Character not found: {ch}"));
                    self.cancel_pending();
                }
                if matches!(self.mode, Mode::Find) {
                    self.mode = Mode::Normal;
                }
                true
            }
            Awaiting::TextObject { around } => {
                self.awaiting = Awaiting::None;
                if let Some(range) = self.text_object(ch, around) {
                    self.apply_operator_range(range, RegisterKind::Character);
                } else {
                    self.message(format!(
                        "Text object not found: {}{ch}",
                        if around { 'a' } else { 'i' }
                    ));
                    self.cancel_pending();
                }
                true
            }
        }
    }

    fn handle_operator_key(&mut self, key: Key) {
        if matches!(key.code, KeyCode::Esc) {
            self.cancel_pending();
            return;
        }
        let KeyCode::Char(ch) = key.code else {
            self.message("Operator requires a motion");
            self.cancel_pending();
            return;
        };
        if ch.is_ascii_digit() && (ch != '0' || self.count.is_some()) {
            self.push_count(ch);
            return;
        }
        let inner_count_explicit = self.count.is_some();
        let inner = self.take_count();
        let pending = self.pending_operator.expect("operator exists");
        let total = pending.count.saturating_mul(inner).max(1);
        let repeated = matches!(
            (pending.operator, ch),
            (Operator::Delete, 'd')
                | (Operator::Change, 'c')
                | (Operator::Yank, 'y')
                | (Operator::Indent, '>')
                | (Operator::Dedent, '<')
                | (Operator::Reindent, '=')
                | (Operator::Comment, 'c')
        );
        if repeated {
            self.apply_line_operator(total);
            return;
        }
        match ch {
            'h' => self.apply_operator_motion(Motion::Left, total),
            'j' => self.apply_operator_motion(Motion::Down, total),
            'k' => self.apply_operator_motion(Motion::Up, total),
            'l' => self.apply_operator_motion(Motion::Right, total),
            'w' => self.apply_operator_motion(Motion::Word, total),
            'b' => self.apply_operator_motion(Motion::BackWord, total),
            'e' => self.apply_operator_motion(Motion::EndWord, total),
            '0' => self.apply_operator_motion(Motion::LineStart, 1),
            '^' => self.apply_operator_motion(Motion::FirstNonBlank, 1),
            '$' => self.apply_operator_motion(Motion::LineEnd, total),
            'G' => self.apply_operator_motion(
                if pending.count_explicit || inner_count_explicit {
                    Motion::FileStart
                } else {
                    Motion::FileEnd
                },
                total,
            ),
            '%' => self.apply_operator_motion(Motion::MatchPair, 1),
            'g' => {
                self.count = (total != 1).then_some(total);
                self.awaiting = Awaiting::GPrefix;
            }
            'f' => self.await_find(false, false, true, total),
            'F' => self.await_find(true, false, true, total),
            't' => self.await_find(false, true, true, total),
            'T' => self.await_find(true, true, true, total),
            'i' => self.awaiting = Awaiting::TextObject { around: false },
            'a' => self.awaiting = Awaiting::TextObject { around: true },
            _ => {
                self.message(format!("Unsupported operator motion: {ch}"));
                self.cancel_pending();
            }
        }
    }

    fn handle_leader_key(&mut self, key: Key) {
        if matches!(key.code, KeyCode::Esc) {
            self.clear_visual_anchors();
            self.mode = Mode::Normal;
            self.leader_prefix.clear();
            return;
        }
        let KeyCode::Char(ch) = key.code else { return };
        self.leader_prefix.push(ch);
        if let Some(command) = command::by_sequence(&self.leader_prefix) {
            let id = command.id;
            self.clear_visual_anchors();
            self.mode = Mode::Normal;
            self.leader_prefix.clear();
            self.execute_command(id);
        } else if !command::has_prefix(&self.leader_prefix) {
            let sequence = std::mem::take(&mut self.leader_prefix);
            self.clear_visual_anchors();
            self.mode = Mode::Normal;
            self.message(format!("Unknown leader sequence: <Space>{sequence}"));
        }
    }

    fn handle_picker_key(&mut self, key: Key) {
        if self.picker.as_ref().is_some_and(|picker| {
            picker.kind == PickerKind::Symbols && picker.return_mode == Mode::Insert
        }) && (matches!(key.code, KeyCode::Backspace | KeyCode::Delete)
            || matches!(key.code, KeyCode::Char(_) if !key.modifiers.contains(Modifiers::CONTROL)))
        {
            if let Some(picker) = self.picker.take() {
                self.mode = picker.return_mode;
            }
            self.handle_key(key);
            return;
        }
        match key.code {
            KeyCode::Esc => {
                if let Some(picker) = self.picker.take() {
                    self.mode = picker.return_mode;
                }
            }
            KeyCode::Up => self.picker_move(-1),
            KeyCode::Down => self.picker_move(1),
            KeyCode::Char('n') if key.modifiers.contains(Modifiers::CONTROL) => self.picker_move(1),
            KeyCode::Char('p') if key.modifiers.contains(Modifiers::CONTROL) => {
                self.picker_move(-1)
            }
            KeyCode::Backspace => {
                if let Some(picker) = self.picker.as_mut() {
                    picker.query.pop();
                }
                self.refresh_picker();
            }
            KeyCode::Enter => self.accept_picker(false),
            KeyCode::Char('v') if key.modifiers.contains(Modifiers::CONTROL) => {
                self.accept_picker_split(Orientation::Vertical)
            }
            KeyCode::Char('s') if key.modifiers.contains(Modifiers::CONTROL) => {
                self.accept_picker_split(Orientation::Horizontal)
            }
            KeyCode::Char(ch) if !key.modifiers.contains(Modifiers::CONTROL) => {
                if let Some(picker) = self.picker.as_mut() {
                    picker.query.push(ch);
                }
                self.refresh_picker();
            }
            _ => {}
        }
    }

    fn handle_explorer_key(&mut self, key: Key) {
        if self.leader_prefix == " " {
            self.leader_prefix.clear();
            if key == Key::char('e') {
                self.execute_command(CommandId::ExplorerToggle);
                return;
            }
        }
        if matches!(key.code, KeyCode::Esc) {
            self.focus = Focus::Editor;
            return;
        }
        if key.modifiers.contains(Modifiers::CONTROL) {
            match key.code {
                KeyCode::Char('l') => self.focus_window_direction('l'),
                _ => self.message("Unsupported control key in explorer"),
            }
            return;
        }
        let KeyCode::Char(ch) = key.code else {
            match key.code {
                KeyCode::Up => self.explorer_move(-1),
                KeyCode::Down => self.explorer_move(1),
                KeyCode::Enter => self.explorer_open_selected(),
                _ => {}
            }
            return;
        };
        match ch {
            ' ' => self.leader_prefix.push(' '),
            'j' => self.explorer_move(1), 'k' => self.explorer_move(-1),
            'h' => self.focus = Focus::Editor, 'l' | '\n' => self.explorer_open_selected(),
            'e' => { self.explorer.show_hidden = !self.explorer.show_hidden; self.request = EditorRequest::RefreshProject; },
            'i' => { self.explorer.show_ignored = !self.explorer.show_ignored; self.request = EditorRequest::RefreshProject; },
            'a' => self.message("Use :e PATH to create a new file"),
            'r' => self.message("Use :saveas PATH to rename safely"),
            'd' => self.message("File deletion requires an explicit filesystem confirmation and is not available in this build"),
            _ => {}
        }
    }

    fn handle_window_prefix_key(&mut self, key: Key) -> bool {
        if !matches!(self.awaiting, Awaiting::WindowPrefix) {
            return false;
        }

        self.awaiting = Awaiting::None;
        if matches!(key.code, KeyCode::Esc) {
            return true;
        }

        let KeyCode::Char(ch) = key.code else {
            self.message("Window command requires h, j, k, or l");
            return true;
        };
        let supported_modifiers =
            key.modifiers == Modifiers::empty() || key.modifiers == Modifiers::CONTROL;
        if supported_modifiers && matches!(ch, 'h' | 'j' | 'k' | 'l') {
            self.focus_window_direction(ch);
        } else {
            self.message(format!("Unsupported window command: <C-w>{ch}"));
        }
        true
    }

    fn focus_window_direction(&mut self, direction: char) {
        if self.focus == Focus::Terminal {
            if direction == 'k' {
                self.focus = Focus::Editor;
            } else {
                self.message("Use <C-\\> to return to the editor");
            }
            return;
        }
        if self.focus == Focus::Explorer {
            if direction == 'l' {
                self.focus = Focus::Editor;
            } else {
                self.message("No window in that direction");
            }
            return;
        }

        if direction == 'h' && self.explorer.open {
            self.focus = Focus::Explorer;
            return;
        }

        if direction == 'j' && self.terminal.visible {
            self.focus = Focus::Terminal;
            return;
        }

        self.cycle_pane(if matches!(direction, 'h' | 'k') {
            -1
        } else {
            1
        });
    }

    fn execute_command(&mut self, id: CommandId) {
        match id {
            CommandId::FindFiles => self.open_picker(PickerKind::Files),
            CommandId::ProjectGrep => self.open_picker(PickerKind::Grep),
            CommandId::ExplorerToggle => {
                if self.explorer.open && self.focus == Focus::Explorer {
                    self.explorer.open = false;
                    self.focus = Focus::Editor;
                } else {
                    self.explorer.open = true;
                    self.focus = Focus::Explorer;
                    self.reveal_active_file();
                    if self.explorer.files.is_empty() {
                        self.request = EditorRequest::RefreshProject;
                    }
                }
            }
            CommandId::TerminalToggle => {
                self.terminal.visible = !self.terminal.visible;
                if self.terminal.visible {
                    self.mode = Mode::Normal;
                    self.focus = Focus::Terminal;
                    self.request = EditorRequest::TerminalToggle(true);
                    self.message("Terminal opened; <C-\\> returns to the editor");
                } else {
                    if self.focus == Focus::Terminal {
                        self.focus = Focus::Editor;
                    }
                    self.request = EditorRequest::TerminalToggle(false);
                    self.message("Terminal hidden");
                }
            }
            CommandId::BufferSwitch => self.open_picker(PickerKind::Buffers),
            CommandId::BufferClose => self.close_buffer(false),
            CommandId::BufferNext => self.cycle_buffer(1),
            CommandId::BufferPrevious => self.cycle_buffer(-1),
            CommandId::RecentFiles => self.open_picker(PickerKind::Recent),
            CommandId::Messages => self.open_picker(PickerKind::Messages),
            CommandId::BufferDiagnostics => self.open_buffer_diagnostics(),
            CommandId::WorkspaceDiagnostics => self.open_picker(PickerKind::Diagnostics),
            CommandId::SplitBelow => self.split(Orientation::Horizontal),
            CommandId::SplitRight => self.split(Orientation::Vertical),
            CommandId::ClosePane => self.close_pane(),
            CommandId::OnlyPane => self.only_pane(),
            CommandId::ToggleInlayHints => {
                self.inlay_hints = !self.inlay_hints;
                self.message(format!(
                    "Inlay hints {}",
                    if self.inlay_hints {
                        "enabled"
                    } else {
                        "disabled"
                    }
                ));
            }
            CommandId::Rename => {
                self.mode = Mode::Command;
                self.prompt = "rename ".into();
            }
            CommandId::RustAnalyzerRestart => {
                self.request = EditorRequest::RustAnalyzer(id);
                self.message("Restarting rust-analyzer");
            }
            CommandId::CodeAction
            | CommandId::Format
            | CommandId::DocumentSymbols
            | CommandId::WorkspaceSymbols => {
                if self.rust_analyzer_status != "ready" {
                    self.message(format!(
                        "{} unavailable: rust-analyzer is {}",
                        id, self.rust_analyzer_status
                    ));
                } else {
                    self.request = EditorRequest::RustAnalyzer(id);
                }
            }
            CommandId::Hover
            | CommandId::Definition
            | CommandId::Declaration
            | CommandId::TypeDefinition
            | CommandId::Implementation
            | CommandId::References
            | CommandId::Completion
            | CommandId::SignatureHelp => {
                if self.rust_analyzer_status != "ready" {
                    self.message(format!(
                        "{} unavailable: rust-analyzer is {}",
                        id, self.rust_analyzer_status
                    ));
                } else {
                    self.request = EditorRequest::RustAnalyzer(id);
                }
            }
            CommandId::CodexToggle
            | CommandId::CodexStatus
            | CommandId::CodexRestart
            | CommandId::CodexRunOnce
            | CommandId::CodexLogs
            | CommandId::CodexDryRun
            | CommandId::CodexWorkspaceWrite => self.request = EditorRequest::CodexWatch(id),
        }
    }

    fn push_count(&mut self, digit: char) {
        let digit = digit.to_digit(10).unwrap_or(0) as usize;
        self.count = Some(
            self.count
                .unwrap_or(0)
                .saturating_mul(10)
                .saturating_add(digit)
                .min(1_000_000),
        );
    }

    fn take_count(&mut self) -> usize {
        self.count.take().unwrap_or(1).max(1)
    }

    fn cancel_pending(&mut self) {
        self.count = None;
        self.pending_operator = None;
        self.awaiting = Awaiting::None;
        self.leader_prefix.clear();
        if !matches!(self.mode, Mode::Insert | Mode::Visual(_)) {
            self.mode = Mode::Normal;
        }
        self.selected_register = '"';
    }

    fn begin_operator(&mut self, operator: Operator, count: usize, count_explicit: bool) {
        self.pending_operator = Some(PendingOperator {
            operator,
            count: count.max(1),
            count_explicit,
        });
        self.mode = Mode::OperatorPending;
    }

    fn await_find(&mut self, backward: bool, till: bool, operator: bool, count: usize) {
        self.count = Some(count);
        self.awaiting = Awaiting::Find {
            backward,
            till,
            operator,
        };
        if !operator && !matches!(self.mode, Mode::Visual(_)) {
            self.mode = Mode::Find;
        }
    }

    fn set_cursor(&mut self, pos: Pos, insert: bool) {
        let pos = if insert {
            self.active_buffer().clamp_pos(pos)
        } else {
            self.normal_clamp(pos)
        };
        let pane = self.active_pane_mut();
        pane.cursor = pos;
        pane.desired_column = pos.grapheme;
    }

    fn normal_clamp(&self, pos: Pos) -> Pos {
        let mut pos = self.active_buffer().clamp_pos(pos);
        let count = self.active_buffer().grapheme_count(pos.line).unwrap_or(0);
        if count > 0 {
            pos.grapheme = pos.grapheme.min(count - 1);
        } else {
            pos.grapheme = 0;
        }
        pos
    }

    fn clamp_active_cursor(&mut self) {
        let pos = self.normal_clamp(self.active_pane().cursor);
        self.active_pane_mut().cursor = pos;
    }

    fn move_cursor(&mut self, motion: Motion, count: usize, insert: bool) {
        if let Some(target) =
            self.motion_target(self.active_pane().cursor, motion, count.max(1), insert)
        {
            self.set_cursor(target, insert);
        }
    }

    fn motion_target(
        &self,
        mut pos: Pos,
        motion: Motion,
        count: usize,
        insert: bool,
    ) -> Option<Pos> {
        let buffer = self.active_buffer();
        match motion {
            Motion::Left => {
                for _ in 0..count {
                    if pos.grapheme > 0 {
                        pos.grapheme -= 1;
                    }
                }
            }
            Motion::Right => {
                for _ in 0..count {
                    let end = buffer.grapheme_count(pos.line)?;
                    let maximum = if insert { end } else { end.saturating_sub(1) };
                    pos.grapheme = (pos.grapheme + 1).min(maximum);
                }
            }
            Motion::Up | Motion::Down => {
                let delta = if matches!(motion, Motion::Up) {
                    -(count.min(isize::MAX as usize) as isize)
                } else {
                    count.min(isize::MAX as usize) as isize
                };
                pos = buffer
                    .move_vertical(pos, delta, self.active_pane().desired_column)
                    .ok()?;
            }
            Motion::Word => {
                for _ in 0..count {
                    pos = self.word_forward(pos)?;
                }
            }
            Motion::BackWord => {
                for _ in 0..count {
                    pos = self.word_backward(pos)?;
                }
            }
            Motion::EndWord => {
                for _ in 0..count {
                    pos = self.word_end(pos)?;
                }
            }
            Motion::LineStart => pos.grapheme = 0,
            Motion::FirstNonBlank => {
                pos.grapheme = first_nonblank(buffer.line(pos.line).unwrap_or(""))
            }
            Motion::LineEnd => {
                pos.line = (pos.line + count.saturating_sub(1)).min(buffer.line_count() - 1);
                let end = buffer.grapheme_count(pos.line)?;
                pos.grapheme = if insert { end } else { end.saturating_sub(1) };
            }
            Motion::FileStart => {
                pos.line = count.saturating_sub(1).min(buffer.line_count() - 1);
                pos.grapheme = 0;
            }
            Motion::FileEnd => {
                pos.line = if count > 1 {
                    count - 1
                } else {
                    buffer.line_count() - 1
                }
                .min(buffer.line_count() - 1);
                let end = buffer.grapheme_count(pos.line)?;
                pos.grapheme = if insert { end } else { end.saturating_sub(1) };
            }
            Motion::MatchPair => pos = self.match_pair(pos)?,
        }
        Some(pos)
    }

    fn word_forward(&self, pos: Pos) -> Option<Pos> {
        let mut graphemes = word_graphemes(self.active_buffer(), pos);
        let (_, initial) = graphemes.next()?;
        let mut in_initial_run = initial != 0;
        let mut end = pos;
        for (next, class) in graphemes {
            end = next;
            if in_initial_run && class == initial {
                continue;
            }
            in_initial_run = false;
            if class != 0 {
                return Some(next);
            }
        }
        // Operators need the insertion-point boundary at EOF to include the
        // final grapheme. Normal movement clamps that boundary afterwards.
        Some(end)
    }

    fn word_backward(&self, pos: Pos) -> Option<Pos> {
        let buffer = self.active_buffer();
        let mut run_class = None;
        let mut start = Pos::ZERO;
        for line_number in (0..=pos.line).rev() {
            let mut graphemes = buffer.line(line_number)?.graphemes(true);
            let mut column = graphemes.clone().count();
            let before = if line_number == pos.line {
                pos.grapheme.min(column)
            } else {
                column
            };
            // Locate the starting boundary once, then retain the iterator as
            // we move backwards instead of restarting segmentation per step.
            while column > before {
                graphemes.next_back();
                column -= 1;
            }
            for grapheme in graphemes.rev() {
                column -= 1;
                let class = grapheme_class(Some(grapheme));
                if let Some(current) = run_class {
                    if current != class {
                        return Some(start);
                    }
                } else if class == 0 {
                    continue;
                } else {
                    run_class = Some(class);
                }
                start = Pos::new(line_number, column);
            }
            // A line separator ends a non-whitespace run.
            if run_class.is_some() {
                return Some(start);
            }
        }
        Some(Pos::ZERO)
    }

    fn word_end(&self, pos: Pos) -> Option<Pos> {
        let mut graphemes = word_graphemes(self.active_buffer(), pos);
        graphemes.next()?;
        let mut run_class = None;
        let mut end = pos;
        for (next, class) in graphemes {
            if let Some(current) = run_class {
                if current != class {
                    return Some(self.normal_clamp(end));
                }
                end = next;
            } else if class != 0 {
                run_class = Some(class);
                end = next;
            }
        }
        Some(self.normal_clamp(end))
    }

    fn match_pair(&self, pos: Pos) -> Option<Pos> {
        let buffer = self.active_buffer();
        let line = buffer.line(pos.line)?;
        let byte_in_line = byte_for_grapheme(line, pos.grapheme);
        let mut byte = buffer.pos_to_byte(Pos::new(pos.line, pos.grapheme)).ok()?.0;
        let text = buffer.text();
        let line_remaining = &line[byte_in_line..];
        let relative = line_remaining
            .char_indices()
            .find(|(_, ch)| "()[]{}".contains(*ch))?
            .0;
        byte += relative;
        let delimiter = text[byte..].chars().next()?;
        let (mate, direction) = match delimiter {
            '(' => (')', 1),
            '[' => (']', 1),
            '{' => ('}', 1),
            ')' => ('(', -1),
            ']' => ('[', -1),
            '}' => ('{', -1),
            _ => return None,
        };
        let bytes = text.as_bytes();
        let mut depth = 0_i32;
        if direction > 0 {
            for (index, value) in bytes.iter().enumerate().skip(byte) {
                if *value == delimiter as u8 {
                    depth += 1;
                } else if *value == mate as u8 {
                    depth -= 1;
                    if depth == 0 {
                        return buffer.byte_to_pos(crate::buffer::ByteOffset(index)).ok();
                    }
                }
            }
        } else {
            for index in (0..=byte).rev() {
                if bytes[index] == delimiter as u8 {
                    depth += 1;
                } else if bytes[index] == mate as u8 {
                    depth -= 1;
                    if depth == 0 {
                        return buffer.byte_to_pos(crate::buffer::ByteOffset(index)).ok();
                    }
                }
            }
        }
        None
    }

    fn apply_operator_motion(&mut self, motion: Motion, count: usize) {
        let operator = self.pending_operator.map(|pending| pending.operator);
        let version = self.active_buffer().version();
        let current = self.active_pane().cursor;
        // Vim treats `cw` like `ce` while on a word or punctuation run, so the
        // following whitespace remains. On whitespace, `cw` retains ordinary
        // `w` motion behavior and changes only that whitespace.
        let target_motion = if operator == Some(Operator::Change)
            && matches!(motion, Motion::Word)
            && grapheme_class(grapheme_at(self.active_buffer(), current)) != 0
        {
            Motion::EndWord
        } else {
            motion
        };
        let Some(target) = self.motion_target(current, target_motion, count, false) else {
            self.message("Motion reached the buffer boundary");
            self.cancel_pending();
            return;
        };
        let linewise = matches!(
            target_motion,
            Motion::Up | Motion::Down | Motion::FileStart | Motion::FileEnd
        );
        let inclusive = matches!(
            target_motion,
            Motion::EndWord | Motion::LineEnd | Motion::MatchPair
        );
        self.apply_operator_target(target, inclusive, linewise);
        if operator == Some(Operator::Delete) && self.active_buffer().version() != version {
            self.last_change = Some(LastChange::DeleteMotion(motion, count));
        }
    }

    fn apply_operator_target(&mut self, target: Pos, inclusive: bool, linewise: bool) {
        let cursor = self.active_pane().cursor;
        let range = if linewise {
            self.line_range(cursor.line.min(target.line), cursor.line.max(target.line))
        } else if target >= cursor {
            let end = if inclusive {
                self.active_buffer()
                    .next_pos(target)
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| self.active_buffer().line_end(target.line).unwrap_or(target))
            } else {
                target
            };
            TextRange::new(cursor, end)
        } else {
            let end = if inclusive {
                self.active_buffer()
                    .next_pos(cursor)
                    .ok()
                    .flatten()
                    .unwrap_or(cursor)
            } else {
                cursor
            };
            TextRange::new(target, end)
        };
        self.apply_operator_range(
            range,
            if linewise {
                RegisterKind::Line
            } else {
                RegisterKind::Character
            },
        );
    }

    fn apply_line_operator(&mut self, count: usize) {
        let operator = self.pending_operator.map(|pending| pending.operator);
        let version = self.active_buffer().version();
        let start = self.active_pane().cursor.line;
        let end = (start + count.saturating_sub(1)).min(self.active_buffer().line_count() - 1);
        let range = self.line_range(start, end);
        if operator == Some(Operator::Delete)
            && start > 0
            && end + 1 == self.active_buffer().line_count()
        {
            self.apply_final_line_delete(range, start);
        } else {
            self.apply_operator_range(range, RegisterKind::Line);
        }
        if operator == Some(Operator::Delete) && self.active_buffer().version() != version {
            self.last_change = Some(LastChange::DeleteLines(count));
        }
    }

    fn apply_final_line_delete(&mut self, register_range: TextRange, first_line: usize) {
        let text = match self.active_buffer().text_in_range(register_range) {
            Ok(text) => text,
            Err(error) => {
                self.message(error.to_string());
                self.cancel_pending();
                return;
            }
        };
        self.write_register(text, RegisterKind::Line);
        let start = match self.active_buffer().line_end(first_line - 1) {
            Ok(start) => start,
            Err(error) => {
                self.message(error.to_string());
                self.cancel_pending();
                return;
            }
        };
        let edit_range = TextRange::new(start, register_range.end);
        let result = (|| -> Result<(), BufferError> {
            self.active_buffer_mut().begin_transaction()?;
            self.active_buffer_mut().delete(edit_range)?;
            self.active_buffer_mut().commit_transaction()?;
            Ok(())
        })();
        match result {
            Ok(()) => {
                let line = first_line - 1;
                let column = first_nonblank(self.active_buffer().line(line).unwrap_or(""));
                self.set_cursor(Pos::new(line, column), false);
                self.pending_operator = None;
                self.awaiting = Awaiting::None;
                self.selected_register = '"';
                self.mode = Mode::Normal;
            }
            Err(error) => {
                if self.active_buffer().in_transaction() {
                    let _ = self.active_buffer_mut().rollback_transaction();
                }
                self.message(error.to_string());
                self.cancel_pending();
            }
        }
    }

    fn line_range(&self, start_line: usize, end_line: usize) -> TextRange {
        let buffer = self.active_buffer();
        let start = Pos::new(start_line.min(buffer.line_count() - 1), 0);
        let end_line = end_line.min(buffer.line_count() - 1);
        let end = if end_line + 1 < buffer.line_count() {
            Pos::new(end_line + 1, 0)
        } else {
            buffer.line_end(end_line).unwrap_or(start)
        };
        TextRange::new(start, end)
    }

    fn apply_operator_range(&mut self, range: TextRange, kind: RegisterKind) {
        let Some(pending) = self.pending_operator else {
            return;
        };
        let range = range.ordered();
        if range.is_empty() {
            self.message("Empty motion");
            self.cancel_pending();
            return;
        }
        if matches!(
            pending.operator,
            Operator::Indent | Operator::Dedent | Operator::Reindent | Operator::Comment
        ) {
            self.apply_line_transform(range, pending.operator);
            self.cancel_pending();
            return;
        }
        let text = match self.active_buffer().text_in_range(range) {
            Ok(text) => text,
            Err(error) => {
                self.message(error.to_string());
                self.cancel_pending();
                return;
            }
        };
        self.write_register(text, kind);
        if pending.operator == Operator::Yank {
            self.set_cursor(range.start, false);
            self.cancel_pending();
            self.message("Yanked");
            return;
        }
        let change = pending.operator == Operator::Change;
        let result = (|| -> Result<(), BufferError> {
            self.active_buffer_mut().begin_transaction()?;
            self.active_buffer_mut().delete(range)?;
            if !change {
                self.active_buffer_mut().commit_transaction()?;
            }
            Ok(())
        })();
        match result {
            Ok(()) => {
                let cursor = if change {
                    self.active_buffer().clamp_pos(range.start)
                } else {
                    self.normal_clamp(range.start)
                };
                self.active_pane_mut().cursor = cursor;
                let motion_count = pending.count;
                self.last_change = Some(LastChange::DeleteMotion(Motion::Word, motion_count));
                self.pending_operator = None;
                self.awaiting = Awaiting::None;
                self.selected_register = '"';
                if change {
                    self.enter_insert(true);
                } else {
                    self.mode = Mode::Normal;
                }
            }
            Err(error) => {
                if self.active_buffer().in_transaction() {
                    let _ = self.active_buffer_mut().rollback_transaction();
                }
                self.message(error.to_string());
                self.cancel_pending();
            }
        }
    }

    fn apply_line_transform(&mut self, range: TextRange, operator: Operator) {
        let first = range.start.line;
        let mut last = range.end.line;
        if range.end.grapheme == 0 && last > first {
            last -= 1;
        }
        last = last.min(self.active_buffer().line_count() - 1);
        let tab_width = self.config.editor.tab_width;
        let comment = match self
            .active_buffer()
            .path()
            .and_then(Path::extension)
            .and_then(|x| x.to_str())
        {
            Some("toml") => "# ",
            _ => "// ",
        };
        let uncomment = operator == Operator::Comment
            && (first..=last).all(|line| {
                self.active_buffer()
                    .line(line)
                    .unwrap_or("")
                    .trim_start()
                    .starts_with(comment.trim_end())
            });
        let result = (|| -> Result<(), BufferError> {
            self.active_buffer_mut().begin_transaction()?;
            for line_number in first..=last {
                let original = self
                    .active_buffer()
                    .line(line_number)
                    .unwrap_or("")
                    .to_owned();
                let replacement = match operator {
                    Operator::Indent => format!("{}{}", " ".repeat(tab_width), original),
                    Operator::Dedent => {
                        if let Some(rest) = original.strip_prefix('\t') {
                            rest.to_owned()
                        } else {
                            original
                                .strip_prefix(&" ".repeat(tab_width))
                                .unwrap_or_else(|| original.trim_start_matches(' '))
                                .to_owned()
                        }
                    }
                    Operator::Reindent => {
                        let depth = (0..line_number).fold(0_isize, |mut depth, index| {
                            for ch in self.active_buffer().line(index).unwrap_or("").chars() {
                                if ch == '{' {
                                    depth += 1;
                                } else if ch == '}' {
                                    depth = (depth - 1).max(0);
                                }
                            }
                            depth
                        });
                        let trimmed = original.trim_start();
                        let depth = if trimmed.starts_with('}') {
                            (depth - 1).max(0)
                        } else {
                            depth
                        } as usize;
                        format!("{}{}", " ".repeat(depth * tab_width), trimmed)
                    }
                    Operator::Comment if uncomment => {
                        let spaces = original.len() - original.trim_start().len();
                        let (indent, rest) = original.split_at(spaces);
                        let rest = rest
                            .strip_prefix(comment.trim_end())
                            .unwrap_or(rest)
                            .strip_prefix(' ')
                            .unwrap_or_else(|| {
                                rest.strip_prefix(comment.trim_end()).unwrap_or(rest)
                            });
                        format!("{indent}{rest}")
                    }
                    Operator::Comment => {
                        let spaces = original.len() - original.trim_start().len();
                        let (indent, rest) = original.split_at(spaces);
                        format!("{indent}{comment}{rest}")
                    }
                    _ => original,
                };
                let end = self.active_buffer().line_end(line_number)?;
                self.active_buffer_mut()
                    .replace(TextRange::new(Pos::new(line_number, 0), end), &replacement)?;
            }
            self.active_buffer_mut().commit_transaction()?;
            Ok(())
        })();
        match result {
            Ok(()) => {
                self.set_cursor(Pos::new(first, 0), false);
                self.mode = Mode::Normal;
            }
            Err(error) => {
                if self.active_buffer().in_transaction() {
                    let _ = self.active_buffer_mut().rollback_transaction();
                }
                self.message(error.to_string());
            }
        }
    }

    fn enter_visual(&mut self, kind: VisualKind) {
        let cursor = self.active_pane().cursor;
        let pane = self.active_pane_mut();
        pane.anchor.get_or_insert(cursor);
        self.mode = Mode::Visual(kind);
    }

    fn leave_visual(&mut self) {
        self.clear_visual_anchors();
        self.mode = Mode::Normal;
        self.cancel_pending();
    }

    fn clear_visual_anchors(&mut self) {
        for pane in &mut self.panes {
            pane.anchor = None;
        }
    }

    pub fn visual_range(&self) -> Option<(TextRange, VisualKind)> {
        let Mode::Visual(kind) = self.mode else {
            return None;
        };
        let pane = self.active_pane();
        let anchor = pane.anchor?;
        let cursor = pane.cursor;
        let range = match kind {
            VisualKind::Line => {
                self.line_range(anchor.line.min(cursor.line), anchor.line.max(cursor.line))
            }
            VisualKind::Character | VisualKind::Block => {
                let (start, last) = if anchor <= cursor {
                    (anchor, cursor)
                } else {
                    (cursor, anchor)
                };
                let end = self
                    .active_buffer()
                    .next_pos(last)
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| self.active_buffer().line_end(last.line).unwrap_or(last));
                TextRange::new(start, end)
            }
        };
        Some((range, kind))
    }

    fn apply_visual_operator(&mut self, operator: Operator, kind: VisualKind) {
        let anchor = self
            .active_pane()
            .anchor
            .unwrap_or(self.active_pane().cursor);
        let cursor = self.active_pane().cursor;
        self.pending_operator = Some(PendingOperator {
            operator,
            count: 1,
            count_explicit: false,
        });
        if kind != VisualKind::Block {
            let range = if kind == VisualKind::Line {
                self.line_range(anchor.line.min(cursor.line), anchor.line.max(cursor.line))
            } else {
                let (start, last) = if anchor <= cursor {
                    (anchor, cursor)
                } else {
                    (cursor, anchor)
                };
                let end = self
                    .active_buffer()
                    .next_pos(last)
                    .ok()
                    .flatten()
                    .unwrap_or(last);
                TextRange::new(start, end)
            };
            self.apply_operator_range(
                range,
                if kind == VisualKind::Line {
                    RegisterKind::Line
                } else {
                    RegisterKind::Character
                },
            );
            self.active_pane_mut().anchor = None;
            if matches!(self.mode, Mode::Visual(_)) {
                self.mode = Mode::Normal;
            }
            return;
        }
        let top = anchor.line.min(cursor.line);
        let bottom = anchor.line.max(cursor.line);
        if matches!(
            operator,
            Operator::Indent | Operator::Dedent | Operator::Reindent | Operator::Comment
        ) {
            let range = self.line_range(top, bottom);
            self.apply_line_transform(range, operator);
            self.active_pane_mut().anchor = None;
            self.mode = Mode::Normal;
            self.cancel_pending();
            return;
        }
        let left = anchor.grapheme.min(cursor.grapheme);
        let right = anchor.grapheme.max(cursor.grapheme);
        let mut ranges = Vec::new();
        let mut pieces = Vec::new();
        for line in top..=bottom {
            let length = self.active_buffer().grapheme_count(line).unwrap_or(0);
            let start = Pos::new(line, left.min(length));
            let end = Pos::new(line, (right + 1).min(length));
            let range = TextRange::new(start, end);
            pieces.push(
                self.active_buffer()
                    .text_in_range(range)
                    .unwrap_or_default(),
            );
            ranges.push(range);
        }
        self.write_register(pieces.join("\n"), RegisterKind::Block);
        if operator != Operator::Yank {
            let result = (|| -> Result<(), BufferError> {
                self.active_buffer_mut().begin_transaction()?;
                for range in ranges.into_iter().rev() {
                    self.active_buffer_mut().delete(range)?;
                }
                if operator != Operator::Change {
                    self.active_buffer_mut().commit_transaction()?;
                }
                Ok(())
            })();
            if let Err(error) = result {
                if self.active_buffer().in_transaction() {
                    let _ = self.active_buffer_mut().rollback_transaction();
                }
                self.message(error.to_string());
            }
        }
        self.active_pane_mut().cursor = self.normal_clamp(Pos::new(top, left));
        self.active_pane_mut().anchor = None;
        self.pending_operator = None;
        if operator == Operator::Change {
            self.enter_insert(true);
        } else {
            self.mode = Mode::Normal;
        }
    }

    fn enter_insert(&mut self, transaction_already_open: bool) {
        if !transaction_already_open
            && !self.active_buffer().in_transaction()
            && let Err(error) = self.active_buffer_mut().begin_transaction()
        {
            self.message(error.to_string());
            return;
        }
        self.mode = Mode::Insert;
        self.pending_operator = None;
        self.awaiting = Awaiting::None;
        self.insert_recording.clear();
    }

    fn leave_insert(&mut self) {
        if self.active_buffer().in_transaction() {
            match self.active_buffer_mut().commit_transaction() {
                Ok(changed) => {
                    if changed && !self.insert_recording.is_empty() {
                        self.last_change = Some(LastChange::Insert(self.insert_recording.clone()));
                    }
                }
                Err(error) => self.message(error.to_string()),
            }
        }
        let cursor = self.active_pane().cursor;
        if cursor.grapheme > 0 {
            self.active_pane_mut().cursor.grapheme -= 1;
        }
        self.clamp_active_cursor();
        self.insert_recording.clear();
        self.mode = Mode::Normal;
    }

    fn move_insert_after(&mut self) {
        let cursor = self.active_pane().cursor;
        let count = self
            .active_buffer()
            .grapheme_count(cursor.line)
            .unwrap_or(0);
        if count > 0 {
            self.active_pane_mut().cursor.grapheme = (cursor.grapheme + 1).min(count);
        }
    }

    fn open_line_below(&mut self) {
        let line = self.active_pane().cursor.line;
        let end = self
            .active_buffer()
            .line_end(line)
            .unwrap_or(Pos::new(line, 0));
        let indent = self
            .active_buffer()
            .line(line)
            .unwrap_or("")
            .chars()
            .take_while(|ch| matches!(ch, ' ' | '\t'))
            .collect::<String>();
        if let Err(error) = self.active_buffer_mut().begin_transaction() {
            self.message(error.to_string());
            return;
        }
        match self.active_buffer_mut().insert(end, &format!("\n{indent}")) {
            Ok(pos) => {
                self.active_pane_mut().cursor = pos;
                self.enter_insert(true);
            }
            Err(error) => {
                let _ = self.active_buffer_mut().rollback_transaction();
                self.message(error.to_string());
            }
        }
    }

    fn open_line_above(&mut self) {
        let line = self.active_pane().cursor.line;
        let indent = self
            .active_buffer()
            .line(line)
            .unwrap_or("")
            .chars()
            .take_while(|ch| matches!(ch, ' ' | '\t'))
            .collect::<String>();
        if let Err(error) = self.active_buffer_mut().begin_transaction() {
            self.message(error.to_string());
            return;
        }
        match self
            .active_buffer_mut()
            .insert(Pos::new(line, 0), &format!("{indent}\n"))
        {
            Ok(_) => {
                self.active_pane_mut().cursor = Pos::new(line, indent.graphemes(true).count());
                self.enter_insert(true);
            }
            Err(error) => {
                let _ = self.active_buffer_mut().rollback_transaction();
                self.message(error.to_string());
            }
        }
    }

    fn delete_chars(&mut self, count: usize) {
        let start = self.active_pane().cursor;
        let mut end = start;
        for _ in 0..count {
            let Some(next) = self.active_buffer().next_pos(end).ok().flatten() else {
                break;
            };
            if next.line != start.line {
                break;
            }
            end = next;
        }
        if end == start {
            return;
        }
        let text = self
            .active_buffer()
            .text_in_range(TextRange::new(start, end))
            .unwrap_or_default();
        self.write_register(text, RegisterKind::Character);
        match self.active_buffer_mut().delete(TextRange::new(start, end)) {
            Ok(_) => {
                self.clamp_active_cursor();
                self.last_change = Some(LastChange::DeleteChars(count));
            }
            Err(error) => self.message(error.to_string()),
        }
    }

    fn replace_chars(&mut self, ch: char, count: usize) {
        let start = self.active_pane().cursor;
        let mut end = start;
        for _ in 0..count {
            let Some(next) = self.active_buffer().next_pos(end).ok().flatten() else {
                break;
            };
            if next.line != start.line {
                break;
            }
            end = next;
        }
        if end == start {
            return;
        }
        let replacement = ch
            .to_string()
            .repeat(end.grapheme.saturating_sub(start.grapheme));
        match self
            .active_buffer_mut()
            .replace(TextRange::new(start, end), &replacement)
        {
            Ok(_) => {
                self.set_cursor(start, false);
                self.last_change = Some(LastChange::Replace { ch, count });
            }
            Err(error) => self.message(error.to_string()),
        }
    }

    fn join_lines(&mut self, count: usize) {
        let start_line = self.active_pane().cursor.line;
        let result = (|| -> Result<(), BufferError> {
            self.active_buffer_mut().begin_transaction()?;
            for _ in 0..count.max(2) - 1 {
                if start_line + 1 >= self.active_buffer().line_count() {
                    break;
                }
                let end = self.active_buffer().line_end(start_line)?;
                let next = self.active_buffer().line(start_line + 1).unwrap_or("");
                let whitespace = next
                    .graphemes(true)
                    .take_while(|g| g.chars().all(char::is_whitespace))
                    .count();
                self.active_buffer_mut()
                    .delete(TextRange::new(end, Pos::new(start_line + 1, whitespace)))?;
                self.active_buffer_mut().insert(end, " ")?;
            }
            self.active_buffer_mut().commit_transaction()?;
            Ok(())
        })();
        match result {
            Ok(()) => {
                self.clamp_active_cursor();
                self.last_change = Some(LastChange::Join(count));
            }
            Err(error) => {
                if self.active_buffer().in_transaction() {
                    let _ = self.active_buffer_mut().rollback_transaction();
                }
                self.message(error.to_string());
            }
        }
    }

    fn write_register(&mut self, text: String, kind: RegisterKind) {
        let register = Register { text, kind };
        self.registers.insert('"', register.clone());
        if self.selected_register != '"' {
            self.registers.insert(self.selected_register, register);
        }
        self.selected_register = '"';
    }

    fn paste(&mut self, before: bool, count: usize) {
        let selected = std::mem::replace(&mut self.selected_register, '"');
        let Some(register) = self.registers.get(&selected).cloned() else {
            self.message(format!("Register {selected} is empty"));
            return;
        };
        if register.text.is_empty() && register.kind == RegisterKind::Character {
            self.message("Register is empty");
            return;
        }
        let cursor = self.active_pane().cursor;
        let result = (|| -> Result<Pos, BufferError> {
            self.active_buffer_mut().begin_transaction()?;
            let mut final_pos = cursor;
            for _ in 0..count {
                match register.kind {
                    RegisterKind::Character => {
                        let at = if before {
                            cursor
                        } else {
                            self.active_buffer()
                                .next_pos(cursor)?
                                .filter(|p| p.line == cursor.line)
                                .unwrap_or_else(|| {
                                    self.active_buffer().line_end(cursor.line).unwrap_or(cursor)
                                })
                        };
                        final_pos = self.active_buffer_mut().insert(at, &register.text)?;
                    }
                    RegisterKind::Line => {
                        let line = if before { cursor.line } else { cursor.line + 1 };
                        let at = if line < self.active_buffer().line_count() {
                            Pos::new(line, 0)
                        } else {
                            self.active_buffer()
                                .line_end(self.active_buffer().line_count() - 1)?
                        };
                        let text = if line < self.active_buffer().line_count() {
                            format!("{}\n", register.text.trim_end_matches(['\r', '\n']))
                        } else {
                            format!("\n{}", register.text.trim_end_matches(['\r', '\n']))
                        };
                        final_pos = self.active_buffer_mut().insert(at, &text)?;
                    }
                    RegisterKind::Block => {
                        for (offset, piece) in register.text.lines().enumerate() {
                            let line =
                                (cursor.line + offset).min(self.active_buffer().line_count() - 1);
                            let at = Pos::new(
                                line,
                                cursor
                                    .grapheme
                                    .min(self.active_buffer().grapheme_count(line).unwrap_or(0)),
                            );
                            final_pos = self.active_buffer_mut().insert(at, piece)?;
                        }
                    }
                }
            }
            self.active_buffer_mut().commit_transaction()?;
            Ok(final_pos)
        })();
        match result {
            Ok(pos) => {
                self.set_cursor(pos, false);
                self.last_change = Some(LastChange::Paste { before });
            }
            Err(error) => {
                if self.active_buffer().in_transaction() {
                    let _ = self.active_buffer_mut().rollback_transaction();
                }
                self.message(error.to_string());
            }
        }
    }

    fn undo(&mut self) {
        match self.active_buffer_mut().undo() {
            Ok(true) => self.clamp_all_cursors_for_active_buffer(),
            Ok(false) => self.message("Already at oldest change"),
            Err(error) => self.message(error.to_string()),
        }
    }

    fn redo(&mut self) {
        match self.active_buffer_mut().redo() {
            Ok(true) => self.clamp_all_cursors_for_active_buffer(),
            Ok(false) => self.message("Already at newest change"),
            Err(error) => self.message(error.to_string()),
        }
    }

    fn clamp_all_cursors_for_active_buffer(&mut self) {
        let buffer_index = self.active_pane().buffer;
        let positions: Vec<(PaneId, Pos)> = self
            .panes
            .iter()
            .filter(|p| p.buffer == buffer_index)
            .map(|p| (p.id, self.buffers[buffer_index].buffer.clamp_pos(p.cursor)))
            .collect();
        for (id, pos) in positions {
            if let Some(pane) = self.panes.iter_mut().find(|p| p.id == id) {
                pane.cursor = pos;
            }
        }
        self.clamp_active_cursor();
    }

    fn repeat_last_change(&mut self, count: usize) {
        let Some(change) = self.last_change.clone() else {
            self.message("No previous change");
            return;
        };
        match change {
            LastChange::Insert(text) => {
                let cursor = self.active_pane().cursor;
                if self.active_buffer_mut().begin_transaction().is_ok() {
                    let mut position = cursor;
                    for _ in 0..count {
                        if let Ok(next) = self.active_buffer_mut().insert(position, &text) {
                            position = next;
                        }
                    }
                    let _ = self.active_buffer_mut().commit_transaction();
                    self.set_cursor(position, false);
                }
            }
            LastChange::DeleteChars(original) => self.delete_chars(original.saturating_mul(count)),
            LastChange::Paste { before } => self.paste(before, count),
            LastChange::Replace {
                ch,
                count: original,
            } => self.replace_chars(ch, original.saturating_mul(count)),
            LastChange::Join(original) => self.join_lines(original.saturating_mul(count)),
            LastChange::DeleteMotion(motion, original) => {
                self.begin_operator(Operator::Delete, 1, false);
                self.apply_operator_motion(motion, original.saturating_mul(count));
            }
            LastChange::DeleteLines(original) => {
                self.begin_operator(Operator::Delete, 1, false);
                self.apply_line_operator(original.saturating_mul(count));
            }
        }
    }

    fn find_target(&self, spec: FindSpec, count: usize, skip: Option<Pos>) -> Option<(Pos, Pos)> {
        let cursor = self.active_pane().cursor;
        let line = self.active_buffer().line(cursor.line)?;
        let graphemes: Vec<&str> = line.graphemes(true).collect();
        let mut index = cursor.grapheme;
        for occurrence in 0..count {
            if spec.backward {
                index = (0..index).rev().find(|at| {
                    graphemes[*at].contains(spec.character)
                        && !(occurrence == 0 && skip == Some(Pos::new(cursor.line, *at)))
                })?;
            } else {
                index = (index + 1..graphemes.len()).find(|at| {
                    graphemes[*at].contains(spec.character)
                        && !(occurrence == 0 && skip == Some(Pos::new(cursor.line, *at)))
                })?;
            }
        }
        let matched = Pos::new(cursor.line, index);
        if spec.till {
            index = if spec.backward {
                (index + 1).min(graphemes.len().saturating_sub(1))
            } else {
                index.saturating_sub(1)
            };
        }
        Some((Pos::new(cursor.line, index), matched))
    }

    fn repeat_find(&mut self, reverse: bool, count: usize) {
        let Some(mut spec) = self.last_find else {
            self.message("No previous f/F/t/T search");
            return;
        };
        if reverse {
            spec.backward = !spec.backward;
        }
        if let Some((target, matched)) = self.find_target(spec, count, self.last_find_match) {
            self.last_find_match = Some(matched);
            self.set_cursor(target, false);
        } else {
            self.message(format!("Character not found: {}", spec.character));
        }
    }

    fn text_object(&self, object: char, around: bool) -> Option<TextRange> {
        let cursor = self.active_pane().cursor;
        match object {
            'w' | 'W' => self.word_object(cursor, around, object == 'W'),
            '"' | '\'' | '`' => self.quote_object(cursor, object, around),
            '(' | ')' | 'b' => self.pair_object(cursor, '(', ')', around),
            '[' | ']' => self.pair_object(cursor, '[', ']', around),
            '{' | '}' | 'B' => self.pair_object(cursor, '{', '}', around),
            _ => None,
        }
    }

    fn word_object(&self, cursor: Pos, around: bool, big: bool) -> Option<TextRange> {
        let line = self.active_buffer().line(cursor.line)?;
        let graphemes: Vec<&str> = line.graphemes(true).collect();
        if graphemes.is_empty() {
            return None;
        }
        let mut at = cursor.grapheme.min(graphemes.len() - 1);
        let classify = |g: &str| {
            if g.chars().all(char::is_whitespace) {
                0
            } else if big || is_word(g) {
                1
            } else {
                2
            }
        };
        if classify(graphemes[at]) == 0 {
            at = (at..graphemes.len()).find(|index| classify(graphemes[*index]) != 0)?;
        }
        let class = classify(graphemes[at]);
        let mut start = at;
        while start > 0 && classify(graphemes[start - 1]) == class {
            start -= 1;
        }
        let mut end = at + 1;
        while end < graphemes.len() && classify(graphemes[end]) == class {
            end += 1;
        }
        if around {
            let original_end = end;
            while end < graphemes.len() && classify(graphemes[end]) == 0 {
                end += 1;
            }
            if end == original_end {
                while start > 0 && classify(graphemes[start - 1]) == 0 {
                    start -= 1;
                }
            }
        }
        Some(TextRange::new(
            Pos::new(cursor.line, start),
            Pos::new(cursor.line, end),
        ))
    }

    fn quote_object(&self, cursor: Pos, quote: char, around: bool) -> Option<TextRange> {
        let line = self.active_buffer().line(cursor.line)?;
        let graphemes: Vec<&str> = line.graphemes(true).collect();
        if graphemes.is_empty() {
            return None;
        }
        let left = (0..=cursor.grapheme.min(graphemes.len().saturating_sub(1)))
            .rev()
            .find(|index| graphemes[*index] == quote.to_string())?;
        let right = (cursor.grapheme.max(left + 1)..graphemes.len())
            .find(|index| graphemes[*index] == quote.to_string())?;
        Some(if around {
            TextRange::new(
                Pos::new(cursor.line, left),
                Pos::new(cursor.line, right + 1),
            )
        } else {
            TextRange::new(
                Pos::new(cursor.line, left + 1),
                Pos::new(cursor.line, right),
            )
        })
    }

    fn pair_object(&self, cursor: Pos, open: char, close: char, around: bool) -> Option<TextRange> {
        let buffer = self.active_buffer();
        let text = buffer.text();
        let cursor_byte = buffer.pos_to_byte(cursor).ok()?.0;
        let bytes = text.as_bytes();
        if bytes.is_empty() {
            return None;
        }
        let mut depth = 0_i32;
        let mut opening = None;
        for index in (0..=cursor_byte.min(bytes.len().saturating_sub(1))).rev() {
            if bytes[index] == close as u8 {
                depth += 1;
            } else if bytes[index] == open as u8 {
                if depth == 0 {
                    opening = Some(index);
                    break;
                }
                depth -= 1;
            }
        }
        let opening = opening?;
        depth = 0;
        let mut closing = None;
        for (index, value) in bytes.iter().enumerate().skip(opening) {
            if *value == open as u8 {
                depth += 1;
            } else if *value == close as u8 {
                depth -= 1;
                if depth == 0 {
                    closing = Some(index);
                    break;
                }
            }
        }
        let closing = closing?;
        let start_byte = if around {
            opening
        } else {
            opening + open.len_utf8()
        };
        let end_byte = if around {
            closing + close.len_utf8()
        } else {
            closing
        };
        Some(TextRange::new(
            buffer
                .byte_to_pos(crate::buffer::ByteOffset(start_byte))
                .ok()?,
            buffer
                .byte_to_pos(crate::buffer::ByteOffset(end_byte))
                .ok()?,
        ))
    }

    fn enter_search(&mut self, backward: bool) {
        self.mode = Mode::Search { backward };
        self.prompt.clear();
    }

    fn run_search(&mut self, query: &str, backward: bool, remember: bool) {
        if query.is_empty() {
            return;
        }
        let regex = match regex::Regex::new(query) {
            Ok(regex) => regex,
            Err(error) => {
                self.message(format!("Invalid search: {error}"));
                return;
            }
        };
        let cursor = self.active_pane().cursor;
        let lines = self.active_buffer().line_count();
        for offset in 0..lines {
            let line_number = if backward {
                (cursor.line + lines - offset) % lines
            } else {
                (cursor.line + offset) % lines
            };
            let line = self.active_buffer().line(line_number).unwrap_or("");
            let cursor_byte = if line_number == cursor.line {
                byte_for_grapheme(line, cursor.grapheme)
            } else if backward {
                line.len()
            } else {
                0
            };
            let found = if backward {
                regex
                    .find_iter(line)
                    .filter(|item| offset > 0 || item.start() < cursor_byte)
                    .last()
            } else {
                regex
                    .find_iter(line)
                    .find(|item| offset > 0 || item.start() > cursor_byte)
            };
            if let Some(found) = found {
                let grapheme = line[..found.start()].graphemes(true).count();
                self.set_cursor(Pos::new(line_number, grapheme), false);
                if remember {
                    self.last_search = Some((query.to_owned(), backward));
                }
                return;
            }
        }
        // Search the wrapped portion of the starting line after every other
        // line. Without this second pass a one-line buffer never wraps, and a
        // multi-line search skips matches across the cursor on its first line.
        let line = self.active_buffer().line(cursor.line).unwrap_or("");
        let cursor_byte = byte_for_grapheme(line, cursor.grapheme);
        let wrapped = if backward {
            regex
                .find_iter(line)
                .filter(|item| item.start() >= cursor_byte)
                .last()
        } else {
            regex
                .find_iter(line)
                .find(|item| item.start() <= cursor_byte)
        };
        if let Some(found) = wrapped {
            let grapheme = line[..found.start()].graphemes(true).count();
            self.set_cursor(Pos::new(cursor.line, grapheme), false);
            if remember {
                self.last_search = Some((query.to_owned(), backward));
            }
            return;
        }
        self.message(format!("Pattern not found: {query}"));
        if remember {
            self.last_search = Some((query.to_owned(), backward));
        }
    }

    fn repeat_search(&mut self, reverse: bool, count: usize) {
        let Some((query, mut backward)) = self.last_search.clone() else {
            self.message("No previous search");
            return;
        };
        if reverse {
            backward = !backward;
        }
        for _ in 0..count {
            self.run_search(&query, backward, false);
        }
    }

    fn search_word(&mut self, backward: bool) {
        let Some(word) = self.word_under_cursor() else {
            self.message("No word under cursor");
            return;
        };
        let query = format!(r"\b{}\b", regex::escape(&word));
        self.run_search(&query, backward, true);
    }

    fn word_under_cursor(&self) -> Option<String> {
        let cursor = self.active_pane().cursor;
        let range = self.word_object(cursor, false, false)?;
        let text = self.active_buffer().text_in_range(range).ok()?;
        is_word(&text).then_some(text)
    }

    fn navigate_diagnostic(&mut self, backward: bool, count: usize) {
        if self.diagnostics.is_empty() {
            self.message("No diagnostics");
            return;
        }
        let path = self.active_buffer().path().map(Path::to_owned);
        let revision = self.active_buffer().revision();
        let cursor = self.active_pane().cursor;
        let mut candidates: Vec<_> = self
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.path == path && diagnostic.version == revision)
            .cloned()
            .collect();
        candidates.sort_by_key(|diagnostic| (diagnostic.line, diagnostic.column));
        if backward {
            candidates.reverse();
        }
        let mut selected = None;
        for _ in 0..count {
            selected = candidates.iter().find(|diagnostic| {
                if backward {
                    diagnostic.line
                        < selected
                            .as_ref()
                            .map_or(cursor.line + 1, |d: &&Diagnostic| d.line)
                } else {
                    diagnostic.line
                        > selected
                            .as_ref()
                            .map_or(cursor.line.saturating_sub(1), |d: &&Diagnostic| d.line)
                }
            });
        }
        if let Some(diagnostic) = selected.cloned().or_else(|| candidates.first().cloned()) {
            self.set_cursor(Pos::new(diagnostic.line, diagnostic.column), false);
            self.message(diagnostic.message);
        }
    }

    fn show_line_diagnostic(&mut self) {
        let path = self.active_buffer().path().map(Path::to_owned);
        let revision = self.active_buffer().revision();
        let line = self.active_pane().cursor.line;
        let messages = self
            .diagnostics
            .iter()
            .filter(|diagnostic| {
                diagnostic.path == path && diagnostic.version == revision && diagnostic.line == line
            })
            .map(|diagnostic| diagnostic.message.as_str())
            .collect::<Vec<_>>()
            .join(" • ");
        if messages.is_empty() {
            self.message("No diagnostics on this line");
        } else {
            self.message(messages);
        }
    }

    fn stop_macro_recording(&mut self) {
        if let Some((register, mut keys)) = self.macro_recording.take() {
            if matches!(
                keys.last(),
                Some(Key {
                    code: KeyCode::Char('q'),
                    ..
                })
            ) {
                keys.pop();
            }
            self.macros.insert(register, keys);
            self.message(format!("Recorded macro @{register}"));
        }
    }

    fn play_macro(&mut self, register: char, count: usize) {
        if self.macro_depth >= 16 {
            self.message("Macro recursion limit reached");
            return;
        }
        let Some(keys) = self.macros.get(&register).cloned() else {
            self.message(format!("Macro @{register} is empty"));
            return;
        };
        if keys.len().saturating_mul(count) > 10_000 {
            self.message("Macro work limit exceeded");
            return;
        }
        self.last_macro = Some(register);
        self.macro_depth += 1;
        for _ in 0..count {
            for key in &keys {
                self.handle_key(*key);
                if self.should_quit {
                    break;
                }
            }
        }
        self.macro_depth -= 1;
    }

    pub fn execute_ex(&mut self, input: &str) {
        let input = input.trim().strip_prefix(':').unwrap_or(input.trim());
        let (command, argument) = input
            .split_once(char::is_whitespace)
            .map_or((input, ""), |(a, b)| (a, b.trim()));
        let forced = command.ends_with('!');
        let command = command.trim_end_matches('!');
        match command {
            "w" | "write" => {
                self.write_buffer(argument, forced);
            }
            "q" | "quit" => self.quit_pane(forced),
            "qa" | "qall" => self.quit_all(forced),
            "wq" | "x" => {
                if self.write_buffer(argument, forced) {
                    self.quit_pane(forced);
                }
            }
            "e" | "edit" => {
                if argument.is_empty() {
                    match if forced {
                        self.active_buffer_mut().reload_force()
                    } else {
                        self.active_buffer_mut().reload()
                    } {
                        Ok(true) => {
                            self.clamp_all_cursors_for_active_buffer();
                            self.message("Reloaded");
                        }
                        Ok(false) => self.message("File unchanged"),
                        Err(error) => self.message(error.to_string()),
                    }
                } else if let Err(error) = self.open_path(self.explorer.root.join(argument)) {
                    self.message(error.to_string());
                }
            }
            "saveas" => {
                if argument.is_empty() {
                    self.message("Usage: :saveas PATH");
                } else {
                    let path = self.explorer.root.join(argument);
                    let result = if forced {
                        self.active_buffer_mut().save_as_force(&path)
                    } else {
                        self.active_buffer_mut().save_as(&path)
                    };
                    match result {
                        Ok(()) => {
                            let index = self.active_pane().buffer;
                            self.buffers[index].display_name = argument.into();
                            self.request = EditorRequest::DocumentSaved(path.clone());
                            self.message(format!("Wrote {}", path.display()));
                        }
                        Err(error) => self.message(error.to_string()),
                    }
                }
            }
            "b" | "buffer" => {
                if argument.is_empty() {
                    self.open_picker(PickerKind::Buffers);
                } else if let Ok(index) = argument.parse::<usize>() {
                    self.switch_buffer(index.saturating_sub(1));
                } else if let Some(index) = self
                    .buffers
                    .iter()
                    .position(|slot| slot.display_name.contains(argument))
                {
                    self.switch_buffer(index);
                } else {
                    self.message(format!("Buffer not found: {argument}"));
                }
            }
            "bn" | "bnext" => self.cycle_buffer(1),
            "bp" | "bprevious" => self.cycle_buffer(-1),
            "bd" | "bdelete" => self.close_buffer(forced),
            "split" | "sp" => {
                self.split(Orientation::Horizontal);
                if !argument.is_empty()
                    && let Err(error) = self.open_path(self.explorer.root.join(argument))
                {
                    self.message(error.to_string());
                }
            }
            "vsplit" | "vs" => {
                self.split(Orientation::Vertical);
                if !argument.is_empty()
                    && let Err(error) = self.open_path(self.explorer.root.join(argument))
                {
                    self.message(error.to_string());
                }
            }
            "only" => self.only_pane(),
            "earlier" => {
                let count = parse_ex_count(argument);
                for _ in 0..count {
                    self.undo();
                }
            }
            "later" => {
                let count = parse_ex_count(argument);
                for _ in 0..count {
                    self.redo();
                }
            }
            "messages" => self.open_picker(PickerKind::Messages),
            "terminal" | "term" => self.execute_command(CommandId::TerminalToggle),
            "rename" => {
                if argument.is_empty() {
                    self.message("Usage: :rename NEW_NAME");
                } else {
                    self.request = EditorRequest::RustAnalyzerWithArgument(
                        CommandId::Rename,
                        argument.to_owned(),
                    );
                }
            }
            "checkhealth" => self.request = EditorRequest::CheckHealth,
            "rarestart" | "lsprestart" => {
                self.request = EditorRequest::RustAnalyzer(CommandId::RustAnalyzerRestart);
                self.message("Restarting rust-analyzer");
            }
            "config" => {
                let sources = if self.config.sources.is_empty() {
                    "built-in defaults".into()
                } else {
                    self.config
                        .sources
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                self.message(format!(
                    "Config schema {} from {sources}",
                    self.config.schema_version
                ));
            }
            "reloadconfig" => match Config::load(Some(&self.explorer.root), false) {
                Ok(config) => {
                    if let Some(background) = parse_hex_color(&config.ui.theme.background) {
                        self.terminal.set_background(background);
                    }
                    self.config = config;
                    self.message("Configuration reloaded");
                }
                Err(error) => self.message(format!(
                    "Config reload failed; keeping current config: {error}"
                )),
            },
            "" => {}
            _ => self.message(format!("Unknown command: :{command}")),
        }
    }

    fn write_buffer(&mut self, argument: &str, forced: bool) -> bool {
        let result = if argument.is_empty() {
            if forced {
                self.active_buffer_mut().save_force()
            } else {
                self.active_buffer_mut().save()
            }
        } else {
            let path = self.explorer.root.join(argument);
            if forced {
                self.active_buffer_mut().save_as_force(path)
            } else {
                self.active_buffer_mut().save_as(path)
            }
        };
        match result {
            Ok(()) => {
                let path = self.active_buffer().path().map(Path::to_owned);
                let display = path
                    .as_deref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "buffer".into());
                if let Some(path) = path {
                    self.request = EditorRequest::DocumentSaved(path);
                }
                self.message(format!("Wrote {display}"));
                true
            }
            Err(error) => {
                self.message(error.to_string());
                false
            }
        }
    }

    fn quit_pane(&mut self, forced: bool) {
        let buffer = self.active_pane().buffer;
        let shown_elsewhere = self
            .panes
            .iter()
            .any(|pane| pane.id != self.active_pane && pane.buffer == buffer);
        if self.active_buffer().is_dirty() && !shown_elsewhere {
            if !forced {
                self.message("Buffer has unsaved changes; use :w to save or :q! to discard it");
                return;
            }
            // Discard only an abandoned buffer. A duplicate pane must retain
            // its text, cursor, and undo history even when this quit is forced.
            self.close_buffer(true);
        }
        if self.panes.len() > 1 {
            self.close_pane();
            return;
        }
        // A forced window quit does not authorize losing other dirty buffers.
        // Surface the first one so it can be saved or explicitly discarded.
        if let Some(index) = self.buffers.iter().position(|slot| slot.buffer.is_dirty()) {
            self.switch_buffer(index);
            self.message("Another buffer has unsaved changes; use :w to save, :q! to discard it, or :qa! to discard all");
            return;
        }
        self.should_quit = true;
    }

    fn quit_all(&mut self, forced: bool) {
        if !forced && self.buffers.iter().any(|slot| slot.buffer.is_dirty()) {
            let dirty = self
                .buffers
                .iter()
                .filter(|slot| slot.buffer.is_dirty())
                .count();
            self.message(format!(
                "Unsaved changes in {dirty} buffer{}; use :qa! to discard and quit",
                if dirty == 1 { "" } else { "s" }
            ));
            return;
        }
        self.should_quit = true;
    }

    fn open_picker(&mut self, kind: PickerKind) {
        let return_mode = self.mode.clone();
        self.picker = Some(Picker {
            kind,
            query: String::new(),
            items: Vec::new(),
            all_items: Vec::new(),
            selected: 0,
            return_mode,
            current_buffer_diagnostics: false,
        });
        self.refresh_picker();
    }

    fn open_buffer_diagnostics(&mut self) {
        self.open_picker(PickerKind::Diagnostics);
        if let Some(picker) = self.picker.as_mut() {
            picker.current_buffer_diagnostics = true;
        }
        self.refresh_picker();
    }

    fn refresh_picker(&mut self) {
        let Some(picker) = self.picker.as_ref() else {
            return;
        };
        let kind = picker.kind;
        let query = picker.query.clone();
        let current_buffer_diagnostics = picker.current_buffer_diagnostics;
        let limit = self.config.limits.search_results;
        let items = match kind {
            PickerKind::Files => {
                self.file_finder_active = true;
                self.file_finder
                    .get_or_insert_with(|| {
                        crate::project::FileFinder::new(self.explorer.root.clone())
                    })
                    .request(query, limit);
                Vec::new()
            }
            PickerKind::Buffers => self
                .buffers
                .iter()
                .enumerate()
                .filter(|(_, slot)| fuzzy_contains(&query, &slot.display_name))
                .map(|(index, slot)| PickerItem {
                    label: format!(
                        "{}{}",
                        if slot.buffer.is_dirty() { "+ " } else { "  " },
                        slot.display_name
                    ),
                    detail: format!("buffer:{index}"),
                    path: slot.buffer.path().map(Path::to_owned),
                    line: None,
                    insert_text: None,
                })
                .collect(),
            PickerKind::Recent => self
                .recent_files
                .iter()
                .filter(|path| fuzzy_contains(&query, &path.to_string_lossy()))
                .map(|path| PickerItem {
                    label: path.display().to_string(),
                    detail: "recent".into(),
                    path: Some(path.clone()),
                    line: None,
                    insert_text: None,
                })
                .collect(),
            PickerKind::Messages => self
                .messages
                .iter()
                .rev()
                .filter(|message| fuzzy_contains(&query, message))
                .map(|message| PickerItem {
                    label: message.clone(),
                    detail: "message".into(),
                    path: None,
                    line: None,
                    insert_text: None,
                })
                .collect(),
            PickerKind::Diagnostics => {
                let path = self.active_buffer().path().map(Path::to_owned);
                let revision = self.active_buffer().revision();
                self.diagnostics
                    .iter()
                    .filter(|diagnostic| {
                        (!current_buffer_diagnostics
                            || diagnostic.path == path && diagnostic.version == revision)
                            && fuzzy_contains(&query, &diagnostic.message)
                    })
                    .map(|diagnostic| PickerItem {
                        label: diagnostic.message.clone(),
                        detail: format!(
                            "{}:{}",
                            diagnostic
                                .path
                                .as_deref()
                                .map(|p| p.display().to_string())
                                .unwrap_or_default(),
                            diagnostic.line + 1
                        ),
                        path: diagnostic.path.clone(),
                        line: Some(diagnostic.line),
                        insert_text: None,
                    })
                    .collect()
            }
            PickerKind::Grep => self
                .project_search_results
                .iter()
                .filter(|item| fuzzy_contains(&query, &format!("{} {}", item.label, item.detail)))
                .cloned()
                .collect(),
            PickerKind::Symbols => picker
                .all_items
                .iter()
                .filter(|item| fuzzy_contains(&query, &format!("{} {}", item.label, item.detail)))
                .cloned()
                .collect(),
        };
        if let Some(picker) = self.picker.as_mut() {
            picker.items = items;
            picker.selected = picker.selected.min(picker.items.len().saturating_sub(1));
        }
    }

    fn picker_move(&mut self, delta: isize) {
        let Some(picker) = self.picker.as_mut() else {
            return;
        };
        if picker.items.is_empty() {
            return;
        }
        picker.selected =
            (picker.selected as isize + delta).rem_euclid(picker.items.len() as isize) as usize;
    }

    fn accept_picker_split(&mut self, orientation: Orientation) {
        self.split(orientation);
        self.accept_picker(false);
    }

    fn accept_picker(&mut self, _alternate: bool) {
        let Some(picker) = self.picker.take() else {
            return;
        };
        self.mode = picker.return_mode.clone();
        let Some(item) = picker.items.get(picker.selected).cloned() else {
            return;
        };
        match picker.kind {
            PickerKind::Buffers => {
                if let Some(index) = item
                    .detail
                    .strip_prefix("buffer:")
                    .and_then(|value| value.parse().ok())
                {
                    self.switch_buffer(index);
                }
            }
            PickerKind::Files | PickerKind::Recent | PickerKind::Grep | PickerKind::Diagnostics => {
                if let Some(path) = item.path
                    && self.open_path(path).is_ok()
                    && let Some(line) = item.line
                {
                    self.set_cursor(Pos::new(line, 0), false);
                }
            }
            PickerKind::Symbols => {
                if let Some(path) = item.path {
                    if self.open_path(path).is_ok()
                        && let Some(line) = item.line
                    {
                        self.set_cursor(Pos::new(line, 0), false);
                    }
                } else if let Some(text) = item.insert_text {
                    let cursor = self.active_pane().cursor;
                    let opened = !self.active_buffer().in_transaction();
                    if opened {
                        let _ = self.active_buffer_mut().begin_transaction();
                    }
                    match self.active_buffer_mut().insert(cursor, &text) {
                        Ok(position) => {
                            self.active_pane_mut().cursor = position;
                            if opened {
                                let _ = self.active_buffer_mut().commit_transaction();
                            }
                        }
                        Err(error) => {
                            if opened && self.active_buffer().in_transaction() {
                                let _ = self.active_buffer_mut().rollback_transaction();
                            }
                            self.message(error.to_string());
                        }
                    }
                }
            }
            PickerKind::Messages => {}
        }
    }

    fn switch_buffer(&mut self, index: usize) {
        if index >= self.buffers.len() {
            self.message(format!("Invalid buffer {}", index + 1));
            return;
        }
        self.active_pane_mut().buffer = index;
        self.clamp_active_cursor();
    }

    fn cycle_buffer(&mut self, delta: isize) {
        if self.buffers.is_empty() {
            return;
        }
        let current = self.active_pane().buffer;
        let next = (current as isize + delta).rem_euclid(self.buffers.len() as isize) as usize;
        self.switch_buffer(next);
    }

    fn close_buffer(&mut self, forced: bool) {
        let index = self.active_pane().buffer;
        if self.buffers[index].buffer.is_dirty() && !forced {
            self.message("Buffer has unsaved changes; use :bd! to discard it");
            return;
        }
        if self.buffers.len() == 1 {
            self.buffers[0] = BufferSlot {
                buffer: Buffer::new(),
                display_name: "[scratch]".into(),
                large_file: false,
            };
            for pane in &mut self.panes {
                pane.buffer = 0;
                pane.cursor = Pos::ZERO;
            }
            return;
        }
        self.buffers.remove(index);
        for pane in &mut self.panes {
            if pane.buffer == index {
                pane.buffer = index.min(self.buffers.len() - 1);
                pane.cursor = Pos::ZERO;
            } else if pane.buffer > index {
                pane.buffer -= 1;
            }
        }
    }

    fn explorer_move(&mut self, delta: isize) {
        if self.explorer.files.is_empty() {
            return;
        }
        self.explorer.selected = (self.explorer.selected as isize + delta)
            .rem_euclid(self.explorer.files.len() as isize)
            as usize;
    }

    fn explorer_open_selected(&mut self) {
        let Some(path) = self.explorer.files.get(self.explorer.selected).cloned() else {
            return;
        };
        if let Err(error) = self.open_path(path) {
            self.message(error.to_string());
        } else {
            self.focus = Focus::Editor;
        }
    }

    fn reveal_active_file(&mut self) {
        let Some(path) = self.active_buffer().path() else {
            return;
        };
        if let Some(index) = self
            .explorer
            .files
            .iter()
            .position(|candidate| candidate == path)
        {
            self.explorer.selected = index;
        }
    }
}

/// Stream word classes with a whitespace boundary at each logical line end.
/// Each visited line is segmented once, including when a motion crosses it.
fn word_graphemes(buffer: &Buffer, from: Pos) -> impl Iterator<Item = (Pos, u8)> + '_ {
    (from.line..buffer.line_count()).flat_map(move |line_number| {
        let skip = if line_number == from.line {
            from.grapheme
        } else {
            0
        };
        buffer
            .line(line_number)
            .unwrap_or("")
            .graphemes(true)
            .map(Some)
            .chain(std::iter::once(None))
            .enumerate()
            .skip(skip)
            .map(move |(column, grapheme)| {
                (Pos::new(line_number, column), grapheme_class(grapheme))
            })
    })
}

fn grapheme_at(buffer: &Buffer, pos: Pos) -> Option<&str> {
    buffer.line(pos.line)?.graphemes(true).nth(pos.grapheme)
}

fn grapheme_class(grapheme: Option<&str>) -> u8 {
    match grapheme {
        None => 0,
        Some(text) if text.chars().all(char::is_whitespace) => 0,
        Some(text) if is_word(text) => 1,
        Some(_) => 2,
    }
}

fn is_word(text: &str) -> bool {
    !text.is_empty() && text.chars().all(|ch| ch.is_alphanumeric() || ch == '_')
}

fn first_nonblank(line: &str) -> usize {
    line.graphemes(true)
        .take_while(|g| g.chars().all(char::is_whitespace))
        .count()
}

fn byte_for_grapheme(line: &str, grapheme: usize) -> usize {
    line.grapheme_indices(true)
        .nth(grapheme)
        .map(|(byte, _)| byte)
        .unwrap_or(line.len())
}

fn fuzzy_contains(query: &str, candidate: &str) -> bool {
    crate::project::fuzzy_score(query, candidate).is_some()
}

fn parse_ex_count(argument: &str) -> usize {
    argument
        .split_whitespace()
        .next()
        .and_then(|word| {
            word.trim_end_matches(|ch: char| ch.is_ascii_alphabetic())
                .parse()
                .ok()
        })
        .unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn editor(text: &str) -> Editor {
        let mut editor = Editor::new(Config::default(), PathBuf::from("/tmp"));
        editor.buffers[0] = BufferSlot {
            buffer: Buffer::from_text(text),
            display_name: "test.rs".into(),
            large_file: false,
        };
        editor
    }

    fn keys(editor: &mut Editor, input: &str) {
        for ch in input.chars() {
            editor.handle_key(Key::char(ch));
        }
    }

    #[test]
    fn insert_session_is_one_undo_step() {
        let mut editor = editor("");
        keys(&mut editor, "ihello");
        editor.handle_key(Key::plain(KeyCode::Esc));
        assert_eq!(editor.active_buffer().text(), "hello");
        keys(&mut editor, "u");
        assert_eq!(editor.active_buffer().text(), "");
    }

    #[test]
    fn counts_compose_for_delete_word() {
        for sequence in ["2dw", "d2w"] {
            let mut editor = editor("one two three");
            keys(&mut editor, sequence);
            assert_eq!(editor.active_buffer().text(), "three");
        }
    }

    #[test]
    fn word_motions_cross_unicode_runs_and_empty_lines() {
        let mut editor = editor("ábc  👩‍💻!\n\n  next word");
        for expected in [Pos::new(0, 5), Pos::new(2, 2), Pos::new(2, 7)] {
            keys(&mut editor, "w");
            assert_eq!(editor.active_pane().cursor, expected);
        }
        for expected in [Pos::new(2, 2), Pos::new(0, 5), Pos::ZERO] {
            keys(&mut editor, "b");
            assert_eq!(editor.active_pane().cursor, expected);
        }
        for expected in [
            Pos::new(0, 2),
            Pos::new(0, 6),
            Pos::new(2, 5),
            Pos::new(2, 10),
        ] {
            keys(&mut editor, "e");
            assert_eq!(editor.active_pane().cursor, expected);
        }
    }

    #[test]
    fn word_operators_handle_long_words_and_final_graphemes() {
        let word = "a\u{301}".repeat(4_000);
        let mut editor = editor(&format!("{word} tail"));
        keys(&mut editor, "dw");
        assert_eq!(editor.active_buffer().text(), "tail");
        keys(&mut editor, "u");
        keys(&mut editor, "e");
        assert_eq!(editor.active_pane().cursor, Pos::new(0, 3_999));
        keys(&mut editor, "b");
        assert_eq!(editor.active_pane().cursor, Pos::ZERO);
    }

    #[test]
    fn change_inner_word_enters_insert() {
        let mut editor = editor("alpha beta");
        keys(&mut editor, "wciwX");
        editor.handle_key(Key::plain(KeyCode::Esc));
        assert_eq!(editor.active_buffer().text(), "alpha X");
        keys(&mut editor, "u");
        assert_eq!(editor.active_buffer().text(), "alpha beta");
    }

    #[test]
    fn visual_delete_uses_register() {
        let mut editor = editor("abcd");
        keys(&mut editor, "vld");
        assert_eq!(editor.active_buffer().text(), "cd");
        keys(&mut editor, "P");
        assert_eq!(editor.active_buffer().text(), "abcd");
    }

    #[test]
    fn unknown_leader_sequence_does_not_edit() {
        let mut editor = editor("safe");
        keys(&mut editor, " z");
        assert_eq!(editor.active_buffer().text(), "safe");
        assert!(editor.current_message().unwrap().contains("Unknown leader"));
    }

    #[test]
    fn leader_terminal_toggle_routes_input_and_preserves_the_session_when_unfocused() {
        let mut editor = editor("safe");
        keys(&mut editor, " t");
        assert!(editor.terminal.visible);
        assert_eq!(editor.focus, Focus::Terminal);
        assert!(matches!(
            editor.take_request(),
            EditorRequest::TerminalToggle(true)
        ));

        editor.handle_key(Key::char('a'));
        assert!(matches!(
            editor.take_request(),
            EditorRequest::TerminalInput(bytes) if bytes == b"a"
        ));
        editor.handle_paste("hello\n");
        assert!(matches!(
            editor.take_request(),
            EditorRequest::TerminalInput(bytes) if bytes == b"hello\n"
        ));

        editor.handle_key(Key::ctrl('\\'));
        assert_eq!(editor.focus, Focus::Editor);
        assert!(editor.terminal.visible);

        editor.focus = Focus::Terminal;
        editor.handle_key(Key::ctrl('4'));
        assert_eq!(editor.focus, Focus::Editor);

        keys(&mut editor, " t");
        assert!(!editor.terminal.visible);
        assert!(matches!(
            editor.take_request(),
            EditorRequest::TerminalToggle(false)
        ));
    }

    #[test]
    fn empty_delimited_text_objects_report_instead_of_panicking() {
        for sequence in ["di\"", "di(", "di[", "di{"] {
            let mut editor = editor("");
            keys(&mut editor, sequence);
            assert_eq!(editor.active_buffer().text(), "");
            assert_eq!(editor.mode, Mode::Normal);
            assert!(
                editor
                    .current_message()
                    .unwrap()
                    .contains("Text object not found")
            );
        }
    }

    #[test]
    fn visual_yank_returns_to_normal_mode() {
        let mut editor = editor("abcd");
        keys(&mut editor, "vly");
        assert_eq!(editor.active_buffer().text(), "abcd");
        assert_eq!(editor.mode, Mode::Normal);
        assert_eq!(editor.active_pane().anchor, None);
    }

    #[test]
    fn visual_block_line_transform_does_not_delete_the_block() {
        let mut editor = editor("ab\ncd");
        editor.handle_key(Key::ctrl('v'));
        keys(&mut editor, "lj>");
        assert_eq!(editor.active_buffer().text(), "    ab\n    cd");
        assert_eq!(editor.mode, Mode::Normal);
        keys(&mut editor, "u");
        assert_eq!(editor.active_buffer().text(), "ab\ncd");
    }

    #[test]
    fn delete_word_at_eof_includes_the_final_grapheme() {
        let mut editor = editor("one");
        keys(&mut editor, "dw");
        assert_eq!(editor.active_buffer().text(), "");
        keys(&mut editor, "u");
        assert_eq!(editor.active_buffer().text(), "one");
    }

    #[test]
    fn empty_named_register_does_not_fall_back_to_unnamed() {
        let mut editor = editor("a\nb");
        keys(&mut editor, "yyj\"zp");
        assert_eq!(editor.active_buffer().text(), "a\nb");
        assert!(
            editor
                .current_message()
                .unwrap()
                .contains("Register z is empty")
        );

        // A register prefix applies to one command; the next put uses unnamed.
        keys(&mut editor, "p");
        assert_eq!(editor.active_buffer().text(), "a\nb\na");
    }

    #[test]
    fn search_wraps_within_the_starting_line() {
        let mut editor = editor("one one");
        keys(&mut editor, "/one");
        editor.handle_key(Key::plain(KeyCode::Enter));
        assert_eq!(editor.active_pane().cursor, Pos::new(0, 4));
        keys(&mut editor, "n");
        assert_eq!(editor.active_pane().cursor, Pos::ZERO);
        keys(&mut editor, "N");
        assert_eq!(editor.active_pane().cursor, Pos::new(0, 4));
    }

    #[test]
    fn undo_and_redo_honor_counts() {
        let mut editor = editor("");
        keys(&mut editor, "ia");
        editor.handle_key(Key::plain(KeyCode::Esc));
        keys(&mut editor, "ab");
        editor.handle_key(Key::plain(KeyCode::Esc));
        assert_eq!(editor.active_buffer().text(), "ab");

        keys(&mut editor, "2u");
        assert_eq!(editor.active_buffer().text(), "");
        keys(&mut editor, "2");
        editor.handle_key(Key::ctrl('r'));
        assert_eq!(editor.active_buffer().text(), "ab");

        keys(&mut editor, "2g-");
        assert_eq!(editor.active_buffer().text(), "");
        keys(&mut editor, "2g+");
        assert_eq!(editor.active_buffer().text(), "ab");
    }

    #[test]
    fn dot_repeats_line_delete_as_a_line_delete() {
        let mut editor = editor("aa xx\nbb yy\ncc zz");
        keys(&mut editor, "dd.");
        assert_eq!(editor.active_buffer().text(), "cc zz");
    }

    #[test]
    fn dot_repeats_the_actual_delete_motion_and_composed_count() {
        let mut first = editor("one two three four five");
        keys(&mut first, "d2w.");
        assert_eq!(first.active_buffer().text(), "five");

        let mut counted = editor("one two three four");
        keys(&mut counted, "dw2.");
        assert_eq!(counted.active_buffer().text(), "four");

        let mut line_end = editor("aa xx\nbb yy");
        keys(&mut line_end, "d$j.");
        assert_eq!(line_end.active_buffer().text(), "\n");
    }

    #[test]
    fn visual_prompt_and_leader_transitions_clear_the_old_anchor() {
        let mut command = editor("a\nb");
        keys(&mut command, "v:");
        assert_eq!(command.active_pane().anchor, None);
        command.handle_key(Key::plain(KeyCode::Enter));
        keys(&mut command, "Gvd");
        assert_eq!(command.active_buffer().text(), "a\n");

        let mut leader = editor("a\nb");
        keys(&mut leader, "v ");
        assert_eq!(leader.active_pane().anchor, None);
        leader.handle_key(Key::plain(KeyCode::Esc));
        keys(&mut leader, "Gvd");
        assert_eq!(leader.active_buffer().text(), "a\n");
    }

    #[test]
    fn change_word_preserves_following_whitespace() {
        let mut one = editor("one two");
        keys(&mut one, "cwX");
        one.handle_key(Key::plain(KeyCode::Esc));
        assert_eq!(one.active_buffer().text(), "X two");

        let mut counted = editor("one two three");
        keys(&mut counted, "c2wX");
        counted.handle_key(Key::plain(KeyCode::Esc));
        assert_eq!(counted.active_buffer().text(), "X three");

        let mut whitespace = editor("one   two");
        keys(&mut whitespace, "3lcwX");
        whitespace.handle_key(Key::plain(KeyCode::Esc));
        assert_eq!(whitespace.active_buffer().text(), "oneXtwo");
    }

    #[test]
    fn till_operator_and_repeat_use_the_matched_character_boundary() {
        let mut forward_delete = editor("abcde");
        keys(&mut forward_delete, "dtd");
        assert_eq!(forward_delete.active_buffer().text(), "de");

        let mut backward_delete = editor("abcde");
        keys(&mut backward_delete, "$dTb");
        assert_eq!(backward_delete.active_buffer().text(), "abe");

        let mut forward_repeat = editor("xaxaxa");
        keys(&mut forward_repeat, "ta;");
        assert_eq!(forward_repeat.active_pane().cursor, Pos::new(0, 2));

        let mut backward_repeat = editor("xaxaxa");
        keys(&mut backward_repeat, "$Tx;");
        assert_eq!(backward_repeat.active_pane().cursor, Pos::new(0, 3));
    }

    #[test]
    fn explicit_one_g_goes_to_the_first_line() {
        let mut motion = editor("a\nb\nc");
        keys(&mut motion, "G");
        assert_eq!(motion.active_pane().cursor.line, 2);
        keys(&mut motion, "1G");
        assert_eq!(motion.active_pane().cursor.line, 0);
        keys(&mut motion, "2G");
        assert_eq!(motion.active_pane().cursor.line, 1);

        let mut inner_count = editor("a\nb\nc");
        keys(&mut inner_count, "jd1G");
        assert_eq!(inner_count.active_buffer().text(), "c");

        let mut outer_count = editor("a\nb\nc");
        keys(&mut outer_count, "j1dG");
        assert_eq!(outer_count.active_buffer().text(), "c");
    }

    #[test]
    fn deleting_final_lines_removes_their_logical_lines() {
        let mut last = editor("one\ntwo");
        keys(&mut last, "Gdd");
        assert_eq!(last.active_buffer().text(), "one");
        assert_eq!(last.active_buffer().line_count(), 1);
        keys(&mut last, "p");
        assert_eq!(last.active_buffer().text(), "one\ntwo");

        let mut final_newline = editor("one\ntwo\n");
        keys(&mut final_newline, "Gdd");
        assert_eq!(final_newline.active_buffer().text(), "one\n");
        assert_eq!(final_newline.active_buffer().line_count(), 1);
        assert!(final_newline.active_buffer().has_final_newline());

        let mut empty_last = editor("one\n\n");
        keys(&mut empty_last, "Gdd");
        assert_eq!(empty_last.active_buffer().text(), "one\n");
        assert_eq!(empty_last.active_buffer().line_count(), 1);
        keys(&mut empty_last, "p");
        assert_eq!(empty_last.active_buffer().text(), "one\n\n");

        let mut counted = editor("a\nb\nc");
        keys(&mut counted, "j2dd");
        assert_eq!(counted.active_buffer().text(), "a");
        assert_eq!(counted.active_buffer().line_count(), 1);
        keys(&mut counted, "p");
        assert_eq!(counted.active_buffer().text(), "a\nb\nc");
    }

    #[test]
    fn imported_scratch_text_is_dirty_and_wq_requires_a_successful_write() {
        let mut imported = editor("");
        imported.open_scratch_text("[stdin]", "important\n");
        assert!(imported.active_buffer().is_dirty());
        imported.execute_ex("q");
        assert!(!imported.should_quit);

        imported.execute_ex("wq");
        assert!(!imported.should_quit);
        assert!(imported.current_message().unwrap().contains("no file path"));

        let mut empty = editor("");
        empty.open_scratch_text("[stdin]", "");
        assert!(!empty.active_buffer().is_dirty());
        empty.execute_ex("x");
        assert!(!empty.should_quit);
        assert!(empty.current_message().unwrap().contains("no file path"));
    }

    #[test]
    fn bd_deletes_the_active_buffer_and_keeps_a_scratch_when_no_buffers_remain() {
        let mut editor = editor("first");
        editor.open_scratch_text("[clean]", "");

        editor.execute_ex("bd");

        assert_eq!(editor.buffers.len(), 1);
        assert_eq!(editor.active_buffer().text(), "first");
        assert!(!editor.should_quit);

        editor.open_scratch_text("[stdin]", "important");
        editor.execute_ex("bd");

        assert_eq!(editor.buffers.len(), 2);
        assert_eq!(editor.active_buffer().text(), "important");
        assert!(
            editor
                .current_message()
                .is_some_and(|message| message.contains(":bd!"))
        );

        editor.execute_ex("bd!");
        assert_eq!(editor.buffers.len(), 1);
        assert_eq!(editor.active_buffer().text(), "first");
        assert!(!editor.should_quit);

        editor.execute_ex("bd");
        assert_eq!(editor.buffers.len(), 1);
        assert!(editor.active_buffer().is_empty());
        assert_eq!(editor.active_slot().display_name, "[scratch]");
        assert!(!editor.should_quit);
    }

    #[test]
    fn q_closes_the_active_split_retains_buffers_and_exits_on_the_last_pane() {
        for command in ["q", "quit"] {
            let mut editor = editor("first");
            let first_pane = editor.active_pane;
            editor.active_pane_mut().cursor = Pos::new(0, 2);
            editor.split(Orientation::Vertical);
            editor.open_scratch_text("[second]", "");
            editor.execute_ex(command);
            assert!(!editor.should_quit);
            assert_eq!(editor.panes.len(), 1);
            assert_eq!(editor.active_pane, first_pane);
            assert!(matches!(editor.layout, Layout::Leaf(id) if id == first_pane));
            assert_eq!(editor.active_pane().cursor, Pos::new(0, 2));
            assert_eq!(editor.active_buffer().text(), "first");
            assert_eq!(editor.buffers.len(), 2);

            editor.execute_ex(command);
            assert!(editor.should_quit);
            assert_eq!(editor.active_buffer().text(), "first");
            assert_eq!(editor.buffers.len(), 2);
        }
    }

    #[test]
    fn quitting_a_duplicate_pane_preserves_dirty_text_and_undo_history() {
        for command in ["q", "q!"] {
            let mut editor = editor("original");
            editor
                .active_buffer_mut()
                .insert(Pos::new(0, 8), " changed")
                .unwrap();
            editor.active_pane_mut().cursor = Pos::new(0, 3);
            let first_pane = editor.active_pane;
            editor.split(Orientation::Horizontal);
            editor.split(Orientation::Vertical);
            editor.execute_ex(command);
            assert_eq!(editor.panes.len(), 2);
            assert!(!editor.should_quit);
            editor.execute_ex(command);
            assert_eq!(editor.panes.len(), 1);
            assert_eq!(editor.active_pane, first_pane);
            assert_eq!(editor.active_pane().cursor, Pos::new(0, 3));
            assert_eq!(editor.buffers.len(), 1);
            assert_eq!(editor.active_buffer().text(), "original changed");
            assert!(editor.active_buffer().is_dirty());
            assert!(editor.active_buffer_mut().undo().unwrap());
            assert_eq!(editor.active_buffer().text(), "original");
        }
    }

    #[test]
    fn q_protects_an_abandoned_dirty_buffer_and_q_bang_discards_only_that_buffer() {
        let mut editor = editor("first");
        let first_pane = editor.active_pane;
        editor.active_pane_mut().cursor = Pos::new(0, 2);
        editor.split(Orientation::Vertical);
        editor.open_scratch_text("[second]", "unsaved");
        let second_pane = editor.active_pane;
        editor.execute_ex("q");
        assert!(!editor.should_quit);
        assert_eq!(editor.panes.len(), 2);
        assert_eq!(editor.active_pane, second_pane);
        assert_eq!(editor.active_buffer().text(), "unsaved");
        assert!(editor.current_message().unwrap().contains(":q!"));

        editor.execute_ex("quit!");
        assert!(!editor.should_quit);
        assert_eq!(editor.panes.len(), 1);
        assert_eq!(editor.active_pane, first_pane);
        assert_eq!(editor.active_pane().cursor, Pos::new(0, 2));
        assert_eq!(editor.buffers.len(), 1);
        assert_eq!(editor.active_buffer().text(), "first");
        editor.execute_ex("q");
        assert!(editor.should_quit);
    }

    #[test]
    fn quitting_the_last_pane_protects_dirty_hidden_buffers_even_when_forced() {
        for command in ["q", "q!"] {
            let mut editor = editor("hidden");
            editor
                .active_buffer_mut()
                .insert(Pos::new(0, 6), " changed")
                .unwrap();
            editor.open_scratch_text("[clean]", "");
            editor.execute_ex(command);
            assert!(!editor.should_quit);
            assert_eq!(editor.panes.len(), 1);
            assert_eq!(editor.active_buffer().text(), "hidden changed");
            assert_eq!(editor.buffers.len(), 2);
            assert!(editor.current_message().unwrap().contains(":qa!"));
        }

        let mut editor = editor("hidden");
        editor
            .active_buffer_mut()
            .insert(Pos::new(0, 6), " changed")
            .unwrap();
        editor.open_scratch_text("[active]", "discard me");
        editor.execute_ex("q!");
        assert!(!editor.should_quit);
        assert_eq!(editor.buffers.len(), 1);
        assert_eq!(editor.active_buffer().text(), "hidden changed");
        editor.execute_ex("q");
        assert!(!editor.should_quit);
        assert_eq!(editor.active_buffer().text(), "hidden changed");
        editor.execute_ex("q!");
        assert!(editor.should_quit);
    }

    #[test]
    fn write_and_quit_closes_one_pane_only_after_a_successful_save() {
        for command in ["wq", "x"] {
            let directory = tempfile::tempdir().unwrap();
            let file = directory.path().join("save.txt");
            std::fs::write(&file, "original").unwrap();
            let mut editor = Editor::new(Config::default(), directory.path().to_owned());
            editor.open_path(&file).unwrap();
            editor.discard_initial_scratch();
            editor
                .active_buffer_mut()
                .insert(Pos::new(0, 8), " changed")
                .unwrap();
            editor.split(Orientation::Vertical);
            editor.execute_ex(command);
            assert_eq!(editor.panes.len(), 1);
            assert!(!editor.should_quit);
            assert_eq!(editor.active_buffer().text(), "original changed");
            assert!(!editor.active_buffer().is_dirty());
            assert_eq!(std::fs::read_to_string(&file).unwrap(), "original changed");
            editor.execute_ex(command);
            assert!(editor.should_quit);
        }
        let directory = tempfile::tempdir().unwrap();
        let mut editor = Editor::new(Config::default(), directory.path().to_owned());
        editor.split(Orientation::Vertical);
        editor.open_scratch_text("[unsaved]", "keep me");
        editor.execute_ex("wq missing-parent/new.txt");
        assert_eq!(editor.panes.len(), 2);
        assert!(!editor.should_quit);
        assert_eq!(editor.active_buffer().text(), "keep me");
    }

    #[test]
    fn forced_write_and_quit_does_not_discard_other_dirty_buffers() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("save.txt");
        std::fs::write(&file, "original").unwrap();
        let mut editor = Editor::new(Config::default(), directory.path().to_owned());
        editor.open_scratch_text("[hidden]", "keep me");
        editor.open_path(&file).unwrap();
        editor
            .active_buffer_mut()
            .insert(Pos::new(0, 8), " changed")
            .unwrap();
        editor.execute_ex("wq!");
        assert!(!editor.should_quit);
        assert_eq!(std::fs::read_to_string(file).unwrap(), "original changed");
        assert_eq!(editor.active_buffer().text(), "keep me");
        assert!(editor.active_buffer().is_dirty());
    }

    #[test]
    fn qa_quits_all_only_when_every_buffer_is_safe() {
        let mut protected = editor("base");
        protected
            .active_buffer_mut()
            .insert(Pos::new(0, 4), " changed")
            .unwrap();
        protected.open_scratch_text("[clean]", "");

        protected.execute_ex("qa");

        assert!(!protected.should_quit);
        assert!(protected.current_message().is_some_and(|message| {
            message.contains("Unsaved changes in 1 buffer") && message.contains(":qa!")
        }));

        protected.execute_ex("qa!");
        assert!(protected.should_quit);

        let mut clean = editor("clean");
        clean.open_scratch_text("[also clean]", "");
        clean.execute_ex("qall");
        assert!(clean.should_quit);
    }

    #[test]
    fn successful_rust_save_requests_an_lsp_did_save_notification() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("main.rs");
        std::fs::write(&file, "fn main() {}\n").unwrap();
        let mut editor = Editor::new(Config::default(), directory.path().to_owned());
        editor.open_path(&file).unwrap();
        editor.discard_initial_scratch();
        editor
            .active_buffer_mut()
            .insert(Pos::new(0, 12), " ")
            .unwrap();

        editor.execute_ex("w");

        assert!(matches!(
            editor.take_request(),
            EditorRequest::DocumentSaved(path) if path == file.canonicalize().unwrap()
        ));
    }

    #[test]
    fn file_picker_publishes_only_current_query_results() {
        let mut editor = editor("");
        editor.set_project_files(vec![PathBuf::from("old.rs"), PathBuf::from("new.rs")]);
        editor.open_picker(PickerKind::Files);
        editor.picker.as_mut().unwrap().query = "old".into();
        editor.refresh_picker();

        fn wait_for_results(editor: &mut Editor) {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !editor.poll_file_finder() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "file finder did not publish"
                );
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }

        wait_for_results(&mut editor);
        assert_eq!(editor.picker.as_ref().unwrap().items[0].label, "old.rs");
        editor.picker.as_mut().unwrap().query = "new".into();
        editor.refresh_picker();
        assert!(editor.picker.as_ref().unwrap().items.is_empty());
        wait_for_results(&mut editor);
        assert_eq!(editor.picker.as_ref().unwrap().items[0].label, "new.rs");

        editor.append_project_files(vec![PathBuf::from("newer.rs")]);
        wait_for_results(&mut editor);
        assert_eq!(editor.picker.as_ref().unwrap().items.len(), 2);

        editor.refresh_picker();
        editor.show_picker_items(
            PickerKind::Symbols,
            vec![PickerItem {
                label: "keep this symbol".into(),
                detail: String::new(),
                path: None,
                line: None,
                insert_text: None,
            }],
        );
        assert!(!editor.poll_file_finder());
        assert_eq!(
            editor.picker.as_ref().unwrap().items[0].label,
            "keep this symbol"
        );
        assert!(!editor.file_finder_active);
    }

    #[test]
    fn explorer_space_e_closes_without_stealing_other_explorer_keys() {
        let mut toggle = editor("");
        toggle.explorer.open = true;
        toggle.focus = Focus::Explorer;
        let show_hidden = toggle.explorer.show_hidden;
        keys(&mut toggle, " e");
        assert!(!toggle.explorer.open);
        assert_eq!(toggle.focus, Focus::Editor);
        assert_eq!(toggle.explorer.show_hidden, show_hidden);

        let mut navigation = editor("");
        navigation.explorer.open = true;
        navigation.focus = Focus::Explorer;
        navigation.set_project_files(vec![PathBuf::from("a"), PathBuf::from("b")]);
        keys(&mut navigation, " j");
        assert_eq!(navigation.explorer.selected, 1);
        assert_eq!(navigation.focus, Focus::Explorer);

        let show_hidden = navigation.explorer.show_hidden;
        keys(&mut navigation, "e");
        assert_ne!(navigation.explorer.show_hidden, show_hidden);
    }

    #[test]
    fn explorer_window_navigation_respects_control_modifiers() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("selected.rs");
        fs::write(&path, "fn selected() {}\n").unwrap();

        let mut editor = editor("keep this buffer active");
        editor.explorer.open = true;
        editor.focus = Focus::Explorer;
        editor.set_project_files(vec![path.clone()]);

        editor.handle_key(Key::ctrl('w'));
        editor.handle_key(Key::char('l'));
        assert_eq!(editor.focus, Focus::Editor);
        assert_eq!(editor.buffers.len(), 1);
        assert_eq!(editor.active_buffer().text(), "keep this buffer active");

        editor.focus = Focus::Explorer;
        editor.handle_key(Key::ctrl('l'));
        assert_eq!(editor.focus, Focus::Editor);
        assert_eq!(editor.buffers.len(), 1);
        assert_eq!(editor.active_buffer().text(), "keep this buffer active");

        editor.focus = Focus::Explorer;
        editor.handle_key(Key::char('l'));
        assert_eq!(editor.focus, Focus::Editor);
        assert_eq!(editor.buffers.len(), 2);
        assert_eq!(editor.active_buffer().path(), Some(path.as_path()));
    }

    #[test]
    fn invalid_window_prefix_key_is_consumed_before_explorer_dispatch() {
        let mut editor = editor("");
        editor.explorer.open = true;
        editor.focus = Focus::Explorer;
        let show_hidden = editor.explorer.show_hidden;

        editor.handle_key(Key::ctrl('w'));
        editor.handle_key(Key::char('e'));

        assert_eq!(editor.focus, Focus::Explorer);
        assert_eq!(editor.explorer.show_hidden, show_hidden);
        assert!(matches!(editor.request, EditorRequest::None));
        assert!(
            editor
                .current_message()
                .is_some_and(|message| message.contains("Unsupported window command"))
        );

        editor.handle_key(Key::ctrl('e'));
        assert_eq!(editor.explorer.show_hidden, show_hidden);
        assert!(
            editor
                .current_message()
                .is_some_and(|message| message.contains("Unsupported control key"))
        );
    }

    #[test]
    fn buffer_diagnostic_commands_ignore_stale_versions() {
        let mut editor = editor("a\nb\nc");
        let version = editor.active_buffer().revision();
        editor.diagnostics = vec![
            Diagnostic {
                path: None,
                line: 1,
                column: 0,
                severity: DiagnosticSeverity::Error,
                message: "stale earlier".into(),
                version: version + 1,
            },
            Diagnostic {
                path: None,
                line: 2,
                column: 0,
                severity: DiagnosticSeverity::Warning,
                message: "current".into(),
                version,
            },
            Diagnostic {
                path: None,
                line: 2,
                column: 0,
                severity: DiagnosticSeverity::Error,
                message: "stale same line".into(),
                version: version + 1,
            },
            Diagnostic {
                path: Some(PathBuf::from("unopened.rs")),
                line: 4,
                column: 0,
                severity: DiagnosticSeverity::Information,
                message: "workspace".into(),
                version: 99,
            },
        ];

        keys(&mut editor, "]d");
        assert_eq!(editor.active_pane().cursor, Pos::new(2, 0));
        assert_eq!(editor.current_message(), Some("current"));

        keys(&mut editor, "gl");
        assert_eq!(editor.current_message(), Some("current"));

        editor.execute_command(CommandId::BufferDiagnostics);
        let labels: Vec<_> = editor
            .picker
            .as_ref()
            .unwrap()
            .items
            .iter()
            .map(|item| item.label.as_str())
            .collect();
        assert_eq!(labels, ["current"]);

        editor.execute_command(CommandId::WorkspaceDiagnostics);
        assert!(
            editor
                .picker
                .as_ref()
                .unwrap()
                .items
                .iter()
                .any(|item| item.label == "workspace")
        );
    }

    #[test]
    fn analyzer_restart_remains_available_when_the_server_failed() {
        let mut editor = editor("");
        editor.rust_analyzer_status = "failed".into();

        keys(&mut editor, " cR");

        assert!(matches!(
            editor.take_request(),
            EditorRequest::RustAnalyzer(CommandId::RustAnalyzerRestart)
        ));
    }
}
