use crate::{
    action::{ActionGrant, InvocationCredential},
    config::Config,
    event::{HookEventEnvelope, HookEventKind},
    registry::{HookId, HookMetadata, HookPreparer, HookRevision},
};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    ffi::OsStr,
    fs, io,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::{Mutex, Semaphore},
    time::{Instant, timeout, timeout_at},
};
use uuid::Uuid;

const WORKER_PROTOCOL_VERSION: u32 = 1;
type RevisionKey = (HookId, String);
type WorkerSlot = Arc<Mutex<Option<WorkerProcess>>>;

struct WorkerCancellationGuard {
    slot: WorkerSlot,
    armed: bool,
}

impl WorkerCancellationGuard {
    fn new(slot: WorkerSlot) -> Self {
        Self { slot, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for WorkerCancellationGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let slot = self.slot.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                if let Some(mut worker) = slot.lock().await.take() {
                    worker.terminate().await;
                }
            });
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PythonError {
    #[error("Python runtime I/O failed at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("Python environment command failed: {command} (status {status:?}): {stderr}")]
    EnvironmentCommand {
        command: String,
        status: Option<i32>,
        stderr: String,
    },
    #[error("Python environment command exceeded {timeout:?}: {command}")]
    EnvironmentTimeout {
        command: String,
        timeout: std::time::Duration,
    },
    #[error(
        "Python environment command {stream} exceeded its {limit}-byte capture limit: {command}"
    )]
    EnvironmentOutputTooLarge {
        command: String,
        stream: &'static str,
        limit: usize,
    },
    #[error("Python worker for {hook}@{revision} failed to start: {message}")]
    Handshake {
        hook: HookId,
        revision: String,
        message: String,
    },
    #[error(
        "Python worker for {hook}@{revision} exceeded its {timeout:?} import handshake timeout"
    )]
    HandshakeTimeout {
        hook: HookId,
        revision: String,
        timeout: std::time::Duration,
    },
    #[error("Python hook candidate {hook} preparation exceeded {timeout:?}")]
    CandidateTimeout {
        hook: HookId,
        timeout: std::time::Duration,
    },
    #[error("Python worker protocol error: {0}")]
    Protocol(String),
    #[error("Python hook {hook}@{revision} invocation {invocation_id} failed: {message}")]
    Invocation {
        hook: HookId,
        revision: String,
        invocation_id: Uuid,
        message: String,
    },
    #[error("Python hook invocation exceeded {0:?}")]
    Timeout(std::time::Duration),
    #[error("Python hook worker capacity is closed")]
    Closed,
}

#[derive(Clone, Debug)]
pub struct PreparedEnvironment {
    pub hash: String,
    pub root: PathBuf,
    pub python: PathBuf,
}

#[derive(Clone)]
pub struct EnvironmentManager {
    base_python: PathBuf,
    sdk_root: PathBuf,
    environments_root: PathBuf,
    command_timeout: std::time::Duration,
    max_command_output_bytes: usize,
    build_lock: Arc<Mutex<()>>,
}

struct StagingDirectory {
    path: PathBuf,
    armed: bool,
}

impl StagingDirectory {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for StagingDirectory {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

impl EnvironmentManager {
    pub fn new(base_python: PathBuf, sdk_root: PathBuf, runtimes_root: PathBuf) -> Self {
        Self::with_limits(
            base_python,
            sdk_root,
            runtimes_root,
            std::time::Duration::from_secs(60),
            1024 * 1024,
        )
    }

    pub fn with_limits(
        base_python: PathBuf,
        sdk_root: PathBuf,
        runtimes_root: PathBuf,
        command_timeout: std::time::Duration,
        max_command_output_bytes: usize,
    ) -> Self {
        Self {
            base_python,
            sdk_root,
            environments_root: runtimes_root.join("python-envs"),
            command_timeout,
            max_command_output_bytes: max_command_output_bytes.max(1),
            build_lock: Arc::new(Mutex::new(())),
        }
    }

