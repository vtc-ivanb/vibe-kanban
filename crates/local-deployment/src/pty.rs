use std::{
    collections::HashMap,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use portable_pty::{Child, CommandBuilder, NativePtySystem, PtySize, PtySystem};
use thiserror::Error;
use tokio::sync::mpsc;
use utils::shell::get_interactive_shell;
use uuid::Uuid;

/// How long to wait for a killed shell to actually disappear.
///
/// Neither platform kills synchronously — Windows `TerminateProcess` only requests
/// termination, and on Unix portable-pty sends `SIGHUP`, which a shell may ignore.
const SHELL_EXIT_TIMEOUT: Duration = Duration::from_secs(5);

/// How often to re-check whether a killed shell has exited.
const SHELL_EXIT_POLL_INTERVAL: Duration = Duration::from_millis(20);

#[derive(Debug, Error)]
pub enum PtyError {
    #[error("Failed to create PTY: {0}")]
    CreateFailed(String),
    #[error("Session not found: {0}")]
    SessionNotFound(Uuid),
    #[error("Failed to write to PTY: {0}")]
    WriteFailed(String),
    #[error("Failed to resize PTY: {0}")]
    ResizeFailed(String),
    #[error("Session already closed")]
    SessionClosed,
}

struct PtySession {
    writer: Box<dyn Write + Send>,
    master: Box<dyn portable_pty::MasterPty + Send>,
    /// Owned here rather than by the reader thread so closing a session can kill the
    /// shell *and* wait for it to actually go away.
    child: Box<dyn Child + Send + Sync>,
    /// Directory the shell was started in, so sessions can be closed by location.
    working_dir: PathBuf,
    _output_handle: thread::JoinHandle<()>,
    closed: bool,
}

#[derive(Clone)]
pub struct PtyService {
    sessions: Arc<Mutex<HashMap<Uuid, PtySession>>>,
}

impl PtyService {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn create_session(
        &self,
        working_dir: PathBuf,
        cols: u16,
        rows: u16,
    ) -> Result<(Uuid, mpsc::UnboundedReceiver<Vec<u8>>), PtyError> {
        let session_id = Uuid::new_v4();
        let (output_tx, output_rx) = mpsc::unbounded_channel();
        let shell = get_interactive_shell().await;
        let session_dir = working_dir.clone();

        let result = tokio::task::spawn_blocking(move || {
            let pty_system = NativePtySystem::default();

            let pty_pair = pty_system
                .openpty(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .map_err(|e| PtyError::CreateFailed(e.to_string()))?;

            let mut cmd = CommandBuilder::new(&shell);
            cmd.cwd(&working_dir);

            // Configure shell-specific options
            let shell_name = shell.file_name().and_then(|n| n.to_str()).unwrap_or("");

            if shell_name == "powershell.exe" || shell_name == "pwsh.exe" {
                // PowerShell: use -NoLogo for cleaner startup
                cmd.arg("-NoLogo");
            } else if shell_name == "cmd.exe" {
                // cmd.exe: no special args needed
            } else {
                // Unix shells
                cmd.env("VIBE_KANBAN_TERMINAL", "1");

                if shell_name == "bash" {
                    cmd.env("PROMPT_COMMAND", r#"PS1='$ '; unset PROMPT_COMMAND"#);
                } else if shell_name == "zsh" {
                    // PROMPT is set after spawning
                } else {
                    cmd.env("PS1", "$ ");
                }
            }

            cmd.env("TERM", "xterm-256color");
            cmd.env("COLORTERM", "truecolor");

            let child = pty_pair
                .slave
                .spawn_command(cmd)
                .map_err(|e| PtyError::CreateFailed(e.to_string()))?;

            let mut writer = pty_pair
                .master
                .take_writer()
                .map_err(|e| PtyError::CreateFailed(e.to_string()))?;

            if shell_name == "zsh" {
                let _ = writer.write_all(b" PROMPT='$ '; RPROMPT=''\n");
                let _ = writer.flush();
                let _ = writer.write_all(b"\x0c");
                let _ = writer.flush();
            }

            let mut reader = pty_pair
                .master
                .try_clone_reader()
                .map_err(|e| PtyError::CreateFailed(e.to_string()))?;

            let output_handle = thread::spawn(move || {
                let mut buf = [0u8; 4096];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            if output_tx.send(buf[..n].to_vec()).is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            });

            Ok::<_, PtyError>((pty_pair.master, writer, child, output_handle))
        })
        .await
        .map_err(|e| PtyError::CreateFailed(e.to_string()))??;

        let (master, writer, child, output_handle) = result;

        let session = PtySession {
            writer,
            master,
            child,
            working_dir: session_dir,
            _output_handle: output_handle,
            closed: false,
        };

        self.sessions
            .lock()
            .map_err(|e| PtyError::CreateFailed(e.to_string()))?
            .insert(session_id, session);

        Ok((session_id, output_rx))
    }

    pub async fn write(&self, session_id: Uuid, data: &[u8]) -> Result<(), PtyError> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|e| PtyError::WriteFailed(e.to_string()))?;
        let session = sessions
            .get_mut(&session_id)
            .ok_or(PtyError::SessionNotFound(session_id))?;

        if session.closed {
            return Err(PtyError::SessionClosed);
        }

        session
            .writer
            .write_all(data)
            .map_err(|e| PtyError::WriteFailed(e.to_string()))?;

        session
            .writer
            .flush()
            .map_err(|e| PtyError::WriteFailed(e.to_string()))?;

        Ok(())
    }

    pub async fn resize(&self, session_id: Uuid, cols: u16, rows: u16) -> Result<(), PtyError> {
        let sessions = self
            .sessions
            .lock()
            .map_err(|e| PtyError::ResizeFailed(e.to_string()))?;
        let session = sessions
            .get(&session_id)
            .ok_or(PtyError::SessionNotFound(session_id))?;

        if session.closed {
            return Err(PtyError::SessionClosed);
        }

        session
            .master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| PtyError::ResizeFailed(e.to_string()))?;

        Ok(())
    }

    pub async fn close_session(&self, session_id: Uuid) -> Result<(), PtyError> {
        let session = self
            .sessions
            .lock()
            .map_err(|_| PtyError::SessionClosed)?
            .remove(&session_id);

        if let Some(session) = session {
            Self::terminate(session).await;
        }
        Ok(())
    }

    /// Close every session whose shell was started inside `root`.
    ///
    /// Returns how many sessions were terminated. Used before a workspace's
    /// worktrees are deleted or recreated: a shell sitting in a worktree keeps its
    /// working directory open, and on Windows that blocks both deletion and the
    /// rename used to recreate the worktree. Waits for the shells to exit, so the
    /// directory really is free once this returns.
    pub async fn close_sessions_under(&self, root: &Path) -> usize {
        let doomed: Vec<PtySession> = {
            let Ok(mut sessions) = self.sessions.lock() else {
                return 0;
            };
            let ids: Vec<Uuid> = sessions
                .iter()
                .filter(|(_, session)| session.working_dir.starts_with(root))
                .map(|(id, _)| *id)
                .collect();
            ids.iter().filter_map(|id| sessions.remove(id)).collect()
        };

        let count = doomed.len();
        futures::future::join_all(doomed.into_iter().map(Self::terminate)).await;
        count
    }

    /// Kill the shell, then wait for it to actually exit.
    ///
    /// Dropping the master alone is not enough: the shell can outlive its PTY (this
    /// is routine on Windows with ConPTY) and go on holding its working directory
    /// open, which is what leaks orphaned shells into deleted worktrees. Neither is
    /// the kill itself enough — it is only a request on both platforms — and callers
    /// delete that directory the moment this returns.
    async fn terminate(session: PtySession) {
        let working_dir = session.working_dir.clone();
        let exited = tokio::task::spawn_blocking(move || Self::kill_and_wait(session))
            .await
            .unwrap_or(false);

        if !exited {
            tracing::warn!(
                "PTY shell in {} did not exit within {:?}; its working directory may still be locked",
                working_dir.display(),
                SHELL_EXIT_TIMEOUT
            );
        }
    }

    /// Kill the shell and poll until it is gone. Returns whether it exited in time.
    ///
    /// Polls rather than blocking in `wait()` so a shell that ignores the signal
    /// cannot pin a thread forever; the caller degrades to a warning instead.
    fn kill_and_wait(session: PtySession) -> bool {
        let PtySession {
            writer,
            master,
            mut child,
            ..
        } = session;

        let _ = child.kill();

        // Closing the PTY gives a shell that ignored the signal an EOF to exit on.
        drop(writer);
        drop(master);

        let deadline = Instant::now() + SHELL_EXIT_TIMEOUT;
        loop {
            // An error here means we can no longer observe the process; treat that as
            // gone rather than spinning until the deadline.
            if !matches!(child.try_wait(), Ok(None)) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(SHELL_EXIT_POLL_INTERVAL);
        }
    }
}

