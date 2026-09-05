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

#[derive(Clone, Debug, Eq, PartialEq)]
struct TextState {
    // `endings[n]` follows `lines[n]`.  There is an ending for every line
    // except an unterminated final line, so `endings.len()` is either
    // `lines.len() - 1` or `lines.len()`.  Keeping separators individually
    // lets mixed-ending files round-trip without exposing a phantom line for a
    // final newline.
    lines: Vec<String>,
    endings: Vec<LineEnding>,
    preferred_ending: LineEnding,
}

impl TextState {
    fn empty() -> Self {
        Self {
            lines: vec![String::new()],
            endings: Vec::new(),
            preferred_ending: LineEnding::Lf,
        }
    }

    fn parse(text: &str) -> Self {
        let mut state = Self::parse_for_insertion(text);
        if text.ends_with('\n') {
            // The final empty segment denotes termination of the preceding
            // line, not another line in a file loaded from disk.
            state.lines.pop();
        }
        debug_assert!(state.invariant_holds());
        state
    }

    /// Splits text for insertion, retaining the segment after its final line
    /// separator so inserting `"\n"` actually creates another logical line.
    fn parse_for_insertion(text: &str) -> Self {
        let mut lines = Vec::new();
        let mut endings = Vec::new();
        let mut start = 0;

        for (newline, _) in text.match_indices('\n') {
            let before_newline = &text[start..newline];
            if let Some(line) = before_newline.strip_suffix('\r') {
                lines.push(line.to_owned());
                endings.push(LineEnding::Crlf);
            } else {
                lines.push(before_newline.to_owned());
                endings.push(LineEnding::Lf);
            }
            start = newline + 1;
        }
        lines.push(text[start..].to_owned());

        let lf_count = endings
            .iter()
            .filter(|ending| **ending == LineEnding::Lf)
            .count();
        let crlf_count = endings.len() - lf_count;
        let preferred_ending = if crlf_count > lf_count {
            LineEnding::Crlf
        } else if crlf_count == lf_count && crlf_count > 0 {
            // On a tie, preserve the first style for newly inserted lines.
            endings[0]
        } else {
            LineEnding::Lf
        };

        Self {
            lines,
            endings,
            preferred_ending,
        }
    }

    fn invariant_holds(&self) -> bool {
        !self.lines.is_empty()
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
        let capacity = self.byte_len();
        let mut output = Vec::with_capacity(capacity);
        self.write_to(&mut output);
        output
    }

    fn byte_len(&self) -> usize {
        self.lines.iter().map(String::len).sum::<usize>()
            + self
                .endings
                .iter()
                .map(|ending| ending.byte_len())
                .sum::<usize>()
    }

    fn to_text(&self) -> String {
        // Every stored line and separator is valid UTF-8.
        String::from_utf8(self.to_bytes()).expect("TextState only contains UTF-8")
    }

    fn grapheme_count(&self, line: usize) -> Option<usize> {
        self.lines
            .get(line)
            .map(|line| line.graphemes(true).count())
    }

    fn validate_pos(&self, pos: Pos) -> Result<()> {
        match self.grapheme_count(pos.line) {
            Some(count) if pos.grapheme <= count => Ok(()),
            _ => Err(BufferError::InvalidPosition(pos)),
        }
    }

    fn byte_in_line(&self, pos: Pos) -> Result<usize> {
        self.validate_pos(pos)?;
        let line = &self.lines[pos.line];
        if pos.grapheme == line.graphemes(true).count() {
            Ok(line.len())
        } else {
            // Validation above proves this index exists.
            Ok(line
                .grapheme_indices(true)
                .nth(pos.grapheme)
                .map(|(byte, _)| byte)
                .expect("validated grapheme position"))
        }
    }

    fn grapheme_at_or_after_byte(line: &str, byte: usize) -> usize {
        debug_assert!(byte <= line.len());
        for (grapheme, (boundary, _)) in line.grapheme_indices(true).enumerate() {
            if boundary >= byte {
                return grapheme;
            }
        }
        line.graphemes(true).count()
    }

    fn selected_text(&self, range: TextRange) -> Result<String> {
        let range = range.ordered();
        self.validate_pos(range.start)?;
        self.validate_pos(range.end)?;
        if range.start > range.end {
            return Err(BufferError::InvalidRange(range));
        }

        let start_byte = self.byte_in_line(range.start)?;
        let end_byte = self.byte_in_line(range.end)?;
        if range.start.line == range.end.line {
            return Ok(self.lines[range.start.line][start_byte..end_byte].to_owned());
        }

        let mut selected = String::new();
        selected.push_str(&self.lines[range.start.line][start_byte..]);
        for line in range.start.line..range.end.line {
            selected.push_str(self.endings[line].as_str());
            if line + 1 < range.end.line {
                selected.push_str(&self.lines[line + 1]);
            }
        }
        selected.push_str(&self.lines[range.end.line][..end_byte]);
        Ok(selected)
    }

