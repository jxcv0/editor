//! Exercise the installed entry point and terminal I/O with private state/config.
#![cfg(unix)]

use std::{
    fs,
    io::{Read, Write},
    path::PathBuf,
    process::{Command, Output, Stdio},
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant},
};

use editor::state::{self, SessionState};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};

struct Workspace {
    _temporary: tempfile::TempDir,
    project: PathBuf,
    config: PathBuf,
    state: PathBuf,
}

impl Workspace {
    fn git(&self, args: &[&str]) -> Vec<u8> {
        let output = Command::new("git")
            .args(args)
            .current_dir(&self.project)
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
    fn new() -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        let config = temporary.path().join("config");
        let state = temporary.path().join("state");
        fs::create_dir_all(&project).unwrap();
        // Give every fixture its own root, independent of the runner's parent
        // Git checkout (including sandbox mounts around the temp directory).
        fs::create_dir(project.join(".git")).unwrap();
        fs::create_dir_all(config.join("editor")).unwrap();
        let missing = temporary
            .path()
            .join("unavailable-tool")
            .to_str()
            .unwrap()
            .to_owned();
        let mut settings = editor::config::Config::default();
        // Keep the fixtures independent of the user's global Git ignores.
        settings.ui.show_ignored = true;
        settings.tools.rust_analyzer.path = missing;
        fs::write(
            config.join("editor/config.toml"),
            toml::to_string(&settings).unwrap(),
        )
        .unwrap();
        Self {
            _temporary: temporary,
            project,
            config,
            state,
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_editor"));
        command
            .args(args)
            .current_dir(&self.project)
            .env("XDG_CONFIG_HOME", &self.config)
            .env("XDG_STATE_HOME", &self.state);
        command
    }

    fn output(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    fn session(&self) -> SessionState {
        state::load_session(&self.session_path())
            .unwrap()
            .expect("a clean editor exit must save session metadata")
    }

    fn session_path(&self) -> PathBuf {
        self.state
            .join("editor/sessions")
            .join(format!("{}.json", state::project_key(&self.project)))
    }

    fn terminal(&self, args: &[&str]) -> EditorTerminal {
        let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_editor"));
        command.args(args);
        command.cwd(&self.project);
        command.env("XDG_CONFIG_HOME", &self.config);
        command.env("XDG_STATE_HOME", &self.state);
        command.env("TERM", "xterm-256color");
        command.env("SHELL", "/bin/sh");
        command.env("ENV", "/dev/null");
        command.env("PS1", "editor-test-shell> ");
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 100,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut reader = pair.master.try_clone_reader().unwrap();
        let writer = pair.master.take_writer().unwrap();
        let child = pair.slave.spawn_command(command).unwrap();
        drop(pair.slave);
        let (sender, output) = mpsc::sync_channel(64);
        thread::spawn(move || {
            let mut bytes = [0; 8192];
            while let Ok(length) = reader.read(&mut bytes) {
                if length == 0 || sender.send(bytes[..length].to_vec()).is_err() {
                    break;
                }
            }
        });
        EditorTerminal {
            master: pair.master,
            writer,
            child,
            output,
            parser: vt100::Parser::new(24, 100, 0),
            raw: Vec::new(),
        }
    }
}

#[test]
fn terminal_git_views_stage_unstage_and_show_line_history() {
    let workspace = Workspace::new();
    workspace.git(&["init", "-q"]);
    workspace.git(&["config", "user.name", "Terminal Test"]);
    workspace.git(&["config", "user.email", "test@example.invalid"]);
    workspace.git(&["config", "commit.gpgsign", "false"]);
    workspace.git(&["config", "core.hooksPath", "/dev/null"]);
    let path = workspace.project.join("file.rs");
    fs::write(&path, "first\noriginal\n").unwrap();
    workspace.git(&["add", "file.rs"]);
    workspace.git(&["commit", "-qm", "Original terminal fixture"]);
    fs::write(&path, "first\nmodified\n").unwrap();
    let mut terminal = workspace.terminal(&["--no-session", "file.rs"]);
    terminal.wait_for("NORMAL");
    terminal.ex("git diff");
    terminal.wait_for("Unstaged (index");
    terminal.wait_for("BEFORE");
    terminal.wait_for("AFTER");
    terminal.wait_for("2 - original");
    terminal.wait_for("2 + modified");
    terminal.send(b"s");
    terminal.wait_for("Saved file staged");
    assert!(!workspace.git(&["diff", "--cached"]).is_empty());
    terminal.send(b"D");
    terminal.wait_for("Staged (HEAD");
    terminal.wait_for("2 + modified");
    terminal.send(b"u");
    terminal.wait_for("File unstaged");
    assert!(workspace.git(&["diff", "--cached"]).is_empty());
    terminal.send(b"q");
    terminal.wait_for("NORMAL");
    terminal.ex("git blame");
    terminal.wait_for("Line 1:");
    terminal.wait_for("Terminal Test");
    terminal.send(b"q");
    terminal.wait_for("NORMAL");
    terminal.ex("git commit");
    terminal.wait_for("2 + original");
    terminal.send(b"q");
    terminal.wait_for("NORMAL");
    terminal.ex("q");
    terminal.finish();
    assert_eq!(fs::read_to_string(path).unwrap(), "first\nmodified\n");
}

struct EditorTerminal {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    output: Receiver<Vec<u8>>,
    parser: vt100::Parser,
    raw: Vec<u8>,
}

impl EditorTerminal {
    fn send(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).unwrap();
        self.writer.flush().unwrap();
    }

    fn read_output(&mut self) {
        if let Ok(bytes) = self.output.recv_timeout(Duration::from_millis(20)) {
            self.parser.process(&bytes);
            self.raw.extend_from_slice(&bytes);
            assert!(
                self.raw.len() < 1024 * 1024,
                "unexpected terminal output flood"
            );
        }
    }

    fn wait_for(&mut self, text: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self.parser.screen().contents().contains(text) {
            assert!(
                Instant::now() < deadline,
                "waiting for {text:?}; screen: {:?}",
                self.parser.screen().contents()
            );
            self.read_output();
        }
    }

    fn wait_for_copies(&mut self, text: &str, copies: usize) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.parser.screen().contents().matches(text).count() != copies {
            assert!(
                Instant::now() < deadline,
                "waiting for {copies} copies of {text:?}; screen: {:?}",
                self.parser.screen().contents()
            );
            self.read_output();
        }
    }

