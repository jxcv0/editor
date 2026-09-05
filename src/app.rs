//! Foreground application loop and coordination of bounded background services.

use std::{
    collections::HashMap,
    ffi::OsString,
    fs, io,
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant},
};

use serde_json::{Value, json};

use crate::{
    buffer::{Pos, Utf16Pos},
    codex_watch::{CodexRunMode, CodexWatch, CodexWatchConfig, CodexWatchEvent, CodexWatchState},
    command::CommandId,
    editor::{
        Diagnostic, DiagnosticSeverity, Editor, EditorRequest, Orientation, PickerItem, PickerKind,
    },
    lsp::{
        LspEvent, LspTextChange, RustAnalyzerClient, RustAnalyzerConfig, RustAnalyzerState,
        path_to_file_uri,
    },
    project::{
        self, BackgroundTask, ProjectEntry, SearchMode, StreamEvent, TextSearchMatch,
        TextSearchOptions,
    },
    state::{self, Journal, RecoveryRecord, STATE_VERSION, SessionPane, SessionState},
    terminal::{TerminalOutput, TerminalProcess, TerminalStatus},
    ui::{self, InputEvent, Renderer, TerminalSession},
};

pub struct Runtime {
    pub editor: Editor,
    renderer: Renderer,
    frame_builder: ui::FrameBuilder,
    redraw: bool,
    rust_analyzer: Option<RustAnalyzerClient>,
    codex_watch: Option<CodexWatch>,
    codex_activity_revision: u64,
    last_codex_animation: Instant,
    project_scan: Option<BackgroundTask<ProjectEntry>>,
    explorer_scan: Option<(PathBuf, BackgroundTask<ProjectEntry>)>,
    project_search: Option<BackgroundTask<TextSearchMatch>>,
    project_search_results: Vec<PickerItem>,
    last_search_query: String,
    lsp_versions: HashMap<PathBuf, u64>,
    pending_lsp: HashMap<u64, PendingLspRequest>,
    diagnostic_versions: HashMap<PathBuf, u64>,
    journal: Option<Journal>,
    journal_versions: HashMap<String, u64>,
    last_disk_check: Instant,
    last_maintenance: Instant,
    session_path: Option<PathBuf>,
    session_rx: Option<Receiver<Option<SessionState>>>,
    restore_session: bool,
    background_started: bool,
    pending_codex_mode: Option<(CodexRunMode, Instant)>,
    terminal: Option<TerminalProcess>,
}

#[derive(Debug, Clone)]
struct PendingLspRequest {
    command: CommandId,
    mutation_origin: Option<LspMutationOrigin>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LspMutationOrigin {
    path: PathBuf,
    revision: u64,
}

impl PendingLspRequest {
    fn command_only(command: CommandId) -> Self {
        Self {
            command,
            mutation_origin: None,
        }
    }

    fn for_document(command: CommandId, path: &Path, revision: u64) -> Self {
        let mutation_origin =
            matches!(command, CommandId::Format | CommandId::Rename).then(|| LspMutationOrigin {
                path: path.to_owned(),
                revision,
            });
        Self {
            command,
            mutation_origin,
        }
    }
}

impl Runtime {
    pub fn new(editor: Editor, restore_session: bool) -> io::Result<Self> {
        Self::with_state_root(editor, restore_session, state::state_dir())
    }

    fn with_state_root(
        editor: Editor,
        restore_session: bool,
        state_root: Option<PathBuf>,
    ) -> io::Result<Self> {
        let root = editor.explorer.root.clone();
        let mut ra_config = RustAnalyzerConfig::new(root.clone());
        ra_config.executable = PathBuf::from(&editor.config.tools.rust_analyzer.path);
        ra_config.args = editor
            .config
            .tools
            .rust_analyzer
            .args
            .iter()
            .map(OsString::from)
            .collect();
        ra_config.max_message_bytes = editor.config.limits.tool_message_bytes;
        let rust_analyzer = RustAnalyzerClient::new(ra_config).ok();

        let mut codex_config = CodexWatchConfig::new(root.clone());
        codex_config.executable = PathBuf::from(&editor.config.tools.codex_watch.path);
        codex_config.args = editor
            .config
            .tools
            .codex_watch
            .args
            .iter()
            .map(OsString::from)
            .collect();
        codex_config.max_event_line_bytes = editor.config.limits.tool_message_bytes;
        let codex_watch = CodexWatch::new(codex_config).ok();

        let journal = state_root
            .as_ref()
            .and_then(|path| Journal::start(path.join("recovery")).ok());
        let session_path = state_root.map(|path| {
            path.join("sessions")
                .join(format!("{}.json", state::project_key(&root)))
        });

        Ok(Self {
            editor,
            renderer: Renderer::new(),
            frame_builder: ui::FrameBuilder::new(),
            redraw: true,
            rust_analyzer,
            codex_watch,
            codex_activity_revision: 0,
            last_codex_animation: Instant::now(),
            project_scan: None,
            explorer_scan: None,
            project_search: None,
            project_search_results: Vec::new(),
            last_search_query: String::new(),
            lsp_versions: HashMap::new(),
            pending_lsp: HashMap::new(),
            diagnostic_versions: HashMap::new(),
            journal,
            journal_versions: HashMap::new(),
            last_disk_check: Instant::now(),
            last_maintenance: Instant::now(),
            session_path,
            session_rx: None,
            restore_session,
            background_started: false,
            pending_codex_mode: None,
            terminal: None,
        })
    }

    pub fn run(mut self) -> io::Result<()> {
        let terminal = TerminalSession::enter()?;
        self.render(&terminal)?;
        self.start_background();
        while !self.editor.should_quit {
            let input = ui::poll_input(Duration::from_millis(25))?;
            self.handle_input(input);
            if self.redraw {
                self.render(&terminal)?;
            }
        }
        self.save_session();
        Ok(())
    }

    fn render(&mut self, terminal: &TerminalSession) -> io::Result<()> {
        self.redraw = false;
        let (width, height) = terminal.size()?;
        let (canvas, cursor) = self
            .frame_builder
            .draw_editor(&mut self.editor, width, height);
        self.sync_terminal_size();
        self.renderer
            .draw(&canvas, cursor, ui::cursor_style(&self.editor))
    }

    fn start_background(&mut self) {
        if self.background_started {
            return;
        }
        self.background_started = true;
        self.redraw = true;
        self.restart_scan();
        if let Some(client) = &self.rust_analyzer {
            if let Err(error) = client.start() {
                self.editor.message(error.to_string());
            }
        } else {
            self.editor.rust_analyzer_status = "failed".into();
        }
        if self.restore_session
            && let Some(path) = self.session_path.clone()
        {
            let (sender, receiver) = mpsc::sync_channel(1);
            let _ = thread::Builder::new()
                .name("editor-session-load".into())
                .spawn(move || {
                    let _ = sender.send(state::load_session(&path).ok().flatten());
                });
            self.session_rx = Some(receiver);
        }
        if let Some(root) = state::state_dir().map(|path| path.join("recovery"))
            && let Ok(records) = state::list_recoverable(&root)
            && !records.is_empty()
        {
            self.editor.message(format!(
                "{} recoverable buffer{} found in {}",
                records.len(),
                if records.len() == 1 { "" } else { "s" },
                root.display()
            ));
        }
    }

    fn handle_input(&mut self, input: InputEvent) {
        let idle = matches!(input, InputEvent::Tick);
        self.redraw |= !idle;
        match input {
            InputEvent::Key(key) => self.editor.handle_key(key),
            InputEvent::Paste(text) => self.editor.handle_paste(&text),
            InputEvent::Resize | InputEvent::Focus | InputEvent::Tick => {}
        }
        self.pump(idle);
    }

    fn pump(&mut self, idle: bool) {
        self.drain_terminal();
        self.handle_request();
        self.discard_stale_diagnostics();
        self.update_project_search();
        self.drain_project_scan();
        self.drain_explorer_scan();
        self.drain_project_search();
        self.redraw |= self.editor.poll_file_finder();
        self.drain_lsp();
        self.drain_codex();
        self.animate_codex(Instant::now());
        self.restore_session_if_ready();
        if idle || self.last_maintenance.elapsed() >= Duration::from_millis(50) {
            self.last_maintenance = Instant::now();
            self.sync_lsp_documents();
            self.journal_buffers();
        }
        // Disk reload still performs synchronous I/O, so leave it idle-only.
        if idle && self.last_disk_check.elapsed() >= Duration::from_secs(2) {
            self.last_disk_check = Instant::now();
            self.reload_clean_external_changes();
        }
    }

    fn restart_scan(&mut self) {
        if let Some(task) = self.project_scan.take() {
            task.cancel();
        }
        self.explorer_scan = None;
        self.editor.explorer.reset();
        self.editor.set_project_files(Vec::new());
        let options = project::ScanOptions {
            include_hidden: self.editor.explorer.show_hidden,
            include_ignored: self.editor.explorer.show_ignored,
            max_results: Some(self.editor.config.limits.search_results.saturating_mul(50)),
            ..project::ScanOptions::default()
        };
        self.project_scan = Some(project::scan_project(&self.editor.explorer.root, options));
    }

    fn drain_explorer_scan(&mut self) {
        if let Some((directory, task)) = &self.explorer_scan {
            let events = task.drain(256);
            self.redraw |= !events.is_empty();
            let mut entries = Vec::new();
            let mut finished = false;
            for event in events {
                match event {
                    StreamEvent::Item(entry) => entries.push(entry),
                    StreamEvent::Error(error) => {
                        self.editor.message(format!("Explorer: {}", error.message))
                    }
                    StreamEvent::Finished(_) => finished = true,
                }
            }
            if !entries.is_empty() {
                self.editor.explorer.append_directory(directory, entries);
            }
            if finished {
                self.explorer_scan = None;
            }
        }
        // One directory worker at a time bounds both threads and foreground
        // draining, including when many expanded paths are restored at once.
        if self.explorer_scan.is_none()
            && let Some(directory) = self.editor.explorer.next_directory_to_load()
        {
            let options = project::ScanOptions {
                include_hidden: self.editor.explorer.show_hidden,
                include_ignored: self.editor.explorer.show_ignored,
                ..project::ScanOptions::default()
            };
            let task = project::scan_directory(&directory, options);
            self.editor
                .explorer
                .append_directory(&directory, Vec::new());
            self.explorer_scan = Some((directory, task));
        }
    }

    fn drain_project_scan(&mut self) {
        let events = self
            .project_scan
            .as_ref()
            .map(|task| task.drain(256))
            .unwrap_or_default();
        self.redraw |= !events.is_empty();
        let mut changed = false;
        let mut files = Vec::new();
        for event in events {
            match event {
                StreamEvent::Item(entry) if entry.is_file() => {
                    files.push(entry.path);
                    changed = true;
                }
                StreamEvent::Error(error) => self
                    .editor
                    .message(format!("Project scan: {}", error.message)),
                StreamEvent::Finished(summary) => {
                    self.editor
                        .message(format!("Indexed {} project files", summary.files_scanned));
                    changed = true;
                }
                _ => {}
            }
        }
        if changed {
            self.redraw = true;
            self.editor.append_project_files(files);
        }
    }

    fn update_project_search(&mut self) {
        let query = self
            .editor
            .picker
            .as_ref()
            .filter(|picker| picker.kind == PickerKind::Grep)
            .map(|picker| picker.query.clone())
            .unwrap_or_default();
        if query == self.last_search_query {
            return;
        }
        self.last_search_query = query.clone();
        if let Some(task) = self.project_search.take() {
            task.cancel();
        }
        self.project_search_results.clear();
        self.editor.set_project_search_results(Vec::new());
        if query.is_empty() {
            return;
        }
        let options = TextSearchOptions {
            scan: project::ScanOptions {
                include_hidden: self.editor.explorer.show_hidden,
                include_ignored: self.editor.explorer.show_ignored,
                ..project::ScanOptions::default()
            },
            mode: SearchMode::Regex,
            max_results: self.editor.config.limits.search_results,
            ..TextSearchOptions::default()
        };
        match project::search_project(&self.editor.explorer.root, query, options) {
            Ok(task) => self.project_search = Some(task),
            Err(error) => self
                .editor
                .message(format!("Invalid project search: {error}")),
        }
    }