    pub async fn prepare(&self, source_dir: &Path) -> Result<PreparedEnvironment, PythonError> {
        let requirements = source_dir.join("requirements.txt");
        let hash =
            self.environment_hash(requirements.is_file().then_some(requirements.as_path()))?;
        let root = self.environments_root.join(&hash);
        let python = environment_python(&root);
        if python.is_file() {
            return Ok(PreparedEnvironment { hash, root, python });
        }

        let _guard = self.build_lock.lock().await;
        if python.is_file() {
            return Ok(PreparedEnvironment { hash, root, python });
        }
        fs::create_dir_all(&self.environments_root)
            .map_err(|source| io_error(self.environments_root.clone(), source))?;
        let staging = self
            .environments_root
            .join(format!(".{hash}.candidate-{}", Uuid::new_v4()));
        let mut staging_guard = StagingDirectory::new(staging);
        let result = self
            .build_environment(
                staging_guard.path(),
                requirements.is_file().then_some(requirements.as_path()),
            )
            .await;
        result?;
        match fs::rename(staging_guard.path(), &root) {
            Ok(()) => staging_guard.disarm(),
            Err(_) if environment_python(&root).is_file() => {}
            Err(source) => return Err(io_error(root.clone(), source)),
        }
        Ok(PreparedEnvironment {
            hash,
            python: environment_python(&root),
            root,
        })
    }

    async fn build_environment(
        &self,
        root: &Path,
        requirements: Option<&Path>,
    ) -> Result<(), PythonError> {
        run_command(
            &self.base_python,
            [OsStr::new("-m"), OsStr::new("venv"), root.as_os_str()],
            self.command_timeout,
            self.max_command_output_bytes,
        )
        .await?;
        let python = environment_python(root);
        let sdk_package = self.sdk_root.join("src/warden");
        run_command(
            &python,
            [
                OsStr::new("-c"),
                OsStr::new(
                    "import pathlib, shutil, sys, sysconfig; target = pathlib.Path(sysconfig.get_paths()['purelib']) / 'warden'; shutil.copytree(sys.argv[1], target)",
                ),
                sdk_package.as_os_str(),
            ],
            self.command_timeout,
            self.max_command_output_bytes,
        )
        .await?;
        if let Some(requirements) = requirements {
            run_command_in(
                &python,
                [
                    OsStr::new("-m"),
                    OsStr::new("pip"),
                    OsStr::new("install"),
                    OsStr::new("--disable-pip-version-check"),
                    OsStr::new("-r"),
                    requirements.as_os_str(),
                ],
                requirements.parent(),
                self.command_timeout,
                self.max_command_output_bytes,
            )
            .await?;
        }
        Ok(())
    }

