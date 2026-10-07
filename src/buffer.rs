//! UTF-8 text storage, editing, history, and safe file persistence.
//!
//! `Buffer` deliberately has no terminal or modal-editing dependencies.  A
//! frontend owns cursors (one buffer can be displayed in several panes) and
//! applies logical changes through the transaction API.  Text positions use
//! extended grapheme-cluster columns so cursor movement and edits cannot split
//! a user-perceived character.

use std::fmt;
use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use unicode_segmentation::UnicodeSegmentation;

/// A cursor position expressed as a zero-based line and grapheme column.
///
/// A position at the end of a line is valid.  Newline bytes are not themselves
/// cursor positions; moving past the end of a line reaches column zero of the
/// following line.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Pos {
    pub line: usize,
    pub grapheme: usize,
}

impl Pos {
    pub const ZERO: Self = Self::new(0, 0);

    pub const fn new(line: usize, grapheme: usize) -> Self {
        Self { line, grapheme }
    }
}

/// A half-open range of text (`start..end`).
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct TextRange {
    pub start: Pos,
    pub end: Pos,
}

impl TextRange {
    pub const fn new(start: Pos, end: Pos) -> Self {
        Self { start, end }
    }

    pub fn ordered(self) -> Self {
        if self.start <= self.end {
            self
        } else {
            Self::new(self.end, self.start)
        }
    }

    pub const fn is_empty(self) -> bool {
        self.start.line == self.end.line && self.start.grapheme == self.end.grapheme
    }
}

/// A byte offset in the serialized UTF-8 document.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ByteOffset(pub usize);

/// A zero-based LSP-compatible line and UTF-16 code-unit column.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Utf16Pos {
    pub line: usize,
    pub code_unit: usize,
}

impl Utf16Pos {
    pub const fn new(line: usize, code_unit: usize) -> Self {
        Self { line, code_unit }
    }
}

/// The separator used between two text lines.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum LineEnding {
    #[default]
    Lf,
    Crlf,
}

impl LineEnding {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Lf => "\n",
            Self::Crlf => "\r\n",
        }
    }

    pub const fn byte_len(self) -> usize {
        self.as_str().len()
    }
}

/// Errors produced by loading, editing, or saving a buffer.
#[derive(Debug)]
pub enum BufferError {
    Io(io::Error),
    InvalidUtf8 { path: PathBuf, valid_up_to: usize },
    InvalidPosition(Pos),
    InvalidRange(TextRange),
    InvalidByteOffset(ByteOffset),
    InvalidUtf16Position(Utf16Pos),
    NoPath,
    UnsavedChanges,
    ExternalModification(PathBuf),
    DestinationExists(PathBuf),
    TransactionInProgress,
    NoTransaction,
}

impl fmt::Display for BufferError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "{error}"),
            Self::InvalidUtf8 { path, valid_up_to } => write!(
                f,
                "{} is not valid UTF-8 (valid through byte {valid_up_to})",
                path.display()
            ),
            Self::InvalidPosition(pos) => {
                write!(f, "invalid buffer position {}:{}", pos.line, pos.grapheme)
            }
            Self::InvalidRange(range) => write!(
                f,
                "invalid buffer range {}:{}..{}:{}",
                range.start.line, range.start.grapheme, range.end.line, range.end.grapheme
            ),
            Self::InvalidByteOffset(offset) => {
                write!(f, "byte offset {} is not a text boundary", offset.0)
            }
            Self::InvalidUtf16Position(pos) => write!(
                f,
                "UTF-16 position {}:{} is not a grapheme boundary",
                pos.line, pos.code_unit
            ),
            Self::NoPath => write!(f, "the buffer has no file path"),
            Self::UnsavedChanges => write!(f, "the buffer has unsaved changes"),
            Self::ExternalModification(path) => write!(
                f,
                "{} changed on disk since it was loaded or saved",
                path.display()
            ),
            Self::DestinationExists(path) => {
                write!(f, "refusing to overwrite existing file {}", path.display())
            }
            Self::TransactionInProgress => write!(f, "an edit transaction is already active"),
            Self::NoTransaction => write!(f, "there is no active edit transaction"),
        }
    }
}

impl std::error::Error for BufferError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for BufferError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

pub type Result<T> = std::result::Result<T, BufferError>;

/// A changed range, including exact separator ownership. A literal CR at the
/// end of a line followed by LF is distinct from a CRLF separator even though
/// their serialized bytes match; undo must not reparse that distinction away.
#[derive(Clone, Debug, Eq, PartialEq)]
struct TextFragment {
    lines: Vec<String>,
    endings: Vec<LineEnding>,
}

impl TextFragment {
    fn to_text(&self) -> String {
        let mut text = String::new();
        for (index, line) in self.lines.iter().enumerate() {
            text.push_str(line);
            if let Some(ending) = self.endings.get(index) {
                text.push_str(ending.as_str());
            }
        }
        text
    }
    fn heap_bytes(&self) -> usize {
        self.lines.capacity() * std::mem::size_of::<String>()
            + self.lines.iter().map(String::capacity).sum::<usize>()
            + self.endings.capacity() * std::mem::size_of::<LineEnding>()
    }
}

#[derive(Debug)]
struct TextState {
    lines: Vec<String>,
    endings: Vec<LineEnding>,
    preferred_ending: LineEnding,
    line_info: Vec<LineInfo>,
    content_hash: u64,
    content_bytes: usize,
    crlf_count: usize,
}

#[derive(Debug)]
struct LineInfo {
    hash: u64,
    key: u64,
    graphemes: std::sync::OnceLock<GraphemeIndex>,
}

#[derive(Debug)]
enum GraphemeIndex {
    Ascii,
    Unicode(Vec<usize>),
}

static LINE_KEY: AtomicU64 = AtomicU64::new(1);

impl LineInfo {
    fn for_lines(lines: &[String]) -> Vec<Self> {
        use std::hash::{Hash, Hasher};
        let first = LINE_KEY.fetch_add(lines.len() as u64, Ordering::Relaxed);
        lines
            .iter()
            .enumerate()
            .map(|(index, line)| {
                let mut hash = std::collections::hash_map::DefaultHasher::new();
                line.hash(&mut hash);
                Self {
                    hash: hash.finish(),
                    key: first + index as u64,
                    graphemes: std::sync::OnceLock::new(),
                }
            })
            .collect()
    }
}

impl Clone for TextState {
    fn clone(&self) -> Self {
        Self {
            lines: self.lines.clone(),
            endings: self.endings.clone(),
            preferred_ending: self.preferred_ending,
            line_info: self
                .line_info
                .iter()
                .map(|line| LineInfo {
                    hash: line.hash,
                    key: line.key,
                    graphemes: std::sync::OnceLock::new(),
                })
                .collect(),
            content_hash: self.content_hash,
            content_bytes: self.content_bytes,
            crlf_count: self.crlf_count,
        }
    }
}

impl PartialEq for TextState {
    fn eq(&self, other: &Self) -> bool {
        self.signature() == other.signature()
            && self.lines == other.lines
            && self.endings == other.endings
    }
}
impl Eq for TextState {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TextSignature {
    hash: u64,
    bytes: usize,
    lines: usize,
    endings: usize,
    crlf: usize,
    preferred: LineEnding,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BytePoint {
    line: usize,
    byte: usize,
}

impl TextState {
    fn empty() -> Self {
        Self::parse("")
    }

    fn from_parts(
        lines: Vec<String>,
        endings: Vec<LineEnding>,
        preferred_ending: LineEnding,
    ) -> Self {
        let line_info = LineInfo::for_lines(&lines);
        let content_hash = line_info
            .iter()
            .fold(0u64, |sum, line| sum.wrapping_add(line.hash));
        let content_bytes = lines.iter().map(String::len).sum();
        let crlf_count = endings
            .iter()
            .filter(|ending| **ending == LineEnding::Crlf)
            .count();
        Self {
            lines,
            endings,
            preferred_ending,
            line_info,
            content_hash,
            content_bytes,
            crlf_count,
        }
    }

    fn signature(&self) -> TextSignature {
        TextSignature {
            hash: self.content_hash,
            bytes: self.content_bytes,
            lines: self.lines.len(),
            endings: self.endings.len(),
            crlf: self.crlf_count,
            preferred: self.preferred_ending,
        }
    }

    fn parse(text: &str) -> Self {
        let (mut lines, endings, preferred) = Self::parse_parts(text);
        if text.ends_with('\n') {
            lines.pop();
        }
        Self::from_parts(lines, endings, preferred)
    }

    fn parse_parts(text: &str) -> (Vec<String>, Vec<LineEnding>, LineEnding) {
        let mut lines = Vec::new();
        let mut endings = Vec::new();
        let mut start = 0;
        for (newline, _) in text.match_indices('\n') {
            let before = &text[start..newline];
            if let Some(line) = before.strip_suffix('\r') {
                lines.push(line.to_owned());
                endings.push(LineEnding::Crlf);
            } else {
                lines.push(before.to_owned());
                endings.push(LineEnding::Lf);
            }
            start = newline + 1;
        }
        lines.push(text[start..].to_owned());
        let crlf = endings
            .iter()
            .filter(|ending| **ending == LineEnding::Crlf)
            .count();
        let preferred = if crlf * 2 > endings.len() {
            LineEnding::Crlf
        } else if crlf > 0 && crlf * 2 == endings.len() {
            endings[0]
        } else {
            LineEnding::Lf
        };
        (lines, endings, preferred)
    }

    fn invariant_holds(&self) -> bool {
        !self.lines.is_empty()
            && self.line_info.len() == self.lines.len()
            && (self.endings.len() + 1 == self.lines.len()
                || self.endings.len() == self.lines.len())
    }

