use std::{
    path::{Path, PathBuf},
    process::Stdio,
    rc::Rc,
    sync::Arc,
};

use agent_client_protocol as proto;
use agent_client_protocol::Agent as _;
use command_group::AsyncGroupChild;
use futures::StreamExt;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
    sync::mpsc,
};
use tokio_util::{
    compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt},
    io::ReaderStream,
    sync::CancellationToken,
};
use tracing::error;
use workspace_utils::{
    approvals::ApprovalStatus, command_ext::GroupSpawnNoWindowExt, stream_lines::LinesStreamExt,
};

use super::{
    AcpClient, SessionManager,
    background_tasks::{self, AfterTurn, BackgroundTaskTracker, StdoutObserver, WaitPolicy},
};
use crate::{
    approvals::ExecutorApprovalService,
    command::{CmdOverrides, CommandParts},
    env::ExecutionEnv,
    executors::{ExecutorError, ExecutorExitResult, SpawnedChild, acp::AcpEvent},
};

/// Reusable harness for ACP-based conns (Gemini, Qwen, etc.)
pub struct AcpAgentHarness {
    session_namespace: String,
    model: Option<String>,
    mode: Option<String>,
    wait_for_background_tasks: bool,
}

impl Default for AcpAgentHarness {
    fn default() -> Self {
        // Keep existing behavior for Gemini
        Self::new()
    }
}

impl AcpAgentHarness {
    /// Create a harness with the default Gemini namespace
    pub fn new() -> Self {
        Self {
            session_namespace: "gemini_sessions".to_string(),
            model: None,
            mode: None,
            wait_for_background_tasks: false,
        }
    }