    fn drain_project_search(&mut self) {
        let events = self
            .project_search
            .as_ref()
            .map(|task| task.drain(256))
            .unwrap_or_default();
        self.redraw |= !events.is_empty();
        let mut changed = false;
        for event in events {
            match event {
                StreamEvent::Item(found) => {
                    self.project_search_results.push(PickerItem {
                        label: format!("{}:{}", found.relative_path.display(), found.line),
                        detail: found.preview,
                        path: Some(found.path),
                        line: Some(found.line.saturating_sub(1)),
                        insert_text: None,
                    });
                    changed = true;
                }
                StreamEvent::Error(error) => self
                    .editor
                    .message(format!("Project search: {}", error.message)),
                StreamEvent::Finished(_) => changed = true,
            }
        }
        if changed {
            self.redraw = true;
            self.editor
                .set_project_search_results(self.project_search_results.clone());
        }
    }

    fn handle_request(&mut self) {
        match self.editor.take_request() {
            EditorRequest::None => {}
            EditorRequest::RefreshProject => self.restart_scan(),
            EditorRequest::DocumentSaved(path) => self.notify_lsp_document_saved(&path),
            EditorRequest::TerminalToggle(visible) => self.toggle_terminal(visible),
            EditorRequest::TerminalInput(bytes) => self.write_terminal(&bytes),
            EditorRequest::CheckHealth => self.show_health(),
            EditorRequest::RustAnalyzer(command) => self.request_lsp(command, None),
            EditorRequest::RustAnalyzerWithArgument(command, argument) => {
                self.request_lsp(command, Some(argument));
            }
            EditorRequest::CodexWatch(command) => self.command_codex(command),
        }
    }

    fn toggle_terminal(&mut self, visible: bool) {
        if !visible || self.terminal.is_some() {
            return;
        }
        self.editor.terminal.reset();
        let (rows, columns) = self.editor.terminal.screen().size();
        match TerminalProcess::spawn(&self.editor.explorer.root, rows, columns) {
            Ok(terminal) => {
                self.terminal = Some(terminal);
                self.editor.terminal.status = TerminalStatus::Running;
            }
            Err(error) => {
                self.editor.terminal.status = TerminalStatus::Failed(error.to_string());
                self.editor.focus = crate::editor::Focus::Editor;
                self.editor
                    .message(format!("Could not start terminal: {error}"));
            }
        }
    }

    fn write_terminal(&mut self, bytes: &[u8]) {
        let Some(terminal) = self.terminal.as_mut() else {
            self.editor
                .message("Terminal is not running; toggle it off and on to restart");
            self.editor.focus = crate::editor::Focus::Editor;
            return;
        };
        if let Err(error) = terminal.write(bytes) {
            self.fail_terminal(format!("terminal input failed: {error}"));
        }
    }

    fn sync_terminal_size(&mut self) {
        if !self.editor.terminal.visible {
            return;
        }
        let (rows, columns) = self.editor.terminal.screen().size();
        let error = self
            .terminal
            .as_mut()
            .and_then(|terminal| terminal.resize(rows, columns).err());
        if let Some(error) = error {
            self.fail_terminal(format!("terminal resize failed: {error}"));
        }
    }

    fn drain_terminal(&mut self) {
        let output = self
            .terminal
            .as_ref()
            .map(|terminal| terminal.drain_output(256))
            .unwrap_or_default();
        self.redraw |= !output.is_empty() && self.editor.terminal.visible;
        let mut read_error = None;
        for event in output {
            match event {
                TerminalOutput::Data(bytes) => self.editor.terminal.process(&bytes),
                TerminalOutput::Error(error) => read_error = Some(error),
                TerminalOutput::Eof => {}
            }
        }
        if let Some(error) = read_error {
            self.fail_terminal(format!("terminal output failed: {error}"));
            return;
        }

        let mut response_error = None;
        for response in self.editor.terminal.take_responses() {
            let Some(terminal) = self.terminal.as_mut() else {
                break;
            };
            if let Err(error) = terminal.write(&response) {
                response_error = Some(error);
                break;
            }
        }
        if let Some(error) = response_error {
            self.fail_terminal(format!("terminal response failed: {error}"));
            return;
        }

        let status = match self.terminal.as_mut().map(TerminalProcess::try_wait) {
            Some(Ok(status)) => status,
            Some(Err(error)) => {
                self.fail_terminal(format!("terminal status failed: {error}"));
                return;
            }
            None => None,
        };
        if let Some(status) = status {
            self.redraw = true;
            self.terminal.take();
            self.editor.terminal.status = TerminalStatus::Exited(status.clone());
            if self.editor.focus == crate::editor::Focus::Terminal {
                self.editor.focus = crate::editor::Focus::Editor;
            }
            self.editor.message(format!("Terminal {status}"));
        }
    }

    fn fail_terminal(&mut self, error: String) {
        self.redraw = true;
        self.terminal.take();
        self.editor.terminal.status = TerminalStatus::Failed(error.clone());
        if self.editor.focus == crate::editor::Focus::Terminal {
            self.editor.focus = crate::editor::Focus::Editor;
        }
        self.editor.message(error);
    }

    fn notify_lsp_document_saved(&mut self, path: &Path) {
        let eligible = self.editor.buffers.iter().any(|slot| {
            slot.buffer.path() == Some(path)
                && path.extension().and_then(|extension| extension.to_str()) == Some("rs")
                && !slot.large_file
        });
        if !eligible {
            return;
        }
        self.sync_lsp_documents();
        let Some(client) = &self.rust_analyzer else {
            return;
        };
        if let Err(error) = client.notify(
            "textDocument/didSave",
            json!({"textDocument": {"uri": path_to_file_uri(path)}}),
        ) {
            self.editor.message(error.to_string());
        }
    }

    fn sync_lsp_documents(&mut self) {
        let Some(client) = &self.rust_analyzer else {
            return;
        };
        let open_paths: Vec<_> = self
            .editor
            .buffers
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| {
                let path = slot.buffer.path()?.to_owned();
                (path.extension().and_then(|extension| extension.to_str()) == Some("rs")
                    && !slot.large_file)
                    .then(|| (path, slot.buffer.revision(), index))
            })
            .collect();
        for (path, version, index) in &open_paths {
            let uri = path_to_file_uri(path);
            match self.lsp_versions.get(path).copied() {
                None => {
                    let text = self.editor.buffers[*index].buffer.text();
                    if client
                        .open_document(uri, "rust", *version as i64, text)
                        .is_ok()
                    {
                        self.lsp_versions.insert(path.clone(), *version);
                    }
                }
                Some(previous) if previous != *version => {
                    let text = self.editor.buffers[*index].buffer.text();
                    if client
                        .change_document(uri, *version as i64, vec![LspTextChange::full(text)])
                        .is_ok()
                    {
                        self.lsp_versions.insert(path.clone(), *version);
                    }
                }
                _ => {}
            }
        }
        let current: Vec<_> = open_paths.into_iter().map(|(path, _, _)| path).collect();
        let closed: Vec<_> = self
            .lsp_versions
            .keys()
            .filter(|path| !current.contains(path))
            .cloned()
            .collect();
        for path in closed {
            let _ = client.close_document(path_to_file_uri(&path));
            self.lsp_versions.remove(&path);
        }
    }

    fn drain_lsp(&mut self) {
        let events = self
            .rust_analyzer
            .as_ref()
            .map(|client| client.drain_events(128))
            .unwrap_or_default();
        self.redraw |= !events.is_empty();
        for event in events {
            match event {
                LspEvent::Status(status) => {
                    self.editor.rust_analyzer_status = ra_state_label(status.state).into();
                    if let Some(error) = status.last_error {
                        self.editor.message(error);
                    }
                }
                LspEvent::Notification { method, params }
                    if method == "textDocument/publishDiagnostics" =>
                {
                    self.publish_diagnostics(params);
                }
                LspEvent::Notification { method, params }
                    if method == "window/logMessage" || method == "window/showMessage" =>
                {
                    if let Some(message) = params.get("message").and_then(Value::as_str) {
                        self.editor.message(format!("rust-analyzer: {message}"));
                    }
                }
                LspEvent::Notification { .. } => {}
                LspEvent::ServerRequest { id, method, params } => {
                    if let Some(client) = &self.rust_analyzer {
                        let result = if method == "workspace/configuration" {
                            let count = params
                                .get("items")
                                .and_then(Value::as_array)
                                .map_or(0, Vec::len);
                            Value::Array(vec![Value::Null; count])
                        } else {
                            Value::Null
                        };
                        let _ = client.respond(id, Some(result), None);
                    }
                }
                LspEvent::Response { id, result, error } => {
                    let request = id.as_u64().and_then(|id| self.pending_lsp.remove(&id));
                    if let Some(error) = error {
                        self.editor.message(format!(
                            "rust-analyzer request failed: {}",
                            compact_json(&error)
                        ));
                    } else if let (Some(request), Some(result)) = (request, result) {
                        if request.mutation_origin.as_ref().is_some_and(|origin| {
                            !lsp_mutation_origin_matches(&self.editor, origin)
                        }) {
                            self.editor.message(format!(
                                "Discarded stale {} response: the originating buffer changed or is no longer active",
                                request.command
                            ));
                            continue;
                        }
                        self.handle_lsp_response(request.command, result);
                    }
                }
                LspEvent::RequestFailed { id, reason } => {
                    self.pending_lsp.remove(&id);
                    self.editor.message(reason);
                }
                LspEvent::Stderr(chunk) => {
                    for line in chunk.lines().filter(|line| !line.is_empty()) {
                        if line.to_ascii_lowercase().contains("error") {
                            self.editor.message(format!("rust-analyzer: {line}"));
                        }
                    }
                }
                LspEvent::Error(error) => self.editor.message(error),
                LspEvent::EventsDropped(count) => self
                    .editor
                    .message(format!("rust-analyzer dropped {count} UI events")),
            }
        }
    }

    fn request_lsp(&mut self, command: CommandId, argument: Option<String>) {
        self.sync_lsp_documents();
        let Some(client) = &self.rust_analyzer else {
            self.editor
                .message("rust-analyzer integration failed to initialize");
            return;
        };
        if command == CommandId::RustAnalyzerRestart {
            self.pending_lsp.clear();
            match client.restart() {
                Ok(()) => {
                    self.editor.rust_analyzer_status = "starting".into();
                    self.editor.message("rust-analyzer restart queued");
                }
                Err(error) => self.editor.message(error.to_string()),
            }
            return;
        }
        if command == CommandId::WorkspaceSymbols {
            match client.request(
                "workspace/symbol",
                json!({"query": argument.unwrap_or_default()}),
            ) {
                Ok(id) => {
                    self.pending_lsp
                        .insert(id, PendingLspRequest::command_only(command));
                }
                Err(error) => self.editor.message(error.to_string()),
            }
            return;
        }
        let Some(path) = self.editor.active_buffer().path().map(Path::to_owned) else {
            self.editor.message("This action requires a saved file");
            return;
        };
        let revision = self.editor.active_buffer().revision();
        let position = self
            .editor
            .active_buffer()
            .pos_to_utf16(self.editor.active_pane().cursor)
            .ok();
        let Some(position) = position else { return };
        let uri = path_to_file_uri(&path);
        let text_document = json!({"uri": uri});
        let at = json!({"line": position.line, "character": position.code_unit});
        let (method, params) = match command {
            CommandId::Hover => (
                "textDocument/hover",
                json!({"textDocument": text_document, "position": at}),
            ),
            CommandId::Definition => (
                "textDocument/definition",
                json!({"textDocument": text_document, "position": at}),
            ),
            CommandId::Declaration => (
                "textDocument/declaration",
                json!({"textDocument": text_document, "position": at}),
            ),
            CommandId::TypeDefinition => (
                "textDocument/typeDefinition",
                json!({"textDocument": text_document, "position": at}),
            ),
            CommandId::Implementation => (
                "textDocument/implementation",
                json!({"textDocument": text_document, "position": at}),
            ),
            CommandId::References => (
                "textDocument/references",
                json!({"textDocument": text_document, "position": at, "context": {"includeDeclaration": true}}),
            ),
            CommandId::Completion => (
                "textDocument/completion",
                json!({"textDocument": text_document, "position": at, "context": {"triggerKind": 1}}),
            ),
            CommandId::SignatureHelp => (
                "textDocument/signatureHelp",
                json!({"textDocument": text_document, "position": at}),
            ),
            CommandId::Format => (
                "textDocument/formatting",
                json!({"textDocument": text_document, "options": {"tabSize": self.editor.config.editor.tab_width, "insertSpaces": self.editor.config.editor.insert_spaces}}),
            ),
            CommandId::Rename => (
                "textDocument/rename",
                json!({"textDocument": text_document, "position": at, "newName": argument.unwrap_or_default()}),
            ),
            CommandId::CodeAction => (
                "textDocument/codeAction",
                json!({"textDocument": text_document, "range": {"start": at, "end": at}, "context": {"diagnostics": []}}),
            ),
            CommandId::DocumentSymbols => (
                "textDocument/documentSymbol",
                json!({"textDocument": text_document}),
            ),
            CommandId::WorkspaceSymbols => unreachable!("handled before document position setup"),
            _ => {
                self.editor
                    .message(format!("No LSP request mapping for {command}"));
                return;
            }
        };
        match client.request(method, params) {
            Ok(id) => {
                self.pending_lsp.insert(
                    id,
                    PendingLspRequest::for_document(command, &path, revision),
                );
            }
            Err(error) => self.editor.message(error.to_string()),
        }
    }