    fn environment_hash(&self, requirements: Option<&Path>) -> Result<String, PythonError> {
        let mut digest = Sha256::new();
        digest.update(b"warden-python-environment-v1\0");
        digest.update(self.base_python.as_os_str().as_encoded_bytes());
        digest.update([0]);
        let pyproject = self.sdk_root.join("pyproject.toml");
        digest.update(fs::read(&pyproject).map_err(|source| io_error(pyproject.clone(), source))?);
        let sdk_source = self.sdk_root.join("src");
        hash_tree(&sdk_source, &sdk_source, &mut digest)?;
        if let Some(requirements) = requirements {
            digest.update(
                fs::read(requirements)
                    .map_err(|source| io_error(requirements.to_owned(), source))?,
            );
        }
        Ok(hex::encode(digest.finalize()))
    }
}

#[derive(Clone)]
pub struct PythonRuntime {
    environments: EnvironmentManager,
    action_socket: PathBuf,
    timeout: std::time::Duration,
    candidate_timeout: std::time::Duration,
    max_message_bytes: usize,
    capacity: Arc<Semaphore>,
    workers: Arc<Mutex<HashMap<RevisionKey, WorkerSlot>>>,
    environments_by_revision: Arc<Mutex<HashMap<RevisionKey, PreparedEnvironment>>>,
}

impl PythonRuntime {
    pub fn new(config: &Config) -> Self {
        Self {
            environments: EnvironmentManager::with_limits(
                config.python.clone(),
                config.python_sdk.clone(),
                config.paths.runtimes.clone(),
                config.candidate_timeout,
                config.max_hook_message_bytes,
            ),
            action_socket: config.paths.action_socket.clone(),
            timeout: config.hook_timeout,
            candidate_timeout: config.candidate_timeout,
            max_message_bytes: config.max_hook_message_bytes,
            capacity: Arc::new(Semaphore::new(config.max_concurrent_hooks)),
            workers: Arc::new(Mutex::new(HashMap::new())),
            environments_by_revision: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn invoke(
        &self,
        revision: Arc<HookRevision>,
        event: HookEventEnvelope,
        credential: InvocationCredential,
    ) -> Result<HookInvocationResult, PythonError> {
        let _permit = self
            .capacity
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| PythonError::Closed)?;
        let key = (revision.id.clone(), revision.revision.clone());
        let cached_environment = {
            let environments = self.environments_by_revision.lock().await;
            environments.get(&key).cloned()
        };
        let environment = match cached_environment {
            Some(environment) => environment,
            None => {
                let environment = self.environments.prepare(&revision.source_dir).await?;
                self.environments_by_revision
                    .lock()
                    .await
                    .insert(key.clone(), environment.clone());
                environment
            }
        };
        let slot = {
            let mut workers = self.workers.lock().await;
            workers
                .entry(key.clone())
                .or_insert_with(|| Arc::new(Mutex::new(None)))
                .clone()
        };
        let invocation_id = credential.invocation_id;
        let mut cancellation_guard = WorkerCancellationGuard::new(slot.clone());
        let operation = async {
            let mut worker = slot.lock().await;
            if worker.is_none() {
                *worker = Some(
                    WorkerProcess::spawn(
                        &environment.python,
                        &revision,
                        &self.action_socket,
                        self.max_message_bytes,
                        self.timeout,
                        self.timeout,
                    )
                    .await?,
                );
            }
            let result = worker
                .as_mut()
                .expect("worker initialized")
                .invoke(event, &credential, self.max_message_bytes)
                .await;
            if matches!(
                result,
                Err(PythonError::Protocol(_) | PythonError::Io { .. })
            ) && let Some(mut failed) = worker.take()
            {
                failed.terminate().await;
            }
            result
        };
        let outcome = timeout(self.timeout, operation).await;
        let result = match outcome {
            Ok(result) => result,
            Err(_) => {
                if let Some(mut timed_out) = slot.lock().await.take() {
                    timed_out.terminate().await;
                }
                Err(PythonError::Timeout(self.timeout))
            }
        };
        self.retire_idle_superseded_workers(&key).await;
        cancellation_guard.disarm();
        result.map_err(|error| match error {
            error @ (PythonError::Invocation { .. } | PythonError::Timeout(_)) => error,
            other => PythonError::Invocation {
                hook: revision.id.clone(),
                revision: revision.revision.clone(),
                invocation_id,
                message: other.to_string(),
            },
        })
    }

    async fn retire_idle_superseded_workers(&self, current: &RevisionKey) {
        let retired = {
            let mut workers = self.workers.lock().await;
            let keys = workers
                .iter()
                .filter(|(key, slot)| {
                    key.0 == current.0 && key.1 != current.1 && Arc::strong_count(slot) == 1
                })
                .map(|(key, _)| key.clone())
                .collect::<Vec<_>>();
            keys.into_iter()
                .filter_map(|key| workers.remove(&key).map(|slot| (key, slot)))
                .collect::<Vec<_>>()
        };
        if retired.is_empty() {
            return;
        }
        let retired_keys = retired
            .iter()
            .map(|(key, _)| key.clone())
            .collect::<HashSet<_>>();
        self.environments_by_revision
            .lock()
            .await
            .retain(|key, _| !retired_keys.contains(key));
        for (_, slot) in retired {
            if let Some(mut worker) = slot.lock().await.take() {
                worker.terminate().await;
            }
        }
    }

    pub async fn shutdown(&self) {
        let slots = self
            .workers
            .lock()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for slot in slots {
            if let Some(mut worker) = slot.lock().await.take() {
                worker.terminate().await;
            }
        }
    }

    async fn validate_revision(
        &self,
        id: &HookId,
        source_dir: &Path,
    ) -> Result<HookMetadata, PythonError> {
        let deadline = Instant::now() + self.candidate_timeout;
        let environment = timeout_at(deadline, self.environments.prepare(source_dir))
            .await
            .map_err(|_| PythonError::CandidateTimeout {
                hook: id.clone(),
                timeout: self.candidate_timeout,
            })??;
        let revision = HookRevision {
            id: id.clone(),
            revision: "candidate".into(),
            source_dir: source_dir.to_owned(),
            modules_dir: source_dir
                .parent()
                .expect("candidate source has a revision parent")
                .join("modules"),
            hook_file: source_dir.join("hook.py"),
            requirements_file: source_dir
                .join("requirements.txt")
                .is_file()
                .then(|| source_dir.join("requirements.txt")),
            metadata: HookMetadata {
                function: String::new(),
                events: HashSet::new(),
                actions: HashSet::new(),
                blocking: false,
            },
        };
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(PythonError::CandidateTimeout {
                hook: id.clone(),
                timeout: self.candidate_timeout,
            });
        }
        let mut worker = WorkerProcess::spawn(
            &environment.python,
            &revision,
            &self.action_socket,
            self.max_message_bytes,
            remaining,
            self.timeout,
        )
        .await?;
        let metadata = worker.metadata.clone();
        worker.terminate().await;
        ActionGrant::from_names(metadata.actions.iter())
            .map_err(|error| PythonError::Protocol(error.to_string()))?;
        Ok(metadata)
    }
}

#[async_trait]
impl HookPreparer for PythonRuntime {
    async fn prepare(&self, id: &HookId, revision_source: &Path) -> Result<HookMetadata, String> {
        self.validate_revision(id, revision_source)
            .await
            .map_err(|error| error.to_string())
    }
}

struct WorkerProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    metadata: HookMetadata,
    hook_id: HookId,
    revision: String,
}

impl WorkerProcess {
    async fn spawn(
        python: &Path,
        revision: &HookRevision,
        socket: &Path,
        max_message_bytes: usize,
        handshake_timeout: std::time::Duration,
        request_timeout: std::time::Duration,
    ) -> Result<Self, PythonError> {
        let mut child = Command::new(python)
            .args([
                OsStr::new("-m"),
                OsStr::new("warden.worker"),
                OsStr::new("--hook"),
                revision.hook_file.as_os_str(),
                OsStr::new("--hook-name"),
                OsStr::new(revision.id.as_str()),
            ])
            .env("WARDEN_SOCKET", socket)
            .env("WARDEN_MODULES_ROOT", &revision.modules_dir)
            .env("WARDEN_MAX_MESSAGE_BYTES", max_message_bytes.to_string())
            .env(
                "WARDEN_REQUEST_TIMEOUT_SECONDS",
                request_timeout.as_secs_f64().max(0.001).to_string(),
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|source| io_error(python.to_owned(), source))?;
        let stdin = child.stdin.take().expect("worker stdin is piped");
        let mut stdout = BufReader::new(child.stdout.take().expect("worker stdout is piped"));
        let handshake = timeout(handshake_timeout, async {
            let line = read_bounded_line(&mut stdout, max_message_bytes).await?;
            serde_json::from_slice(&line).map_err(|error| {
                PythonError::Protocol(format!("invalid worker handshake: {error}"))
            })
        })
        .await;
        let handshake: WorkerHandshake = match handshake {
            Ok(Ok(handshake)) => handshake,
            Ok(Err(error)) => {
                terminate_child(&mut child).await;
                return Err(error);
            }
            Err(_) => {
                terminate_child(&mut child).await;
                return Err(PythonError::HandshakeTimeout {
                    hook: revision.id.clone(),
                    revision: revision.revision.clone(),
                    timeout: handshake_timeout,
                });
            }
        };
        if handshake.message_type != "handshake"
            || handshake.protocol_version != WORKER_PROTOCOL_VERSION
            || !handshake.ok
        {
            let _ = child.kill().await;
            return Err(PythonError::Handshake {
                hook: revision.id.clone(),
                revision: revision.revision.clone(),
                message: handshake
                    .error
                    .and_then(|error| {
                        error
                            .get("message")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    })
                    .unwrap_or_else(|| "worker rejected the hook".into()),
            });
        }
        let hook = handshake.hook.ok_or_else(|| {
            PythonError::Protocol("successful handshake has no hook metadata".into())
        })?;
        let metadata = HookMetadata {
            function: hook.function,
            events: hook.events.into_iter().collect(),
            actions: hook.actions.into_iter().collect(),
            blocking: hook.blocking,
        };
        Ok(Self {
            child,
            stdin,
            stdout,
            metadata,
            hook_id: revision.id.clone(),
            revision: revision.revision.clone(),
        })
    }

