//! Small, reusable supervision primitives for external tools.
//!
//! Processes are always started with [`Command`] and an explicit argument vector.  There is no
//! shell expansion.  Blocking pipe I/O happens on dedicated reader threads and reaches the owner
//! through a bounded channel, so an external tool cannot stall the editor's input path or grow its
//! memory use without bound.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::fmt;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

/// Everything needed to inspect and launch an external program.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessSpec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub current_dir: Option<PathBuf>,
    /// Environment overrides only.  Callers should not expose values in logs or status output.
    pub env: Vec<(OsString, OsString)>,
    pub clear_env: bool,
}

impl ProcessSpec {
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            current_dir: None,
            env: Vec::new(),
            clear_env: false,
        }
    }

    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn current_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.current_dir = Some(path.into());
        self
    }

    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    pub fn clear_env(mut self, clear: bool) -> Self {
        self.clear_env = clear;
        self
    }

    /// Construct the direct child command.  This deliberately does not expose a shell command
    /// string, because displaying such a string tends to invite executing it through a shell.
    pub fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command.args(&self.args);
        if let Some(current_dir) = &self.current_dir {
            command.current_dir(current_dir);
        }
        if self.clear_env {
            command.env_clear();
        }
        command.envs(self.env.iter().cloned());
        command.stdin(Stdio::piped());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());

        // A separate process group lets shutdown include grandchildren without involving a shell.
        #[cfg(unix)]
        command.process_group(0);

        command
    }
}

/// Hard bounds applied to one supervised child.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessLimits {
    /// Maximum bytes in any stdout/stderr event delivered to the owner.
    pub output_chunk_bytes: usize,
    /// Number of pipe events that may wait for the owner.
    pub event_queue_capacity: usize,
    /// Largest single write accepted for stdin.
    pub stdin_write_bytes: usize,
    /// Time given to a child after graceful termination before it is killed.
    pub shutdown_timeout: Duration,
}

impl Default for ProcessLimits {
    fn default() -> Self {
        Self {
            output_chunk_bytes: 8 * 1024,
            event_queue_capacity: 256,
            stdin_write_bytes: 8 * 1024 * 1024 + 1024,
            shutdown_timeout: Duration::from_millis(750),
        }
    }
}