    fn handle_lsp_response(&mut self, command: CommandId, result: Value) {
        match command {
            CommandId::Hover | CommandId::SignatureHelp => {
                self.editor
                    .message(format!("{}: {}", command, summarize_lsp_text(&result)));
            }
            CommandId::Definition
            | CommandId::Declaration
            | CommandId::TypeDefinition
            | CommandId::Implementation
            | CommandId::References => self.open_locations(result),
            CommandId::Completion => {
                let array = result
                    .get("items")
                    .and_then(Value::as_array)
                    .or_else(|| result.as_array())
                    .cloned()
                    .unwrap_or_default();
                let items = array
                    .into_iter()
                    .take(200)
                    .map(|item| {
                        let label = item
                            .get("label")
                            .and_then(Value::as_str)
                            .unwrap_or("completion")
                            .to_owned();
                        let insert = item
                            .get("insertText")
                            .and_then(Value::as_str)
                            .unwrap_or(&label)
                            .to_owned();
                        let detail = item
                            .get("detail")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned();
                        PickerItem {
                            label,
                            detail,
                            path: None,
                            line: None,
                            insert_text: Some(insert),
                        }
                    })
                    .collect();
                self.editor.show_completion(items);
            }
            CommandId::Format => self.apply_text_edits(&result),
            CommandId::Rename => self.apply_workspace_edit(&result),
            CommandId::CodeAction | CommandId::DocumentSymbols | CommandId::WorkspaceSymbols => {
                let values = result.as_array().cloned().unwrap_or_default();
                let items = values
                    .into_iter()
                    .take(500)
                    .map(|item| {
                        let label = item
                            .get("title")
                            .or_else(|| item.get("name"))
                            .and_then(Value::as_str)
                            .unwrap_or("item")
                            .to_owned();
                        let line = item
                            .pointer("/range/start/line")
                            .or_else(|| item.pointer("/location/range/start/line"))
                            .and_then(Value::as_u64)
                            .map(|line| line as usize);
                        let path = item
                            .pointer("/location/uri")
                            .and_then(Value::as_str)
                            .and_then(file_uri_to_path);
                        PickerItem {
                            label,
                            detail: command.to_string(),
                            path,
                            line,
                            insert_text: None,
                        }
                    })
                    .collect();
                self.editor.show_picker_items(PickerKind::Symbols, items);
            }
            _ => {}
        }
    }

    fn open_locations(&mut self, result: Value) {
        let values = if let Some(array) = result.as_array() {
            array.clone()
        } else if result.is_null() {
            Vec::new()
        } else {
            vec![result]
        };
        let mut locations = Vec::new();
        for value in values {
            let uri = value
                .get("uri")
                .or_else(|| value.get("targetUri"))
                .and_then(Value::as_str);
            let range = value
                .get("range")
                .or_else(|| value.get("targetSelectionRange"));
            let Some((path, range)) = uri.and_then(file_uri_to_path).zip(range) else {
                continue;
            };
            let line = range
                .pointer("/start/line")
                .and_then(Value::as_u64)
                .unwrap_or(0) as usize;
            let column = range
                .pointer("/start/character")
                .and_then(Value::as_u64)
                .unwrap_or(0) as usize;
            locations.push((path, Utf16Pos::new(line, column)));
        }
        if locations.len() == 1 {
            let (path, position) = locations.remove(0);
            if let Err(error) = self.editor.jump_to(path, position) {
                self.editor.message(error.to_string());
            }
        } else if locations.is_empty() {
            self.editor.message("No locations found");
        } else {
            let items = locations
                .into_iter()
                .map(|(path, position)| PickerItem {
                    label: format!(
                        "{}:{}:{}",
                        path.display(),
                        position.line + 1,
                        position.code_unit + 1
                    ),
                    detail: "location".into(),
                    path: Some(path),
                    line: Some(position.line),
                    insert_text: None,
                })
                .collect();
            self.editor.show_picker_items(PickerKind::Symbols, items);
        }
    }

    fn apply_text_edits(&mut self, value: &Value) {
        let Some(array) = value.as_array() else {
            self.editor.message("Formatter returned no edits");
            return;
        };
        let edits = parse_text_edits(array);
        if edits.len() != array.len() {
            self.editor
                .message("Could not apply formatting: malformed text edit");
            return;
        }
        match self.editor.apply_lsp_edits(edits) {
            Ok(()) => self.editor.message("Formatting applied"),
            Err(error) => self
                .editor
                .message(format!("Could not apply formatting: {error}")),
        }
    }

    fn apply_workspace_edit(&mut self, value: &Value) {
        let Some(path) = self.editor.active_buffer().path().map(Path::to_owned) else {
            return;
        };
        let uri = path_to_file_uri(&path);
        let changes = value
            .get("changes")
            .and_then(|changes| changes.get(&uri))
            .and_then(Value::as_array);
        let Some(changes) = changes else {
            self.editor
                .message("Rename returned no edits for the active buffer");
            return;
        };
        let edits = parse_text_edits(changes);
        if edits.len() != changes.len() {
            self.editor
                .message("Could not apply rename: malformed text edit");
            return;
        }
        match self.editor.apply_lsp_edits(edits) {
            Ok(()) => self.editor.message("Rename applied to active buffer"),
            Err(error) => self
                .editor
                .message(format!("Could not apply rename: {error}")),
        }
    }

