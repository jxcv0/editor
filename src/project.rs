//! Project discovery, asynchronous file walking, and project-wide search.
//!
//! All filesystem traversal in this module happens on a background thread.  A
//! [`BackgroundTask`] deliberately exposes only non-blocking polling methods so
//! the terminal event loop cannot accidentally wait on a slow filesystem.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File};
use std::io::{self, Read};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering as AtomicOrdering};
use std::thread;
use std::time::Duration;

use crossbeam_channel::{Receiver, SendTimeoutError, Sender, bounded};
use ignore::{DirEntry, Walk, WalkBuilder};
use regex::{Regex, RegexBuilder};

const DEFAULT_CHANNEL_CAPACITY: usize = 128;
const SEND_RETRY_INTERVAL: Duration = Duration::from_millis(10);

/// Find the root that should own editor project state for `start`.
///
/// The lookup uses the following precedence:
///
/// 1. the closest containing Cargo workspace;
/// 2. the closest `Cargo.toml` when it is not in a workspace;
/// 3. the closest Git work tree;
/// 4. the supplied directory (or the supplied file's parent).
///
/// Returned paths are absolute and canonical when the platform permits it.
pub fn discover_project_root(start: impl AsRef<Path>) -> io::Result<PathBuf> {
    let start = absolute_path(start.as_ref())?;
    let start_dir = closest_existing_directory(&start)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no existing parent directory"))?;
    let start_dir = fs::canonicalize(&start_dir).unwrap_or(start_dir);

    let ancestors: Vec<PathBuf> = start_dir.ancestors().map(Path::to_path_buf).collect();
    let mut nearest_manifest = None;

    // The first workspace declaration encountered is the closest containing
    // workspace. An explicit `package.workspace` pointer can name a workspace
    // outside the lexical ancestor chain, so handle it here as well.
    for directory in &ancestors {
        let manifest = directory.join("Cargo.toml");
        if !manifest.is_file() {
            continue;
        }
        nearest_manifest.get_or_insert_with(|| directory.clone());

        if let Some(workspace) = manifest_workspace_pointer(&manifest) {
            let workspace = normalize_existing_path(workspace);
            if manifest_declares_workspace(&workspace.join("Cargo.toml")) {
                return Ok(workspace);
            }
        }

        if manifest_declares_workspace(&manifest) {
            return Ok(directory.clone());
        }
    }

    if let Some(directory) = nearest_manifest {
        return Ok(directory);
    }

    if let Some(directory) = ancestors
        .iter()
        .find(|directory| directory.join(".git").exists())
    {
        return Ok(directory.clone());
    }

    Ok(start_dir)
}

/// Discover a project beginning at the process working directory.
pub fn discover_project_root_from_current_dir() -> io::Result<PathBuf> {
    discover_project_root(std::env::current_dir()?)
}

fn absolute_path(path: &Path) -> io::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn closest_existing_directory(path: &Path) -> Option<PathBuf> {
    if path.is_dir() {
        return Some(path.to_path_buf());
    }
    if path.is_file() {
        return path.parent().map(Path::to_path_buf);
    }

    path.ancestors()
        .find(|parent| parent.is_dir())
        .map(Path::to_path_buf)
}

fn normalize_existing_path(path: PathBuf) -> PathBuf {
    fs::canonicalize(&path).unwrap_or(path)
}

fn manifest_declares_workspace(manifest: &Path) -> bool {
    let Ok(contents) = fs::read_to_string(manifest) else {
        return false;
    };

    contents.lines().any(|line| {
        let line = strip_toml_comment(line).trim();
        line == "[workspace]" || line.starts_with("[workspace.")
    })
}

fn manifest_workspace_pointer(manifest: &Path) -> Option<PathBuf> {
    let contents = fs::read_to_string(manifest).ok()?;
    let mut in_package = false;

    for line in contents.lines() {
        let line = strip_toml_comment(line).trim();
        if line.starts_with('[') {
            in_package = line == "[package]";
            continue;
        }
        if !in_package {
            continue;
        }

        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() != "workspace" {
            continue;
        }
        let value = value.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'))
            .or_else(|| {
                value
                    .strip_prefix('\'')
                    .and_then(|value| value.strip_suffix('\''))
            })?;
        return Some(manifest.parent()?.join(value));
    }

    None
}

/// Remove a TOML comment while retaining `#` characters inside quoted values.
fn strip_toml_comment(line: &str) -> &str {
    let mut quote = None;
    let mut escaped = false;
    for (index, character) in line.char_indices() {
        match quote {
            Some('"') if escaped => escaped = false,
            Some('"') if character == '\\' => escaped = true,
            Some(current) if character == current => quote = None,
            Some(_) => {}
            None if character == '"' || character == '\'' => quote = Some(character),
            None if character == '#' => return &line[..index],
            None => {}
        }
    }
    line
}

/// Cooperative cancellation shared between a task and its worker.
#[derive(Clone, Debug, Default)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, AtomicOrdering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(AtomicOrdering::Acquire)
    }
}

/// Observable state of a background task.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskStatus {
    Running,
    Complete,
    Cancelled,
}

impl TaskStatus {
    fn encode(self) -> u8 {
        match self {
            Self::Running => 0,
            Self::Complete => 1,
            Self::Cancelled => 2,
        }
    }

    fn decode(value: u8) -> Self {
        match value {
            1 => Self::Complete,
            2 => Self::Cancelled,
            _ => Self::Running,
        }
    }

    pub fn is_finished(self) -> bool {
        self != Self::Running
    }
}

/// Aggregate counters sent after a successful scan.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TaskSummary {
    pub entries_scanned: usize,
    pub files_scanned: usize,
    pub results_emitted: usize,
    pub errors: usize,
    /// Whether a configured result limit stopped the scan early.
    pub truncated: bool,
}

/// A recoverable error encountered while walking or reading one path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectError {
    pub path: Option<PathBuf>,
    pub message: String,
}

impl ProjectError {
    fn at(path: impl Into<PathBuf>, error: impl std::fmt::Display) -> Self {
        Self {
            path: Some(path.into()),
            message: error.to_string(),
        }
    }

    fn walk(error: ignore::Error) -> Self {
        Self {
            path: ignore_error_path(&error),
            message: error.to_string(),
        }
    }
}

fn ignore_error_path(error: &ignore::Error) -> Option<PathBuf> {
    match error {
        ignore::Error::Partial(errors) => errors.iter().find_map(ignore_error_path),
        ignore::Error::WithLineNumber { err, .. } | ignore::Error::WithDepth { err, .. } => {
            ignore_error_path(err)
        }
        ignore::Error::WithPath { path, .. } => Some(path.clone()),
        ignore::Error::Loop { child, .. } => Some(child.clone()),
        ignore::Error::Io(_)
        | ignore::Error::Glob { .. }
        | ignore::Error::UnrecognizedFileType(_)
        | ignore::Error::InvalidDefinition => None,
    }
}

/// Incremental output from a background project operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StreamEvent<T> {
    Item(T),
    Error(ProjectError),
    Finished(TaskSummary),
}

/// A bounded, cancellable background operation.
///
/// `try_recv` and `drain` never wait for the worker. Dropping the task requests
/// cancellation, including when the worker is currently applying channel
/// backpressure.
#[must_use = "dropping a background task cancels it"]
pub struct BackgroundTask<T> {
    receiver: Receiver<StreamEvent<T>>,
    cancellation: CancellationToken,
    status: Arc<AtomicU8>,
}

impl<T> BackgroundTask<T> {
    /// Return the next ready event, or `None` without blocking.
    pub fn try_recv(&self) -> Option<StreamEvent<T>> {
        self.receiver.try_recv().ok()
    }

