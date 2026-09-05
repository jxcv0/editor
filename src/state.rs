//! Private crash-recovery journals and content-free project sessions.

use std::{
    env, fs, io,
    path::{Path, PathBuf},
    sync::mpsc::{self, SyncSender, TrySendError},
    thread,
};

use serde::{Deserialize, Serialize};

pub const STATE_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryRecord {
    pub version: u32,
    pub key: String,
    pub path: Option<PathBuf>,
    pub buffer_version: u64,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SessionState {
    pub version: u32,
    pub project_root: PathBuf,
    pub files: Vec<PathBuf>,
    pub active: usize,
    pub panes: Vec<SessionPane>,
    pub explorer_open: bool,
    pub explorer_width: u16,
    pub expanded_directories: Vec<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionPane {
    pub file: PathBuf,
    pub cursor_line: usize,
    pub cursor_grapheme: usize,
    pub viewport_line: usize,
    #[serde(default)]
    pub viewport_column: usize,
}

pub fn state_dir() -> Option<PathBuf> {
    env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(dirs::state_dir)
        .map(|path| path.join("editor"))
}

pub struct Journal {
    sender: SyncSender<JournalAction>,
    worker: Option<thread::JoinHandle<()>>,
}

enum JournalAction {
    Write(RecoveryRecord),
    Remove(String),
    Finish,
}

impl Journal {
    /// Start a bounded writer. `queue` never blocks the input path.
    pub fn start(root: PathBuf) -> io::Result<Self> {
        create_private_dir(&root)?;
        let (sender, receiver) = mpsc::sync_channel(64);
        let worker = thread::Builder::new()
            .name("editor-recovery".into())
            .spawn(move || {
                while let Ok(action) = receiver.recv() {
                    match action {
                        JournalAction::Write(record) => {
                            if let Ok(bytes) = serde_json::to_vec(&record) {
                                let _ = atomic_private_write(
                                    &root.join(format!("{}.json", safe_key(&record.key))),
                                    &bytes,
                                );
                            }
                        }
                        JournalAction::Remove(key) => {
                            let _ = fs::remove_file(root.join(format!("{}.json", safe_key(&key))));
                        }
                        JournalAction::Finish => break,
                    }
                }
            })?;
        Ok(Self {
            sender,
            worker: Some(worker),
        })
    }

    pub fn queue(&self, record: RecoveryRecord) -> bool {
        match self.sender.try_send(JournalAction::Write(record)) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => false,
        }
    }

    pub fn remove(&self, key: impl Into<String>) -> bool {
        self.sender
            .try_send(JournalAction::Remove(key.into()))
            .is_ok()
    }
}

impl Drop for Journal {
    fn drop(&mut self) {
        let _ = self.sender.send(JournalAction::Finish);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub fn list_recoverable(root: &Path) -> io::Result<Vec<RecoveryRecord>> {
    let mut records = Vec::new();
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(records),
        Err(error) => return Err(error),
    };
    for entry in entries.flatten() {
        if entry.path().extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        if let Ok(bytes) = fs::read(entry.path())
            && let Ok(record) = serde_json::from_slice::<RecoveryRecord>(&bytes)
            && record.version == STATE_VERSION
        {
            records.push(record);
        }
    }
    Ok(records)
}

pub fn save_session(path: &Path, session: &SessionState) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        create_private_dir(parent)?;
    }
    let bytes = serde_json::to_vec(session).map_err(io::Error::other)?;
    atomic_private_write(path, &bytes)
}

pub fn load_session(path: &Path) -> io::Result<Option<SessionState>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let session: SessionState = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    Ok((session.version == STATE_VERSION).then_some(session))
}

pub fn project_key(root: &Path) -> String {
    // FNV-1a is used only to create a stable, non-sensitive filename.
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in root.as_os_str().as_encoded_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

fn safe_key(key: &str) -> String {
    key.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .take(120)
        .collect()
}

fn atomic_private_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    create_private_dir(parent)?;
    let temporary = parent.join(format!(".editor-{}.tmp", std::process::id()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    {
        use io::Write;
        let mut file = options.open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    fs::rename(&temporary, path)?;
    Ok(())
}

fn create_private_dir(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_keys_are_stable_and_filename_safe() {
        assert_eq!(
            project_key(Path::new("/tmp/project")),
            project_key(Path::new("/tmp/project"))
        );
        assert!(
            project_key(Path::new("/tmp/project"))
                .chars()
                .all(|c| c.is_ascii_hexdigit())
        );
    }

    #[test]
    fn sessions_never_contain_buffer_text() {
        let session = SessionState {
            version: STATE_VERSION,
            project_root: "/work".into(),
            ..SessionState::default()
        };
        let encoded = serde_json::to_string(&session).unwrap();
        assert!(!encoded.contains("text"));
    }

    #[test]
    fn older_session_panes_default_the_horizontal_viewport() {
        let pane: SessionPane = serde_json::from_str(
            r#"{"file":"/work/main.rs","cursor_line":3,"cursor_grapheme":4,"viewport_line":2}"#,
        )
        .unwrap();
        assert_eq!(pane.viewport_column, 0);
    }

    #[test]
    fn dropping_journal_flushes_queued_recovery_records() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("recovery");
        {
            let journal = Journal::start(root.clone()).unwrap();
            assert!(journal.queue(RecoveryRecord {
                version: STATE_VERSION,
                key: "scratch".into(),
                path: None,
                buffer_version: 7,
                text: "unsaved".into(),
            }));
        }

        let records = list_recoverable(&root).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].text, "unsaved");
        assert_eq!(records[0].buffer_version, 7);
    }
}