    fn publish_diagnostics(&mut self, params: Value) {
        let Some(uri) = params.get("uri").and_then(Value::as_str) else {
            return;
        };
        let Some(path) = file_uri_to_path(uri) else {
            return;
        };
        let open_slot = self
            .editor
            .buffers
            .iter()
            .find(|slot| slot.buffer.path() == Some(path.as_path()));
        let open_version = open_slot.map(|slot| slot.buffer.revision());
        let suppress_disk_backed = open_slot.is_some_and(|slot| slot.buffer.is_dirty());
        let version = params
            .get("version")
            .and_then(Value::as_u64)
            .or(open_version)
            .unwrap_or(0);
        if !diagnostic_version_is_current(
            open_version,
            self.diagnostic_versions.get(&path).copied(),
            version,
        ) {
            return;
        }
        let diagnostics = params
            .get("diagnostics")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|item| !suppress_disk_backed || !diagnostic_is_disk_backed(item))
            .map(|item| {
                let line = item
                    .pointer("/range/start/line")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as usize;
                let utf16 = item
                    .pointer("/range/start/character")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as usize;
                let column = self
                    .editor
                    .buffers
                    .iter()
                    .find(|slot| slot.buffer.path() == Some(path.as_path()))
                    .and_then(|slot| slot.buffer.utf16_to_pos(Utf16Pos::new(line, utf16)).ok())
                    .map_or(utf16, |position| position.grapheme);
                let severity = match item.get("severity").and_then(Value::as_u64).unwrap_or(3) {
                    1 => DiagnosticSeverity::Error,
                    2 => DiagnosticSeverity::Warning,
                    4 => DiagnosticSeverity::Hint,
                    _ => DiagnosticSeverity::Information,
                };
                Diagnostic {
                    path: Some(path.clone()),
                    line,
                    column,
                    severity,
                    message: item
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("diagnostic")
                        .to_owned(),
                    version,
                }
            })
            .collect();
        self.editor.replace_diagnostics(&path, diagnostics);
        self.diagnostic_versions.insert(path, version);
    }

    fn discard_stale_diagnostics(&mut self) {
        let previous_count = self.editor.diagnostics.len();
        self.editor.diagnostics.retain(|diagnostic| {
            self.editor
                .buffers
                .iter()
                .find(|slot| slot.buffer.path() == diagnostic.path.as_deref())
                .is_none_or(|slot| diagnostic.version == slot.buffer.revision())
        });
        self.redraw |= previous_count != self.editor.diagnostics.len();
    }

    fn command_codex(&mut self, command: CommandId) {
        let Some(watch) = &self.codex_watch else {
            self.editor
                .message("codex-watch integration failed to initialize");
            return;
        };
        let status = watch.status();
        if matches!(
            command,
            CommandId::CodexDryRun | CommandId::CodexWorkspaceWrite
        ) {
            let mode = if command == CommandId::CodexDryRun {
                CodexRunMode::DryRun
            } else {
                CodexRunMode::WorkspaceWrite
            };
            let confirmed = self.pending_codex_mode.is_some_and(|(pending, at)| {
                pending == mode && at.elapsed() <= Duration::from_secs(5)
            });
            if !confirmed {
                self.pending_codex_mode = Some((mode, Instant::now()));
                self.editor.message(format!(
                    "Confirm codex-watch {mode}: press <Space>a{} again within 5 seconds",
                    if mode == CodexRunMode::DryRun {
                        "d"
                    } else {
                        "w"
                    }
                ));
                return;
            }
            self.pending_codex_mode = None;
        }
        let result = match command {
            CommandId::CodexDryRun => watch
                .enable()
                .and_then(|_| watch.set_run_mode(CodexRunMode::DryRun)),
            CommandId::CodexWorkspaceWrite => watch
                .enable()
                .and_then(|_| watch.set_run_mode(CodexRunMode::WorkspaceWrite)),
            CommandId::CodexToggle
                if status.pid.is_some() || status.state == CodexWatchState::Starting =>
            {
                watch.stop()
            }
            CommandId::CodexToggle if status.enabled && status.run_mode.is_some() => watch.start(),
            CommandId::CodexToggle => {
                self.editor.message(
                    "Select dry-run (<Space>ad) or workspace-write (<Space>aw) before starting",
                );
                return;
            }
            CommandId::CodexRestart => watch.restart(),
            CommandId::CodexRunOnce => watch.run_once(),
            CommandId::CodexStatus => {
                self.editor.message(format!(
                    "codex-watch: {:?}, mode {}",
                    status.state,
                    status
                        .run_mode
                        .map_or("not selected".into(), |mode| mode.to_string())
                ));
                return;
            }
            CommandId::CodexLogs => {
                let (output, truncated) = watch.captured_output();
                let mut items: Vec<_> = output
                    .lines()
                    .rev()
                    .take(500)
                    .map(|line| PickerItem {
                        label: line.to_owned(),
                        detail: "codex-watch".into(),
                        path: None,
                        line: None,
                        insert_text: None,
                    })
                    .collect();
                if truncated > 0 {
                    items.push(PickerItem {
                        label: format!("… {truncated} older bytes truncated"),
                        detail: String::new(),
                        path: None,
                        line: None,
                        insert_text: None,
                    });
                }
                self.editor.show_picker_items(PickerKind::Messages, items);
                return;
            }
            _ => return,
        };
        match result {
            Ok(()) => self
                .editor
                .message(format!("codex-watch command queued: {command}")),
            Err(error) => self.editor.message(error.to_string()),
        }
    }

    fn drain_codex(&mut self) {
        let events = self
            .codex_watch
            .as_ref()
            .map(|watch| watch.drain_events(128))
            .unwrap_or_default();
        self.redraw |= !events.is_empty();
        for event in events {
            match event {
                CodexWatchEvent::Status(status) => {
                    self.editor.codex_watch_status =
                        format!("{:?}", status.state).to_ascii_lowercase();
                    if let Some(mode) = status.run_mode {
                        self.editor.codex_watch_status.push_str(&format!(":{mode}"));
                    }
                    if let Some(error) = status.last_error {
                        self.editor.message(error);
                    }
                    if status.state == CodexWatchState::Completed {
                        self.reload_clean_external_changes();
                    }
                }
                CodexWatchEvent::Json(event) => {
                    let state = event.state_name();
                    // One task may finish while other files are still being processed,
                    // so the aggregate watcher status need not become Completed.
                    if matches!(state, "applied" | "previewed") {
                        self.reload_clean_external_changes();
                    }
                    if matches!(
                        state,
                        "queued"
                            | "preparing"
                            | "waiting"
                            | "retrying"
                            | "applied"
                            | "previewed"
                            | "processing"
                            | "started"
                            | "completed"
                            | "failed"
                    ) {
                        let detail_key = if matches!(state, "applied" | "previewed" | "completed") {
                            "summary"
                        } else {
                            "message"
                        };
                        let detail = event.payload.get(detail_key).and_then(Value::as_str);
                        if state == "failed"
                            && detail
                                .is_some_and(|detail| self.editor.current_message() == Some(detail))
                        {
                            continue;
                        }
                        self.editor
                            .message(match detail.filter(|text| !text.trim().is_empty()) {
                                Some(detail) => format!("codex-watch: {state}: {detail}"),
                                None => format!("codex-watch: {state}"),
                            });
                    }
                }
                CodexWatchEvent::Stderr(line) => {
                    self.editor.message(format!("codex-watch: {line}"))
                }
                CodexWatchEvent::Error(error) => self.editor.message(error),
                CodexWatchEvent::EventsDropped(count) => self
                    .editor
                    .message(format!("codex-watch dropped {count} UI events")),
            }
        }
        if let Some((revision, lines)) = self
            .codex_watch
            .as_ref()
            .and_then(|watch| watch.working_lines_since(self.codex_activity_revision))
        {
            if self.editor.codex_working_lines.is_empty() {
                self.editor.codex_spinner_frame = 0;
                self.last_codex_animation = Instant::now();
            }
            self.codex_activity_revision = revision;
            self.editor.codex_working_lines = lines;
            self.redraw = true;
        }
    }

    fn animate_codex(&mut self, now: Instant) {
        if !self.editor.codex_working_lines.is_empty()
            && now.duration_since(self.last_codex_animation) >= Duration::from_millis(120)
        {
            self.last_codex_animation = now;
            self.editor.codex_spinner_frame = self.editor.codex_spinner_frame.wrapping_add(1);
            self.redraw = true;
        }
    }

    fn journal_buffers(&mut self) {
        let Some(journal) = &self.journal else { return };
        let project_key = state::project_key(&self.editor.explorer.root);
        for (index, slot) in self.editor.buffers.iter().enumerate() {
            let path = slot.buffer.path().map(Path::to_owned);
            let key = path
                .as_deref()
                .map(state::project_key)
                .unwrap_or_else(|| format!("{project_key}-scratch-{index}"));
            if !slot.buffer.is_dirty() {
                if self.journal_versions.contains_key(&key) && journal.remove(key.clone()) {
                    self.journal_versions.remove(&key);
                }
                continue;
            }
            if self.journal_versions.get(&key) == Some(&slot.buffer.revision()) {
                continue;
            }
            let queued = journal.queue(RecoveryRecord {
                version: STATE_VERSION,
                key: key.clone(),
                path,
                buffer_version: slot.buffer.version(),
                text: slot.buffer.text(),
            });
            if queued {
                self.journal_versions.insert(key, slot.buffer.revision());
            }
        }
    }

    fn reload_clean_external_changes(&mut self) {
        let mut messages = Vec::new();
        for index in 0..self.editor.buffers.len() {
            if self.editor.buffers[index].buffer.path().is_none()
                || self.editor.buffers[index].buffer.is_dirty()
                || self.editor.buffers[index].buffer.in_transaction()
            {
                continue;
            }
            match self.editor.buffers[index].buffer.may_have_changed_on_disk() {
                Ok(false) => continue,
                Ok(true) => {}
                Err(error) => {
                    messages.push(format!("External change check failed: {error}"));
                    continue;
                }
            }
            let Some(path) = self.editor.buffers[index].buffer.path().map(Path::to_owned) else {
                continue;
            };
            match fs::metadata(&path) {
                Ok(metadata) if metadata.len() > self.editor.config.limits.max_file_bytes => {
                    messages.push(format!(
                        "Skipped reloading {}: file is {} bytes (limit {})",
                        self.editor.buffers[index].display_name,
                        metadata.len(),
                        self.editor.config.limits.max_file_bytes,
                    ));
                    continue;
                }
                Ok(_) => {}
                // Let reload report deletion and replacement races with its
                // stronger byte-level integrity checks.
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    messages.push(format!("External change check failed: {error}"));
                    continue;
                }
            }
            match self.editor.buffers[index].buffer.reload() {
                Ok(true) => {
                    clamp_panes_for_buffer(&mut self.editor, index);
                    messages.push(format!(
                        "Reloaded {}",
                        self.editor.buffers[index].display_name
                    ));
                }
                Ok(false) => {}
                Err(error) => messages.push(format!("External change check failed: {error}")),
            }
        }
        for message in messages {
            self.redraw = true;
            self.editor.message(message);
        }
    }

    fn restore_session_if_ready(&mut self) {
        let Some(receiver) = &self.session_rx else {
            return;
        };
        let session = match receiver.try_recv() {
            Ok(session) => session,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => {
                self.session_rx = None;
                return;
            }
        };
        self.session_rx = None;
        let Some(session) = session else { return };
        self.redraw = true;
        if restore_session_state(&mut self.editor, session) {
            self.editor.message("Session restored");
        } else {
            self.editor
                .message("Session restore skipped because editing has already started");
        }
    }

    fn save_session(&self) {
        let Some(path) = &self.session_path else {
            return;
        };
        let session = session_state_from_editor(&self.editor);
        let _ = state::save_session(path, &session);
    }

    fn show_health(&mut self) {
        let ra = self.rust_analyzer.as_ref().map(|client| client.status());
        let codex = self.codex_watch.as_ref().map(|watch| watch.status());
        let items = vec![
            PickerItem {
                label: "Terminal".into(),
                detail: "UTF-8, true color, bracketed paste, diff rendering".into(),
                path: None,
                line: None,
                insert_text: None,
            },
            PickerItem {
                label: "Project".into(),
                detail: self.editor.explorer.root.display().to_string(),
                path: None,
                line: None,
                insert_text: None,
            },
            PickerItem {
                label: "rust-analyzer".into(),
                detail: ra.map_or("worker unavailable".into(), |status| {
                    format!(
                        "{:?}{}",
                        status.state,
                        status
                            .last_error
                            .map_or(String::new(), |error| format!(": {error}"))
                    )
                }),
                path: None,
                line: None,
                insert_text: None,
            },
            PickerItem {
                label: "codex-watch".into(),
                detail: codex.map_or("worker unavailable".into(), |status| {
                    format!(
                        "{:?}, enabled={}, mode={:?}",
                        status.state, status.enabled, status.run_mode
                    )
                }),
                path: None,
                line: None,
                insert_text: None,
            },
            PickerItem {
                label: "State".into(),
                detail: state::state_dir()
                    .map_or("unavailable".into(), |path| path.display().to_string()),
                path: None,
                line: None,
                insert_text: None,
            },
        ];
        self.editor.show_picker_items(PickerKind::Messages, items);
    }
}

fn session_state_from_editor(editor: &Editor) -> SessionState {
    let named_buffers: Vec<_> = editor
        .buffers
        .iter()
        .enumerate()
        .filter_map(|(index, slot)| slot.buffer.path().map(|path| (index, path.to_owned())))
        .collect();
    let files: Vec<_> = named_buffers.iter().map(|(_, path)| path.clone()).collect();
    let active_buffer = editor.active_pane().buffer;
    let active = named_buffers
        .iter()
        .position(|(buffer, _)| *buffer == active_buffer)
        .unwrap_or(0);

    let save_pane = |pane: &crate::editor::Pane| {
        editor
            .buffers
            .get(pane.buffer)?
            .buffer
            .path()
            .map(|file| SessionPane {
                file: file.to_owned(),
                cursor_line: pane.cursor.line,
                cursor_grapheme: pane.cursor.grapheme,
                viewport_line: pane.viewport_line,
                viewport_column: pane.viewport_column,
            })
    };
    let mut panes = Vec::new();
    // Session v1 has no active-pane id. Saving the active pane first lets
    // restore select the same pane even when several panes show one file.
    if let Some(pane) = save_pane(editor.active_pane()) {
        panes.push(pane);
    }
    panes.extend(
        editor
            .panes
            .iter()
            .filter(|pane| pane.id != editor.active_pane)
            .filter_map(save_pane),
    );

    SessionState {
        version: STATE_VERSION,
        project_root: editor.explorer.root.clone(),
        files,
        active,
        panes,
        explorer_open: editor.explorer.open,
        explorer_width: editor.explorer.width,
        expanded_directories: editor.explorer.expanded.iter().cloned().collect(),
    }
}

fn restore_session_state(editor: &mut Editor, session: SessionState) -> bool {
    if !can_replace_initial_scratch(editor) {
        return false;
    }

    let SessionState {
        files,
        active,
        panes,
        explorer_open,
        explorer_width,
        expanded_directories,
        ..
    } = session;
    let active_path = files.get(active).or_else(|| files.first()).cloned();

    for path in &files {
        let _ = editor.open_path(path);
    }
    editor.discard_initial_scratch();

    let active_buffer = active_path
        .as_deref()
        .and_then(|path| buffer_index_for_path(editor, path))
        .or_else(|| {
            files
                .iter()
                .find_map(|path| buffer_index_for_path(editor, path))
        });
    let restored_panes: Vec<_> = panes
        .into_iter()
        .filter_map(|pane| buffer_index_for_path(editor, &pane.file).map(|buffer| (pane, buffer)))
        .collect();

    if let Some((first, remaining)) = restored_panes.split_first() {
        editor.only_pane();
        apply_session_pane(editor, &first.0, first.1);
        // Split topology and orientation were not part of session v1. Rebuild
        // a deterministic right-deep vertical layout until the schema grows.
        for (pane, buffer) in remaining {
            editor.split(Orientation::Vertical);
            apply_session_pane(editor, pane, *buffer);
        }
    }

    if let Some(buffer) = active_buffer {
        if let Some(pane_id) = editor
            .panes
            .iter()
            .find(|pane| pane.buffer == buffer)
            .map(|pane| pane.id)
        {
            editor.active_pane = pane_id;
        } else if let Some(pane_id) = editor.panes.first().map(|pane| pane.id) {
            editor.active_pane = pane_id;
            let pane = editor.active_pane_mut();
            pane.buffer = buffer;
            pane.cursor = Pos::ZERO;
            pane.anchor = None;
            pane.viewport_line = 0;
            pane.viewport_column = 0;
            pane.desired_column = 0;
        }
    }

    editor.explorer.open = explorer_open;
    editor.explorer.width = explorer_width;
    editor.explorer.expanded = expanded_directories.into_iter().collect();
    editor.explorer.rebuild_rows();
    true
}

fn can_replace_initial_scratch(editor: &Editor) -> bool {
    editor.buffers.len() == 1
        && editor.panes.len() == 1
        && editor.active_pane().buffer == 0
        && editor.buffers[0].buffer.path().is_none()
        && editor.buffers[0].buffer.is_empty()
        && !editor.buffers[0].buffer.is_dirty()
        && !editor.buffers[0].buffer.in_transaction()
}

fn buffer_index_for_path(editor: &Editor, path: &Path) -> Option<usize> {
    editor
        .buffers
        .iter()
        .position(|slot| slot.buffer.path() == Some(path))
}

fn apply_session_pane(editor: &mut Editor, saved: &SessionPane, buffer_index: usize) {
    let buffer = &editor.buffers[buffer_index].buffer;
    let cursor = buffer.clamp_pos(Pos::new(saved.cursor_line, saved.cursor_grapheme));
    let viewport_line = saved
        .viewport_line
        .min(buffer.line_count().saturating_sub(1));
    let pane = editor.active_pane_mut();
    pane.buffer = buffer_index;
    pane.cursor = cursor;
    pane.anchor = None;
    pane.viewport_line = viewport_line;
    pane.viewport_column = saved.viewport_column;
    pane.desired_column = cursor.grapheme;
}