    /// Drain at most `limit` currently ready events without blocking.
    pub fn drain(&self, limit: usize) -> Vec<StreamEvent<T>> {
        let mut events = Vec::with_capacity(limit.min(self.receiver.len()));
        for _ in 0..limit {
            let Some(event) = self.try_recv() else {
                break;
            };
            events.push(event);
        }
        events
    }

    pub fn cancel(&self) {
        self.cancellation.cancel();
    }

    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    pub fn status(&self) -> TaskStatus {
        TaskStatus::decode(self.status.load(AtomicOrdering::Acquire))
    }

    pub fn is_finished(&self) -> bool {
        self.status().is_finished()
    }

    /// Number of events currently buffered for immediate polling.
    pub fn ready_len(&self) -> usize {
        self.receiver.len()
    }
}

impl<T> Drop for BackgroundTask<T> {
    fn drop(&mut self) {
        self.cancel();
    }
}

fn task_channel<T>(capacity: usize) -> (Sender<StreamEvent<T>>, BackgroundTask<T>) {
    let (sender, receiver) = bounded(capacity.max(1));
    let cancellation = CancellationToken::new();
    let status = Arc::new(AtomicU8::new(TaskStatus::Running.encode()));
    let task = BackgroundTask {
        receiver,
        cancellation,
        status,
    };
    (sender, task)
}

fn send_cancellable<T>(
    sender: &Sender<StreamEvent<T>>,
    cancellation: &CancellationToken,
    mut event: StreamEvent<T>,
) -> bool {
    loop {
        if cancellation.is_cancelled() {
            return false;
        }
        match sender.send_timeout(event, SEND_RETRY_INTERVAL) {
            Ok(()) => return true,
            Err(SendTimeoutError::Timeout(returned)) => event = returned,
            Err(SendTimeoutError::Disconnected(_)) => return false,
        }
    }
}

fn finish_task<T>(
    sender: &Sender<StreamEvent<T>>,
    cancellation: &CancellationToken,
    status: &AtomicU8,
    summary: TaskSummary,
) {
    let completed = !cancellation.is_cancelled()
        && send_cancellable(sender, cancellation, StreamEvent::Finished(summary));
    let final_status = if completed {
        TaskStatus::Complete
    } else {
        TaskStatus::Cancelled
    };
    status.store(final_status.encode(), AtomicOrdering::Release);
}

/// Controls project walking. Hidden paths and ignored paths are independent.
#[derive(Clone, Debug)]
pub struct ScanOptions {
    pub include_hidden: bool,
    pub include_ignored: bool,
    pub include_files: bool,
    pub include_directories: bool,
    pub follow_symlinks: bool,
    /// `None` recursively walks the whole project. A value of `1` lists only
    /// the root's direct children and is useful for a lazy tree explorer.
    pub max_depth: Option<usize>,
    /// `None` has no total result limit; channel memory remains bounded.
    pub max_results: Option<usize>,
    pub channel_capacity: usize,
    /// Sort each visited directory by path, making emission deterministic.
    pub sort_by_path: bool,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            include_hidden: false,
            include_ignored: false,
            include_files: true,
            include_directories: false,
            follow_symlinks: false,
            max_depth: None,
            max_results: None,
            channel_capacity: DEFAULT_CHANNEL_CAPACITY,
            sort_by_path: true,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProjectEntryKind {
    File,
    Directory,
    Symlink,
    Other,
}

/// One entry discovered below a project root.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectEntry {
    pub path: PathBuf,
    pub relative_path: PathBuf,
    pub kind: ProjectEntryKind,
    pub depth: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReferencePreview {
    pub path: PathBuf,
    pub line: usize,
    pub column: usize,
    pub text: String,
}

/// A bounded excerpt around an LSP UTF-16 column, also safe for open buffers.
pub fn reference_preview_text(line: Option<&str>, column: usize) -> String {
    let Some(line) = line else {
        return "[Line unavailable]".into();
    };
    let mut end = line.len().min(16 * 1024);
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    let characters = line[..end].chars().collect::<Vec<_>>();
    let mut units = 0;
    let mut at = 0;
    while at < characters.len() && units < column {
        units += characters[at].len_utf16();
        at += 1;
    }
    if units < column && end < line.len() {
        return "[Reference exceeds line preview limit]".into();
    }
    let start = at.saturating_sub(40);
    let stop = (start + 160).min(characters.len());
    let text: String = characters[start..stop].iter().collect();
    format!(
        "{}{}{}",
        if start > 0 { "…" } else { "" },
        text.trim().replace('\t', "    "),
        if stop < characters.len() || end < line.len() {
            "…"
        } else {
            ""
        },
    )
}

/// Read each referenced file once, keeping disk I/O off the foreground thread.
pub fn preview_references(
    locations: Vec<(PathBuf, usize, usize)>,
    max_file_bytes: usize,
) -> BackgroundTask<ReferencePreview> {
    let (sender, task) = task_channel(DEFAULT_CHANNEL_CAPACITY);
    let cancellation = task.cancellation_token();
    let status = Arc::clone(&task.status);
    thread::Builder::new()
        .name("reference-previews".into())
        .spawn(move || {
            let mut files = BTreeMap::<PathBuf, BTreeSet<(usize, usize)>>::new();
            for (path, line, column) in locations {
                files.entry(path).or_default().insert((line, column));
            }
            let mut summary = TaskSummary::default();
            'files: for (path, positions) in files {
                if cancellation.is_cancelled() {
                    break;
                }
                let (contents, error) =
                    match read_searchable_file(&path, max_file_bytes.min(8 * 1024 * 1024)) {
                        Ok(Some(text)) => (text, None),
                        Ok(None) => (
                            String::new(),
                            Some(
                                "[Preview unavailable: file too large or not UTF-8 text]"
                                    .to_owned(),
                            ),
                        ),
                        Err(error) => (
                            String::new(),
                            Some(format!("[Preview unavailable: {error}]")),
                        ),
                    };
                summary.files_scanned += 1;
                summary.errors += usize::from(error.is_some());
                let mut lines = contents.lines().enumerate().peekable();
                for (line, column) in positions {
                    if cancellation.is_cancelled() {
                        break 'files;
                    }
                    while lines.peek().is_some_and(|(number, _)| *number < line) {
                        lines.next();
                    }
                    let text = error.clone().unwrap_or_else(|| {
                        reference_preview_text(
                            lines
                                .peek()
                                .filter(|(number, _)| *number == line)
                                .map(|(_, text)| *text),
                            column,
                        )
                    });
                    let preview = ReferencePreview {
                        path: path.clone(),
                        line,
                        column,
                        text,
                    };
                    if !send_cancellable(&sender, &cancellation, StreamEvent::Item(preview)) {
                        break 'files;
                    }
                    summary.results_emitted += 1;
                }
            }
            finish_task(&sender, &cancellation, &status, summary);
        })
        .expect("failed to spawn reference preview worker");
    task
}

impl ProjectEntry {
    pub fn is_file(&self) -> bool {
        self.kind == ProjectEntryKind::File
    }

    pub fn is_directory(&self) -> bool {
        self.kind == ProjectEntryKind::Directory
    }
}