    fn normal_mode(&mut self) {
        // Wait for the standalone escape to be decoded before sending ':'.
        self.send(b"\x1b");
        self.wait_for("NORMAL");
    }

    fn ex(&mut self, command: &str) {
        self.send(format!(":{command}").as_bytes());
        self.wait_for("COMMAND");
        self.wait_for(&format!(":{command}"));
        self.send(b"\r");
    }

    fn finish(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            self.read_output();
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "editor did not exit; screen: {:?}",
                self.parser.screen().contents()
            );
        };
        loop {
            match self.output.recv_timeout(Duration::from_millis(20)) {
                Ok(bytes) => {
                    self.raw.extend_from_slice(&bytes);
                    assert!(
                        self.raw.len() < 1024 * 1024,
                        "unexpected terminal output flood"
                    );
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    assert!(
                        Instant::now() < deadline,
                        "PTY reader did not finish after editor exit"
                    );
                }
            }
        }
        assert!(status.success(), "editor failed: {status}; {:?}", self.raw);
        assert!(
            self.raw
                .windows(b"\x1b[?1049l".len())
                .any(|part| part == b"\x1b[?1049l"),
            "clean exit must leave the alternate screen"
        );
    }
}

impl Drop for EditorTerminal {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[test]
fn integrated_terminal_ctrl_d_closes_exited_shell_and_can_reopen() {
    let workspace = Workspace::new();
    fs::write(workspace.project.join("edit.txt"), "original\n").unwrap();
    let mut terminal = workspace.terminal(&["--no-session", "edit.txt"]);
    terminal.wait_for("NORMAL");
    terminal.ex("terminal");
    terminal.wait_for("editor-test-shell>");
    terminal.wait_for("TERMINAL");

    // EOF should finish a foreground program without closing its parent shell.
    terminal.send(b"cat; printf '\\nEOF-%s\\n' handled\r");
    terminal.send(b"\x04");
    terminal.wait_for("EOF-handled");
    assert!(terminal.parser.screen().contents().contains("TERMINAL"));

    terminal.send(b"\x04");
    terminal.wait_for_copies("TERMINAL", 0);
    assert!(terminal.child.try_wait().unwrap().is_none());
    terminal.send(b"iAFTER-EOF-");
    terminal.wait_for("AFTER-EOF-original");
    terminal.normal_mode();

    terminal.ex("terminal");
    terminal.wait_for("editor-test-shell>");
    terminal.wait_for("TERMINAL");
    assert!(!terminal.parser.screen().contents().contains("EOF-handled"));
    terminal.send(b"printf 'fresh-%s\\n' shell\r");
    terminal.wait_for("fresh-shell");
    terminal.send(b"exit\r");
    terminal.wait_for_copies("TERMINAL", 0);
    terminal.ex("wq");
    terminal.finish();
    assert_eq!(
        fs::read_to_string(workspace.project.join("edit.txt")).unwrap(),
        "AFTER-EOF-original\n"
    );
}

#[test]
fn project_tree_expands_collapses_opens_files_and_restores_expansion() {
    let workspace = Workspace::new();
    fs::create_dir_all(workspace.project.join("src/nested")).unwrap();
    fs::write(
        workspace.project.join("src/nested/leaf.rs"),
        "tree file opened\n",
    )
    .unwrap();
    let mut terminal = workspace.terminal(&["--no-session", "."]);
    terminal.wait_for("▸ src/");
    assert!(!terminal.parser.screen().contents().contains("▸ nested/"));
    terminal.send(b"l");
    terminal.wait_for("▸ nested/");
    terminal.send(b"j\r");
    terminal.wait_for("leaf.rs");
    terminal.send(b"j\r");
    terminal.wait_for("tree file opened");
    terminal.send(b" e");
    terminal.send(b"hh");
    terminal.wait_for("▸ nested/");
    terminal.send(b"h\r");
    terminal.wait_for("▸ src/");
    assert!(!terminal.parser.screen().contents().contains("▸ nested/"));
    terminal.send(b"\r");
    terminal.wait_for("▸ nested/");
    terminal.send(b"\x17l");
    terminal.ex("qa");
    terminal.finish();
    let session = workspace.session();
    assert!(
        session
            .expanded_directories
            .contains(&workspace.project.join("src"))
    );
    assert!(
        !session
            .expanded_directories
            .contains(&workspace.project.join("src/nested"))
    );

    let mut restored = workspace.terminal(&["."]);
    restored.wait_for("tree file opened");
    restored.wait_for("▸ nested/");
    restored.send(b"\x17l");
    restored.ex("qa");
    restored.finish();
}

#[test]
fn help_and_version_work_without_loading_invalid_configuration() {
    let workspace = Workspace::new();
    fs::write(workspace.project.join(".editor.toml"), "invalid toml").unwrap();
    for (arg, expected) in [
        ("--help", editor::cli::HELP.to_string()),
        ("--version", editor::cli::VERSION.to_string()),
    ] {
        let output = workspace.output(&[arg]);
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().trim_end(),
            expected.trim_end()
        );
        assert!(output.stderr.is_empty());
    }
    assert!(!workspace.state.exists());
}