    fn write_to(&self, output: &mut Vec<u8>) {
        debug_assert!(self.invariant_holds());
        for (index, line) in self.lines.iter().enumerate() {
            output.extend_from_slice(line.as_bytes());
            if let Some(ending) = self.endings.get(index) {
                output.extend_from_slice(ending.as_str().as_bytes());
            }
        }
    }
    fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.byte_len());
        self.write_to(&mut out);
        out
    }
    fn byte_len(&self) -> usize {
        self.content_bytes + self.endings.len() + self.crlf_count
    }
    fn to_text(&self) -> String {
        String::from_utf8(self.to_bytes()).expect("TextState only contains UTF-8")
    }

    fn grapheme_index(&self, line: usize) -> Option<&GraphemeIndex> {
        let text = self.lines.get(line)?;
        Some(self.line_info[line].graphemes.get_or_init(|| {
            if text.is_ascii() {
                GraphemeIndex::Ascii
            } else {
                let mut offsets: Vec<_> =
                    text.grapheme_indices(true).map(|(byte, _)| byte).collect();
                offsets.push(text.len());
                GraphemeIndex::Unicode(offsets)
            }
        }))
    }
    fn grapheme_count(&self, line: usize) -> Option<usize> {
        Some(match self.grapheme_index(line)? {
            GraphemeIndex::Ascii => self.lines[line].len(),
            GraphemeIndex::Unicode(offsets) => offsets.len() - 1,
        })
    }
    fn validate_pos(&self, pos: Pos) -> Result<()> {
        self.byte_in_line(pos).map(|_| ())
    }
    fn byte_in_line(&self, pos: Pos) -> Result<usize> {
        let line = self
            .lines
            .get(pos.line)
            .ok_or(BufferError::InvalidPosition(pos))?;
        if pos.grapheme == 0 {
            return Ok(0);
        }
        let index = &self.line_info[pos.line].graphemes;
        if index.get().is_none() && line.is_ascii() {
            let _ = index.set(GraphemeIndex::Ascii);
        }
        let byte = match index.get() {
            Some(GraphemeIndex::Ascii) => (pos.grapheme <= line.len()).then_some(pos.grapheme),
            Some(GraphemeIndex::Unicode(offsets)) => offsets.get(pos.grapheme).copied(),
            // A cursor near the beginning of a cold Unicode line needs only
            // its prefix. Full counts build an index when callers need EOF.
            None => line
                .grapheme_indices(true)
                .map(|(byte, _)| byte)
                .chain(std::iter::once(line.len()))
                .nth(pos.grapheme),
        };
        byte.ok_or(BufferError::InvalidPosition(pos))
    }
    fn grapheme_at_or_after_byte(&self, line: usize, byte: usize) -> usize {
        let text = &self.lines[line];
        let index = &self.line_info[line].graphemes;
        if index.get().is_none() && text.is_ascii() {
            let _ = index.set(GraphemeIndex::Ascii);
        }
        match index.get() {
            Some(GraphemeIndex::Ascii) => byte,
            Some(GraphemeIndex::Unicode(offsets)) => {
                offsets.partition_point(|boundary| *boundary < byte)
            }
            None => {
                let mut count = 0;
                for (grapheme, (boundary, _)) in text.grapheme_indices(true).enumerate() {
                    if boundary >= byte {
                        return grapheme;
                    }
                    count = grapheme + 1;
                }
                count
            }
        }
    }
    fn selected_bytes(&self, start: BytePoint, end: BytePoint) -> String {
        if start.line == end.line {
            return self.lines[start.line][start.byte..end.byte].to_owned();
        }
        let mut selected = self.lines[start.line][start.byte..].to_owned();
        for line in start.line..end.line {
            selected.push_str(self.endings[line].as_str());
            if line + 1 < end.line {
                selected.push_str(&self.lines[line + 1]);
            }
        }
        selected.push_str(&self.lines[end.line][..end.byte]);
        selected
    }
    fn selected_fragment(&self, start: BytePoint, end: BytePoint) -> TextFragment {
        if start.line == end.line {
            return TextFragment {
                lines: vec![self.lines[start.line][start.byte..end.byte].to_owned()],
                endings: Vec::new(),
            };
        }
        let mut lines = Vec::with_capacity(end.line - start.line + 1);
        lines.push(self.lines[start.line][start.byte..].to_owned());
        lines.extend(self.lines[start.line + 1..end.line].iter().cloned());
        lines.push(self.lines[end.line][..end.byte].to_owned());
        TextFragment {
            lines,
            endings: self.endings[start.line..end.line].to_vec(),
        }
    }
    fn selected_text(&self, range: TextRange) -> Result<String> {
        let range = range.ordered();
        let start = BytePoint {
            line: range.start.line,
            byte: self.byte_in_line(range.start)?,
        };
        let end = BytePoint {
            line: range.end.line,
            byte: self.byte_in_line(range.end)?,
        };
        Ok(self.selected_bytes(start, end))
    }

    /// Applies already validated byte boundaries. Undo uses byte boundaries so
    /// inserting or removing combining characters cannot invalidate its range.
    fn splice_fragment(&mut self, start: BytePoint, end: BytePoint, fragment: &TextFragment) {
        let mut replacement = fragment.lines.clone();
        let endings = &fragment.endings;
        replacement[0].insert_str(0, &self.lines[start.line][..start.byte]);
        replacement
            .last_mut()
            .unwrap()
            .push_str(&self.lines[end.line][end.byte..]);
        for line in start.line..=end.line {
            self.content_hash = self.content_hash.wrapping_sub(self.line_info[line].hash);
            self.content_bytes -= self.lines[line].len();
        }
        for ending in &self.endings[start.line..end.line] {
            self.crlf_count -= usize::from(*ending == LineEnding::Crlf);
        }
        for ending in endings {
            self.crlf_count += usize::from(*ending == LineEnding::Crlf);
        }
        let info = LineInfo::for_lines(&replacement);
        for (line, info) in replacement.iter().zip(&info) {
            self.content_hash = self.content_hash.wrapping_add(info.hash);
            self.content_bytes += line.len();
        }
        self.lines.splice(start.line..=end.line, replacement);
        self.line_info.splice(start.line..=end.line, info);
        self.endings
            .splice(start.line..end.line, endings.iter().copied());
        debug_assert!(self.invariant_holds());
    }

    fn replace(&mut self, range: TextRange, text: &str) -> Result<(Pos, String, Option<Change>)> {
        let range = range.ordered();
        let start = BytePoint {
            line: range.start.line,
            byte: self.byte_in_line(range.start)?,
        };
        let before_end = BytePoint {
            line: range.end.line,
            byte: self.byte_in_line(range.end)?,
        };
        let before = self.selected_fragment(start, before_end);
        let removed = before.to_text();
        let (parts, mut endings, inserted_preferred) = Self::parse_parts(text);
        let preferred_before = self.preferred_ending;
        let empty_after_delete = self.lines.len() - (before_end.line - start.line) == 1
            && start.byte == 0
            && before_end.byte == self.lines[before_end.line].len()
            && self.endings.len() == before_end.line - start.line;
        let preferred_after = if parts.len() > 1 && empty_after_delete {
            inserted_preferred
        } else {
            preferred_before
        };
        endings.fill(preferred_after);
        let after_end = BytePoint {
            line: start.line + parts.len() - 1,
            byte: if parts.len() == 1 {
                start.byte + parts[0].len()
            } else {
                parts.last().unwrap().len()
            },
        };
        let after = TextFragment {
            lines: parts,
            endings,
        };
        let change = if before != after || preferred_before != preferred_after {
            self.splice_fragment(start, before_end, &after);
            self.preferred_ending = preferred_after;
            Some(Change::Text {
                start,
                before_end,
                after_end,
                before,
                after,
                preferred_before,
                preferred_after,
            })
        } else {
            None
        };
        let cursor = Pos::new(
            after_end.line,
            self.grapheme_at_or_after_byte(after_end.line, after_end.byte),
        );
        Ok((cursor, removed, change))
    }
}

#[derive(Clone, Debug)]
enum Change {
    /// Expose the implicit empty line after a loaded final newline so LSP
    /// edits can distinguish the end of the last line from serialized EOF.
    EofLine,
    Text {
        start: BytePoint,
        before_end: BytePoint,
        after_end: BytePoint,
        before: TextFragment,
        after: TextFragment,
        preferred_before: LineEnding,
        preferred_after: LineEnding,
    },
    Whole {
        before: Box<TextState>,
        after: Box<TextState>,
    },
    Endings {
        before: Vec<LineEnding>,
        preferred_before: LineEnding,
        after: LineEnding,
    },
}

impl Change {
    fn apply(&self, state: &mut TextState, forward: bool) {
        match self {
            Self::EofLine => {
                if forward {
                    let info = LineInfo::for_lines(&[String::new()]).pop().unwrap();
                    state.content_hash = state.content_hash.wrapping_add(info.hash);
                    state.lines.push(String::new());
                    state.line_info.push(info);
                } else {
                    state.lines.pop();
                    let info = state.line_info.pop().unwrap();
                    state.content_hash = state.content_hash.wrapping_sub(info.hash);
                }
                debug_assert!(state.invariant_holds());
            }
            Self::Text {
                start,
                before_end,
                after_end,
                before,
                after,
                preferred_before,
                preferred_after,
            } => {
                state.splice_fragment(
                    *start,
                    if forward { *before_end } else { *after_end },
                    if forward { after } else { before },
                );
                state.preferred_ending = if forward {
                    *preferred_after
                } else {
                    *preferred_before
                };
            }
            Self::Whole { before, after } => {
                *state = if forward {
                    after.as_ref()
                } else {
                    before.as_ref()
                }
                .clone()
            }
            Self::Endings {
                before,
                preferred_before,
                after,
            } => {
                if forward {
                    state.endings.fill(*after);
                    state.preferred_ending = *after;
                } else {
                    state.endings.clone_from(before);
                    state.preferred_ending = *preferred_before;
                }
                state.crlf_count = state
                    .endings
                    .iter()
                    .filter(|ending| **ending == LineEnding::Crlf)
                    .count();
            }
        }
    }
    fn heap_bytes(&self) -> usize {
        match self {
            Self::EofLine => 0,
            Self::Text { before, after, .. } => before.heap_bytes() + after.heap_bytes(),
            Self::Whole { before, after } => [before, after]
                .iter()
                .map(|state| {
                    std::mem::size_of::<TextState>()
                        + state.lines.iter().map(String::capacity).sum::<usize>()
                        + state.lines.capacity() * std::mem::size_of::<String>()
                        + state.endings.capacity() * std::mem::size_of::<LineEnding>()
                        + state.line_info.capacity() * std::mem::size_of::<LineInfo>()
                        + state
                            .line_info
                            .iter()
                            .map(|line| match line.graphemes.get() {
                                Some(GraphemeIndex::Unicode(offsets)) => {
                                    offsets.capacity() * std::mem::size_of::<usize>()
                                }
                                _ => 0,
                            })
                            .sum::<usize>()
                })
                .sum(),
            Self::Endings { before, .. } => before.capacity() * std::mem::size_of::<LineEnding>(),
        }
    }
}