/// Recursively scan `root` on a bounded background task.
pub fn scan_project(root: impl AsRef<Path>, options: ScanOptions) -> BackgroundTask<ProjectEntry> {
    let root = make_absolute_lossy(root.as_ref());
    let (sender, task) = task_channel(options.channel_capacity);
    let cancellation = task.cancellation_token();
    let status = Arc::clone(&task.status);

    thread::Builder::new()
        .name("project-scan".into())
        .spawn(move || {
            let mut summary = TaskSummary::default();
            let walker = build_walker(&root, &options);

            for result in walker {
                if cancellation.is_cancelled() {
                    break;
                }
                let entry = match result {
                    Ok(entry) => entry,
                    Err(error) => {
                        summary.errors += 1;
                        if !send_cancellable(
                            &sender,
                            &cancellation,
                            StreamEvent::Error(ProjectError::walk(error)),
                        ) {
                            break;
                        }
                        continue;
                    }
                };
                if entry.depth() == 0 {
                    continue;
                }
                summary.entries_scanned += 1;

                let kind = entry_kind(&entry);
                if kind == ProjectEntryKind::File {
                    summary.files_scanned += 1;
                }
                let selected = match kind {
                    ProjectEntryKind::Directory => options.include_directories,
                    _ => options.include_files,
                };
                if !selected {
                    continue;
                }

                if options
                    .max_results
                    .is_some_and(|limit| summary.results_emitted >= limit)
                {
                    summary.truncated = true;
                    break;
                }

                let path = entry.path().to_path_buf();
                let relative_path = path.strip_prefix(&root).unwrap_or(&path).to_path_buf();
                let result = ProjectEntry {
                    path,
                    relative_path,
                    kind,
                    depth: entry.depth(),
                };
                if !send_cancellable(&sender, &cancellation, StreamEvent::Item(result)) {
                    break;
                }
                summary.results_emitted += 1;
            }

            finish_task(&sender, &cancellation, &status, summary);
        })
        .expect("failed to spawn project scan worker");

    task
}

/// List only the immediate children of `directory`, suitable for lazy loading.
pub fn scan_directory(
    directory: impl AsRef<Path>,
    mut options: ScanOptions,
) -> BackgroundTask<ProjectEntry> {
    options.max_depth = Some(1);
    options.include_directories = true;
    scan_project(directory, options)
}

fn make_absolute_lossy(path: &Path) -> PathBuf {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|directory| directory.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    fs::canonicalize(&path).unwrap_or(path)
}

fn build_walker(root: &Path, options: &ScanOptions) -> Walk {
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(!options.include_hidden)
        .ignore(!options.include_ignored)
        .git_ignore(!options.include_ignored)
        .git_global(!options.include_ignored)
        .git_exclude(!options.include_ignored)
        .parents(!options.include_ignored)
        // Project-local .gitignore files should work even in a copied source
        // tree without a .git directory.
        .require_git(false)
        .follow_links(options.follow_symlinks)
        .max_depth(options.max_depth);
    if options.sort_by_path {
        builder.sort_by_file_path(|left, right| left.cmp(right));
    }
    builder.build()
}

fn entry_kind(entry: &DirEntry) -> ProjectEntryKind {
    let Some(file_type) = entry.file_type() else {
        return ProjectEntryKind::Other;
    };
    if file_type.is_dir() {
        ProjectEntryKind::Directory
    } else if file_type.is_file() {
        ProjectEntryKind::File
    } else if file_type.is_symlink() {
        ProjectEntryKind::Symlink
    } else {
        ProjectEntryKind::Other
    }
}

/// A scored subsequence match. Positions are UTF-8 byte offsets into the
/// candidate and are suitable for highlighting matched characters.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FuzzyMatch {
    pub score: i64,
    pub positions: Vec<usize>,
}

/// Score `query` as a fuzzy subsequence of `candidate`.
///
/// Higher scores are better. Word/path boundaries, consecutive characters,
/// exact case, prefixes, and basename matches receive bonuses.
pub fn fuzzy_match(query: &str, candidate: &str) -> Option<FuzzyMatch> {
    if query.is_empty() {
        return Some(FuzzyMatch {
            score: 0,
            positions: Vec::new(),
        });
    }

    let query: Vec<char> = query.chars().collect();
    let candidate: Vec<(usize, char)> = candidate.char_indices().collect();
    if query.len() > candidate.len() {
        return None;
    }

    const IMPOSSIBLE: i64 = i64::MIN / 4;
    let width = candidate.len();
    let mut previous = vec![IMPOSSIBLE; width];
    let mut parents = vec![usize::MAX; query.len() * width];

    for (query_index, &needle) in query.iter().enumerate() {
        let mut current = vec![IMPOSSIBLE; width];
        let mut best_prefix = IMPOSSIBLE;
        let mut best_prefix_index = usize::MAX;

        for candidate_index in 0..width {
            if query_index > 0 && candidate_index >= 2 {
                let prior_index = candidate_index - 2;
                if previous[prior_index] != IMPOSSIBLE {
                    let value = previous[prior_index] + prior_index as i64;
                    if value > best_prefix {
                        best_prefix = value;
                        best_prefix_index = prior_index;
                    }
                }
            }

            let hay = candidate[candidate_index].1;
            if !chars_equal_folded(needle, hay) {
                continue;
            }

            let intrinsic = fuzzy_character_bonus(&candidate, candidate_index, needle);
            if query_index == 0 {
                current[candidate_index] = intrinsic - candidate_index as i64;
                continue;
            }

            let mut best = IMPOSSIBLE;
            let mut parent = usize::MAX;
            if candidate_index > 0 && previous[candidate_index - 1] != IMPOSSIBLE {
                best = previous[candidate_index - 1] + 15;
                parent = candidate_index - 1;
            }
            if best_prefix != IMPOSSIBLE {
                let separated = best_prefix + 1 - candidate_index as i64;
                if separated > best {
                    best = separated;
                    parent = best_prefix_index;
                }
            }
            if best != IMPOSSIBLE {
                current[candidate_index] = best + intrinsic;
                parents[query_index * width + candidate_index] = parent;
            }
        }
        previous = current;
    }

    let (mut candidate_index, mut score) = previous
        .iter()
        .enumerate()
        .filter(|(_, score)| **score != IMPOSSIBLE)
        .map(|(index, score)| {
            // Prefer compact matches instead of an equally scored match that
            // leaves a long unmatched suffix.
            (index, *score - ((width - index - 1) as i64 / 4))
        })
        .max_by_key(|(_, score)| *score)?;

    let mut character_positions = vec![0; query.len()];
    for query_index in (0..query.len()).rev() {
        character_positions[query_index] = candidate[candidate_index].0;
        if query_index > 0 {
            candidate_index = parents[query_index * width + candidate_index];
        }
    }

    let folded_query = query.iter().collect::<String>().to_lowercase();
    let folded_candidate = candidate
        .iter()
        .map(|(_, ch)| ch)
        .collect::<String>()
        .to_lowercase();
    let basename = candidate_basename(candidate.as_slice());
    if folded_candidate == folded_query {
        score += 80;
    } else if folded_candidate.starts_with(&folded_query) {
        score += 30;
    }
    if basename.to_lowercase() == folded_query {
        score += 50;
    }

    Some(FuzzyMatch {
        score,
        positions: character_positions,
    })
}

pub fn fuzzy_score(query: &str, candidate: &str) -> Option<i64> {
    fuzzy_match(query, candidate).map(|matched| matched.score)
}

fn chars_equal_folded(left: char, right: char) -> bool {
    left == right || left.to_lowercase().eq(right.to_lowercase())
}

fn fuzzy_character_bonus(candidate: &[(usize, char)], index: usize, query: char) -> i64 {
    let current = candidate[index].1;
    let mut score = 10;
    if current == query {
        score += 1;
    }
    if index == 0 {
        score += 20;
    } else {
        let previous = candidate[index - 1].1;
        if matches!(previous, '/' | '\\' | '_' | '-' | ' ' | '.') {
            score += 16;
        } else if previous.is_lowercase() && current.is_uppercase() {
            score += 8;
        }
    }
    score
}

fn candidate_basename(candidate: &[(usize, char)]) -> String {
    let start = candidate
        .iter()
        .rposition(|(_, character)| matches!(character, '/' | '\\'))
        .map_or(0, |index| index + 1);
    candidate[start..]
        .iter()
        .map(|(_, character)| character)
        .collect()
}

