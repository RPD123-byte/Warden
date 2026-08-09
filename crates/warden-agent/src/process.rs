use std::{
    collections::{BTreeMap, HashMap},
    ffi::OsString,
    path::PathBuf,
    process::ExitStatus,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::{Notify, watch},
    task::JoinHandle,
    time::{Instant, sleep_until, timeout_at},
};
use uuid::Uuid;

#[cfg(unix)]
use tokio::time::sleep;

use crate::{AgentError, InvocationEnvironment, ProviderKind};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);
const DEFAULT_MAX_INPUT: usize = 4 * 1024 * 1024;
const DEFAULT_MAX_OUTPUT: usize = 16 * 1024 * 1024;
const TERMINATION_FORCE_WAIT: Duration = Duration::from_millis(750);
const SHUTDOWN_WAIT: Duration = Duration::from_secs(2);
#[cfg(unix)]
const TERMINATION_GRACE: Duration = Duration::from_millis(250);
#[cfg(unix)]
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Injectable process configuration. Defaults inherit the local user's normal
/// environment so provider subscription authentication remains available.
#[derive(Clone)]
pub struct CliConfig {
    pub program: PathBuf,
    pub prefix_args: Vec<OsString>,
    pub current_dir: Option<PathBuf>,
    pub env: BTreeMap<OsString, OsString>,
    pub clear_env: bool,
    pub timeout: Duration,
    pub max_input_bytes: usize,
    pub max_stdout_bytes: usize,
    pub max_stderr_bytes: usize,
}

impl std::fmt::Debug for CliConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CliConfig")
            .field("program", &self.program)
            .field("prefix_args", &self.prefix_args)
            .field("current_dir", &self.current_dir)
            .field("env_keys", &self.env.keys().collect::<Vec<_>>())
            .field("env_values", &"<redacted>")
            .field("clear_env", &self.clear_env)
            .field("timeout", &self.timeout)
            .field("max_input_bytes", &self.max_input_bytes)
            .field("max_stdout_bytes", &self.max_stdout_bytes)
            .field("max_stderr_bytes", &self.max_stderr_bytes)
            .finish()
    }
}

impl CliConfig {
    #[must_use]
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            prefix_args: Vec::new(),
            current_dir: None,
            env: BTreeMap::new(),
            clear_env: false,
            timeout: DEFAULT_TIMEOUT,
            max_input_bytes: DEFAULT_MAX_INPUT,
            max_stdout_bytes: DEFAULT_MAX_OUTPUT,
            max_stderr_bytes: DEFAULT_MAX_OUTPUT,
        }
    }

    #[must_use]
    pub fn with_prefix_arg(mut self, arg: impl Into<OsString>) -> Self {
        self.prefix_args.push(arg.into());
        self
    }

    #[must_use]
    pub fn with_current_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.current_dir = Some(path.into());
        self
    }

    #[must_use]
    pub fn with_env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    #[must_use]
    pub fn with_clear_env(mut self, clear_env: bool) -> Self {
        self.clear_env = clear_env;
        self
    }

    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    #[must_use]
    pub fn with_limits(mut self, input: usize, stdout: usize, stderr: usize) -> Self {
        self.max_input_bytes = input;
        self.max_stdout_bytes = stdout;
        self.max_stderr_bytes = stderr;
        self
    }
}

#[derive(Debug)]
pub(crate) struct ProcessOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

#[derive(Clone, Debug)]
pub(crate) struct CliRunner {
    provider: ProviderKind,
    config: CliConfig,
    active: Arc<Mutex<HashMap<Uuid, watch::Sender<bool>>>>,
    active_changed: Arc<Notify>,
    shutting_down: Arc<AtomicBool>,
}

