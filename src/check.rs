//! `cargo check` runs for the watch panel.
//!
//! The runtime starts at most one supervised `cargo check --message-format=json`
//! child at a time. A worker thread parses compiler messages into compact
//! entries and streams them through a bounded queue, while the editor owns only
//! the renderable [`CheckPanel`] state. Dropping a [`CargoCheckTask`] terminates
//! the cargo process group.

use std::{
    collections::HashSet,
    io,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, Ordering},
        mpsc::TryRecvError,
    },
    thread,
    time::{Duration, Instant},
};

use crossbeam_channel::{Receiver, SendTimeoutError, Sender, bounded};
use serde_json::Value;

use crate::{
    config::ToolConfig,
    process::{OutputStream, ProcessEvent, ProcessLimits, ProcessSpec, SupervisedChild},
};

const EVENT_QUEUE_CAPACITY: usize = 256;
const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;
const MAX_ENTRIES: usize = 1_000;
const POLL_INTERVAL: Duration = Duration::from_millis(5);
const SEND_RETRY_INTERVAL: Duration = Duration::from_millis(20);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CheckLevel {
    Error,
    Warning,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CheckLocation {
    pub path: PathBuf,
    /// Zero-based line.
    pub line: usize,
    /// Zero-based column in Unicode scalar values, as rustc reports it.
    pub column: usize,
}

/// One compiler diagnostic, or one line of cargo's own output when `level` is
/// `None` (for example a manifest error).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CheckEntry {
    pub level: Option<CheckLevel>,
    /// Header text such as `error[E0308]: mismatched types`.
    pub title: String,
    pub location: Option<CheckLocation>,
    /// The primary span as reported, such as `src/main.rs:3:18`.
    pub origin: Option<String>,
    pub label: Option<String>,
    /// Child notes and help, such as `help: remove this`.
    pub notes: Vec<String>,
}

impl CheckEntry {
    fn output(line: String) -> Self {
        Self {
            level: None,
            title: line,
            location: None,
            origin: None,
            label: None,
            notes: Vec::new(),
        }
    }