/// One path returned by [`rank_paths`] or [`find_project_files`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FuzzyPathMatch {
    pub path: PathBuf,
    pub relative_path: PathBuf,
    pub score: i64,
    pub positions: Vec<usize>,
}

/// Rank an in-memory set of paths, retaining only the best `limit` matches.
pub fn rank_paths<I, P>(query: &str, paths: I, limit: usize) -> Vec<FuzzyPathMatch>
where
    I: IntoIterator<Item = P>,
    P: AsRef<Path>,
{
    if limit == 0 {
        return Vec::new();
    }

    // Keep this helper bounded too: the vector remains sorted best-first and
    // never retains more than `limit` owned paths.
    let mut ranked = Vec::with_capacity(limit.min(1_024));
    for path in paths {
        let path = path.as_ref();
        let displayed = path.to_string_lossy();
        let Some(matched) = fuzzy_match(query, &displayed) else {
            continue;
        };
        let candidate = FuzzyPathMatch {
            path: path.to_path_buf(),
            relative_path: path.to_path_buf(),
            score: matched.score,
            positions: matched.positions,
        };
        let insertion = ranked.partition_point(|existing| {
            compare_fuzzy_paths(existing, &candidate) == Ordering::Less
        });
        if insertion < limit {
            ranked.insert(insertion, candidate);
            if ranked.len() > limit {
                ranked.pop();
            }
        }
    }
    ranked
}

fn compare_fuzzy_paths(left: &FuzzyPathMatch, right: &FuzzyPathMatch) -> Ordering {
    right
        .score
        .cmp(&left.score)
        .then_with(|| left.relative_path.cmp(&right.relative_path))
}

/// A ranked file from the cached project index. Only retained results own paths.
#[derive(Debug)]
pub struct RankedFile {
    pub path: PathBuf,
    pub relative_path: PathBuf,
    pub score: i64,
}

#[derive(Debug)]
pub struct FileFinderResult {
    pub generation: u64,
    pub query: String,
    pub files: Vec<RankedFile>,
}

/// Index changes apply in order. A sweep removes every path that was not
/// appended since the latest `BeginSweep`, so a background rescan can refresh
/// the index without emptying it first.
enum IndexChange {
    Reset(PathBuf),
    Append(Vec<PathBuf>),
    BeginSweep,
    Sweep,
}

#[derive(Default)]
struct FinderInbox {
    changes: Vec<IndexChange>,
    query: Option<Option<(String, usize)>>,
}

/// One worker owns the normalized path index and matcher scratch storage. Query
/// replacement is constant work on the caller: stale ranking observes the
/// generation counter, and only the newest queued query is retained.
pub struct FileFinder {
    inbox: Arc<std::sync::Mutex<FinderInbox>>,
    generation: Arc<std::sync::atomic::AtomicU64>,
    closing: Arc<AtomicBool>,
    wake: Sender<()>,
    results: Receiver<FileFinderResult>,
    worker: Option<thread::JoinHandle<()>>,
}

impl FileFinder {
    pub fn new(root: PathBuf) -> Self {
        let inbox = Arc::new(std::sync::Mutex::new(FinderInbox::default()));
        let generation = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let closing = Arc::new(AtomicBool::new(false));
        let (wake, wake_rx) = bounded(1);
        let (result_tx, results) = bounded(1);
        let stale_results = results.clone();
        let worker_inbox = Arc::clone(&inbox);
        let worker_generation = Arc::clone(&generation);
        let worker_closing = Arc::clone(&closing);
        let worker = thread::Builder::new()
            .name("project-file-rank".into())
            .spawn(move || {
                let mut root = root;
                let mut candidates: Vec<PathCandidate> = Vec::new();
                let mut positions: HashMap<PathBuf, usize> = HashMap::new();
                let mut mark = 0_u64;
                let mut query = None;
                let mut scratch = ScoreScratch::default();
                while wake_rx.recv().is_ok() {
                    if worker_closing.load(AtomicOrdering::Acquire) {
                        break;
                    }
                    let (updates, generation) = {
                        let mut inbox = worker_inbox.lock().unwrap_or_else(|e| e.into_inner());
                        let generation = worker_generation.load(AtomicOrdering::Acquire);
                        (std::mem::take(&mut *inbox), generation)
                    };
                    for change in updates.changes {
                        match change {
                            IndexChange::Reset(new_root) => {
                                root = new_root;
                                candidates.clear();
                                positions.clear();
                            }
                            IndexChange::Append(paths) => {
                                for path in paths {
                                    if worker_closing.load(AtomicOrdering::Acquire) {
                                        return;
                                    }
                                    // Rescans and saved files repeat known paths.
                                    if let Some(&position) = positions.get(&path) {
                                        candidates[position].mark = mark;
                                        continue;
                                    }
                                    positions.insert(path.clone(), candidates.len());
                                    let mut candidate = PathCandidate::new(path, &root);
                                    candidate.mark = mark;
                                    candidates.push(candidate);
                                }
                            }
                            IndexChange::BeginSweep => mark += 1,
                            IndexChange::Sweep => {
                                candidates.retain(|candidate| candidate.mark == mark);
                                positions = candidates
                                    .iter()
                                    .enumerate()
                                    .map(|(position, candidate)| (candidate.path.clone(), position))
                                    .collect();
                            }
                        }
                    }
                    if let Some(new_query) = updates.query {
                        query = new_query;
                    }
                    let Some((text, limit)) = &query else {
                        continue;
                    };
                    let cancelled = || {
                        worker_generation.load(AtomicOrdering::Acquire) != generation
                            || worker_closing.load(AtomicOrdering::Acquire)
                    };
                    let Some(files) =
                        rank_candidates(text, &candidates, *limit, &mut scratch, &cancelled)
                    else {
                        continue;
                    };
                    let result = FileFinderResult {
                        generation,
                        query: text.clone(),
                        files,
                    };
                    // A slow UI needs the newest result, not a queue of results
                    // for queries it has already replaced.
                    if let Err(crossbeam_channel::TrySendError::Full(result)) =
                        result_tx.try_send(result)
                    {
                        let _ = stale_results.try_recv();
                        let _ = result_tx.try_send(result);
                    }
                }
            })
            .expect("failed to spawn file-ranking worker");
        Self {
            inbox,
            generation,
            closing,
            wake,
            results,
            worker: Some(worker),
        }
    }

    pub fn reset(&self, root: PathBuf) {
        self.update(|inbox| {
            inbox.changes.clear();
            inbox.changes.push(IndexChange::Reset(root));
        });
    }

    /// Add paths to the index. Paths already indexed are not duplicated.
    pub fn append(&self, paths: Vec<PathBuf>) {
        if !paths.is_empty() {
            self.update(|inbox| match inbox.changes.last_mut() {
                Some(IndexChange::Append(pending)) => pending.extend(paths),
                _ => inbox.changes.push(IndexChange::Append(paths)),
            });
        }
    }

    /// Start a rescan: [`Self::sweep`] later removes every indexed path that
    /// was not appended again in between.
    pub fn begin_sweep(&self) {
        self.update(|inbox| inbox.changes.push(IndexChange::BeginSweep));
    }

    pub fn sweep(&self) {
        self.update(|inbox| inbox.changes.push(IndexChange::Sweep));
    }

    pub fn request(&self, query: String, limit: usize) {
        self.update(|inbox| inbox.query = Some(Some((query, limit))));
    }

    pub fn cancel(&self) {
        self.update(|inbox| inbox.query = Some(None));
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(AtomicOrdering::Acquire)
    }

    pub fn try_recv(&self) -> Option<FileFinderResult> {
        self.results.try_recv().ok()
    }

    fn update(&self, change: impl FnOnce(&mut FinderInbox)) {
        {
            let mut inbox = self.inbox.lock().unwrap_or_else(|e| e.into_inner());
            change(&mut inbox);
            self.generation.fetch_add(1, AtomicOrdering::AcqRel);
        }
        let _ = self.wake.try_send(());
    }
}