impl CliRunner {
    pub(crate) fn new(provider: ProviderKind, config: CliConfig) -> Self {
        Self {
            provider,
            config,
            active: Arc::new(Mutex::new(HashMap::new())),
            active_changed: Arc::new(Notify::new()),
            shutting_down: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) async fn run(
        &self,
        invocation_id: Uuid,
        args: Vec<OsString>,
        input: String,
        environment: &InvocationEnvironment,
    ) -> Result<ProcessOutput, AgentError> {
        if input.len() > self.config.max_input_bytes {
            return Err(AgentError::InputTooLarge {
                provider: self.provider,
                actual: input.len(),
                limit: self.config.max_input_bytes,
            });
        }

        let (cancel_tx, mut cancel_rx) = watch::channel(false);
        {
            let mut active = self
                .active
                .lock()
                .map_err(|_| AgentError::InvocationRegistryPoisoned)?;
            if self.shutting_down.load(Ordering::Acquire) {
                return Err(AgentError::ShuttingDown {
                    provider: self.provider,
                });
            }
            active.insert(invocation_id, cancel_tx);
        }
        let _registration = ActiveRegistration {
            invocation_id,
            active: Arc::clone(&self.active),
            active_changed: Arc::clone(&self.active_changed),
        };

        let mut command = Command::new(&self.config.program);
        command
            .args(&self.config.prefix_args)
            .args(args)
            .kill_on_drop(true)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        configure_process_group(&mut command);
        if self.config.clear_env {
            command.env_clear();
        }
        command.envs(&self.config.env);
        command.envs(environment.iter());
        if let Some(current_dir) = &self.config.current_dir {
            command.current_dir(current_dir);
        }

        let mut child = command.spawn().map_err(|source| AgentError::Spawn {
            provider: self.provider,
            program: self.config.program.clone(),
            source,
        })?;
        // The guard is deliberately armed before the first await after spawn. If any owner drops
        // this invocation future (gateway revocation, client disconnect, task abort), its Drop
        // implementation synchronously kills the whole Unix process group. Normal async cleanup
        // disarms it after the bounded TERM-to-KILL sequence.
        let mut process_group = ProcessGroupGuard::for_child(&child);
        let mut stdin = child.stdin.take().expect("piped child stdin");
        let stdout = child.stdout.take().expect("piped child stdout");
        let stderr = child.stderr.take().expect("piped child stderr");

        let mut io_tasks = IoTasks {
            writer: tokio::spawn(async move {
                stdin.write_all(input.as_bytes()).await?;
                stdin.shutdown().await
            }),
            stdout: tokio::spawn(read_bounded(stdout, self.config.max_stdout_bytes)),
            stderr: tokio::spawn(read_bounded(stderr, self.config.max_stderr_bytes)),
        };

        let deadline = Instant::now() + self.config.timeout;
        let outcome = tokio::select! {
            status = child.wait() => WaitOutcome::Exited(status),
            _ = sleep_until(deadline) => WaitOutcome::TimedOut,
            changed = cancel_rx.changed() => {
                let _ = changed;
                WaitOutcome::Interrupted
            }
        };

        let status = match outcome {
            WaitOutcome::Exited(Ok(status)) => status,
            WaitOutcome::Exited(Err(source)) => {
                terminate(&mut child, &mut process_group).await;
                io_tasks.abort();
                return Err(AgentError::Wait {
                    provider: self.provider,
                    source,
                });
            }
            WaitOutcome::TimedOut => {
                terminate(&mut child, &mut process_group).await;
                io_tasks.abort();
                return Err(AgentError::Timeout {
                    provider: self.provider,
                    timeout: self.config.timeout,
                });
            }
            WaitOutcome::Interrupted => {
                terminate(&mut child, &mut process_group).await;
                io_tasks.abort();
                return Err(AgentError::Interrupted {
                    provider: self.provider,
                });
            }
        };

        let captured = collect_output(
            self.provider,
            (self.config.max_stdout_bytes, self.config.max_stderr_bytes),
            self.config.timeout,
            deadline,
            &mut cancel_rx,
            &mut io_tasks,
        )
        .await;
        let (stdout, stderr) = match captured {
            Ok(captured) => captured,
            Err(error) => {
                terminate(&mut child, &mut process_group).await;
                io_tasks.abort();
                return Err(error);
            }
        };

        // A well-behaved CLI exits with its descendants. If it detached a helper while closing
        // its pipes, Warden still owns that process group and must not leak it past the invocation.
        terminate(&mut child, &mut process_group).await;

        Ok(ProcessOutput {
            status,
            stdout: self.redact_output(stdout, environment),
            stderr: self.redact_output(stderr, environment),
        })
    }

    pub(crate) fn interrupt(&self, invocation_id: Uuid) -> Result<bool, AgentError> {
        let active = self
            .active
            .lock()
            .map_err(|_| AgentError::InvocationRegistryPoisoned)?;
        let Some(cancel) = active.get(&invocation_id) else {
            return Ok(false);
        };
        Ok(cancel.send(true).is_ok())
    }

    pub(crate) async fn shutdown(&self) -> Result<(), AgentError> {
        {
            let active = self
                .active
                .lock()
                .map_err(|_| AgentError::InvocationRegistryPoisoned)?;
            self.shutting_down.store(true, Ordering::Release);
            for cancel in active.values() {
                let _ = cancel.send(true);
            }
        }

        let deadline = Instant::now() + SHUTDOWN_WAIT;
        loop {
            // Subscribe before checking to avoid losing the final removal notification.
            let changed = self.active_changed.notified();
            if self
                .active
                .lock()
                .map_err(|_| AgentError::InvocationRegistryPoisoned)?
                .is_empty()
            {
                return Ok(());
            }
            if timeout_at(deadline, changed).await.is_err() {
                return Err(AgentError::ShutdownTimeout {
                    provider: self.provider,
                    timeout: SHUTDOWN_WAIT,
                });
            }
        }
    }

    fn redact_output(&self, bytes: Vec<u8>, environment: &InvocationEnvironment) -> Vec<u8> {
        let mut text = match String::from_utf8(bytes) {
            Ok(text) => text,
            // Provider JSON/JSONL is UTF-8. Preserve invalid provider output so
            // the parser can report it instead of manufacturing replacement
            // bytes that might conceal the actual protocol error.
            Err(error) => return error.into_bytes(),
        };
        for value in self
            .config
            .env
            .values()
            .chain(environment.iter().map(|(_, value)| value))
        {
            if let Some(secret) = value.to_str()
                && !secret.is_empty()
            {
                text = text.replace(secret, "<redacted>");
            }
        }
        text.into_bytes()
    }
}

enum WaitOutcome {
    Exited(std::io::Result<ExitStatus>),
    TimedOut,
    Interrupted,
}

struct Capture {
    bytes: Vec<u8>,
    exceeded: bool,
}

struct IoTasks {
    writer: JoinHandle<std::io::Result<()>>,
    stdout: JoinHandle<std::io::Result<Capture>>,
    stderr: JoinHandle<std::io::Result<Capture>>,
}

impl IoTasks {
    fn abort(self) {
        self.writer.abort();
        self.stdout.abort();
        self.stderr.abort();
    }
}

async fn read_bounded(
    mut reader: impl AsyncRead + Unpin,
    limit: usize,
) -> std::io::Result<Capture> {
    let mut captured = Vec::with_capacity(limit.min(8192));
    let mut exceeded = false;
    let mut buffer = [0_u8; 8192];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let remaining = limit.saturating_sub(captured.len());
        let retained = remaining.min(read);
        captured.extend_from_slice(&buffer[..retained]);
        exceeded |= retained < read;
    }
    Ok(Capture {
        bytes: captured,
        exceeded,
    })
}