    async fn invoke(
        &mut self,
        event: HookEventEnvelope,
        credential: &InvocationCredential,
        max_message_bytes: usize,
    ) -> Result<HookInvocationResult, PythonError> {
        let id = credential.invocation_id.to_string();
        let message = json!({
            "type": "invoke",
            "protocol_version": WORKER_PROTOCOL_VERSION,
            "id": id,
            "event": event,
            "warden": {
                "invocation_id": credential.invocation_id,
                "token": credential.token,
            }
        });
        let mut encoded = serde_json::to_vec(&message)
            .map_err(|error| PythonError::Protocol(error.to_string()))?;
        if encoded.len() + 1 > max_message_bytes {
            return Err(PythonError::Protocol(
                "worker request exceeds message limit".into(),
            ));
        }
        encoded.push(b'\n');
        self.stdin
            .write_all(&encoded)
            .await
            .map_err(|source| io_error(PathBuf::from("worker stdin"), source))?;
        self.stdin
            .flush()
            .await
            .map_err(|source| io_error(PathBuf::from("worker stdin"), source))?;
        let mut stale_results = 0_u8;
        let result = loop {
            let line = read_bounded_line(&mut self.stdout, max_message_bytes).await?;
            let result: WorkerResult = serde_json::from_slice(&line).map_err(|error| {
                PythonError::Protocol(format!("invalid worker result: {error}"))
            })?;
            if result.message_type != "result" {
                return Err(PythonError::Protocol(format!(
                    "worker returned unexpected message type {:?} for invocation {id}",
                    result.message_type
                )));
            }
            if result.id.as_deref() == Some(&id) {
                break result;
            }
            stale_results = stale_results.saturating_add(1);
            tracing::warn!(
                hook = %self.hook_id,
                revision = %self.revision,
                expected_invocation = %id,
                stale_invocation = ?result.id,
                "discarding stale Python worker result from a cancelled invocation"
            );
            if stale_results >= 32 {
                return Err(PythonError::Protocol(format!(
                    "worker returned too many stale results while waiting for invocation {id}"
                )));
            }
        };
        if !result.ok {
            return Err(PythonError::Invocation {
                hook: self.hook_id.clone(),
                revision: self.revision.clone(),
                invocation_id: credential.invocation_id,
                message: result
                    .error
                    .and_then(|error| {
                        error
                            .get("message")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    })
                    .unwrap_or_else(|| "hook returned a failure".into()),
            });
        }
        Ok(HookInvocationResult {
            invocation_id: credential.invocation_id,
            result: result.result.unwrap_or(Value::Null),
            logs: result.logs.unwrap_or(Value::Null),
        })
    }