fn clamp_panes_for_buffer(editor: &mut Editor, buffer_index: usize) {
    let buffer = &editor.buffers[buffer_index].buffer;
    let last_line = buffer.line_count().saturating_sub(1);
    for pane in editor
        .panes
        .iter_mut()
        .filter(|pane| pane.buffer == buffer_index)
    {
        pane.cursor = buffer.clamp_pos(pane.cursor);
        pane.anchor = pane.anchor.map(|anchor| buffer.clamp_pos(anchor));
        pane.viewport_line = pane.viewport_line.min(last_line);
        pane.desired_column = pane.cursor.grapheme;
    }
}

fn ra_state_label(state: RustAnalyzerState) -> &'static str {
    match state {
        RustAnalyzerState::Stopped => "stopped",
        RustAnalyzerState::Starting => "starting",
        RustAnalyzerState::Initializing => "initializing",
        RustAnalyzerState::Ready => "ready",
        RustAnalyzerState::Stopping => "stopping",
        RustAnalyzerState::Failed => "failed",
    }
}

fn lsp_mutation_origin_matches(editor: &Editor, origin: &LspMutationOrigin) -> bool {
    editor.active_buffer().path() == Some(origin.path.as_path())
        && editor.active_buffer().revision() == origin.revision
}

fn diagnostic_version_is_current(
    open_version: Option<u64>,
    last_accepted: Option<u64>,
    incoming: u64,
) -> bool {
    match open_version {
        Some(current) => incoming == current,
        None => last_accepted.is_none_or(|accepted| incoming >= accepted),
    }
}

fn diagnostic_is_disk_backed(item: &Value) -> bool {
    item.get("source")
        .and_then(Value::as_str)
        .is_some_and(|source| matches!(source, "rustc" | "clippy"))
}

fn parse_text_edits(values: &[Value]) -> Vec<(Utf16Pos, Utf16Pos, String)> {
    values
        .iter()
        .filter_map(|edit| {
            let start = edit.pointer("/range/start")?;
            let end = edit.pointer("/range/end")?;
            Some((
                Utf16Pos::new(
                    start.get("line")?.as_u64()? as usize,
                    start.get("character")?.as_u64()? as usize,
                ),
                Utf16Pos::new(
                    end.get("line")?.as_u64()? as usize,
                    end.get("character")?.as_u64()? as usize,
                ),
                edit.get("newText")?.as_str()?.to_owned(),
            ))
        })
        .collect()
}

fn summarize_lsp_text(value: &Value) -> String {
    fn collect(value: &Value, output: &mut Vec<String>) {
        match value {
            Value::String(text) => output.push(text.replace('\n', " ")),
            Value::Array(values) => {
                for value in values {
                    collect(value, output);
                }
            }
            Value::Object(map) => {
                for key in ["value", "label", "documentation", "contents", "signatures"] {
                    if let Some(value) = map.get(key) {
                        collect(value, output);
                    }
                }
            }
            _ => {}
        }
    }
    let mut parts = Vec::new();
    collect(value, &mut parts);
    let text = parts.join(" • ");
    text.chars().take(500).collect()
}

fn compact_json(value: &Value) -> String {
    value.to_string().chars().take(500).collect()
}