impl Drop for FileFinder {
    fn drop(&mut self) {
        self.closing.store(true, AtomicOrdering::Release);
        let _ = self.wake.try_send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct PathCandidate {
    /// Sweep generation in which the path was last seen.
    mark: u64,
    path: PathBuf,
    relative_path: PathBuf,
    characters: Vec<(usize, char)>,
    bonuses: Vec<i64>,
    folded: String,
    folded_basename: String,
}

impl PathCandidate {
    fn new(path: PathBuf, root: &Path) -> Self {
        let relative_path = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
        let text = relative_path.to_string_lossy();
        let characters: Vec<_> = text.char_indices().collect();
        let bonuses = (0..characters.len())
            .map(|index| fuzzy_character_bonus(&characters, index, characters[index].1) - 1)
            .collect();
        let folded = text.to_lowercase();
        let folded_basename = candidate_basename(&characters).to_lowercase();
        Self {
            mark: 0,
            path,
            relative_path,
            characters,
            bonuses,
            folded,
            folded_basename,
        }
    }
}

#[derive(Default)]
struct ScoreScratch {
    previous: Vec<i64>,
    current: Vec<i64>,
}

impl ScoreScratch {
    fn score(
        &mut self,
        query: &[char],
        folded_query: &str,
        candidate: &PathCandidate,
        cancelled: &impl Fn() -> bool,
    ) -> Option<i64> {
        if query.is_empty() {
            return Some(0);
        }
        let width = candidate.characters.len();
        if query.len() > width {
            return None;
        }
        // Most paths can be rejected without entering dynamic programming.
        let mut next = 0;
        for &needle in query {
            while next < width && !chars_equal_folded(needle, candidate.characters[next].1) {
                next += 1;
            }
            if next == width {
                return None;
            }
            next += 1;
        }
        const IMPOSSIBLE: i64 = i64::MIN / 4;
        self.previous.resize(width, IMPOSSIBLE);
        self.previous.fill(IMPOSSIBLE);
        self.current.resize(width, IMPOSSIBLE);
        for (query_index, &needle) in query.iter().enumerate() {
            if cancelled() {
                return None;
            }
            self.current.fill(IMPOSSIBLE);
            let mut best_prefix = IMPOSSIBLE;
            for index in 0..width {
                if query_index > 0 && index >= 2 && self.previous[index - 2] != IMPOSSIBLE {
                    best_prefix = best_prefix.max(self.previous[index - 2] + (index - 2) as i64);
                }
                let hay = candidate.characters[index].1;
                if !chars_equal_folded(needle, hay) {
                    continue;
                }
                let intrinsic = candidate.bonuses[index] + i64::from(needle == hay);
                if query_index == 0 {
                    self.current[index] = intrinsic - index as i64;
                    continue;
                }
                let mut best = IMPOSSIBLE;
                if index > 0 && self.previous[index - 1] != IMPOSSIBLE {
                    best = self.previous[index - 1] + 15;
                }
                if best_prefix != IMPOSSIBLE {
                    best = best.max(best_prefix + 1 - index as i64);
                }
                if best != IMPOSSIBLE {
                    self.current[index] = best + intrinsic;
                }
            }
            std::mem::swap(&mut self.previous, &mut self.current);
        }
        let mut score = self
            .previous
            .iter()
            .enumerate()
            .filter(|(_, score)| **score != IMPOSSIBLE)
            .map(|(index, score)| score - ((width - index - 1) as i64 / 4))
            .max()?;
        if candidate.folded == folded_query {
            score += 80;
        } else if candidate.folded.starts_with(folded_query) {
            score += 30;
        }
        if candidate.folded_basename == folded_query {
            score += 50;
        }
        Some(score)
    }
}

#[derive(Eq, PartialEq)]
struct RankedCandidate<'a> {
    index: usize,
    score: i64,
    path: &'a Path,
}

impl Ord for RankedCandidate<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        // The heap root is the worst retained match.
        other
            .score
            .cmp(&self.score)
            .then_with(|| self.path.cmp(other.path))
            .then_with(|| self.index.cmp(&other.index))
    }
}

impl PartialOrd for RankedCandidate<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn rank_candidates(
    text: &str,
    candidates: &[PathCandidate],
    limit: usize,
    scratch: &mut ScoreScratch,
    cancelled: &impl Fn() -> bool,
) -> Option<Vec<RankedFile>> {
    let query: Vec<_> = text.chars().collect();
    let folded_query = text.to_lowercase();
    let mut ranked = std::collections::BinaryHeap::with_capacity(limit.min(candidates.len()));
    if limit == 0 {
        return Some(Vec::new());
    }
    for (index, candidate) in candidates.iter().enumerate() {
        if cancelled() {
            return None;
        }
        let Some(score) = scratch.score(&query, &folded_query, candidate, cancelled) else {
            continue;
        };
        let found = RankedCandidate {
            index,
            score,
            path: &candidate.relative_path,
        };
        if ranked.len() < limit {
            ranked.push(found);
        } else if ranked.peek().is_some_and(|worst| found < *worst) {
            *ranked.peek_mut().expect("nonempty limited heap") = found;
        }
    }
    if cancelled() {
        return None;
    }
    Some(
        ranked
            .into_sorted_vec()
            .into_iter()
            .map(|matched| {
                let candidate = &candidates[matched.index];
                RankedFile {
                    path: candidate.path.clone(),
                    relative_path: candidate.relative_path.clone(),
                    score: matched.score,
                }
            })
            .collect(),
    )
}

#[derive(Clone, Debug)]
pub struct FileFindOptions {
    pub scan: ScanOptions,
    /// Maximum matching candidates emitted for this query.
    pub max_results: usize,
}

impl Default for FileFindOptions {
    fn default() -> Self {
        Self {
            scan: ScanOptions::default(),
            max_results: 512,
        }
    }
}

/// Walk and fuzzy-match project files incrementally.
///
/// Items are emitted in traversal order with a score; consumers can maintain a
/// small sorted top-N list as they arrive. Replacing a query should cancel and
/// drop its previous task before starting another one.
pub fn find_project_files(
    root: impl AsRef<Path>,
    query: impl Into<String>,
    mut options: FileFindOptions,
) -> BackgroundTask<FuzzyPathMatch> {
    let root = make_absolute_lossy(root.as_ref());
    let query = query.into();
    options.scan.include_files = true;
    options.scan.include_directories = false;
    options.scan.max_results = None;
    let (sender, task) = task_channel(options.scan.channel_capacity);
    let cancellation = task.cancellation_token();
    let status = Arc::clone(&task.status);

    thread::Builder::new()
        .name("project-find".into())
        .spawn(move || {
            let mut summary = TaskSummary::default();
            for result in build_walker(&root, &options.scan) {
                if cancellation.is_cancelled() {
                    break;
                }
                let entry = match result {
                    Ok(entry) => entry,
                    Err(error) => {
                        summary.errors += 1;
                        if !send_cancellable(
                            &sender,
                            &cancellation,
                            StreamEvent::Error(ProjectError::walk(error)),
                        ) {
                            break;
                        }
                        continue;
                    }
                };
                if entry.depth() == 0 {
                    continue;
                }
                summary.entries_scanned += 1;
                if entry_kind(&entry) != ProjectEntryKind::File {
                    continue;
                }
                summary.files_scanned += 1;
                let path = entry.path().to_path_buf();
                let relative_path = path.strip_prefix(&root).unwrap_or(&path).to_path_buf();
                let displayed = relative_path.to_string_lossy();
                let Some(matched) = fuzzy_match(&query, &displayed) else {
                    continue;
                };
                if summary.results_emitted >= options.max_results {
                    summary.truncated = true;
                    break;
                }
                let result = FuzzyPathMatch {
                    path,
                    relative_path,
                    score: matched.score,
                    positions: matched.positions,
                };
                if !send_cancellable(&sender, &cancellation, StreamEvent::Item(result)) {
                    break;
                }
                summary.results_emitted += 1;
            }
            finish_task(&sender, &cancellation, &status, summary);
        })
        .expect("failed to spawn project finder worker");

    task
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SearchMode {
    Literal,
    Regex,
}

/// Controls bounded project-wide text search.
#[derive(Clone, Debug)]
pub struct TextSearchOptions {
    pub scan: ScanOptions,
    pub mode: SearchMode,
    pub case_sensitive: bool,
    pub max_results: usize,
    /// Files larger than this are skipped, limiting per-worker memory.
    pub max_file_bytes: usize,
    /// Maximum number of Unicode scalar values retained in a result preview.
    pub max_preview_chars: usize,
}

impl Default for TextSearchOptions {
    fn default() -> Self {
        Self {
            scan: ScanOptions::default(),
            mode: SearchMode::Literal,
            case_sensitive: true,
            max_results: 1_000,
            max_file_bytes: 8 * 1024 * 1024,
            max_preview_chars: 400,
        }
    }
}

/// One textual match. Lines and columns are one-based. Byte ranges refer to
/// UTF-8 bytes and make highlighting unambiguous for non-ASCII text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TextSearchMatch {
    pub path: PathBuf,
    pub relative_path: PathBuf,
    pub line: usize,
    pub column: usize,
    pub match_bytes: Range<usize>,
    pub preview: String,
    pub preview_match_bytes: Range<usize>,
    pub preview_truncated: bool,
}