    async fn terminate(&mut self) {
        let shutdown = json!({"type":"shutdown","id":Uuid::new_v4().to_string()});
        if let Ok(mut bytes) = serde_json::to_vec(&shutdown) {
            bytes.push(b'\n');
            let _ = self.stdin.write_all(&bytes).await;
            let _ = self.stdin.flush().await;
        }
        if timeout(std::time::Duration::from_secs(2), self.child.wait())
            .await
            .is_err()
        {
            let _ = self.child.kill().await;
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
struct WorkerHandshake {
    #[serde(rename = "type")]
    message_type: String,
    protocol_version: u32,
    ok: bool,
    hook: Option<WorkerHookMetadata>,
    error: Option<Value>,
}

#[derive(Clone, Debug, Deserialize)]
struct WorkerHookMetadata {
    function: String,
    events: Vec<HookEventKind>,
    #[serde(default)]
    actions: Vec<String>,
    #[serde(default)]
    blocking: bool,
}

#[derive(Clone, Debug, Deserialize)]
struct WorkerResult {
    #[serde(rename = "type")]
    message_type: String,
    id: Option<String>,
    ok: bool,
    result: Option<Value>,
    logs: Option<Value>,
    error: Option<Value>,
}

#[derive(Clone, Debug)]
pub struct HookInvocationResult {
    pub invocation_id: Uuid,
    pub result: Value,
    pub logs: Value,
}

async fn read_bounded_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    max_bytes: usize,
) -> Result<Vec<u8>, PythonError> {
    let mut output = Vec::new();
    loop {
        let available = reader
            .fill_buf()
            .await
            .map_err(|source| io_error(PathBuf::from("worker stdout"), source))?;
        if available.is_empty() {
            return Err(PythonError::Protocol("worker closed stdout".into()));
        }
        let take = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        if output.len().saturating_add(take) > max_bytes {
            return Err(PythonError::Protocol("worker message exceeds limit".into()));
        }
        let complete = available[take - 1] == b'\n';
        output.extend_from_slice(&available[..take]);
        reader.consume(take);
        if complete {
            return Ok(output);
        }
    }
}

async fn run_command<'a>(
    program: &Path,
    arguments: impl IntoIterator<Item = &'a OsStr>,
    command_timeout: std::time::Duration,
    max_output_bytes: usize,
) -> Result<(), PythonError> {
    run_command_in(program, arguments, None, command_timeout, max_output_bytes).await
}

async fn run_command_in<'a>(
    program: &Path,
    arguments: impl IntoIterator<Item = &'a OsStr>,
    current_dir: Option<&Path>,
    command_timeout: std::time::Duration,
    max_output_bytes: usize,
) -> Result<(), PythonError> {
    let arguments = arguments
        .into_iter()
        .map(OsStr::to_owned)
        .collect::<Vec<_>>();
    let command_description = format!(
        "{} {}",
        program.display(),
        arguments
            .iter()
            .map(|argument| argument.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ")
    );
    let mut command = Command::new(program);
    command
        .args(&arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(current_dir) = current_dir {
        command.current_dir(current_dir);
    }
    let mut child = command
        .spawn()
        .map_err(|source| io_error(program.to_owned(), source))?;
    let stdout = child.stdout.take().expect("environment stdout is piped");
    let stderr = child.stderr.take().expect("environment stderr is piped");
    let outcome = timeout(command_timeout, async {
        tokio::try_join!(
            child.wait(),
            read_bounded_output(stdout, max_output_bytes),
            read_bounded_output(stderr, max_output_bytes),
        )
    })
    .await;
    let (status, stdout, stderr) = match outcome {
        Ok(Ok(output)) => output,
        Ok(Err(source)) => {
            terminate_child(&mut child).await;
            return Err(io_error(program.to_owned(), source));
        }
        Err(_) => {
            terminate_child(&mut child).await;
            return Err(PythonError::EnvironmentTimeout {
                command: command_description,
                timeout: command_timeout,
            });
        }
    };
    if stdout.exceeded {
        return Err(PythonError::EnvironmentOutputTooLarge {
            command: command_description,
            stream: "stdout",
            limit: max_output_bytes,
        });
    }
    if stderr.exceeded {
        return Err(PythonError::EnvironmentOutputTooLarge {
            command: command_description,
            stream: "stderr",
            limit: max_output_bytes,
        });
    }
    if status.success() {
        return Ok(());
    }
    Err(PythonError::EnvironmentCommand {
        command: command_description,
        status: status.code(),
        stderr: String::from_utf8_lossy(&stderr.bytes)
            .chars()
            .take(16 * 1024)
            .collect(),
    })
}

struct BoundedCommandOutput {
    bytes: Vec<u8>,
    exceeded: bool,
}

async fn read_bounded_output(
    mut reader: impl AsyncRead + Unpin,
    limit: usize,
) -> io::Result<BoundedCommandOutput> {
    let mut bytes = Vec::with_capacity(limit.min(8192));
    let mut exceeded = false;
    let mut buffer = [0_u8; 8192];
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        let retained = limit.saturating_sub(bytes.len()).min(count);
        bytes.extend_from_slice(&buffer[..retained]);
        exceeded |= retained < count;
    }
    Ok(BoundedCommandOutput { bytes, exceeded })
}

async fn terminate_child(child: &mut Child) {
    let _ = child.kill().await;
    let _ = child.wait().await;
}

fn environment_python(root: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        root.join("Scripts/python.exe")
    }
    #[cfg(not(windows))]
    {
        root.join("bin/python")
    }
}

fn hash_tree(root: &Path, directory: &Path, digest: &mut Sha256) -> Result<(), PythonError> {
    let mut entries = fs::read_dir(directory)
        .map_err(|source| io_error(directory.to_owned(), source))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| io_error(directory.to_owned(), source))?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let kind = entry
            .file_type()
            .map_err(|source| io_error(path.clone(), source))?;
        if kind.is_symlink() || entry.file_name() == OsStr::new("__pycache__") {
            continue;
        }
        if kind.is_dir() {
            hash_tree(root, &path, digest)?;
        } else if kind.is_file() {
            digest.update(
                path.strip_prefix(root)
                    .expect("SDK hash walk stays under root")
                    .as_os_str()
                    .as_encoded_bytes(),
            );
            digest.update([0]);
            digest.update(fs::read(&path).map_err(|source| io_error(path, source))?);
            digest.update([0]);
        }
    }
    Ok(())
}

fn io_error(path: PathBuf, source: io::Error) -> PythonError {
    PythonError::Io { path, source }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bounded_reader_rejects_overflow_before_newline() {
        let bytes = b"123456\n";
        let mut reader = BufReader::new(&bytes[..]);
        assert!(matches!(
            read_bounded_line(&mut reader, 4).await,
            Err(PythonError::Protocol(_))
        ));
    }

    #[test]
    fn older_worker_handshake_without_blocking_defaults_to_non_blocking() {
        let handshake: WorkerHandshake = serde_json::from_value(json!({
            "type": "handshake",
            "protocol_version": 1,
            "ok": true,
            "hook": {
                "function": "run",
                "events": ["turn_started"],
                "actions": []
            }
        }))
        .unwrap();

        assert!(!handshake.hook.unwrap().blocking);
    }
}