    fn rank(&self) -> u8 {
        match self.level {
            Some(CheckLevel::Error) => 0,
            Some(CheckLevel::Warning) => 1,
            None => 2,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckEvent {
    Entry(CheckEntry),
    /// A cargo status line such as `Checking editor v0.1.0`.
    Progress(String),
    Finished {
        success: bool,
        code: Option<i32>,
    },
    Failed(String),
}

/// The command a run uses: `cargo check` with machine-readable diagnostics,
/// followed by the configured extra arguments.
pub fn cargo_check_spec(tool: &ToolConfig, root: &Path) -> ProcessSpec {
    ProcessSpec::new(&tool.path)
        .args(["check", "--message-format=json", "--color=never"])
        .args(&tool.args)
        .current_dir(root)
}

/// Saving one of these files re-runs a watched check.
pub fn is_cargo_input(path: &Path) -> bool {
    path.extension().is_some_and(|extension| extension == "rs")
        || path
            .file_name()
            .is_some_and(|name| name == "Cargo.toml" || name == "Cargo.lock")
}

/// A running check. Dropping it cancels the run and kills its process group.
#[must_use = "dropping a cargo check cancels it"]
pub struct CargoCheckTask {
    events: Receiver<CheckEvent>,
    child: Arc<Mutex<Option<SupervisedChild>>>,
    cancelled: Arc<AtomicBool>,
}

impl CargoCheckTask {
    /// Start `spec` and a worker that resolves relative diagnostic paths
    /// against `root`, the directory cargo runs in.
    pub fn spawn(spec: ProcessSpec, root: PathBuf) -> io::Result<Self> {
        let mut child = SupervisedChild::spawn(spec, ProcessLimits::default())?;
        child.close_stdin();
        let child = Arc::new(Mutex::new(Some(child)));
        let cancelled = Arc::new(AtomicBool::new(false));
        let (sender, events) = bounded(EVENT_QUEUE_CAPACITY);
        let worker = Worker {
            child: Arc::clone(&child),
            cancelled: Arc::clone(&cancelled),
            sender,
            root,
        };
        thread::Builder::new()
            .name("editor-cargo-check".into())
            .spawn(move || worker.run())?;
        Ok(Self {
            events,
            child,
            cancelled,
        })
    }

    /// Drain at most `limit` ready events without waiting.
    pub fn drain(&self, limit: usize) -> Vec<CheckEvent> {
        self.events.try_iter().take(limit).collect()
    }
}

impl Drop for CargoCheckTask {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        // Dropping the supervised child kills the whole cargo process group.
        lock(&self.child).take();
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

struct Worker {
    child: Arc<Mutex<Option<SupervisedChild>>>,
    cancelled: Arc<AtomicBool>,
    sender: Sender<CheckEvent>,
    root: PathBuf,
}

impl Worker {
    fn run(self) {
        let mut stdout = LineBuffer::default();
        let mut stderr = LineBuffer::default();
        let mut open_streams = 2;
        let mut seen = HashSet::new();
        while open_streams > 0 {
            let event = match lock(&self.child).as_ref() {
                Some(child) => child.try_recv(),
                None => return,
            };
            let (stream, lines) = match event {
                Ok(ProcessEvent::Output { stream, bytes }) => {
                    let lines = match stream {
                        OutputStream::Stdout => stdout.push(&bytes),
                        OutputStream::Stderr => stderr.push(&bytes),
                    };
                    (stream, lines)
                }
                Ok(ProcessEvent::Eof(stream) | ProcessEvent::ReadError { stream, .. }) => {
                    open_streams -= 1;
                    let line = match stream {
                        OutputStream::Stdout => stdout.finish(),
                        OutputStream::Stderr => stderr.finish(),
                    };
                    (stream, line.into_iter().collect())
                }
                Ok(ProcessEvent::OutputDropped { .. } | ProcessEvent::WriteError(_)) => continue,
                Err(TryRecvError::Empty) => {
                    if self.cancelled.load(Ordering::Acquire) {
                        return;
                    }
                    thread::sleep(POLL_INTERVAL);
                    continue;
                }
                Err(TryRecvError::Disconnected) => break,
            };
            for line in lines {
                let event = match stream {
                    OutputStream::Stdout => parse_message_line(&line, &self.root)
                        .filter(|entry| seen.insert(entry.clone()))
                        .map(CheckEvent::Entry),
                    OutputStream::Stderr => stderr_event(&line),
                };
                if let Some(event) = event
                    && !self.send(event)
                {
                    return;
                }
            }
        }
        self.finish();
    }

    fn finish(&self) {
        loop {
            if self.cancelled.load(Ordering::Acquire) {
                return;
            }
            let status = match lock(&self.child).as_mut() {
                Some(child) => child.try_wait(),
                None => return,
            };
            match status {
                Ok(Some(exit)) => {
                    self.send(CheckEvent::Finished {
                        success: exit.success,
                        code: exit.code,
                    });
                    return;
                }
                Ok(None) => thread::sleep(POLL_INTERVAL),
                Err(error) => {
                    self.send(CheckEvent::Failed(format!(
                        "could not read cargo's exit status: {error}"
                    )));
                    return;
                }
            }
        }
    }

    /// Wait for queue capacity, giving up when the task is dropped.
    fn send(&self, mut event: CheckEvent) -> bool {
        loop {
            if self.cancelled.load(Ordering::Acquire) {
                return false;
            }
            match self.sender.send_timeout(event, SEND_RETRY_INTERVAL) {
                Ok(()) => return true,
                Err(SendTimeoutError::Timeout(returned)) => event = returned,
                Err(SendTimeoutError::Disconnected(_)) => return false,
            }
        }
    }
}

/// Splits pipe chunks into lines, discarding any line longer than
/// `MAX_LINE_BYTES` instead of buffering it.
#[derive(Default)]
struct LineBuffer {
    pending: Vec<u8>,
    discarding: bool,
}

impl LineBuffer {
    fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        let mut lines = Vec::new();
        for chunk in bytes.split_inclusive(|byte| *byte == b'\n') {
            let complete = chunk.ends_with(b"\n");
            let content = chunk.strip_suffix(b"\n").unwrap_or(chunk);
            if !self.discarding {
                if self.pending.len().saturating_add(content.len()) > MAX_LINE_BYTES {
                    self.pending = Vec::new();
                    self.discarding = true;
                } else {
                    self.pending.extend_from_slice(content);
                }
            }
            if complete {
                if let Some(line) = self.take() {
                    lines.push(line);
                }
                self.discarding = false;
            }
        }
        lines
    }

    fn finish(&mut self) -> Option<String> {
        let line = self.take();
        self.discarding = false;
        line
    }

    fn take(&mut self) -> Option<String> {
        let bytes = std::mem::take(&mut self.pending);
        if self.discarding || bytes.is_empty() {
            return None;
        }
        let line = String::from_utf8_lossy(&bytes);
        Some(line.strip_suffix('\r').unwrap_or(&line).to_owned())
    }
}

/// Parse one `--message-format=json` line. Only error and warning
/// diagnostics produce entries; artifacts, notes, and summaries do not.
fn parse_message_line(line: &str, root: &Path) -> Option<CheckEntry> {
    let value: Value = serde_json::from_str(line).ok()?;
    if value.get("reason")?.as_str()? != "compiler-message" {
        return None;
    }
    let manifest_dir = value
        .get("manifest_path")
        .and_then(Value::as_str)
        .and_then(|path| Path::new(path).parent());
    parse_diagnostic(value.get("message")?, root, manifest_dir)
}

fn parse_diagnostic(
    message: &Value,
    root: &Path,
    manifest_dir: Option<&Path>,
) -> Option<CheckEntry> {
    let level_name = message.get("level")?.as_str()?;
    let level = match level_name {
        "warning" => CheckLevel::Warning,
        name if name.starts_with("error") => CheckLevel::Error,
        _ => return None,
    };
    let text = message.get("message")?.as_str()?.trim_end();
    let spans = message
        .get("spans")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    if spans.is_empty() && is_summary(text) {
        return None;
    }
    let code = message
        .get("code")
        .and_then(|code| code.get("code"))
        .and_then(Value::as_str)
        .map(|code| format!("[{code}]"))
        .unwrap_or_default();
    let primary = spans
        .iter()
        .find(|span| span.get("is_primary").and_then(Value::as_bool) == Some(true))
        .or_else(|| spans.first());
    let label = primary
        .and_then(|span| span.get("label"))
        .and_then(Value::as_str)
        .filter(|label| !label.is_empty())
        .map(str::to_owned);
    let primary = primary.map(source_span);
    let file_name = primary.and_then(|span| span.get("file_name")?.as_str());
    let line_start = primary.and_then(|span| span.get("line_start")?.as_u64());
    let column_start = primary.and_then(|span| span.get("column_start")?.as_u64());
    let (location, origin) = match (file_name, line_start, column_start) {
        (Some(file_name), Some(line), Some(column)) => (
            Some(CheckLocation {
                path: resolve_path(file_name, root, manifest_dir),
                line: usize::try_from(line.saturating_sub(1)).unwrap_or(usize::MAX),
                column: usize::try_from(column.saturating_sub(1)).unwrap_or(usize::MAX),
            }),
            Some(format!("{file_name}:{line}:{column}")),
        ),
        _ => (None, None),
    };
    let notes = message
        .get("children")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice)
        .iter()
        .filter_map(child_note)
        .collect();
    Some(CheckEntry {
        level: Some(level),
        title: format!("{level_name}{code}: {text}"),
        location,
        origin,
        label,
        notes,
    })
}

/// Rustc's span-less closing summaries repeat what the panel already counts.
fn is_summary(text: &str) -> bool {
    text.starts_with("aborting due to")
        || (text.ends_with("emitted")
            && text
                .split_whitespace()
                .next()
                .is_some_and(|count| count.chars().all(|ch| ch.is_ascii_digit())))
}

/// Follow macro expansions back to a real file, so a diagnostic inside a
/// macro from another crate points at its call site.
fn source_span(span: &Value) -> &Value {
    let mut span = span;
    for _ in 0..32 {
        let virtual_file = span
            .get("file_name")
            .and_then(Value::as_str)
            .is_none_or(|name| name.starts_with('<'));
        let Some(call_site) = span
            .get("expansion")
            .and_then(|expansion| expansion.get("span"))
            .filter(|_| virtual_file)
        else {
            break;
        };
        span = call_site;
    }
    span
}

fn child_note(child: &Value) -> Option<String> {
    let level = child.get("level")?.as_str()?;
    let text = child.get("message")?.as_str()?.trim_end();
    if text.is_empty() {
        return None;
    }
    let suggestion = child
        .get("spans")
        .and_then(Value::as_array)
        .and_then(|spans| {
            spans
                .iter()
                .find_map(|span| span.get("suggested_replacement")?.as_str())
        })
        .filter(|replacement| {
            !replacement.is_empty() && !replacement.contains('\n') && replacement.len() <= 80
        });
    Some(match suggestion {
        Some(replacement) => format!("{level}: {text}: `{replacement}`"),
        None => format!("{level}: {text}"),
    })
}

/// Rustc reports workspace members relative to the workspace root, which is
/// the project root in the common case. Otherwise search the package's
/// ancestors for a directory containing the file.
fn resolve_path(file_name: &str, root: &Path, manifest_dir: Option<&Path>) -> PathBuf {
    let path = Path::new(file_name);
    if path.is_absolute() {
        return path.to_owned();
    }
    std::iter::once(root)
        .chain(manifest_dir.into_iter().flat_map(Path::ancestors))
        .map(|base| base.join(path))
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| root.join(path))
}

/// Cargo writes right-aligned status lines (`   Compiling foo`) and its own
/// errors to stderr. The final "could not compile" lines repeat the panel's
/// summary.
fn stderr_event(line: &str) -> Option<CheckEvent> {
    let trimmed = line.trim();
    if trimmed.is_empty()
        || trimmed.starts_with("error: could not compile")
        || trimmed.starts_with("warning: build failed, waiting for other jobs")
    {
        return None;
    }
    let status_verb = line.starts_with(' ')
        && trimmed.split_whitespace().next().is_some_and(|word| {
            word.starts_with(|ch: char| ch.is_ascii_uppercase())
                && word.chars().all(|ch| ch.is_ascii_alphabetic())
        });
    Some(if status_verb {
        CheckEvent::Progress(trimmed.to_owned())
    } else {
        CheckEvent::Entry(CheckEntry::output(line.trim_end().to_owned()))
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum CheckStatus {
    #[default]
    Idle,
    Running,
    Finished {
        success: bool,
        code: Option<i32>,
        elapsed: Duration,
    },
    Failed(String),
    Cancelled,
}

/// Panel state kept in the editor model, so rendering never touches the
/// process. A new run's results replace the previous ones only when it ends,
/// which keeps the panel stable while a watched check re-runs.
#[derive(Debug, Default)]
pub struct CheckPanel {
    pub visible: bool,
    pub status: CheckStatus,
    /// Latest cargo status line of the current run.
    pub progress: String,
    pub selected: usize,
    /// First entry drawn; the renderer keeps the selection visible.
    pub scroll: usize,
    entries: Vec<CheckEntry>,
    pending: Vec<CheckEntry>,
    omitted: usize,
    pending_omitted: usize,
    started: Option<Instant>,
}

impl CheckPanel {
    pub fn entries(&self) -> &[CheckEntry] {
        &self.entries
    }

    /// Entries beyond the retained limit in the displayed results.
    pub fn omitted(&self) -> usize {
        self.omitted
    }

    pub fn is_running(&self) -> bool {
        self.status == CheckStatus::Running
    }

    pub fn begin(&mut self) {
        self.status = CheckStatus::Running;
        self.progress.clear();
        self.pending.clear();
        self.pending_omitted = 0;
        self.started = Some(Instant::now());
    }

    pub fn cancel(&mut self) {
        if self.is_running() {
            self.status = CheckStatus::Cancelled;
            self.progress.clear();
            self.pending.clear();
        }
    }

    /// Apply one worker event. Returns a summary message when the run ends.
    pub fn apply(&mut self, event: CheckEvent) -> Option<String> {
        if !self.is_running() {
            return None;
        }
        match event {
            CheckEvent::Entry(entry) => {
                if self.pending.len() >= MAX_ENTRIES {
                    self.pending_omitted = self.pending_omitted.saturating_add(1);
                } else {
                    let rank = entry.rank();
                    let index = self.pending.partition_point(|other| other.rank() <= rank);
                    self.pending.insert(index, entry);
                }
                None
            }
            CheckEvent::Progress(line) => {
                self.progress = line;
                None
            }
            CheckEvent::Finished { success, code } => {
                let elapsed = self
                    .started
                    .take()
                    .map_or(Duration::ZERO, |started| started.elapsed());
                self.publish();
                self.status = CheckStatus::Finished {
                    success,
                    code,
                    elapsed,
                };
                Some(format!("cargo check: {}", self.summary()))
            }
            CheckEvent::Failed(error) => {
                self.started = None;
                self.publish();
                self.status = CheckStatus::Failed(error.clone());
                Some(format!("cargo check failed: {error}"))
            }
        }
    }

    pub fn fail(&mut self, error: String) {
        self.begin();
        self.apply(CheckEvent::Failed(error));
    }

    fn publish(&mut self) {
        self.entries = std::mem::take(&mut self.pending);
        self.omitted = std::mem::take(&mut self.pending_omitted);
        self.progress.clear();
        self.selected = self.selected.min(self.entries.len().saturating_sub(1));
        self.scroll = self.scroll.min(self.selected);
    }

    pub fn counts(&self) -> (usize, usize) {
        self.entries
            .iter()
            .fold((0, 0), |(errors, warnings), entry| match entry.level {
                Some(CheckLevel::Error) => (errors + 1, warnings),
                Some(CheckLevel::Warning) => (errors, warnings + 1),
                None => (errors, warnings),
            })
    }

    /// One-line result such as `1 error, 2 warnings` or `failed (exit 101)`.
    pub fn summary(&self) -> String {
        let (errors, warnings) = self.counts();
        let plural = |count: usize, noun: &str| {
            format!("{count} {noun}{}", if count == 1 { "" } else { "s" })
        };
        match &self.status {
            CheckStatus::Idle => "not run".into(),
            CheckStatus::Running if self.progress.is_empty() => "running…".into(),
            CheckStatus::Running => format!("running · {}", self.progress),
            CheckStatus::Cancelled => "cancelled".into(),
            CheckStatus::Failed(error) => format!("failed: {error}"),
            CheckStatus::Finished { success, code, .. } => {
                if errors > 0 && warnings > 0 {
                    format!(
                        "{}, {}",
                        plural(errors, "error"),
                        plural(warnings, "warning")
                    )
                } else if errors > 0 {
                    plural(errors, "error")
                } else if !success {
                    match code {
                        Some(code) => format!("failed (exit {code})"),
                        None => "failed (terminated)".into(),
                    }
                } else if warnings > 0 {
                    plural(warnings, "warning")
                } else {
                    "no errors or warnings".into()
                }
            }
        }
    }

    pub fn move_selection(&mut self, delta: isize) {
        let last = self.entries.len().saturating_sub(1);
        self.selected = self.selected.saturating_add_signed(delta).min(last);
    }

    pub fn select_last(&mut self) {
        self.selected = self.entries.len().saturating_sub(1);
    }

    pub fn selected_entry(&self) -> Option<&CheckEntry> {
        self.entries.get(self.selected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool() -> ToolConfig {
        ToolConfig {
            path: "cargo".into(),
            args: vec!["--all-targets".into()],
        }
    }

    #[test]
    fn spec_runs_cargo_check_directly_with_json_and_extra_arguments() {
        let spec = cargo_check_spec(&tool(), Path::new("/work"));
        assert_eq!(spec.program, PathBuf::from("cargo"));
        assert_eq!(
            spec.args,
            [
                "check",
                "--message-format=json",
                "--color=never",
                "--all-targets"
            ]
            .map(std::ffi::OsString::from)
        );
        assert_eq!(spec.current_dir.as_deref(), Some(Path::new("/work")));
    }

    #[test]
    fn only_rust_sources_and_manifests_trigger_watched_runs() {
        assert!(is_cargo_input(Path::new("/work/src/main.rs")));
        assert!(is_cargo_input(Path::new("/work/Cargo.toml")));
        assert!(is_cargo_input(Path::new("/work/Cargo.lock")));
        assert!(!is_cargo_input(Path::new("/work/README.md")));
        assert!(!is_cargo_input(Path::new("/work/config.toml")));
    }

    #[test]
    fn compiler_messages_become_compact_entries() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        std::fs::create_dir_all(root.join("member/src")).unwrap();
        std::fs::write(root.join("member/src/lib.rs"), "").unwrap();
        let manifest = root.join("member/Cargo.toml");
        let line = serde_json::json!({
            "reason": "compiler-message",
            "manifest_path": manifest,
            "message": {
                "level": "warning",
                "message": "unused variable: `unused`",
                "code": {"code": "unused_variables"},
                "spans": [{
                    "file_name": "member/src/lib.rs",
                    "line_start": 2,
                    "column_start": 9,
                    "is_primary": true,
                    "label": null,
                }],
                "children": [
                    {"level": "note", "message": "`#[warn(unused_variables)]` on by default", "spans": []},
                    {"level": "help", "message": "if this is intentional, prefix it with an underscore",
                     "spans": [{"suggested_replacement": "_unused"}]},
                ],
            },
        })
        .to_string();
        // A nested workspace member runs from its own directory.
        let entry = parse_message_line(&line, &root.join("member")).unwrap();
        assert_eq!(entry.level, Some(CheckLevel::Warning));
        assert_eq!(
            entry.title,
            "warning[unused_variables]: unused variable: `unused`"
        );
        assert_eq!(
            entry.location,
            Some(CheckLocation {
                path: root.join("member/src/lib.rs"),
                line: 1,
                column: 8,
            })
        );
        assert_eq!(entry.origin.as_deref(), Some("member/src/lib.rs:2:9"));
        assert_eq!(entry.label, None);
        assert_eq!(
            entry.notes,
            [
                "note: `#[warn(unused_variables)]` on by default",
                "help: if this is intentional, prefix it with an underscore: `_unused`",
            ]
        );
    }

    #[test]
    fn primary_labels_macro_call_sites_and_noise_are_handled() {
        let root = Path::new("/work");
        let error = serde_json::json!({
            "level": "error",
            "message": "mismatched types",
            "code": {"code": "E0308"},
            "spans": [
                {"file_name": "src/main.rs", "line_start": 3, "column_start": 12,
                 "is_primary": false, "label": "expected due to this"},
                {"file_name": "src/main.rs", "line_start": 3, "column_start": 18,
                 "is_primary": true, "label": "expected `u32`, found `&str`"},
            ],
            "children": [],
        });
        let entry = parse_diagnostic(&error, root, None).unwrap();
        assert_eq!(entry.title, "error[E0308]: mismatched types");
        assert_eq!(entry.origin.as_deref(), Some("src/main.rs:3:18"));
        assert_eq!(entry.label.as_deref(), Some("expected `u32`, found `&str`"));
        assert_eq!(entry.location.unwrap().path, root.join("src/main.rs"));

        let in_macro = serde_json::json!({
            "level": "error",
            "message": "boom",
            "spans": [{
                "file_name": "<::core::macros::panic>", "line_start": 1, "column_start": 1,
                "is_primary": true,
                "expansion": {"span": {"file_name": "src/lib.rs", "line_start": 7, "column_start": 5}},
            }],
        });
        let entry = parse_diagnostic(&in_macro, root, None).unwrap();
        assert_eq!(entry.title, "error: boom");
        assert_eq!(entry.origin.as_deref(), Some("src/lib.rs:7:5"));

        for noise in [
            serde_json::json!({"level": "failure-note", "message": "For more information about this error, try `rustc --explain E0308`.", "spans": []}),
            serde_json::json!({"level": "error", "message": "aborting due to 2 previous errors", "spans": []}),
            serde_json::json!({"level": "warning", "message": "3 warnings emitted", "spans": []}),
        ] {
            assert_eq!(parse_diagnostic(&noise, root, None), None, "{noise}");
        }
        let artifact = r#"{"reason":"compiler-artifact","target":{"name":"demo"}}"#;
        assert_eq!(parse_message_line(artifact, root), None);
        assert_eq!(parse_message_line("not json", root), None);
    }

    #[test]
    fn stderr_separates_status_lines_from_cargo_errors() {
        assert_eq!(
            stderr_event("    Checking demo v0.1.0 (/work)"),
            Some(CheckEvent::Progress("Checking demo v0.1.0 (/work)".into()))
        );
        assert_eq!(
            stderr_event("error: could not find `Cargo.toml` in `/work` or any parent directory"),
            Some(CheckEvent::Entry(CheckEntry::output(
                "error: could not find `Cargo.toml` in `/work` or any parent directory".into()
            )))
        );
        assert!(matches!(
            stderr_event("  failed to parse manifest"),
            Some(CheckEvent::Entry(_))
        ));
        assert_eq!(
            stderr_event("error: could not compile `demo` (bin \"demo\") due to 1 previous error"),
            None
        );
        assert_eq!(stderr_event(""), None);
    }

    #[test]
    fn line_buffer_joins_chunks_and_drops_oversized_lines() {
        let mut buffer = LineBuffer::default();
        assert!(buffer.push(b"{\"a\":").is_empty());
        assert_eq!(buffer.push(b"1}\r\nnext\n"), ["{\"a\":1}", "next"]);
        assert_eq!(buffer.push(b"tail"), Vec::<String>::new());
        assert_eq!(buffer.finish().as_deref(), Some("tail"));

        let oversized = vec![b'x'; MAX_LINE_BYTES + 1];
        assert!(buffer.push(&oversized).is_empty());
        assert_eq!(buffer.push(b"rest\nkept\n"), ["kept"]);
        assert_eq!(buffer.finish(), None);
    }

    fn entry(level: Option<CheckLevel>, title: &str) -> CheckEntry {
        CheckEntry {
            level,
            title: title.into(),
            location: None,
            origin: None,
            label: None,
            notes: Vec::new(),
        }
    }

    #[test]
    fn panel_keeps_previous_results_until_a_run_finishes_and_sorts_errors_first() {
        let mut panel = CheckPanel::default();
        assert_eq!(panel.summary(), "not run");
        panel.begin();
        assert_eq!(panel.summary(), "running…");
        panel.apply(CheckEvent::Progress("Checking demo".into()));
        assert_eq!(panel.summary(), "running · Checking demo");
        panel.apply(CheckEvent::Entry(entry(
            Some(CheckLevel::Warning),
            "warning: a",
        )));
        panel.apply(CheckEvent::Entry(entry(None, "note from cargo")));
        panel.apply(CheckEvent::Entry(entry(
            Some(CheckLevel::Error),
            "error: b",
        )));
        panel.apply(CheckEvent::Entry(entry(
            Some(CheckLevel::Warning),
            "warning: c",
        )));
        assert!(panel.entries().is_empty());
        let message = panel.apply(CheckEvent::Finished {
            success: false,
            code: Some(101),
        });
        assert_eq!(message.as_deref(), Some("cargo check: 1 error, 2 warnings"));
        let titles = panel
            .entries()
            .iter()
            .map(|entry| entry.title.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            titles,
            ["error: b", "warning: a", "warning: c", "note from cargo"]
        );

        panel.move_selection(10);
        assert_eq!(panel.selected, 3);
        panel.begin();
        panel.apply(CheckEvent::Entry(entry(
            Some(CheckLevel::Warning),
            "warning: a",
        )));
        assert_eq!(panel.entries().len(), 4, "old results stay while running");
        panel.apply(CheckEvent::Finished {
            success: true,
            code: Some(0),
        });
        assert_eq!(panel.entries().len(), 1);
        assert_eq!(panel.selected, 0);
        assert_eq!(panel.summary(), "1 warning");

        panel.begin();
        panel.cancel();
        assert_eq!(panel.summary(), "cancelled");
        assert_eq!(
            panel.apply(CheckEvent::Finished {
                success: true,
                code: Some(0)
            }),
            None
        );
        assert_eq!(panel.entries().len(), 1);
    }

    #[test]
    fn failures_without_diagnostics_report_the_exit_status() {
        let mut panel = CheckPanel::default();
        panel.begin();
        panel.apply(CheckEvent::Entry(entry(None, "error: manifest is broken")));
        panel.apply(CheckEvent::Finished {
            success: false,
            code: Some(101),
        });
        assert_eq!(panel.summary(), "failed (exit 101)");
        assert_eq!(panel.entries().len(), 1);

        panel.fail("could not start cargo: not found".into());
        assert_eq!(panel.summary(), "failed: could not start cargo: not found");
        assert!(panel.entries().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn task_streams_parsed_output_and_exit_status() {
        let directory = tempfile::tempdir().unwrap();
        let message = serde_json::json!({
            "reason": "compiler-message",
            "message": {
                "level": "error",
                "message": "mismatched types",
                "spans": [{"file_name": "src/main.rs", "line_start": 3, "column_start": 18,
                           "is_primary": true, "label": null}],
            },
        });
        let script = format!(
            "printf '%s\\n' '{message}' '{message}'; echo '    Checking demo' >&2; \
             echo 'error: could not compile `demo`' >&2; exit 101"
        );
        let spec = ProcessSpec::new("sh").args(["-c", &script]);
        let task = CargoCheckTask::spawn(spec, directory.path().to_owned()).unwrap();
        let mut events = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !events
            .iter()
            .any(|event| matches!(event, CheckEvent::Finished { .. }))
        {
            assert!(Instant::now() < deadline, "cargo check task timed out");
            events.extend(task.drain(64));
            thread::sleep(Duration::from_millis(5));
        }
        let entries = events
            .iter()
            .filter(|event| matches!(event, CheckEvent::Entry(_)))
            .count();
        assert_eq!(entries, 1, "duplicate diagnostics are reported once");
        assert!(events.contains(&CheckEvent::Progress("Checking demo".into())));
        assert_eq!(
            events.last(),
            Some(&CheckEvent::Finished {
                success: false,
                code: Some(101),
            })
        );
    }

    #[cfg(unix)]
    #[test]
    fn dropping_a_task_stops_a_running_check() {
        let directory = tempfile::tempdir().unwrap();
        let spec = ProcessSpec::new("sh").args(["-c", "echo '    Checking slow' >&2; sleep 30"]);
        let task = CargoCheckTask::spawn(spec, directory.path().to_owned()).unwrap();
        let started = Instant::now();
        drop(task);
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
