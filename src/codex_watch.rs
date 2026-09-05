//! Explicit, bounded integration with `codex-watch --json-events`.
//!
//! Merely constructing this controller never launches the tool.  A project must first be enabled
//! and have an explicit dry-run or workspace-write mode selected.  Like the LSP client, all child
//! work lives on a background thread and the editor-facing API only touches bounded channels.

use crate::process::{
    BoundedLineDecoder, BoundedLog, OutputStream, ProcessEvent, ProcessLimits, ProcessSpec,
    SupervisedChild,
};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const MAX_STATUS_ERROR_BYTES: usize = 4 * 1024;
const MAX_ACTIVE_PATHS: usize = 4096;
const MAX_ACTIVE_PATH_BYTES: usize = 4 * 1024 * 1024;

/// Active marker lines, using canonical file paths and zero-based line numbers.
pub type CodexWorkingLines = HashMap<PathBuf, BTreeSet<usize>>;

#[derive(Default)]
struct CodexActivity {
    tasks: BTreeMap<(PathBuf, u64), usize>,
    path_bytes: usize,
    revision: u64,
}

impl CodexActivity {
    fn update(&mut self, event: &CodexJsonEvent, path: PathBuf) -> Result<(), String> {
        let state = event.state_name();
        let task = event.payload["task"].as_u64();
        if matches!(state, "queued" | "preparing" | "waiting" | "retrying") {
            let Some(line) = event.payload["line"]
                .as_u64()
                .and_then(|line| usize::try_from(line).ok())
                .and_then(|line| line.checked_sub(1))
            else {
                return Ok(());
            };
            let bytes = path.as_os_str().len();
            let key = (path, task.unwrap_or(0));
            if !self.tasks.contains_key(&key) {
                if self.tasks.len() >= MAX_ACTIVE_PATHS
                    || bytes > MAX_ACTIVE_PATH_BYTES - self.path_bytes
                {
                    return Err("codex-watch active line tracking limit exceeded".into());
                }
                self.path_bytes += bytes;
            }
            if self.tasks.insert(key, line) != Some(line) {
                self.revision = self.revision.wrapping_add(1);
            }
        } else if matches!(state, "applied" | "previewed" | "failed" | "idle") {
            let previous_len = self.tasks.len();
            self.tasks.retain(|(candidate, id), _| {
                let remove =
                    *candidate == path && (state == "idle" || task.is_none_or(|task| task == *id));
                if remove {
                    self.path_bytes -= candidate.as_os_str().len();
                }
                !remove
            });
            if self.tasks.len() != previous_len {
                self.revision = self.revision.wrapping_add(1);
            }
        }
        Ok(())
    }

    fn clear(&mut self) {
        if !self.tasks.is_empty() {
            self.tasks.clear();
            self.path_bytes = 0;
            self.revision = self.revision.wrapping_add(1);
        }
    }