    /// Create a harness with a custom session namespace (e.g. for Qwen)
    pub fn with_session_namespace(namespace: impl Into<String>) -> Self {
        Self {
            session_namespace: namespace.into(),
            model: None,
            mode: None,
            wait_for_background_tasks: false,
        }
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    pub fn with_mode(mut self, mode: impl Into<String>) -> Self {
        self.mode = Some(mode.into());
        self
    }

    /// Keep the turn going while background tasks the agent started are
    /// still running, instead of letting them be killed when it ends. Relies
    /// on Kimi Code's background task output and a POSIX shell, so it only
    /// takes effect on unix; see [`background_tasks`].
    pub fn with_background_task_wait(mut self) -> Self {
        self.wait_for_background_tasks = true;
        self
    }

    pub fn apply_overrides(&mut self, executor_config: &crate::profile::ExecutorConfig) {
        if let Some(model_id) = &executor_config.model_id {
            self.model = Some(model_id.clone());
        }

        if let Some(agent_id) = &executor_config.agent_id {
            self.mode = Some(agent_id.clone());
        }
    }

    pub async fn spawn_with_command(
        &self,
        current_dir: &Path,
        prompt: String,
        command_parts: CommandParts,
        env: &ExecutionEnv,
        cmd_overrides: &CmdOverrides,
        approvals: Option<std::sync::Arc<dyn ExecutorApprovalService>>,
    ) -> Result<SpawnedChild, ExecutorError> {
        let (program_path, args) = command_parts.into_resolved().await?;
        let mut command = Command::new(program_path);
        command
            .kill_on_drop(true)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .current_dir(current_dir)
            .env("NPM_CONFIG_LOGLEVEL", "error")
            .env("NODE_NO_WARNINGS", "1")
            .args(&args);

        env.clone()
            .with_profile(cmd_overrides)
            .apply_to_command(&mut command);

        let mut child = command.group_spawn_no_window()?;

        let (exit_tx, exit_rx) = tokio::sync::oneshot::channel::<ExecutorExitResult>();
        let cancel = CancellationToken::new();

        Self::bootstrap_acp_connection(
            &mut child,
            current_dir.to_path_buf(),
            None,
            None,
            prompt,
            Some(exit_tx),
            self.session_namespace.clone(),
            self.model.clone(),
            self.mode.clone(),
            self.wait_for_background_tasks,
            approvals,
            cancel.clone(),
        )
        .await?;

        Ok(SpawnedChild {
            child,
            exit_signal: Some(exit_rx),
            cancel: Some(cancel),
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn spawn_follow_up_with_command(
        &self,
        current_dir: &Path,
        prompt: String,
        session_id: &str,
        reset_to_message_id: Option<&str>,
        command_parts: CommandParts,
        env: &ExecutionEnv,
        cmd_overrides: &CmdOverrides,
        approvals: Option<std::sync::Arc<dyn ExecutorApprovalService>>,
    ) -> Result<SpawnedChild, ExecutorError> {
        let (program_path, args) = command_parts.into_resolved().await?;
        let mut command = Command::new(program_path);
        command
            .kill_on_drop(true)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .current_dir(current_dir)
            .env("NPM_CONFIG_LOGLEVEL", "error")
            .env("NODE_NO_WARNINGS", "1")
            .args(&args);

        env.clone()
            .with_profile(cmd_overrides)
            .apply_to_command(&mut command);

        let mut child = command.group_spawn_no_window()?;

        let (exit_tx, exit_rx) = tokio::sync::oneshot::channel::<ExecutorExitResult>();
        let cancel = CancellationToken::new();

        Self::bootstrap_acp_connection(
            &mut child,
            current_dir.to_path_buf(),
            Some(session_id.to_string()),
            reset_to_message_id.map(str::to_string),
            prompt,
            Some(exit_tx),
            self.session_namespace.clone(),
            self.model.clone(),
            self.mode.clone(),
            self.wait_for_background_tasks,
            approvals,
            cancel.clone(),
        )
        .await?;

        Ok(SpawnedChild {
            child,
            exit_signal: Some(exit_rx),
            cancel: Some(cancel),
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn bootstrap_acp_connection(
        child: &mut AsyncGroupChild,
        cwd: PathBuf,
        existing_session: Option<String>,
        reset_to_message_id: Option<String>,
        prompt: String,
        exit_signal: Option<tokio::sync::oneshot::Sender<ExecutorExitResult>>,
        session_namespace: String,
        model: Option<String>,
        mode: Option<String>,
        wait_for_background_tasks: bool,
        approvals: Option<std::sync::Arc<dyn ExecutorApprovalService>>,
        cancel: CancellationToken,
    ) -> Result<(), ExecutorError> {
        let agent_pid = child.id();
        let background_tasks =
            (wait_for_background_tasks && cfg!(unix)).then(BackgroundTaskTracker::default);

        // Take child's stdio for ACP wiring
        let orig_stdout = child.inner().stdout.take().ok_or_else(|| {
            ExecutorError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "Child process has no stdout",
            ))
        })?;
        let orig_stdin = child.inner().stdin.take().ok_or_else(|| {
            ExecutorError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "Child process has no stdin",
            ))
        })?;

        // Create a fresh stdout pipe for logs
        let writer = crate::stdout_dup::create_stdout_pipe_writer(child)?;
        let shared_writer = Arc::new(tokio::sync::Mutex::new(writer));
        let (log_tx, mut log_rx) = mpsc::unbounded_channel::<String>();

        // Spawn log -> stdout writer task
        tokio::spawn(async move {
            while let Some(line) = log_rx.recv().await {
                let mut data = line.into_bytes();
                data.push(b'\n');
                let mut w = shared_writer.lock().await;
                let _ = w.write_all(&data).await;
            }
        });

        // ACP client STDIO
        let (mut to_acp_writer, acp_incoming_reader) = tokio::io::duplex(64 * 1024);
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        // Process stdout -> ACP
        let replay_tracker = Arc::new(ProtocolReplayTracker::default());
        let stdout_replay_tracker = replay_tracker.clone();
        let stdout_shutdown_rx = shutdown_rx.clone();
        let mut stdout_observer = background_tasks.clone().map(StdoutObserver::new);
        tokio::spawn(async move {
            let mut stdout = BufReader::new(orig_stdout);
            let mut line = String::new();
            loop {
                if *stdout_shutdown_rx.borrow() {
                    break;
                }
                line.clear();
                match stdout.read_line(&mut line).await {
                    Ok(0) => break,
                    Ok(_) => {
                        // Observe before forwarding, so tool updates are tracked
                        // before the ACP connection can resolve a later response.
                        if let Some(observer) = &mut stdout_observer {
                            observer.observe(line.as_bytes());
                        }
                        stdout_replay_tracker.record(&line);
                        let _ = to_acp_writer.write_all(line.as_bytes()).await;
                    }
                    Err(_) => break,
                }
            }
        });

        // ACP crate expects futures::AsyncRead + AsyncWrite, use tokio compat to adapt tokio::io::AsyncRead + Write
        let (acp_out_writer, acp_out_reader) = tokio::io::duplex(64 * 1024);
        let outgoing = acp_out_writer.compat_write();
        let incoming = acp_incoming_reader.compat();

        // Process ACP -> stdin
        let stdin_shutdown_rx = shutdown_rx.clone();
        tokio::spawn(async move {
            let mut child_stdin = orig_stdin;
            let mut lines = ReaderStream::new(acp_out_reader)
                .map(|res| res.map(|bytes| String::from_utf8_lossy(&bytes).into_owned()))
                .lines();
            while let Some(result) = lines.next().await {
                if *stdin_shutdown_rx.borrow() {
                    break;
                }
                match result {
                    Ok(line) => {
                        // Use \r\n on Windows for compatibility with buggy ACP implementations
                        const LINE_ENDING: &str = if cfg!(windows) { "\r\n" } else { "\n" };
                        let line = line + LINE_ENDING;
                        if let Err(err) = child_stdin.write_all(line.as_bytes()).await {
                            tracing::debug!("Failed to write to child stdin {err}");
                            break;
                        }
                        let _ = child_stdin.flush().await;
                    }
                    Err(err) => {
                        tracing::debug!("ACP stdin line error {err}");
                        break;
                    }
                }
            }
        });

        let mut exit_signal_tx = exit_signal;

        // Run ACP client in a LocalSet
        tokio::task::spawn_blocking(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build runtime");

            rt.block_on(async move {
                let local = tokio::task::LocalSet::new();
                local
                    .run_until(async move {
                        // Create event and raw channels
                        // Typed events available for future use; raw lines forwarded and persisted
                        let (event_tx, mut event_rx) =
                            mpsc::unbounded_channel::<crate::executors::acp::AcpEvent>();

                        // Create session manager
                        let session_manager = match SessionManager::new(session_namespace) {
                            Ok(sm) => sm,
                            Err(e) => {
                                error!("Failed to create session manager: {}", e);
                                return;
                            }
                        };
                        let session_manager = std::sync::Arc::new(session_manager);

                        // Create ACP client with approvals support
                        let client =
                            AcpClient::new(event_tx.clone(), approvals.clone(), cancel.clone());
                        let mut wait_policy = WaitPolicy::default();
                        let client_feedback_handle = client.clone();

                        client.record_user_prompt_event(&prompt);

                        // Set up connection
                        let (conn, io_fut) =
                            proto::ClientSideConnection::new(client, outgoing, incoming, |fut| {
                                tokio::task::spawn_local(fut);
                            });
                        let conn = Rc::new(conn);

                        // Drive I/O
                        let io_handle = tokio::task::spawn_local(async move {
                            let _ = io_fut.await;
                        });

                        let initialize_response = match conn
                            .initialize(proto::InitializeRequest::new(proto::ProtocolVersion::V1))
                            .await
                        {
                            Ok(response) => response,
                            Err(e) => {
                                report_startup_failure(
                                    &log_tx,
                                    &mut exit_signal_tx,
                                    "initialize",
                                    &e,
                                );
                                return;
                            }
                        };

                        let SessionSetup {
                            acp_session_id,
                            display_session_id,
                            prompt_to_send,
                        } = match establish_session(
                            &conn,
                            &session_manager,
                            &cwd,
                            existing_session,
                            reset_to_message_id.as_deref(),
                            &prompt,
                            initialize_response.agent_capabilities.load_session,
                            Some(replay_tracker.clone()),
                            &client_feedback_handle,
                        )
                        .await
                        {
                            Ok(session) => session,
                            Err(e) => {
                                report_startup_failure(
                                    &log_tx,
                                    &mut exit_signal_tx,
                                    "create session",
                                    &e,
                                );
                                return;
                            }
                        };

                        // Emit session ID
                        let _ = log_tx
                            .send(AcpEvent::SessionStart(display_session_id.clone()).to_string());

                        if let Some(model) = model.clone() {
                            match conn
                                .set_session_model(proto::SetSessionModelRequest::new(
                                    proto::SessionId::new(acp_session_id.clone()),
                                    model,
                                ))
                                .await
                            {
                                Ok(_) => {}
                                Err(e) => error!("Failed to set session model: {}", e),
                            }
                        }

                        if let Some(mode) = mode.clone() {
                            match conn
                                .set_session_mode(proto::SetSessionModeRequest::new(
                                    proto::SessionId::new(acp_session_id.clone()),
                                    mode,
                                ))
                                .await
                            {
                                Ok(_) => {}
                                Err(e) => error!("Failed to set session mode: {}", e),
                            }
                        }

                        // Start raw event forwarder and persistence
                        let app_tx_clone = log_tx.clone();
                        let sess_id_for_writer = display_session_id.clone();
                        let sm_for_writer = session_manager.clone();
                        let conn_for_cancel = conn.clone();
                        let acp_session_id_for_cancel = acp_session_id.clone();
                        tokio::task::spawn_local(async move {
                            while let Some(event) = event_rx.recv().await {
                                if let AcpEvent::ApprovalResponse(resp) = &event
                                    && let ApprovalStatus::Denied {
                                        reason: Some(reason),
                                    } = &resp.status
                                    && !reason.trim().is_empty()
                                {
                                    let _ = conn_for_cancel
                                        .cancel(proto::CancelNotification::new(
                                            proto::SessionId::new(
                                                acp_session_id_for_cancel.clone(),
                                            ),
                                        ))
                                        .await;
                                }

                                let line = event.to_string();
                                // Forward to stdout
                                let _ = app_tx_clone.send(line.clone());
                                // Persist to session file
                                let _ = sm_for_writer.append_raw_line(&sess_id_for_writer, &line);
                            }
                        });

                        // Build prompt request
                        let initial_req = proto::PromptRequest::new(
                            proto::SessionId::new(acp_session_id.clone()),
                            vec![proto::ContentBlock::Text(proto::TextContent::new(
                                prompt_to_send,
                            ))],
                        );

                        let mut current_req = Some(initial_req);

                        let mut silent_turn_retries = 0;
                        while let Some(req) = current_req.take() {
                            if cancel.is_cancelled() {
                                tracing::debug!("ACP executor cancelled, stopping prompt loop");
                                break;
                            }

                            // The protocol resolves responses independently of notification
                            // handlers. Drain the preceding turn before resetting its state.
                            tokio::select! {
                                _ = cancel.cancelled() => break,
                                _ = client_feedback_handle.drain_notifications(
                                    replay_tracker.notifications.load(std::sync::atomic::Ordering::Acquire)
                                ) => {}
                            }
                            client_feedback_handle.reset_turn_message();
                            tracing::trace!(?req, "sending ACP prompt request");
                            // Send the prompt and await completion to obtain stop_reason
                            let prompt_result = tokio::select! {
                                _ = cancel.cancelled() => {
                                    tracing::debug!("ACP executor cancelled during prompt");
                                    break;
                                }
                                result = conn.prompt(req) => result,
                            };

                            tokio::select! {
                                _ = cancel.cancelled() => break,
                                _ = client_feedback_handle.drain_notifications(
                                    replay_tracker.notifications.load(std::sync::atomic::Ordering::Acquire)
                                ) => {}
                            }
                            let mut turn_ended = false;
                            let mut background_wait_abandoned = false;
                            match prompt_result {
                                Ok(resp) => {
                                    turn_ended = resp.stop_reason == proto::StopReason::EndTurn;
                                    // Emit done with stop_reason
                                    let stop_reason = serde_json::to_string(&resp.stop_reason)
                                        .unwrap_or_default();
                                    let _ = log_tx.send(AcpEvent::Done(stop_reason).to_string());
                                }
                                Err(e) => {
                                    tracing::debug!("error {} {e} {:?}", e.code, e.data);
                                    if e.code
                                        == agent_client_protocol::ErrorCode::INTERNAL_ERROR.code
                                        && e.data
                                            .as_ref()
                                            .is_some_and(|d| d == "server shut down unexpectedly")
                                    {
                                        tracing::debug!("ACP server killed");
                                    } else {
                                        let _ = log_tx
                                            .send(AcpEvent::Error(format!("{e}")).to_string());
                                    }
                                }
                            }

                            // Flush any pending user feedback after finish
                            let feedback = client_feedback_handle
                                .drain_feedback()
                                .await
                                .join("\n")
                                .trim()
                                .to_string();
                            if !feedback.is_empty() {
                                tracing::trace!(?feedback, "sending ACP follow-up feedback");
                                let session_id = proto::SessionId::new(acp_session_id.clone());
                                let feedback_req = proto::PromptRequest::new(
                                    session_id.clone(),
                                    vec![proto::ContentBlock::Text(proto::TextContent::new(
                                        feedback,
                                    ))],
                                );
                                current_req = Some(feedback_req);
                            }

                            if turn_ended
                                && current_req.is_none()
                                && let Some(tracker) = &background_tasks
                            {
                                let live = tracker.live_tasks(|pid| {
                                    agent_pid.is_some_and(|agent| {
                                        workspace_utils::process::is_live_descendant(pid, agent)
                                    })
                                });
                                match wait_policy.after_turn(live, tracker.take_turn_tool_calls()) {
                                    AfterTurn::Finish => {}
                                    AfterTurn::Wait(tasks) => {
                                        client_feedback_handle.record_event(AcpEvent::Notice(
                                            background_tasks::waiting_notice(&tasks),
                                        ));
                                        current_req = Some(proto::PromptRequest::new(
                                            proto::SessionId::new(acp_session_id.clone()),
                                            vec![proto::ContentBlock::Text(
                                                proto::TextContent::new(
                                                    background_tasks::wait_prompt(&tasks),
                                                ),
                                            )],
                                        ));
                                    }
                                    AfterTurn::GiveUp(tasks) => {
                                        background_wait_abandoned = true;
                                        client_feedback_handle.record_event(AcpEvent::Notice(
                                            background_tasks::give_up_notice(&tasks),
                                        ));
                                    }
                                }
                            }
                            // Some agents end immediately after a tool result without
                            // reporting the outcome. Give them one bounded recovery turn.
                            if turn_ended
                                && current_req.is_none()
                                && !background_wait_abandoned
                                && client_feedback_handle.turn_had_tools()
                                && !client_feedback_handle.has_final_message()
                                && silent_turn_retries < 1
                            {
                                silent_turn_retries += 1;
                                client_feedback_handle.record_event(AcpEvent::Notice(
                                    "Agent ended without a final response; requesting completion.".into(),
                                ));
                                current_req = Some(proto::PromptRequest::new(
                                    proto::SessionId::new(acp_session_id.clone()),
                                    vec![proto::ContentBlock::Text(proto::TextContent::new(
                                        "Your last turn ended after tool activity without a final response. Review the original request and latest tool results. Continue any unfinished work, resolve or explain test failures and timeouts, and provide a final response describing the outcome and any remaining blockers.",
                                    ))],
                                ));
                            }
                        }

                        // Cancel session work before cleanup so the agent cannot keep
                        // starting background tasks while their process groups are stopped.
                        let _ = conn
                            .cancel(proto::CancelNotification::new(proto::SessionId::new(
                                acp_session_id,
                            )))
                            .await;

                        if let Some(tracker) = &background_tasks {
                            background_tasks::stop_background_tasks(tracker, agent_pid).await;
                        }

                        // Notify container of completion
                        if let Some(tx) = exit_signal_tx.take() {
                            let _ = tx.send(ExecutorExitResult::Success);
                        }

                        // Cleanup
                        drop(conn);
                        let _ = shutdown_tx.send(true);
                        let _ = io_handle.await;
                        drop(log_tx);
                    })
                    .await;
            });
        });

        Ok(())
    }
}

struct SessionSetup {
    acp_session_id: String,
    display_session_id: String,
    prompt_to_send: String,
}

#[derive(Default)]
struct ReplayProtocolState {
    active: bool,
    response_received: bool,
    replayed: usize,
}

#[derive(Default)]
struct ProtocolReplayTracker {
    state: std::sync::Mutex<ReplayProtocolState>,
    response_received: tokio::sync::Notify,
    notifications: std::sync::atomic::AtomicUsize,
}

impl ProtocolReplayTracker {
    fn start(&self) {
        let mut state = self.state.lock().unwrap();
        state.active = true;
        state.response_received = false;
        state.replayed = 0;
    }

    fn record(&self, line: &str) {
        let Ok(message) = serde_json::from_str::<serde_json::Value>(line) else {
            return;
        };
        let is_response = message.get("id").is_some() && message.get("method").is_none();
        let is_session_update = message.get("method").and_then(|m| m.as_str())
            == Some("session/update")
            && message.get("params").is_some_and(|params| {
                serde_json::from_value::<proto::SessionNotification>(params.clone()).is_ok()
            });

        if is_session_update {
            self.notifications
                .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        }
        let mut state = self.state.lock().unwrap();
        if !state.active {
            return;
        }
        if is_session_update {
            state.replayed += 1;
        }
        if is_response {
            state.active = false;
            state.response_received = true;
            self.response_received.notify_one();
        }
    }

    fn replayed_after_response(&self) -> Option<usize> {
        let state = self.state.lock().unwrap();
        state.response_received.then_some(state.replayed)
    }

    async fn wait_for_response(&self) -> usize {
        loop {
            if let Some(replayed) = self.replayed_after_response() {
                return replayed;
            }
            self.response_received.notified().await;
        }
    }
}

struct HistoryReplayGuard {
    client: AcpClient,
    tracker: Option<Arc<ProtocolReplayTracker>>,
}

impl HistoryReplayGuard {
    fn start(client: &AcpClient, tracker: Option<Arc<ProtocolReplayTracker>>) -> Self {
        client.set_history_replay(true);
        if let Some(tracker) = &tracker {
            tracker.start();
        }
        Self {
            client: client.clone(),
            tracker,
        }
    }

    async fn finish<T>(self, result: Result<T, proto::Error>) -> Result<T, proto::Error> {
        let expected = match self.tracker {
            Some(tracker) if result.is_ok() => tracker.wait_for_response().await,
            Some(tracker) => tracker.replayed_after_response().unwrap_or(0),
            None => 0,
        };
        self.client.wait_for_replayed_notifications(expected).await;
        self.client.set_history_replay(false);
        result
    }
}

async fn establish_session(
    conn: &impl proto::Agent,
    session_manager: &SessionManager,
    cwd: &Path,
    existing_session: Option<String>,
    reset_to_message_id: Option<&str>,
    prompt: &str,
    native_resume_supported: bool,
    replay_tracker: Option<Arc<ProtocolReplayTracker>>,
    client: &AcpClient,
) -> Result<SessionSetup, proto::Error> {
    if let Some(existing) = existing_session {
        let checkpoint = reset_to_message_id.unwrap_or(&existing);
        let display_session_id = uuid::Uuid::new_v4().to_string();
        if let Err(e) = session_manager.fork_session(checkpoint, &display_session_id) {
            tracing::warn!("Failed to snapshot ACP session history: {e}");
        }

        let native_session = if native_resume_supported && reset_to_message_id.is_none() {
            match session_manager.native_session_id(&existing) {
                Ok(native_id) => {
                    match session_manager.native_session_is_current(&existing, &native_id) {
                        Ok(true) => Some(native_id),
                        Ok(false) => None,
                        Err(e) => {
                            tracing::warn!("Failed to resolve native ACP session head: {e}");
                            None
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to resolve native ACP session ID: {e}");
                    None
                }
            }
        } else {
            None
        };

        if let Some(native_id) = native_session {
            let replay = HistoryReplayGuard::start(client, replay_tracker);
            let result = conn
                .load_session(proto::LoadSessionRequest::new(
                    native_id.clone(),
                    cwd.to_path_buf(),
                ))
                .await;

            match replay.finish(result).await {
                Ok(_) => {
                    if let Err(e) =
                        session_manager.set_native_session_id(&display_session_id, &native_id)
                    {
                        tracing::warn!("Failed to save native ACP session mapping: {e}");
                    }
                    return Ok(SessionSetup {
                        acp_session_id: native_id,
                        display_session_id,
                        prompt_to_send: prompt.to_string(),
                    });
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to load native ACP session {native_id}: {e}; falling back to reconstructed history"
                    );
                }
            }
        }

        let prompt_to_send = session_manager
            .generate_resume_prompt(checkpoint, prompt)
            .unwrap_or_else(|e| {
                tracing::warn!("Failed to reconstruct ACP session history: {e}");
                prompt.to_string()
            });
        let response = conn
            .new_session(proto::NewSessionRequest::new(cwd.to_path_buf()))
            .await?;
        let native_id = response.session_id.0.to_string();
        if let Err(e) = session_manager.set_native_session_id(&display_session_id, &native_id) {
            tracing::warn!("Failed to save native ACP session mapping: {e}");
        }

        return Ok(SessionSetup {
            acp_session_id: native_id,
            display_session_id,
            prompt_to_send,
        });
    }

    let response = conn
        .new_session(proto::NewSessionRequest::new(cwd.to_path_buf()))
        .await?;
    let session_id = response.session_id.0.to_string();
    Ok(SessionSetup {
        acp_session_id: session_id.clone(),
        display_session_id: session_id,
        prompt_to_send: prompt.to_string(),
    })
}

fn report_startup_failure(
    log_tx: &mpsc::UnboundedSender<String>,
    exit_signal: &mut Option<tokio::sync::oneshot::Sender<ExecutorExitResult>>,
    operation: &str,
    error: &proto::Error,
) {
    let message = format!("Failed to {operation}: {error}");
    error!("{message}");
    let _ = log_tx.send(AcpEvent::Error(message).to_string());
    if let Some(tx) = exit_signal.take() {
        let _ = tx.send(ExecutorExitResult::Failure);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct FakeAgent {
        new_session_requests: Mutex<Vec<proto::NewSessionRequest>>,
        load_session_requests: Mutex<Vec<proto::LoadSessionRequest>>,
        fail_load: bool,
    }

    #[async_trait::async_trait(?Send)]
    impl proto::Agent for FakeAgent {
        async fn initialize(
            &self,
            args: proto::InitializeRequest,
        ) -> Result<proto::InitializeResponse, proto::Error> {
            Ok(proto::InitializeResponse::new(args.protocol_version))
        }

        async fn authenticate(
            &self,
            _args: proto::AuthenticateRequest,
        ) -> Result<proto::AuthenticateResponse, proto::Error> {
            Ok(proto::AuthenticateResponse::default())
        }

        async fn new_session(
            &self,
            args: proto::NewSessionRequest,
        ) -> Result<proto::NewSessionResponse, proto::Error> {
            self.new_session_requests.lock().unwrap().push(args);
            Ok(proto::NewSessionResponse::new("fallback-native-id"))
        }

        async fn load_session(
            &self,
            args: proto::LoadSessionRequest,
        ) -> Result<proto::LoadSessionResponse, proto::Error> {
            self.load_session_requests.lock().unwrap().push(args);
            if self.fail_load {
                Err(proto::Error::invalid_request())
            } else {
                Ok(proto::LoadSessionResponse::new())
            }
        }

        async fn prompt(
            &self,
            _args: proto::PromptRequest,
        ) -> Result<proto::PromptResponse, proto::Error> {
            Ok(proto::PromptResponse::new(proto::StopReason::EndTurn))
        }

        async fn cancel(&self, _args: proto::CancelNotification) -> Result<(), proto::Error> {
            Ok(())
        }
    }

    struct TestManager {
        dir: PathBuf,
        manager: SessionManager,
    }

    impl TestManager {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "vibe-kanban-acp-harness-test-{}",
                uuid::Uuid::new_v4()
            ));
            let manager = SessionManager::with_base_dir(dir.clone()).unwrap();
            Self { dir, manager }
        }

        fn add_history(&self, session_id: &str) {
            self.manager
                .append_raw_line(
                    session_id,
                    &AcpEvent::User("earlier request".into()).to_string(),
                )
                .unwrap();
            self.manager
                .append_raw_line(
                    session_id,
                    &AcpEvent::Message(proto::ContentBlock::Text(proto::TextContent::new(
                        "earlier response",
                    )))
                    .to_string(),
                )
                .unwrap();
        }
    }

    impl Drop for TestManager {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn client() -> AcpClient {
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        AcpClient::new(event_tx, None, CancellationToken::new())
    }

    async fn establish(
        agent: &FakeAgent,
        manager: &SessionManager,
        native_resume_supported: bool,
    ) -> SessionSetup {
        establish_from(agent, manager, "previous", None, native_resume_supported).await
    }

    async fn establish_from(
        agent: &FakeAgent,
        manager: &SessionManager,
        existing: &str,
        reset_to_message_id: Option<&str>,
        native_resume_supported: bool,
    ) -> SessionSetup {
        establish_session(
            agent,
            manager,
            Path::new("/tmp/workspace"),
            Some(existing.to_string()),
            reset_to_message_id,
            "continue",
            native_resume_supported,
            None,
            &client(),
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn native_resume_loads_existing_session_without_reconstructed_history() {
        let agent = FakeAgent::default();
        let test = TestManager::new();
        test.add_history("previous");
        let original = test.manager.read_session_raw("previous").unwrap();

        let setup = establish(&agent, &test.manager, true).await;

        assert_eq!(setup.acp_session_id, "previous");
        assert_ne!(setup.display_session_id, "previous");
        assert_eq!(setup.prompt_to_send, "continue");
        assert!(agent.new_session_requests.lock().unwrap().is_empty());
        let loads = agent.load_session_requests.lock().unwrap();
        assert_eq!(loads.len(), 1);
        assert_eq!(loads[0].session_id.0.as_ref(), "previous");
        assert_eq!(loads[0].cwd, Path::new("/tmp/workspace"));
        assert_eq!(test.manager.read_session_raw("previous").unwrap(), original);
        assert_eq!(
            test.manager
                .read_session_raw(&setup.display_session_id)
                .unwrap(),
            original
        );
        assert_eq!(
            test.manager
                .native_session_id(&setup.display_session_id)
                .unwrap(),
            "previous"
        );
    }

    #[tokio::test]
    async fn fallback_sends_history_once_and_preserves_native_session_id() {
        let agent = FakeAgent::default();
        let test = TestManager::new();
        test.add_history("previous");

        let setup = establish(&agent, &test.manager, false).await;

        assert_eq!(setup.acp_session_id, "fallback-native-id");
        assert_ne!(setup.display_session_id, "fallback-native-id");
        assert_eq!(setup.prompt_to_send.matches("earlier request").count(), 1);
        assert_eq!(setup.prompt_to_send.matches("earlier response").count(), 1);
        assert_eq!(
            setup
                .prompt_to_send
                .matches("=== CURRENT REQUEST ===")
                .count(),
            1
        );
        assert!(agent.load_session_requests.lock().unwrap().is_empty());
        let requests = agent.new_session_requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].meta.is_none());
        let copied = test
            .manager
            .read_resume_history(&setup.display_session_id)
            .unwrap();
        assert!(copied.contains("earlier request"));
        assert!(copied.contains("earlier response"));
        assert_eq!(
            test.manager
                .native_session_id(&setup.display_session_id)
                .unwrap(),
            "fallback-native-id"
        );
    }

    #[tokio::test]
    async fn failed_native_load_uses_single_history_fallback() {
        let agent = FakeAgent {
            fail_load: true,
            ..Default::default()
        };
        let test = TestManager::new();
        test.add_history("previous");

        let setup = establish(&agent, &test.manager, true).await;

        assert_ne!(setup.display_session_id, "fallback-native-id");
        assert_eq!(setup.prompt_to_send.matches("earlier request").count(), 1);
        assert_eq!(agent.load_session_requests.lock().unwrap().len(), 1);
        let requests = agent.new_session_requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].meta.is_none());
    }