#[test]
fn invalid_options_fail_before_creating_editor_state() {
    let workspace = Workspace::new();
    for args in [
        vec!["--unknown"],
        vec!["+0", "file"],
        vec!["-", "-"],
        vec!["+3"],
    ] {
        let output = workspace.output(&args);
        assert!(!output.status.success(), "accepted {args:?}");
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).starts_with("editor: "));
    }
    assert!(!workspace.state.exists());
}

#[test]
fn invalid_project_configuration_reports_the_source_and_reason() {
    let workspace = Workspace::new();
    let path = workspace.project.join(".editor.toml");
    for (config, reason) in [
        ("[editor", "TOML parse error"),
        ("schema_version = 2", "unsupported schema_version"),
        ("[editor]\ntab_width = 0", "editor.tab_width"),
        ("[ui]\nexplorer_width = 121", "ui.explorer_width"),
        ("[ui.theme]\nbackground = 'blue'", "invalid theme color"),
        ("[ui.theme]\nbackground = '#aéabc'", "invalid theme color"),
        (
            "[keymap]\n'' = 'code.hover'",
            "keymap bindings cannot be empty",
        ),
        ("magic = true", "unknown field"),
    ] {
        fs::write(&path, config).unwrap();
        let output = workspace.output(&["--no-session"]);
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "accepted {config}");
        assert!(error.contains(path.to_str().unwrap()), "{error}");
        assert!(error.contains(reason), "{error}");
        assert!(output.stdout.is_empty());
    }
    assert!(!workspace.state.exists());
}