#[derive(Clone, Debug)]
struct UndoNode {
    changes: Vec<Change>,
    parent: Option<usize>,
    children: Vec<usize>,
    preferred_child: Option<usize>,
}

#[derive(Clone, Debug)]
struct UndoHistory {
    nodes: Vec<UndoNode>,
    current: usize,
    limit: usize,
    byte_limit: usize,
    retained_bytes: usize,
}

impl UndoHistory {
    const DEFAULT_LIMIT: usize = 1_000;
    const DEFAULT_BYTE_LIMIT: usize = 64 * 1024 * 1024;
    fn new() -> Self {
        Self {
            nodes: vec![UndoNode {
                changes: Vec::new(),
                parent: None,
                children: Vec::new(),
                preferred_child: None,
            }],
            current: 0,
            limit: Self::DEFAULT_LIMIT,
            byte_limit: Self::DEFAULT_BYTE_LIMIT,
            retained_bytes: 0,
        }
    }
    fn reset(&mut self) {
        let (limit, byte_limit) = (self.limit, self.byte_limit);
        *self = Self::new();
        self.limit = limit;
        self.byte_limit = byte_limit;
    }
    fn set_limit(&mut self, limit: usize) {
        self.limit = limit.max(2);
        if self.nodes.len() > self.limit {
            self.reset();
        }
    }
    fn set_byte_limit(&mut self, limit: usize) {
        self.byte_limit = limit;
        if self.retained_bytes > limit {
            self.reset();
        }
    }
    fn commit(&mut self, changes: Vec<Change>) {
        let bytes = changes.capacity() * std::mem::size_of::<Change>()
            + changes.iter().map(Change::heap_bytes).sum::<usize>();
        if self.nodes.len() >= self.limit
            || self.retained_bytes.saturating_add(bytes) > self.byte_limit
        {
            self.reset();
        }
        // An individual edit larger than the budget remains applied, but must
        // not retain an unbounded recovery copy in the undo tree.
        if bytes > self.byte_limit {
            return;
        }
        let parent = self.current;
        let child = self.nodes.len();
        self.nodes.push(UndoNode {
            changes,
            parent: Some(parent),
            children: Vec::new(),
            preferred_child: None,
        });
        self.nodes[parent].children.push(child);
        self.nodes[parent].preferred_child = Some(child);
        self.current = child;
        self.retained_bytes += bytes;
    }
    fn undo(&mut self, state: &mut TextState) -> bool {
        let child = self.current;
        let Some(parent) = self.nodes[child].parent else {
            return false;
        };
        for change in self.nodes[child].changes.iter().rev() {
            change.apply(state, false);
        }
        self.nodes[parent].preferred_child = Some(child);
        self.current = parent;
        true
    }
    fn redo(&mut self, state: &mut TextState) -> bool {
        let Some(child) = self.nodes[self.current]
            .preferred_child
            .or_else(|| self.nodes[self.current].children.last().copied())
        else {
            return false;
        };
        self.redo_to(child, state);
        true
    }
    fn redo_branch(&mut self, branch: usize, state: &mut TextState) -> bool {
        let Some(child) = self.nodes[self.current].children.get(branch).copied() else {
            return false;
        };
        self.redo_to(child, state);
        true
    }
    fn redo_to(&mut self, child: usize, state: &mut TextState) {
        self.nodes[self.current].preferred_child = Some(child);
        for change in &self.nodes[child].changes {
            change.apply(state, true);
        }
        self.current = child;
    }
    fn redo_branch_count(&self) -> usize {
        self.nodes[self.current].children.len()
    }
    fn can_undo(&self) -> bool {
        self.nodes[self.current].parent.is_some()
    }
    fn can_redo(&self) -> bool {
        !self.nodes[self.current].children.is_empty()
    }
}

#[derive(Clone, Debug)]
struct Transaction {
    before: TextSignature,
    changes: Vec<Change>,
}

impl Transaction {
    fn changed(&self, state: &TextState) -> bool {
        if self.changes.is_empty() {
            return false;
        }
        if self.before != state.signature() {
            return true;
        }
        // Hashes only reject equality. For a possible net-zero transaction,
        // reconstruct the original using borrowed unchanged lines, then compare
        // exact text. This uncommon path copies metadata and touched text only.
        use std::borrow::Cow;
        let mut lines: Vec<Cow<'_, str>> = state
            .lines
            .iter()
            .map(|line| Cow::Borrowed(line.as_str()))
            .collect();
        let mut endings = state.endings.clone();
        let mut preferred = state.preferred_ending;
        for change in self.changes.iter().rev() {
            match change {
                Change::EofLine => {
                    lines.pop();
                }
                Change::Text {
                    start,
                    after_end,
                    before,
                    preferred_before,
                    ..
                } => {
                    let mut replacement = before.lines.clone();
                    replacement[0].insert_str(0, &lines[start.line][..start.byte]);
                    replacement
                        .last_mut()
                        .unwrap()
                        .push_str(&lines[after_end.line][after_end.byte..]);
                    lines.splice(
                        start.line..=after_end.line,
                        replacement.into_iter().map(Cow::Owned),
                    );
                    endings.splice(start.line..after_end.line, before.endings.iter().copied());
                    preferred = *preferred_before;
                }
                Change::Whole { before, .. } => {
                    lines = before
                        .lines
                        .iter()
                        .map(|line| Cow::Borrowed(line.as_str()))
                        .collect();
                    endings.clone_from(&before.endings);
                    preferred = before.preferred_ending;
                }
                Change::Endings {
                    before,
                    preferred_before,
                    ..
                } => {
                    endings.clone_from(before);
                    preferred = *preferred_before;
                }
            }
        }
        preferred != state.preferred_ending
            || endings != state.endings
            || lines.len() != state.lines.len()
            || lines
                .iter()
                .zip(&state.lines)
                .any(|(left, right)| left.as_ref() != right)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DiskState {
    target: PathBuf,
    bytes: Vec<u8>,
    modified: Option<SystemTime>,
    readonly: bool,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    mode: u32,
}

impl DiskState {
    fn capture(path: &Path) -> io::Result<Self> {
        let target = fs::canonicalize(path)?;
        for _ in 0..3 {
            // Read through one open file description and compare metadata on
            // both sides, avoiding a torn baseline when another process is
            // actively replacing or writing the file.
            let mut file = File::open(&target)?;
            let before = file.metadata()?;
            let mut bytes = Vec::with_capacity(before.len().try_into().unwrap_or(0));
            file.read_to_end(&mut bytes)?;
            let after = file.metadata()?;
            if metadata_matches(&before, &after) && bytes.len() as u64 == after.len() {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    return Ok(Self {
                        target,
                        bytes,
                        modified: after.modified().ok(),
                        readonly: after.permissions().readonly(),
                        device: after.dev(),
                        inode: after.ino(),
                        mode: after.mode(),
                    });
                }

                #[cfg(not(unix))]
                {
                    return Ok(Self {
                        target,
                        bytes,
                        modified: after.modified().ok(),
                        readonly: after.permissions().readonly(),
                    });
                }
            }
        }
        Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "file changed repeatedly while it was being inspected",
        ))
    }

    fn metadata_still_matches(&self, metadata: &fs::Metadata) -> bool {
        let portable = metadata.len() == self.bytes.len() as u64
            && metadata.modified().ok() == self.modified
            && metadata.permissions().readonly() == self.readonly;

        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            portable
                && metadata.dev() == self.device
                && metadata.ino() == self.inode
                && metadata.mode() == self.mode
        }

        #[cfg(not(unix))]
        {
            portable
        }
    }
}

fn metadata_matches(before: &fs::Metadata, after: &fs::Metadata) -> bool {
    let portable = before.len() == after.len()
        && before.modified().ok() == after.modified().ok()
        && before.permissions().readonly() == after.permissions().readonly();

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        portable
            && before.dev() == after.dev()
            && before.ino() == after.ino()
            && before.mode() == after.mode()
    }

    #[cfg(not(unix))]
    {
        portable
    }
}

/// An editable UTF-8 document with bounded branching undo history.
#[derive(Clone, Debug)]
pub struct Buffer {
    state: TextState,
    clean_state: TextState,
    path: Option<PathBuf>,
    baseline: Option<DiskState>,
    dirty: bool,
    version: u64,
    revision: u64,
    history: UndoHistory,
    transaction: Option<Transaction>,
}

impl Default for Buffer {
    fn default() -> Self {
        Self::new()
    }
}

impl Buffer {
    /// Creates an empty, clean scratch buffer.
    pub fn new() -> Self {
        let state = TextState::empty();
        Self {
            clean_state: state.clone(),
            history: UndoHistory::new(),
            state,
            path: None,
            baseline: None,
            dirty: false,
            version: 0,
            revision: 0,
            transaction: None,
        }
    }