enum JoinDeadline {
    TimedOut,
    Interrupted,
}

impl JoinDeadline {
    fn into_agent_error(self, provider: ProviderKind, timeout: Duration) -> AgentError {
        match self {
            Self::TimedOut => AgentError::Timeout { provider, timeout },
            Self::Interrupted => AgentError::Interrupted { provider },
        }
    }
}

async fn join_until<T>(
    handle: &mut JoinHandle<T>,
    cancel: &mut watch::Receiver<bool>,
    deadline: Instant,
) -> Result<Result<T, tokio::task::JoinError>, JoinDeadline> {
    tokio::select! {
        joined = handle => Ok(joined),
        _ = sleep_until(deadline) => Err(JoinDeadline::TimedOut),
        changed = cancel.changed() => {
            let _ = changed;
            Err(JoinDeadline::Interrupted)
        }
    }
}

async fn collect_output(
    provider: ProviderKind,
    limits: (usize, usize),
    configured_timeout: Duration,
    deadline: Instant,
    cancel: &mut watch::Receiver<bool>,
    tasks: &mut IoTasks,
) -> Result<(Vec<u8>, Vec<u8>), AgentError> {
    let write_result = join_until(&mut tasks.writer, cancel, deadline)
        .await
        .map_err(|reason| reason.into_agent_error(provider, configured_timeout))??;
    write_result.map_err(|source| AgentError::WriteInput { provider, source })?;

    let stdout = join_until(&mut tasks.stdout, cancel, deadline)
        .await
        .map_err(|reason| reason.into_agent_error(provider, configured_timeout))??
        .map_err(|source| AgentError::Wait { provider, source })?;
    let stderr = join_until(&mut tasks.stderr, cancel, deadline)
        .await
        .map_err(|reason| reason.into_agent_error(provider, configured_timeout))??
        .map_err(|source| AgentError::Wait { provider, source })?;

    Ok((
        checked_capture(provider, "stdout", limits.0, stdout)?,
        checked_capture(provider, "stderr", limits.1, stderr)?,
    ))
}

fn checked_capture(
    provider: ProviderKind,
    stream: &'static str,
    limit: usize,
    capture: Capture,
) -> Result<Vec<u8>, AgentError> {
    if capture.exceeded {
        return Err(AgentError::OutputTooLarge {
            provider,
            stream,
            limit,
        });
    }
    Ok(capture.bytes)
}

#[cfg(unix)]
fn configure_process_group(command: &mut Command) {
    // Group id zero asks `setpgid` to create a group whose id is the child pid. This happens in
    // the child before exec and avoids the race inherent in grouping it from the parent.
    command.process_group(0);
}

