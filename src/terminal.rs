//! Pseudo-terminal process supervision and terminal-emulator state.

use std::{
    collections::VecDeque,
    env,
    ffi::OsString,
    io::{self, Read, Write},
    path::Path,
    sync::mpsc::{self, Receiver},
    thread,
};

use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::input::{Key, KeyCode, Modifiers};

pub const DEFAULT_ROWS: u16 = 12;
pub const DEFAULT_COLUMNS: u16 = 80;
const SCROLLBACK_ROWS: usize = 10_000;
const OUTPUT_QUEUE_CHUNKS: usize = 64;
const MAX_PENDING_RESPONSES: usize = 64;
const DEFAULT_BACKGROUND: (u8, u8, u8) = (0x10, 0x16, 0x19);
const DCS_INDN_QUERY: &[u8] = b"\x1bP+q696e646e\x1b\\";
const DCS_OS_QUERY: &[u8] = b"\x1bP+q71756572792d6f732d6e616d65\x1b\\";
const DCS_TAIL_BYTES: usize = DCS_OS_QUERY.len() - 1;

#[derive(Debug)]
struct TerminalCallbacks {
    responses: VecDeque<Vec<u8>>,
    background: (u8, u8, u8),
}

impl TerminalCallbacks {
    fn new(background: (u8, u8, u8)) -> Self {
        Self {
            responses: VecDeque::new(),
            background,
        }
    }

    fn queue(&mut self, response: Vec<u8>) {
        if self.responses.len() < MAX_PENDING_RESPONSES {
            self.responses.push_back(response);
        }
    }
}

