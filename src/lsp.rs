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
                "workspace": {"configuration": true, "workspaceFolders": true},
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
                if timeout_error.is_none() {
                    let _ = send_notification(&mut runtime, &self.config, "exit", Value::Null);
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
    use super::{
        JsonRpcFrameDecoder, JsonRpcFrameError, LspPosition, LspRange, LspTextChange,
        apply_text_changes, encode_json_rpc, path_to_file_uri,
    };
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
}