#[test]
fn invalid_utf8_stdin_is_rejected_before_entering_the_terminal() {
    let workspace = Workspace::new();
    let mut child = workspace
        .command(&["-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"valid\xff").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("standard input is not UTF-8 (valid through byte 5)")
    );
    assert!(!workspace.state.exists());
}

#[test]
fn terminal_edits_first_file_at_grapheme_position_and_saves_all_session_files() {
    let workspace = Workspace::new();
    let first = workspace.project.join("first.txt");
    let second = workspace.project.join("second.txt");
    fs::write(&first, "first\nα🙂z\n").unwrap();
    fs::write(&second, "second untouched\n").unwrap();
    let mut terminal = workspace.terminal(&["--no-session", "+2:2", "first.txt", "second.txt"]);
    terminal.wait_for("NORMAL");
    terminal.wait_for("α🙂z");
    terminal.send(b"iINSERTED");
    terminal.wait_for("INSERTED");
    terminal.normal_mode();
    terminal.ex("wq");
    terminal.finish();
    assert_eq!(fs::read_to_string(&first).unwrap(), "first\nαINSERTED🙂z\n");
    assert_eq!(fs::read_to_string(&second).unwrap(), "second untouched\n");
    let session = workspace.session();
    assert_eq!(session.files, vec![first, second]);
    assert_eq!(session.active, 0);
    assert_eq!(session.panes[0].cursor_line, 1);
}

#[test]
fn terminal_quit_closes_the_active_split_then_exits_from_the_last_pane() {
    let workspace = Workspace::new();
    let path = workspace.project.join("split.txt");
    fs::write(&path, "split body marker\n").unwrap();
    let mut terminal = workspace.terminal(&["--no-session", "split.txt"]);
    terminal.wait_for("NORMAL");
    terminal.ex("split");
    terminal.wait_for("NORMAL");
    terminal.wait_for_copies("split body marker", 2);

    terminal.ex("q");
    terminal.wait_for("NORMAL");
    terminal.wait_for_copies("split body marker", 1);
    assert!(terminal.child.try_wait().unwrap().is_none());
    terminal.ex("q");
    terminal.finish();

    assert_eq!(fs::read_to_string(&path).unwrap(), "split body marker\n");
    let session = workspace.session();
    assert_eq!(session.files, vec![path.clone()]);
    assert_eq!(session.panes.len(), 1);
    assert_eq!(session.panes[0].file, path);
}

#[test]
fn terminal_quit_preserves_dirty_buffer_in_the_remaining_duplicate_pane() {
    for quit in ["q", "q!"] {
        let workspace = Workspace::new();
        let path = workspace.project.join("shared.txt");
        fs::write(&path, "shared body marker\n").unwrap();
        let mut terminal = workspace.terminal(&["--no-session", "shared.txt"]);
        terminal.wait_for("NORMAL");
        terminal.send(b"iKEEP-");
        terminal.wait_for("KEEP-shared body marker");
        terminal.normal_mode();
        terminal.ex("vsplit");
        terminal.wait_for("NORMAL");
        terminal.wait_for_copies("KEEP-shared body marker", 2);

        terminal.ex(quit);
        terminal.wait_for("NORMAL");
        terminal.wait_for_copies("KEEP-shared body marker", 1);
        terminal.wait_for("shared.txt [+]");
        assert!(terminal.child.try_wait().unwrap().is_none());
        assert_eq!(fs::read_to_string(&path).unwrap(), "shared body marker\n");
        terminal.ex("wq");
        terminal.finish();

        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "KEEP-shared body marker\n"
        );
        assert_eq!(workspace.session().panes.len(), 1);
    }
}