    #[tokio::test]
    async fn retry_reconstructs_from_checkpoint_instead_of_native_history() {
        let agent = FakeAgent::default();
        let test = TestManager::new();
        test.add_history("previous");

        let setup = establish_from(&agent, &test.manager, "previous", Some("previous"), true).await;

        assert_eq!(setup.acp_session_id, "fallback-native-id");
        assert_ne!(setup.display_session_id, "previous");
        assert_eq!(setup.prompt_to_send.matches("earlier request").count(), 1);
        assert_eq!(setup.prompt_to_send.matches("earlier response").count(), 1);
        assert!(
            setup
                .prompt_to_send
                .contains("=== CURRENT REQUEST ===\ncontinue")
        );
        assert!(agent.load_session_requests.lock().unwrap().is_empty());
        assert_eq!(agent.new_session_requests.lock().unwrap().len(), 1);
        assert_eq!(
            test.manager
                .native_session_id(&setup.display_session_id)
                .unwrap(),
            "fallback-native-id"
        );
    }

    #[tokio::test]
    async fn older_checkpoint_after_standalone_reset_skips_native_resume() {
        let agent = FakeAgent::default();
        let test = TestManager::new();
        test.add_history("retained");
        test.add_history("latest");
        test.manager
            .set_native_session_id("retained", "native")
            .unwrap();
        test.manager
            .set_native_session_id("latest", "native")
            .unwrap();

        let setup = establish_from(&agent, &test.manager, "retained", None, true).await;

        assert_eq!(setup.acp_session_id, "fallback-native-id");
        assert!(setup.prompt_to_send.contains("=== CURRENT REQUEST ==="));
        assert!(agent.load_session_requests.lock().unwrap().is_empty());
        assert_eq!(agent.new_session_requests.lock().unwrap().len(), 1);
    }