/// Search project text on a background thread. Invalid regex syntax is
/// delivered as stream events, alongside walking and file errors.
pub fn search_project(
    root: impl AsRef<Path>,
    query: impl Into<String>,
    mut options: TextSearchOptions,
) -> Result<BackgroundTask<TextSearchMatch>, regex::Error> {
    let query = query.into();
    let root = make_absolute_lossy(root.as_ref());
    options.scan.include_files = true;
    options.scan.include_directories = false;
    options.scan.max_results = None;
    let (sender, task) = task_channel(options.scan.channel_capacity);
    let cancellation = task.cancellation_token();
    let status = Arc::clone(&task.status);

    thread::Builder::new()
        .name("project-search".into())
        .spawn(move || {
            let mut summary = TaskSummary::default();
            let matcher = match compile_search_pattern(&query, options.mode, options.case_sensitive)
            {
                Ok(matcher) => matcher,
                Err(error) => {
                    summary.errors = 1;
                    send_cancellable(
                        &sender,
                        &cancellation,
                        StreamEvent::Error(ProjectError {
                            path: None,
                            message: format!("Invalid project search: {error}"),
                        }),
                    );
                    finish_task(&sender, &cancellation, &status, summary);
                    return;
                }
            };
            // An empty interactive query should not enumerate every position
            // in every file. It simply completes until the user types input.
            if query.is_empty() || options.max_results == 0 {
                finish_task(&sender, &cancellation, &status, summary);
                return;
            }

            'walk: for result in build_walker(&root, &options.scan) {
                if cancellation.is_cancelled() {
                    break;
                }
                let entry = match result {
                    Ok(entry) => entry,
                    Err(error) => {
                        summary.errors += 1;
                        if !send_cancellable(
                            &sender,
                            &cancellation,
                            StreamEvent::Error(ProjectError::walk(error)),
                        ) {
                            break;
                        }
                        continue;
                    }
                };
                if entry.depth() == 0 {
                    continue;
                }
                summary.entries_scanned += 1;
                if entry_kind(&entry) != ProjectEntryKind::File {
                    continue;
                }
                summary.files_scanned += 1;

                let path = entry.path();
                let contents = match read_searchable_file(path, options.max_file_bytes) {
                    Ok(Some(contents)) => contents,
                    Ok(None) => continue,
                    Err(error) => {
                        summary.errors += 1;
                        if !send_cancellable(
                            &sender,
                            &cancellation,
                            StreamEvent::Error(ProjectError::at(path, error)),
                        ) {
                            break;
                        }
                        continue;
                    }
                };
                let relative_path = path.strip_prefix(&root).unwrap_or(path).to_path_buf();

                for (line_index, line) in contents.lines().enumerate() {
                    if cancellation.is_cancelled() {
                        break 'walk;
                    }
                    for matched in matcher.find_iter(line) {
                        if summary.results_emitted >= options.max_results {
                            summary.truncated = true;
                            break 'walk;
                        }
                        let (preview, preview_match_bytes, preview_truncated) = make_preview(
                            line,
                            matched.start()..matched.end(),
                            options.max_preview_chars,
                        );
                        let result = TextSearchMatch {
                            path: path.to_path_buf(),
                            relative_path: relative_path.clone(),
                            line: line_index + 1,
                            column: line[..matched.start()].chars().count() + 1,
                            match_bytes: matched.start()..matched.end(),
                            preview,
                            preview_match_bytes,
                            preview_truncated,
                        };
                        if !send_cancellable(&sender, &cancellation, StreamEvent::Item(result)) {
                            break 'walk;
                        }
                        summary.results_emitted += 1;
                    }
                }
            }

            finish_task(&sender, &cancellation, &status, summary);
        })
        .expect("failed to spawn project text-search worker");

    Ok(task)
}

fn compile_search_pattern(
    query: &str,
    mode: SearchMode,
    case_sensitive: bool,
) -> Result<Regex, regex::Error> {
    let expression = match mode {
        SearchMode::Literal => regex::escape(query),
        SearchMode::Regex => query.to_owned(),
    };
    RegexBuilder::new(&expression)
        .case_insensitive(!case_sensitive)
        .build()
}