impl ProcessLimits {
    fn normalized(&self) -> Self {
        Self {
            output_chunk_bytes: self.output_chunk_bytes.max(1),
            event_queue_capacity: self.event_queue_capacity.max(1),
            stdin_write_bytes: self.stdin_write_bytes.max(1),
            shutdown_timeout: self.shutdown_timeout,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputStream {
    Stdout,
    Stderr,
}

#[derive(Debug)]
pub enum ProcessEvent {
    Output {
        stream: OutputStream,
        bytes: Vec<u8>,
    },
    Eof(OutputStream),
    ReadError {
        stream: OutputStream,
        error: io::Error,
    },
    /// One or more events could not fit in the bounded queue.  Protocol consumers should treat
    /// dropped stdout as fatal because it destroys framing.
    OutputDropped {
        stream: OutputStream,
        events: usize,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessExit {
    pub code: Option<i32>,
    pub success: bool,
}

impl From<ExitStatus> for ProcessExit {
    fn from(status: ExitStatus) -> Self {
        Self {
            code: status.code(),
            success: status.success(),
        }
    }
}

/// An owned child whose pipe readers can never block its owner.
pub struct SupervisedChild {
    spec: ProcessSpec,
    limits: ProcessLimits,
    child: Child,
    stdin: Option<ChildStdin>,
    events: Receiver<ProcessEvent>,
    dropped_stdout: Arc<AtomicUsize>,
    dropped_stderr: Arc<AtomicUsize>,
    readers: Vec<JoinHandle<()>>,
    exit: Option<ProcessExit>,
}

impl fmt::Debug for SupervisedChild {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SupervisedChild")
            .field("spec", &self.spec)
            .field("limits", &self.limits)
            .field("pid", &self.child.id())
            .field("exit", &self.exit)
            .finish_non_exhaustive()
    }
}

impl SupervisedChild {
    pub fn spawn(spec: ProcessSpec, limits: ProcessLimits) -> io::Result<Self> {
        let limits = limits.normalized();
        let mut child = spec.command().spawn()?;
        let Some(stdin) = child.stdin.take() else {
            let _ = child.kill();
            return Err(io::Error::other("child stdin was not piped"));
        };
        let Some(stdout) = child.stdout.take() else {
            let _ = child.kill();
            return Err(io::Error::other("child stdout was not piped"));
        };
        let Some(stderr) = child.stderr.take() else {
            let _ = child.kill();
            return Err(io::Error::other("child stderr was not piped"));
        };

        let (event_tx, events) = mpsc::sync_channel(limits.event_queue_capacity);
        let dropped_stdout = Arc::new(AtomicUsize::new(0));
        let dropped_stderr = Arc::new(AtomicUsize::new(0));

        let stdout_reader = spawn_pipe_reader(
            "editor-child-stdout",
            stdout,
            OutputStream::Stdout,
            limits.output_chunk_bytes,
            event_tx.clone(),
            Arc::clone(&dropped_stdout),
        );
        let stdout_reader = match stdout_reader {
            Ok(reader) => reader,
            Err(error) => {
                kill_immediately(&mut child);
                return Err(error);
            }
        };

        let stderr_reader = spawn_pipe_reader(
            "editor-child-stderr",
            stderr,
            OutputStream::Stderr,
            limits.output_chunk_bytes,
            event_tx,
            Arc::clone(&dropped_stderr),
        );
        let stderr_reader = match stderr_reader {
            Ok(reader) => reader,
            Err(error) => {
                kill_immediately(&mut child);
                if stdout_reader.is_finished() {
                    let _ = stdout_reader.join();
                }
                return Err(error);
            }
        };

        Ok(Self {
            spec,
            limits,
            child,
            stdin: Some(stdin),
            events,
            dropped_stdout,
            dropped_stderr,
            readers: vec![stdout_reader, stderr_reader],
            exit: None,
        })
    }

    pub fn spec(&self) -> &ProcessSpec {
        &self.spec
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Write one bounded protocol message.  Call this only from an integration worker, never from
    /// the editor input loop.
    pub fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        if bytes.len() > self.limits.stdin_write_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "stdin write is {} bytes; limit is {} bytes",
                    bytes.len(),
                    self.limits.stdin_write_bytes
                ),
            ));
        }
        let stdin = self.stdin.as_mut().ok_or_else(|| {
            io::Error::new(io::ErrorKind::BrokenPipe, "child stdin is already closed")
        })?;
        stdin.write_all(bytes)?;
        stdin.flush()
    }

    pub fn close_stdin(&mut self) {
        self.stdin.take();
    }

    /// Poll one event without waiting.  Drop notifications are synthesized ahead of queued data.
    pub fn try_recv(&self) -> Result<ProcessEvent, TryRecvError> {
        let stdout = self.dropped_stdout.swap(0, Ordering::AcqRel);
        if stdout != 0 {
            return Ok(ProcessEvent::OutputDropped {
                stream: OutputStream::Stdout,
                events: stdout,
            });
        }
        let stderr = self.dropped_stderr.swap(0, Ordering::AcqRel);
        if stderr != 0 {
            return Ok(ProcessEvent::OutputDropped {
                stream: OutputStream::Stderr,
                events: stderr,
            });
        }
        self.events.try_recv()
    }

