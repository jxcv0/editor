//! Bounded, asynchronous Git inspection and explicit file staging.
//! Git sees saved files. The foreground suppresses line data for dirty buffers.

use std::{
    ffi::OsString,
    io,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crate::process::{OutputStream, ProcessEvent, ProcessLimits, ProcessSpec, SupervisedChild};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GitAction {
    Refresh,
    Status,
    Unstaged,
    Staged,
    Head,
    Blame,
    LineCommit,
    Stage,
    Unstage,
    NextHunk,
    PreviousHunk,
}

impl GitAction {
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "" | "status" => Self::Status,
            "refresh" => Self::Refresh,
            "diff" => Self::Unstaged,
            "staged" => Self::Staged,
            "head" => Self::Head,
            "blame" => Self::Blame,
            "commit" => Self::LineCommit,
            "stage" => Self::Stage,
            "unstage" => Self::Unstage,
            "next" => Self::NextHunk,
            "prev" => Self::PreviousHunk,
            _ => return None,
        })
    }

    pub fn mutates(self) -> bool {
        matches!(self, Self::Stage | Self::Unstage)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusEntry {
    pub path: PathBuf,
    pub index: char,
    pub worktree: char,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hunk {
    /// Zero-based range boundaries, including empty insertion/deletion ranges.
    pub old_start: usize,
    pub old_len: usize,
    pub new_start: usize,
    pub new_len: usize,
}

impl Hunk {
    pub fn anchor(self) -> usize {
        if self.new_len == 0 {
            self.new_start.saturating_sub(1)
        } else {
            self.new_start
        }
    }

    fn marker(self, line: usize) -> Option<char> {
        if self.new_len == 0 {
            (line == self.anchor()).then_some('-')
        } else if (self.new_start..self.new_start.saturating_add(self.new_len)).contains(&line) {
            Some(if self.old_len == 0 { '+' } else { '~' })
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct FileChanges {
    pub path: PathBuf,
    pub revision: u64,
    pub staged: Vec<Hunk>,
    pub unstaged: Vec<Hunk>,
}

impl FileChanges {
    pub fn markers(&self, line: usize) -> (Option<char>, Option<char>) {
        // Map worktree coordinates back into the index before showing staged
        // marks. Newly inserted/replaced text has no corresponding index line.
        let index_line = map_line(line, &self.unstaged, false);
        let marker = |hunks: &[Hunk], line| {
            let end = hunks.partition_point(|h| h.anchor() <= line);
            end.checked_sub(1).and_then(|i| hunks[i].marker(line))
        };
        let staged = index_line.and_then(|line| marker(&self.staged, line));
        let unstaged = marker(&self.unstaged, line);
        (staged, unstaged)
    }

    pub fn hunk_lines(&self) -> Vec<usize> {
        let mut lines: Vec<_> = self.unstaged.iter().map(|h| h.anchor()).collect();
        lines.extend(
            self.staged
                .iter()
                .filter_map(|h| map_line(h.anchor(), &self.unstaged, true)),
        );
        lines.sort_unstable();
        lines.dedup();
        lines
    }
}

fn map_line(line: usize, hunks: &[Hunk], forward: bool) -> Option<usize> {
    let end = hunks.partition_point(|h| (if forward { h.old_start } else { h.new_start }) <= line);
    let delta = if let Some(h) = end.checked_sub(1).map(|i| &hunks[i]) {
        let (start, len, target, target_len) = if forward {
            (h.old_start, h.old_len, h.new_start, h.new_len)
        } else {
            (h.new_start, h.new_len, h.old_start, h.old_len)
        };
        if line < start.saturating_add(len) {
            return None;
        }
        target as i128 + target_len as i128 - start as i128 - len as i128
    } else {
        0
    };
    usize::try_from(line as i128 + delta).ok()
}

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub root: PathBuf,
    pub branch: String,
    pub entries: Vec<StatusEntry>,
    pub file: Option<FileChanges>,
}

#[derive(Debug, Clone)]
pub struct Document {
    pub title: String,
    pub lines: Vec<String>,
    /// Prepared on the worker, once per snapshot; rendering never reparses a patch.
    pub rows: Vec<DiffRow>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub number: usize,
    pub text: String,
    pub changed: bool,
    pub no_newline: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffRow {
    Header(String),
    Lines {
        old: Option<DiffLine>,
        new: Option<DiffLine>,
    },
}

/// Align each contiguous replacement block in linear time. Context appears on
/// both sides, and unequal blocks get empty cells rather than shifted context.
/// Hunk counts, not prefix guesses, distinguish source text from file headers.
pub fn align_diff(lines: &[String]) -> Vec<DiffRow> {
    let mut rows = Vec::new();
    let (mut removed, mut added): (Vec<DiffLine>, Vec<DiffLine>) = (Vec::new(), Vec::new());
    let (mut old, mut new, mut old_left, mut new_left) = (0, 0, 0, 0);
    let mut last_side = ' ';
    fn flush(rows: &mut Vec<DiffRow>, removed: &mut Vec<DiffLine>, added: &mut Vec<DiffLine>) {
        let count = removed.len().max(added.len());
        let mut old = removed.drain(..);
        let mut new = added.drain(..);
        for _ in 0..count {
            rows.push(DiffRow::Lines {
                old: old.next(),
                new: new.next(),
            });
        }
    }
    fn range(value: &str, prefix: char) -> Option<(usize, usize)> {
        let value = value.strip_prefix(prefix)?;
        let (start, len) = value.split_once(',').unwrap_or((value, "1"));
        Some((start.parse().ok()?, len.parse().ok()?))
    }
    for line in lines {
        if line == "\\ No newline at end of file" {
            match last_side {
                '-' => {
                    if let Some(line) = removed.last_mut() {
                        line.no_newline = true;
                    }
                }
                '+' => {
                    if let Some(line) = added.last_mut() {
                        line.no_newline = true;
                    }
                }
                _ => {
                    if let Some(DiffRow::Lines { old, new }) = rows.last_mut() {
                        for line in old.iter_mut().chain(new.iter_mut()) {
                            line.no_newline = true;
                        }
                    } else {
                        rows.push(DiffRow::Header(line.clone()));
                    }
                }
            }
            continue;
        }
        if let Some(header) = line.strip_prefix("@@ ") {
            flush(&mut rows, &mut removed, &mut added);
            let mut parts = header.split_whitespace();
            if let Some(((a, b), (c, d))) = parts
                .next()
                .and_then(|p| range(p, '-'))
                .zip(parts.next().and_then(|p| range(p, '+')))
                .filter(|_| parts.next() == Some("@@"))
            {
                (old, old_left, new, new_left) = (a, b, c, d);
            } else {
                (old_left, new_left) = (0, 0);
            }
            rows.push(DiffRow::Header(line.clone()));
            last_side = ' ';
            continue;
        }
        let prefix = line.chars().next().unwrap_or('\0');
        let valid = match prefix {
            '-' => old_left > 0,
            '+' => new_left > 0,
            ' ' => old_left > 0 && new_left > 0,
            _ => false,
        };
        if !valid {
            flush(&mut rows, &mut removed, &mut added);
            (old_left, new_left) = (0, 0);
            rows.push(DiffRow::Header(line.clone()));
            last_side = ' ';
            continue;
        }
        let text = line[1..].to_owned();
        let make = |number| DiffLine {
            number,
            text: text.clone(),
            changed: prefix != ' ',
            no_newline: false,
        };
        match prefix {
            '-' => {
                removed.push(make(old));
                old = old.saturating_add(1);
                old_left -= 1;
            }
            '+' => {
                added.push(make(new));
                new = new.saturating_add(1);
                new_left -= 1;
            }
            _ => {
                flush(&mut rows, &mut removed, &mut added);
                rows.push(DiffRow::Lines {
                    old: Some(make(old)),
                    new: Some(make(new)),
                });
                old = old.saturating_add(1);
                new = new.saturating_add(1);
                old_left -= 1;
                new_left -= 1;
            }
        }
        last_side = prefix;
    }
    flush(&mut rows, &mut removed, &mut added);
    rows
}

#[derive(Debug, Default)]
pub struct GitPanel {
    pub snapshot: Option<Snapshot>,
    pub visible: bool,
    pub loading: bool,
    pub selected: usize,
    pub scroll: usize,
    pub horizontal: usize,
    pub document: Option<Document>,
    pub path: Option<PathBuf>,
    pub error: Option<String>,
    /// Dismissing or replacing a view invalidates in-flight presentation.
    pub generation: u64,
}

impl GitPanel {
    pub fn selected_path(&self) -> Option<PathBuf> {
        if self.document.is_some() {
            return self.path.clone();
        }
        let snapshot = self.snapshot.as_ref()?;
        snapshot
            .entries
            .get(self.selected)
            .map(|entry| snapshot.root.join(&entry.path))
    }

    pub fn replace_snapshot(&mut self, snapshot: Snapshot) {
        let selected = self.selected_path();
        self.selected = selected
            .and_then(|path| {
                snapshot
                    .entries
                    .iter()
                    .position(|entry| snapshot.root.join(&entry.path) == path)
            })
            .unwrap_or(self.selected)
            .min(snapshot.entries.len().saturating_sub(1));
        self.snapshot = Some(snapshot);
    }
}

#[derive(Debug, Clone)]
pub struct GitRequest {
    pub action: GitAction,
    pub root: PathBuf,
    pub path: Option<PathBuf>,
    pub line: usize,
    pub revision: u64,
    pub generation: u64,
    pub explicit: bool,
}

pub struct GitResult {
    pub snapshot: Snapshot,
    pub document: Option<Document>,
}

/// One worker owns its children; no filesystem or process waits on input.
pub struct GitTask {
    pub request: GitRequest,
    result: Receiver<Result<GitResult, String>>,
    cancelled: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl GitTask {
    pub fn spawn(request: GitRequest, max_bytes: usize) -> io::Result<Self> {
        let cancelled = Arc::new(AtomicBool::new(false));
        let (sender, result) = mpsc::sync_channel(1);
        let job = request.clone();
        let stop = cancelled.clone();
        let worker = thread::Builder::new()
            .name("editor-git".into())
            .spawn(move || {
                let runner = Runner {
                    cancelled: stop,
                    max_bytes: max_bytes.clamp(1024, 32 * 1024 * 1024),
                };
                let _ = sender.send(run_job(&runner, &job));
            })?;
        Ok(Self {
            request,
            result,
            cancelled,
            worker: Some(worker),
        })
    }

    pub fn poll(&self) -> Option<Result<GitResult, String>> {
        match self.result.try_recv() {
            Ok(result) => Some(result),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => Some(Err("Git worker stopped".into())),
        }
    }
}

impl Drop for GitTask {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        // The runtime never replaces a busy task. Shutdown joins the worker
        // so it cannot leave children or an index write behind on exit.
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct Runner {
    cancelled: Arc<AtomicBool>,
    max_bytes: usize,
}

impl Runner {
    fn run(&self, root: &Path, args: &[OsString], diff_exit: bool) -> Result<Vec<u8>, String> {
        if self.cancelled.load(Ordering::Acquire) {
            return Err("Git cancelled".into());
        }
        let spec = ProcessSpec::new("git")
            .args([
                "--no-pager",
                "--literal-pathspecs",
                "-c",
                "color.ui=false",
                "-c",
                "core.quotePath=false",
            ])
            .args(args)
            .current_dir(root)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("LC_ALL", "C");
        let mut child = SupervisedChild::spawn(
            spec,
            ProcessLimits {
                shutdown_timeout: Duration::from_millis(100),
                ..ProcessLimits::default()
            },
        )
        .map_err(|e| format!("Cannot start Git: {e}"))?;
        child.close_stdin();
        let started = Instant::now();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut eof = 0;
        loop {
            if self.cancelled.load(Ordering::Acquire) {
                return Err("Git cancelled".into());
            }
            if started.elapsed() >= Duration::from_secs(10) {
                return Err("Git timed out after 10 seconds".into());
            }
            for event in child.drain_events(128) {
                match event {
                    ProcessEvent::Output { stream, bytes } => {
                        if stdout.len() + stderr.len() + bytes.len() > self.max_bytes {
                            return Err(format!(
                                "Git output exceeds {} bytes; narrow the view",
                                self.max_bytes
                            ));
                        }
                        match stream {
                            OutputStream::Stdout => stdout.extend(bytes),
                            OutputStream::Stderr => stderr.extend(bytes),
                        }
                    }
                    ProcessEvent::Eof(_) => eof += 1,
                    ProcessEvent::ReadError { error, .. } | ProcessEvent::WriteError(error) => {
                        return Err(error.to_string());
                    }
                    ProcessEvent::OutputDropped { .. } => {
                        return Err("Git output queue overflow".into());
                    }
                }
            }
            if eof >= 2
                && let Some(exit) = child.try_wait().map_err(|e| e.to_string())?
            {
                if exit.success || (diff_exit && exit.code == Some(1)) {
                    return Ok(stdout);
                }
                let error = String::from_utf8_lossy(&stderr);
                return Err(if error.trim().is_empty() {
                    format!("Git exited with {:?}", exit.code)
                } else {
                    error.trim().chars().take(4096).collect()
                });
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn strings(&self, root: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
        self.run(
            root,
            &args.iter().map(OsString::from).collect::<Vec<_>>(),
            false,
        )
    }
}

fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        PathBuf::from(OsString::from_vec(bytes.to_vec()))
    }
    #[cfg(not(unix))]
    {
        PathBuf::from(String::from_utf8_lossy(bytes).as_ref())
    }
}

fn parse_status(root: PathBuf, bytes: &[u8]) -> Result<Snapshot, String> {
    let mut snapshot = Snapshot {
        root,
        ..Snapshot::default()
    };
    for record in bytes.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        if let Some(branch) = record.strip_prefix(b"## ") {
            snapshot.branch = String::from_utf8_lossy(branch).into_owned();
        } else if record.len() >= 4 && record[2] == b' ' {
            if snapshot.entries.len() == 10_000 {
                return Err("Git status exceeds 10000 entries".into());
            }
            snapshot.entries.push(StatusEntry {
                index: record[0] as char,
                worktree: record[1] as char,
                path: path_from_bytes(&record[3..]),
            });
        } else {
            return Err("Malformed Git status".into());
        }
    }
    Ok(snapshot)
}

pub fn parse_hunks(diff: &[u8]) -> Vec<Hunk> {
    fn range(value: &str) -> Option<(usize, usize)> {
        let value = value.get(1..)?;
        let (start, len) = value.split_once(',').unwrap_or((value, "1"));
        let start: usize = start.parse().ok()?;
        let len = len.parse().ok()?;
        Some((
            if len == 0 {
                start
            } else {
                start.checked_sub(1)?
            },
            len,
        ))
    }
    String::from_utf8_lossy(diff)
        .lines()
        .filter_map(|line| {
            let mut fields = line.strip_prefix("@@ ")?.split_whitespace();
            let old = fields.next()?;
            if !old.starts_with('-') {
                return None;
            }
            let (old_start, old_len) = range(old)?;
            let new = fields.next()?;
            if !new.starts_with('+') {
                return None;
            }
            let (new_start, new_len) = range(new)?;
            if fields.next()? != "@@" {
                return None;
            }
            Some(Hunk {
                old_start,
                old_len,
                new_start,
                new_len,
            })
        })
        .collect()
}

fn diff(
    runner: &Runner,
    snapshot: &Snapshot,
    path: &Path,
    action: GitAction,
    context: usize,
) -> Result<Vec<u8>, String> {
    let untracked = snapshot
        .entries
        .iter()
        .any(|e| e.path == path && e.index == '?');
    let mut args: Vec<OsString> = [
        "diff",
        "--no-ext-diff",
        "--no-textconv",
        "--no-color",
        "--no-renames",
        "--no-relative",
        "--inter-hunk-context=0",
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    args.push(format!("--unified={context}").into());
    match action {
        GitAction::Staged => args.push("--cached".into()),
        GitAction::Head if !untracked => args.push("HEAD".into()),
        _ => {}
    }
    let no_index = untracked && action != GitAction::Staged;
    if no_index {
        args.push("--no-index".into());
    }
    args.push("--".into());
    if no_index {
        args.push("/dev/null".into());
    }
    args.push(path.as_os_str().into());
    runner.run(&snapshot.root, &args, no_index)
}

fn run_job(runner: &Runner, request: &GitRequest) -> Result<GitResult, String> {
    let root = runner.strings(&request.root, &["rev-parse", "--show-toplevel"])?;
    let root = path_from_bytes(root.strip_suffix(b"\n").unwrap_or(&root));
    let path = request
        .path
        .as_ref()
        .map(|path| {
            path.strip_prefix(&root)
                .map(Path::to_owned)
                .map_err(|_| "File is outside this Git repository".to_owned())
        })
        .transpose()?;
    if request.action.mutates() {
        let path = path.as_ref().ok_or("Save the buffer to a file first")?;
        let mut args: Vec<OsString> = if request.action == GitAction::Stage {
            vec!["add".into()]
        } else if runner
            .strings(&root, &["rev-parse", "--verify", "HEAD"])
            .is_ok()
        {
            vec!["restore".into(), "--staged".into()]
        } else {
            vec!["rm".into(), "--cached".into(), "--ignore-unmatch".into()]
        };
        args.extend([OsString::from("--"), path.as_os_str().into()]);
        runner.run(&root, &args, false)?;
    }
    inspect_job(runner, request, root, path).map_err(|error| {
        if request.action.mutates() {
            format!(
                "File {}; refresh failed: {error}",
                if request.action == GitAction::Stage {
                    "staged"
                } else {
                    "unstaged"
                }
            )
        } else {
            error
        }
    })
}

fn inspect_job(
    runner: &Runner,
    request: &GitRequest,
    root: PathBuf,
    path: Option<PathBuf>,
) -> Result<GitResult, String> {
    let bytes = runner.strings(
        &root,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--branch",
            "--untracked-files=all",
            "--no-renames",
        ],
    )?;
    let mut snapshot = parse_status(root, &bytes)?;
    if let Some(path) = &path {
        let unstaged = diff(runner, &snapshot, path, GitAction::Unstaged, 0)?;
        let staged = diff(runner, &snapshot, path, GitAction::Staged, 0)?;
        snapshot.file = Some(FileChanges {
            path: snapshot.root.join(path),
            revision: request.revision,
            staged: parse_hunks(&staged),
            unstaged: parse_hunks(&unstaged),
        });
    }
    let document = match request.action {
        GitAction::Unstaged | GitAction::Staged | GitAction::Head => {
            let path = path.as_ref().ok_or("Save the buffer to a file first")?;
            let output = diff(runner, &snapshot, path, request.action, 3)?;
            let label = match request.action {
                GitAction::Staged => "Staged (HEAD → index)",
                GitAction::Head => "HEAD → saved file",
                _ => "Unstaged (index → saved file)",
            };
            Some(document(format!("{label}: {}", path.display()), output))
        }
        GitAction::Blame | GitAction::LineCommit => {
            let path = path.as_ref().ok_or("Save the buffer to a file first")?;
            let line = request.line.saturating_add(1);
            let args = [
                OsString::from("blame"),
                "--line-porcelain".into(),
                "--no-textconv".into(),
                "-L".into(),
                format!("{line},{line}").into(),
                "--".into(),
                path.as_os_str().into(),
            ];
            let blame = runner.run(&snapshot.root, &args, false)?;
            let blame = String::from_utf8_lossy(&blame);
            let hash = blame
                .split_whitespace()
                .next()
                .ok_or("No blame information")?;
            if !matches!(hash.len(), 40 | 64) || !hash.bytes().all(|c| c.is_ascii_hexdigit()) {
                return Err("Invalid blame commit ID".into());
            }
            if hash.bytes().all(|c| c == b'0') {
                return Err("This line has not been committed yet".into());
            }
            let patch = if request.action == GitAction::LineCommit {
                "--patch"
            } else {
                "--no-patch"
            };
            let output = runner.strings(
                &snapshot.root,
                &[
                    "show",
                    "--format=fuller",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--no-color",
                    patch,
                    hash,
                    "--",
                ],
            )?;
            Some(document(
                format!("Line {line}: {} ({})", path.display(), &hash[..12]),
                output,
            ))
        }
        _ => None,
    };
    Ok(GitResult { snapshot, document })
}

fn document(title: String, bytes: Vec<u8>) -> Document {
    let mut lines: Vec<_> = String::from_utf8_lossy(&bytes)
        .lines()
        .take(50_001)
        .map(str::to_owned)
        .collect();
    if lines.len() > 50_000 {
        lines.truncate(50_000);
        lines.push("… view limited to 50000 lines".into());
    }
    if lines.is_empty() {
        lines.push("No changes".into());
    }
    let rows = align_diff(&lines);
    Document { title, lines, rows }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, process::Command};

    #[test]
    fn split_diff_aligns_unequal_changes_context_and_numbers() {
        let doc = document("patch".into(), b"--- a/file\n+++ b/file\n@@ -10,4 +20,3 @@ fn\n same\n-old one\n-old two\n+new\n tail\n@@ -30,0 +40,2 @@\n+added\n+again\n".to_vec());
        assert_eq!(
            doc.rows[3],
            DiffRow::Lines {
                old: Some(DiffLine {
                    number: 10,
                    text: "same".into(),
                    changed: false,
                    no_newline: false
                }),
                new: Some(DiffLine {
                    number: 20,
                    text: "same".into(),
                    changed: false,
                    no_newline: false
                }),
            }
        );
        assert!(
            matches!(&doc.rows[4], DiffRow::Lines { old: Some(a), new: Some(b) }
            if a.number == 11 && b.number == 21 && a.changed && b.changed)
        );
        assert!(
            matches!(&doc.rows[5], DiffRow::Lines { old: Some(a), new: None } if a.number == 12)
        );
        assert!(
            matches!(&doc.rows[6], DiffRow::Lines { old: Some(a), new: Some(b) }
            if a.text == "tail" && b.text == "tail" && a.number == 13 && b.number == 22)
        );
        assert!(
            matches!(&doc.rows[8], DiffRow::Lines { old: None, new: Some(b) } if b.number == 40)
        );
    }

    #[test]
    fn split_diff_handles_no_newline_header_like_source_and_multiple_files() {
        let doc = document("patch".into(), b"diff --git a/a b/a\n@@ -1 +1 @@\n---source\n\\ No newline at end of file\n+++source\n\\ No newline at end of file\ndiff --git a/b b/b\n@@ -1 +0,0 @@\n-deleted\n".to_vec());
        assert!(
            matches!(&doc.rows[2], DiffRow::Lines { old: Some(a), new: Some(b) }
            if a.text == "--source" && b.text == "++source" && a.no_newline && b.no_newline)
        );
        assert!(matches!(&doc.rows[3], DiffRow::Header(text) if text.starts_with("diff --git")));
        assert!(
            matches!(&doc.rows[5], DiffRow::Lines { old: Some(a), new: None } if a.number == 1)
        );
        for bytes in [
            b"Binary files a/a and b/a differ\n".as_slice(),
            b"rename from a\nrename to b\n",
            b"",
            b"@@ malformed\ntext\n",
        ] {
            let doc = document("meta".into(), bytes.to_vec());
            assert!(doc.rows.iter().all(|row| matches!(row, DiffRow::Header(_))));
        }
    }

    fn git(root: &Path, args: &[&str]) -> Vec<u8> {
        let output = Command::new("git")
            .args(args)
            .current_dir(root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q"]);
        git(
            dir.path(),
            &["config", "user.email", "test@example.invalid"],
        );
        git(dir.path(), &["config", "user.name", "Editor Test"]);
        git(dir.path(), &["config", "commit.gpgsign", "false"]);
        git(dir.path(), &["config", "core.hooksPath", "/dev/null"]);
        dir
    }

    fn run(root: &Path, path: Option<&Path>, action: GitAction) -> Result<GitResult, String> {
        let request = GitRequest {
            root: root.to_owned(),
            path: path.map(Path::to_owned),
            action,
            line: 0,
            revision: 7,
            generation: 1,
            explicit: true,
        };
        let task = GitTask::spawn(request, 1024 * 1024).unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(result) = task.poll() {
                return result;
            }
            assert!(Instant::now() < deadline, "Git task failed to finish");
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn parses_hunk_boundaries_deletions_and_maps_staged_lines() {
        let hunks = parse_hunks(b"@@ -0,0 +1,2 @@\n+a\n+b\n@@ -3 +5 @@\n-x\n+y\n@@ -8,2 +9,0 @@\n");
        assert_eq!(
            hunks,
            vec![
                Hunk {
                    old_start: 0,
                    old_len: 0,
                    new_start: 0,
                    new_len: 2
                },
                Hunk {
                    old_start: 2,
                    old_len: 1,
                    new_start: 4,
                    new_len: 1
                },
                Hunk {
                    old_start: 7,
                    old_len: 2,
                    new_start: 9,
                    new_len: 0
                },
            ]
        );
        let changes = FileChanges {
            staged: vec![Hunk {
                old_start: 2,
                old_len: 1,
                new_start: 2,
                new_len: 1,
            }],
            unstaged: vec![hunks[0]],
            ..FileChanges::default()
        };
        assert_eq!(changes.markers(0), (None, Some('+')));
        assert_eq!(changes.markers(4), (Some('~'), None));
        assert_eq!(changes.hunk_lines(), vec![0, 4]);
        assert_eq!(hunks[2].marker(8), Some('-'));
        assert!(parse_hunks(b"@@@ -1 -1 +1 @@@\n@@ -0 +0 @@").is_empty());
    }

    #[test]
    fn reads_and_stages_literal_paths_in_unborn_repository() {
        let dir = repo();
        let name = ":(glob)* ü\nfile.rs";
        let path = dir.path().join(name);
        fs::write(&path, "hello\nworld\n").unwrap();
        fs::write(dir.path().join("unrelated.rs"), "keep\n").unwrap();
        let status = run(dir.path(), Some(&path), GitAction::Unstaged).unwrap();
        assert_eq!(
            status
                .snapshot
                .entries
                .iter()
                .find(|e| e.path == Path::new(name))
                .unwrap()
                .index,
            '?'
        );
        assert!(status.document.unwrap().lines.iter().any(|s| s == "+hello"));
        assert_eq!(status.snapshot.file.unwrap().markers(1), (None, Some('+')));
        let staged = run(dir.path(), Some(&path), GitAction::Stage).unwrap();
        assert_eq!(staged.snapshot.file.unwrap().markers(0), (Some('+'), None));
        assert_eq!(
            git(dir.path(), &["ls-files", "-z"]),
            format!("{name}\0").as_bytes()
        );
        run(dir.path(), Some(&path), GitAction::Unstage).unwrap();
        assert!(git(dir.path(), &["ls-files", "-z"]).is_empty());
        assert_eq!(fs::read_to_string(&path).unwrap(), "hello\nworld\n");
    }

    #[test]
    fn partial_staging_diff_blame_and_unstage_preserve_worktree() {
        let dir = repo();
        let path = dir.path().join("file.rs");
        fs::write(&path, "one\ntwo\nthree\n").unwrap();
        git(dir.path(), &["add", "file.rs"]);
        git(dir.path(), &["commit", "-qm", "Initial lines"]);
        fs::write(&path, "one\nTWO\nthree\n").unwrap();
        git(dir.path(), &["add", "file.rs"]);
        fs::write(&path, "zero\none\nTWO\nthree\n").unwrap();
        let staged = run(dir.path(), Some(&path), GitAction::Staged).unwrap();
        let file = staged.snapshot.file.unwrap();
        assert_eq!(file.markers(0), (None, Some('+')));
        assert_eq!(file.markers(2), (Some('~'), None));
        assert!(staged.document.unwrap().lines.iter().any(|s| s == "+TWO"));
        let unstaged = run(dir.path(), Some(&path), GitAction::Unstaged)
            .unwrap()
            .document
            .unwrap();
        assert!(unstaged.lines.iter().any(|s| s == "+zero"));
        assert!(!unstaged.lines.iter().any(|s| s == "+TWO"));
        let head = run(dir.path(), Some(&path), GitAction::Head)
            .unwrap()
            .document
            .unwrap();
        assert!(head.lines.iter().any(|s| s == "+TWO"));
        assert!(
            run(dir.path(), Some(&path), GitAction::Blame)
                .err()
                .unwrap()
                .contains("not been committed")
        );
        run(dir.path(), Some(&path), GitAction::Unstage).unwrap();
        assert!(git(dir.path(), &["diff", "--cached"]).is_empty());
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "zero\none\nTWO\nthree\n"
        );
        fs::write(&path, "one\ntwo\nthree\n").unwrap();
        let blame = run(dir.path(), Some(&path), GitAction::Blame)
            .unwrap()
            .document
            .unwrap();
        assert!(blame.lines.iter().any(|s| s.contains("Editor Test")));
        let commit = run(dir.path(), Some(&path), GitAction::LineCommit)
            .unwrap()
            .document
            .unwrap();
        assert!(commit.lines.iter().any(|s| s == "+one"));
    }

    #[test]
    fn status_works_for_nested_projects_deleted_files_and_binary_diffs() {
        let dir = repo();
        let nested = dir.path().join("nested");
        fs::create_dir(&nested).unwrap();
        let path = nested.join("binary");
        fs::write(&path, b"before\0\xff").unwrap();
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-qm", "binary"]);
        fs::write(&path, b"after\0\xfe").unwrap();
        let result = run(&nested, Some(&path), GitAction::Unstaged).unwrap();
        assert_eq!(result.snapshot.root, dir.path());
        assert!(result.snapshot.file.unwrap().unstaged.is_empty());
        assert!(
            result
                .document
                .unwrap()
                .lines
                .iter()
                .any(|s| s.contains("Binary files"))
        );
        fs::remove_file(&path).unwrap();
        let result = run(&nested, Some(&path), GitAction::Stage).unwrap();
        assert_eq!(result.snapshot.entries[0].index, 'D');
    }

    #[cfg(unix)]
    #[test]
    fn nul_status_preserves_non_utf8_filenames() {
        use std::os::unix::ffi::OsStrExt;
        let snapshot =
            parse_status(PathBuf::from("/repo"), b"## main\0 M odd\xff\nname\0").unwrap();
        assert_eq!(
            snapshot.entries[0].path.as_os_str().as_bytes(),
            b"odd\xff\nname"
        );
    }

    #[test]
    fn output_limits_and_missing_repositories_are_reported() {
        let dir = repo();
        let path = dir.path().join("large");
        fs::write(&path, "long line\n".repeat(1000)).unwrap();
        let runner = Runner {
            cancelled: Arc::new(AtomicBool::new(false)),
            max_bytes: 100,
        };
        assert!(
            runner
                .strings(dir.path(), &["diff", "--no-index", "/dev/null", "large"])
                .err()
                .unwrap()
                .contains("exceeds")
        );
        let outside = tempfile::tempdir().unwrap();
        assert!(run(outside.path(), None, GitAction::Status).is_err());
        runner.cancelled.store(true, Ordering::Release);
        assert_eq!(
            runner.strings(dir.path(), &["status"]).unwrap_err(),
            "Git cancelled"
        );
    }

    #[test]
    fn successful_index_write_is_reported_when_subsequent_refresh_exceeds_limits() {
        let dir = repo();
        let path = dir.path().join("small");
        fs::write(&path, "stage me\n").unwrap();
        fs::write(dir.path().join("a".repeat(200)), "large status entry").unwrap();
        let runner = Runner {
            cancelled: Arc::new(AtomicBool::new(false)),
            max_bytes: 150,
        };
        let request = GitRequest {
            root: dir.path().to_owned(),
            path: Some(path),
            action: GitAction::Stage,
            line: 0,
            revision: 0,
            generation: 0,
            explicit: true,
        };
        let error = run_job(&runner, &request).err().unwrap();
        assert!(error.starts_with("File staged; refresh failed:"), "{error}");
        assert_eq!(git(dir.path(), &["ls-files", "-z"]), b"small\0");
    }
}