fn read_searchable_file(path: &Path, max_bytes: usize) -> io::Result<Option<String>> {
    let file = File::open(path)?;
    if file.metadata()?.len() > max_bytes as u64 {
        return Ok(None);
    }

    let mut bytes = Vec::new();
    file.take(max_bytes.saturating_add(1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > max_bytes || bytes.contains(&0) {
        return Ok(None);
    }
    match String::from_utf8(bytes) {
        Ok(contents) => Ok(Some(contents)),
        // Search never silently replaces invalid UTF-8.
        Err(_) => Ok(None),
    }
}

fn make_preview(
    line: &str,
    matched: Range<usize>,
    max_chars: usize,
) -> (String, Range<usize>, bool) {
    let max_chars = max_chars.max(1);
    let total_chars = line.chars().count();
    if total_chars <= max_chars {
        return (line.to_owned(), matched, false);
    }

    let match_start_char = line[..matched.start].chars().count();
    let match_end_char = line[..matched.end].chars().count();
    let match_chars = match_end_char.saturating_sub(match_start_char);
    let before = max_chars.saturating_sub(match_chars.min(max_chars)) / 3;
    let mut window_start = match_start_char.saturating_sub(before);
    let mut window_end = (window_start + max_chars).min(total_chars);
    if match_end_char <= total_chars && match_end_char > window_end {
        window_end = match_end_char.min(total_chars);
        window_start = window_end.saturating_sub(max_chars);
    }

    let start_byte = char_boundary(line, window_start);
    let end_byte = char_boundary(line, window_end);
    let prefix = window_start > 0;
    let suffix = window_end < total_chars;
    let mut preview = String::new();
    if prefix {
        preview.push('…');
    }
    let prefix_bytes = preview.len();
    preview.push_str(&line[start_byte..end_byte]);
    if suffix {
        preview.push('…');
    }

    let visible_match_start = matched.start.max(start_byte).min(end_byte);
    let visible_match_end = matched.end.max(start_byte).min(end_byte);
    let preview_start = prefix_bytes + visible_match_start - start_byte;
    let preview_end = prefix_bytes + visible_match_end - start_byte;
    (preview, preview_start..preview_end, true)
}

fn char_boundary(text: &str, character_index: usize) -> usize {
    text.char_indices()
        .nth(character_index)
        .map_or(text.len(), |(byte, _)| byte)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(name: &str) -> Self {
            let unique = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "editor-project-{name}-{}-{unique}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn mkdir(&self, relative: &str) -> PathBuf {
            let path = self.0.join(relative);
            fs::create_dir_all(&path).unwrap();
            path
        }

        fn write(&self, relative: &str, contents: &str) -> PathBuf {
            let path = self.0.join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&path, contents).unwrap();
            path
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn collect<T>(task: &BackgroundTask<T>) -> (Vec<T>, Option<TaskSummary>) {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut items = Vec::new();
        let mut finished = None;
        loop {
            for event in task.drain(256) {
                match event {
                    StreamEvent::Item(item) => items.push(item),
                    StreamEvent::Error(error) => panic!("unexpected task error: {error:?}"),
                    StreamEvent::Finished(summary) => finished = Some(summary),
                }
            }
            if task.is_finished() && task.ready_len() == 0 {
                break;
            }
            assert!(Instant::now() < deadline, "background task timed out");
            thread::sleep(Duration::from_millis(2));
        }
        (items, finished)
    }

    #[test]
    fn workspace_wins_over_nested_manifest_and_git_root() {
        let temp = TestDirectory::new("workspace-root");
        temp.mkdir(".git");
        temp.write("Cargo.toml", "[workspace]\nmembers = [\"crates/app\"]\n");
        temp.write(
            "crates/app/Cargo.toml",
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\n",
        );
        let source = temp.mkdir("crates/app/src");

        assert_eq!(discover_project_root(source).unwrap(), temp.path());
    }

    #[test]
    fn nearest_manifest_precedes_git_and_git_is_used_without_cargo() {
        let temp = TestDirectory::new("root-precedence");
        temp.mkdir(".git");
        temp.write(
            "nested/Cargo.toml",
            "[package]\nname = \"nested\"\nversion = \"0.1.0\"\n",
        );
        let source = temp.mkdir("nested/src");
        assert_eq!(
            discover_project_root(source).unwrap(),
            temp.path().join("nested")
        );

        let other = temp.mkdir("plain/deep");
        assert_eq!(discover_project_root(other).unwrap(), temp.path());

        // Exercise fallback directory selection directly because a test's
        // system temp directory may itself live below a repository marker.
        let fallback = TestDirectory::new("fallback");
        let deep = fallback.mkdir("a/b");
        assert_eq!(closest_existing_directory(&deep), Some(deep));
    }

    #[test]
    fn scan_respects_hidden_and_ignored_independently() {
        let temp = TestDirectory::new("ignore");
        temp.write(".gitignore", "ignored.txt\nignored-dir/\n");
        temp.write("visible.txt", "visible");
        temp.write("ignored.txt", "ignored");
        temp.write("ignored-dir/inside.txt", "ignored");
        temp.write(".hidden.txt", "hidden");

        let (default, _) = collect(&scan_project(temp.path(), ScanOptions::default()));
        let paths: Vec<_> = default.iter().map(|entry| &entry.relative_path).collect();
        assert!(paths.contains(&&PathBuf::from("visible.txt")));
        assert!(!paths.contains(&&PathBuf::from("ignored.txt")));
        assert!(!paths.contains(&&PathBuf::from("ignored-dir/inside.txt")));
        assert!(!paths.contains(&&PathBuf::from(".hidden.txt")));

        let include_hidden = ScanOptions {
            include_hidden: true,
            ..ScanOptions::default()
        };
        let (hidden, _) = collect(&scan_project(temp.path(), include_hidden));
        assert!(
            hidden
                .iter()
                .any(|entry| entry.relative_path == Path::new(".hidden.txt"))
        );
        assert!(
            !hidden
                .iter()
                .any(|entry| entry.relative_path == Path::new("ignored.txt"))
        );

        let include_ignored = ScanOptions {
            include_ignored: true,
            ..ScanOptions::default()
        };
        let (ignored, _) = collect(&scan_project(temp.path(), include_ignored));
        assert!(
            ignored
                .iter()
                .any(|entry| entry.relative_path == Path::new("ignored.txt"))
        );
        assert!(
            !ignored
                .iter()
                .any(|entry| entry.relative_path == Path::new(".hidden.txt"))
        );
    }

    #[test]
    fn reference_previews_read_target_lines_and_report_unavailable_files() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("code.rs");
        fs::write(
            &path,
            "fn first() {}\r\n\tuse_symbol();\r\n😀 second_symbol();\n",
        )
        .unwrap();
        let missing = directory.path().join("missing.rs");
        let (previews, summary) = collect(&preview_references(
            vec![
                (path.clone(), 2, 3),
                (path.clone(), 1, 1),
                (path.clone(), 1, 1),
                (path.clone(), 99, 0),
                (missing.clone(), 0, 0),
            ],
            1024,
        ));
        assert_eq!(previews.len(), 4);
        assert_eq!(
            previews.iter().find(|item| item.line == 1).unwrap().text,
            "use_symbol();"
        );
        assert_eq!(
            previews.iter().find(|item| item.line == 2).unwrap().text,
            "😀 second_symbol();"
        );
        assert_eq!(
            previews.iter().find(|item| item.line == 99).unwrap().text,
            "[Line unavailable]"
        );
        assert!(
            previews
                .iter()
                .find(|item| item.path == missing)
                .unwrap()
                .text
                .contains("Preview unavailable")
        );
        assert_eq!(summary.unwrap().files_scanned, 2);
        let (oversized, _) = collect(&preview_references(vec![(path.clone(), 1, 1)], 1));
        assert!(oversized[0].text.contains("too large"));
        fs::write(&path, b"invalid\xff").unwrap();
        let (binary, _) = collect(&preview_references(vec![(path, 0, 0)], 1024));
        assert!(binary[0].text.contains("not UTF-8"));
    }

    #[test]
    fn reference_excerpt_centers_on_utf16_columns_and_bounds_long_lines() {
        let text = format!("{}call_target(){}", "😀".repeat(100), "tail".repeat(100));
        let excerpt = reference_preview_text(Some(&text), 200);
        assert!(excerpt.starts_with('…'));
        assert!(excerpt.ends_with('…'));
        assert!(excerpt.contains("call_target()"));
        assert!(excerpt.chars().count() <= 162);
        assert!(
            reference_preview_text(Some(&"x".repeat(1024 * 1024)), 1024 * 1024 - 1)
                .contains("limit")
        );
    }

    #[test]
    fn cancellation_releases_a_backpressured_worker() {
        let temp = TestDirectory::new("cancel");
        for index in 0..100 {
            temp.write(&format!("file-{index:03}.txt"), "text");
        }
        let options = ScanOptions {
            channel_capacity: 1,
            ..ScanOptions::default()
        };
        let task = scan_project(temp.path(), options);
        let deadline = Instant::now() + Duration::from_secs(2);
        while task.ready_len() == 0 && Instant::now() < deadline {
            thread::yield_now();
        }
        task.cancel();
        while !task.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(task.status(), TaskStatus::Cancelled);
    }

    #[test]
    fn fuzzy_matching_rewards_boundaries_and_rejects_non_subsequences() {
        let boundary = fuzzy_score("fb", "src/foo_bar.rs").unwrap();
        let scattered = fuzzy_score("fb", "src/farawayboat.rs").unwrap();
        assert!(boundary > scattered);
        assert!(fuzzy_score("xyz", "src/main.rs").is_none());

        let exact = fuzzy_match("main.rs", "main.rs").unwrap();
        assert_eq!(exact.positions, vec![0, 1, 2, 3, 4, 5, 6]);

        let paths = [
            PathBuf::from("src/farawayboat.rs"),
            PathBuf::from("src/foo_bar.rs"),
            PathBuf::from("src/fizz_buzz.rs"),
        ];
        let ranked = rank_paths("fb", &paths, 2);
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].relative_path, PathBuf::from("src/foo_bar.rs"));
    }

    #[test]
    fn cached_ranking_preserves_fuzzy_scores_and_top_order() {
        let paths = [
            "src/farawayboat.rs",
            "src/foo_bar.rs",
            "src/fizz_buzz.rs",
            "src/main.rs",
            "src/Äpfel.rs",
            "src/İtem.rs",
            "src/日本語.rs",
        ];
        let candidates: Vec<_> = paths
            .iter()
            .map(|path| PathCandidate::new(PathBuf::from(path), Path::new("")))
            .collect();
        let mut scratch = ScoreScratch::default();
        for query in ["", "fb", "main.rs", "ä", "İt", "日本", "missing"] {
            let actual = rank_candidates(query, &candidates, 3, &mut scratch, &|| false).unwrap();
            let expected = rank_paths(query, paths, 3);
            assert_eq!(
                actual
                    .iter()
                    .map(|item| (&item.relative_path, item.score))
                    .collect::<Vec<_>>(),
                expected
                    .iter()
                    .map(|item| (&item.relative_path, item.score))
                    .collect::<Vec<_>>(),
                "query {query:?}",
            );
        }
    }

    #[test]
    fn ranking_cancels_before_scanning_the_rest_of_the_index() {
        let candidates: Vec<_> = (0..100)
            .map(|index| {
                PathCandidate::new(PathBuf::from(format!("src/file{index}.rs")), Path::new(""))
            })
            .collect();
        let checks = std::cell::Cell::new(0);
        let cancelled = || {
            checks.set(checks.get() + 1);
            checks.get() >= 10
        };
        assert!(
            rank_candidates(
                "file",
                &candidates,
                5,
                &mut ScoreScratch::default(),
                &cancelled
            )
            .is_none()
        );
        assert!(checks.get() <= 11);
    }

    fn current_finder_result(finder: &FileFinder) -> FileFinderResult {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(result) = finder.try_recv()
                && result.generation == finder.generation()
            {
                return result;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "file finder did not finish"
            );
            thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn finder_replaces_queued_results_and_uses_latest_query_and_index() {
        let finder = FileFinder::new(PathBuf::from("/project"));
        finder.append(vec![PathBuf::from("/project/old.rs")]);
        finder.request("old".into(), 10);
        // Leave the old result in the bounded channel to exercise replacement.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while finder.results.is_empty() {
            assert!(std::time::Instant::now() < deadline);
            thread::sleep(Duration::from_millis(1));
        }
        finder.request("unwanted".into(), 10);
        finder.request("new".into(), 10);
        finder.append(vec![PathBuf::from("/project/new.rs")]);
        let result = current_finder_result(&finder);
        assert_eq!(result.query, "new");
        assert_eq!(result.files.len(), 1);
        assert_eq!(result.files[0].relative_path, Path::new("new.rs"));

        finder.reset(PathBuf::from("/different"));
        finder.append(vec![PathBuf::from("/different/newer.rs")]);
        let result = current_finder_result(&finder);
        assert_eq!(result.files.len(), 1);
        assert_eq!(result.files[0].relative_path, Path::new("newer.rs"));
    }

    #[test]
    fn finder_ignores_repeated_paths_and_sweeps_paths_a_rescan_missed() {
        let finder = FileFinder::new(PathBuf::from("/project"));
        let names = |result: FileFinderResult| {
            let mut names = result
                .files
                .iter()
                .map(|file| file.relative_path.display().to_string())
                .collect::<Vec<_>>();
            names.sort();
            names
        };
        finder.append(vec![
            PathBuf::from("/project/kept.rs"),
            PathBuf::from("/project/removed.rs"),
        ]);
        finder.append(vec![PathBuf::from("/project/kept.rs")]);
        finder.request("rs".into(), 10);
        assert_eq!(
            names(current_finder_result(&finder)),
            ["kept.rs", "removed.rs"]
        );

        finder.begin_sweep();
        finder.append(vec![
            PathBuf::from("/project/kept.rs"),
            PathBuf::from("/project/added.rs"),
        ]);
        // The previous index stays searchable until the rescan completes.
        assert_eq!(
            names(current_finder_result(&finder)),
            ["added.rs", "kept.rs", "removed.rs"]
        );
        finder.sweep();
        assert_eq!(
            names(current_finder_result(&finder)),
            ["added.rs", "kept.rs"]
        );
        finder.append(vec![PathBuf::from("/project/removed.rs")]);
        assert_eq!(
            names(current_finder_result(&finder)),
            ["added.rs", "kept.rs", "removed.rs"]
        );
    }

    #[test]
    fn finder_index_updates_rerank_the_active_query() {
        let finder = FileFinder::new(PathBuf::from("/project"));
        finder.request("needle".into(), 10);
        assert!(current_finder_result(&finder).files.is_empty());
        finder.append(vec![PathBuf::from("/project/needle.rs")]);
        assert_eq!(current_finder_result(&finder).files.len(), 1);
        finder.cancel();
        finder.append(vec![PathBuf::from("/project/another_needle.rs")]);
        finder.request("needle".into(), 1);
        assert_eq!(current_finder_result(&finder).files.len(), 1);
    }

    #[test]
    fn invalid_regex_is_reported_by_the_background_search() {
        let temp = TestDirectory::new("invalid-regex");
        let task = search_project(
            temp.path(),
            "[",
            TextSearchOptions {
                mode: SearchMode::Regex,
                ..TextSearchOptions::default()
            },
        )
        .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            match task.try_recv() {
                Some(StreamEvent::Error(error)) => {
                    assert!(error.message.contains("Invalid project search"));
                    break;
                }
                Some(event) => panic!("expected regex error, got {event:?}"),
                None => {
                    assert!(std::time::Instant::now() < deadline);
                    thread::sleep(Duration::from_millis(1));
                }
            }
        }
    }

    #[test]
    fn text_search_streams_literal_and_regex_matches_with_limits() {
        let temp = TestDirectory::new("search");
        temp.write(".gitignore", "ignored.rs\n");
        temp.write("src/main.rs", "fn main() {\n    println!(\"héllo\");\n}\n");
        temp.write("ignored.rs", "hello from ignored\n");
        temp.write("binary.bin", "hello\0world");

        let options = TextSearchOptions {
            case_sensitive: false,
            ..TextSearchOptions::default()
        };
        let (literal, summary) = collect(&search_project(temp.path(), "HÉLLO", options).unwrap());
        assert_eq!(literal.len(), 1);
        assert_eq!(literal[0].relative_path, PathBuf::from("src/main.rs"));
        assert_eq!((literal[0].line, literal[0].column), (2, 15));
        assert_eq!(summary.unwrap().results_emitted, 1);

        let regex_options = TextSearchOptions {
            mode: SearchMode::Regex,
            max_results: 1,
            ..TextSearchOptions::default()
        };
        let (regex, summary) =
            collect(&search_project(temp.path(), r"(?:fn|println)!?", regex_options).unwrap());
        assert_eq!(regex.len(), 1);
        assert!(summary.unwrap().truncated);
    }

    #[test]
    fn long_search_previews_remain_bounded_and_highlightable() {
        let line = format!("{}needle{}", "a".repeat(500), "z".repeat(500));
        let start = 500;
        let end = start + "needle".len();
        let (preview, range, truncated) = make_preview(&line, start..end, 40);
        assert!(truncated);
        assert!(preview.chars().count() <= 42); // optional prefix and suffix ellipses
        assert_eq!(&preview[range], "needle");
    }
}