#[test]
fn terminal_quit_protects_a_dirty_sole_view_and_force_discards_only_that_split() {
    let workspace = Workspace::new();
    let first = workspace.project.join("first.txt");
    let second = workspace.project.join("second.txt");
    fs::write(&first, "first body marker\n").unwrap();
    fs::write(&second, "second body marker\n").unwrap();
    let mut terminal = workspace.terminal(&["--no-session", "first.txt"]);
    terminal.wait_for("NORMAL");
    terminal.send(b"iKEEP-");
    terminal.wait_for("KEEP-first body marker");
    terminal.normal_mode();
    terminal.ex("split second.txt");
    terminal.wait_for("NORMAL");
    terminal.wait_for("second body marker");
    terminal.send(b"iDISCARD-");
    terminal.wait_for("DISCARD-second body marker");
    terminal.normal_mode();

    terminal.ex("q");
    terminal.wait_for("NORMAL");
    terminal.wait_for("second.txt [+]");
    terminal.wait_for("DISCARD-second body marker");
    terminal.wait_for("KEEP-first body marker");
    assert!(terminal.child.try_wait().unwrap().is_none());
    terminal.ex("q!");
    terminal.wait_for("NORMAL");
    terminal.wait_for_copies("second body marker", 0);
    terminal.wait_for("first.txt [+]");
    terminal.wait_for_copies("KEEP-first body marker", 1);
    assert!(terminal.child.try_wait().unwrap().is_none());
    assert_eq!(fs::read_to_string(&second).unwrap(), "second body marker\n");
    terminal.ex("wq");
    terminal.finish();

    assert_eq!(
        fs::read_to_string(&first).unwrap(),
        "KEEP-first body marker\n"
    );
    assert_eq!(fs::read_to_string(&second).unwrap(), "second body marker\n");
    let session = workspace.session();
    assert_eq!(session.panes.len(), 1);
    assert_eq!(session.panes[0].file, first);
}

#[test]
fn terminal_write_quit_saves_and_closes_only_the_active_split() {
    let workspace = Workspace::new();
    let first = workspace.project.join("first.txt");
    let second = workspace.project.join("second.txt");
    fs::write(&first, "first body marker\n").unwrap();
    fs::write(&second, "second body marker\n").unwrap();
    let mut terminal = workspace.terminal(&["--no-session", "first.txt"]);
    terminal.wait_for("NORMAL");
    terminal.ex("vsplit second.txt");
    terminal.wait_for("NORMAL");
    terminal.wait_for("second body marker");
    terminal.send(b"iSAVED-");
    terminal.wait_for("SAVED-second body marker");
    terminal.normal_mode();

    terminal.ex("wq");
    terminal.wait_for("NORMAL");
    terminal.wait_for_copies("second body marker", 0);
    terminal.wait_for_copies("first body marker", 1);
    assert!(terminal.child.try_wait().unwrap().is_none());
    assert_eq!(
        fs::read_to_string(&second).unwrap(),
        "SAVED-second body marker\n"
    );
    assert_eq!(fs::read_to_string(&first).unwrap(), "first body marker\n");
    terminal.send(b"iSTILL-EDITING-");
    terminal.wait_for("STILL-EDITING-first body marker");
    terminal.normal_mode();
    terminal.ex("wq");
    terminal.finish();

    assert_eq!(
        fs::read_to_string(&first).unwrap(),
        "STILL-EDITING-first body marker\n"
    );
    let session = workspace.session();
    assert_eq!(session.files, vec![first.clone(), second]);
    assert_eq!(session.panes.len(), 1);
    assert_eq!(session.panes[0].file, first);
}

#[test]
fn terminal_force_quit_of_last_pane_reveals_another_hidden_dirty_buffer() {
    let workspace = Workspace::new();
    let first = workspace.project.join("first.txt");
    let second = workspace.project.join("second.txt");
    fs::write(&first, "first body marker\n").unwrap();
    fs::write(&second, "second body marker\n").unwrap();
    let mut terminal = workspace.terminal(&["--no-session", "first.txt"]);
    terminal.wait_for("NORMAL");
    terminal.send(b"iKEEP-");
    terminal.wait_for("KEEP-first body marker");
    terminal.normal_mode();
    terminal.ex("e second.txt");
    terminal.wait_for("NORMAL");
    terminal.wait_for_copies("first body marker", 0);
    terminal.send(b"iDISCARD-");
    terminal.wait_for("DISCARD-second body marker");
    terminal.normal_mode();

    terminal.ex("q!");
    terminal.wait_for("NORMAL");
    terminal.wait_for("first.txt [+]");
    terminal.wait_for_copies("KEEP-first body marker", 1);
    terminal.wait_for_copies("second body marker", 0);
    assert!(terminal.child.try_wait().unwrap().is_none());
    terminal.ex("wq");
    terminal.finish();

    assert_eq!(
        fs::read_to_string(&first).unwrap(),
        "KEEP-first body marker\n"
    );
    assert_eq!(fs::read_to_string(&second).unwrap(), "second body marker\n");
    let session = workspace.session();
    assert_eq!(session.files, vec![first.clone()]);
    assert_eq!(session.panes.len(), 1);
    assert_eq!(session.panes[0].file, first);
}