    /// Creates a clean scratch buffer containing `text`.
    pub fn from_text(text: impl AsRef<str>) -> Self {
        let state = TextState::parse(text.as_ref());
        Self {
            clean_state: state.clone(),
            history: UndoHistory::new(),
            state,
            path: None,
            baseline: None,
            dirty: false,
            version: 0,
            revision: 0,
            transaction: None,
        }
    }

    /// Creates an unnamed buffer whose nonempty contents have not been saved.
    ///
    /// This is used for standard input and other imported scratch content so
    /// normal quit checks and crash recovery cannot mistake it for disposable
    /// empty scratch state.
    pub fn from_unsaved_text(text: impl AsRef<str>) -> Self {
        let state = TextState::parse(text.as_ref());
        let clean_state = TextState::empty();
        let dirty = state != clean_state;
        Self {
            clean_state,
            history: UndoHistory::new(),
            state,
            path: None,
            baseline: None,
            dirty,
            version: 0,
            revision: 0,
            transaction: None,
        }
    }

    /// Creates an empty buffer associated with a path that does not yet exist.
    ///
    /// The path is retained so a normal write creates it atomically. Existing
    /// destinations are rejected to avoid accidentally treating them as new.
    pub fn new_file(path: impl AsRef<Path>) -> Result<Self> {
        let path = absolute_path(path.as_ref())?;
        if path_exists(&path)? {
            return Err(BufferError::DestinationExists(path));
        }
        let mut buffer = Self::new();
        buffer.path = Some(path);
        Ok(buffer)
    }