    pub fn drain_events(&self, limit: usize) -> Vec<ProcessEvent> {
        let mut events = Vec::with_capacity(limit.min(32));
        for _ in 0..limit {
            match self.try_recv() {
                Ok(event) => events.push(event),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
        events
    }

    pub fn try_wait(&mut self) -> io::Result<Option<ProcessExit>> {
        if let Some(exit) = &self.exit {
            return Ok(Some(exit.clone()));
        }
        let Some(status) = self.child.try_wait()? else {
            return Ok(None);
        };
        let exit = ProcessExit::from(status);
        self.exit = Some(exit.clone());
        Ok(Some(exit))
    }

    /// Give the process group a bounded grace period, then forcefully terminate it.
    ///
    /// This method can wait for `grace`, so integrations invoke it on their worker threads.
    pub fn terminate(&mut self, grace: Duration) -> io::Result<ProcessExit> {
        if let Some(exit) = self.try_wait()? {
            self.reap_finished_readers();
            return Ok(exit);
        }

        self.close_stdin();
        signal_process_group(self.child.id(), TerminationSignal::Terminate);
        let deadline = Instant::now() + grace;
        loop {
            if let Some(exit) = self.try_wait()? {
                self.reap_finished_readers();
                return Ok(exit);
            }
            if Instant::now() >= deadline {
                break;
            }
            thread::sleep(
                Duration::from_millis(10).min(deadline.saturating_duration_since(Instant::now())),
            );
        }

        signal_process_group(self.child.id(), TerminationSignal::Kill);
        // The process-group signal is best effort (and unavailable on non-Unix platforms).
        let _ = self.child.kill();
        let status = self.child.wait()?;
        let exit = ProcessExit::from(status);
        self.exit = Some(exit.clone());
        self.reap_finished_readers();
        Ok(exit)
    }

    pub fn terminate_with_configured_timeout(&mut self) -> io::Result<ProcessExit> {
        self.terminate(self.limits.shutdown_timeout)
    }

    fn reap_finished_readers(&mut self) {
        for reader in self.readers.drain(..) {
            // A descendant that deliberately keeps an inherited pipe open must not turn shutdown
            // into an unbounded join. Dropping an unfinished handle detaches that bounded reader.
            if reader.is_finished() {
                let _ = reader.join();
            }
        }
    }
}

impl Drop for SupervisedChild {
    fn drop(&mut self) {
        if self.exit.is_none() {
            self.close_stdin();
            signal_process_group(self.child.id(), TerminationSignal::Kill);
            let _ = self.child.kill();
            if let Ok(status) = self.child.wait() {
                self.exit = Some(status.into());
            }
        }
        self.reap_finished_readers();
    }
}

fn spawn_pipe_reader<R>(
    name: &str,
    mut reader: R,
    stream: OutputStream,
    chunk_bytes: usize,
    sender: SyncSender<ProcessEvent>,
    dropped: Arc<AtomicUsize>,
) -> io::Result<JoinHandle<()>>
where
    R: Read + Send + 'static,
{
    thread::Builder::new().name(name.to_owned()).spawn(move || {
        let mut buffer = vec![0; chunk_bytes.max(1)];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => {
                    send_pipe_event(&sender, ProcessEvent::Eof(stream), &dropped);
                    break;
                }
                Ok(read) => send_pipe_event(
                    &sender,
                    ProcessEvent::Output {
                        stream,
                        bytes: buffer[..read].to_vec(),
                    },
                    &dropped,
                ),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    send_pipe_event(&sender, ProcessEvent::ReadError { stream, error }, &dropped);
                    break;
                }
            }
        }
    })
}

fn send_pipe_event(sender: &SyncSender<ProcessEvent>, event: ProcessEvent, dropped: &AtomicUsize) {
    match sender.try_send(event) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) => {
            dropped.fetch_add(1, Ordering::Relaxed);
        }
        Err(TrySendError::Disconnected(_)) => {}
    }
}

fn kill_immediately(child: &mut Child) {
    signal_process_group(child.id(), TerminationSignal::Kill);
    let _ = child.kill();
    let _ = child.wait();
}

#[derive(Clone, Copy)]
enum TerminationSignal {
    Terminate,
    Kill,
}

#[cfg(unix)]
fn signal_process_group(pid: u32, signal: TerminationSignal) {
    const SIGTERM: i32 = 15;
    const SIGKILL: i32 = 9;
    let Ok(pid) = i32::try_from(pid) else {
        return;
    };
    let signal = match signal {
        TerminationSignal::Terminate => SIGTERM,
        TerminationSignal::Kill => SIGKILL,
    };

    unsafe extern "C" {
        fn kill(pid: i32, signal: i32) -> i32;
    }

    // Negative PID targets the process group created in `ProcessSpec::command`.
    unsafe {
        let _ = kill(-pid, signal);
    }
}

#[cfg(not(unix))]
fn signal_process_group(_pid: u32, _signal: TerminationSignal) {}

/// A byte log that retains only its newest `capacity` bytes.
#[derive(Clone, Debug)]
pub struct BoundedLog {
    capacity: usize,
    bytes: VecDeque<u8>,
    truncated_bytes: u64,
}