impl Default for PtyService {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session_count(service: &PtyService) -> usize {
        service.sessions.lock().unwrap().len()
    }

    #[tokio::test]
    async fn close_sessions_under_only_closes_matching_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        let worktree = workspace.join("repo");
        let unrelated = tmp.path().join("other");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::create_dir_all(&unrelated).unwrap();

        let service = PtyService::new();
        let (_inside, _inside_rx) = service.create_session(worktree, 80, 24).await.unwrap();
        let (outside, _outside_rx) = service.create_session(unrelated, 80, 24).await.unwrap();
        assert_eq!(session_count(&service), 2);

        // Matches on the workspace root, not just the exact working directory.
        assert_eq!(service.close_sessions_under(&workspace).await, 1);
        assert_eq!(session_count(&service), 1);

        // The surviving session is still writable, i.e. it was left fully intact.
        service.write(outside, b"\n").await.unwrap();

        service.close_session(outside).await.unwrap();
        assert_eq!(session_count(&service), 0);
    }

    /// The postcondition the workspace cleanup depends on: once the sessions under a
    /// directory are closed, that directory can be deleted and recreated. A shell
    /// left running there pins it, and on Windows that is what made archived
    /// workspaces impossible to resume.
    #[cfg(windows)]
    #[tokio::test]
    async fn close_sessions_under_frees_the_working_directory() {
        use std::time::Duration;

        let tmp = tempfile::tempdir().unwrap();
        let worktree = tmp.path().join("repo");
        let probe = tmp.path().join("probe");
        std::fs::create_dir(&worktree).unwrap();

        let service = PtyService::new();
        let (_id, mut rx) = service
            .create_session(worktree.clone(), 80, 24)
            .await
            .unwrap();

        // Output means the PTY is live; the shell may still be adopting its cwd.
        tokio::time::timeout(Duration::from_secs(60), rx.recv())
            .await
            .expect("PTY produced no output")
            .expect("PTY closed before the shell started");

        // Wait for the shell to actually adopt the directory as its cwd.
        let mut pinned = false;
        for _ in 0..100 {
            if std::fs::rename(&worktree, &probe).is_err() {
                pinned = true;
                break;
            }
            std::fs::rename(&probe, &worktree).unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(pinned, "the shell should pin its working directory");

        assert_eq!(service.close_sessions_under(&worktree).await, 1);

        // Exactly what the workspace cleanup does next, with no grace period in
        // between. Killing the shell is not enough on its own: ConPTY's console host
        // outlives it by tens of milliseconds, which the removal retries through.
        utils::fs::remove_dir_all_safe(&worktree)
            .expect("closing a session must let the worktree be removed");
        assert!(!worktree.exists());
    }

    #[tokio::test]
    async fn close_session_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let service = PtyService::new();
        let (id, _rx) = service
            .create_session(tmp.path().to_path_buf(), 80, 24)
            .await
            .unwrap();

        service.close_session(id).await.unwrap();
        // The terminal WS closes after the shell dies, so this runs a second time.
        service.close_session(id).await.unwrap();
        assert_eq!(session_count(&service), 0);
    }
}