    /// Loads a UTF-8 file without lossy decoding.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = absolute_path(path.as_ref())?;
        let disk = DiskState::capture(&path)?;
        let text = std::str::from_utf8(&disk.bytes).map_err(|error| BufferError::InvalidUtf8 {
            path: path.clone(),
            valid_up_to: error.valid_up_to(),
        })?;
        let state = TextState::parse(text);
        Ok(Self {
            clean_state: state.clone(),
            history: UndoHistory::new(),
            state,
            path: Some(path),
            baseline: Some(disk),
            dirty: false,
            version: 0,
            revision: 0,
            transaction: None,
        })
    }

    /// Alias for [`Buffer::open`].
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        Self::open(path)
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn line_count(&self) -> usize {
        self.state.lines.len()
    }

    pub fn lines(&self) -> &[String] {
        &self.state.lines
    }

    pub fn line(&self, line: usize) -> Option<&str> {
        self.state.lines.get(line).map(String::as_str)
    }

    pub fn grapheme_count(&self, line: usize) -> Option<usize> {
        self.state.grapheme_count(line)
    }

    /// Byte offset within a line, using its cached grapheme boundaries.
    pub fn grapheme_byte(&self, pos: Pos) -> Result<usize> {
        self.state.byte_in_line(pos)
    }

    /// Stable for an unchanged line, including when other lines are inserted.
    /// Different line contents and independently created buffers use new keys.
    pub fn line_cache_key(&self, line: usize) -> Option<u64> {
        self.state.line_info.get(line).map(|info| info.key)
    }

    /// Returns the exact current document text, including original separators.
    pub fn text(&self) -> String {
        self.state.to_text()
    }

    pub fn byte_len(&self) -> usize {
        self.state.byte_len()
    }

    pub fn is_empty(&self) -> bool {
        self.state.lines.len() == 1 && self.state.lines[0].is_empty()
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Whether the loaded on-disk target was marked read-only at the most
    /// recent open, save, or reload.
    pub fn is_read_only(&self) -> bool {
        self.baseline.as_ref().is_some_and(|disk| disk.readonly)
    }

    /// Performs a metadata-only external-change preflight.
    ///
    /// This is suitable for frequent UI polling: it avoids rereading every
    /// clean file when its identity, size, timestamp, and permissions are
    /// unchanged. A subsequent reload/save still performs the full byte-level
    /// integrity check.
    pub fn may_have_changed_on_disk(&self) -> Result<bool> {
        let path = self.path.as_ref().ok_or(BufferError::NoPath)?;
        let Some(baseline) = &self.baseline else {
            return path_exists(path).map_err(BufferError::Io);
        };
        let target = match fs::canonicalize(path) {
            Ok(target) => target,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(true),
            Err(error) => return Err(BufferError::Io(error)),
        };
        if target != baseline.target {
            return Ok(true);
        }
        match fs::metadata(target) {
            Ok(metadata) => Ok(!baseline.metadata_still_matches(&metadata)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
            Err(error) => Err(BufferError::Io(error)),
        }
    }

    /// Monotonic document version.  It advances once for every committed
    /// transaction, undo, redo, or content-changing reload.
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Monotonic live-text revision. Unlike [`Self::version`], this advances
    /// for each content mutation inside an open transaction so asynchronous
    /// consumers can reject stale snapshots while undo remains grouped.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// The dominant original line ending, used for newly created lines by
    /// helpers such as [`Buffer::split_line`].
    pub fn line_ending(&self) -> LineEnding {
        self.state.preferred_ending
    }

    /// Returns the exact separator following `line`, or `None` for the final
    /// unterminated line.
    pub fn line_ending_after(&self, line: usize) -> Option<LineEnding> {
        self.state.endings.get(line).copied()
    }

    pub fn has_final_newline(&self) -> bool {
        self.state.endings.len() == self.state.lines.len()
            || (self.state.lines.len() > 1 && self.state.lines.last().is_some_and(String::is_empty))
    }

    pub fn validate_pos(&self, pos: Pos) -> Result<()> {
        self.state.validate_pos(pos)
    }

    /// Clamps a position to an existing line and that line's grapheme count.
    pub fn clamp_pos(&self, pos: Pos) -> Pos {
        let line = pos.line.min(self.line_count() - 1);
        let candidate = Pos::new(line, pos.grapheme);
        if self.state.byte_in_line(candidate).is_ok() {
            return candidate;
        }
        Pos::new(line, self.grapheme_count(line).expect("line exists"))
    }

    pub fn line_start(&self, line: usize) -> Result<Pos> {
        if line < self.line_count() {
            Ok(Pos::new(line, 0))
        } else {
            Err(BufferError::InvalidPosition(Pos::new(line, 0)))
        }
    }

    pub fn line_end(&self, line: usize) -> Result<Pos> {
        self.grapheme_count(line)
            .map(|column| Pos::new(line, column))
            .ok_or(BufferError::InvalidPosition(Pos::new(line, 0)))
    }

    pub fn end_pos(&self) -> Pos {
        self.line_end(self.line_count() - 1)
            .expect("a buffer always has one line")
    }

    /// Returns the following grapheme position, crossing a line separator.
    pub fn next_pos(&self, pos: Pos) -> Result<Option<Pos>> {
        self.validate_pos(pos)?;
        let end = self.grapheme_count(pos.line).expect("validated line");
        if pos.grapheme < end {
            Ok(Some(Pos::new(pos.line, pos.grapheme + 1)))
        } else if pos.line + 1 < self.line_count() {
            Ok(Some(Pos::new(pos.line + 1, 0)))
        } else {
            Ok(None)
        }
    }

    /// Returns the preceding grapheme position, crossing a line separator.
    pub fn previous_pos(&self, pos: Pos) -> Result<Option<Pos>> {
        self.validate_pos(pos)?;
        if pos.grapheme > 0 {
            Ok(Some(Pos::new(pos.line, pos.grapheme - 1)))
        } else if pos.line > 0 {
            Ok(Some(self.line_end(pos.line - 1)?))
        } else {
            Ok(None)
        }
    }

    /// Moves vertically and clamps the requested grapheme column to the target
    /// line.  A pane can retain its own desired column across repeated calls.
    pub fn move_vertical(&self, pos: Pos, line_delta: isize, desired: usize) -> Result<Pos> {
        self.validate_pos(pos)?;
        let line = pos
            .line
            .saturating_add_signed(line_delta)
            .min(self.line_count() - 1);
        Ok(Pos::new(
            line,
            desired.min(self.grapheme_count(line).expect("line exists")),
        ))
    }

    /// Converts a grapheme position to a byte offset in the exact serialized
    /// document (CRLF separators therefore occupy two bytes).
    pub fn pos_to_byte(&self, pos: Pos) -> Result<ByteOffset> {
        let within_line = self.state.byte_in_line(pos)?;
        let prior_lines = (0..pos.line)
            .map(|line| self.state.lines[line].len() + self.state.endings[line].byte_len())
            .sum::<usize>();
        Ok(ByteOffset(prior_lines + within_line))
    }

    /// Converts an exact byte/grapheme boundary to a cursor position.
    pub fn byte_to_pos(&self, offset: ByteOffset) -> Result<Pos> {
        let mut base = 0;
        for (line_index, line) in self.state.lines.iter().enumerate() {
            let line_end = base + line.len();
            if offset.0 <= line_end {
                let relative = offset.0 - base;
                if relative == line.len() {
                    return Ok(Pos::new(line_index, line.graphemes(true).count()));
                }
                if line.is_char_boundary(relative)
                    && let Some(grapheme) = line
                        .grapheme_indices(true)
                        .position(|(byte, _)| byte == relative)
                {
                    return Ok(Pos::new(line_index, grapheme));
                }
                return Err(BufferError::InvalidByteOffset(offset));
            }

            if let Some(ending) = self.state.endings.get(line_index) {
                let next_base = line_end + ending.byte_len();
                if offset.0 < next_base {
                    return Err(BufferError::InvalidByteOffset(offset));
                }
                base = next_base;
            }
        }
        if offset.0 == base && self.state.endings.len() == self.state.lines.len() {
            // A loaded final newline has no phantom cursor line.  Treat the
            // serialized EOF immediately after it as an alias for the final
            // line's end.
            return Ok(self.end_pos());
        }
        Err(BufferError::InvalidByteOffset(offset))
    }

    pub fn pos_to_utf16(&self, pos: Pos) -> Result<Utf16Pos> {
        let byte = self.state.byte_in_line(pos)?;
        let code_unit = self.state.lines[pos.line][..byte].encode_utf16().count();
        Ok(Utf16Pos::new(pos.line, code_unit))
    }

    /// Converts an LSP UTF-16 position if it is also a grapheme boundary.
    pub fn utf16_to_pos(&self, pos: Utf16Pos) -> Result<Pos> {
        if pos.line == self.line_count()
            && pos.code_unit == 0
            && self.state.endings.len() == self.state.lines.len()
        {
            // LSP may describe EOF after a final newline as the start of an
            // otherwise invisible next line.
            return Ok(self.end_pos());
        }
        let Some(line) = self.state.lines.get(pos.line) else {
            return Err(BufferError::InvalidUtf16Position(pos));
        };
        let mut code_unit = 0;
        for (grapheme, text) in line.graphemes(true).enumerate() {
            if code_unit == pos.code_unit {
                return Ok(Pos::new(pos.line, grapheme));
            }
            code_unit += text.encode_utf16().count();
        }
        if code_unit == pos.code_unit {
            return Ok(Pos::new(pos.line, line.graphemes(true).count()));
        }
        Err(BufferError::InvalidUtf16Position(pos))
    }

    pub fn text_in_range(&self, range: TextRange) -> Result<String> {
        self.state.selected_text(range)
    }

    /// Starts a logical undo transaction.  Mutations remain visible
    /// immediately, but history and the document version advance only when it
    /// is committed.
    pub fn begin_transaction(&mut self) -> Result<()> {
        if self.transaction.is_some() {
            return Err(BufferError::TransactionInProgress);
        }
        self.transaction = Some(Transaction {
            before: self.state.signature(),
            changes: Vec::new(),
        });
        Ok(())
    }

    /// Commits the active transaction. Returns whether it changed exact text.
    pub fn commit_transaction(&mut self) -> Result<bool> {
        let transaction = self.transaction.take().ok_or(BufferError::NoTransaction)?;
        let changed = transaction.changed(&self.state);
        if changed {
            self.history.commit(transaction.changes);
            self.advance_version();
        }
        self.refresh_dirty();
        Ok(changed)
    }

    /// Rolls back the recorded byte edits, including contextual grapheme joins.
    pub fn rollback_transaction(&mut self) -> Result<()> {
        let transaction = self.transaction.take().ok_or(BufferError::NoTransaction)?;
        let changed = transaction.changed(&self.state);
        for change in transaction.changes.iter().rev() {
            change.apply(&mut self.state, false);
        }
        if changed {
            self.advance_revision();
        }
        self.refresh_dirty();
        Ok(())
    }

    pub fn in_transaction(&self) -> bool {
        self.transaction.is_some()
    }

    /// Inserts UTF-8 text and returns the position immediately after it.
    /// Newline sequences in `text` become the buffer's preferred line-ending
    /// style; separators already present in the buffer are left untouched.
    pub fn insert(&mut self, at: Pos, text: &str) -> Result<Pos> {
        self.replace(TextRange::new(at, at), text)
    }

    /// Deletes a half-open range and returns its exact original separators.
    pub fn delete(&mut self, range: TextRange) -> Result<String> {
        let (_, removed, change) = self.state.replace(range, "")?;
        if let Some(change) = change {
            self.finish_mutation(change);
        }
        Ok(removed)
    }

    /// Validates both endpoints before changing text; rollback data contains
    /// only the replaced bytes, even inside a multi-line transaction.
    pub fn replace(&mut self, range: TextRange, text: &str) -> Result<Pos> {
        let (cursor, _, change) = self.state.replace(range, text)?;
        if let Some(change) = change {
            self.finish_mutation(change);
        }
        Ok(cursor)
    }

    pub(crate) fn materialize_eof_line(&mut self) {
        if self.state.endings.len() == self.state.lines.len() {
            let change = Change::EofLine;
            change.apply(&mut self.state, true);
            self.finish_mutation(change);
        }
    }

    pub fn delete_grapheme_forward(&mut self, at: Pos) -> Result<Option<String>> {
        let Some(end) = self.next_pos(at)? else {
            return Ok(None);
        };
        self.delete(TextRange::new(at, end)).map(Some)
    }

    pub fn delete_grapheme_backward(&mut self, at: Pos) -> Result<Option<(Pos, String)>> {
        let Some(start) = self.previous_pos(at)? else {
            return Ok(None);
        };
        let deleted = self.delete(TextRange::new(start, at))?;
        Ok(Some((start, deleted)))
    }

    pub fn split_line(&mut self, at: Pos) -> Result<Pos> {
        let ending = self.state.preferred_ending.as_str();
        self.insert(at, ending)
    }

    pub fn join_with_next(&mut self, line: usize) -> Result<bool> {
        if line + 1 >= self.line_count() {
            return Ok(false);
        }
        let start = self.line_end(line)?;
        let end = Pos::new(line + 1, 0);
        self.delete(TextRange::new(start, end))?;
        Ok(true)
    }

    /// Replaces the complete buffer as one transaction.
    pub fn replace_all(&mut self, text: &str) -> Result<()> {
        let replacement = TextState::parse(text);
        if replacement != self.state {
            let before = std::mem::replace(&mut self.state, replacement);
            let after = self.state.clone();
            self.finish_mutation(Change::Whole {
                before: Box::new(before),
                after: Box::new(after),
            });
        }
        Ok(())
    }

    /// Converts all existing separators and selects the style for future lines.
    pub fn set_line_ending(&mut self, ending: LineEnding) -> Result<()> {
        if self.state.preferred_ending != ending
            || self.state.endings.iter().any(|current| *current != ending)
        {
            let change = Change::Endings {
                before: self.state.endings.clone(),
                preferred_before: self.state.preferred_ending,
                after: ending,
            };
            change.apply(&mut self.state, true);
            self.finish_mutation(change);
        }
        Ok(())
    }

    pub fn can_undo(&self) -> bool {
        self.history.can_undo()
    }

    pub fn can_redo(&self) -> bool {
        self.history.can_redo()
    }

    pub fn undo(&mut self) -> Result<bool> {
        if self.transaction.is_some() {
            return Err(BufferError::TransactionInProgress);
        }
        if !self.history.undo(&mut self.state) {
            return Ok(false);
        }
        self.advance_version();
        self.advance_revision();
        self.refresh_dirty();
        Ok(true)
    }

    /// Redoes the most recently visited branch.
    pub fn redo(&mut self) -> Result<bool> {
        if self.transaction.is_some() {
            return Err(BufferError::TransactionInProgress);
        }
        if !self.history.redo(&mut self.state) {
            return Ok(false);
        }
        self.advance_version();
        self.advance_revision();
        self.refresh_dirty();
        Ok(true)
    }

    pub fn redo_branch_count(&self) -> usize {
        self.history.redo_branch_count()
    }

    /// Selects a divergent redo branch by insertion order.
    pub fn redo_branch(&mut self, branch: usize) -> Result<bool> {
        if self.transaction.is_some() {
            return Err(BufferError::TransactionInProgress);
        }
        if !self.history.redo_branch(branch, &mut self.state) {
            return Ok(false);
        }
        self.advance_version();
        self.advance_revision();
        self.refresh_dirty();
        Ok(true)
    }

    /// Bounds undo-tree nodes. Values below two retain a root and one change.
    pub fn set_undo_limit(&mut self, limit: usize) -> Result<()> {
        if self.transaction.is_some() {
            return Err(BufferError::TransactionInProgress);
        }
        self.history.set_limit(limit);
        Ok(())
    }

    /// Bounds retained change arrays and their owned data in bytes. Tree-node
    /// and branch-index metadata is separately bounded by [`Self::set_undo_limit`].
    /// An individual change exceeding this budget stays applied but clears undo
    /// history. Active transactions retain rollback data until commit or rollback.
    pub fn set_undo_byte_limit(&mut self, limit: usize) -> Result<()> {
        if self.transaction.is_some() {
            return Err(BufferError::TransactionInProgress);
        }
        self.history.set_byte_limit(limit);
        Ok(())
    }

    /// Retained change storage, excluding the separately bounded tree metadata.
    pub fn undo_bytes(&self) -> usize {
        self.history.retained_bytes
    }
    pub fn undo_byte_limit(&self) -> usize {
        self.history.byte_limit
    }

    /// Saves to the associated path after checking the exact on-disk baseline.
    pub fn save(&mut self) -> Result<()> {
        let path = self.path.clone().ok_or(BufferError::NoPath)?;
        self.save_impl(path, false, false)
    }

    /// Explicitly overwrites an externally modified associated file.
    pub fn save_force(&mut self) -> Result<()> {
        let path = self.path.clone().ok_or(BufferError::NoPath)?;
        self.save_impl(path, true, false)
    }

    /// Saves under a new path.  An existing destination is rejected.
    pub fn save_as(&mut self, path: impl AsRef<Path>) -> Result<()> {
        let path = absolute_path(path.as_ref())?;
        self.save_impl(path, false, true)
    }

    /// Saves under a new path, explicitly allowing an existing destination.
    pub fn save_as_force(&mut self, path: impl AsRef<Path>) -> Result<()> {
        let path = absolute_path(path.as_ref())?;
        self.save_impl(path, true, true)
    }

    /// Reloads the associated file if the buffer is clean.
    pub fn reload(&mut self) -> Result<bool> {
        if self.dirty {
            return Err(BufferError::UnsavedChanges);
        }
        self.reload_force()
    }

    /// Discards local text and reloads the associated file.
    pub fn reload_force(&mut self) -> Result<bool> {
        if self.transaction.is_some() {
            return Err(BufferError::TransactionInProgress);
        }
        let path = self.path.clone().ok_or(BufferError::NoPath)?;
        let disk = DiskState::capture(&path)?;
        let text = std::str::from_utf8(&disk.bytes).map_err(|error| BufferError::InvalidUtf8 {
            path: path.clone(),
            valid_up_to: error.valid_up_to(),
        })?;
        let state = TextState::parse(text);
        let changed = state != self.state;
        self.clean_state = state.clone();
        self.state = state;
        self.history.reset();
        self.baseline = Some(disk);
        self.dirty = false;
        if changed {
            self.advance_version();
            self.advance_revision();
        }
        Ok(changed)
    }

    fn finish_mutation(&mut self, change: Change) {
        self.advance_revision();
        if let Some(transaction) = &mut self.transaction {
            transaction.changes.push(change);
            self.dirty = true;
        } else {
            self.history.commit(vec![change]);
            self.advance_version();
            self.refresh_dirty();
        }
    }

    fn advance_version(&mut self) {
        self.version = self.version.saturating_add(1);
    }

    fn advance_revision(&mut self) {
        self.revision = self.revision.saturating_add(1);
    }

    fn refresh_dirty(&mut self) {
        self.dirty = self.state != self.clean_state;
    }

    fn save_impl(&mut self, path: PathBuf, force: bool, save_as: bool) -> Result<()> {
        if self.transaction.is_some() {
            return Err(BufferError::TransactionInProgress);
        }

        let same_path = self.path.as_ref().is_some_and(|current| current == &path);
        let expected = if same_path {
            self.baseline.as_ref()
        } else {
            None
        };

        if !force {
            if let Some(expected) = expected {
                ensure_unchanged(&path, expected)?;
            } else if path_exists(&path)? {
                return Err(BufferError::DestinationExists(path));
            }
        }

        // A clean :write still validates the baseline, but needlessly replacing
        // the inode would only create risk and disturb filesystem observers.
        if same_path && self.baseline.is_some() && !self.dirty && !force && !save_as {
            return Ok(());
        }

        let existed = path_exists(&path)?;
        let target = resolve_write_target(&path, existed)?;
        let permissions = if existed {
            Some(fs::metadata(&target)?.permissions())
        } else {
            None
        };
        let bytes = self.state.to_bytes();
        atomic_write(
            &path,
            &target,
            &bytes,
            permissions,
            if force { None } else { expected },
            !existed,
        )?;

        let disk = DiskState::capture(&path)?;
        self.path = Some(path);
        self.baseline = Some(disk);
        self.clean_state = self.state.clone();
        self.dirty = false;
        Ok(())
    }
}

fn absolute_path(path: &Path) -> io::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_owned())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn path_exists(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn ensure_unchanged(path: &Path, expected: &DiskState) -> Result<()> {
    match DiskState::capture(path) {
        Ok(current) if &current == expected => Ok(()),
        Ok(_) | Err(_) => Err(BufferError::ExternalModification(path.to_owned())),
    }
}

fn resolve_write_target(path: &Path, exists: bool) -> io::Result<PathBuf> {
    if exists {
        // Resolving the final symlink is essential: renaming over `path` would
        // replace the link rather than its target.
        fs::canonicalize(path)
    } else {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let file_name = path.file_name().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "save path has no file name")
        })?;
        Ok(fs::canonicalize(parent)?.join(file_name))
    }
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempPath {
    path: PathBuf,
}