    fn insert(&mut self, at: Pos, text: &str) -> Result<Pos> {
        let byte = self.byte_in_line(at)?;
        if text.is_empty() {
            return Ok(at);
        }

        let mut inserted = Self::parse_for_insertion(text);
        let adopt_inserted_ending =
            self.endings.is_empty() && self.lines.len() == 1 && self.lines[0].is_empty();
        let inserted_preferred_ending = inserted.preferred_ending;
        let new_ending = if adopt_inserted_ending {
            inserted_preferred_ending
        } else {
            self.preferred_ending
        };
        // Inserted newlines are logical line breaks.  Normalize them to the
        // buffer's established style so typing or bracketed paste cannot
        // accidentally turn a CRLF file into a mixed-ending file.  Separators
        // that were loaded from disk remain individually preserved.
        inserted.endings.fill(new_ending);
        if inserted.lines.len() == 1 {
            self.lines[at.line].insert_str(byte, text);
            let column = Self::grapheme_at_or_after_byte(&self.lines[at.line], byte + text.len());
            return Ok(Pos::new(at.line, column));
        }

        let original = &self.lines[at.line];
        let prefix = &original[..byte];
        let suffix = &original[byte..];
        let mut replacement = inserted.lines;
        replacement[0].insert_str(0, prefix);
        replacement
            .last_mut()
            .expect("parsed text always has a line")
            .push_str(suffix);

        let inserted_line_count = replacement.len();
        let final_line = replacement.last().expect("replacement is nonempty");
        let inserted_end_byte = final_line.len() - suffix.len();
        let last_inserted_graphemes =
            Self::grapheme_at_or_after_byte(final_line, inserted_end_byte);

        self.lines.splice(at.line..=at.line, replacement);
        self.endings
            .splice(at.line..at.line, inserted.endings.iter().copied());
        if adopt_inserted_ending {
            self.preferred_ending = inserted_preferred_ending;
        }
        debug_assert!(self.invariant_holds());

        Ok(Pos::new(
            at.line + inserted_line_count - 1,
            last_inserted_graphemes,
        ))
    }

    fn delete(&mut self, range: TextRange) -> Result<String> {
        let range = range.ordered();
        self.validate_pos(range.start)?;
        self.validate_pos(range.end)?;
        let deleted = self.selected_text(range)?;
        if range.is_empty() {
            return Ok(deleted);
        }

        let start_byte = self.byte_in_line(range.start)?;
        let end_byte = self.byte_in_line(range.end)?;
        if range.start.line == range.end.line {
            self.lines[range.start.line].replace_range(start_byte..end_byte, "");
        } else {
            let mut merged = self.lines[range.start.line][..start_byte].to_owned();
            merged.push_str(&self.lines[range.end.line][end_byte..]);
            self.lines
                .splice(range.start.line..=range.end.line, [merged]);
            self.endings.drain(range.start.line..range.end.line);
        }
        debug_assert!(self.invariant_holds());
        Ok(deleted)
    }
}

#[derive(Clone, Debug)]
struct UndoNode {
    state: TextState,
    parent: Option<usize>,
    children: Vec<usize>,
    preferred_child: Option<usize>,
}

#[derive(Clone, Debug)]
struct UndoHistory {
    nodes: Vec<UndoNode>,
    current: usize,
    limit: usize,
}

impl UndoHistory {
    const DEFAULT_LIMIT: usize = 1_000;

    fn new(initial: TextState) -> Self {
        Self {
            nodes: vec![UndoNode {
                state: initial,
                parent: None,
                children: Vec::new(),
                preferred_child: None,
            }],
            current: 0,
            limit: Self::DEFAULT_LIMIT,
        }
    }

    fn reset(&mut self, state: TextState) {
        let limit = self.limit;
        *self = Self::new(state);
        self.limit = limit;
    }

    fn set_limit(&mut self, limit: usize, current_state: &TextState) {
        self.limit = limit.max(2);
        if self.nodes.len() > self.limit {
            self.reset(current_state.clone());
        }
    }

    fn commit(&mut self, state: TextState) {
        // Snapshot history is intentionally bounded.  When the tree reaches
        // the configured cap, retain the pre-change state as a new root so the
        // change being committed is still immediately undoable.
        if self.nodes.len() >= self.limit {
            let before = self.nodes[self.current].state.clone();
            self.reset(before);
        }

        let parent = self.current;
        let child = self.nodes.len();
        self.nodes.push(UndoNode {
            state,
            parent: Some(parent),
            children: Vec::new(),
            preferred_child: None,
        });
        self.nodes[parent].children.push(child);
        self.nodes[parent].preferred_child = Some(child);
        self.current = child;
    }

    fn undo(&mut self) -> Option<TextState> {
        let child = self.current;
        let parent = self.nodes[child].parent?;
        self.nodes[parent].preferred_child = Some(child);
        self.current = parent;
        Some(self.nodes[parent].state.clone())
    }