fn file_uri_to_path(uri: &str) -> Option<PathBuf> {
    let encoded = uri.strip_prefix("file://")?;
    let mut bytes = Vec::with_capacity(encoded.len());
    let raw = encoded.as_bytes();
    let mut index = 0;
    while index < raw.len() {
        if raw[index] == b'%' && index + 2 < raw.len() {
            let hex = std::str::from_utf8(&raw[index + 1..index + 3]).ok()?;
            bytes.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            bytes.push(raw[index]);
            index += 1;
        }
    }
    String::from_utf8(bytes).ok().map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use tempfile::tempdir;

    fn local_runtime(root: &Path) -> Runtime {
        let editor = Editor::new(Config::default(), root.to_owned());
        let mut runtime = Runtime::with_state_root(editor, false, None).unwrap();
        runtime.rust_analyzer = None;
        runtime.codex_watch = None;
        runtime
    }

    #[test]
    fn explorer_loads_only_open_directories_and_refreshes_hidden_and_ignored_filters() {
        fn load(runtime: &mut Runtime) {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                runtime.drain_explorer_scan();
                if runtime.explorer_scan.is_none()
                    && runtime.editor.explorer.next_directory_to_load().is_none()
                {
                    return;
                }
                assert!(Instant::now() < deadline, "explorer scan did not finish");
                thread::sleep(Duration::from_millis(1));
            }
        }
        let directory = tempdir().unwrap();
        for name in ["src", "empty", ".hidden", "ignored"] {
            fs::create_dir(directory.path().join(name)).unwrap();
        }
        fs::write(
            directory.path().join(".gitignore"),
            "ignored/\nsrc/skipped.rs\n",
        )
        .unwrap();
        fs::write(directory.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        fs::write(directory.path().join("src/skipped.rs"), "").unwrap();
        let mut runtime = local_runtime(directory.path());
        load(&mut runtime);
        assert!(runtime.editor.explorer.rows().is_empty());
        runtime.editor.explorer.open = true;
        runtime.editor.focus = crate::editor::Focus::Explorer;
        load(&mut runtime);
        assert_eq!(runtime.editor.explorer.rows().len(), 2);
        assert!(
            runtime
                .editor
                .explorer
                .rows()
                .iter()
                .all(ProjectEntry::is_directory)
        );
        runtime.editor.handle_key(crate::input::Key::char('j'));
        runtime.editor.handle_key(crate::input::Key::char('l'));
        load(&mut runtime);
        assert_eq!(runtime.editor.explorer.rows().len(), 3);
        assert_eq!(
            runtime.editor.explorer.rows()[2].relative_path,
            Path::new("src/main.rs")
        );
        runtime.editor.handle_key(crate::input::Key::char('e'));
        runtime.handle_request();
        load(&mut runtime);
        assert!(
            runtime
                .editor
                .explorer
                .rows()
                .iter()
                .any(|entry| entry.relative_path == Path::new(".hidden"))
        );
        assert!(
            runtime
                .editor
                .explorer
                .rows()
                .iter()
                .all(|entry| entry.relative_path != Path::new("ignored"))
        );
        runtime.editor.handle_key(crate::input::Key::char('i'));
        runtime.handle_request();
        load(&mut runtime);
        assert!(
            runtime
                .editor
                .explorer
                .rows()
                .iter()
                .any(|entry| entry.relative_path == Path::new("ignored"))
        );
        assert!(
            runtime
                .editor
                .explorer
                .rows()
                .iter()
                .any(|entry| entry.relative_path == Path::new("src/skipped.rs"))
        );
        assert_eq!(
            runtime
                .editor
                .explorer
                .selected_entry()
                .unwrap()
                .relative_path,
            Path::new("src")
        );
    }

    fn open_test_file(runtime: &mut Runtime, name: &str, contents: &str) -> PathBuf {
        let path = runtime.editor.explorer.root.join(name);
        fs::write(&path, contents).unwrap();
        runtime.editor.open_path(&path).unwrap();
        path.canonicalize().unwrap()
    }

    fn text_edit(line: usize, start: usize, end: usize, text: &str) -> Value {
        json!({"range": {"start": {"line": line, "character": start},
            "end": {"line": line, "character": end}}, "newText": text})
    }

    #[cfg(unix)]
    fn start_codex_event_peer(runtime: &mut Runtime, events: &[Value]) {
        // cat emits the installed watcher's real JSON schema, then waits on stdin.
        // This exercises process delivery without executing Codex or editing its config.
        let path = runtime.editor.explorer.root.join("watcher-events.jsonl");
        let mut data = Vec::new();
        for event in events {
            serde_json::to_writer(&mut data, event).unwrap();
            data.push(b'\n');
        }
        fs::write(&path, data).unwrap();
        let mut config = CodexWatchConfig::new(&runtime.editor.explorer.root);
        config.executable = PathBuf::from("/bin/cat");
        config.args = vec![path.into_os_string()];
        config.json_events_arg = "-".into();
        config.dry_run_args.clear();
        config.process_limits.shutdown_timeout = Duration::from_millis(50);
        let watch = CodexWatch::new(config).unwrap();
        watch.enable().unwrap();
        watch.set_run_mode(CodexRunMode::DryRun).unwrap();
        watch.start().unwrap();
        runtime.codex_watch = Some(watch);
    }

    #[cfg(unix)]
    fn wait_codex_message(runtime: &mut Runtime, expected: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while runtime.editor.current_message() != Some(expected) {
            runtime.drain_codex();
            assert!(
                Instant::now() < deadline,
                "missing Codex message {expected:?}"
            );
            thread::sleep(Duration::from_millis(1));
        }
    }

    #[cfg(unix)]
    #[test]
    fn codex_line_spinners_animate_without_input_and_stop_with_the_watcher() {
        let directory = tempdir().unwrap();
        let mut runtime = local_runtime(directory.path());
        let path = open_test_file(&mut runtime, "active.rs", "first\n// @codex change this\n");
        start_codex_event_peer(
            &mut runtime,
            &[
                json!({"type":"status","state":"preparing","path":"active.rs","line":2,"task":1}),
                json!({"type":"status","state":"waiting","path":"active.rs","line":2,"task":1}),
            ],
        );
        wait_codex_message(&mut runtime, "codex-watch: waiting");
        assert_eq!(
            runtime.editor.codex_working_lines[&path],
            std::collections::BTreeSet::from([1])
        );
        runtime.redraw = false;
        let now = runtime.last_codex_animation;
        runtime.animate_codex(now + Duration::from_millis(119));
        assert!(!runtime.redraw);
        runtime.animate_codex(now + Duration::from_millis(120));
        assert!(runtime.redraw);
        assert_eq!(runtime.editor.codex_spinner_frame, 1);
        runtime.codex_watch.as_ref().unwrap().stop().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !runtime.editor.codex_working_lines.is_empty() {
            runtime.drain_codex();
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(1));
        }
        runtime.redraw = false;
        runtime.animate_codex(now + Duration::from_secs(1));
        assert!(!runtime.redraw);
    }

    #[cfg(unix)]
    #[test]
    fn codex_task_completion_reloads_clean_files_while_another_task_is_processing() {
        for completed_state in ["applied", "previewed"] {
            let directory = tempdir().unwrap();
            let mut runtime = local_runtime(directory.path());
            let clean_path = open_test_file(&mut runtime, "clean.rs", "old\n");
            let clean = runtime.editor.active_pane().buffer;
            let dirty_path = open_test_file(&mut runtime, "dirty.rs", "original\n");
            let dirty = runtime.editor.active_pane().buffer;
            runtime
                .editor
                .active_buffer_mut()
                .insert(Pos::ZERO, "local ")
                .unwrap();
            fs::write(&clean_path, "new contents\n").unwrap();
            fs::write(&dirty_path, "external contents\n").unwrap();
            start_codex_event_peer(
                &mut runtime,
                &[
                    json!({"version":1,"type":"status","state":"preparing","path":clean_path,"line":1,"task":1}),
                    json!({"version":1,"type":"status","state":"waiting","path":dirty_path,"line":1,"task":1}),
                    json!({"version":1,"type":"status","state":"idle","path":"unrelated.rs"}),
                    json!({"version":1,"type":"status","state":completed_state,"path":clean_path,"line":1,"task":1,"summary":"Movement updated"}),
                ],
            );
            wait_codex_message(
                &mut runtime,
                &format!("codex-watch: {completed_state}: Movement updated"),
            );
            assert_eq!(
                runtime.codex_watch.as_ref().unwrap().status().state,
                CodexWatchState::Processing
            );
            assert_eq!(runtime.editor.codex_watch_status, "processing:dry-run");
            assert!(!runtime.editor.codex_working_lines.contains_key(&clean_path));
            assert!(runtime.editor.codex_working_lines.contains_key(&dirty_path));
            assert_eq!(
                runtime.editor.buffers[clean].buffer.text(),
                "new contents\n"
            );
            assert_eq!(
                runtime.editor.buffers[dirty].buffer.text(),
                "local original\n"
            );
            assert!(runtime.editor.buffers[dirty].buffer.is_dirty());
        }
    }

    #[cfg(unix)]
    #[test]
    fn codex_task_failure_is_visible_while_another_task_is_processing() {
        let directory = tempdir().unwrap();
        let mut runtime = local_runtime(directory.path());
        start_codex_event_peer(
            &mut runtime,
            &[
                json!({"version":1,"type":"status","state":"waiting","path":"one.rs","line":1,"task":1}),
                json!({"version":1,"type":"status","state":"waiting","path":"two.rs","line":1,"task":1}),
                json!({"version":1,"type":"status","state":"failed","path":"one.rs","message":"Codex executable was unavailable"}),
            ],
        );
        wait_codex_message(&mut runtime, "Codex executable was unavailable");
        assert_eq!(runtime.editor.codex_watch_status, "processing:dry-run");
        assert_eq!(
            runtime
                .editor
                .messages
                .iter()
                .filter(|message| message.contains("Codex executable was unavailable"))
                .count(),
            1
        );
    }

    #[cfg(unix)]
    #[test]
    fn codex_toggle_stops_a_live_watcher_after_completion_or_failure() {
        for (state, message) in [
            ("applied", "codex-watch: applied: Finished"),
            ("failed", "Task failed"),
        ] {
            let directory = tempdir().unwrap();
            let mut runtime = local_runtime(directory.path());
            start_codex_event_peer(
                &mut runtime,
                &[
                    json!({"version":1,"type":"status","state":state,"path":"one.rs","summary":"Finished","message":"Task failed"}),
                ],
            );
            wait_codex_message(&mut runtime, message);
            assert!(runtime.codex_watch.as_ref().unwrap().status().pid.is_some());
            runtime.command_codex(CommandId::CodexToggle);
            let deadline = Instant::now() + Duration::from_secs(5);
            while runtime.codex_watch.as_ref().unwrap().status().pid.is_some() {
                runtime.drain_codex();
                assert!(
                    Instant::now() < deadline,
                    "toggle did not stop watcher after {state}"
                );
                thread::sleep(Duration::from_millis(1));
            }
            assert_eq!(
                runtime.codex_watch.as_ref().unwrap().status().state,
                CodexWatchState::Stopped
            );
            assert!(
                !runtime
                    .editor
                    .messages
                    .iter()
                    .any(|message| message.contains("already running"))
            );
        }
    }

    #[test]
    fn formatting_applies_utf16_edits_as_one_undo_step() {
        let directory = tempdir().unwrap();
        let mut runtime = local_runtime(directory.path());
        let original = "😀 ab cd\n";
        open_test_file(&mut runtime, "format.rs", original);
        runtime.editor.split(Orientation::Vertical);
        runtime.handle_lsp_response(
            CommandId::Format,
            json!([text_edit(0, 3, 5, "alpha"), text_edit(0, 6, 8, "beta")]),
        );
        assert_eq!(runtime.editor.active_buffer().text(), "😀 alpha beta\n");
        assert_eq!(runtime.editor.current_message(), Some("Formatting applied"));
        assert!(!runtime.editor.active_buffer().in_transaction());
        assert!(runtime.editor.active_buffer_mut().undo().unwrap());
        assert_eq!(runtime.editor.active_buffer().text(), original);
        assert!(!runtime.editor.active_buffer().can_undo());
    }

    #[test]
    fn formatting_rejects_invalid_positions_and_malformed_batches_atomically() {
        let directory = tempdir().unwrap();
        let mut runtime = local_runtime(directory.path());
        open_test_file(&mut runtime, "format.rs", "😀 value\n");
        for invalid in [
            text_edit(0, 1, 2, "invalid surrogate"),
            json!({"newText": "missing range"}),
        ] {
            runtime.apply_text_edits(&json!([text_edit(0, 3, 8, "changed"), invalid]));
            assert_eq!(runtime.editor.active_buffer().text(), "😀 value\n");
            assert!(!runtime.editor.active_buffer().can_undo());
            assert!(!runtime.editor.active_buffer().in_transaction());
            assert!(
                runtime
                    .editor
                    .current_message()
                    .unwrap()
                    .starts_with("Could not apply formatting:")
            );
        }
        runtime.apply_text_edits(&Value::Null);
        assert_eq!(
            runtime.editor.current_message(),
            Some("Formatter returned no edits")
        );
    }

    #[test]
    fn rename_applies_active_document_edits_and_rejects_malformed_batches() {
        let directory = tempdir().unwrap();
        let mut runtime = local_runtime(directory.path());
        let first = open_test_file(&mut runtime, "first.rs", "old old\n");
        let first_index = runtime.editor.active_pane().buffer;
        let second = open_test_file(&mut runtime, "second.rs", "old\n");
        let second_index = runtime.editor.active_pane().buffer;
        runtime.editor.active_pane_mut().buffer = first_index;
        runtime.handle_lsp_response(
            CommandId::Rename,
            json!({"changes": {
                path_to_file_uri(&first): [text_edit(0, 0, 3, "new"), text_edit(0, 4, 7, "new")],
                path_to_file_uri(&second): [text_edit(0, 0, 3, "other")]
            }}),
        );
        assert_eq!(runtime.editor.active_buffer().text(), "new new\n");
        assert_eq!(runtime.editor.buffers[second_index].buffer.text(), "old\n");
        assert_eq!(
            runtime.editor.current_message(),
            Some("Rename applied to active buffer")
        );
        assert!(runtime.editor.active_buffer_mut().undo().unwrap());
        assert_eq!(runtime.editor.active_buffer().text(), "old old\n");
        runtime.apply_workspace_edit(&json!({"changes": {path_to_file_uri(&first): [
            text_edit(0, 0, 3, "partial"), {"range": {}}
        ]}}));
        assert_eq!(runtime.editor.active_buffer().text(), "old old\n");
        assert_eq!(
            runtime.editor.current_message(),
            Some("Could not apply rename: malformed text edit")
        );
        runtime.apply_workspace_edit(&json!({"changes": {}}));
        assert_eq!(
            runtime.editor.current_message(),
            Some("Rename returned no edits for the active buffer")
        );
    }

    #[test]
    fn completion_responses_are_bounded_and_do_not_change_text_before_acceptance() {
        let directory = tempdir().unwrap();
        let mut runtime = local_runtime(directory.path());
        runtime.editor.handle_key(crate::input::Key::char('i'));
        let items: Vec<_> = (0..205)
            .map(|index| {
                json!({"label": format!("item{index}"),
            "insertText": format!("insert{index}"), "detail": "function"})
            })
            .collect();
        runtime.handle_lsp_response(CommandId::Completion, json!({"items": items}));
        let picker = runtime.editor.picker.as_ref().unwrap();
        assert_eq!(picker.items.len(), 200);
        assert_eq!(picker.items[0].label, "item0");
        assert_eq!(picker.items[0].detail, "function");
        assert_eq!(picker.items[0].insert_text.as_deref(), Some("insert0"));
        assert_eq!(runtime.editor.active_buffer().text(), "");
        runtime.handle_lsp_response(CommandId::Completion, json!([{"label": "fallback"}]));
        assert_eq!(
            runtime.editor.picker.as_ref().unwrap().items[0]
                .insert_text
                .as_deref(),
            Some("fallback")
        );
        assert_eq!(runtime.editor.active_buffer().text(), "");
    }

    #[test]
    fn location_responses_support_links_utf16_and_multiple_targets() {
        let directory = tempdir().unwrap();
        let mut runtime = local_runtime(directory.path());
        let path = directory.path().join("target with space.rs");
        fs::write(&path, "first\n😀 target\n").unwrap();
        let uri = path_to_file_uri(&path);
        let range =
            json!({"start": {"line": 1, "character": 3}, "end": {"line": 1, "character": 9}});
        runtime.handle_lsp_response(
            CommandId::Definition,
            json!({"targetUri": uri, "targetSelectionRange": range}),
        );
        assert_eq!(runtime.editor.active_buffer().path(), Some(path.as_path()));
        assert_eq!(runtime.editor.active_pane().cursor, Pos::new(1, 2));
        runtime.open_locations(json!([
            {"uri": uri, "range": range}, {"targetUri": uri, "targetSelectionRange": range},
            {"uri": "https://invalid.example/ignored", "range": range}
        ]));
        let picker = runtime.editor.picker.as_ref().unwrap();
        assert_eq!(picker.kind, PickerKind::Symbols);
        assert_eq!(picker.items.len(), 2);
        assert_eq!(picker.items[0].path.as_deref(), Some(path.as_path()));
        assert_eq!(picker.items[0].line, Some(1));
        runtime.open_locations(Value::Null);
        assert_eq!(runtime.editor.current_message(), Some("No locations found"));
    }

    #[test]
    fn symbol_and_hover_responses_present_bounded_details() {
        let directory = tempdir().unwrap();
        let mut runtime = local_runtime(directory.path());
        let path = directory.path().join("symbols.rs");
        runtime.handle_lsp_response(
            CommandId::WorkspaceSymbols,
            json!([
                {"name": "workspace", "location": {"uri": path_to_file_uri(&path),
                    "range": {"start": {"line": 4, "character": 0}}}},
                {"name": "local", "range": {"start": {"line": 2, "character": 0}}}
            ]),
        );
        let picker = runtime.editor.picker.as_ref().unwrap();
        assert_eq!(picker.items[0].label, "workspace");
        assert_eq!(picker.items[0].path.as_deref(), Some(path.as_path()));
        assert_eq!(picker.items[0].line, Some(4));
        assert_eq!(picker.items[1].line, Some(2));
        runtime.handle_lsp_response(
            CommandId::Hover,
            json!({"contents": {"kind": "markdown", "value": "first\nsecond"}}),
        );
        assert!(
            runtime
                .editor
                .current_message()
                .unwrap()
                .contains("first second")
        );
        assert_eq!(
            summarize_lsp_text(&json!({"contents": "界".repeat(800)}))
                .chars()
                .count(),
            500
        );
        runtime.handle_lsp_response(
            CommandId::SignatureHelp,
            json!({"signatures": [
                {"label": "fn example(value: u32)", "documentation": {"value": "argument help"}}
            ]}),
        );
        let message = runtime.editor.current_message().unwrap();
        assert!(message.contains("fn example(value: u32)"));
        assert!(message.contains("argument help"));
    }

    #[test]
    fn diagnostics_follow_live_revisions_and_suppress_dirty_disk_results() {
        let directory = tempdir().unwrap();
        let mut runtime = local_runtime(directory.path());
        let path = open_test_file(&mut runtime, "diagnostics.rs", "😀 value\n");
        let uri = path_to_file_uri(&path);
        let diagnostic = |source: &str, severity: u8, message: &str| {
            json!({
                "range": {"start": {"line": 0, "character": 3}},
                "source": source, "severity": severity, "message": message
            })
        };
        runtime.publish_diagnostics(json!({"uri": uri, "version": 0, "diagnostics": [
            diagnostic("rustc", 1, "on disk")
        ]}));
        assert_eq!(runtime.editor.diagnostics.len(), 1);
        assert_eq!(runtime.editor.diagnostics[0].column, 2);
        assert_eq!(
            runtime.editor.diagnostics[0].severity,
            DiagnosticSeverity::Error
        );
        runtime
            .editor
            .active_buffer_mut()
            .begin_transaction()
            .unwrap();
        runtime
            .editor
            .active_buffer_mut()
            .insert(Pos::new(0, 7), "x")
            .unwrap();
        let revision = runtime.editor.active_buffer().revision();
        runtime.redraw = false;
        runtime.discard_stale_diagnostics();
        assert!(runtime.redraw);
        assert!(runtime.editor.diagnostics.is_empty());
        runtime.publish_diagnostics(json!({"uri": uri, "version": 0, "diagnostics": [
            diagnostic("rust-analyzer", 1, "stale")
        ]}));
        assert!(runtime.editor.diagnostics.is_empty());
        runtime.publish_diagnostics(json!({"uri": uri, "version": revision, "diagnostics": [
            diagnostic("rustc", 1, "old disk"), diagnostic("clippy", 2, "old lint"),
            diagnostic("rust-analyzer", 2, "live warning"), diagnostic("rust-analyzer", 4, "live hint")
        ]}));
        assert_eq!(runtime.editor.diagnostics.len(), 2);
        assert_eq!(runtime.editor.diagnostics[0].message, "live warning");
        assert_eq!(
            runtime.editor.diagnostics[0].severity,
            DiagnosticSeverity::Warning
        );
        assert_eq!(
            runtime.editor.diagnostics[1].severity,
            DiagnosticSeverity::Hint
        );
        runtime.publish_diagnostics(json!({"uri": uri, "diagnostics": []}));
        assert!(runtime.editor.diagnostics.is_empty());
        assert_eq!(runtime.diagnostic_versions.get(&path), Some(&revision));
    }

    #[test]
    fn unopened_diagnostics_reject_out_of_order_updates_and_invalid_uris() {
        let directory = tempdir().unwrap();
        let mut runtime = local_runtime(directory.path());
        let path = directory.path().join("closed.rs");
        let uri = path_to_file_uri(&path);
        runtime.publish_diagnostics(
            json!({"uri": uri, "version": 4, "diagnostics": [{"message": "new"}]}),
        );
        runtime.publish_diagnostics(json!({"uri": uri, "version": 3, "diagnostics": []}));
        runtime
            .publish_diagnostics(json!({"uri": "https://example.invalid/file", "diagnostics": []}));
        runtime.publish_diagnostics(json!({"diagnostics": []}));
        runtime.discard_stale_diagnostics();
        assert_eq!(runtime.editor.diagnostics.len(), 1);
        assert_eq!(runtime.editor.diagnostics[0].message, "new");
        assert_eq!(
            runtime.editor.diagnostics[0].severity,
            DiagnosticSeverity::Information
        );
        assert_eq!(runtime.editor.diagnostics[0].column, 0);
        runtime.publish_diagnostics(json!({"uri": uri, "version": 5, "diagnostics": []}));
        assert!(runtime.editor.diagnostics.is_empty());
    }

    #[test]
    fn document_sync_tracks_only_eligible_buffers_and_live_edits() {
        let directory = tempdir().unwrap();
        let mut runtime = local_runtime(directory.path());
        let path = open_test_file(&mut runtime, "active.rs", "let value = 1;\n");
        let active = runtime.editor.active_pane().buffer;
        open_test_file(&mut runtime, "notes.txt", "notes");
        open_test_file(&mut runtime, "large.rs", "large");
        let large = runtime.editor.active_pane().buffer;
        runtime.editor.buffers[large].large_file = true;
        let mut config = RustAnalyzerConfig::new(directory.path());
        config.executable = directory.path().join("unused-analyzer");
        runtime.rust_analyzer = Some(RustAnalyzerClient::new(config).unwrap());
        runtime.sync_lsp_documents();
        assert_eq!(runtime.lsp_versions, HashMap::from([(path.clone(), 0)]));
        runtime.editor.buffers[active]
            .buffer
            .begin_transaction()
            .unwrap();
        runtime.editor.buffers[active]
            .buffer
            .insert(Pos::ZERO, "x")
            .unwrap();
        let revision = runtime.editor.buffers[active].buffer.revision();
        runtime.sync_lsp_documents();
        assert_eq!(runtime.lsp_versions.get(&path), Some(&revision));
        runtime.sync_lsp_documents();
        assert_eq!(runtime.lsp_versions.len(), 1);
        runtime.editor.buffers[active].large_file = true;
        runtime.sync_lsp_documents();
        assert!(runtime.lsp_versions.is_empty());
        assert_eq!(
            runtime.rust_analyzer.as_ref().unwrap().status().state,
            RustAnalyzerState::Stopped
        );
    }

    #[test]
    fn queued_lsp_requests_capture_mutation_origins_and_failed_requests_are_removed() {
        let directory = tempdir().unwrap();
        let mut runtime = local_runtime(directory.path());
        runtime.request_lsp(CommandId::Hover, None);
        assert_eq!(
            runtime.editor.current_message(),
            Some("rust-analyzer integration failed to initialize")
        );
        let mut config = RustAnalyzerConfig::new(directory.path());
        config.executable = directory.path().join("unused-analyzer");
        runtime.rust_analyzer = Some(RustAnalyzerClient::new(config).unwrap());
        runtime.request_lsp(CommandId::Hover, None);
        assert_eq!(
            runtime.editor.current_message(),
            Some("This action requires a saved file")
        );
        runtime.request_lsp(CommandId::WorkspaceSymbols, Some("query".into()));
        let path = open_test_file(&mut runtime, "requests.rs", "😀 value\n");
        runtime.editor.active_pane_mut().cursor = Pos::new(0, 2);
        for command in [
            CommandId::Hover,
            CommandId::Definition,
            CommandId::Declaration,
            CommandId::TypeDefinition,
            CommandId::Implementation,
            CommandId::References,
            CommandId::Completion,
            CommandId::SignatureHelp,
            CommandId::Format,
            CommandId::Rename,
            CommandId::CodeAction,
            CommandId::DocumentSymbols,
        ] {
            runtime.request_lsp(command, Some("renamed".into()));
        }
        assert_eq!(runtime.pending_lsp.len(), 13);
        for pending in runtime.pending_lsp.values() {
            if matches!(pending.command, CommandId::Format | CommandId::Rename) {
                assert_eq!(
                    pending.mutation_origin,
                    Some(LspMutationOrigin {
                        path: path.clone(),
                        revision: 0
                    })
                );
            } else {
                assert!(pending.mutation_origin.is_none());
            }
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while !runtime.pending_lsp.is_empty() && Instant::now() < deadline {
            runtime.drain_lsp();
            thread::yield_now();
        }
        assert!(runtime.pending_lsp.is_empty());
        assert_eq!(
            runtime.editor.current_message(),
            Some("rust-analyzer is stopped")
        );
        assert_eq!(runtime.editor.active_buffer().text(), "😀 value\n");
    }

    #[test]
    fn project_search_replaces_queries_and_clears_cancelled_results() {
        let directory = tempdir().unwrap();
        fs::write(directory.path().join("search.rs"), "alpha\nbeta\n").unwrap();
        let mut runtime = local_runtime(directory.path());
        runtime.editor.explorer.show_ignored = true;
        runtime
            .editor
            .show_picker_items(PickerKind::Grep, Vec::new());
        runtime.editor.picker.as_mut().unwrap().query = "alpha".into();
        runtime.update_project_search();
        let cancelled = runtime
            .project_search
            .as_ref()
            .unwrap()
            .cancellation_token();
        runtime.editor.picker.as_mut().unwrap().query = "beta".into();
        runtime.update_project_search();
        assert!(cancelled.is_cancelled());
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            runtime.drain_project_search();
            let task = runtime.project_search.as_ref().unwrap();
            if task.is_finished() && task.ready_len() == 0 {
                break;
            }
            assert!(Instant::now() < deadline, "project search should finish");
            thread::yield_now();
        }
        assert_eq!(runtime.project_search_results.len(), 1);
        assert_eq!(runtime.project_search_results[0].line, Some(1));
        assert_eq!(runtime.project_search_results[0].detail, "beta");
        assert_eq!(runtime.editor.picker.as_ref().unwrap().items.len(), 1);
        runtime.editor.picker = None;
        runtime.update_project_search();
        assert!(runtime.project_search.is_none());
        assert!(runtime.project_search_results.is_empty());
        assert!(runtime.last_search_query.is_empty());
        runtime
            .editor
            .show_picker_items(PickerKind::Grep, Vec::new());
        runtime.editor.picker.as_mut().unwrap().query = "[".into();
        runtime.update_project_search();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !runtime
            .editor
            .current_message()
            .is_some_and(|message| message.contains("Invalid project search:"))
        {
            runtime.drain_project_search();
            assert!(
                Instant::now() < deadline,
                "invalid regex should report a background error"
            );
            thread::yield_now();
        }
        assert!(runtime.project_search_results.is_empty());
    }

    #[test]
    fn terminal_failures_restore_editor_focus_and_keep_buffer_text() {
        let directory = tempdir().unwrap();
        let mut runtime = local_runtime(directory.path());
        runtime
            .editor
            .active_buffer_mut()
            .insert(Pos::ZERO, "keep")
            .unwrap();
        runtime.editor.focus = crate::editor::Focus::Terminal;
        runtime.write_terminal(b"input");
        assert_eq!(runtime.editor.focus, crate::editor::Focus::Editor);
        assert!(
            runtime
                .editor
                .current_message()
                .unwrap()
                .starts_with("Terminal is not running")
        );
        runtime.editor.focus = crate::editor::Focus::Terminal;
        runtime.redraw = false;
        runtime.fail_terminal("synthetic terminal failure".into());
        assert!(runtime.redraw);
        assert_eq!(runtime.editor.focus, crate::editor::Focus::Editor);
        assert_eq!(
            runtime.editor.terminal.status,
            TerminalStatus::Failed("synthetic terminal failure".into())
        );
        assert_eq!(runtime.editor.active_buffer().text(), "keep");
        runtime.toggle_terminal(false);
        runtime.sync_terminal_size();
        runtime.drain_terminal();
        assert!(runtime.terminal.is_none());
    }

    #[test]
    fn journal_records_are_removed_after_saving_or_undoing_to_clean() {
        for save in [false, true] {
            let directory = tempdir().unwrap();
            let recovery = directory.path().join("recovery");
            let mut runtime = local_runtime(directory.path());
            open_test_file(&mut runtime, "journal.rs", "before");
            runtime.journal = Some(Journal::start(recovery.clone()).unwrap());
            runtime
                .editor
                .active_buffer_mut()
                .insert(Pos::ZERO, "after ")
                .unwrap();
            runtime.journal_buffers();
            assert_eq!(runtime.journal_versions.len(), 1);
            let versions = runtime.journal_versions.clone();
            runtime.journal_buffers();
            assert_eq!(runtime.journal_versions, versions);
            if save {
                runtime.editor.active_buffer_mut().save().unwrap();
            } else {
                runtime.editor.active_buffer_mut().undo().unwrap();
            }
            runtime.journal_buffers();
            assert!(runtime.journal_versions.is_empty());
            drop(runtime.journal.take());
            assert!(state::list_recoverable(&recovery).unwrap().is_empty());
        }
    }

    #[test]
    fn session_delivery_waits_for_data_and_does_not_overwrite_started_editing() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("session.rs");
        fs::write(&path, "saved").unwrap();
        for started_editing in [false, true] {
            let mut runtime = local_runtime(directory.path());
            let (sender, receiver) = mpsc::channel();
            runtime.session_rx = Some(receiver);
            runtime.redraw = false;
            runtime.restore_session_if_ready();
            assert!(runtime.session_rx.is_some());
            assert!(!runtime.redraw);
            if started_editing {
                runtime
                    .editor
                    .active_buffer_mut()
                    .insert(Pos::ZERO, "local")
                    .unwrap();
            }
            sender
                .send(Some(SessionState {
                    version: STATE_VERSION,
                    files: vec![path.clone()],
                    project_root: directory.path().to_owned(),
                    ..SessionState::default()
                }))
                .unwrap();
            runtime.restore_session_if_ready();
            assert!(runtime.session_rx.is_none());
            assert!(runtime.redraw);
            if started_editing {
                assert_eq!(runtime.editor.active_buffer().text(), "local");
                assert!(
                    runtime
                        .editor
                        .current_message()
                        .unwrap()
                        .contains("editing has already started")
                );
            } else {
                assert_eq!(runtime.editor.active_buffer().text(), "saved");
                assert_eq!(runtime.editor.current_message(), Some("Session restored"));
            }
        }
        let mut runtime = local_runtime(directory.path());
        let (sender, receiver) = mpsc::channel();
        runtime.session_rx = Some(receiver);
        drop(sender);
        runtime.restore_session_if_ready();
        assert!(runtime.session_rx.is_none());
    }

    #[test]
    fn runtime_saves_session_state_to_the_supplied_location() {
        let directory = tempdir().unwrap();
        let mut runtime = local_runtime(directory.path());
        let path = open_test_file(&mut runtime, "session.rs", "first\nsecond\n");
        runtime.editor.active_pane_mut().cursor = Pos::new(1, 3);
        runtime.editor.explorer.open = true;
        let session_path = directory.path().join("state/session.json");
        runtime.session_path = Some(session_path.clone());
        runtime.save_session();
        let saved = state::load_session(&session_path).unwrap().unwrap();
        assert_eq!(saved.files, vec![path]);
        assert_eq!(saved.panes[0].cursor_line, 1);
        assert_eq!(saved.panes[0].cursor_grapheme, 3);
        assert!(saved.explorer_open);
    }

    #[test]
    fn external_reload_preserves_dirty_buffers_and_rejects_oversized_replacements() {
        let directory = tempdir().unwrap();
        let mut runtime = local_runtime(directory.path());
        let clean_path = open_test_file(&mut runtime, "clean.rs", "first line\nsecond\n");
        let clean = runtime.editor.active_pane().buffer;
        let dirty_path = open_test_file(&mut runtime, "dirty.rs", "original");
        let dirty = runtime.editor.active_pane().buffer;
        runtime
            .editor
            .active_buffer_mut()
            .insert(Pos::ZERO, "local ")
            .unwrap();
        let large_path = open_test_file(&mut runtime, "large.rs", "small");
        let large = runtime.editor.active_pane().buffer;
        let transaction_path = open_test_file(&mut runtime, "transaction.rs", "old");
        let transaction = runtime.editor.active_pane().buffer;
        runtime
            .editor
            .active_buffer_mut()
            .begin_transaction()
            .unwrap();
        runtime.editor.config.limits.max_file_bytes = 8;
        fs::write(&clean_path, "x\n").unwrap();
        fs::write(&dirty_path, "disk").unwrap();
        fs::write(&large_path, "replacement exceeds limit").unwrap();
        fs::write(&transaction_path, "changed").unwrap();
        runtime.redraw = false;
        runtime.reload_clean_external_changes();
        assert_eq!(runtime.editor.buffers[clean].buffer.text(), "x\n");
        assert_eq!(
            runtime.editor.buffers[dirty].buffer.text(),
            "local original"
        );
        assert_eq!(runtime.editor.buffers[large].buffer.text(), "small");
        assert_eq!(runtime.editor.buffers[transaction].buffer.text(), "old");
        assert!(runtime.redraw);
        assert!(
            runtime
                .editor
                .current_message()
                .unwrap()
                .contains("Skipped reloading")
        );
    }

    #[test]
    fn recovery_tracks_typing_without_closing_the_undo_transaction() {
        let directory = tempdir().unwrap();
        let mut runtime = local_runtime(directory.path());
        runtime.journal = Some(Journal::start(directory.path().join("recovery")).unwrap());
        for character in "iabc".chars() {
            runtime
                .editor
                .handle_key(crate::input::Key::char(character));
        }
        let committed = runtime.editor.active_buffer().version();
        runtime.journal_buffers();
        for character in "def".chars() {
            runtime
                .editor
                .handle_key(crate::input::Key::char(character));
        }
        assert_eq!(runtime.editor.active_buffer().version(), committed);
        assert!(runtime.editor.active_buffer().in_transaction());
        runtime.journal_buffers();
        // Joining the writer makes this a deterministic test of persisted
        // recovery text, rather than a timing assertion about queue delivery.
        drop(runtime.journal.take());
        let records = state::list_recoverable(&directory.path().join("recovery")).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].text, "abcdef");
    }

    #[test]
    fn idle_ticks_do_not_request_frames_but_input_and_results_do() {
        let directory = tempdir().unwrap();
        let mut runtime = local_runtime(directory.path());
        runtime.redraw = false;
        runtime.handle_input(InputEvent::Tick);
        assert!(!runtime.redraw);
        runtime.handle_input(InputEvent::Key(crate::input::Key::char('i')));
        assert!(runtime.redraw);
        runtime.redraw = false;
        runtime.handle_input(InputEvent::Resize);
        assert!(runtime.redraw);
        runtime.redraw = false;
        runtime.project_scan = Some(project::scan_project(
            directory.path(),
            project::ScanOptions {
                include_ignored: true,
                ..Default::default()
            },
        ));
        let deadline = Instant::now() + Duration::from_secs(2);
        while !runtime.redraw && Instant::now() < deadline {
            runtime.handle_input(InputEvent::Tick);
            thread::yield_now();
        }
        assert!(
            runtime.redraw,
            "background scan completion must update the frame"
        );
    }

    #[test]
    fn file_uri_decoding_is_utf8_and_percent_aware() {
        assert_eq!(
            file_uri_to_path("file:///tmp/a%20b/%E7%95%8C.rs"),
            Some(PathBuf::from("/tmp/a b/界.rs"))
        );
    }

    #[test]
    fn parses_lsp_text_edits() {
        let value = json!([{"range":{"start":{"line":1,"character":2},"end":{"line":1,"character":4}},"newText":"x"}]);
        let edits = parse_text_edits(value.as_array().unwrap());
        assert_eq!(edits[0].0, Utf16Pos::new(1, 2));
        assert_eq!(edits[0].2, "x");
    }

    #[test]
    fn mutating_lsp_origin_requires_the_same_live_revision() {
        let directory = tempdir().unwrap();
        let first = directory.path().join("first.rs");
        let second = directory.path().join("second.rs");
        std::fs::write(&first, "fn first() {}\n").unwrap();
        std::fs::write(&second, "fn second() {}\n").unwrap();
        let mut editor = Editor::new(Config::default(), directory.path().to_owned());
        editor.open_path(&first).unwrap();
        editor.discard_initial_scratch();

        let committed_version = editor.active_buffer().version();
        let origin = LspMutationOrigin {
            path: first.canonicalize().unwrap(),
            revision: editor.active_buffer().revision(),
        };
        assert!(lsp_mutation_origin_matches(&editor, &origin));

        editor.active_buffer_mut().begin_transaction().unwrap();
        editor
            .active_buffer_mut()
            .insert(crate::buffer::Pos::ZERO, "x")
            .unwrap();
        assert_eq!(editor.active_buffer().version(), committed_version);
        assert!(!lsp_mutation_origin_matches(&editor, &origin));
        editor.active_buffer_mut().rollback_transaction().unwrap();

        editor.open_path(&second).unwrap();
        assert!(!lsp_mutation_origin_matches(&editor, &origin));
    }

    #[test]
    fn diagnostics_reject_non_current_and_out_of_order_versions() {
        assert!(diagnostic_version_is_current(Some(7), Some(9), 7));
        assert!(!diagnostic_version_is_current(Some(7), Some(6), 6));
        assert!(!diagnostic_version_is_current(Some(7), Some(7), 8));
        assert!(diagnostic_version_is_current(None, Some(7), 7));
        assert!(diagnostic_version_is_current(None, Some(7), 8));
        assert!(!diagnostic_version_is_current(None, Some(7), 6));
    }

    #[test]
    fn identifies_diagnostics_from_on_disk_checkers() {
        assert!(diagnostic_is_disk_backed(&json!({"source": "rustc"})));
        assert!(diagnostic_is_disk_backed(&json!({"source": "clippy"})));
        assert!(!diagnostic_is_disk_backed(
            &json!({"source": "rust-analyzer"})
        ));
        assert!(!diagnostic_is_disk_backed(&json!({})));
    }

    #[test]
    fn session_active_index_ignores_the_initial_scratch_buffer() {
        let directory = tempdir().unwrap();
        let first = directory.path().join("first.rs");
        let second = directory.path().join("second.rs");
        std::fs::write(&first, "first\n").unwrap();
        std::fs::write(&second, "second\n").unwrap();
        let mut editor = Editor::new(Config::default(), directory.path().to_owned());
        editor.open_path(&first).unwrap();
        editor.open_path(&second).unwrap();

        assert_eq!(editor.active_pane().buffer, 2);
        let session = session_state_from_editor(&editor);
        assert_eq!(
            session.files,
            vec![
                first.canonicalize().unwrap(),
                second.canonicalize().unwrap(),
            ]
        );
        assert_eq!(session.active, 1);
    }

    #[test]
    fn session_restore_rebuilds_panes_viewports_and_active_selection() {
        let directory = tempdir().unwrap();
        let first = directory.path().join("first.rs");
        let second = directory.path().join("second.rs");
        std::fs::write(&first, "zero\none two\nabcdefgh\nlast\n").unwrap();
        std::fs::write(&second, "alpha\nbeta\ngamma delta\nomega\n").unwrap();
        let first = first.canonicalize().unwrap();
        let second = second.canonicalize().unwrap();
        let expanded = directory.path().join("src");
        let session = SessionState {
            version: STATE_VERSION,
            project_root: directory.path().to_owned(),
            files: vec![first.clone(), second.clone()],
            active: 1,
            // The active pane is first, matching session_state_from_editor.
            panes: vec![
                SessionPane {
                    file: second.clone(),
                    cursor_line: 2,
                    cursor_grapheme: 5,
                    viewport_line: 1,
                    viewport_column: 7,
                },
                SessionPane {
                    file: first.clone(),
                    cursor_line: 3,
                    cursor_grapheme: 2,
                    viewport_line: 2,
                    viewport_column: 4,
                },
            ],
            explorer_open: true,
            explorer_width: 31,
            expanded_directories: vec![expanded.clone()],
        };
        let mut editor = Editor::new(Config::default(), directory.path().to_owned());

        assert!(restore_session_state(&mut editor, session));
        assert_eq!(
            editor.buffers.len(),
            2,
            "the pristine scratch buffer is discarded"
        );
        assert_eq!(editor.panes.len(), 2);

        let second_pane = editor
            .panes
            .iter()
            .find(|pane| editor.buffers[pane.buffer].buffer.path() == Some(second.as_path()))
            .unwrap();
        assert_eq!(second_pane.cursor, Pos::new(2, 5));
        assert_eq!(second_pane.viewport_line, 1);
        assert_eq!(second_pane.viewport_column, 7);
        assert_eq!(editor.active_pane, second_pane.id);

        let first_pane = editor
            .panes
            .iter()
            .find(|pane| editor.buffers[pane.buffer].buffer.path() == Some(first.as_path()))
            .unwrap();
        assert_eq!(first_pane.cursor, Pos::new(3, 2));
        assert_eq!(first_pane.viewport_line, 2);
        assert_eq!(first_pane.viewport_column, 4);
        assert!(matches!(
            editor.layout,
            crate::editor::Layout::Split {
                orientation: Orientation::Vertical,
                ..
            }
        ));
        assert!(editor.explorer.open);
        assert_eq!(editor.explorer.width, 31);
        assert!(editor.explorer.expanded.contains(&expanded));
    }

    #[test]
    fn session_restore_does_not_replace_a_modified_scratch_buffer() {
        let directory = tempdir().unwrap();
        let file = directory.path().join("saved.rs");
        std::fs::write(&file, "saved\n").unwrap();
        let mut editor = Editor::new(Config::default(), directory.path().to_owned());
        editor
            .active_buffer_mut()
            .insert(Pos::ZERO, "unsaved")
            .unwrap();
        let session = SessionState {
            version: STATE_VERSION,
            project_root: directory.path().to_owned(),
            files: vec![file],
            ..SessionState::default()
        };

        assert!(!restore_session_state(&mut editor, session));
        assert_eq!(editor.active_buffer().text(), "unsaved");
        assert!(editor.active_buffer().path().is_none());
    }

    #[test]
    fn external_reload_clamps_every_pane_showing_the_shortened_buffer() {
        let directory = tempdir().unwrap();
        let file = directory.path().join("file.rs");
        std::fs::write(&file, "first line\nsecond line\nthird line\nfourth line\n").unwrap();
        let mut editor = Editor::new(Config::default(), directory.path().to_owned());
        editor.open_path(&file).unwrap();
        editor.discard_initial_scratch();
        let buffer = editor.active_pane().buffer;
        editor.split(Orientation::Vertical);
        for pane in &mut editor.panes {
            pane.cursor = Pos::new(3, 8);
            pane.anchor = Some(Pos::new(2, 6));
            pane.viewport_line = 3;
            pane.desired_column = 8;
        }

        std::fs::write(&file, "x\n").unwrap();
        assert!(
            editor.buffers[buffer]
                .buffer
                .may_have_changed_on_disk()
                .unwrap()
        );
        assert!(editor.buffers[buffer].buffer.reload().unwrap());
        clamp_panes_for_buffer(&mut editor, buffer);

        for pane in &editor.panes {
            assert_eq!(pane.cursor, Pos::new(0, 1));
            assert_eq!(pane.anchor, Some(Pos::new(0, 1)));
            assert_eq!(pane.viewport_line, 0);
            assert_eq!(pane.desired_column, 1);
        }
    }
}