impl Drop for TempPath {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn create_temp(target: &Path, private_creation_mode: bool) -> io::Result<(File, TempPath)> {
    let directory = target.parent().unwrap_or_else(|| Path::new("."));
    let stem = target
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("buffer");
    for _ in 0..128 {
        let unique = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let name = format!(".{stem}.editor-save-{}-{unique}", std::process::id());
        let path = directory.join(name);
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        if private_creation_mode {
            use std::os::unix::fs::OpenOptionsExt;
            // Existing private files must never briefly appear in the staging
            // name with a broader default mode before their mode is restored.
            options.mode(0o600);
        }
        #[cfg(not(unix))]
        let _ = private_creation_mode;
        match options.open(&path) {
            Ok(file) => return Ok((file, TempPath { path })),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique atomic-save temporary file",
    ))
}

fn atomic_write(
    requested_path: &Path,
    target: &Path,
    bytes: &[u8],
    permissions: Option<Permissions>,
    expected: Option<&DiskState>,
    create_without_overwrite: bool,
) -> Result<()> {
    let (mut file, temporary) = create_temp(target, permissions.is_some())?;
    if let Some(permissions) = permissions {
        file.set_permissions(permissions)?;
    }
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);

    // Repeat the external-change check immediately before publication.  The
    // final rename is atomic on the target filesystem.
    if let Some(expected) = expected {
        ensure_unchanged(requested_path, expected)?;
    }

    if create_without_overwrite {
        // `hard_link` gives a portable no-clobber publication within the same
        // directory; the fully written temp inode becomes visible atomically.
        fs::hard_link(&temporary.path, target).map_err(|error| {
            if error.kind() == io::ErrorKind::AlreadyExists {
                BufferError::DestinationExists(requested_path.to_owned())
            } else {
                BufferError::Io(error)
            }
        })?;
        // Remove the private staging name before syncing the directory; the
        // published target remains linked to the fully written inode.
        fs::remove_file(&temporary.path)?;
    } else {
        fs::rename(&temporary.path, target)?;
    }

