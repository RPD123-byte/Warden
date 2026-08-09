use crate::registry::{ControlOperation, HookId, HookRegistry};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, VecDeque},
    fs, io,
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::RwLock;

const RECORD_VERSION: u32 = 1;
const MAX_DIAGNOSTICS: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ContinuousKey {
    pub hook: HookId,
    pub thread_id: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContinuousStatus {
    Running,
    Paused,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContinuousSession {
    pub key: ContinuousKey,
    pub status: ContinuousStatus,
    pub transitioned_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContinuousDiagnostic {
    pub hook: Option<HookId>,
    pub thread_id: String,
    pub occurred_at_ms: u64,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransitionOutcome {
    Changed(Option<ContinuousStatus>),
    Unchanged(Option<ContinuousStatus>),
    Rejected(String),
}

#[derive(Debug, thiserror::Error)]
pub enum ContinuousError {
    #[error("continuous-session I/O failed at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("continuous-session record at {path} is invalid: {message}")]
    Invalid { path: PathBuf, message: String },
}

#[derive(Serialize, Deserialize)]
struct StoredRecord {
    version: u32,
    session: ContinuousSession,
}

#[derive(Clone)]
pub struct ContinuousSessionStore {
    root: PathBuf,
    sessions: Arc<RwLock<HashMap<ContinuousKey, ContinuousSession>>>,
    diagnostics: Arc<RwLock<VecDeque<ContinuousDiagnostic>>>,
}

impl ContinuousSessionStore {
    pub fn load(root: PathBuf) -> Result<Self, ContinuousError> {
        fs::create_dir_all(&root).map_err(|source| io_error(root.clone(), source))?;
        let mut sessions = HashMap::new();
        let mut diagnostics = VecDeque::new();
        for entry in fs::read_dir(&root).map_err(|source| io_error(root.clone(), source))? {
            let entry = entry.map_err(|source| io_error(root.clone(), source))?;
            let path = entry.path();
            if !entry
                .file_type()
                .map_err(|source| io_error(path.clone(), source))?
                .is_file()
                || path.extension().and_then(|value| value.to_str()) != Some("json")
            {
                continue;
            }
            let loaded = fs::read(&path)
                .map_err(|source| io_error(path.clone(), source))
                .and_then(|bytes| {
                    serde_json::from_slice::<StoredRecord>(&bytes).map_err(|error| {
                        ContinuousError::Invalid {
                            path: path.clone(),
                            message: error.to_string(),
                        }
                    })
                });
            match loaded {
                Ok(record) if record.version == RECORD_VERSION => {
                    let expected = record_path(&root, &record.session.key);
                    if expected != path {
                        quarantine(&path)?;
                        push_diagnostic(
                            &mut diagnostics,
                            ContinuousDiagnostic {
                                hook: Some(record.session.key.hook),
                                thread_id: record.session.key.thread_id,
                                occurred_at_ms: now_ms(),
                                message: "continuous-session record had the wrong storage key"
                                    .into(),
                            },
                        );
                    } else {
                        sessions.insert(record.session.key.clone(), record.session);
                    }
                }
                Ok(record) => {
                    let session = record.session;
                    quarantine(&path)?;
                    push_diagnostic(
                        &mut diagnostics,
                        ContinuousDiagnostic {
                            hook: Some(session.key.hook),
                            thread_id: session.key.thread_id,
                            occurred_at_ms: now_ms(),
                            message: format!(
                                "unsupported continuous-session record version {}",
                                record.version
                            ),
                        },
                    );
                }
                Err(error) => {
                    quarantine(&path)?;
                    tracing::warn!(%error, "quarantined invalid continuous-session state");
                    push_diagnostic(
                        &mut diagnostics,
                        ContinuousDiagnostic {
                            hook: None,
                            thread_id: String::new(),
                            occurred_at_ms: now_ms(),
                            message: error.to_string().chars().take(1024).collect(),
                        },
                    );
                }
            }
        }
        Ok(Self {
            root,
            sessions: Arc::new(RwLock::new(sessions)),
            diagnostics: Arc::new(RwLock::new(diagnostics)),
        })
    }

    pub async fn transition(
        &self,
        hook: HookId,
        thread_id: &str,
        operation: ControlOperation,
        stateful: bool,
    ) -> Result<TransitionOutcome, ContinuousError> {
        let key = ContinuousKey {
            hook: hook.clone(),
            thread_id: thread_id.to_owned(),
        };
        if matches!(
            operation,
            ControlOperation::Pause | ControlOperation::Resume
        ) && !stateful
        {
            let message = "pause and resume are unavailable for a stateless hook".to_owned();
            self.note(hook, thread_id, message.clone()).await;
            return Ok(TransitionOutcome::Rejected(message));
        }

        let mut sessions = self.sessions.write().await;
        let current = sessions.get(&key).map(|record| record.status);
        let next = match (current, operation) {
            (None, ControlOperation::Start) => Some(ContinuousStatus::Running),
            (Some(ContinuousStatus::Paused), ControlOperation::Start) => {
                Some(ContinuousStatus::Running)
            }
            (Some(status), ControlOperation::Start) => Some(status),
            (Some(ContinuousStatus::Running), ControlOperation::Pause) => {
                Some(ContinuousStatus::Paused)
            }
            (Some(ContinuousStatus::Paused), ControlOperation::Pause) => {
                Some(ContinuousStatus::Paused)
            }
            (Some(ContinuousStatus::Paused), ControlOperation::Resume) => {
                Some(ContinuousStatus::Running)
            }
            (Some(ContinuousStatus::Running), ControlOperation::Resume) => {
                Some(ContinuousStatus::Running)
            }
            (None, ControlOperation::Resume) => {
                let message = "there is no paused continuous session to resume".to_owned();
                drop(sessions);
                self.note(hook, thread_id, message.clone()).await;
                return Ok(TransitionOutcome::Rejected(message));
            }
            (None, ControlOperation::Pause) => None,
            (_, ControlOperation::Stop) => None,
        };

        if next == current {
            return Ok(TransitionOutcome::Unchanged(current));
        }
        match next {
            Some(status) => {
                let session = ContinuousSession {
                    key: key.clone(),
                    status,
                    transitioned_at_ms: now_ms(),
                    last_error: None,
                };
                write_record(&self.root, &session)?;
                sessions.insert(key, session);
            }
            None => {
                remove_record(&self.root, &key)?;
                sessions.remove(&key);
            }
        }
        Ok(TransitionOutcome::Changed(next))
    }

    pub async fn running_for_thread(&self, thread_id: &str) -> Vec<HookId> {
        let mut hooks = self
            .sessions
            .read()
            .await
            .values()
            .filter(|session| {
                session.key.thread_id == thread_id && session.status == ContinuousStatus::Running
            })
            .map(|session| session.key.hook.clone())
            .collect::<Vec<_>>();
        hooks.sort();
        hooks
    }

    pub async fn remove_hook(&self, hook: &HookId) -> Result<usize, ContinuousError> {
        self.remove_where(|key| &key.hook == hook).await
    }

    pub async fn remove_thread(&self, thread_id: &str) -> Result<usize, ContinuousError> {
        self.remove_where(|key| key.thread_id == thread_id).await
    }

    async fn remove_where(
        &self,
        predicate: impl Fn(&ContinuousKey) -> bool,
    ) -> Result<usize, ContinuousError> {
        let mut sessions = self.sessions.write().await;
        let keys = sessions
            .keys()
            .filter(|key| predicate(key))
            .cloned()
            .collect::<Vec<_>>();
        for key in &keys {
            remove_record(&self.root, key)?;
            sessions.remove(key);
        }
        Ok(keys.len())
    }

    pub async fn reconcile(&self, registry: &HookRegistry) -> Result<usize, ContinuousError> {
        let revisions = registry.all().await;
        let capabilities = revisions
            .into_iter()
            .map(|revision| {
                (
                    revision.id.clone(),
                    revision.metadata.persistent_agent_sessions,
                )
            })
            .collect::<HashMap<_, _>>();
        let mut sessions = self.sessions.write().await;
        let keys = sessions
            .iter()
            .filter_map(|(key, session)| match capabilities.get(&key.hook) {
                None => Some(key.clone()),
                Some(false) if session.status == ContinuousStatus::Paused => Some(key.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        for key in &keys {
            remove_record(&self.root, key)?;
            sessions.remove(key);
        }
        Ok(keys.len())
    }

    pub async fn sessions(&self) -> Vec<ContinuousSession> {
        let mut sessions = self
            .sessions
            .read()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        sessions.sort_by(|left, right| {
            left.key
                .hook
                .cmp(&right.key.hook)
                .then_with(|| left.key.thread_id.cmp(&right.key.thread_id))
        });
        sessions
    }

    pub async fn diagnostics(&self) -> Vec<ContinuousDiagnostic> {
        self.diagnostics.read().await.iter().cloned().collect()
    }

    pub async fn note(&self, hook: HookId, thread_id: &str, message: String) {
        let message = message.chars().take(1024).collect::<String>();
        let key = ContinuousKey {
            hook: hook.clone(),
            thread_id: thread_id.to_owned(),
        };
        {
            let mut sessions = self.sessions.write().await;
            if let Some(session) = sessions.get_mut(&key) {
                session.last_error = Some(message.clone());
                if let Err(error) = write_record(&self.root, session) {
                    tracing::warn!(%error, "could not persist continuous-session diagnostic");
                }
            }
        }
        let diagnostic = ContinuousDiagnostic {
            hook: Some(hook),
            thread_id: thread_id.to_owned(),
            occurred_at_ms: now_ms(),
            message,
        };
        let mut diagnostics = self.diagnostics.write().await;
        push_diagnostic(&mut diagnostics, diagnostic);
    }
}

fn push_diagnostic(
    diagnostics: &mut VecDeque<ContinuousDiagnostic>,
    diagnostic: ContinuousDiagnostic,
) {
    if diagnostics.len() == MAX_DIAGNOSTICS {
        diagnostics.pop_front();
    }
    diagnostics.push_back(diagnostic);
}

fn record_path(root: &Path, key: &ContinuousKey) -> PathBuf {
    let mut digest = Sha256::new();
    digest.update(key.hook.as_str().as_bytes());
    digest.update([0]);
    digest.update(key.thread_id.as_bytes());
    root.join(format!("{}.json", hex::encode(digest.finalize())))
}

fn write_record(root: &Path, session: &ContinuousSession) -> Result<(), ContinuousError> {
    let path = record_path(root, &session.key);
    let temporary = root.join(format!(".session-{}.json", uuid::Uuid::new_v4()));
    let bytes = serde_json::to_vec_pretty(&StoredRecord {
        version: RECORD_VERSION,
        session: session.clone(),
    })
    .expect("continuous-session records serialize");
    fs::write(&temporary, bytes).map_err(|source| io_error(temporary.clone(), source))?;
    fs::File::open(&temporary)
        .and_then(|file| file.sync_all())
        .map_err(|source| io_error(temporary.clone(), source))?;
    fs::rename(&temporary, &path).map_err(|source| io_error(path, source))?;
    sync_directory(root)
}

fn remove_record(root: &Path, key: &ContinuousKey) -> Result<(), ContinuousError> {
    let path = record_path(root, key);
    match fs::remove_file(&path) {
        Ok(()) => sync_directory(root),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(io_error(path, source)),
    }
}

fn quarantine(path: &Path) -> Result<(), ContinuousError> {
    let target = path.with_extension(format!("invalid-{}", uuid::Uuid::new_v4()));
    fs::rename(path, &target).map_err(|source| io_error(path.to_owned(), source))
}

fn sync_directory(path: &Path) -> Result<(), ContinuousError> {
    #[cfg(unix)]
    fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| io_error(path.to_owned(), source))?;
    Ok(())
}

fn io_error(path: PathBuf, source: io::Error) -> ContinuousError {
    ContinuousError::Io { path, source }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn id() -> HookId {
        HookId::parse("demo").unwrap()
    }

    #[tokio::test]
    async fn transitions_are_durable_and_task_scoped() {
        let temp = TempDir::new().unwrap();
        let store = ContinuousSessionStore::load(temp.path().to_owned()).unwrap();
        assert!(matches!(
            store
                .transition(id(), "one", ControlOperation::Start, false)
                .await
                .unwrap(),
            TransitionOutcome::Changed(Some(ContinuousStatus::Running))
        ));
        store
            .transition(id(), "two", ControlOperation::Start, true)
            .await
            .unwrap();
        store
            .transition(id(), "two", ControlOperation::Pause, true)
            .await
            .unwrap();

        let restored = ContinuousSessionStore::load(temp.path().to_owned()).unwrap();
        assert_eq!(restored.running_for_thread("one").await, [id()]);
        assert!(restored.running_for_thread("two").await.is_empty());
        assert_eq!(restored.sessions().await.len(), 2);
    }

    #[tokio::test]
    async fn stateless_pause_and_missing_resume_are_rejected() {
        let temp = TempDir::new().unwrap();
        let store = ContinuousSessionStore::load(temp.path().to_owned()).unwrap();
        assert!(matches!(
            store
                .transition(id(), "one", ControlOperation::Pause, false)
                .await
                .unwrap(),
            TransitionOutcome::Rejected(_)
        ));
        assert!(matches!(
            store
                .transition(id(), "one", ControlOperation::Resume, true)
                .await
                .unwrap(),
            TransitionOutcome::Rejected(_)
        ));
        assert_eq!(store.diagnostics().await.len(), 2);
    }

    #[tokio::test]
    async fn stop_is_idempotent_and_targeted_cleanup_never_crosses_tasks() {
        let temp = TempDir::new().unwrap();
        let store = ContinuousSessionStore::load(temp.path().to_owned()).unwrap();
        for thread in ["one", "two"] {
            store
                .transition(id(), thread, ControlOperation::Start, false)
                .await
                .unwrap();
        }
        assert_eq!(store.remove_thread("one").await.unwrap(), 1);
        assert_eq!(store.running_for_thread("two").await, [id()]);
        assert!(matches!(
            store
                .transition(id(), "one", ControlOperation::Stop, false)
                .await
                .unwrap(),
            TransitionOutcome::Unchanged(None)
        ));
        assert_eq!(store.remove_hook(&id()).await.unwrap(), 1);
        assert!(store.sessions().await.is_empty());
    }

    #[test]
    fn malformed_records_are_quarantined_without_inventing_state() {
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join("broken.json"), b"not json").unwrap();
        let store = ContinuousSessionStore::load(temp.path().to_owned()).unwrap();
        assert!(store.sessions.blocking_read().is_empty());
        assert_eq!(store.diagnostics.blocking_read().len(), 1);
        assert!(!temp.path().join("broken.json").exists());
        assert!(fs::read_dir(temp.path()).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("invalid")
        }));
    }

    #[tokio::test]
    async fn diagnostics_are_count_and_message_bounded() {
        let temp = TempDir::new().unwrap();
        let store = ContinuousSessionStore::load(temp.path().to_owned()).unwrap();
        for index in 0..80 {
            store
                .note(id(), "one", format!("{index}:{}", "x".repeat(2048)))
                .await;
        }
        let diagnostics = store.diagnostics().await;
        assert_eq!(diagnostics.len(), MAX_DIAGNOSTICS);
        assert!(
            diagnostics
                .iter()
                .all(|entry| entry.message.chars().count() == 1024)
        );
    }
}
