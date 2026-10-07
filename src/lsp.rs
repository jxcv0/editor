//! Asynchronous `rust-analyzer` integration over LSP stdio.
//!
//! The handle exposed to the editor only performs bounded `try_send`/`try_recv` operations.  All
//! process launch, pipe I/O, JSON parsing, and shutdown work runs on a dedicated worker.

use crate::process::{
    BoundedLog, OutputStream, ProcessEvent, ProcessLimits, ProcessSpec, SupervisedChild,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const INITIALIZE_REQUEST_ID: u64 = 0;
const SHUTDOWN_REQUEST_ID: i64 = -1;
const MAX_STATUS_ERROR_BYTES: usize = 4 * 1024;

#[derive(Clone, Debug)]
pub struct RustAnalyzerConfig {
    pub executable: PathBuf,
    pub args: Vec<OsString>,
    pub workspace_root: PathBuf,
    pub process_limits: ProcessLimits,
    pub max_message_bytes: usize,
    pub max_header_bytes: usize,
    pub command_queue_capacity: usize,
    pub event_queue_capacity: usize,
    pub captured_stderr_bytes: usize,
    pub initialize_timeout: Duration,
}

impl RustAnalyzerConfig {
    pub fn new(workspace_root: impl Into<PathBuf>) -> Self {
        Self {
            executable: PathBuf::from("rust-analyzer"),
            args: Vec::new(),
            workspace_root: workspace_root.into(),
            process_limits: ProcessLimits::default(),
            max_message_bytes: 8 * 1024 * 1024,
            max_header_bytes: 16 * 1024,
            command_queue_capacity: 256,
            event_queue_capacity: 512,
            captured_stderr_bytes: 128 * 1024,
            initialize_timeout: Duration::from_secs(15),
        }
    }

    /// The exact direct command that will be launched.  This is safe to show in health/status UI;
    /// it intentionally contains no inherited environment values.
    pub fn process_spec(&self) -> ProcessSpec {
        ProcessSpec::new(self.executable.clone())
            .args(self.args.clone())
            .current_dir(self.workspace_root.clone())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RustAnalyzerState {
    Stopped,
    Starting,
    Initializing,
    Ready,
    Stopping,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RustAnalyzerStatus {
    pub state: RustAnalyzerState,
    pub pid: Option<u32>,
    pub generation: u64,
    pub last_error: Option<String>,
}

impl Default for RustAnalyzerStatus {
    fn default() -> Self {
        Self {
            state: RustAnalyzerState::Stopped,
            pid: None,
            generation: 0,
            last_error: None,
        }
    }
}

#[derive(Clone, Debug)]
pub enum LspEvent {
    Status(RustAnalyzerStatus),
    Notification {
        method: String,
        params: Value,
    },
    ServerRequest {
        id: Value,
        method: String,
        params: Value,
    },
    Response {
        id: Value,
        result: Option<Value>,
        error: Option<Value>,
    },
    RequestFailed {
        id: u64,
        reason: String,
    },
    Stderr(String),
    Error(String),
    EventsDropped(usize),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LspClientError {
    QueueFull,
    WorkerStopped,
    MessageTooLarge { size: usize, limit: usize },
    InvalidInput(String),
}

impl fmt::Display for LspClientError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::QueueFull => write!(formatter, "rust-analyzer command queue is full"),
            Self::WorkerStopped => write!(formatter, "rust-analyzer worker has stopped"),
            Self::MessageTooLarge { size, limit } => {
                write!(
                    formatter,
                    "LSP payload is {size} bytes; limit is {limit} bytes"
                )
            }
            Self::InvalidInput(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for LspClientError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LspPosition {
    pub line: u32,
    pub character: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LspRange {
    pub start: LspPosition,
    pub end: LspPosition,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LspTextChange {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub range: Option<LspRange>,
    pub text: String,
}

impl LspTextChange {
    pub fn incremental(range: LspRange, text: impl Into<String>) -> Self {
        Self {
            range: Some(range),
            text: text.into(),
        }
    }

    pub fn full(text: impl Into<String>) -> Self {
        Self {
            range: None,
            text: text.into(),
        }
    }
}

/// Nonblocking handle to the rust-analyzer worker.
pub struct RustAnalyzerClient {
    config: Arc<RustAnalyzerConfig>,
    commands: SyncSender<LspCommand>,
    events: Receiver<LspEvent>,
    status: Arc<Mutex<RustAnalyzerStatus>>,
    stderr: Arc<Mutex<BoundedLog>>,
    dropped_events: Arc<AtomicUsize>,
    next_request_id: AtomicU64,
    closing: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl RustAnalyzerClient {
    pub fn new(config: RustAnalyzerConfig) -> std::io::Result<Self> {
        let config = Arc::new(config);
        let (command_tx, command_rx) = mpsc::sync_channel(config.command_queue_capacity.max(1));
        let (event_tx, events) = mpsc::sync_channel(config.event_queue_capacity.max(1));
        let status = Arc::new(Mutex::new(RustAnalyzerStatus::default()));
        let stderr = Arc::new(Mutex::new(BoundedLog::new(config.captured_stderr_bytes)));
        let dropped_events = Arc::new(AtomicUsize::new(0));
        let closing = Arc::new(AtomicBool::new(false));

        let worker = LspWorker {
            config: Arc::clone(&config),
            commands: command_rx,
            publisher: EventPublisher {
                sender: event_tx,
                dropped: Arc::clone(&dropped_events),
                status: Arc::clone(&status),
            },
            stderr: Arc::clone(&stderr),
            closing: Arc::clone(&closing),
            runtime: None,
            documents: BTreeMap::new(),
            restart_after_stop: false,
        };
        let worker = thread::Builder::new()
            .name("editor-rust-analyzer".to_owned())
            .spawn(move || worker.run())?;

        Ok(Self {
            config,
            commands: command_tx,
            events,
            status,
            stderr,
            dropped_events,
            next_request_id: AtomicU64::new(1),
            closing,
            worker: Some(worker),
        })
    }

    pub fn config(&self) -> &RustAnalyzerConfig {
        &self.config
    }

    /// Queue asynchronous launch.  This never waits for process creation or initialization.
    pub fn start(&self) -> Result<(), LspClientError> {
        self.enqueue(LspCommand::Start)
    }

    /// Gracefully stop the server without stopping the reusable worker.
    pub fn shutdown(&self) -> Result<(), LspClientError> {
        self.enqueue(LspCommand::Shutdown)
    }

    pub fn restart(&self) -> Result<(), LspClientError> {
        self.enqueue(LspCommand::Restart)
    }

    pub fn open_document(
        &self,
        uri: impl Into<String>,
        language_id: impl Into<String>,
        version: i64,
        text: impl Into<String>,
    ) -> Result<(), LspClientError> {
        let text = text.into();
        self.check_size(text.len())?;
        self.enqueue(LspCommand::Open(Document {
            uri: uri.into(),
            language_id: language_id.into(),
            version,
            text,
            open_on_server: false,
        }))
    }

    /// Send versioned LSP changes.  Ranges use the protocol's UTF-16 coordinates.  The worker also
    /// applies them to its restart snapshot and rejects stale versions.
    pub fn change_document(
        &self,
        uri: impl Into<String>,
        version: i64,
        changes: Vec<LspTextChange>,
    ) -> Result<(), LspClientError> {
        let size = changes.iter().fold(0usize, |size, change| {
            size.saturating_add(change.text.len())
        });
        self.check_size(size)?;
        if changes.is_empty() {
            return Err(LspClientError::InvalidInput(
                "an LSP change must contain at least one content change".to_owned(),
            ));
        }
        self.enqueue(LspCommand::Change {
            uri: uri.into(),
            version,
            changes,
        })
    }

    pub fn close_document(&self, uri: impl Into<String>) -> Result<(), LspClientError> {
        self.enqueue(LspCommand::Close { uri: uri.into() })
    }

    /// Queue an arbitrary request used by completion, hover, navigation, actions, formatting, and
    /// the other typed editor commands.  The returned ID correlates the eventual response.
    pub fn request(&self, method: impl Into<String>, params: Value) -> Result<u64, LspClientError> {
        let method = method.into();
        if method.is_empty() {
            return Err(LspClientError::InvalidInput(
                "LSP request method cannot be empty".to_owned(),
            ));
        }
        self.check_json_size(&params)?;
        let id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        self.enqueue(LspCommand::Request { id, method, params })?;
        Ok(id)
    }

    pub fn notify(&self, method: impl Into<String>, params: Value) -> Result<(), LspClientError> {
        let method = method.into();
        if method.is_empty() {
            return Err(LspClientError::InvalidInput(
                "LSP notification method cannot be empty".to_owned(),
            ));
        }
        self.check_json_size(&params)?;
        self.enqueue(LspCommand::Notification { method, params })
    }

    pub fn respond(
        &self,
        id: Value,
        result: Option<Value>,
        error: Option<Value>,
    ) -> Result<(), LspClientError> {
        if result.is_some() == error.is_some() {
            return Err(LspClientError::InvalidInput(
                "an LSP response must contain exactly one of result or error".to_owned(),
            ));
        }
        if let Some(payload) = result.as_ref().or(error.as_ref()) {
            self.check_json_size(payload)?;
        }
        self.enqueue(LspCommand::Response { id, result, error })
    }

    pub fn status(&self) -> RustAnalyzerStatus {
        lock_unpoison(&self.status).clone()
    }

    pub fn captured_stderr(&self) -> (String, u64) {
        let log = lock_unpoison(&self.stderr);
        (log.to_string_lossy(), log.truncated_bytes())
    }

    pub fn try_recv(&self) -> Result<LspEvent, TryRecvError> {
        let dropped = self.dropped_events.swap(0, Ordering::AcqRel);
        if dropped != 0 {
            return Ok(LspEvent::EventsDropped(dropped));
        }
        self.events.try_recv()
    }

    pub fn drain_events(&self, limit: usize) -> Vec<LspEvent> {
        let mut events = Vec::with_capacity(limit.min(32));
        for _ in 0..limit {
            match self.try_recv() {
                Ok(event) => events.push(event),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
        events
    }

    fn check_size(&self, size: usize) -> Result<(), LspClientError> {
        if size > self.config.max_message_bytes {
            Err(LspClientError::MessageTooLarge {
                size,
                limit: self.config.max_message_bytes,
            })
        } else {
            Ok(())
        }
    }

    fn check_json_size(&self, value: &Value) -> Result<(), LspClientError> {
        let size = serde_json::to_vec(value)
            .map_err(|error| LspClientError::InvalidInput(error.to_string()))?
            .len();
        self.check_size(size)
    }

    fn enqueue(&self, command: LspCommand) -> Result<(), LspClientError> {
        match self.commands.try_send(command) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(LspClientError::QueueFull),
            Err(TrySendError::Disconnected(_)) => Err(LspClientError::WorkerStopped),
        }
    }
}

impl Drop for RustAnalyzerClient {
    fn drop(&mut self) {
        // The worker performs the protocol shutdown and applies the configured
        // bounded process-group termination timeout before it exits.
        self.closing.store(true, Ordering::Release);
        let _ = self.commands.try_send(LspCommand::Wake);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[derive(Clone, Debug)]
struct Document {
    uri: String,
    language_id: String,
    version: i64,
    text: String,
    open_on_server: bool,
}

enum LspCommand {
    Start,
    Shutdown,
    Restart,
    Open(Document),
    Change {
        uri: String,
        version: i64,
        changes: Vec<LspTextChange>,
    },
    Close {
        uri: String,
    },
    Request {
        id: u64,
        method: String,
        params: Value,
    },
    Notification {
        method: String,
        params: Value,
    },
    Response {
        id: Value,
        result: Option<Value>,
        error: Option<Value>,
    },
    Wake,
}

struct EventPublisher {
    sender: SyncSender<LspEvent>,
    dropped: Arc<AtomicUsize>,
    status: Arc<Mutex<RustAnalyzerStatus>>,
}

impl EventPublisher {
    fn emit(&self, event: LspEvent) {
        match self.sender.try_send(event) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
    }

    fn transition(
        &self,
        state: RustAnalyzerState,
        pid: Option<u32>,
        generation: u64,
        error: Option<String>,
    ) {
        let snapshot = {
            let mut status = lock_unpoison(&self.status);
            status.state = state;
            status.pid = pid;
            status.generation = generation;
            status.last_error = error.map(|error| truncate_utf8(&error, MAX_STATUS_ERROR_BYTES));
            status.clone()
        };
        self.emit(LspEvent::Status(snapshot));
    }
}

struct LspWorker {
    config: Arc<RustAnalyzerConfig>,
    commands: Receiver<LspCommand>,
    publisher: EventPublisher,
    stderr: Arc<Mutex<BoundedLog>>,
    closing: Arc<AtomicBool>,
    runtime: Option<LspRuntime>,
    documents: BTreeMap<String, Document>,
    restart_after_stop: bool,
}

struct LspRuntime {
    child: SupervisedChild,
    decoder: JsonRpcFrameDecoder,
    generation: u64,
    ready: bool,
    initialize_deadline: Instant,
    shutdown_deadline: Option<Instant>,
}

enum PumpOutcome {
    Keep,
    Stop { timeout_error: Option<String> },
    Fail(String),
}

enum MessageOutcome {
    Continue,
    ShutdownAcknowledged,
}

impl LspWorker {
    fn run(mut self) {
        loop {
            if self.closing.load(Ordering::Acquire) {
                self.restart_after_stop = false;
                if self.runtime.is_none() {
                    break;
                }
                if self
                    .runtime
                    .as_ref()
                    .is_some_and(|runtime| runtime.shutdown_deadline.is_none())
                {
                    self.begin_shutdown();
                }
            }

            self.pump();
            if self.closing.load(Ordering::Acquire) && self.runtime.is_none() {
                break;
            }

            let mut handled = 0;
            while handled < 64 {
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
                match self.commands.recv_timeout(Duration::from_millis(10)) {
                    Ok(command) => self.handle_command(command),
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        self.closing.store(true, Ordering::Release);
                    }
                }
            }
        }
    }

    fn handle_command(&mut self, command: LspCommand) {
        match command {
            LspCommand::Start => {
                if self.runtime.is_none() && !self.closing.load(Ordering::Acquire) {
                    self.launch();
                }
            }
            LspCommand::Restart => {
                if self.closing.load(Ordering::Acquire) {
                    return;
                }
                self.restart_after_stop = true;
                if self.runtime.is_some() {
                    self.begin_shutdown();
                } else {
                    self.launch();
                }
            }
            LspCommand::Shutdown => {
                self.restart_after_stop = false;
                self.begin_shutdown();
            }
            LspCommand::Open(mut document) => {
                if let Some(previous) = self.documents.get(&document.uri) {
                    if document.version < previous.version {
                        self.publisher.emit(LspEvent::Error(format!(
                            "ignored stale open for {} at version {} (current {})",
                            document.uri, document.version, previous.version
                        )));
                        return;
                    }
                    document.open_on_server = previous.open_on_server;
                }
                if self.runtime.as_ref().is_some_and(|runtime| runtime.ready) {
                    if document.open_on_server
                        && let Some(runtime) = self.runtime.as_mut()
                    {
                        let _ = send_notification(
                            runtime,
                            &self.config,
                            "textDocument/didClose",
                            json!({"textDocument": {"uri": document.uri}}),
                        );
                    }
                    document.open_on_server = false;
                    if let Some(runtime) = self.runtime.as_mut() {
                        match send_did_open(runtime, &self.config, &document) {
                            Ok(()) => document.open_on_server = true,
                            Err(error) => {
                                self.documents.insert(document.uri.clone(), document);
                                self.fail_current(error);
                                return;
                            }
                        }
                    }
                }
                self.documents.insert(document.uri.clone(), document);
            }
            LspCommand::Change {
                uri,
                version,
                changes,
            } => {
                let Some(document) = self.documents.get_mut(&uri) else {
                    self.publisher.emit(LspEvent::Error(format!(
                        "cannot change unopened LSP document {uri}"
                    )));
                    return;
                };
                if version <= document.version {
                    self.publisher.emit(LspEvent::Error(format!(
                        "ignored stale change for {uri} at version {version} (current {})",
                        document.version
                    )));
                    return;
                }
                let updated = match apply_text_changes(&document.text, &changes) {
                    Ok(updated) => updated,
                    Err(error) => {
                        self.publisher.emit(LspEvent::Error(format!(
                            "invalid incremental change for {uri}: {error}"
                        )));
                        return;
                    }
                };
                document.text = updated;
                document.version = version;
                if document.open_on_server
                    && self.runtime.as_ref().is_some_and(|runtime| runtime.ready)
                {
                    let params = json!({
                        "textDocument": {"uri": uri, "version": version},
                        "contentChanges": changes,
                    });
                    if let Some(runtime) = self.runtime.as_mut()
                        && let Err(error) = send_notification(
                            runtime,
                            &self.config,
                            "textDocument/didChange",
                            params,
                        )
                    {
                        self.fail_current(error);
                    }
                }
            }
            LspCommand::Close { uri } => {
                if let Some(document) = self.documents.remove(&uri)
                    && document.open_on_server
                    && let Some(runtime) = self.runtime.as_mut()
                    && let Err(error) = send_notification(
                        runtime,
                        &self.config,
                        "textDocument/didClose",
                        json!({"textDocument": {"uri": uri}}),
                    )
                {
                    self.fail_current(error);
                }
            }
            LspCommand::Request { id, method, params } => {
                let Some(runtime) = self.runtime.as_mut().filter(|runtime| runtime.ready) else {
                    self.publisher.emit(LspEvent::RequestFailed {
                        id,
                        reason: unavailable_reason(&self.publisher.status),
                    });
                    return;
                };
                if let Err(error) = send_request(runtime, &self.config, json!(id), &method, params)
                {
                    self.publisher.emit(LspEvent::RequestFailed {
                        id,
                        reason: error.clone(),
                    });
                    self.fail_current(error);
                }
            }
            LspCommand::Notification { method, params } => {
                let Some(runtime) = self.runtime.as_mut().filter(|runtime| runtime.ready) else {
                    self.publisher.emit(LspEvent::Error(format!(
                        "cannot send {method}: {}",
                        unavailable_reason(&self.publisher.status)
                    )));
                    return;
                };
                if let Err(error) = send_notification(runtime, &self.config, &method, params) {
                    self.fail_current(error);
                }
            }
            LspCommand::Response { id, result, error } => {
                let Some(runtime) = self.runtime.as_mut() else {
                    return;
                };
                let message = match (result, error) {
                    (Some(result), None) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
                    (None, Some(error)) => json!({"jsonrpc": "2.0", "id": id, "error": error}),
                    _ => return,
                };
                if let Err(error) = write_json(runtime, &self.config, &message) {
                    self.fail_current(error);
                }
            }
            LspCommand::Wake => {}
        }
    }

    fn launch(&mut self) {
        let generation = lock_unpoison(&self.publisher.status)
            .generation
            .saturating_add(1);
        self.publisher
            .transition(RustAnalyzerState::Starting, None, generation, None);
        for document in self.documents.values_mut() {
            document.open_on_server = false;
        }

        let child = SupervisedChild::spawn(
            self.config.process_spec(),
            self.config.process_limits.clone(),
        );
        let child = match child {
            Ok(child) => child,
            Err(error) => {
                let message = format!(
                    "failed to launch {}: {error}",
                    self.config.executable.display()
                );
                self.publisher.transition(
                    RustAnalyzerState::Failed,
                    None,
                    generation,
                    Some(message.clone()),
                );
                self.publisher.emit(LspEvent::Error(message));
                return;
            }
        };
        let pid = child.pid();
        let mut runtime = LspRuntime {
            child,
            decoder: JsonRpcFrameDecoder::new(
                self.config.max_header_bytes,
                self.config.max_message_bytes,
            ),
            generation,
            ready: false,
            initialize_deadline: Instant::now() + self.config.initialize_timeout,
            shutdown_deadline: None,
        };

        let root_uri = path_to_file_uri(&self.config.workspace_root);
        let initialize = json!({
            "processId": std::process::id(),
            "clientInfo": {"name": "editor", "version": env!("CARGO_PKG_VERSION")},
            "rootUri": root_uri,
            "workspaceFolders": [{
                "uri": root_uri,
                "name": self.config.workspace_root.file_name()
                    .and_then(|name| name.to_str()).unwrap_or("workspace")
            }],
            "capabilities": {
                "general": {"positionEncodings": ["utf-16"]},
                "workspace": {"configuration": true, "workspaceFolders": true, "inlayHint": {"refreshSupport": true}},
                "textDocument": {
                    "synchronization": {"dynamicRegistration": false, "didSave": true},
                    "completion": {"completionItem": {"snippetSupport": true}},
                    "hover": {"contentFormat": ["markdown", "plaintext"]},
                    "signatureHelp": {},
                    "definition": {"linkSupport": true},
                    "typeDefinition": {"linkSupport": true},
                    "implementation": {"linkSupport": true},
                    "references": {},
                    "rename": {"prepareSupport": true},
                    "codeAction": {},
                    "formatting": {},
                    "inlayHint": {}
                }
            },
            "trace": "off"
        });
        if let Err(error) = send_request(
            &mut runtime,
            &self.config,
            json!(INITIALIZE_REQUEST_ID),
            "initialize",
            initialize,
        ) {
            let _ = runtime.child.terminate(Duration::ZERO);
            self.publisher.transition(
                RustAnalyzerState::Failed,
                None,
                generation,
                Some(error.clone()),
            );
            self.publisher.emit(LspEvent::Error(error));
            return;
        }

        self.publisher
            .transition(RustAnalyzerState::Initializing, Some(pid), generation, None);
        self.runtime = Some(runtime);
    }

    fn begin_shutdown(&mut self) {
        let Some(runtime) = self.runtime.as_mut() else {
            let generation = lock_unpoison(&self.publisher.status).generation;
            self.publisher
                .transition(RustAnalyzerState::Stopped, None, generation, None);
            if self.restart_after_stop && !self.closing.load(Ordering::Acquire) {
                self.restart_after_stop = false;
                self.launch();
            }
            return;
        };
        if runtime.shutdown_deadline.is_some() {
            return;
        }

        if !runtime.ready {
            let mut runtime = self.runtime.take().expect("runtime was checked");
            let generation = runtime.generation;
            let _ = runtime
                .child
                .terminate(self.config.process_limits.shutdown_timeout);
            self.publisher
                .transition(RustAnalyzerState::Stopped, None, generation, None);
            self.after_stop();
            return;
        }

        let result = send_request(
            runtime,
            &self.config,
            json!(SHUTDOWN_REQUEST_ID),
            "shutdown",
            Value::Null,
        );
        if let Err(error) = result {
            self.fail_current(error);
            return;
        }
        runtime.shutdown_deadline =
            Some(Instant::now() + self.config.process_limits.shutdown_timeout);
        self.publisher.transition(
            RustAnalyzerState::Stopping,
            Some(runtime.child.pid()),
            runtime.generation,
            None,
        );
    }

    fn pump(&mut self) {
        let Some(mut runtime) = self.runtime.take() else {
            return;
        };
        match self.pump_runtime(&mut runtime) {
            PumpOutcome::Keep => self.runtime = Some(runtime),
            PumpOutcome::Stop { timeout_error } => {
                let generation = runtime.generation;
                if timeout_error.is_none()
                    && send_notification(&mut runtime, &self.config, "exit", Value::Null).is_ok()
                {
                    runtime.child.close_stdin();
                    // A flushed pipe only proves the exit notification was
                    // delivered to the kernel. Give the server time to read
                    // it and finish its cleanup before sending any signal.
                    let deadline = Instant::now() + self.config.process_limits.shutdown_timeout;
                    while matches!(runtime.child.try_wait(), Ok(None)) && Instant::now() < deadline
                    {
                        thread::sleep(Duration::from_millis(1));
                    }
                }
                let _ = runtime.child.terminate(Duration::from_millis(100));
                if let Some(error) = &timeout_error {
                    self.publisher.emit(LspEvent::Error(error.clone()));
                }
                self.publisher.transition(
                    RustAnalyzerState::Stopped,
                    None,
                    generation,
                    timeout_error,
                );
                self.after_stop();
            }
            PumpOutcome::Fail(error) => {
                let generation = runtime.generation;
                let _ = runtime.child.terminate(Duration::ZERO);
                self.publisher.transition(
                    RustAnalyzerState::Failed,
                    None,
                    generation,
                    Some(error.clone()),
                );
                self.publisher.emit(LspEvent::Error(error));
                self.restart_after_stop = false;
            }
        }
    }

    fn pump_runtime(&mut self, runtime: &mut LspRuntime) -> PumpOutcome {
        for event in runtime.child.drain_events(64) {
            match event {
                ProcessEvent::WriteError(error) => {
                    return PumpOutcome::Fail(format!(
                        "failed writing rust-analyzer stdin: {error}"
                    ));
                }
                ProcessEvent::Output {
                    stream: OutputStream::Stdout,
                    bytes,
                } => {
                    let messages = match runtime.decoder.push(&bytes) {
                        Ok(messages) => messages,
                        Err(error) => {
                            return PumpOutcome::Fail(format!("invalid LSP frame: {error}"));
                        }
                    };
                    for message in messages {
                        match self.handle_message(runtime, message) {
                            Ok(MessageOutcome::Continue) => {}
                            Ok(MessageOutcome::ShutdownAcknowledged) => {
                                return PumpOutcome::Stop {
                                    timeout_error: None,
                                };
                            }
                            Err(error) => return PumpOutcome::Fail(error),
                        }
                    }
                }
                ProcessEvent::Output {
                    stream: OutputStream::Stderr,
                    bytes,
                } => {
                    lock_unpoison(&self.stderr).push(&bytes);
                    self.publisher.emit(LspEvent::Stderr(
                        String::from_utf8_lossy(&bytes).into_owned(),
                    ));
                }
                ProcessEvent::OutputDropped {
                    stream: OutputStream::Stdout,
                    events,
                } => {
                    return PumpOutcome::Fail(format!(
                        "rust-analyzer stdout overflowed; dropped {events} protocol chunks"
                    ));
                }
                ProcessEvent::OutputDropped {
                    stream: OutputStream::Stderr,
                    events,
                } => {
                    self.publisher.emit(LspEvent::Error(format!(
                        "rust-analyzer stderr overflowed; dropped {events} chunks"
                    )));
                }
                ProcessEvent::ReadError {
                    stream: OutputStream::Stdout,
                    error,
                } => {
                    return PumpOutcome::Fail(format!(
                        "failed reading rust-analyzer stdout: {error}"
                    ));
                }
                ProcessEvent::ReadError {
                    stream: OutputStream::Stderr,
                    error,
                } => self.publisher.emit(LspEvent::Error(format!(
                    "failed reading rust-analyzer stderr: {error}"
                ))),
                ProcessEvent::Eof(OutputStream::Stdout) => {
                    if runtime.child.try_wait().ok().flatten().is_none() {
                        return PumpOutcome::Fail(
                            "rust-analyzer closed its protocol stream unexpectedly".to_owned(),
                        );
                    }
                }
                ProcessEvent::Eof(OutputStream::Stderr) => {}
            }
        }

        match runtime.child.try_wait() {
            Ok(Some(_exit)) if runtime.shutdown_deadline.is_some() => {
                return PumpOutcome::Stop {
                    timeout_error: None,
                };
            }
            Ok(Some(exit)) => {
                return PumpOutcome::Fail(match exit.code {
                    Some(code) => format!("rust-analyzer exited unexpectedly with code {code}"),
                    None => "rust-analyzer was terminated unexpectedly".to_owned(),
                });
            }
            Ok(None) => {}
            Err(error) => {
                return PumpOutcome::Fail(format!(
                    "failed to inspect rust-analyzer process: {error}"
                ));
            }
        }

        let now = Instant::now();
        if !runtime.ready && now >= runtime.initialize_deadline {
            return PumpOutcome::Fail(format!(
                "rust-analyzer did not initialize within {:?}",
                self.config.initialize_timeout
            ));
        }
        if runtime
            .shutdown_deadline
            .is_some_and(|deadline| now >= deadline)
        {
            return PumpOutcome::Stop {
                timeout_error: Some(
                    "rust-analyzer did not acknowledge shutdown in time".to_owned(),
                ),
            };
        }
        PumpOutcome::Keep
    }

    fn handle_message(
        &mut self,
        runtime: &mut LspRuntime,
        message: Value,
    ) -> Result<MessageOutcome, String> {
        let object = message
            .as_object()
            .ok_or_else(|| "LSP message is not a JSON object".to_owned())?;
        if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            return Err("LSP message has no valid jsonrpc version".to_owned());
        }

        if object.get("id") == Some(&json!(INITIALIZE_REQUEST_ID)) && !runtime.ready {
            if let Some(error) = object.get("error") {
                return Err(format!("rust-analyzer initialization failed: {error}"));
            }
            if !object.contains_key("result") {
                return Err("rust-analyzer initialize response has no result".to_owned());
            }
            send_notification(runtime, &self.config, "initialized", json!({}))?;
            runtime.ready = true;
            self.publisher.transition(
                RustAnalyzerState::Ready,
                Some(runtime.child.pid()),
                runtime.generation,
                None,
            );
            for document in self.documents.values_mut() {
                send_did_open(runtime, &self.config, document)?;
                document.open_on_server = true;
            }
            return Ok(MessageOutcome::Continue);
        }

        if object.get("id") == Some(&json!(SHUTDOWN_REQUEST_ID))
            && runtime.shutdown_deadline.is_some()
        {
            return Ok(MessageOutcome::ShutdownAcknowledged);
        }

        if let Some(method) = object.get("method").and_then(Value::as_str) {
            let params = object.get("params").cloned().unwrap_or(Value::Null);
            if let Some(id) = object.get("id") {
                self.publisher.emit(LspEvent::ServerRequest {
                    id: id.clone(),
                    method: method.to_owned(),
                    params,
                });
            } else {
                self.publisher.emit(LspEvent::Notification {
                    method: method.to_owned(),
                    params,
                });
            }
            return Ok(MessageOutcome::Continue);
        }

        if let Some(id) = object.get("id") {
            self.publisher.emit(LspEvent::Response {
                id: id.clone(),
                result: object.get("result").cloned(),
                error: object.get("error").cloned(),
            });
            return Ok(MessageOutcome::Continue);
        }

        Err("LSP message is neither a request, response, nor notification".to_owned())
    }

    fn fail_current(&mut self, error: String) {
        let Some(mut runtime) = self.runtime.take() else {
            return;
        };
        let generation = runtime.generation;
        let _ = runtime.child.terminate(Duration::ZERO);
        self.publisher.transition(
            RustAnalyzerState::Failed,
            None,
            generation,
            Some(error.clone()),
        );
        self.publisher.emit(LspEvent::Error(error));
        self.restart_after_stop = false;
    }

    fn after_stop(&mut self) {
        for document in self.documents.values_mut() {
            document.open_on_server = false;
        }
        if self.restart_after_stop && !self.closing.load(Ordering::Acquire) {
            self.restart_after_stop = false;
            self.launch();
        }
    }
}

fn send_did_open(
    runtime: &mut LspRuntime,
    config: &RustAnalyzerConfig,
    document: &Document,
) -> Result<(), String> {
    send_notification(
        runtime,
        config,
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": document.uri,
                "languageId": document.language_id,
                "version": document.version,
                "text": document.text,
            }
        }),
    )
}

fn send_request(
    runtime: &mut LspRuntime,
    config: &RustAnalyzerConfig,
    id: Value,
    method: &str,
    params: Value,
) -> Result<(), String> {
    write_json(
        runtime,
        config,
        &json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}),
    )
}

fn send_notification(
    runtime: &mut LspRuntime,
    config: &RustAnalyzerConfig,
    method: &str,
    params: Value,
) -> Result<(), String> {
    write_json(
        runtime,
        config,
        &json!({"jsonrpc": "2.0", "method": method, "params": params}),
    )
}

fn write_json(
    runtime: &mut LspRuntime,
    config: &RustAnalyzerConfig,
    value: &Value,
) -> Result<(), String> {
    let frame =
        encode_json_rpc(value, config.max_message_bytes).map_err(|error| error.to_string())?;
    runtime
        .child
        .write_all(&frame)
        .map_err(|error| format!("failed writing rust-analyzer stdin: {error}"))
}

fn unavailable_reason(status: &Mutex<RustAnalyzerStatus>) -> String {
    let status = lock_unpoison(status);
    match status.state {
        RustAnalyzerState::Stopped => "rust-analyzer is stopped".to_owned(),
        RustAnalyzerState::Starting => "rust-analyzer is starting".to_owned(),
        RustAnalyzerState::Initializing => "rust-analyzer is still initializing".to_owned(),
        RustAnalyzerState::Ready => "rust-analyzer is unavailable".to_owned(),
        RustAnalyzerState::Stopping => "rust-analyzer is stopping".to_owned(),
        RustAnalyzerState::Failed => status
            .last_error
            .clone()
            .unwrap_or_else(|| "rust-analyzer failed".to_owned()),
    }
}

fn apply_text_changes(text: &str, changes: &[LspTextChange]) -> Result<String, String> {
    let mut text = text.to_owned();
    for change in changes {
        match change.range {
            None => text = change.text.clone(),
            Some(range) => {
                let start = utf16_position_to_byte(&text, range.start)?;
                let end = utf16_position_to_byte(&text, range.end)?;
                if start > end {
                    return Err("change range starts after it ends".to_owned());
                }
                text.replace_range(start..end, &change.text);
            }
        }
    }
    Ok(text)
}

fn utf16_position_to_byte(text: &str, position: LspPosition) -> Result<usize, String> {
    let mut line_start = 0usize;
    for _ in 0..position.line {
        let Some(newline) = text[line_start..].find('\n') else {
            return Err(format!("line {} is outside the document", position.line));
        };
        line_start += newline + 1;
    }
    let line_end = text[line_start..]
        .find('\n')
        .map_or(text.len(), |newline| line_start + newline);
    let line = &text[line_start..line_end];
    let mut utf16 = 0u32;
    for (byte, character) in line.char_indices() {
        if utf16 == position.character {
            return Ok(line_start + byte);
        }
        utf16 = utf16.saturating_add(character.len_utf16() as u32);
        if utf16 > position.character {
            return Err("UTF-16 position splits a surrogate pair".to_owned());
        }
    }
    if utf16 == position.character {
        Ok(line_end)
    } else {
        Err(format!(
            "UTF-16 character {} is outside line {}",
            position.character, position.line
        ))
    }
}

pub fn path_to_file_uri(path: &Path) -> String {
    let text = path.to_string_lossy();
    let mut uri = String::from("file://");
    if !text.starts_with('/') {
        uri.push('/');
    }
    for byte in text.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(*byte, b'-' | b'.' | b'_' | b'~' | b'/') {
            uri.push(char::from(*byte));
        } else {
            use std::fmt::Write as _;
            let _ = write!(uri, "%{byte:02X}");
        }
    }
    uri
}

fn truncate_utf8(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let suffix = if max_bytes >= "…".len() { "…" } else { "" };
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

/// Incremental decoder for `Content-Length` framed JSON-RPC messages.
#[derive(Clone, Debug)]
pub struct JsonRpcFrameDecoder {
    buffer: Vec<u8>,
    expected_body: Option<usize>,
    max_header_bytes: usize,
    max_message_bytes: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JsonRpcFrameError {
    HeaderTooLarge { limit: usize },
    HeaderNotUtf8,
    InvalidHeader(String),
    MissingContentLength,
    DuplicateContentLength,
    InvalidContentLength,
    MessageTooLarge { size: usize, limit: usize },
    InvalidJson(String),
    TruncatedFrame,
}

impl fmt::Display for JsonRpcFrameError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HeaderTooLarge { limit } => {
                write!(formatter, "LSP header exceeds the {limit}-byte limit")
            }
            Self::HeaderNotUtf8 => write!(formatter, "LSP header is not UTF-8"),
            Self::InvalidHeader(header) => write!(formatter, "invalid LSP header: {header}"),
            Self::MissingContentLength => write!(formatter, "LSP frame has no Content-Length"),
            Self::DuplicateContentLength => {
                write!(formatter, "LSP frame has duplicate Content-Length headers")
            }
            Self::InvalidContentLength => write!(formatter, "invalid LSP Content-Length"),
            Self::MessageTooLarge { size, limit } => {
                write!(
                    formatter,
                    "LSP message is {size} bytes; limit is {limit} bytes"
                )
            }
            Self::InvalidJson(error) => write!(formatter, "invalid LSP JSON: {error}"),
            Self::TruncatedFrame => write!(formatter, "LSP stream ended in a partial frame"),
        }
    }
}

impl std::error::Error for JsonRpcFrameError {}

impl JsonRpcFrameDecoder {
    pub fn new(max_header_bytes: usize, max_message_bytes: usize) -> Self {
        Self {
            buffer: Vec::new(),
            expected_body: None,
            max_header_bytes: max_header_bytes.max(1),
            max_message_bytes: max_message_bytes.max(1),
        }
    }

    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<Value>, JsonRpcFrameError> {
        self.buffer.extend_from_slice(bytes);
        let mut messages = Vec::new();
        loop {
            if self.expected_body.is_none() {
                let Some(header_end) = find_subslice(&self.buffer, b"\r\n\r\n") else {
                    if self.buffer.len() > self.max_header_bytes {
                        self.buffer.clear();
                        return Err(JsonRpcFrameError::HeaderTooLarge {
                            limit: self.max_header_bytes,
                        });
                    }
                    break;
                };
                if header_end > self.max_header_bytes {
                    self.buffer.clear();
                    return Err(JsonRpcFrameError::HeaderTooLarge {
                        limit: self.max_header_bytes,
                    });
                }
                let content_length = parse_content_length(&self.buffer[..header_end])?;
                if content_length > self.max_message_bytes {
                    self.buffer.clear();
                    return Err(JsonRpcFrameError::MessageTooLarge {
                        size: content_length,
                        limit: self.max_message_bytes,
                    });
                }
                self.buffer.drain(..header_end + 4);
                self.expected_body = Some(content_length);
            }

            let expected = self.expected_body.expect("set above");
            if self.buffer.len() < expected {
                break;
            }
            let body: Vec<u8> = self.buffer.drain(..expected).collect();
            self.expected_body = None;
            let value = serde_json::from_slice(&body)
                .map_err(|error| JsonRpcFrameError::InvalidJson(error.to_string()))?;
            messages.push(value);
        }
        Ok(messages)
    }

    pub fn finish(&self) -> Result<(), JsonRpcFrameError> {
        if self.buffer.is_empty() && self.expected_body.is_none() {
            Ok(())
        } else {
            Err(JsonRpcFrameError::TruncatedFrame)
        }
    }

    pub fn buffered_bytes(&self) -> usize {
        self.buffer.len()
    }
}

pub fn encode_json_rpc(
    value: &Value,
    max_message_bytes: usize,
) -> Result<Vec<u8>, JsonRpcFrameError> {
    let body = serde_json::to_vec(value)
        .map_err(|error| JsonRpcFrameError::InvalidJson(error.to_string()))?;
    if body.len() > max_message_bytes {
        return Err(JsonRpcFrameError::MessageTooLarge {
            size: body.len(),
            limit: max_message_bytes,
        });
    }
    let mut frame = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    frame.extend_from_slice(&body);
    Ok(frame)
}

fn parse_content_length(header: &[u8]) -> Result<usize, JsonRpcFrameError> {
    let header = std::str::from_utf8(header).map_err(|_| JsonRpcFrameError::HeaderNotUtf8)?;
    let mut content_length = None;
    for line in header.split("\r\n") {
        let Some((name, value)) = line.split_once(':') else {
            return Err(JsonRpcFrameError::InvalidHeader(line.to_owned()));
        };
        if name.eq_ignore_ascii_case("Content-Length") {
            if content_length.is_some() {
                return Err(JsonRpcFrameError::DuplicateContentLength);
            }
            let value = value.trim();
            if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(JsonRpcFrameError::InvalidContentLength);
            }
            content_length = Some(
                value
                    .parse()
                    .map_err(|_| JsonRpcFrameError::InvalidContentLength)?,
            );
        }
    }
    content_length.ok_or(JsonRpcFrameError::MissingContentLength)
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::Path;

    #[test]
    fn decoder_accepts_fragmented_and_back_to_back_frames() {
        let first = json!({"jsonrpc":"2.0","id":1,"result":{"ok":true}});
        let second = json!({"jsonrpc":"2.0","method":"window/logMessage","params":{}});
        let mut bytes = encode_json_rpc(&first, 1024).unwrap();
        bytes.extend(encode_json_rpc(&second, 1024).unwrap());
        let split = 17;

        let mut decoder = JsonRpcFrameDecoder::new(256, 1024);
        assert!(decoder.push(&bytes[..split]).unwrap().is_empty());
        assert_eq!(decoder.push(&bytes[split..]).unwrap(), vec![first, second]);
        assert_eq!(decoder.finish(), Ok(()));
    }

    #[test]
    fn decoder_accepts_content_type_and_case_insensitive_length() {
        let body = br#"{"jsonrpc":"2.0","result":null,"id":7}"#;
        let mut frame = format!(
            "Content-Type: application/vscode-jsonrpc; charset=utf-8\r\ncontent-length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        frame.extend(body);
        let mut decoder = JsonRpcFrameDecoder::new(256, 1024);
        assert_eq!(
            decoder.push(&frame).unwrap(),
            vec![json!({
                "jsonrpc":"2.0", "result":null, "id":7
            })]
        );
    }

    #[test]
    fn decoder_rejects_oversized_body_before_buffering_it() {
        let mut decoder = JsonRpcFrameDecoder::new(128, 4);
        assert_eq!(
            decoder.push(b"Content-Length: 5\r\n\r\n"),
            Err(JsonRpcFrameError::MessageTooLarge { size: 5, limit: 4 })
        );
        assert_eq!(decoder.buffered_bytes(), 0);
    }

    #[test]
    fn decoder_rejects_duplicate_content_length() {
        let mut decoder = JsonRpcFrameDecoder::new(128, 128);
        assert_eq!(
            decoder.push(b"Content-Length: 2\r\nContent-Length: 2\r\n\r\n{}"),
            Err(JsonRpcFrameError::DuplicateContentLength)
        );
    }

    #[test]
    fn incremental_change_uses_utf16_positions() {
        let changes = [LspTextChange::incremental(
            LspRange {
                start: LspPosition {
                    line: 0,
                    character: 3,
                },
                end: LspPosition {
                    line: 0,
                    character: 4,
                },
            },
            "!",
        )];
        assert_eq!(apply_text_changes("a😀b\n", &changes).unwrap(), "a😀!\n");
    }

    #[test]
    fn file_uri_is_percent_encoded() {
        assert_eq!(
            path_to_file_uri(Path::new("/tmp/a b.rs")),
            "file:///tmp/a%20b.rs"
        );
    }
    #[cfg(unix)]
    fn peer_config(directory: &Path, mode: &str) -> RustAnalyzerConfig {
        let mut config = RustAnalyzerConfig::new(directory);
        config.executable = "/bin/sh".into();
        // libtest owns stdout. Keep its output on stderr while fd 3 carries
        // only framed LSP; all variable arguments remain separate argv values.
        // Timeout tests intentionally kill this fixture. Discard its profile
        // so an interrupted child cannot leave a partial LLVM coverage file.
        config.args = vec![
            "-c".into(),
            "LLVM_PROFILE_FILE=/dev/null exec \"$1\" --exact lsp::tests::lsp_test_peer --nocapture --skip \"$2\" 3>&1 1>&2"
                .into(),
            "editor-lsp-test".into(),
            std::env::current_exe().unwrap().into_os_string(),
            format!("lsp-peer:{mode}").into(),
        ];
        config.initialize_timeout = Duration::from_secs(2);
        config.process_limits.shutdown_timeout = Duration::from_millis(200);
        config
    }

    #[cfg(unix)]
    #[test]
    fn lsp_test_peer() {
        use std::io::{Read, Write};
        use std::os::fd::FromRawFd;
        let Some(mode) =
            std::env::args().find_map(|arg| arg.strip_prefix("lsp-peer:").map(str::to_owned))
        else {
            return;
        };
        // SAFETY: peer_config's shell wrapper creates fd 3 exclusively for
        // this peer before exec; this File is its sole Rust owner.
        let mut output = unsafe { std::fs::File::from_raw_fd(3) };
        let mut decoder = JsonRpcFrameDecoder::new(16 * 1024, 8 * 1024 * 1024);
        let mut input = std::io::stdin().lock();
        let mut bytes = [0; 113]; // Deliberately fragment the initialize request.
        let mut initialized = false;
        loop {
            let length = input.read(&mut bytes).unwrap();
            if length == 0 {
                return;
            }
            for message in decoder.push(&bytes[..length]).unwrap() {
                let method = message.get("method").and_then(Value::as_str).unwrap_or("");
                let id = message.get("id").cloned().unwrap_or(Value::Null);
                let params = message.get("params").cloned().unwrap_or(Value::Null);
                let response = match method {
                    "initialize" => match mode.as_str() {
                        "initialize-error" => Some(
                            json!({"jsonrpc":"2.0","id":id,"error":{"code":-32002,"message":"fixture rejected initialization"}}),
                        ),
                        "initialize-no-result" => Some(json!({"jsonrpc":"2.0","id":id})),
                        "initialize-timeout" => None,
                        "bad-frame" => {
                            output.write_all(b"Content-Length: 4\r\n\r\nnope").unwrap();
                            output.flush().unwrap();
                            None
                        }
                        "bad-version" => Some(json!({"jsonrpc":"1.0","id":id,"result":{}})),
                        "bad-object" => Some(json!([])),
                        "unclassifiable" => Some(json!({"jsonrpc":"2.0","unexpected":true})),
                        _ => {
                            let observed =
                                json!({"jsonrpc":"2.0","method":"test/initialize","params":params});
                            output
                                .write_all(&encode_json_rpc(&observed, 8 * 1024 * 1024).unwrap())
                                .unwrap();
                            Some(
                                json!({"jsonrpc":"2.0","id":id,"result":{"capabilities":{"textDocumentSync":2}}}),
                            )
                        }
                    },
                    "initialized" => {
                        initialized = true;
                        writeln!(std::io::stderr(), "fixture initialized").unwrap();
                        Some(json!({"jsonrpc":"2.0","method":"test/initialized","params":params}))
                    }
                    "textDocument/didOpen" | "textDocument/didChange" | "textDocument/didClose" => {
                        assert!(initialized, "document synchronization preceded initialized");
                        Some(
                            json!({"jsonrpc":"2.0","method":"test/observed","params":{"method":method,"params":params}}),
                        )
                    }
                    "test/echo" => Some(json!({"jsonrpc":"2.0","id":id,"result":params})),
                    "test/error" => Some(
                        json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"fixture method unavailable"}}),
                    ),
                    "test/note" => {
                        Some(json!({"jsonrpc":"2.0","method":"test/noted","params":params}))
                    }
                    "test/server-request" => {
                        let request = json!({"jsonrpc":"2.0","id":"server-7","method":"workspace/configuration","params":{"items":[{"section":"rust-analyzer"}]}});
                        output
                            .write_all(&encode_json_rpc(&request, 8 * 1024 * 1024).unwrap())
                            .unwrap();
                        Some(json!({"jsonrpc":"2.0","id":id,"result":null}))
                    }
                    "test/crash" => std::process::exit(23),
                    "shutdown" if mode == "shutdown-timeout" => None,
                    "shutdown" => Some(json!({"jsonrpc":"2.0","id":id,"result":null})),
                    "exit" => {
                        let mut exits = std::fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open("peer-exits")
                            .unwrap();
                        writeln!(exits, "{}", std::process::id()).unwrap();
                        std::process::exit(0);
                    }
                    "" if message.get("id").is_some() => Some(
                        json!({"jsonrpc":"2.0","method":"test/server-response","params":message}),
                    ),
                    _ => panic!("unexpected peer message: {message}"),
                };
                if let Some(response) = response {
                    let frame = encode_json_rpc(&response, 8 * 1024 * 1024).unwrap();
                    // Exercise a response split inside Content-Length.
                    output.write_all(&frame[..7.min(frame.len())]).unwrap();
                    output.flush().unwrap();
                    output.write_all(&frame[7.min(frame.len())..]).unwrap();
                    output.flush().unwrap();
                    if method == "shutdown" && mode == "delayed-exit" {
                        // Acknowledging shutdown does not mean the subsequent
                        // exit notification has already been consumed.
                        std::thread::sleep(Duration::from_millis(50));
                    }
                }
            }
        }
    }

    #[cfg(unix)]
    fn wait_event(
        client: &RustAnalyzerClient,
        description: &str,
        predicate: impl Fn(&LspEvent) -> bool,
    ) -> LspEvent {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut seen = Vec::new();
        loop {
            match client.try_recv() {
                Ok(event) if predicate(&event) => return event,
                Ok(event) => seen.push(event),
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    std::thread::sleep(Duration::from_millis(1))
                }
                Err(error) => {
                    panic!("worker stopped waiting for {description}: {error}; seen {seen:?}")
                }
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {description}; status {:?}; stderr {:?}; seen {seen:?}",
                client.status(),
                client.captured_stderr()
            );
        }
    }

    #[cfg(unix)]
    fn wait_state(client: &RustAnalyzerClient, state: RustAnalyzerState) -> RustAnalyzerStatus {
        match wait_event(
            client,
            &format!("{state:?}"),
            |event| matches!(event, LspEvent::Status(status) if status.state == state),
        ) {
            LspEvent::Status(status) => status,
            _ => unreachable!(),
        }
    }

    #[cfg(unix)]
    fn observed(client: &RustAnalyzerClient, method: &str) -> Value {
        match wait_event(
            client,
            method,
            |event| matches!(event, LspEvent::Notification { method: observed, params } if observed == "test/observed" && params["method"] == method),
        ) {
            LspEvent::Notification { params, .. } => params["params"].clone(),
            _ => unreachable!(),
        }
    }

    #[cfg(unix)]
    #[test]
    fn client_initializes_and_synchronizes_versioned_documents() {
        let directory = tempfile::tempdir().unwrap();
        let client = RustAnalyzerClient::new(peer_config(directory.path(), "normal")).unwrap();
        let uri = path_to_file_uri(&directory.path().join("source.rs"));
        assert_eq!(client.status().state, RustAnalyzerState::Stopped);
        assert_eq!(client.config().workspace_root, directory.path());
        client.open_document(&uri, "rust", 1, "initial").unwrap();
        client
            .change_document(&uri, 2, vec![LspTextChange::full("a😀b\nsecond")])
            .unwrap();
        client.start().unwrap();
        let initialization = wait_event(
            &client,
            "initialize payload",
            |event| matches!(event, LspEvent::Notification { method, .. } if method == "test/initialize"),
        );
        let LspEvent::Notification { params, .. } = initialization else {
            unreachable!()
        };
        assert_eq!(params["rootUri"], path_to_file_uri(directory.path()));
        assert_eq!(
            params["capabilities"]["general"]["positionEncodings"],
            json!(["utf-16"])
        );
        assert_eq!(params["clientInfo"]["name"], "editor");
        let ready = wait_state(&client, RustAnalyzerState::Ready);
        assert_eq!(ready.generation, 1);
        assert!(ready.pid.is_some());
        let opened = observed(&client, "textDocument/didOpen");
        assert_eq!(
            opened["textDocument"],
            json!({"uri":uri,"languageId":"rust","version":2,"text":"a😀b\nsecond"})
        );
        let change = LspTextChange::incremental(
            LspRange {
                start: LspPosition {
                    line: 0,
                    character: 3,
                },
                end: LspPosition {
                    line: 0,
                    character: 4,
                },
            },
            "!",
        );
        client
            .change_document(&uri, 3, vec![change.clone()])
            .unwrap();
        let changed = observed(&client, "textDocument/didChange");
        assert_eq!(changed["textDocument"]["version"], 3);
        assert_eq!(changed["contentChanges"], json!([change]));
        client
            .change_document(&uri, 3, vec![LspTextChange::full("stale")])
            .unwrap();
        wait_event(
            &client,
            "stale change rejection",
            |event| matches!(event, LspEvent::Error(error) if error.contains("ignored stale change")),
        );
        client
            .open_document(&uri, "rust", 2, "stale reopen")
            .unwrap();
        wait_event(
            &client,
            "stale open rejection",
            |event| matches!(event, LspEvent::Error(error) if error.contains("ignored stale open")),
        );
        client
            .open_document(&uri, "rust", 4, "replacement")
            .unwrap();
        assert_eq!(
            observed(&client, "textDocument/didClose")["textDocument"]["uri"],
            uri
        );
        assert_eq!(
            observed(&client, "textDocument/didOpen")["textDocument"]["text"],
            "replacement"
        );
        client.close_document(&uri).unwrap();
        assert_eq!(
            observed(&client, "textDocument/didClose")["textDocument"]["uri"],
            uri
        );
        client
            .change_document(&uri, 5, vec![LspTextChange::full("closed")])
            .unwrap();
        wait_event(
            &client,
            "closed document rejection",
            |event| matches!(event, LspEvent::Error(error) if error.contains("unopened LSP document")),
        );
        client.shutdown().unwrap();
        assert_eq!(
            wait_state(&client, RustAnalyzerState::Stopped).last_error,
            None
        );
        assert_eq!(
            std::fs::read_to_string(directory.path().join("peer-exits"))
                .unwrap()
                .lines()
                .count(),
            1
        );
    }

    #[cfg(unix)]
    #[test]
    fn restart_reopens_the_latest_valid_snapshot_and_forgets_closed_documents() {
        let directory = tempfile::tempdir().unwrap();
        let client = RustAnalyzerClient::new(peer_config(directory.path(), "normal")).unwrap();
        client.start().unwrap();
        let first = wait_state(&client, RustAnalyzerState::Ready);
        client
            .open_document("file:///keep.rs", "rust", 1, "a😀b\r\n")
            .unwrap();
        observed(&client, "textDocument/didOpen");
        client
            .open_document("file:///closed.rs", "rust", 1, "closed")
            .unwrap();
        observed(&client, "textDocument/didOpen");
        client.close_document("file:///closed.rs").unwrap();
        observed(&client, "textDocument/didClose");
        client
            .change_document(
                "file:///keep.rs",
                2,
                vec![LspTextChange::incremental(
                    LspRange {
                        start: LspPosition {
                            line: 0,
                            character: 3,
                        },
                        end: LspPosition {
                            line: 0,
                            character: 4,
                        },
                    },
                    "!",
                )],
            )
            .unwrap();
        observed(&client, "textDocument/didChange");
        client
            .change_document(
                "file:///keep.rs",
                3,
                vec![LspTextChange::incremental(
                    LspRange {
                        start: LspPosition {
                            line: 0,
                            character: 2,
                        },
                        end: LspPosition {
                            line: 0,
                            character: 3,
                        },
                    },
                    "bad",
                )],
            )
            .unwrap();
        wait_event(
            &client,
            "invalid surrogate boundary",
            |event| matches!(event, LspEvent::Error(error) if error.contains("invalid incremental change")),
        );
        client.restart().unwrap();
        let second = wait_state(&client, RustAnalyzerState::Ready);
        assert_eq!(second.generation, first.generation + 1);
        assert_ne!(second.pid, first.pid);
        let reopened = observed(&client, "textDocument/didOpen");
        assert_eq!(
            reopened["textDocument"],
            json!({"uri":"file:///keep.rs","languageId":"rust","version":2,"text":"a😀!\r\n"})
        );
        let barrier = client.request("test/echo", json!("barrier")).unwrap();
        let mut reopened_closed = false;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match client.try_recv() {
                Ok(LspEvent::Notification { method, params })
                    if method == "test/observed" && params["method"] == "textDocument/didOpen" =>
                {
                    reopened_closed |=
                        params["params"]["textDocument"]["uri"] == "file:///closed.rs"
                }
                Ok(LspEvent::Response { id, .. }) if id == barrier => break,
                Ok(_) => {}
                Err(_) => std::thread::sleep(Duration::from_millis(1)),
            }
            assert!(Instant::now() < deadline);
        }
        assert!(!reopened_closed);
        client.shutdown().unwrap();
        wait_state(&client, RustAnalyzerState::Stopped);
        assert_eq!(
            std::fs::read_to_string(directory.path().join("peer-exits"))
                .unwrap()
                .lines()
                .count(),
            2
        );
    }

    #[cfg(unix)]
    #[test]
    fn client_routes_responses_notifications_and_server_requests_bidirectionally() {
        let directory = tempfile::tempdir().unwrap();
        let client = RustAnalyzerClient::new(peer_config(directory.path(), "normal")).unwrap();
        client.start().unwrap();
        wait_state(&client, RustAnalyzerState::Ready);
        let original_pid = client.status().pid;
        client.start().unwrap();
        let id = client
            .request("test/echo", json!({"unicode":"😀","number":7}))
            .unwrap();
        let response = wait_event(
            &client,
            "echo response",
            |event| matches!(event, LspEvent::Response { id: response, .. } if *response == id),
        );
        assert!(
            matches!(response, LspEvent::Response { result: Some(result), error: None, .. } if result == json!({"unicode":"😀","number":7}))
        );
        assert_eq!(client.status().pid, original_pid);
        assert_eq!(client.status().generation, 1);
        let failure = client.request("test/error", Value::Null).unwrap();
        assert!(failure > id);
        let response = wait_event(
            &client,
            "error response",
            |event| matches!(event, LspEvent::Response { id, .. } if *id == failure),
        );
        assert!(
            matches!(response, LspEvent::Response { result: None, error: Some(error), .. } if error["code"] == -32601)
        );
        client.notify("test/note", json!({"saved":true})).unwrap();
        wait_event(
            &client,
            "notification",
            |event| matches!(event, LspEvent::Notification { method, params } if method == "test/noted" && params["saved"] == true),
        );
        client.request("test/server-request", Value::Null).unwrap();
        let server = wait_event(&client, "server request", |event| {
            matches!(event, LspEvent::ServerRequest { .. })
        });
        assert!(
            matches!(server, LspEvent::ServerRequest { id, method, params } if id == "server-7" && method == "workspace/configuration" && params["items"][0]["section"] == "rust-analyzer")
        );
        client
            .respond(json!("server-7"), Some(json!([{"checkOnSave":true}])), None)
            .unwrap();
        wait_event(
            &client,
            "successful server reply",
            |event| matches!(event, LspEvent::Notification { method, params } if method == "test/server-response" && params["result"][0]["checkOnSave"] == true),
        );
        client
            .respond(
                json!("server-error"),
                None,
                Some(json!({"code":-32602,"message":"unsupported"})),
            )
            .unwrap();
        wait_event(
            &client,
            "failed server reply",
            |event| matches!(event, LspEvent::Notification { method, params } if method == "test/server-response" && params["id"] == "server-error" && params["error"]["code"] == -32602),
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while !client.captured_stderr().0.contains("fixture initialized") {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(client.captured_stderr().1, 0);
    }

    #[cfg(unix)]
    #[test]
    fn initialization_and_protocol_failures_are_reported_without_panicking_the_worker() {
        for (mode, expected) in [
            ("initialize-error", "initialization failed"),
            ("initialize-no-result", "initialize response has no result"),
            ("bad-frame", "invalid LSP frame"),
            ("bad-version", "no valid jsonrpc version"),
            ("bad-object", "not a JSON object"),
            (
                "unclassifiable",
                "neither a request, response, nor notification",
            ),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let client = RustAnalyzerClient::new(peer_config(directory.path(), mode)).unwrap();
            client.start().unwrap();
            let failed = wait_state(&client, RustAnalyzerState::Failed);
            assert!(
                failed.last_error.as_ref().unwrap().contains(expected),
                "{mode}: {failed:?}"
            );
            assert_eq!(failed.pid, None);
            let request = client.request("test/echo", Value::Null).unwrap();
            wait_event(
                &client,
                "request after protocol failure",
                |event| matches!(event, LspEvent::RequestFailed { id, reason } if *id == request && reason.contains(expected)),
            );
            client.shutdown().unwrap();
            wait_state(&client, RustAnalyzerState::Stopped);
        }
    }

    #[cfg(unix)]
    #[test]
    fn initialization_and_shutdown_timeouts_leave_a_reusable_worker() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = peer_config(directory.path(), "initialize-timeout");
        config.initialize_timeout = Duration::from_millis(100);
        let client = RustAnalyzerClient::new(config).unwrap();
        client.start().unwrap();
        let failure = wait_state(&client, RustAnalyzerState::Failed);
        assert!(failure.last_error.unwrap().contains("did not initialize"));
        client.restart().unwrap();
        assert_eq!(wait_state(&client, RustAnalyzerState::Failed).generation, 2);

        let mut config = peer_config(directory.path(), "shutdown-timeout");
        config.process_limits.shutdown_timeout = Duration::from_millis(100);
        let client = RustAnalyzerClient::new(config).unwrap();
        client.start().unwrap();
        wait_state(&client, RustAnalyzerState::Ready);
        client.shutdown().unwrap();
        let stopped = wait_state(&client, RustAnalyzerState::Stopped);
        assert!(
            stopped
                .last_error
                .unwrap()
                .contains("did not acknowledge shutdown")
        );
        assert_eq!(stopped.pid, None);
        client.start().unwrap();
        assert_eq!(wait_state(&client, RustAnalyzerState::Ready).generation, 2);
    }

    #[cfg(unix)]
    #[test]
    fn unavailable_missing_and_crashed_servers_report_errors_and_can_restart() {
        let directory = tempfile::tempdir().unwrap();
        let mut missing = RustAnalyzerConfig::new(directory.path());
        missing.executable = directory.path().join("missing-rust-analyzer");
        let client = RustAnalyzerClient::new(missing).unwrap();
        let request = client.request("test/echo", Value::Null).unwrap();
        wait_event(
            &client,
            "request while stopped",
            |event| matches!(event, LspEvent::RequestFailed { id, reason } if *id == request && reason.contains("stopped")),
        );
        client.notify("test/note", Value::Null).unwrap();
        wait_event(
            &client,
            "notification while stopped",
            |event| matches!(event, LspEvent::Error(error) if error.contains("cannot send test/note")),
        );
        client.respond(json!(1), Some(Value::Null), None).unwrap();
        client.start().unwrap();
        let missing = wait_state(&client, RustAnalyzerState::Failed);
        assert!(missing.last_error.unwrap().contains("failed to launch"));
        client.shutdown().unwrap();
        wait_state(&client, RustAnalyzerState::Stopped);

        let client = RustAnalyzerClient::new(peer_config(directory.path(), "normal")).unwrap();
        client.start().unwrap();
        wait_state(&client, RustAnalyzerState::Ready);
        client.request("test/crash", Value::Null).unwrap();
        assert!(
            wait_state(&client, RustAnalyzerState::Failed)
                .last_error
                .is_some()
        );
        client.restart().unwrap();
        assert_eq!(wait_state(&client, RustAnalyzerState::Ready).generation, 2);
    }

    #[cfg(unix)]
    #[test]
    fn oversized_encoded_request_reports_its_id_and_fails_the_connection() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = peer_config(directory.path(), "normal");
        config.max_message_bytes = 2048;
        let client = RustAnalyzerClient::new(config).unwrap();
        client.start().unwrap();
        wait_state(&client, RustAnalyzerState::Ready);
        let id = client.request("x".repeat(4096), Value::Null).unwrap();
        wait_event(
            &client,
            "oversized request failure",
            |event| matches!(event, LspEvent::RequestFailed { id: failed, reason } if *failed == id && reason.contains("limit")),
        );
        assert_eq!(wait_state(&client, RustAnalyzerState::Failed).pid, None);
    }

    #[test]
    fn client_validates_input_and_exposes_queue_pressure_without_blocking() {
        let config = Arc::new(RustAnalyzerConfig {
            max_message_bytes: 8,
            ..RustAnalyzerConfig::new(".")
        });
        let (commands, receiver) = std::sync::mpsc::sync_channel(1);
        let (events_sender, events) = std::sync::mpsc::sync_channel(1);
        let client = RustAnalyzerClient {
            config,
            commands,
            events,
            status: Arc::new(Mutex::new(RustAnalyzerStatus::default())),
            stderr: Arc::new(Mutex::new(BoundedLog::new(8))),
            dropped_events: Arc::new(AtomicUsize::new(0)),
            next_request_id: AtomicU64::new(1),
            closing: Arc::new(AtomicBool::new(false)),
            worker: None,
        };
        assert!(matches!(
            client.request("", Value::Null),
            Err(LspClientError::InvalidInput(_))
        ));
        assert!(matches!(
            client.notify("", Value::Null),
            Err(LspClientError::InvalidInput(_))
        ));
        assert!(matches!(
            client.change_document("file:///a", 1, vec![]),
            Err(LspClientError::InvalidInput(_))
        ));
        assert!(matches!(
            client.respond(json!(1), None, None),
            Err(LspClientError::InvalidInput(_))
        ));
        assert!(matches!(
            client.respond(json!(1), Some(Value::Null), Some(Value::Null)),
            Err(LspClientError::InvalidInput(_))
        ));
        assert!(matches!(
            client.open_document("file:///a", "rust", 1, "123456789"),
            Err(LspClientError::MessageTooLarge { size: 9, limit: 8 })
        ));
        assert!(matches!(
            client.change_document(
                "file:///a",
                2,
                vec![LspTextChange::full("12345"), LspTextChange::full("6789")]
            ),
            Err(LspClientError::MessageTooLarge { size: 9, limit: 8 })
        ));
        assert!(matches!(
            client.request("method", json!("1234567")),
            Err(LspClientError::MessageTooLarge { .. })
        ));
        assert!(matches!(
            client.notify("method", json!("1234567")),
            Err(LspClientError::MessageTooLarge { .. })
        ));
        assert!(matches!(
            client.respond(json!(1), Some(json!("1234567")), None),
            Err(LspClientError::MessageTooLarge { .. })
        ));
        assert!(matches!(
            client.respond(json!(1), None, Some(json!("1234567"))),
            Err(LspClientError::MessageTooLarge { .. })
        ));
        client.start().unwrap();
        assert_eq!(client.restart(), Err(LspClientError::QueueFull));
        drop(receiver);
        assert_eq!(client.shutdown(), Err(LspClientError::WorkerStopped));

        let publisher = EventPublisher {
            sender: events_sender,
            dropped: Arc::clone(&client.dropped_events),
            status: Arc::clone(&client.status),
        };
        publisher.emit(LspEvent::Error("first".into()));
        publisher.emit(LspEvent::Error("dropped".into()));
        publisher.transition(RustAnalyzerState::Failed, None, 7, Some("😀".repeat(2000)));
        assert!(matches!(client.try_recv(), Ok(LspEvent::EventsDropped(2))));
        assert_eq!(client.status().generation, 7);
        let error = client.status().last_error.unwrap();
        assert!(error.len() <= MAX_STATUS_ERROR_BYTES);
        assert!(error.is_char_boundary(error.len()));
        assert!(
            matches!(client.drain_events(1).as_slice(), [LspEvent::Error(error)] if error == "first")
        );
        assert!(client.drain_events(0).is_empty());
        drop(publisher);
        assert!(client.try_recv().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn initialization_can_be_interrupted_and_restarted_before_ready() {
        let directory = tempfile::tempdir().unwrap();
        let client =
            RustAnalyzerClient::new(peer_config(directory.path(), "initialize-timeout")).unwrap();
        client.start().unwrap();
        assert_eq!(
            wait_state(&client, RustAnalyzerState::Initializing).generation,
            1
        );
        let request = client.request("test/echo", Value::Null).unwrap();
        wait_event(
            &client,
            "request before initialize response",
            |event| matches!(event, LspEvent::RequestFailed { id, reason } if *id == request && reason.contains("still initializing")),
        );
        client.restart().unwrap();
        assert_eq!(
            wait_state(&client, RustAnalyzerState::Initializing).generation,
            2
        );
        client.shutdown().unwrap();
        let stopped = wait_state(&client, RustAnalyzerState::Stopped);
        assert_eq!(stopped.pid, None);
        assert_eq!(stopped.last_error, None);
        assert_eq!(stopped.generation, 2);
    }

    #[cfg(unix)]
    #[test]
    fn graceful_shutdown_waits_for_the_peer_to_consume_exit_after_acknowledging_shutdown() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = peer_config(directory.path(), "delayed-exit");
        config.process_limits.shutdown_timeout = Duration::from_secs(1);
        let client = RustAnalyzerClient::new(config).unwrap();
        client.start().unwrap();
        wait_state(&client, RustAnalyzerState::Ready);
        client.shutdown().unwrap();
        let stopped = wait_state(&client, RustAnalyzerState::Stopped);
        assert_eq!(stopped.last_error, None);
        assert_eq!(stopped.pid, None);
        assert_eq!(
            std::fs::read_to_string(directory.path().join("peer-exits"))
                .unwrap()
                .lines()
                .count(),
            1
        );
    }

    #[test]
    fn status_error_truncation_respects_utf8_and_includes_its_suffix_in_the_limit() {
        for text in ["ascii message", "😀😀😀😀", "a😀b😀c", ""] {
            for limit in 0..20 {
                let truncated = truncate_utf8(text, limit);
                assert!(truncated.len() <= limit, "{text:?}, {limit}: {truncated:?}");
                if text.len() <= limit {
                    assert_eq!(truncated, text);
                }
            }
        }
        assert_eq!(truncate_utf8("abcdef", 4), "a…");
        assert_eq!(truncate_utf8("😀", 3), "…");
    }

    #[test]
    fn decoder_rejects_malformed_headers_and_reports_partial_streams() {
        for (header, expected) in [
            (
                b"not a header\r\n\r\n".as_slice(),
                JsonRpcFrameError::InvalidHeader("not a header".into()),
            ),
            (
                b"Content-Type: json\r\n\r\n".as_slice(),
                JsonRpcFrameError::MissingContentLength,
            ),
            (
                b"Content-Length: -1\r\n\r\n".as_slice(),
                JsonRpcFrameError::InvalidContentLength,
            ),
            (
                b"Content-Length: \r\n\r\n".as_slice(),
                JsonRpcFrameError::InvalidContentLength,
            ),
            (
                b"Content-Length: 999999999999999999999999999999999\r\n\r\n".as_slice(),
                JsonRpcFrameError::InvalidContentLength,
            ),
            (
                b"\xff: invalid\r\n\r\n".as_slice(),
                JsonRpcFrameError::HeaderNotUtf8,
            ),
        ] {
            let error = JsonRpcFrameDecoder::new(128, 128).push(header).unwrap_err();
            assert_eq!(error, expected);
            assert!(!error.to_string().is_empty());
        }
        for header in [
            b"unterminated".as_slice(),
            b"Content-Length: 2\r\n\r\n{}".as_slice(),
        ] {
            let mut decoder = JsonRpcFrameDecoder::new(4, 128);
            assert_eq!(
                decoder.push(header),
                Err(JsonRpcFrameError::HeaderTooLarge { limit: 4 })
            );
            assert_eq!(decoder.buffered_bytes(), 0);
        }
        for partial in [
            b"Content-Len".as_slice(),
            b"Content-Length: 4\r\n\r\nnu".as_slice(),
        ] {
            let mut decoder = JsonRpcFrameDecoder::new(128, 128);
            assert!(decoder.push(partial).unwrap().is_empty());
            assert!(decoder.buffered_bytes() > 0);
            assert_eq!(decoder.finish(), Err(JsonRpcFrameError::TruncatedFrame));
        }
        let mut decoder = JsonRpcFrameDecoder::new(128, 128);
        let error = decoder.push(b"Content-Length: 4\r\n\r\nnope").unwrap_err();
        assert!(matches!(error, JsonRpcFrameError::InvalidJson(_)));
        assert_eq!(
            encode_json_rpc(&json!("😀"), 5),
            Err(JsonRpcFrameError::MessageTooLarge { size: 6, limit: 5 })
        );
    }

    #[test]
    fn text_changes_validate_ranges_and_apply_sequential_utf16_coordinates() {
        let range = |start, end| LspRange { start, end };
        let pos = |line, character| LspPosition { line, character };
        let changes = [
            LspTextChange::full("a😀b\r\nsecond\n"),
            LspTextChange::incremental(range(pos(0, 1), pos(0, 3)), "XY"),
            LspTextChange::incremental(range(pos(1, 0), pos(2, 0)), "last"),
        ];
        assert_eq!(apply_text_changes("old", &changes).unwrap(), "aXYb\r\nlast");
        for invalid in [
            range(pos(0, 2), pos(0, 3)),
            range(pos(0, 4), pos(0, 1)),
            range(pos(2, 0), pos(2, 1)),
            range(pos(9, 0), pos(9, 0)),
        ] {
            assert!(
                apply_text_changes("a😀b\n", &[LspTextChange::incremental(invalid, "x")]).is_err()
            );
        }
        assert_eq!(utf16_position_to_byte("one\r\ntwo", pos(1, 3)).unwrap(), 8);
        assert_eq!(utf16_position_to_byte("one\n", pos(1, 0)).unwrap(), 4);
        assert_eq!(
            path_to_file_uri(Path::new("/tmp/😀?#.rs")),
            "file:///tmp/%F0%9F%98%80%3F%23.rs"
        );
    }
}