impl BoundedLog {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            bytes: VecDeque::with_capacity(capacity.min(64 * 1024)),
            truncated_bytes: 0,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) {
        if self.capacity == 0 {
            self.truncated_bytes = self
                .truncated_bytes
                .saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
            return;
        }

        if bytes.len() >= self.capacity {
            self.truncated_bytes = self.truncated_bytes.saturating_add(
                u64::try_from(self.bytes.len() + bytes.len() - self.capacity).unwrap_or(u64::MAX),
            );
            self.bytes.clear();
            self.bytes
                .extend(bytes[bytes.len() - self.capacity..].iter().copied());
            return;
        }

        let overflow = self
            .bytes
            .len()
            .saturating_add(bytes.len())
            .saturating_sub(self.capacity);
        if overflow != 0 {
            self.bytes.drain(..overflow);
            self.truncated_bytes = self
                .truncated_bytes
                .saturating_add(u64::try_from(overflow).unwrap_or(u64::MAX));
        }
        self.bytes.extend(bytes.iter().copied());
    }

    pub fn clear(&mut self) {
        self.bytes.clear();
        self.truncated_bytes = 0;
    }

    pub fn bytes(&self) -> Vec<u8> {
        self.bytes.iter().copied().collect()
    }

    pub fn to_string_lossy(&self) -> String {
        String::from_utf8_lossy(&self.bytes()).into_owned()
    }

    pub fn truncated_bytes(&self) -> u64 {
        self.truncated_bytes
    }
}

/// Incremental newline framing with a strict per-line bound.
#[derive(Clone, Debug)]
pub struct BoundedLineDecoder {
    pending: Vec<u8>,
    max_line_bytes: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LineDecodeError {
    LineTooLong { limit: usize },
}

impl fmt::Display for LineDecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LineTooLong { limit } => {
                write!(formatter, "protocol line exceeds the {limit}-byte limit")
            }
        }
    }
}

impl std::error::Error for LineDecodeError {}

impl BoundedLineDecoder {
    pub fn new(max_line_bytes: usize) -> Self {
        Self {
            pending: Vec::new(),
            max_line_bytes: max_line_bytes.max(1),
        }
    }

    /// Append bytes and return all complete lines.  The newline and an optional preceding carriage
    /// return are omitted.
    pub fn push(&mut self, mut bytes: &[u8]) -> Result<Vec<Vec<u8>>, LineDecodeError> {
        let mut lines = Vec::new();
        while let Some(newline) = bytes.iter().position(|byte| *byte == b'\n') {
            let (part, remainder) = bytes.split_at(newline);
            if self.pending.len().saturating_add(part.len()) > self.max_line_bytes {
                self.pending.clear();
                return Err(LineDecodeError::LineTooLong {
                    limit: self.max_line_bytes,
                });
            }
            self.pending.extend_from_slice(part);
            if self.pending.last() == Some(&b'\r') {
                self.pending.pop();
            }
            lines.push(std::mem::take(&mut self.pending));
            bytes = &remainder[1..];
        }

        if self.pending.len().saturating_add(bytes.len()) > self.max_line_bytes {
            self.pending.clear();
            return Err(LineDecodeError::LineTooLong {
                limit: self.max_line_bytes,
            });
        }
        self.pending.extend_from_slice(bytes);
        Ok(lines)
    }

    /// Return the final unterminated line, if any.
    pub fn finish(&mut self) -> Option<Vec<u8>> {
        if self.pending.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut self.pending))
        }
    }

    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }
}

#[cfg(test)]
mod tests {
    use super::{BoundedLineDecoder, BoundedLog, LineDecodeError};

    #[test]
    fn line_decoder_handles_fragmented_crlf_and_multiple_lines() {
        let mut decoder = BoundedLineDecoder::new(32);
        assert!(decoder.push(b"one\r").unwrap().is_empty());
        assert_eq!(
            decoder.push(b"\ntwo\nthree").unwrap(),
            vec![b"one".to_vec(), b"two".to_vec()]
        );
        assert_eq!(decoder.finish(), Some(b"three".to_vec()));
    }

    #[test]
    fn line_decoder_rejects_an_unterminated_oversized_line() {
        let mut decoder = BoundedLineDecoder::new(4);
        assert!(decoder.push(b"1234").unwrap().is_empty());
        assert_eq!(
            decoder.push(b"5"),
            Err(LineDecodeError::LineTooLong { limit: 4 })
        );
        assert_eq!(decoder.pending_len(), 0);
    }

    #[test]
    fn bounded_log_retains_the_newest_bytes() {
        let mut log = BoundedLog::new(5);
        log.push(b"abc");
        log.push(b"defg");
        assert_eq!(log.bytes(), b"cdefg");
        assert_eq!(log.truncated_bytes(), 2);
    }
}