impl vt100::Callbacks for TerminalCallbacks {
    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        first_intermediate: Option<u8>,
        second_intermediate: Option<u8>,
        params: &[&[u16]],
        final_char: char,
    ) {
        let parameter = params
            .first()
            .and_then(|values| values.first())
            .copied()
            .unwrap_or(0);
        match (
            first_intermediate,
            second_intermediate,
            parameter,
            final_char,
        ) {
            // Primary Device Attributes: identify as a VT100 with advanced
            // video support. Shells use the response to confirm that they are
            // attached to a functioning terminal emulator.
            (None, None, 0, 'c') => self.queue(b"\x1b[?1;2c".to_vec()),
            // Device Status Report: cursor coordinates are one-based on the
            // wire and zero-based in vt100's screen model.
            (None, None, 6, 'n') => {
                let (row, column) = screen.cursor_position();
                self.queue(
                    format!(
                        "\x1b[{};{}R",
                        row.saturating_add(1),
                        column.saturating_add(1)
                    )
                    .into_bytes(),
                );
            }
            // XTVERSION query.
            (Some(b'>'), None, 0, 'q') => self.queue(
                concat!("\x1bP>|editor(", env!("CARGO_PKG_VERSION"), ")\x1b\\")
                    .as_bytes()
                    .to_vec(),
            ),
            _ => {}
        }
    }

    fn unhandled_osc(&mut self, _: &mut vt100::Screen, params: &[&[u8]]) {
        if params.len() == 2 && params[0] == b"11" && params[1] == b"?" {
            let (red, green, blue) = self.background;
            self.queue(
                format!(
                    "\x1b]11;rgb:{:04x}/{:04x}/{:04x}\x1b\\",
                    u16::from(red) * 0x101,
                    u16::from(green) * 0x101,
                    u16::from(blue) * 0x101,
                )
                .into_bytes(),
            );
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalStatus {
    Stopped,
    Running,
    Exited(String),
    Failed(String),
}

impl TerminalStatus {
    pub fn label(&self) -> String {
        match self {
            Self::Stopped => "stopped".into(),
            Self::Running => "running".into(),
            Self::Exited(status) => format!("exited: {status}"),
            Self::Failed(error) => format!("failed: {error}"),
        }
    }
}

/// Terminal state kept in the editor model so rendering stays deterministic
/// and does not need access to operating-system process handles.
pub struct TerminalPanel {
    pub visible: bool,
    pub status: TerminalStatus,
    parser: vt100::Parser<TerminalCallbacks>,
    dcs_tail: Vec<u8>,
}

impl TerminalPanel {
    pub fn new() -> Self {
        Self::with_background(DEFAULT_BACKGROUND)
    }

    pub fn with_background(background: (u8, u8, u8)) -> Self {
        Self {
            visible: false,
            status: TerminalStatus::Stopped,
            parser: terminal_parser(DEFAULT_ROWS, DEFAULT_COLUMNS, background),
            dcs_tail: Vec::new(),
        }
    }

    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    pub fn process(&mut self, bytes: &[u8]) {
        let replies = self.scan_dcs_queries(bytes);
        let mut processed = 0;
        for (end, reply) in replies {
            if end > processed {
                self.parser.process(&bytes[processed..end]);
                processed = end;
            }
            self.parser.callbacks_mut().queue(reply);
        }
        self.parser.process(&bytes[processed..]);
    }

    pub fn take_responses(&mut self) -> Vec<Vec<u8>> {
        self.parser.callbacks_mut().responses.drain(..).collect()
    }

    pub fn set_background(&mut self, background: (u8, u8, u8)) {
        self.parser.callbacks_mut().background = background;
    }

    pub fn reset(&mut self) {
        let (rows, columns) = self.parser.screen().size();
        let background = self.parser.callbacks().background;
        self.parser = terminal_parser(rows, columns, background);
        self.dcs_tail.clear();
    }

    pub fn resize(&mut self, rows: u16, columns: u16) {
        let rows = rows.max(1);
        let columns = columns.max(1);
        if self.parser.screen().size() != (rows, columns) {
            self.parser.screen_mut().set_size(rows, columns);
        }
    }

    pub fn scroll_up(&mut self) {
        let page = usize::from(self.parser.screen().size().0.max(1));
        let position = self.parser.screen().scrollback().saturating_add(page);
        self.parser.screen_mut().set_scrollback(position);
    }

    pub fn scroll_down(&mut self) {
        let page = usize::from(self.parser.screen().size().0.max(1));
        let position = self.parser.screen().scrollback().saturating_sub(page);
        self.parser.screen_mut().set_scrollback(position);
    }

    pub fn scroll_to_top(&mut self) {
        self.parser.screen_mut().set_scrollback(usize::MAX);
    }

    pub fn scroll_to_bottom(&mut self) {
        self.parser.screen_mut().set_scrollback(0);
    }

    pub fn encode_key(&mut self, key: Key) -> Option<Vec<u8>> {
        self.scroll_to_bottom();
        terminal_key_bytes(key, self.parser.screen().application_cursor())
    }

    pub fn encode_paste(&mut self, text: &str) -> Vec<u8> {
        self.scroll_to_bottom();
        if self.parser.screen().bracketed_paste() {
            let mut bytes = Vec::with_capacity(text.len().saturating_add(12));
            bytes.extend_from_slice(b"\x1b[200~");
            bytes.extend_from_slice(text.as_bytes());
            bytes.extend_from_slice(b"\x1b[201~");
            bytes
        } else {
            text.as_bytes().to_vec()
        }
    }

    fn scan_dcs_queries(&mut self, bytes: &[u8]) -> Vec<(usize, Vec<u8>)> {
        let previous_length = self.dcs_tail.len();
        let mut combined = Vec::with_capacity(previous_length.saturating_add(bytes.len()));
        combined.extend_from_slice(&self.dcs_tail);
        combined.extend_from_slice(bytes);

        let mut replies = Vec::new();
        collect_new_matches(
            &combined,
            previous_length,
            DCS_INDN_QUERY,
            b"\x1bP1+r696e646e\x1b\\".to_vec(),
            &mut replies,
        );
        collect_new_matches(
            &combined,
            previous_length,
            DCS_OS_QUERY,
            xtgettcap_os_response(),
            &mut replies,
        );
        replies.sort_by_key(|(position, _)| *position);
        replies.truncate(MAX_PENDING_RESPONSES);

        let keep_from = combined.len().saturating_sub(DCS_TAIL_BYTES);
        self.dcs_tail.clear();
        self.dcs_tail.extend_from_slice(&combined[keep_from..]);
        replies
            .into_iter()
            .map(|(end, reply)| (end.saturating_sub(previous_length).min(bytes.len()), reply))
            .collect()
    }
}

impl Default for TerminalPanel {
    fn default() -> Self {
        Self::new()
    }
}

fn terminal_parser(
    rows: u16,
    columns: u16,
    background: (u8, u8, u8),
) -> vt100::Parser<TerminalCallbacks> {
    vt100::Parser::new_with_callbacks(
        rows,
        columns,
        SCROLLBACK_ROWS,
        TerminalCallbacks::new(background),
    )
}

fn collect_new_matches(
    bytes: &[u8],
    previous_length: usize,
    needle: &[u8],
    reply: Vec<u8>,
    matches: &mut Vec<(usize, Vec<u8>)>,
) {
    let initial_length = matches.len();
    for (position, window) in bytes.windows(needle.len()).enumerate() {
        let end = position.saturating_add(needle.len());
        if window == needle && end > previous_length {
            matches.push((end, reply.clone()));
            if matches.len().saturating_sub(initial_length) >= MAX_PENDING_RESPONSES {
                break;
            }
        }
    }
}

fn xtgettcap_os_response() -> Vec<u8> {
    let os_name = match env::consts::OS {
        "linux" => "Linux",
        "macos" => "Darwin",
        "windows" => "Windows_NT",
        "freebsd" => "FreeBSD",
        "openbsd" => "OpenBSD",
        "netbsd" => "NetBSD",
        "dragonfly" => "DragonFly",
        other => other,
    };
    let mut response = b"\x1bP1+r71756572792d6f732d6e616d65=".to_vec();
    for byte in os_name.bytes() {
        response.extend_from_slice(format!("{byte:02x}").as_bytes());
    }
    response.extend_from_slice(b"\x1b\\");
    response
}

fn terminal_key_bytes(key: Key, application_cursor: bool) -> Option<Vec<u8>> {
    if let KeyCode::Char(character) = key.code {
        let mut bytes = if key.modifiers.contains(Modifiers::CONTROL) {
            vec![control_byte(character)?]
        } else {
            let mut encoded = [0; 4];
            character.encode_utf8(&mut encoded).as_bytes().to_vec()
        };
        if key.modifiers.contains(Modifiers::ALT) {
            bytes.insert(0, b'\x1b');
        }
        return Some(bytes);
    }

    let modifier = 1
        + usize::from(key.modifiers.contains(Modifiers::SHIFT))
        + usize::from(key.modifiers.contains(Modifiers::ALT)) * 2
        + usize::from(key.modifiers.contains(Modifiers::CONTROL)) * 4;
    let modified_csi = |final_byte: char| format!("\x1b[1;{modifier}{final_byte}").into_bytes();
    let tilde = |number: u8| {
        if modifier == 1 {
            format!("\x1b[{number}~").into_bytes()
        } else {
            format!("\x1b[{number};{modifier}~").into_bytes()
        }
    };
    let cursor = |normal: &'static [u8], application: &'static [u8], final_byte| {
        if modifier != 1 {
            modified_csi(final_byte)
        } else if application_cursor {
            application.to_vec()
        } else {
            normal.to_vec()
        }
    };

    Some(match key.code {
        KeyCode::Enter => with_alt_prefix(vec![b'\r'], key),
        KeyCode::Esc => vec![b'\x1b'],
        KeyCode::Backspace => with_alt_prefix(vec![b'\x7f'], key),
        KeyCode::Delete => tilde(3),
        KeyCode::Tab => with_alt_prefix(vec![b'\t'], key),
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Left => cursor(b"\x1b[D", b"\x1bOD", 'D'),
        KeyCode::Right => cursor(b"\x1b[C", b"\x1bOC", 'C'),
        KeyCode::Up => cursor(b"\x1b[A", b"\x1bOA", 'A'),
        KeyCode::Down => cursor(b"\x1b[B", b"\x1bOB", 'B'),
        KeyCode::Home => cursor(b"\x1b[H", b"\x1bOH", 'H'),
        KeyCode::End => cursor(b"\x1b[F", b"\x1bOF", 'F'),
        KeyCode::PageUp => tilde(5),
        KeyCode::PageDown => tilde(6),
        KeyCode::Char(_) | KeyCode::Unknown => return None,
    })
}

fn control_byte(character: char) -> Option<u8> {
    let character = character.to_ascii_uppercase();
    match character {
        ' ' | '@' => Some(0),
        'A'..='Z' => Some(character as u8 - b'A' + 1),
        '[' => Some(27),
        '\\' => Some(28),
        ']' => Some(29),
        '^' => Some(30),
        '_' => Some(31),
        '?' => Some(127),
        _ => None,
    }
}

fn with_alt_prefix(mut bytes: Vec<u8>, key: Key) -> Vec<u8> {
    if key.modifiers.contains(Modifiers::ALT) {
        bytes.insert(0, b'\x1b');
    }
    bytes
}

#[derive(Debug)]
pub enum TerminalOutput {
    Data(Vec<u8>),
    Error(String),
    Eof,
}

/// The operating-system half of the integrated terminal. Output is read on a
/// bounded worker so a noisy child cannot grow editor memory without limit.
pub struct TerminalProcess {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    output: Receiver<TerminalOutput>,
    size: (u16, u16),
}

impl TerminalProcess {
    pub fn spawn(cwd: &Path, rows: u16, columns: u16) -> io::Result<Self> {
        let rows = rows.max(1);
        let columns = columns.max(1);
        let shell = env::var_os("SHELL")
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| OsString::from("/bin/sh"));
        let mut command = CommandBuilder::new(shell);
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        command.env("TERM_PROGRAM", "editor");

        Self::spawn_command(cwd, rows, columns, command)
    }

    fn spawn_command(
        cwd: &Path,
        rows: u16,
        columns: u16,
        mut command: CommandBuilder,
    ) -> io::Result<Self> {
        command.cwd(cwd);
        let pty = native_pty_system();
        let pair = pty
            .openpty(PtySize {
                rows,
                cols: columns,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(pty_error)?;

        let mut reader = pair.master.try_clone_reader().map_err(pty_error)?;
        let writer = pair.master.take_writer().map_err(pty_error)?;
        let mut child = pair.slave.spawn_command(command).map_err(pty_error)?;
        drop(pair.slave);

        let (sender, output) = mpsc::sync_channel(OUTPUT_QUEUE_CHUNKS);
        if let Err(error) = thread::Builder::new()
            .name("editor-terminal-read".into())
            .spawn(move || {
                let mut buffer = [0_u8; 8192];
                loop {
                    match reader.read(&mut buffer) {
                        Ok(0) => {
                            let _ = sender.send(TerminalOutput::Eof);
                            break;
                        }
                        Ok(length) => {
                            if sender
                                .send(TerminalOutput::Data(buffer[..length].to_vec()))
                                .is_err()
                            {
                                break;
                            }
                        }
                        Err(error) => {
                            let _ = sender.send(TerminalOutput::Error(error.to_string()));
                            break;
                        }
                    }
                }
            })
        {
            let _ = child.kill();
            return Err(error);
        }

        Ok(Self {
            master: pair.master,
            writer,
            child,
            output,
            size: (rows, columns),
        })
    }

    pub fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.writer.write_all(bytes)?;
        self.writer.flush()
    }

    pub fn resize(&mut self, rows: u16, columns: u16) -> io::Result<()> {
        let size = (rows.max(1), columns.max(1));
        if size == self.size {
            return Ok(());
        }
        self.master
            .resize(PtySize {
                rows: size.0,
                cols: size.1,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(pty_error)?;
        self.size = size;
        Ok(())
    }

    pub fn drain_output(&self, limit: usize) -> Vec<TerminalOutput> {
        self.output.try_iter().take(limit).collect()
    }

    pub fn try_wait(&mut self) -> io::Result<Option<String>> {
        self.child
            .try_wait()
            .map(|status| status.map(|status| status.to_string()))
            .map_err(pty_error)
    }
}

impl Drop for TerminalProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.try_wait();
    }
}

fn pty_error(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_panel_parses_output_and_resizes() {
        let mut panel = TerminalPanel::new();
        panel.resize(4, 20);
        panel.process(b"hello\x1b[31m red");

        assert_eq!(panel.screen().size(), (4, 20));
        assert!(panel.screen().contents().contains("hello red"));
        assert_eq!(
            panel.screen().cell(0, 6).unwrap().fgcolor(),
            vt100::Color::Idx(1)
        );
    }

    #[test]
    fn terminal_keys_use_control_and_application_sequences() {
        assert_eq!(terminal_key_bytes(Key::ctrl('c'), false), Some(vec![3]));
        assert_eq!(
            terminal_key_bytes(Key::plain(KeyCode::Up), false),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            terminal_key_bytes(Key::plain(KeyCode::Up), true),
            Some(b"\x1bOA".to_vec())
        );
        assert_eq!(
            terminal_key_bytes(
                Key {
                    code: KeyCode::Right,
                    modifiers: Modifiers::CONTROL,
                },
                false,
            ),
            Some(b"\x1b[1;5C".to_vec())
        );
    }

    #[test]
    fn terminal_paste_honors_bracketed_paste_mode() {
        let mut panel = TerminalPanel::new();
        assert_eq!(panel.encode_paste("a\nb"), b"a\nb");
        panel.process(b"\x1b[?2004h");
        assert_eq!(panel.encode_paste("a\nb"), b"\x1b[200~a\nb\x1b[201~");
    }

    #[test]
    fn terminal_replies_to_shell_capability_queries_across_chunks() {
        let mut panel = TerminalPanel::with_background((0x12, 0x34, 0x56));
        let queries = b"\x1b[?u\x1b[>0q\x1b]11;?\x1b\\\x1bP+q696e646e\x1b\\\x1bP+q71756572792d6f732d6e616d65\x1b\\\x1b[0c";
        for chunk in queries.chunks(3) {
            panel.process(chunk);
        }

        assert_eq!(
            panel.take_responses(),
            vec![
                concat!("\x1bP>|editor(", env!("CARGO_PKG_VERSION"), ")\x1b\\")
                    .as_bytes()
                    .to_vec(),
                b"\x1b]11;rgb:1212/3434/5656\x1b\\".to_vec(),
                b"\x1bP1+r696e646e\x1b\\".to_vec(),
                xtgettcap_os_response(),
                b"\x1b[?1;2c".to_vec(),
            ]
        );
    }

    #[test]
    fn terminal_reports_one_based_cursor_position() {
        let mut panel = TerminalPanel::new();
        panel.process(b"\x1b[3;5H\x1b[6n");

        assert_eq!(panel.take_responses(), vec![b"\x1b[3;5R".to_vec()]);
    }

    #[test]
    fn terminal_capability_response_queue_is_bounded() {
        let mut panel = TerminalPanel::new();
        panel.process(&b"\x1b[0c".repeat(MAX_PENDING_RESPONSES + 20));

        assert_eq!(panel.take_responses().len(), MAX_PENDING_RESPONSES);
    }

    #[cfg(unix)]
    #[test]
    fn terminal_process_runs_in_the_selected_working_directory() {
        use std::time::{Duration, Instant};

        let mut command = CommandBuilder::new("/bin/sh");
        command.arg("-c");
        command.arg("printf 'terminal-probe:%s\\n' \"$PWD\"");
        let mut process =
            TerminalProcess::spawn_command(Path::new("/tmp"), 4, 40, command).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut output = Vec::new();
        let mut exited = false;
        let mut eof = false;
        while Instant::now() < deadline && !(exited && eof) {
            for event in process.drain_output(64) {
                match event {
                    TerminalOutput::Data(bytes) => output.extend(bytes),
                    TerminalOutput::Eof => eof = true,
                    TerminalOutput::Error(error) => panic!("terminal reader failed: {error}"),
                }
            }
            exited = process.try_wait().unwrap().is_some();
            if !(exited && eof) {
                thread::sleep(Duration::from_millis(10));
            }
        }

        assert!(exited, "terminal child did not exit");
        assert!(eof, "terminal output did not close");
        assert!(
            String::from_utf8_lossy(&output).contains("terminal-probe:/tmp"),
            "unexpected terminal output: {}",
            String::from_utf8_lossy(&output)
        );
    }
}