    if let Some(directory) = target.parent() {
        // Directory fsync makes the rename/link durable on filesystems that
        // support it.  Some platforms reject directory handles, so Linux gets
        // the strong path while other targets can still save successfully.
        #[cfg(unix)]
        File::open(directory)?.sync_all()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn undo_memory_tracks_small_edits_independently_of_document_size() {
        fn history_bytes(lines: usize) -> usize {
            let mut buffer = Buffer::from_text("unchanged source line\n".repeat(lines));
            let initial = buffer.text();
            for _ in 0..100 {
                buffer.insert(Pos::ZERO, "x").unwrap();
            }
            let bytes = buffer.undo_bytes();
            assert!(bytes < 64 * 1024, "100 byte edits retained {bytes} bytes");
            for _ in 0..100 {
                assert!(buffer.undo().unwrap());
            }
            assert_eq!(buffer.text(), initial);
            assert!(!buffer.is_dirty());
            bytes
        }
        assert_eq!(history_bytes(1), history_bytes(20_000));
    }

    #[test]
    fn line_transform_records_only_changed_ranges_in_one_transaction() {
        let mut buffer = Buffer::from_text("source\r\n".repeat(2_000));
        let original = buffer.text();
        buffer.begin_transaction().unwrap();
        for line in 0..buffer.line_count() {
            buffer
                .replace(
                    TextRange::new(Pos::new(line, 0), Pos::new(line, 6)),
                    "    source",
                )
                .unwrap();
        }
        assert!(buffer.commit_transaction().unwrap());
        assert!(buffer.undo_bytes() < 1024 * 1024);
        assert_eq!(buffer.text(), "    source\r\n".repeat(2_000));
        assert!(buffer.undo().unwrap());
        assert_eq!(buffer.text(), original);
        assert!(!buffer.can_undo());
        assert!(buffer.redo().unwrap());
        assert_eq!(buffer.text(), "    source\r\n".repeat(2_000));
    }

    #[test]
    fn delta_history_restores_mixed_endings_and_contextual_graphemes() {
        let mut buffer = Buffer::from_text("a\r\nb🇺🇳\nc\r\n");
        let original = buffer.text();
        buffer.begin_transaction().unwrap();
        buffer.insert(Pos::new(0, 1), "\u{301}").unwrap();
        buffer
            .replace(TextRange::new(Pos::new(1, 1), Pos::new(2, 0)), "👩‍💻\nX")
            .unwrap();
        let changed = buffer.text();
        buffer.commit_transaction().unwrap();
        assert!(buffer.undo().unwrap());
        assert_eq!(buffer.text(), original);
        assert!(buffer.redo().unwrap());
        assert_eq!(buffer.text(), changed);

        buffer.begin_transaction().unwrap();
        buffer.replace_all("entirely different\n").unwrap();
        buffer.set_line_ending(LineEnding::Lf).unwrap();
        buffer.insert(Pos::ZERO, "prefix ").unwrap();
        buffer.rollback_transaction().unwrap();
        assert_eq!(buffer.text(), changed);
        assert!(buffer.undo().unwrap());
        assert_eq!(buffer.text(), original);
    }

    #[test]
    fn possible_equal_fingerprints_still_compare_exact_line_order() {
        let mut buffer = Buffer::from_text("left\nright");
        buffer.begin_transaction().unwrap();
        buffer.replace_all("right\nleft").unwrap();
        // The incremental rejection hash is insensitive to line ordering.
        assert_eq!(
            buffer.transaction.as_ref().unwrap().before,
            buffer.state.signature()
        );
        assert!(buffer.commit_transaction().unwrap());
        assert!(buffer.is_dirty());
        assert!(buffer.undo().unwrap());
        assert_eq!(buffer.text(), "left\nright");

        buffer.insert(Pos::ZERO, "dirty ").unwrap();
        buffer.begin_transaction().unwrap();
        let end = buffer.insert(Pos::ZERO, "temporary\r\n").unwrap();
        buffer.delete(TextRange::new(Pos::ZERO, end)).unwrap();
        let version = buffer.version();
        assert!(!buffer.commit_transaction().unwrap());
        assert_eq!(buffer.version(), version);
        assert_eq!(buffer.text(), "dirty left\nright");
        assert!(buffer.is_dirty());
    }

    #[test]
    fn undo_byte_limit_prunes_old_history_and_rejects_oversized_entries() {
        let mut buffer = Buffer::from_text("abc");
        buffer.insert(Pos::ZERO, "x").unwrap();
        let one_change = buffer.undo_bytes();
        buffer.set_undo_byte_limit(one_change).unwrap();
        buffer.insert(Pos::ZERO, "y").unwrap();
        assert!(buffer.undo_bytes() <= one_change);
        assert!(buffer.undo().unwrap());
        assert_eq!(buffer.text(), "xabc");
        assert!(!buffer.can_undo());

        buffer.begin_transaction().unwrap();
        buffer
            .insert(Pos::ZERO, &"z".repeat(one_change + 1))
            .unwrap();
        buffer.rollback_transaction().unwrap();
        assert_eq!(buffer.text(), "xabc");
        buffer
            .insert(Pos::ZERO, &"z".repeat(one_change + 1))
            .unwrap();
        assert_eq!(buffer.undo_bytes(), 0);
        assert!(!buffer.can_undo());
        assert!(!buffer.can_redo());
        assert!(buffer.text().ends_with("xabc"));

        buffer.set_undo_byte_limit(0).unwrap();
        buffer.insert(Pos::ZERO, "more").unwrap();
        assert_eq!(buffer.undo_bytes(), 0);
        assert!(!buffer.can_undo());
    }

    #[test]
    fn replacing_invalid_ranges_does_not_mutate_text_or_history() {
        let mut buffer = Buffer::from_text("a🇺🇳\r\nb");
        let original = buffer.text();
        assert!(
            buffer
                .replace(TextRange::new(Pos::ZERO, Pos::new(1, 2)), "x")
                .is_err()
        );
        assert_eq!(buffer.text(), original);
        assert_eq!(buffer.version(), 0);
        assert_eq!(buffer.revision(), 0);
        assert!(!buffer.can_undo());
    }

    #[test]
    fn replacement_uses_original_byte_boundaries_when_neighbors_can_join() {
        let mut buffer = Buffer::from_text("🇧b🇬\u{200d}");
        let original = buffer.text();
        let cursor = buffer
            .replace(TextRange::new(Pos::new(0, 1), Pos::new(0, 2)), "b")
            .unwrap();
        assert_eq!(buffer.text(), original);
        assert_eq!(cursor, Pos::new(0, 2));
        assert!(!buffer.can_undo());

        buffer
            .replace(TextRange::new(Pos::new(0, 1), Pos::new(0, 2)), "x")
            .unwrap();
        assert_eq!(buffer.text(), "🇧x🇬\u{200d}");
        assert!(buffer.undo().unwrap());
        assert_eq!(buffer.text(), original);
    }

    #[test]
    fn rollback_preserves_literal_cr_separately_from_line_endings() {
        let mut buffer = Buffer::from_text("\nb");
        buffer.begin_transaction().unwrap();
        buffer.insert(Pos::ZERO, "a\r").unwrap();
        buffer
            .delete(TextRange::new(Pos::ZERO, Pos::new(1, 0)))
            .unwrap();
        buffer.rollback_transaction().unwrap();
        assert_eq!(buffer.lines(), &["", "b"]);
        assert_eq!(buffer.line_ending_after(0), Some(LineEnding::Lf));
        assert_eq!(buffer.text(), "\nb");

        buffer.begin_transaction().unwrap();
        buffer.insert(Pos::ZERO, "a\r").unwrap();
        buffer
            .delete(TextRange::new(Pos::ZERO, Pos::new(1, 0)))
            .unwrap();
        buffer.commit_transaction().unwrap();
        assert!(buffer.undo().unwrap());
        assert_eq!(buffer.lines(), &["", "b"]);
        assert!(buffer.redo().unwrap());
        assert_eq!(buffer.text(), "b");

        let mut buffer = Buffer::from_text("\nb");
        buffer.insert(Pos::ZERO, "a\r").unwrap();
        let serialized = buffer.text();
        buffer.begin_transaction().unwrap();
        buffer
            .replace(TextRange::new(Pos::ZERO, Pos::new(1, 0)), "a\r\n")
            .unwrap();
        assert_eq!(buffer.text(), "a\nb");
        assert!(buffer.commit_transaction().unwrap());
        assert!(buffer.undo().unwrap());
        assert_eq!(buffer.text(), serialized);
        assert_eq!(buffer.line(0), Some("a\r"));
        assert_eq!(buffer.line_ending_after(0), Some(LineEnding::Lf));
    }

    #[test]
    fn line_caches_survive_unrelated_edits_and_invalidate_unicode_changes() {
        let mut buffer = Buffer::from_text("a🇺🇳\nunchanged");
        assert_eq!(buffer.grapheme_byte(Pos::new(0, 2)).unwrap(), 9);
        let first = buffer.line_cache_key(0);
        let second = buffer.line_cache_key(1);
        buffer.insert(Pos::new(0, 1), "\u{301}").unwrap();
        assert_ne!(buffer.line_cache_key(0), first);
        assert_eq!(buffer.line_cache_key(1), second);
        assert_eq!(buffer.grapheme_count(0), Some(2));
        assert_eq!(buffer.grapheme_byte(Pos::new(0, 1)).unwrap(), 3);
        buffer.split_line(Pos::new(0, 1)).unwrap();
        assert_eq!(buffer.line_cache_key(2), second);
        assert_eq!(buffer.grapheme_count(1), Some(1));
        assert_ne!(Buffer::from_text("unchanged").line_cache_key(0), second);

        let ascii = Buffer::from_text("a".repeat(1024 * 1024));
        assert_eq!(ascii.clamp_pos(Pos::ZERO), Pos::ZERO);
        assert!(ascii.state.line_info[0].graphemes.get().is_none());
        assert_eq!(ascii.grapheme_count(0), Some(1024 * 1024));
        assert!(matches!(
            ascii.state.line_info[0].graphemes.get(),
            Some(GraphemeIndex::Ascii)
        ));

        let mut unicode = Buffer::from_text("e\u{301}".repeat(100_000));
        let cursor = unicode.insert(Pos::ZERO, "x").unwrap();
        assert_eq!(cursor, Pos::new(0, 1));
        assert_eq!(unicode.clamp_pos(cursor), cursor);
        assert_eq!(unicode.grapheme_byte(cursor).unwrap(), 1);
        assert!(unicode.state.line_info[0].graphemes.get().is_none());
    }

    #[test]
    fn typing_without_newlines_preserves_an_explicit_empty_buffer_style() {
        let mut buffer = Buffer::new();
        buffer.set_line_ending(LineEnding::Crlf).unwrap();
        buffer.insert(Pos::ZERO, "x").unwrap();
        assert_eq!(buffer.line_ending(), LineEnding::Crlf);
        buffer.split_line(Pos::new(0, 1)).unwrap();
        assert_eq!(buffer.text(), "x\r\n");
        assert!(buffer.undo().unwrap());
        assert_eq!(buffer.text(), "x");
        assert_eq!(buffer.line_ending(), LineEnding::Crlf);
    }

    #[test]
    fn deterministic_edit_sequences_round_trip_every_history_state() {
        let mut buffer = Buffer::from_text("a🇺🇳\r\ne\u{301}\n👩‍💻 end\r\n");
        let mut expected = vec![(buffer.text(), buffer.line_ending(), buffer.line_count())];
        let fragments = [
            "x",
            "\u{301}",
            "🇺🇳",
            "\n",
            "\r\n",
            "👩‍💻\nnext",
            "",
            " ",
            "\r",
            "a\r",
            "\r\r\n",
        ];
        let mut seed = 0x91a2_b3c4_d5e6_f708_u64;
        for step in 0..150 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let line = seed as usize % buffer.line_count();
            let count = buffer.grapheme_count(line).unwrap();
            let start = Pos::new(line, (seed >> 12) as usize % (count + 1));
            let end_line = (line + ((seed >> 24) as usize % 3)).min(buffer.line_count() - 1);
            let end_count = buffer.grapheme_count(end_line).unwrap();
            let end = Pos::new(end_line, (seed >> 36) as usize % (end_count + 1));
            buffer.begin_transaction().unwrap();
            match step % 11 {
                0 => buffer.set_line_ending(LineEnding::Lf).unwrap(),
                1 => buffer.set_line_ending(LineEnding::Crlf).unwrap(),
                2 => buffer
                    .replace_all(fragments[step % fragments.len()])
                    .unwrap(),
                _ => {
                    buffer
                        .replace(
                            TextRange::new(start, end),
                            fragments[step % fragments.len()],
                        )
                        .unwrap();
                    // Record a second edit at a position from the updated state.
                    let at = buffer.end_pos();
                    buffer
                        .insert(at, fragments[(step + 3) % fragments.len()])
                        .unwrap();
                }
            }
            if buffer.commit_transaction().unwrap() {
                expected.push((buffer.text(), buffer.line_ending(), buffer.line_count()));
            }
            assert_eq!(buffer.byte_len(), buffer.text().len());
            assert!(buffer.state.invariant_holds());
        }
        for state in expected[..expected.len() - 1].iter().rev() {
            assert!(buffer.undo().unwrap());
            assert_eq!(
                (buffer.text(), buffer.line_ending(), buffer.line_count()),
                *state
            );
            assert_eq!(buffer.byte_len(), state.0.len());
        }
        assert!(!buffer.can_undo());
        assert!(!buffer.is_dirty());
        for state in expected.iter().skip(1) {
            assert!(buffer.redo().unwrap());
            assert_eq!(
                (buffer.text(), buffer.line_ending(), buffer.line_count()),
                *state
            );
            assert_eq!(buffer.byte_len(), state.0.len());
        }
        assert!(!buffer.can_redo());
    }

    #[test]
    fn parses_and_round_trips_mixed_line_endings() {
        let buffer = Buffer::from_text("one\r\ntwo\nthree\r\n");
        assert_eq!(buffer.lines(), &["one", "two", "three"]);
        assert_eq!(buffer.text(), "one\r\ntwo\nthree\r\n");
        assert!(buffer.has_final_newline());
        assert_eq!(buffer.line_ending(), LineEnding::Crlf);
        assert_eq!(buffer.line_ending_after(0), Some(LineEnding::Crlf));
        assert_eq!(buffer.line_ending_after(1), Some(LineEnding::Lf));
        assert_eq!(buffer.line_ending_after(2), Some(LineEnding::Crlf));
        assert_eq!(buffer.line_ending_after(3), None);
    }