    #[test]
    fn replay_tracker_ignores_notifications_acp_rejects() {
        let tracker = ProtocolReplayTracker::default();
        tracker.start();
        tracker.record(
            r#"{"method":"session/update","params":{"sessionId":"previous","update":{"sessionUpdate":"agentMessageChunk","content":{"type":"text","text":"invalid"}}}}"#,
        );
        tracker.record(
            r#"{"method":"session/update","params":{"sessionId":"previous","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"valid"}}}}"#,
        );
        tracker.record(r#"{"id":0,"result":null}"#);

        assert_eq!(tracker.replayed_after_response(), Some(1));
    }

    #[tokio::test]
    async fn native_resume_drains_queued_history_before_live_notifications() {
        use tokio::io::{AsyncBufReadExt, BufReader};

        tokio::task::LocalSet::new()
            .run_until(async {
                let (event_tx, mut event_rx) = mpsc::unbounded_channel();
                let client = AcpClient::new(event_tx, None, CancellationToken::new());
                let tracker = Arc::new(ProtocolReplayTracker::default());
                let agent_tracker = tracker.clone();
                let (client_stream, agent_stream) = tokio::io::duplex(4096);
                let (client_read, client_write) = tokio::io::split(client_stream);
                let (conn, io) = proto::ClientSideConnection::new(
                    client.clone(),
                    client_write.compat_write(),
                    client_read.compat(),
                    |fut| {
                        tokio::task::spawn_local(fut);
                    },
                );
                let io_handle = tokio::task::spawn_local(io);
                let agent_handle = tokio::task::spawn_local(async move {
                    let mut agent_stream = BufReader::new(agent_stream);
                    let mut request = String::new();
                    agent_stream.read_line(&mut request).await.unwrap();
                    let request: serde_json::Value = serde_json::from_str(&request).unwrap();
                    let mut payload = (0..100)
                        .map(|index| {
                            serde_json::json!({
                                "jsonrpc": "2.0",
                                "method": "session/update",
                                "params": {
                                    "sessionId": "previous",
                                    "update": {
                                        "sessionUpdate": "agent_message_chunk",
                                        "content": { "type": "text", "text": format!("old {index}") }
                                    }
                                }
                            })
                            .to_string()
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    let response = serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": request["id"],
                        "result": null
                    });
                    payload.push_str(&format!("\n{response}\n"));
                    for line in payload.lines() {
                        agent_tracker.record(line);
                    }
                    agent_stream
                        .get_mut()
                        .write_all(payload.as_bytes())
                        .await
                        .unwrap();
                });

                let replay = HistoryReplayGuard::start(&client, Some(tracker));
                let result = conn
                    .load_session(proto::LoadSessionRequest::new(
                        "previous",
                        Path::new("/tmp/workspace"),
                    ))
                    .await;
                tokio::time::timeout(std::time::Duration::from_secs(10), replay.finish(result))
                    .await
                    .expect("replay drain timed out")
                    .unwrap();

                assert!(event_rx.try_recv().is_err());
                proto::Client::session_notification(
                    &client,
                    proto::SessionNotification::new(
                        "previous",
                        proto::SessionUpdate::AgentMessageChunk(proto::ContentChunk::new(
                            proto::ContentBlock::Text(proto::TextContent::new("new")),
                        )),
                    ),
                )
                .await
                .unwrap();
                assert!(matches!(
                    event_rx.recv().await,
                    Some(AcpEvent::Message(proto::ContentBlock::Text(content))) if content.text == "new"
                ));

                drop(conn);
                agent_handle.abort();
                io_handle.abort();
            })
            .await;
    }

    #[tokio::test]
    async fn prompt_drains_final_message_batched_with_response() {
        use tokio::io::{AsyncBufReadExt, BufReader};

        tokio::task::LocalSet::new()
            .run_until(async {
                let (event_tx, mut event_rx) = mpsc::unbounded_channel();
                let client = AcpClient::new(event_tx, None, CancellationToken::new());
                let tracker = Arc::new(ProtocolReplayTracker::default());
                let agent_tracker = tracker.clone();
                let (client_stream, agent_stream) = tokio::io::duplex(4096);
                let (client_read, client_write) = tokio::io::split(client_stream);
                let (conn, io) = proto::ClientSideConnection::new(
                    client.clone(),
                    client_write.compat_write(),
                    client_read.compat(),
                    |fut| {
                        tokio::task::spawn_local(fut);
                    },
                );
                let io_handle = tokio::task::spawn_local(io);
                let agent_handle = tokio::task::spawn_local(async move {
                    let mut agent_stream = BufReader::new(agent_stream);
                    let mut request = String::new();
                    agent_stream.read_line(&mut request).await.unwrap();
                    let request: serde_json::Value = serde_json::from_str(&request).unwrap();
                    let mut payload = (0..100)
                        .map(|index| {
                            serde_json::json!({
                                "jsonrpc": "2.0",
                                "method": "session/update",
                                "params": {
                                    "sessionId": "previous",
                                    "update": {
                                        "sessionUpdate": "agent_message_chunk",
                                        "content": { "type": "text", "text": format!("old {index}") }
                                    }
                                }
                            })
                            .to_string()
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    let response = serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": request["id"],
                        "result": {"stopReason": "end_turn"}
                    });
                    payload.push_str(&format!("\n{response}\n"));
                    for line in payload.lines() {
                        agent_tracker.record(line);
                    }
                    agent_stream
                        .get_mut()
                        .write_all(payload.as_bytes())
                        .await
                        .unwrap();
                });

                client.reset_turn_message();
                conn.prompt(proto::PromptRequest::new(
                    "previous",
                    vec![proto::ContentBlock::Text(proto::TextContent::new("work"))],
                )).await.unwrap();
                tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    client.drain_notifications(tracker.notifications.load(std::sync::atomic::Ordering::Acquire)),
                ).await.expect("notification drain timed out");
                assert!(client.has_final_message(), "completed response must suppress recovery");
                assert!(event_rx.try_recv().is_ok());

                drop(conn);
                agent_handle.abort();
                io_handle.abort();
            })
            .await;
    }

    #[tokio::test]
    async fn startup_failures_are_visible_and_fail_the_execution() {
        let (log_tx, mut log_rx) = mpsc::unbounded_channel();
        let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();
        let mut exit_signal = Some(exit_tx);
        report_startup_failure(
            &log_tx,
            &mut exit_signal,
            "create session",
            &proto::Error::invalid_request(),
        );
        assert!(matches!(
            exit_rx.await.unwrap(),
            ExecutorExitResult::Failure
        ));
        assert!(matches!(
            log_rx.recv().await.unwrap().parse::<AcpEvent>().unwrap(),
            AcpEvent::Error(message) if message.contains("Failed to create session")
        ));
        assert!(exit_signal.is_none());
    }
}