    fn lines_since(&self, revision: u64) -> Option<(u64, CodexWorkingLines)> {
        if self.revision == revision {
            return None;
        }
        let mut lines = CodexWorkingLines::new();
        for ((path, _), line) in &self.tasks {
            lines.entry(path.clone()).or_default().insert(*line);
        }
        Some((self.revision, lines))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodexRunMode {
    DryRun,
    WorkspaceWrite,
}

impl fmt::Display for CodexRunMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DryRun => formatter.write_str("dry-run"),
            Self::WorkspaceWrite => formatter.write_str("workspace-write"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodexRunKind {
    Watch,
    Once,
}

#[derive(Clone, Debug)]
pub struct CodexWatchConfig {
    pub executable: PathBuf,
    /// Trusted base arguments inserted before the integration-owned flags.
    pub args: Vec<OsString>,
    pub project_root: PathBuf,
    pub json_events_arg: OsString,
    pub run_once_arg: OsString,
    pub dry_run_args: Vec<OsString>,
    pub workspace_write_args: Vec<OsString>,
    pub process_limits: ProcessLimits,
    pub max_event_line_bytes: usize,
    pub command_queue_capacity: usize,
    pub event_queue_capacity: usize,
    pub captured_output_bytes: usize,
}

impl CodexWatchConfig {
    pub fn new(project_root: impl Into<PathBuf>) -> Self {
        Self {
            executable: PathBuf::from("codex-watch"),
            args: Vec::new(),
            project_root: project_root.into(),
            json_events_arg: OsString::from("--json-events"),
            run_once_arg: OsString::from("--once"),
            dry_run_args: vec![OsString::from("--dry-run")],
            workspace_write_args: vec![OsString::from("--workspace-write")],
            process_limits: ProcessLimits::default(),
            max_event_line_bytes: 1024 * 1024,
            command_queue_capacity: 64,
            event_queue_capacity: 256,
            captured_output_bytes: 256 * 1024,
        }
    }

    /// The exact direct invocation for status/confirmation UI.
    pub fn process_spec(&self, mode: CodexRunMode, kind: CodexRunKind) -> ProcessSpec {
        let mut args = self.args.clone();
        // Execution-control flags belong to the typed integration, not an opaque argument list.
        // Removing configured duplicates also makes adapting the global ToolConfig (whose default
        // already includes `--json-events`) safe and deterministic.
        let mut owned_flags = vec![self.json_events_arg.clone(), self.run_once_arg.clone()];
        owned_flags.extend(self.dry_run_args.iter().cloned());
        owned_flags.extend(self.workspace_write_args.iter().cloned());
        args.retain(|arg| !owned_flags.iter().any(|owned| owned == arg));
        args.push(self.json_events_arg.clone());
        match mode {
            CodexRunMode::DryRun => args.extend(self.dry_run_args.iter().cloned()),
            CodexRunMode::WorkspaceWrite => args.extend(self.workspace_write_args.iter().cloned()),
        }
        if kind == CodexRunKind::Once {
            args.push(self.run_once_arg.clone());
        }
        ProcessSpec::new(self.executable.clone())
            .args(args)
            .current_dir(self.project_root.clone())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodexWatchState {
    Disabled,
    Stopped,
    Starting,
    Watching,
    Processing,
    Completed,
    Stopping,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodexWatchStatus {
    pub state: CodexWatchState,
    pub enabled: bool,
    pub run_mode: Option<CodexRunMode>,
    pub active_kind: Option<CodexRunKind>,
    pub pid: Option<u32>,
    pub generation: u64,
    pub last_exit_code: Option<i32>,
    pub last_error: Option<String>,
}

impl Default for CodexWatchStatus {
    fn default() -> Self {
        Self {
            state: CodexWatchState::Disabled,
            enabled: false,
            run_mode: None,
            active_kind: None,
            pid: None,
            generation: 0,
            last_exit_code: None,
            last_error: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CodexJsonEvent {
    pub kind: String,
    pub payload: Value,
}

impl CodexJsonEvent {
    /// Current codex-watch uses a status envelope; older peers name the event directly.
    pub fn state_name(&self) -> &str {
        if self.kind == "status" {
            self.payload["state"].as_str().unwrap_or(&self.kind)
        } else {
            &self.kind
        }
    }
}

#[derive(Clone, Debug)]
pub enum CodexWatchEvent {
    Status(CodexWatchStatus),
    Json(CodexJsonEvent),
    Stderr(String),
    Error(String),
    EventsDropped(usize),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CodexWatchError {
    QueueFull,
    WorkerStopped,
}

impl fmt::Display for CodexWatchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::QueueFull => write!(formatter, "codex-watch command queue is full"),
            Self::WorkerStopped => write!(formatter, "codex-watch worker has stopped"),
        }
    }
}

impl std::error::Error for CodexWatchError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CodexEventParseError {
    TooLarge { size: usize, limit: usize },
    InvalidUtf8,
    InvalidJson(String),
    NotAnObject,
    MissingKind,
}

impl fmt::Display for CodexEventParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge { size, limit } => {
                write!(
                    formatter,
                    "codex-watch event is {size} bytes; limit is {limit} bytes"
                )
            }
            Self::InvalidUtf8 => write!(formatter, "codex-watch event is not UTF-8"),
            Self::InvalidJson(error) => write!(formatter, "invalid codex-watch JSON: {error}"),
            Self::NotAnObject => write!(formatter, "codex-watch event is not a JSON object"),
            Self::MissingKind => write!(
                formatter,
                "codex-watch event has no string `type`, `event`, or `kind` field"
            ),
        }
    }
}

impl std::error::Error for CodexEventParseError {}

/// Parse one newline-free JSON event while enforcing the configured byte bound.
pub fn parse_event_line(
    line: &[u8],
    max_event_line_bytes: usize,
) -> Result<CodexJsonEvent, CodexEventParseError> {
    if line.len() > max_event_line_bytes {
        return Err(CodexEventParseError::TooLarge {
            size: line.len(),
            limit: max_event_line_bytes,
        });
    }
    std::str::from_utf8(line).map_err(|_| CodexEventParseError::InvalidUtf8)?;
    let payload: Value = serde_json::from_slice(line)
        .map_err(|error| CodexEventParseError::InvalidJson(error.to_string()))?;
    let object = payload
        .as_object()
        .ok_or(CodexEventParseError::NotAnObject)?;
    let kind = ["type", "event", "kind"]
        .into_iter()
        .find_map(|field| object.get(field).and_then(Value::as_str))
        .filter(|kind| !kind.is_empty())
        .ok_or(CodexEventParseError::MissingKind)?
        .to_owned();
    Ok(CodexJsonEvent { kind, payload })
}

/// Nonblocking handle to one per-project watcher worker.
pub struct CodexWatch {
    config: Arc<CodexWatchConfig>,
    commands: SyncSender<CodexCommand>,
    events: Receiver<CodexWatchEvent>,
    status: Arc<Mutex<CodexWatchStatus>>,
    activity: Arc<Mutex<CodexActivity>>,
    captured_output: Arc<Mutex<BoundedLog>>,
    dropped_events: Arc<AtomicUsize>,
    closing: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl CodexWatch {
    pub fn new(config: CodexWatchConfig) -> std::io::Result<Self> {
        let config = Arc::new(config);
        let (command_tx, command_rx) = mpsc::sync_channel(config.command_queue_capacity.max(1));
        let (event_tx, events) = mpsc::sync_channel(config.event_queue_capacity.max(1));
        let status = Arc::new(Mutex::new(CodexWatchStatus::default()));
        let activity = Arc::new(Mutex::new(CodexActivity::default()));
        let captured_output = Arc::new(Mutex::new(BoundedLog::new(config.captured_output_bytes)));
        let dropped_events = Arc::new(AtomicUsize::new(0));
        let closing = Arc::new(AtomicBool::new(false));

        let worker = CodexWorker {
            config: Arc::clone(&config),
            commands: command_rx,
            publisher: CodexPublisher {
                sender: event_tx,
                dropped: Arc::clone(&dropped_events),
                status: Arc::clone(&status),
                activity: Arc::clone(&activity),
            },
            captured_output: Arc::clone(&captured_output),
            closing: Arc::clone(&closing),
            runtime: None,
            enabled: false,
            run_mode: None,
            generation: 0,
            active_paths: BTreeSet::new(),
            active_path_bytes: 0,
            had_task_failure: false,
        };
        let worker = thread::Builder::new()
            .name("editor-codex-watch".to_owned())
            .spawn(move || worker.run())?;

        Ok(Self {
            config,
            commands: command_tx,
            events,
            status,
            activity,
            captured_output,
            dropped_events,
            closing,
            worker: Some(worker),
        })
    }

    pub fn config(&self) -> &CodexWatchConfig {
        &self.config
    }

    /// Explicitly enable this project.  Enabling alone never starts a process.
    pub fn enable(&self) -> Result<(), CodexWatchError> {
        self.enqueue(CodexCommand::SetEnabled(true))
    }

    pub fn disable(&self) -> Result<(), CodexWatchError> {
        self.enqueue(CodexCommand::SetEnabled(false))
    }

    /// Select the visible execution permission.  The worker only accepts mode changes while the
    /// tool is stopped, so a running process can never silently gain workspace-write access.
    pub fn set_run_mode(&self, mode: CodexRunMode) -> Result<(), CodexWatchError> {
        self.enqueue(CodexCommand::SetRunMode(mode))
    }

    pub fn start(&self) -> Result<(), CodexWatchError> {
        self.enqueue(CodexCommand::Start)
    }

    pub fn stop(&self) -> Result<(), CodexWatchError> {
        self.enqueue(CodexCommand::Stop)
    }

    pub fn restart(&self) -> Result<(), CodexWatchError> {
        self.enqueue(CodexCommand::Restart)
    }

    pub fn run_once(&self) -> Result<(), CodexWatchError> {
        self.enqueue(CodexCommand::RunOnce)
    }

    pub fn status(&self) -> CodexWatchStatus {
        lock_unpoison(&self.status).clone()
    }

    /// Latest line activity survives dropped UI events. Unchanged polls do not
    /// clone paths, and lifecycle resets publish an empty snapshot.
    pub fn working_lines_since(&self, revision: u64) -> Option<(u64, CodexWorkingLines)> {
        lock_unpoison(&self.activity).lines_since(revision)
    }

    pub fn captured_output(&self) -> (String, u64) {
        let output = lock_unpoison(&self.captured_output);
        (output.to_string_lossy(), output.truncated_bytes())
    }

    pub fn clear_captured_output(&self) {
        lock_unpoison(&self.captured_output).clear();
    }

    pub fn try_recv(&self) -> Result<CodexWatchEvent, TryRecvError> {
        let dropped = self.dropped_events.swap(0, Ordering::AcqRel);
        if dropped != 0 {
            return Ok(CodexWatchEvent::EventsDropped(dropped));
        }
        self.events.try_recv()
    }

    pub fn drain_events(&self, limit: usize) -> Vec<CodexWatchEvent> {
        let mut events = Vec::with_capacity(limit.min(32));
        for _ in 0..limit {
            match self.try_recv() {
                Ok(event) => events.push(event),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
        events
    }

    fn enqueue(&self, command: CodexCommand) -> Result<(), CodexWatchError> {
        match self.commands.try_send(command) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(CodexWatchError::QueueFull),
            Err(TrySendError::Disconnected(_)) => Err(CodexWatchError::WorkerStopped),
        }
    }
}

impl Drop for CodexWatch {
    fn drop(&mut self) {
        self.closing.store(true, Ordering::Release);
        let _ = self.commands.try_send(CodexCommand::Wake);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

enum CodexCommand {
    SetEnabled(bool),
    SetRunMode(CodexRunMode),
    Start,
    Stop,
    Restart,
    RunOnce,
    Wake,
}

struct CodexRuntime {
    child: SupervisedChild,
    decoder: BoundedLineDecoder,
    kind: CodexRunKind,
    generation: u64,
    stdout_closed: bool,
    stderr_closed: bool,
    exit_drain_deadline: Option<Instant>,
}

struct CodexPublisher {
    sender: SyncSender<CodexWatchEvent>,
    dropped: Arc<AtomicUsize>,
    status: Arc<Mutex<CodexWatchStatus>>,
    activity: Arc<Mutex<CodexActivity>>,
}

impl CodexPublisher {
    fn emit(&self, event: CodexWatchEvent) {
        match self.sender.try_send(event) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn transition(
        &self,
        state: CodexWatchState,
        enabled: bool,
        run_mode: Option<CodexRunMode>,
        active_kind: Option<CodexRunKind>,
        pid: Option<u32>,
        generation: u64,
        last_exit_code: Option<i32>,
        error: Option<String>,
    ) {
        let snapshot = {
            let mut status = lock_unpoison(&self.status);
            *status = CodexWatchStatus {
                state,
                enabled,
                run_mode,
                active_kind,
                pid,
                generation,
                last_exit_code,
                last_error: error.map(|error| truncate_utf8(&error, MAX_STATUS_ERROR_BYTES)),
            };
            status.clone()
        };
        self.emit(CodexWatchEvent::Status(snapshot));
    }
}

struct CodexWorker {
    config: Arc<CodexWatchConfig>,
    commands: Receiver<CodexCommand>,
    publisher: CodexPublisher,
    captured_output: Arc<Mutex<BoundedLog>>,
    closing: Arc<AtomicBool>,
    runtime: Option<CodexRuntime>,
    enabled: bool,
    run_mode: Option<CodexRunMode>,
    generation: u64,
    // Retain only files being processed, not all files visited by a long-lived watcher.
    active_paths: BTreeSet<String>,
    active_path_bytes: usize,
    had_task_failure: bool,
}

impl CodexWorker {
    fn run(mut self) {
        loop {
            if self.closing.load(Ordering::Acquire) {
                if self.runtime.is_some() {
                    self.stop_current();
                }
                break;
            }

            self.pump();
            let mut handled = 0;
            while handled < 32 {
                match self.commands.try_recv() {
                    Ok(command) => {
                        self.handle_command(command);
                        handled += 1;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        self.closing.store(true, Ordering::Release);
                        break;
                    }
                }
            }
            if handled == 0 {
                match self.commands.recv_timeout(Duration::from_millis(15)) {
                    Ok(command) => self.handle_command(command),
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        self.closing.store(true, Ordering::Release);
                    }
                }
            }
        }
    }

    fn handle_command(&mut self, command: CodexCommand) {
        match command {
            CodexCommand::SetEnabled(enabled) => {
                if enabled == self.enabled {
                    return;
                }
                if !enabled && self.runtime.is_some() {
                    self.stop_current();
                }
                self.enabled = enabled;
                let state = if enabled {
                    CodexWatchState::Stopped
                } else {
                    CodexWatchState::Disabled
                };
                self.transition(state, None, None, None);
            }
            CodexCommand::SetRunMode(mode) => {
                if self.runtime.is_some() {
                    self.report_error(
                        "stop codex-watch before changing dry-run/workspace-write mode".to_owned(),
                    );
                    return;
                }
                self.run_mode = Some(mode);
                self.transition(
                    if self.enabled {
                        CodexWatchState::Stopped
                    } else {
                        CodexWatchState::Disabled
                    },
                    None,
                    None,
                    None,
                );
            }
            CodexCommand::Start => self.start_kind(CodexRunKind::Watch),
            CodexCommand::RunOnce => self.start_kind(CodexRunKind::Once),
            CodexCommand::Stop => self.stop_current(),
            CodexCommand::Restart => {
                if self.runtime.is_some() {
                    self.stop_current();
                }
                self.start_kind(CodexRunKind::Watch);
            }
            CodexCommand::Wake => {}
        }
    }

    fn start_kind(&mut self, kind: CodexRunKind) {
        if self.runtime.is_some() {
            self.report_error("codex-watch is already running".to_owned());
            return;
        }
        if !self.enabled {
            self.report_error(
                "codex-watch is disabled for this project; enable it explicitly first".to_owned(),
            );
            return;
        }
        let Some(mode) = self.run_mode else {
            self.report_error(
                "select dry-run or workspace-write mode before starting codex-watch".to_owned(),
            );
            return;
        };

        self.reset_task_progress();
        self.generation = self.generation.saturating_add(1);
        self.publisher.transition(
            CodexWatchState::Starting,
            self.enabled,
            self.run_mode,
            Some(kind),
            None,
            self.generation,
            None,
            None,
        );
        let child = SupervisedChild::spawn(
            self.config.process_spec(mode, kind),
            self.config.process_limits.clone(),
        );
        match child {
            Ok(child) => {
                let pid = child.pid();
                self.runtime = Some(CodexRuntime {
                    child,
                    decoder: BoundedLineDecoder::new(self.config.max_event_line_bytes),
                    kind,
                    generation: self.generation,
                    stdout_closed: false,
                    stderr_closed: false,
                    exit_drain_deadline: None,
                });
                self.publisher.transition(
                    match kind {
                        CodexRunKind::Watch => CodexWatchState::Watching,
                        CodexRunKind::Once => CodexWatchState::Processing,
                    },
                    self.enabled,
                    self.run_mode,
                    Some(kind),
                    Some(pid),
                    self.generation,
                    None,
                    None,
                );
            }
            Err(error) => {
                let message = format!(
                    "failed to launch {}: {error}",
                    self.config.executable.display()
                );
                self.publisher.transition(
                    CodexWatchState::Failed,
                    self.enabled,
                    self.run_mode,
                    None,
                    None,
                    self.generation,
                    None,
                    Some(message.clone()),
                );
                self.publisher.emit(CodexWatchEvent::Error(message));
            }
        }
    }

    fn stop_current(&mut self) {
        self.reset_task_progress();
        let Some(mut runtime) = self.runtime.take() else {
            self.transition(
                if self.enabled {
                    CodexWatchState::Stopped
                } else {
                    CodexWatchState::Disabled
                },
                None,
                None,
                None,
            );
            return;
        };
        self.publisher.transition(
            CodexWatchState::Stopping,
            self.enabled,
            self.run_mode,
            Some(runtime.kind),
            Some(runtime.child.pid()),
            runtime.generation,
            None,
            None,
        );
        let exit = runtime
            .child
            .terminate(self.config.process_limits.shutdown_timeout);
        let (exit_code, error) = match exit {
            Ok(exit) => (exit.code, None),
            Err(error) => (None, Some(format!("failed stopping codex-watch: {error}"))),
        };
        if let Some(error) = &error {
            self.publisher.emit(CodexWatchEvent::Error(error.clone()));
        }
        self.publisher.transition(
            if self.enabled {
                CodexWatchState::Stopped
            } else {
                CodexWatchState::Disabled
            },
            self.enabled,
            self.run_mode,
            None,
            None,
            runtime.generation,
            exit_code,
            error,
        );
    }

    fn pump(&mut self) {
        let Some(mut runtime) = self.runtime.take() else {
            return;
        };
        let mut fatal_error = None;
        for event in runtime.child.drain_events(64) {
            let result = match event {
                ProcessEvent::WriteError(error) => {
                    Err(format!("failed writing codex-watch stdin: {error}"))
                }
                ProcessEvent::Output {
                    stream: OutputStream::Stdout,
                    bytes,
                } => {
                    lock_unpoison(&self.captured_output).push(&bytes);
                    self.consume_stdout(&mut runtime, &bytes)
                }
                ProcessEvent::Output {
                    stream: OutputStream::Stderr,
                    bytes,
                } => {
                    lock_unpoison(&self.captured_output).push(&bytes);
                    self.publisher.emit(CodexWatchEvent::Stderr(
                        String::from_utf8_lossy(&bytes).into_owned(),
                    ));
                    Ok(())
                }
                ProcessEvent::OutputDropped {
                    stream: OutputStream::Stdout,
                    events,
                } => Err(format!(
                    "codex-watch stdout overflowed; dropped {events} JSON chunks"
                )),
                ProcessEvent::OutputDropped {
                    stream: OutputStream::Stderr,
                    events,
                } => {
                    self.publisher.emit(CodexWatchEvent::Error(format!(
                        "codex-watch stderr overflowed; dropped {events} chunks"
                    )));
                    Ok(())
                }
                ProcessEvent::ReadError {
                    stream: OutputStream::Stdout,
                    error,
                } => Err(format!("failed reading codex-watch stdout: {error}")),
                ProcessEvent::ReadError {
                    stream: OutputStream::Stderr,
                    error,
                } => {
                    runtime.stderr_closed = true;
                    self.publisher.emit(CodexWatchEvent::Error(format!(
                        "failed reading codex-watch stderr: {error}"
                    )));
                    Ok(())
                }
                ProcessEvent::Eof(OutputStream::Stdout) => {
                    runtime.stdout_closed = true;
                    match runtime.decoder.finish() {
                        Some(line) if !line.is_empty() => self.consume_line(&line),
                        _ => Ok(()),
                    }
                }
                ProcessEvent::Eof(OutputStream::Stderr) => {
                    runtime.stderr_closed = true;
                    Ok(())
                }
            };
            if let Err(error) = result {
                fatal_error = Some(error);
                break;
            }
        }

        if let Some(error) = fatal_error {
            self.reset_task_progress();
            let generation = runtime.generation;
            let _ = runtime.child.terminate(Duration::ZERO);
            self.publisher.transition(
                CodexWatchState::Failed,
                self.enabled,
                self.run_mode,
                None,
                None,
                generation,
                None,
                Some(error.clone()),
            );
            self.publisher.emit(CodexWatchEvent::Error(error));
            return;
        }

        match runtime.child.try_wait() {
            Ok(None) => self.runtime = Some(runtime),
            Ok(Some(exit)) => {
                // Process exit does not mean the pipe reader has delivered all
                // bytes. Keep pumping bounded batches through both EOFs so a
                // final event or stderr error cannot disappear after a fast
                // exit. Descendants retaining pipes cannot defer exit forever.
                if !runtime.stdout_closed || !runtime.stderr_closed {
                    let deadline = *runtime.exit_drain_deadline.get_or_insert_with(|| {
                        Instant::now()
                            + self
                                .config
                                .process_limits
                                .shutdown_timeout
                                .max(Duration::from_secs(2))
                    });
                    if Instant::now() < deadline {
                        self.runtime = Some(runtime);
                        return;
                    }
                    let error = "codex-watch output remained open after process exit".to_owned();
                    self.reset_task_progress();
                    let _ = runtime.child.terminate(Duration::ZERO);
                    self.publisher.transition(
                        CodexWatchState::Failed,
                        self.enabled,
                        self.run_mode,
                        None,
                        None,
                        runtime.generation,
                        exit.code,
                        Some(error.clone()),
                    );
                    self.publisher.emit(CodexWatchEvent::Error(error));
                    return;
                }
                let previous_state = lock_unpoison(&self.publisher.status).state;
                let (state, error) = match runtime.kind {
                    CodexRunKind::Once
                        if exit.success
                            && !self.had_task_failure
                            && previous_state != CodexWatchState::Failed =>
                    {
                        (CodexWatchState::Completed, None)
                    }
                    CodexRunKind::Once
                        if self.had_task_failure || previous_state == CodexWatchState::Failed =>
                    {
                        (
                            CodexWatchState::Failed,
                            Some("codex-watch reported a failed task".to_owned()),
                        )
                    }
                    CodexRunKind::Once => (
                        CodexWatchState::Failed,
                        Some(exit_description("codex-watch run", exit.code)),
                    ),
                    CodexRunKind::Watch => (
                        CodexWatchState::Failed,
                        Some(exit_description(
                            "codex-watch exited unexpectedly",
                            exit.code,
                        )),
                    ),
                };
                if let Some(error) = &error {
                    self.publisher.emit(CodexWatchEvent::Error(error.clone()));
                }
                self.reset_task_progress();
                self.publisher.transition(
                    state,
                    self.enabled,
                    self.run_mode,
                    None,
                    None,
                    runtime.generation,
                    exit.code,
                    error,
                );
            }
            Err(error) => {
                self.reset_task_progress();
                let message = format!("failed to inspect codex-watch process: {error}");
                let _ = runtime.child.terminate(Duration::ZERO);
                self.publisher.transition(
                    CodexWatchState::Failed,
                    self.enabled,
                    self.run_mode,
                    None,
                    None,
                    runtime.generation,
                    None,
                    Some(message.clone()),
                );
                self.publisher.emit(CodexWatchEvent::Error(message));
            }
        }
    }

    fn consume_stdout(&mut self, runtime: &mut CodexRuntime, bytes: &[u8]) -> Result<(), String> {
        let lines = runtime
            .decoder
            .push(bytes)
            .map_err(|error| format!("invalid codex-watch event stream: {error}"))?;
        for line in lines {
            if !line.is_empty() {
                self.consume_line(&line)?;
            }
        }
        Ok(())
    }

    fn consume_line(&mut self, line: &[u8]) -> Result<(), String> {
        let event = parse_event_line(line, self.config.max_event_line_bytes)
            .map_err(|error| format!("malformed codex-watch event: {error}"))?;
        self.apply_event_state(&event)?;
        if event.kind == "status"
            && let Some(path) = event.payload["path"]
                .as_str()
                .filter(|path| !path.is_empty())
        {
            let path = self.config.project_root.join(Path::new(path));
            // Resolve aliases on the worker, never on the input/render thread.
            let path = std::fs::canonicalize(&path).unwrap_or(path);
            lock_unpoison(&self.publisher.activity).update(&event, path)?;
        }
        self.publisher.emit(CodexWatchEvent::Json(event));
        Ok(())
    }

    fn reset_task_progress(&mut self) {
        lock_unpoison(&self.publisher.activity).clear();
        self.active_paths.clear();
        self.active_path_bytes = 0;
        self.had_task_failure = false;
    }

    fn apply_event_state(&mut self, event: &CodexJsonEvent) -> Result<(), String> {
        let normalized = event.state_name().to_ascii_lowercase().replace('-', "_");
        let current = lock_unpoison(&self.publisher.status).clone();
        let state = if event.kind == "status" {
            let path = event.payload["path"].as_str().unwrap_or("");
            match normalized.as_str() {
                "preparing" | "waiting" | "retrying" => {
                    if !self.active_paths.contains(path) {
                        if self.active_paths.len() >= MAX_ACTIVE_PATHS
                            || path.len() > MAX_ACTIVE_PATH_BYTES - self.active_path_bytes
                        {
                            return Err(
                                "codex-watch active task tracking limit exceeded".to_owned()
                            );
                        }
                        self.active_paths.insert(path.to_owned());
                        self.active_path_bytes += path.len();
                    }
                    Some(CodexWatchState::Processing)
                }
                "applied" | "previewed" | "failed" | "idle" => {
                    let removed = self.active_paths.remove(path);
                    if removed {
                        self.active_path_bytes -= path.len();
                    }
                    if !self.active_paths.is_empty() {
                        Some(CodexWatchState::Processing)
                    } else {
                        match normalized.as_str() {
                            "applied" | "previewed" => Some(CodexWatchState::Completed),
                            "failed" => Some(CodexWatchState::Failed),
                            "idle" if removed => Some(CodexWatchState::Watching),
                            // A scan of an unrelated file must not erase the latest
                            // completion/error or interrupt a legacy processing event.
                            _ => None,
                        }
                    }
                }
                _ => None,
            }
        } else if normalized.contains("watch") && normalized.contains("start") {
            Some(CodexWatchState::Watching)
        } else if normalized.contains("fail") || normalized == "error" {
            Some(CodexWatchState::Failed)
        } else if normalized.contains("complete")
            || normalized.contains("finish")
            || normalized == "success"
        {
            Some(CodexWatchState::Completed)
        } else if normalized.contains("process")
            || normalized.contains("task_start")
            || normalized == "started"
            || normalized == "running"
        {
            Some(CodexWatchState::Processing)
        } else {
            None
        };
        if let Some(state) = state {
            let failed = normalized == "failed" || state == CodexWatchState::Failed;
            self.had_task_failure |= failed;
            self.publisher.transition(
                state,
                current.enabled,
                current.run_mode,
                current.active_kind,
                current.pid,
                current.generation,
                current.last_exit_code,
                if failed { event_message(event) } else { None },
            );
        }
        Ok(())
    }

    fn report_error(&self, error: String) {
        self.publisher.emit(CodexWatchEvent::Error(error));
    }

    fn transition(
        &self,
        state: CodexWatchState,
        active_kind: Option<CodexRunKind>,
        exit_code: Option<i32>,
        error: Option<String>,
    ) {
        self.publisher.transition(
            state,
            self.enabled,
            self.run_mode,
            active_kind,
            self.runtime.as_ref().map(|runtime| runtime.child.pid()),
            self.generation,
            exit_code,
            error,
        );
    }
}

fn event_message(event: &CodexJsonEvent) -> Option<String> {
    let object = event.payload.as_object()?;
    ["message", "error", "reason"]
        .into_iter()
        .find_map(|field| object.get(field).and_then(Value::as_str))
        .map(ToOwned::to_owned)
}

fn exit_description(context: &str, code: Option<i32>) -> String {
    match code {
        Some(code) => format!("{context} with code {code}"),
        None => format!("{context} after being terminated"),
    }
}

fn truncate_utf8(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let ellipsis = "…";
    let suffix = if max_bytes >= ellipsis.len() {
        ellipsis
    } else {
        ""
    };
    let mut end = max_bytes - suffix.len();
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{suffix}", &text[..end])
}

fn lock_unpoison<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Instant;

    fn worker(config: CodexWatchConfig) -> (CodexWorker, Receiver<CodexWatchEvent>) {
        let (_commands, command_rx) = mpsc::sync_channel(4);
        let (events, event_rx) = mpsc::sync_channel(config.event_queue_capacity.max(1));
        let config = Arc::new(config);
        let worker = CodexWorker {
            captured_output: Arc::new(Mutex::new(BoundedLog::new(config.captured_output_bytes))),
            config,
            commands: command_rx,
            publisher: CodexPublisher {
                sender: events,
                dropped: Arc::new(AtomicUsize::new(0)),
                status: Arc::new(Mutex::new(CodexWatchStatus::default())),
                activity: Arc::new(Mutex::new(CodexActivity::default())),
            },
            closing: Arc::new(AtomicBool::new(false)),
            runtime: None,
            enabled: false,
            run_mode: None,
            generation: 0,
            active_paths: BTreeSet::new(),
            active_path_bytes: 0,
            had_task_failure: false,
        };
        (worker, event_rx)
    }

    fn wait_until(mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !condition() {
            assert!(Instant::now() < deadline, "codex-watch test timed out");
            thread::sleep(Duration::from_millis(1));
        }
    }

    fn wait_error(watch: &CodexWatch, expected: &str) {
        wait_until(|| {
            watch.drain_events(128).iter().any(|event| {
            matches!(event, CodexWatchEvent::Error(message) if message.contains(expected))
        })
        });
    }

    fn task_status(worker: &mut CodexWorker, state: &str, path: &str) -> CodexWatchStatus {
        worker
            .consume_line(
                serde_json::to_string(&json!({
                    "version": 1, "type": "status", "state": state, "path": path,
                    "line": 89, "task": 1, "message": "task detail"
                }))
                .unwrap()
                .as_bytes(),
            )
            .unwrap();
        lock_unpoison(&worker.publisher.status).clone()
    }

    #[test]
    fn working_lines_track_tasks_and_survive_dropped_ui_events() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = CodexWatchConfig::new(directory.path());
        config.event_queue_capacity = 1;
        let (mut worker, _events) = worker(config);
        let send = |worker: &mut CodexWorker, state, task, line| {
            worker.consume_line(serde_json::to_string(&serde_json::json!({
                "type": "status", "state": state, "path": "src.rs", "task": task, "line": line,
            })).unwrap().as_bytes()).unwrap();
        };
        send(&mut worker, "preparing", Some(1), Some(2));
        send(&mut worker, "waiting", Some(2), Some(5));
        send(&mut worker, "retrying", Some(1), None);
        assert_eq!(
            lock_unpoison(&worker.publisher.activity)
                .lines_since(0)
                .unwrap()
                .1[&directory.path().join("src.rs")],
            BTreeSet::from([1, 4])
        );
        assert!(worker.publisher.dropped.load(Ordering::Relaxed) > 0);
        send(&mut worker, "applied", Some(1), Some(2));
        assert_eq!(
            lock_unpoison(&worker.publisher.activity)
                .lines_since(0)
                .unwrap()
                .1[&directory.path().join("src.rs")],
            BTreeSet::from([4])
        );
        send(&mut worker, "failed", Some(2), None);
        assert!(lock_unpoison(&worker.publisher.activity).tasks.is_empty());
        send(&mut worker, "queued", Some(3), Some(0));
        send(&mut worker, "waiting", Some(3), None);
        assert!(lock_unpoison(&worker.publisher.activity).tasks.is_empty());
        send(&mut worker, "waiting", Some(3), Some(7));
        send(&mut worker, "idle", None, None);
        assert!(lock_unpoison(&worker.publisher.activity).tasks.is_empty());
        send(&mut worker, "waiting", Some(4), Some(9));
        worker.stop_current();
        let activity = lock_unpoison(&worker.publisher.activity);
        assert!(activity.lines_since(0).unwrap().1.is_empty());
        assert!(activity.lines_since(activity.revision).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn working_lines_resolve_relative_paths_and_symlinks_on_the_worker() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("source.rs");
        std::fs::write(&file, "// @codex fix this\n").unwrap();
        std::os::unix::fs::symlink(&file, directory.path().join("alias.rs")).unwrap();
        let (mut worker, _events) = worker(CodexWatchConfig::new(directory.path()));
        task_status(&mut worker, "preparing", "alias.rs");
        assert!(
            lock_unpoison(&worker.publisher.activity)
                .lines_since(0)
                .unwrap()
                .1
                .contains_key(&file.canonicalize().unwrap())
        );
        task_status(&mut worker, "previewed", "./source.rs");
        assert!(lock_unpoison(&worker.publisher.activity).tasks.is_empty());
    }

    #[test]
    fn working_line_tracking_bounds_multiple_tasks_in_one_file() {
        let mut activity = CodexActivity::default();
        for task in 0..MAX_ACTIVE_PATHS {
            let event = CodexJsonEvent {
                kind: "status".into(),
                payload: serde_json::json!({
                    "state": "waiting", "task": task, "line": task + 1,
                }),
            };
            activity
                .update(&event, PathBuf::from("/project/a.rs"))
                .unwrap();
        }
        let event = CodexJsonEvent {
            kind: "status".into(),
            payload: serde_json::json!({
                "state": "waiting", "task": MAX_ACTIVE_PATHS, "line": 1,
            }),
        };
        assert!(
            activity
                .update(&event, PathBuf::from("/project/a.rs"))
                .unwrap_err()
                .contains("limit")
        );
        activity.clear();
        assert_eq!(activity.path_bytes, 0);
        assert!(
            activity
                .update(&event, PathBuf::from("a".repeat(MAX_ACTIVE_PATH_BYTES + 1)))
                .unwrap_err()
                .contains("limit")
        );
    }

    #[test]
    fn real_status_events_track_overlapping_tasks_without_idle_hiding_work() {
        let directory = tempfile::tempdir().unwrap();
        let (mut worker, events) = worker(CodexWatchConfig::new(directory.path()));
        for state in ["preparing", "waiting", "retrying"] {
            assert_eq!(
                task_status(&mut worker, state, "/project/a.rs").state,
                CodexWatchState::Processing
            );
        }
        assert_eq!(worker.active_paths.len(), 1);
        assert_eq!(worker.active_path_bytes, "/project/a.rs".len());
        for state in ["queued", "idle", "future_state"] {
            assert_eq!(
                task_status(&mut worker, state, "/project/unrelated.rs").state,
                CodexWatchState::Processing
            );
        }
        task_status(&mut worker, "preparing", "/project/b.rs");
        assert_eq!(
            task_status(&mut worker, "applied", "/project/a.rs").state,
            CodexWatchState::Processing
        );
        assert_eq!(
            task_status(&mut worker, "previewed", "/project/b.rs").state,
            CodexWatchState::Completed
        );
        assert!(worker.active_paths.is_empty());
        assert_eq!(worker.active_path_bytes, 0);
        assert!(events.try_iter().any(|event| matches!(event,
            CodexWatchEvent::Json(event) if event.kind == "status"
                && event.state_name() == "applied" && event.payload["line"] == 89
        )));

        task_status(&mut worker, "preparing", "/project/a.rs");
        task_status(&mut worker, "retrying", "/project/a.rs");
        assert_eq!(
            task_status(&mut worker, "idle", "/project/a.rs").state,
            CodexWatchState::Watching
        );
        assert!(worker.active_paths.is_empty());
    }

    #[test]
    fn task_failures_remain_visible_while_other_files_are_processing() {
        let directory = tempfile::tempdir().unwrap();
        let (mut worker, _events) = worker(CodexWatchConfig::new(directory.path()));
        task_status(&mut worker, "preparing", "a.rs");
        task_status(&mut worker, "preparing", "b.rs");
        let status = task_status(&mut worker, "failed", "a.rs");
        assert_eq!(status.state, CodexWatchState::Processing);
        assert_eq!(status.last_error.as_deref(), Some("task detail"));
        assert!(worker.had_task_failure);
        assert_eq!(
            task_status(&mut worker, "failed", "b.rs").state,
            CodexWatchState::Failed
        );
        assert_eq!(
            task_status(&mut worker, "idle", "unrelated.rs").state,
            CodexWatchState::Failed
        );
        worker.stop_current();
        assert!(!worker.had_task_failure);
        assert!(worker.active_paths.is_empty());
    }

    #[test]
    fn active_task_tracking_bounds_paths_and_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = CodexWatchConfig::new(directory.path());
        config.max_event_line_bytes = MAX_ACTIVE_PATH_BYTES + 256;
        let (mut worker, _events) = worker(config);
        for index in 0..MAX_ACTIVE_PATHS {
            task_status(&mut worker, "preparing", &format!("{index}.rs"));
        }
        let event = parse_event_line(
            br#"{"type":"status","state":"preparing","path":"excess.rs"}"#,
            256,
        )
        .unwrap();
        assert!(
            worker
                .apply_event_state(&event)
                .unwrap_err()
                .contains("limit")
        );
        assert_eq!(worker.active_paths.len(), MAX_ACTIVE_PATHS);
        worker.stop_current();
        assert!(worker.active_paths.is_empty());
        assert_eq!(worker.active_path_bytes, 0);

        let path = format!("/{}", "a".repeat(MAX_ACTIVE_PATH_BYTES - 1));
        task_status(&mut worker, "preparing", &path);
        task_status(&mut worker, "waiting", &path);
        assert_eq!(worker.active_paths.len(), 1);
        assert!(
            worker
                .apply_event_state(&event)
                .unwrap_err()
                .contains("limit")
        );
        task_status(&mut worker, "idle", &path);
        assert_eq!(worker.active_path_bytes, 0);
        worker.apply_event_state(&event).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn once_run_preserves_an_earlier_task_failure_and_restart_clears_progress() {
        let directory = tempfile::tempdir().unwrap();
        let (mut worker, _events) = worker(echo_config(directory.path()));
        worker.enabled = true;
        worker.run_mode = Some(CodexRunMode::DryRun);
        worker.start_kind(CodexRunKind::Once);
        let runtime = worker.runtime.as_mut().unwrap();
        runtime.child.write_all(concat!(
            "{\"version\":1,\"type\":\"status\",\"state\":\"preparing\",\"path\":\"a.rs\"}\n",
            "{\"version\":1,\"type\":\"status\",\"state\":\"preparing\",\"path\":\"b.rs\"}\n",
            "{\"version\":1,\"type\":\"status\",\"state\":\"failed\",\"path\":\"a.rs\",\"message\":\"failed a\"}\n",
            "{\"version\":1,\"type\":\"status\",\"state\":\"applied\",\"path\":\"b.rs\"}\n"
        ).as_bytes()).unwrap();
        runtime.child.close_stdin();
        wait_until(|| {
            worker.pump();
            worker.runtime.is_none()
        });
        let status = lock_unpoison(&worker.publisher.status).clone();
        assert_eq!(status.last_exit_code, Some(0));
        assert_eq!(status.state, CodexWatchState::Failed);
        assert!(status.last_error.unwrap().contains("failed task"));
        assert!(worker.active_paths.is_empty());
        worker.start_kind(CodexRunKind::Watch);
        assert!(!worker.had_task_failure);
        task_status(&mut worker, "waiting", "unfinished.rs");
        worker.handle_command(CodexCommand::Restart);
        assert!(worker.active_paths.is_empty());
        assert_eq!(worker.active_path_bytes, 0);
        assert_eq!(
            lock_unpoison(&worker.publisher.status).state,
            CodexWatchState::Watching
        );
        worker.stop_current();
    }

    #[cfg(unix)]
    fn echo_config(root: &std::path::Path) -> CodexWatchConfig {
        // A pipe echo process lets tests supply event bytes directly without
        // installing codex-watch or changing inherited environment/configuration.
        let mut config = CodexWatchConfig::new(root);
        config.executable = PathBuf::from("/bin/cat");
        config.json_events_arg = "-".into();
        config.run_once_arg = "-".into();
        config.dry_run_args.clear();
        config.workspace_write_args.clear();
        config.process_limits.shutdown_timeout = Duration::from_millis(50);
        config
    }

    #[test]
    fn controller_requires_enablement_and_explicit_mode_before_spawning() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = CodexWatchConfig::new(directory.path());
        config.executable = directory.path().join("missing-codex-watch");
        let watch = CodexWatch::new(config).unwrap();
        assert_eq!(watch.config().project_root, directory.path());
        assert_eq!(watch.status(), CodexWatchStatus::default());
        watch.start().unwrap();
        wait_error(&watch, "disabled for this project");
        assert_eq!(watch.status().generation, 0);
        watch.enable().unwrap();
        watch.enable().unwrap();
        watch.start().unwrap();
        wait_error(&watch, "select dry-run or workspace-write");
        assert_eq!(watch.status().generation, 0);
        assert_eq!(watch.status().state, CodexWatchState::Stopped);
        watch.set_run_mode(CodexRunMode::DryRun).unwrap();
        watch.run_once().unwrap();
        wait_until(|| watch.status().state == CodexWatchState::Failed);
        let status = watch.status();
        assert!(status.last_error.unwrap().contains("failed to launch"));
        assert!(status.pid.is_none());
        assert_eq!(status.generation, 1);
        watch.disable().unwrap();
        watch.stop().unwrap();
        wait_until(|| watch.status().state == CodexWatchState::Disabled);
    }

    #[cfg(unix)]
    #[test]
    fn running_controller_rejects_permission_changes_and_supports_restart_stop_disable() {
        let directory = tempfile::tempdir().unwrap();
        let watch = CodexWatch::new(echo_config(directory.path())).unwrap();
        watch.set_run_mode(CodexRunMode::DryRun).unwrap();
        wait_until(|| watch.status().run_mode == Some(CodexRunMode::DryRun));
        assert_eq!(watch.status().state, CodexWatchState::Disabled);
        watch.enable().unwrap();
        watch.start().unwrap();
        wait_until(|| watch.status().state == CodexWatchState::Watching);
        let first = watch.status();
        assert!(first.pid.is_some());
        assert_eq!(first.active_kind, Some(CodexRunKind::Watch));
        watch.set_run_mode(CodexRunMode::WorkspaceWrite).unwrap();
        wait_error(&watch, "stop codex-watch before changing");
        assert_eq!(watch.status().run_mode, Some(CodexRunMode::DryRun));
        watch.start().unwrap();
        wait_error(&watch, "already running");
        watch.restart().unwrap();
        wait_until(|| {
            watch.status().generation == 2 && watch.status().state == CodexWatchState::Watching
        });
        assert_ne!(watch.status().pid, first.pid);
        watch.stop().unwrap();
        wait_until(|| watch.status().state == CodexWatchState::Stopped);
        assert!(watch.status().pid.is_none());
        watch.set_run_mode(CodexRunMode::WorkspaceWrite).unwrap();
        watch.restart().unwrap();
        wait_until(|| {
            watch.status().generation == 3 && watch.status().state == CodexWatchState::Watching
        });
        assert_eq!(watch.status().run_mode, Some(CodexRunMode::WorkspaceWrite));
        watch.disable().unwrap();
        wait_until(|| watch.status().state == CodexWatchState::Disabled);
        assert!(watch.status().pid.is_none());
        assert!(!watch.status().enabled);
        watch.enable().unwrap();
        watch.start().unwrap();
        wait_until(|| watch.status().state == CodexWatchState::Watching);
        let status = Arc::clone(&watch.status);
        drop(watch);
        assert_eq!(lock_unpoison(&status).state, CodexWatchState::Stopped);
        assert!(lock_unpoison(&status).pid.is_none());
    }

    #[test]
    fn json_lifecycle_preserves_payload_and_bounds_utf8_status_errors() {
        let directory = tempfile::tempdir().unwrap();
        let (mut worker, events) = worker(CodexWatchConfig::new(directory.path()));
        for (kind, state) in [
            ("watch-started", CodexWatchState::Watching),
            ("task_start", CodexWatchState::Processing),
            ("completed", CodexWatchState::Completed),
            ("running", CodexWatchState::Processing),
            ("success", CodexWatchState::Completed),
        ] {
            worker
                .consume_line(
                    serde_json::to_string(&json!({"type": kind, "task_id": 9}))
                        .unwrap()
                        .as_bytes(),
                )
                .unwrap();
            assert_eq!(lock_unpoison(&worker.publisher.status).state, state);
            let delivered: Vec<_> = events.try_iter().collect();
            assert!(delivered.iter().any(|event| matches!(event, CodexWatchEvent::Json(event) if event.kind == kind && event.payload["task_id"] == 9)));
        }
        worker
            .consume_line(br#"{"type":"progress","percent":50}"#)
            .unwrap();
        assert_eq!(
            lock_unpoison(&worker.publisher.status).state,
            CodexWatchState::Completed
        );
        let message = "é".repeat(MAX_STATUS_ERROR_BYTES);
        worker
            .consume_line(
                serde_json::to_string(&json!({"type":"failed", "reason":message}))
                    .unwrap()
                    .as_bytes(),
            )
            .unwrap();
        let status = lock_unpoison(&worker.publisher.status).clone();
        assert_eq!(status.state, CodexWatchState::Failed);
        let error = status.last_error.unwrap();
        assert!(error.len() <= MAX_STATUS_ERROR_BYTES);
        assert!(error.ends_with('…'));
        assert!(
            worker
                .consume_line(&[0xff])
                .unwrap_err()
                .contains("not UTF-8")
        );
        assert!(
            worker
                .consume_line(b"not JSON")
                .unwrap_err()
                .contains("invalid codex-watch JSON")
        );
        assert_eq!(
            parse_event_line(br#"{"type":""}"#, 128),
            Err(CodexEventParseError::MissingKind)
        );
    }

    #[cfg(unix)]
    #[test]
    fn pump_decodes_fragmented_events_and_rejects_oversized_streams() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = echo_config(directory.path());
        config.max_event_line_bytes = 128;
        config.process_limits.output_chunk_bytes = 3;
        let (mut worker, events) = worker(config);
        worker.handle_command(CodexCommand::SetEnabled(true));
        worker.handle_command(CodexCommand::SetRunMode(CodexRunMode::DryRun));
        worker.handle_command(CodexCommand::Start);
        worker
            .runtime
            .as_mut()
            .unwrap()
            .child
            .write_all(b"\n{\"type\":\"task_started\",\"id\":42}\r\n")
            .unwrap();
        wait_until(|| {
            worker.pump();
            events.try_iter().any(
                |event| matches!(event, CodexWatchEvent::Json(event) if event.payload["id"] == 42),
            )
        });
        assert_eq!(
            lock_unpoison(&worker.publisher.status).state,
            CodexWatchState::Processing
        );
        worker
            .runtime
            .as_mut()
            .unwrap()
            .child
            .write_all(&[b'x'; 129])
            .unwrap();
        wait_until(|| {
            worker.pump();
            worker.runtime.is_none()
        });
        let status = lock_unpoison(&worker.publisher.status).clone();
        assert_eq!(status.state, CodexWatchState::Failed);
        assert!(
            status
                .last_error
                .unwrap()
                .contains("invalid codex-watch event stream")
        );
    }

    #[test]
    fn bounded_ui_queue_reports_loss_without_losing_current_status() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = CodexWatchConfig::new(directory.path());
        config.event_queue_capacity = 1;
        config.captured_output_bytes = 4;
        let watch = CodexWatch::new(config).unwrap();
        watch.set_run_mode(CodexRunMode::DryRun).unwrap();
        watch.enable().unwrap();
        wait_until(|| watch.status().enabled && watch.dropped_events.load(Ordering::Acquire) > 0);
        assert!(matches!(
            watch.try_recv(),
            Ok(CodexWatchEvent::EventsDropped(_))
        ));
        assert_eq!(watch.status().state, CodexWatchState::Stopped);
        assert_eq!(watch.drain_events(1).len(), 1);
        lock_unpoison(&watch.captured_output).push(b"123456");
        assert_eq!(watch.captured_output(), ("3456".into(), 2));
        watch.clear_captured_output();
        assert_eq!(watch.captured_output(), (String::new(), 0));
    }

    #[cfg(unix)]
    #[test]
    fn completed_process_drains_its_final_unterminated_json_event() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = echo_config(directory.path());
        config.process_limits.output_chunk_bytes = 1;
        config.process_limits.event_queue_capacity = 4096;
        let (mut worker, events) = worker(config);
        worker.handle_command(CodexCommand::SetEnabled(true));
        worker.handle_command(CodexCommand::SetRunMode(CodexRunMode::DryRun));
        worker.handle_command(CodexCommand::RunOnce);
        let mut payload = b"{\"type\":\"progress\"}\n".repeat(8);
        payload.extend_from_slice(br#"{"type":"completed","task_id":99}"#);
        let runtime = worker.runtime.as_mut().unwrap();
        runtime.child.write_all(&payload).unwrap();
        runtime.child.close_stdin();
        wait_until(|| runtime.child.try_wait().unwrap().is_some());
        wait_until(|| {
            worker.pump();
            worker.runtime.is_none()
        });
        let final_status = lock_unpoison(&worker.publisher.status).clone();
        assert_eq!(final_status.state, CodexWatchState::Completed);
        assert_eq!(final_status.last_exit_code, Some(0));
        assert!(
            events.try_iter().any(|event| matches!(event,
                CodexWatchEvent::Json(event) if event.payload["task_id"] == 99
            )),
            "process exit must not discard unread final stdout"
        );
    }

    #[cfg(unix)]
    #[test]
    fn failed_tasks_unexpected_exits_and_malformed_final_events_are_reported() {
        let directory = tempfile::tempdir().unwrap();
        for (kind, payload, expected) in [
            (
                CodexRunKind::Once,
                &br#"{"type":"task_failed","message":"task rejected"}"#[..],
                "reported a failed task",
            ),
            (
                CodexRunKind::Watch,
                &b""[..],
                "exited unexpectedly with code 0",
            ),
            (
                CodexRunKind::Once,
                &b"invalid JSON"[..],
                "malformed codex-watch event",
            ),
        ] {
            let (mut worker, events) = worker(echo_config(directory.path()));
            worker.enabled = true;
            worker.run_mode = Some(CodexRunMode::DryRun);
            worker.start_kind(kind);
            let runtime = worker.runtime.as_mut().unwrap();
            runtime.child.write_all(payload).unwrap();
            runtime.child.close_stdin();
            wait_until(|| {
                worker.pump();
                worker.runtime.is_none()
            });
            assert_eq!(
                lock_unpoison(&worker.publisher.status).state,
                CodexWatchState::Failed
            );
            assert!(
                lock_unpoison(&worker.publisher.status)
                    .last_error
                    .as_ref()
                    .unwrap()
                    .contains(expected)
            );
            assert!(events.try_iter().any(|event| matches!(event, CodexWatchEvent::Error(message) if message.contains(expected))));
        }
    }

    #[cfg(unix)]
    #[test]
    fn nonzero_exit_preserves_captured_stderr() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = echo_config(directory.path());
        config
            .args
            .push(directory.path().join("missing-input").into_os_string());
        let (mut worker, events) = worker(config);
        worker.enabled = true;
        worker.run_mode = Some(CodexRunMode::DryRun);
        worker.start_kind(CodexRunKind::Once);
        // Wait for stderr before closing stdin to isolate output capture from
        // platform-dependent EOF ordering between the two pipe reader threads.
        wait_until(|| {
            worker.pump();
            lock_unpoison(&worker.captured_output)
                .to_string_lossy()
                .contains("missing-input")
        });
        worker.runtime.as_mut().unwrap().child.close_stdin();
        wait_until(|| {
            worker.pump();
            worker.runtime.is_none()
        });
        let status = lock_unpoison(&worker.publisher.status).clone();
        assert_eq!(status.state, CodexWatchState::Failed);
        assert_eq!(status.last_exit_code, Some(1));
        assert!(status.last_error.unwrap().contains("with code 1"));
        assert!(events.try_iter().any(|event| matches!(event,
            CodexWatchEvent::Stderr(message) if message.contains("missing-input")
        )));
    }

    #[test]
    fn controller_reports_full_and_disconnected_command_queues() {
        let directory = tempfile::tempdir().unwrap();
        let (commands, receiver) = mpsc::sync_channel(1);
        let (_event_sender, events) = mpsc::channel();
        let watch = CodexWatch {
            config: Arc::new(CodexWatchConfig::new(directory.path())),
            commands,
            events,
            status: Arc::new(Mutex::new(CodexWatchStatus::default())),
            activity: Arc::new(Mutex::new(CodexActivity::default())),
            captured_output: Arc::new(Mutex::new(BoundedLog::new(16))),
            dropped_events: Arc::new(AtomicUsize::new(0)),
            closing: Arc::new(AtomicBool::new(false)),
            worker: None,
        };
        watch.start().unwrap();
        assert_eq!(watch.stop(), Err(CodexWatchError::QueueFull));
        assert!(
            CodexWatchError::QueueFull
                .to_string()
                .contains("queue is full")
        );
        drop(receiver);
        assert_eq!(watch.stop(), Err(CodexWatchError::WorkerStopped));
        assert!(
            CodexWatchError::WorkerStopped
                .to_string()
                .contains("worker has stopped")
        );
    }

    #[cfg(unix)]
    #[test]
    fn exited_process_drains_stderr_after_stdout_has_closed() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = echo_config(directory.path());
        config.process_limits.output_chunk_bytes = 1;
        config.process_limits.event_queue_capacity = 16_384;
        for index in 0..8 {
            config.args.push(
                directory
                    .path()
                    .join(format!("missing-input-{index}"))
                    .into_os_string(),
            );
        }
        let (mut worker, events) = worker(config);
        worker.enabled = true;
        worker.run_mode = Some(CodexRunMode::DryRun);
        worker.start_kind(CodexRunKind::Once);
        let runtime = worker.runtime.as_mut().unwrap();
        runtime.child.close_stdin();
        wait_until(|| runtime.child.try_wait().unwrap().is_some());
        // Force the valid interleaving where an earlier pump saw stdout EOF
        // before the independent stderr reader finished queuing its chunks.
        runtime.stdout_closed = true;
        worker.pump();
        assert!(
            worker.runtime.is_some(),
            "stderr still has more than one batch to drain"
        );
        let mut stderr = String::new();
        wait_until(|| {
            worker.pump();
            for event in events.try_iter() {
                if let CodexWatchEvent::Stderr(chunk) = event {
                    stderr.push_str(&chunk);
                }
            }
            worker.runtime.is_none()
        });
        let captured = lock_unpoison(&worker.captured_output).to_string_lossy();
        for index in 0..8 {
            let filename = format!("missing-input-{index}");
            assert!(
                captured.contains(&filename),
                "captured output lost {filename}"
            );
            assert!(stderr.contains(&filename), "UI output lost {filename}");
        }
        assert_eq!(
            lock_unpoison(&worker.publisher.status).last_exit_code,
            Some(1)
        );
    }

    #[test]
    fn parses_type_event_and_preserves_payload() {
        let event = parse_event_line(br#"{"type":"task_started","task_id":9}"#, 128).unwrap();
        assert_eq!(event.kind, "task_started");
        assert_eq!(event.payload, json!({"type":"task_started","task_id":9}));
    }

    #[test]
    fn truncated_status_errors_fit_small_and_unicode_byte_limits() {
        for text in ["plain ASCII error", "éééé", "🙂🙂🙂"] {
            for limit in 0..=text.len() {
                let truncated = truncate_utf8(text, limit);
                assert!(
                    truncated.len() <= limit,
                    "{text:?}, limit {limit}, got {truncated:?}"
                );
                if text.len() <= limit {
                    assert_eq!(truncated, text);
                } else if limit >= "…".len() {
                    assert!(truncated.ends_with('…'));
                }
            }
        }
    }

    #[test]
    fn accepts_event_or_kind_aliases() {
        assert_eq!(
            parse_event_line(br#"{"event":"completed"}"#, 128)
                .unwrap()
                .kind,
            "completed"
        );
        assert_eq!(
            parse_event_line(br#"{"kind":"progress"}"#, 128)
                .unwrap()
                .kind,
            "progress"
        );
    }

    #[test]
    fn rejects_oversized_input_before_json_parsing() {
        assert_eq!(
            parse_event_line(br#"{"type":"x"}"#, 4),
            Err(CodexEventParseError::TooLarge { size: 12, limit: 4 })
        );
    }

    #[test]
    fn rejects_json_without_an_event_kind() {
        assert_eq!(
            parse_event_line(br#"{"message":"hello"}"#, 128),
            Err(CodexEventParseError::MissingKind)
        );
    }

    #[test]
    fn rejects_non_object_json() {
        assert_eq!(
            parse_event_line(br#"["task_started"]"#, 128),
            Err(CodexEventParseError::NotAnObject)
        );
    }

    #[test]
    fn typed_mode_replaces_control_flags_from_base_arguments() {
        let mut config = CodexWatchConfig::new("/tmp/project");
        config.args = vec![
            OsString::from("--json-events"),
            OsString::from("--workspace-write"),
            OsString::from("--verbose"),
        ];
        let spec = config.process_spec(CodexRunMode::DryRun, CodexRunKind::Once);
        assert_eq!(
            spec.args,
            vec![
                OsString::from("--verbose"),
                OsString::from("--json-events"),
                OsString::from("--dry-run"),
                OsString::from("--once"),
            ]
        );
    }
}