    #[test]
    fn imported_scratch_text_requires_an_explicit_save_or_discard() {
        let buffer = Buffer::from_unsaved_text("from stdin\n");
        assert!(buffer.is_dirty());
        assert_eq!(buffer.path(), None);

        let empty = Buffer::from_unsaved_text("");
        assert!(!empty.is_dirty());
    }

    #[test]
    fn final_newline_does_not_create_a_phantom_loaded_line() {
        let mut buffer = Buffer::from_text("one\n");
        assert_eq!(buffer.line_count(), 1);
        assert_eq!(buffer.lines(), &["one"]);
        assert!(buffer.has_final_newline());
        assert_eq!(buffer.byte_len(), 4);
        assert_eq!(buffer.byte_to_pos(ByteOffset(4)).unwrap(), Pos::new(0, 3));
        assert_eq!(
            buffer.utf16_to_pos(Utf16Pos::new(1, 0)).unwrap(),
            Pos::new(0, 3)
        );

        let cursor = buffer.split_line(Pos::new(0, 3)).unwrap();
        assert_eq!(cursor, Pos::new(1, 0));
        assert_eq!(buffer.line_count(), 2);
        assert_eq!(buffer.text(), "one\n\n");
        assert!(buffer.has_final_newline());
    }

    #[test]
    fn grapheme_movement_never_splits_clusters() {
        let buffer = Buffer::from_text("a🇺🇳e\u{301}\n👩‍💻");
        assert_eq!(buffer.grapheme_count(0), Some(3));
        assert_eq!(buffer.grapheme_count(1), Some(1));

        let flag = Pos::new(0, 1);
        let after_flag = buffer.next_pos(flag).unwrap().unwrap();
        assert_eq!(after_flag, Pos::new(0, 2));
        let byte = buffer.pos_to_byte(after_flag).unwrap();
        assert_eq!(buffer.byte_to_pos(byte).unwrap(), after_flag);
        assert!(buffer.byte_to_pos(ByteOffset(2)).is_err());
    }

    #[test]
    fn unicode_insert_delete_and_undo_are_atomic() {
        let mut buffer = Buffer::from_text("ab");
        buffer.begin_transaction().unwrap();
        let cursor = buffer.insert(Pos::new(0, 1), "👩‍💻").unwrap();
        buffer.insert(cursor, "e\u{301}").unwrap();
        assert_eq!(buffer.version(), 0);
        assert_eq!(buffer.revision(), 2);
        assert!(buffer.commit_transaction().unwrap());
        assert_eq!(buffer.version(), 1);
        assert_eq!(buffer.revision(), 2);
        assert_eq!(buffer.text(), "a👩‍💻e\u{301}b");

        assert!(buffer.undo().unwrap());
        assert_eq!(buffer.revision(), 3);
        assert_eq!(buffer.text(), "ab");
        assert!(!buffer.is_dirty());
        assert!(buffer.redo().unwrap());
        assert_eq!(buffer.revision(), 4);

        let deleted = buffer
            .delete(TextRange::new(Pos::new(0, 1), Pos::new(0, 3)))
            .unwrap();
        assert_eq!(deleted, "👩‍💻e\u{301}");
        assert_eq!(buffer.text(), "ab");
        assert_eq!(buffer.revision(), 5);
    }

    #[test]
    fn transaction_that_returns_to_its_start_is_clean_and_version_stable() {
        let mut buffer = Buffer::new();
        buffer.begin_transaction().unwrap();
        let end = buffer.insert(Pos::ZERO, "x").unwrap();
        assert!(buffer.is_dirty());
        buffer.delete(TextRange::new(Pos::ZERO, end)).unwrap();

        assert!(!buffer.commit_transaction().unwrap());
        assert!(!buffer.is_dirty());
        assert_eq!(buffer.version(), 0);
        assert_eq!(buffer.revision(), 2);
        assert!(!buffer.can_undo());
    }

    #[test]
    fn rollback_gets_a_new_live_revision_without_committing_undo_history() {
        let mut buffer = Buffer::from_text("original");
        buffer.begin_transaction().unwrap();
        buffer.insert(Pos::new(0, 8), " change").unwrap();

        assert_eq!(buffer.version(), 0);
        assert_eq!(buffer.revision(), 1);
        buffer.rollback_transaction().unwrap();

        assert_eq!(buffer.text(), "original");
        assert_eq!(buffer.version(), 0);
        assert_eq!(buffer.revision(), 2);
        assert!(!buffer.can_undo());
    }

    #[test]
    fn insertion_cursor_accounts_for_contextual_grapheme_joining() {
        let mut buffer = Buffer::from_text("a");
        let cursor = buffer.insert(Pos::new(0, 1), "\u{301}").unwrap();
        assert_eq!(buffer.text(), "a\u{301}");
        assert_eq!(cursor, Pos::new(0, 1));
        buffer.validate_pos(cursor).unwrap();

        let mut buffer = Buffer::from_text("x\u{301}");
        let cursor = buffer.insert(Pos::ZERO, "a\n\u{301}").unwrap();
        buffer.validate_pos(cursor).unwrap();
    }

    #[test]
    fn multiline_edits_preserve_surrounding_separator() {
        let mut buffer = Buffer::from_text("left\r\nright\nlast");
        let cursor = buffer.insert(Pos::new(1, 2), "A\nB").unwrap();
        assert_eq!(cursor, Pos::new(2, 1));
        assert_eq!(buffer.text(), "left\r\nriA\r\nBght\nlast");

        let removed = buffer
            .delete(TextRange::new(Pos::new(0, 2), Pos::new(2, 1)))
            .unwrap();
        assert_eq!(removed, "ft\r\nriA\r\nB");
        assert_eq!(buffer.text(), "leght\nlast");
    }

    #[test]
    fn utf16_conversion_requires_grapheme_boundaries() {
        let buffer = Buffer::from_text("a🙂e\u{301}");
        assert_eq!(
            buffer.pos_to_utf16(Pos::new(0, 2)).unwrap(),
            Utf16Pos::new(0, 3)
        );
        assert_eq!(
            buffer.utf16_to_pos(Utf16Pos::new(0, 3)).unwrap(),
            Pos::new(0, 2)
        );
        assert!(buffer.utf16_to_pos(Utf16Pos::new(0, 2)).is_err());
        assert!(buffer.utf16_to_pos(Utf16Pos::new(0, 4)).is_err());
    }

    #[test]
    fn divergent_redo_branches_are_retained() {
        let mut buffer = Buffer::new();
        buffer.insert(Pos::ZERO, "a").unwrap();
        buffer.insert(Pos::new(0, 1), "b").unwrap();
        buffer.undo().unwrap();
        buffer.insert(Pos::new(0, 1), "c").unwrap();
        buffer.undo().unwrap();

        assert_eq!(buffer.redo_branch_count(), 2);
        buffer.redo_branch(0).unwrap();
        assert_eq!(buffer.text(), "ab");
        buffer.undo().unwrap();
        buffer.redo_branch(1).unwrap();
        assert_eq!(buffer.text(), "ac");
    }

    #[test]
    fn save_rejects_external_changes_and_preserves_crlf() {
        let directory = TestDirectory::new();
        let path = directory.path.join("source.rs");
        fs::write(&path, b"fn main() {}\r\n").unwrap();
        let mut buffer = Buffer::open(&path).unwrap();
        buffer.insert(Pos::new(0, 0), "// hi\r\n").unwrap();

        fs::write(&path, b"external\r\n").unwrap();
        assert!(matches!(
            buffer.save(),
            Err(BufferError::ExternalModification(_))
        ));
        assert_eq!(fs::read(&path).unwrap(), b"external\r\n");

        buffer.save_force().unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"// hi\r\nfn main() {}\r\n");
        assert!(!buffer.is_dirty());
    }

    #[cfg(unix)]
    #[test]
    fn save_follows_and_preserves_a_symlink() {
        use std::os::unix::fs::symlink;

        let directory = TestDirectory::new();
        let target = directory.path.join("real.txt");
        let link = directory.path.join("link.txt");
        fs::write(&target, "old").unwrap();
        symlink(&target, &link).unwrap();

        let mut buffer = Buffer::open(&link).unwrap();
        buffer.replace_all("new").unwrap();
        buffer.save().unwrap();

        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_to_string(&target).unwrap(), "new");
    }

    #[test]
    fn new_file_is_created_by_normal_save() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("new.rs");
        let mut buffer = Buffer::new_file(&path).unwrap();
        buffer.insert(Pos::ZERO, "fn main() {}\n").unwrap();
        buffer.save().unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "fn main() {}\n");
        assert!(!buffer.is_dirty());
    }

    #[test]
    fn metadata_preflight_avoids_clean_reads_and_detects_common_changes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("tracked.txt");
        fs::write(&path, "one").unwrap();
        let buffer = Buffer::open(&path).unwrap();
        assert!(!buffer.may_have_changed_on_disk().unwrap());

        fs::write(&path, "different length").unwrap();
        assert!(buffer.may_have_changed_on_disk().unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn save_preserves_file_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let directory = TestDirectory::new();
        let path = directory.path.join("mode.txt");
        fs::write(&path, "old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();

        let mut buffer = Buffer::open(&path).unwrap();
        buffer.replace_all("new").unwrap();
        buffer.save().unwrap();

        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }

    #[test]
    fn invalid_utf8_is_rejected_without_replacement() {
        let directory = TestDirectory::new();
        let path = directory.path.join("binary.dat");
        fs::write(&path, [b'a', 0xff, b'b']).unwrap();
        assert!(matches!(
            Buffer::open(&path),
            Err(BufferError::InvalidUtf8 { valid_up_to: 1, .. })
        ));
        assert_eq!(fs::read(&path).unwrap(), [b'a', 0xff, b'b']);
    }

    static TEST_DIRECTORY_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory {
        path: PathBuf,
    }

    impl TestDirectory {
        fn new() -> Self {
            let unique = TEST_DIRECTORY_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "editor-buffer-test-{}-{unique}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self { path }
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}