#[cfg(not(unix))]
fn configure_process_group(_: &mut Command) {}

#[cfg(unix)]
#[derive(Clone, Copy)]
struct ProcessGroup(Option<rustix::process::Pid>);

#[cfg(not(unix))]
#[derive(Clone, Copy)]
struct ProcessGroup;

impl ProcessGroup {
    #[cfg(unix)]
    fn for_child(child: &tokio::process::Child) -> Self {
        Self(
            child
                .id()
                .and_then(|id| i32::try_from(id).ok())
                .and_then(rustix::process::Pid::from_raw),
        )
    }

    #[cfg(not(unix))]
    fn for_child(_: &tokio::process::Child) -> Self {
        Self
    }

    #[cfg(unix)]
    fn exists(self) -> bool {
        self.0
            .is_some_and(|group| rustix::process::test_kill_process_group(group).is_ok())
    }

    #[cfg(unix)]
    fn signal(self, signal: rustix::process::Signal) {
        if let Some(group) = self.0 {
            let _ = rustix::process::kill_process_group(group, signal);
        }
    }
}

/// Cancellation-safe ownership of a provider process group.
///
/// Async Rust drops a future at any suspension point when its task is aborted or its caller wins
/// another `select!` branch. `tokio::process::Child::kill_on_drop` only targets the direct child,
/// so this guard supplies the corresponding synchronous group kill for that cancellation path.
struct ProcessGroupGuard {
    group: ProcessGroup,
    armed: bool,
}

impl ProcessGroupGuard {
    fn for_child(child: &tokio::process::Child) -> Self {
        Self {
            group: ProcessGroup::for_child(child),
            armed: true,
        }
    }

    fn group(&self) -> ProcessGroup {
        self.group
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        if self.armed {
            self.group.signal(rustix::process::Signal::KILL);
        }
    }
}

#[cfg(unix)]
async fn wait_for_group_exit(
    child: &mut tokio::process::Child,
    group: ProcessGroup,
    duration: Duration,
) -> bool {
    let deadline = Instant::now() + duration;
    loop {
        let _ = child.try_wait();
        if !group.exists() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        sleep(PROCESS_POLL_INTERVAL).await;
    }
}

#[cfg(unix)]
async fn terminate(child: &mut tokio::process::Child, guard: &mut ProcessGroupGuard) {
    let group = guard.group();
    if group.0.is_none() {
        let _ = child.start_kill();
        let _ = timeout_at(Instant::now() + TERMINATION_FORCE_WAIT, child.wait()).await;
        guard.disarm();
        return;
    }

    group.signal(rustix::process::Signal::TERM);
    if wait_for_group_exit(child, group, TERMINATION_GRACE).await {
        guard.disarm();
        return;
    }

    group.signal(rustix::process::Signal::KILL);
    if !wait_for_group_exit(child, group, TERMINATION_FORCE_WAIT).await {
        // The process group signal should include the direct child. Keep a direct-child fallback
        // for unusual platforms and ensure waiting is bounded even if kernel state is stale.
        let _ = child.start_kill();
        let _ = timeout_at(Instant::now() + TERMINATION_FORCE_WAIT, child.wait()).await;
    }
    guard.disarm();
}

#[cfg(not(unix))]
async fn terminate(child: &mut tokio::process::Child, guard: &mut ProcessGroupGuard) {
    if child.try_wait().ok().flatten().is_none() {
        let _ = child.start_kill();
        let _ = timeout_at(Instant::now() + TERMINATION_FORCE_WAIT, child.wait()).await;
    }
    guard.disarm();
}

struct ActiveRegistration {
    invocation_id: Uuid,
    active: Arc<Mutex<HashMap<Uuid, watch::Sender<bool>>>>,
    active_changed: Arc<Notify>,
}

impl Drop for ActiveRegistration {
    fn drop(&mut self) {
        if let Ok(mut active) = self.active.lock() {
            active.remove(&self.invocation_id);
        }
        self.active_changed.notify_waiters();
    }
}

pub(crate) fn parse_jsonl(
    provider: ProviderKind,
    stdout: &[u8],
) -> Result<Vec<serde_json::Value>, AgentError> {
    let stdout = String::from_utf8_lossy(stdout);
    let mut events = Vec::new();
    for (index, line) in stdout.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let event = serde_json::from_str(line).map_err(|source| AgentError::InvalidJsonLine {
            provider,
            line: index + 1,
            text: line.to_owned(),
            source,
        })?;
        events.push(event);
    }
    if events.is_empty() {
        return Err(AgentError::EmptyOutput { provider });
    }
    Ok(events)
}

pub(crate) fn process_exit_error(provider: ProviderKind, output: ProcessOutput) -> AgentError {
    AgentError::ProcessExit {
        provider,
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}
