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
use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const MAX_STATUS_ERROR_BYTES: usize = 4 * 1024;

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
            },
            captured_output: Arc::clone(&captured_output),
            closing: Arc::clone(&closing),
            runtime: None,
            enabled: false,
            run_mode: None,
            generation: 0,
        };
        let worker = thread::Builder::new()
            .name("editor-codex-watch".to_owned())
            .spawn(move || worker.run())?;

        Ok(Self {
            config,
            commands: command_tx,
            events,
            status,
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
}

struct CodexPublisher {
    sender: SyncSender<CodexWatchEvent>,
    dropped: Arc<AtomicUsize>,
    status: Arc<Mutex<CodexWatchStatus>>,
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
                    self.publisher.emit(CodexWatchEvent::Error(format!(
                        "failed reading codex-watch stderr: {error}"
                    )));
                    Ok(())
                }
                ProcessEvent::Eof(OutputStream::Stdout) => match runtime.decoder.finish() {
                    Some(line) if !line.is_empty() => self.consume_line(&line),
                    _ => Ok(()),
                },
                ProcessEvent::Eof(OutputStream::Stderr) => Ok(()),
            };
            if let Err(error) = result {
                fatal_error = Some(error);
                break;
            }
        }

        if let Some(error) = fatal_error {
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
                let previous_state = lock_unpoison(&self.publisher.status).state;
                let (state, error) = match runtime.kind {
                    CodexRunKind::Once
                        if exit.success && previous_state != CodexWatchState::Failed =>
                    {
                        (CodexWatchState::Completed, None)
                    }
                    CodexRunKind::Once if previous_state == CodexWatchState::Failed => (
                        CodexWatchState::Failed,
                        Some("codex-watch reported a failed task".to_owned()),
                    ),
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
        self.apply_event_state(&event);
        self.publisher.emit(CodexWatchEvent::Json(event));
        Ok(())
    }

    fn apply_event_state(&self, event: &CodexJsonEvent) {
        let normalized = event.kind.to_ascii_lowercase().replace('-', "_");
        let current = lock_unpoison(&self.publisher.status).clone();
        let state = if normalized.contains("watch") && normalized.contains("start") {
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
            self.publisher.transition(
                state,
                current.enabled,
                current.run_mode,
                current.active_kind,
                current.pid,
                current.generation,
                current.last_exit_code,
                if state == CodexWatchState::Failed {
                    event_message(event)
                } else {
                    None
                },
            );
        }
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
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

fn lock_unpoison<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::{
        CodexEventParseError, CodexRunKind, CodexRunMode, CodexWatchConfig, parse_event_line,
    };
    use serde_json::json;
    use std::ffi::OsString;

    #[test]
    fn parses_type_event_and_preserves_payload() {
        let event = parse_event_line(br#"{"type":"task_started","task_id":9}"#, 128).unwrap();
        assert_eq!(event.kind, "task_started");
        assert_eq!(event.payload, json!({"type":"task_started","task_id":9}));
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