    fn redo(&mut self) -> Option<TextState> {
        let current = self.current;
        let child = self.nodes[current]
            .preferred_child
            .or_else(|| self.nodes[current].children.last().copied())?;
        self.current = child;
        Some(self.nodes[child].state.clone())
    }

    fn redo_branch(&mut self, branch: usize) -> Option<TextState> {
        let current = self.current;
        let child = *self.nodes[current].children.get(branch)?;
        self.nodes[current].preferred_child = Some(child);
        self.current = child;
        Some(self.nodes[child].state.clone())
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
    before: TextState,
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
            history: UndoHistory::new(state.clone()),
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
            history: UndoHistory::new(state.clone()),
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
            history: UndoHistory::new(state.clone()),
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
            history: UndoHistory::new(state.clone()),
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
        Pos::new(
            line,
            pos.grapheme
                .min(self.grapheme_count(line).expect("line exists")),
        )
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
            before: self.state.clone(),
        });
        Ok(())
    }

    /// Commits the active transaction.  Returns whether it changed state.
    pub fn commit_transaction(&mut self) -> Result<bool> {
        let transaction = self.transaction.take().ok_or(BufferError::NoTransaction)?;
        if transaction.before == self.state {
            self.refresh_dirty();
            return Ok(false);
        }
        self.history.commit(self.state.clone());
        self.advance_version();
        self.refresh_dirty();
        Ok(true)
    }

    /// Restores the state from before the active transaction without creating
    /// an undo entry.
    pub fn rollback_transaction(&mut self) -> Result<()> {
        let transaction = self.transaction.take().ok_or(BufferError::NoTransaction)?;
        let changed = self.state != transaction.before;
        self.state = transaction.before;
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
        self.state.validate_pos(at)?;
        if text.is_empty() {
            return Ok(at);
        }
        let position = self.state.insert(at, text)?;
        self.finish_mutation();
        Ok(position)
    }

    /// Deletes a half-open range and returns its exact text, including its
    /// original line separators.
    pub fn delete(&mut self, range: TextRange) -> Result<String> {
        let range = range.ordered();
        self.state.validate_pos(range.start)?;
        self.state.validate_pos(range.end)?;
        let deleted = self.state.delete(range)?;
        if !range.is_empty() {
            self.finish_mutation();
        }
        Ok(deleted)
    }

    /// Replaces a range as one undoable operation and returns the new cursor.
    pub fn replace(&mut self, range: TextRange, text: &str) -> Result<Pos> {
        let range = range.ordered();
        self.mutate(|state| {
            state.delete(range)?;
            state.insert(range.start, text)
        })
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
            self.state = replacement;
            self.finish_mutation();
        }
        Ok(())
    }

    /// Converts all existing separators and selects the style for future
    /// inserted lines.
    pub fn set_line_ending(&mut self, ending: LineEnding) -> Result<()> {
        if self.state.preferred_ending != ending
            || self.state.endings.iter().any(|current| *current != ending)
        {
            self.state.preferred_ending = ending;
            self.state.endings.fill(ending);
            self.finish_mutation();
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
        let Some(state) = self.history.undo() else {
            return Ok(false);
        };
        self.state = state;
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
        let Some(state) = self.history.redo() else {
            return Ok(false);
        };
        self.state = state;
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
        let Some(state) = self.history.redo_branch(branch) else {
            return Ok(false);
        };
        self.state = state;
        self.advance_version();
        self.advance_revision();
        self.refresh_dirty();
        Ok(true)
    }

    /// Bounds the number of in-memory snapshots.  Values below two are raised
    /// to two so the next change remains undoable.
    pub fn set_undo_limit(&mut self, limit: usize) -> Result<()> {
        if self.transaction.is_some() {
            return Err(BufferError::TransactionInProgress);
        }
        self.history.set_limit(limit, &self.state);
        Ok(())
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
        self.state = state.clone();
        self.clean_state = state.clone();
        self.history.reset(state);
        self.baseline = Some(disk);
        self.dirty = false;
        if changed {
            self.advance_version();
            self.advance_revision();
        }
        Ok(changed)
    }

    fn mutate<T>(&mut self, operation: impl FnOnce(&mut TextState) -> Result<T>) -> Result<T> {
        let before = self.state.clone();
        let result = match operation(&mut self.state) {
            Ok(result) => result,
            Err(error) => {
                self.state = before;
                return Err(error);
            }
        };
        if self.state != before {
            self.finish_mutation();
        }
        Ok(result)
    }

    fn finish_mutation(&mut self) {
        self.advance_revision();
        if self.transaction.is_none() {
            self.history.commit(self.state.clone());
            self.advance_version();
            self.refresh_dirty();
        } else {
            // During a transaction this is deliberately conservative. Commit
            // or rollback recomputes exact cleanliness once for the complete
            // logical edit, avoiding a full-buffer comparison per keystroke.
            self.dirty = true;
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