#[test]
fn terminal_clamps_start_position_and_handles_resize_and_bracketed_paste() {
    let workspace = Workspace::new();
    let path = workspace.project.join("short.txt");
    fs::write(&path, "abc").unwrap();
    let mut terminal = workspace.terminal(&["--no-session", "+999:999", "short.txt"]);
    terminal.wait_for("NORMAL");
    terminal
        .master
        .resize(PtySize {
            rows: 30,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    terminal.parser.screen_mut().set_size(30, 120);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !terminal
        .parser
        .screen()
        .contents()
        .lines()
        .nth(28)
        .is_some_and(|line| line.contains("NORMAL"))
    {
        assert!(Instant::now() < deadline, "resize was not rendered");
        terminal.read_output();
    }
    terminal.send(b"i");
    terminal.wait_for("INSERT");
    terminal.send("\x1b[200~🙂\nnext\x1b[201~".as_bytes());
    terminal.wait_for("next");
    terminal.normal_mode();
    terminal.ex("wq");
    terminal.finish();
    assert_eq!(fs::read_to_string(path).unwrap(), "abc🙂\nnext");
}

#[test]
fn terminal_stdin_scratch_protects_unsaved_input_and_can_be_saved() {
    let workspace = Workspace::new();
    let mut terminal = workspace.terminal(&["-"]);
    // Canonical-mode EOF completes read_to_end before the editor enables raw mode.
    terminal.send(b"from stdin\n\x04");
    terminal.wait_for("NORMAL");
    terminal.wait_for("[stdin]");
    terminal.ex("qa");
    // Startup tool/index messages can replace the transient quit warning in
    // the same frame. Assert the durable result: back in Normal with dirty
    // input still present, and the process remains available to save it.
    terminal.wait_for("NORMAL");
    terminal.wait_for("[stdin] [+]");
    assert!(terminal.child.try_wait().unwrap().is_none());
    terminal.ex("wq imported.txt");
    terminal.finish();
    assert_eq!(
        fs::read_to_string(workspace.project.join("imported.txt")).unwrap(),
        "from stdin\n"
    );
}

#[test]
fn directory_startup_applies_project_ui_config_and_skips_session_when_requested() {
    let workspace = Workspace::new();
    fs::write(
        workspace.project.join(".editor.toml"),
        "[ui]\nexplorer_width = 42\n[tools]\nnot_a_tool = true\n",
    )
    .unwrap();
    fs::write(workspace.project.join("visible.txt"), "file\n").unwrap();
    state::save_session(
        &workspace.session_path(),
        &SessionState {
            version: state::STATE_VERSION,
            project_root: workspace.project.clone(),
            files: vec![workspace.project.join("visible.txt")],
            ..SessionState::default()
        },
    )
    .unwrap();
    let mut terminal = workspace.terminal(&["--no-session", "."]);
    terminal.wait_for("NORMAL");
    terminal.wait_for("visible.txt");
    // Return focus from explorer before typing an Ex command.
    terminal.send(b"\x17l");
    terminal.ex("qa");
    terminal.finish();
    let session = workspace.session();
    assert!(session.explorer_open);
    assert_eq!(session.explorer_width, 42);
    assert!(session.files.is_empty());
}

#[test]
fn project_startup_restores_saved_files_and_cursor_before_editing() {
    let workspace = Workspace::new();
    let file = workspace.project.join("restore.txt");
    fs::write(&file, "first\nrestored\n").unwrap();
    state::save_session(
        &workspace.session_path(),
        &SessionState {
            version: state::STATE_VERSION,
            project_root: workspace.project.clone(),
            files: vec![file.clone()],
            panes: vec![state::SessionPane {
                file: file.clone(),
                cursor_line: 1,
                cursor_grapheme: 3,
                viewport_line: 0,
                viewport_column: 0,
            }],
            explorer_width: 30,
            ..SessionState::default()
        },
    )
    .unwrap();
    let mut terminal = workspace.terminal(&[]);
    terminal.wait_for("restore.txt");
    terminal.wait_for("restored");
    terminal.send(b"i!");
    terminal.wait_for("res!tored");
    terminal.normal_mode();
    terminal.ex("wq");
    terminal.finish();
    assert_eq!(fs::read_to_string(file).unwrap(), "first\nres!tored\n");
}

#[test]
fn config_layers_merge_in_order_and_only_trusted_projects_override_tools() {
    const CHILD_ROOT: &str = "EDITOR_TEST_CONFIG_ROOT";
    if let Some(root) = std::env::var_os(CHILD_ROOT) {
        // Environment changes are confined to this child process; parallel
        // library tests never observe our private config directory.
        let root = PathBuf::from(root);
        let user_path = editor::config::Config::user_path().unwrap();
        let user = editor::config::Config::load(None, false).unwrap();
        assert_eq!(user.editor.tab_width, 2);
        assert!(user.editor.format_on_save);
        assert_eq!(user.sources, vec![user_path.clone()]);
        for trusted in [false, true] {
            let config = editor::config::Config::load(Some(&root), trusted).unwrap();
            assert_eq!(
                config.sources,
                vec![user_path.clone(), root.join(".editor.toml")]
            );
            assert_eq!(config.editor.tab_width, 8);
            assert!(config.editor.format_on_save);
            assert_eq!(config.ui.explorer_width, 35);
            assert_eq!(config.ui.theme.accent, "#123456");
            assert!(config.ui.relative_numbers);
            assert_eq!(config.limits.undo_bytes, 512);
            if trusted {
                assert_eq!(config.tools.rust_analyzer.path, "project-analyzer");
                assert_eq!(config.tools.rust_analyzer.args, ["--project"]);
            } else {
                assert_eq!(config.tools.rust_analyzer.path, "user-analyzer");
                assert_eq!(
                    config.tools.rust_analyzer.args,
                    ["--user", "literal argument"]
                );
            }
        }
        fs::remove_file(&user_path).unwrap();
        let defaults = editor::config::Config::load(None, false).unwrap();
        assert!(defaults.sources.is_empty());
        assert_eq!(defaults.editor.tab_width, 4);
        fs::create_dir(&user_path).unwrap();
        let error = editor::config::Config::load(None, false).unwrap_err();
        assert_eq!(error.path, user_path);
        return;
    }
    let workspace = Workspace::new();
    fs::write(
        workspace.config.join("editor/config.toml"),
        r#"
[editor]
tab_width = 2
format_on_save = true
[ui]
explorer_width = 35
[limits]
undo_bytes = 512
[tools.rust_analyzer]
path = "user-analyzer"
args = ["--user", "literal argument"]
"#,
    )
    .unwrap();
    fs::write(
        workspace.project.join(".editor.toml"),
        r#"
[editor]
tab_width = 8
[ui.theme]
accent = '#123456'
[tools.rust_analyzer]
path = "project-analyzer"
args = ["--project"]
"#,
    )
    .unwrap();
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "config_layers_merge_in_order_and_only_trusted_projects_override_tools",
            "--nocapture",
        ])
        .env(CHILD_ROOT, &workspace.project)
        .env("XDG_CONFIG_HOME", &workspace.config)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn missing_parent_is_visible_and_failed_save_preserves_the_live_buffer() {
    let workspace = Workspace::new();
    let mut terminal = workspace.terminal(&["--no-session", "absent/new.txt"]);
    terminal.wait_for("Parent directory does not exist");
    terminal.send(b"ikeep me");
    terminal.wait_for("keep me");
    terminal.normal_mode();
    terminal.ex("w");
    terminal.wait_for("No such file or directory");
    assert!(terminal.child.try_wait().unwrap().is_none());
    terminal.ex("wq recovered.txt");
    terminal.finish();
    assert_eq!(
        fs::read_to_string(workspace.project.join("recovered.txt")).unwrap(),
        "keep me"
    );
    assert!(!workspace.project.join("absent").exists());
}
