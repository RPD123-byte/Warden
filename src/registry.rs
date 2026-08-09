use crate::event::HookEventKind;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    ffi::OsStr,
    fs, io,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::{Mutex, RwLock};

pub const MARKER_BODY: &str =
    "This skill is an activation marker for the local Warden service. Ignore";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlOperation {
    Start,
    Pause,
    Resume,
    Stop,
}

impl ControlOperation {
    fn suffix(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Pause => "pause",
            Self::Resume => "resume",
            Self::Stop => "stop",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MarkerIntent {
    Primary(HookId),
    Control {
        hook: HookId,
        operation: ControlOperation,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HookId(String);

impl HookId {
    pub fn parse(value: impl Into<String>) -> Result<Self, RegistryError> {
        let value = value.into();
        let valid = !value.is_empty()
            && value.len() <= 128
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'));
        if valid {
            Ok(Self(value))
        } else {
            Err(RegistryError::InvalidHookId(value))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for HookId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookMetadata {
    pub function: String,
    pub events: HashSet<HookEventKind>,
    #[serde(default)]
    pub actions: HashSet<String>,
    #[serde(default)]
    pub blocking: bool,
    #[serde(default)]
    pub persistent_agent_sessions: bool,
}

#[derive(Clone, Debug)]
pub struct HookRevision {
    pub id: HookId,
    pub revision: String,
    pub source_dir: PathBuf,
    pub modules_dir: PathBuf,
    pub hook_file: PathBuf,
    pub requirements_file: Option<PathBuf>,
    pub metadata: HookMetadata,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RegistryDelta {
    pub published: Vec<HookId>,
    pub removed: Vec<HookId>,
    pub unchanged: Vec<HookId>,
    pub failed: Vec<(HookId, String)>,
}

#[derive(Debug, Deserialize, Serialize)]
struct CurrentRevisionManifest {
    revision: String,
}

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("invalid hook identity {0:?}; use only letters, numbers, '-' and '_'")]
    InvalidHookId(String),
    #[error("I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("hook {hook} validation failed: {message}")]
    Validation { hook: HookId, message: String },
}

#[async_trait]
pub trait HookPreparer: Send + Sync + 'static {
    async fn prepare(&self, id: &HookId, revision_source: &Path) -> Result<HookMetadata, String>;
}

#[derive(Clone)]
pub struct HookRegistry {
    hooks_root: PathBuf,
    modules_root: PathBuf,
    generated_skills_root: PathBuf,
    revisions_root: PathBuf,
    preparer: Arc<dyn HookPreparer>,
    refresh_lock: Arc<Mutex<()>>,
    current: Arc<RwLock<HashMap<HookId, Arc<HookRevision>>>>,
    failures: Arc<RwLock<HashMap<HookId, String>>>,
    marker_catalog: Arc<RwLock<HashMap<String, MarkerIntent>>>,
}

impl HookRegistry {
    pub fn new(
        hooks_root: PathBuf,
        modules_root: PathBuf,
        generated_skills_root: PathBuf,
        runtimes_root: PathBuf,
        preparer: Arc<dyn HookPreparer>,
    ) -> Self {
        Self {
            hooks_root,
            modules_root,
            generated_skills_root,
            revisions_root: runtimes_root.join("revisions"),
            preparer,
            refresh_lock: Arc::new(Mutex::new(())),
            current: Arc::new(RwLock::new(HashMap::new())),
            failures: Arc::new(RwLock::new(HashMap::new())),
            marker_catalog: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn current(&self, id: &HookId) -> Option<Arc<HookRevision>> {
        self.current.read().await.get(id).cloned()
    }

    pub async fn all(&self) -> Vec<Arc<HookRevision>> {
        let mut revisions = self
            .current
            .read()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        revisions.sort_by(|left, right| left.id.cmp(&right.id));
        revisions
    }

    pub async fn failures(&self) -> HashMap<HookId, String> {
        self.failures.read().await.clone()
    }

    pub async fn marker_intent(&self, name: &str) -> Option<MarkerIntent> {
        self.marker_catalog.read().await.get(name).cloned()
    }

    /// Scans authored hook directories and atomically publishes only fully prepared revisions.
    /// Existing `Arc<HookRevision>` values remain valid for in-flight activations.
    pub async fn refresh(&self) -> Result<RegistryDelta, RegistryError> {
        // A refresh is one publication transaction. Besides avoiding duplicate preparation work,
        // serialization prevents two refreshes from racing marker removal against candidate
        // publication or trying to publish the same immutable revision concurrently.
        let _refresh_guard = self.refresh_lock.lock().await;
        create_dir(&self.hooks_root)?;
        create_dir(&self.modules_root)?;
        create_dir(&self.generated_skills_root)?;
        create_dir(&self.revisions_root)?;
        self.restore_current_manifests().await?;

        let candidates = discover(&self.hooks_root)?;
        let candidate_ids = candidates.keys().cloned().collect::<HashSet<_>>();
        let existing_ids = self
            .current
            .read()
            .await
            .keys()
            .cloned()
            .collect::<HashSet<_>>();
        let reserved_by_current = {
            let current = self.current.read().await;
            control_marker_owners(&current, &candidate_ids)
        };
        let mut delta = RegistryDelta::default();

        for (id, authored_dir) in candidates {
            if let Some(owner) = reserved_by_current.get(id.as_str())
                && owner != &id
            {
                let message = format!("hook identity collides with a control marker for {owner}");
                self.failures
                    .write()
                    .await
                    .insert(id.clone(), message.clone());
                delta.failed.push((id, message));
                continue;
            }
            if let Some(message) = base_control_collision(&id, &candidate_ids) {
                self.failures
                    .write()
                    .await
                    .insert(id.clone(), message.clone());
                delta.failed.push((id, message));
                continue;
            }
            // Copy first, then derive identity and validate from that private snapshot. Authored
            // files remain live and may change at any time; they are never used after this point
            // in the candidate transaction.
            let snapshot = CandidateSnapshot::create(
                &self.revisions_root,
                &id,
                &authored_dir,
                &self.modules_root,
            )?;
            let revision = snapshot.revision()?;
            if self
                .current(&id)
                .await
                .is_some_and(|current| current.revision == revision)
            {
                delta.unchanged.push(id);
                continue;
            }

            match self.prepare_candidate(&id, &revision, snapshot).await {
                Ok(candidate) => {
                    if let Some(message) =
                        stateful_control_collision(&id, &candidate.metadata, &candidate_ids)
                    {
                        self.failures
                            .write()
                            .await
                            .insert(id.clone(), message.clone());
                        delta.failed.push((id, message));
                        continue;
                    }
                    persist_current_manifest(&self.revisions_root, &candidate)?;
                    self.current
                        .write()
                        .await
                        .insert(id.clone(), Arc::new(candidate));
                    self.failures.write().await.remove(&id);
                    delta.published.push(id);
                }
                Err(error) => {
                    let message = error.to_string();
                    self.failures
                        .write()
                        .await
                        .insert(id.clone(), message.clone());
                    delta.failed.push((id, message));
                }
            }
        }

        for id in existing_ids.difference(&candidate_ids) {
            self.current.write().await.remove(id);
            self.failures.write().await.remove(id);
            remove_current_manifest(&self.revisions_root, id)?;
            delta.removed.push(id.clone());
        }
        self.reconcile_markers().await?;
        delta.published.sort();
        delta.removed.sort();
        delta.unchanged.sort();
        Ok(delta)
    }

    async fn restore_current_manifests(&self) -> Result<(), RegistryError> {
        let entries = fs::read_dir(&self.revisions_root)
            .map_err(|source| io_error(self.revisions_root.clone(), source))?;
        for entry in entries {
            let entry = entry.map_err(|source| io_error(self.revisions_root.clone(), source))?;
            if !entry
                .file_type()
                .map_err(|source| io_error(entry.path(), source))?
                .is_dir()
            {
                continue;
            }
            let Ok(id) = HookId::parse(entry.file_name().to_string_lossy().into_owned()) else {
                continue;
            };
            if self.current(&id).await.is_some() {
                continue;
            }
            match self.restore_current_manifest(&id, &entry.path()).await {
                Ok(Some(revision)) => {
                    self.current
                        .write()
                        .await
                        .insert(id.clone(), Arc::new(revision));
                    self.failures.write().await.remove(&id);
                }
                Ok(None) => {}
                Err(error) => {
                    self.failures.write().await.insert(id, error.to_string());
                }
            }
        }
        Ok(())
    }

    async fn restore_current_manifest(
        &self,
        id: &HookId,
        hook_revisions_root: &Path,
    ) -> Result<Option<HookRevision>, RegistryError> {
        let manifest_path = hook_revisions_root.join("current.json");
        let bytes = match fs::read(&manifest_path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(io_error(manifest_path, source)),
        };
        let manifest: CurrentRevisionManifest =
            serde_json::from_slice(&bytes).map_err(|error| RegistryError::Validation {
                hook: id.clone(),
                message: format!("invalid current revision manifest: {error}"),
            })?;
        if manifest.revision.len() != 64
            || !manifest
                .revision
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(RegistryError::Validation {
                hook: id.clone(),
                message: "current revision manifest contains an invalid revision identity".into(),
            });
        }
        let revision_root = hook_revisions_root.join(&manifest.revision);
        let source_dir = revision_root.join("source");
        let modules_dir = revision_root.join("modules");
        let actual = hash_candidate(&source_dir, &modules_dir)?;
        if actual != manifest.revision || !source_dir.join("hook.py").is_file() {
            return Err(RegistryError::Validation {
                hook: id.clone(),
                message: format!(
                    "saved current revision {} failed its content integrity check",
                    manifest.revision
                ),
            });
        }
        let metadata = self
            .preparer
            .prepare(id, &source_dir)
            .await
            .map_err(|message| RegistryError::Validation {
                hook: id.clone(),
                message: format!("saved current revision could not be restored: {message}"),
            })?;
        Ok(Some(build_revision(
            id,
            &manifest.revision,
            source_dir,
            metadata,
        )))
    }

    async fn reconcile_markers(&self) -> Result<(), RegistryError> {
        let revisions = self.all().await;
        let mut desired = HashMap::new();
        for revision in revisions {
            insert_marker(
                &mut desired,
                revision.id.as_str().to_owned(),
                MarkerIntent::Primary(revision.id.clone()),
            )?;
            for operation in [ControlOperation::Start, ControlOperation::Stop] {
                insert_control_marker(&mut desired, &revision.id, operation)?;
            }
            if revision.metadata.persistent_agent_sessions {
                for operation in [ControlOperation::Pause, ControlOperation::Resume] {
                    insert_control_marker(&mut desired, &revision.id, operation)?;
                }
            }
        }

        let existing = discover_marker_ids(&self.generated_skills_root)?;
        for (name, intent) in &desired {
            ensure_marker(&self.generated_skills_root, name, intent)?;
        }
        let desired_ids = desired
            .keys()
            .filter_map(|name| HookId::parse(name.clone()).ok())
            .collect::<HashSet<_>>();
        for stale in existing.difference(&desired_ids) {
            remove_marker(&self.generated_skills_root, stale.as_str())?;
        }
        *self.marker_catalog.write().await = desired;
        Ok(())
    }

    async fn prepare_candidate(
        &self,
        id: &HookId,
        revision: &str,
        snapshot: CandidateSnapshot,
    ) -> Result<HookRevision, RegistryError> {
        let final_dir = self.revisions_root.join(id.as_str()).join(revision);
        let metadata = self
            .preparer
            .prepare(id, snapshot.source_dir())
            .await
            .map_err(|message| RegistryError::Validation {
                hook: id.clone(),
                message,
            })?;

        // Validation imports and executes user Python. Detect any candidate-byte mutation before
        // publication so metadata can never describe bytes other than the revision identity.
        let validated_revision = snapshot.revision()?;
        if validated_revision != revision {
            return Err(RegistryError::Validation {
                hook: id.clone(),
                message: format!(
                    "candidate snapshot changed during validation (expected {revision}, found {validated_revision})"
                ),
            });
        }

        create_dir(final_dir.parent().expect("revision has hook parent"))?;
        let installed_by_this_refresh = snapshot.publish(&final_dir)?;
        let source_dir = final_dir.join("source");
        let installed_revision = hash_candidate(&source_dir, &final_dir.join("modules"))?;
        if installed_revision != revision || !source_dir.join("hook.py").is_file() {
            // When another process populated the content-addressed destination first, refuse to
            // trust it unless its bytes are exactly the snapshot we validated. A directory we
            // just renamed is checked too, making the publication invariant explicit.
            return Err(RegistryError::Validation {
                hook: id.clone(),
                message: format!(
                    "published revision integrity check failed for {revision}{}",
                    if installed_by_this_refresh {
                        ""
                    } else {
                        " (destination already existed)"
                    }
                ),
            });
        }
        Ok(build_revision(id, revision, source_dir, metadata))
    }
}

/// A private, content-addressed publication candidate. Cleanup-on-drop ensures failed validation
/// and unchanged candidates do not accumulate staging directories.
struct CandidateSnapshot {
    root: PathBuf,
    source_dir: PathBuf,
    modules_dir: PathBuf,
    cleanup_on_drop: bool,
}

impl CandidateSnapshot {
    fn create(
        revisions_root: &Path,
        id: &HookId,
        authored_dir: &Path,
        modules_root: &Path,
    ) -> Result<Self, RegistryError> {
        let parent = revisions_root.join(id.as_str());
        create_dir(&parent)?;
        let root = parent.join(format!(".candidate-{}", uuid::Uuid::new_v4()));
        let snapshot = Self {
            source_dir: root.join("source"),
            modules_dir: root.join("modules"),
            root,
            cleanup_on_drop: true,
        };
        copy_tree(authored_dir, snapshot.source_dir())?;
        copy_tree(modules_root, snapshot.modules_dir())?;
        Ok(snapshot)
    }

    fn source_dir(&self) -> &Path {
        &self.source_dir
    }

    fn modules_dir(&self) -> &Path {
        &self.modules_dir
    }

    fn revision(&self) -> Result<String, RegistryError> {
        hash_candidate(self.source_dir(), self.modules_dir())
    }

    /// Atomically installs the complete snapshot. Returns `true` when this call performed the
    /// rename and `false` when a content-addressed destination already existed.
    fn publish(mut self, destination: &Path) -> Result<bool, RegistryError> {
        match fs::rename(&self.root, destination) {
            Ok(()) => {
                self.cleanup_on_drop = false;
                Ok(true)
            }
            Err(_) if destination.is_dir() => Ok(false),
            Err(source) => Err(io_error(destination.to_owned(), source)),
        }
    }
}

impl Drop for CandidateSnapshot {
    fn drop(&mut self) {
        if self.cleanup_on_drop {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}

fn build_revision(
    id: &HookId,
    revision: &str,
    source_dir: PathBuf,
    metadata: HookMetadata,
) -> HookRevision {
    let requirements = source_dir.join("requirements.txt");
    let modules_dir = source_dir
        .parent()
        .expect("revision source has a revision parent")
        .join("modules");
    HookRevision {
        id: id.clone(),
        revision: revision.to_owned(),
        hook_file: source_dir.join("hook.py"),
        requirements_file: requirements.is_file().then_some(requirements),
        source_dir,
        modules_dir,
        metadata,
    }
}

fn discover(root: &Path) -> Result<HashMap<HookId, PathBuf>, RegistryError> {
    let mut hooks = HashMap::new();
    for entry in fs::read_dir(root).map_err(|source| io_error(root.to_owned(), source))? {
        let entry = entry.map_err(|source| io_error(root.to_owned(), source))?;
        if !entry
            .file_type()
            .map_err(|source| io_error(entry.path(), source))?
            .is_dir()
            || !entry.path().join("hook.py").is_file()
        {
            continue;
        }
        let id = HookId::parse(entry.file_name().to_string_lossy().into_owned())?;
        hooks.insert(id, entry.path());
    }
    Ok(hooks)
}

fn discover_marker_ids(root: &Path) -> Result<HashSet<HookId>, RegistryError> {
    let mut ids = HashSet::new();
    for entry in fs::read_dir(root).map_err(|source| io_error(root.to_owned(), source))? {
        let entry = entry.map_err(|source| io_error(root.to_owned(), source))?;
        if entry
            .file_type()
            .map_err(|source| io_error(entry.path(), source))?
            .is_dir()
            && entry.path().join("SKILL.md").is_file()
            && let Ok(id) = HookId::parse(entry.file_name().to_string_lossy().into_owned())
        {
            ids.insert(id);
        }
    }
    Ok(ids)
}

fn hash_candidate(root: &Path, modules_root: &Path) -> Result<String, RegistryError> {
    let mut files = Vec::new();
    collect_files(root, root, &mut files)?;
    files.sort_by(|left, right| left.0.cmp(&right.0));
    let mut digest = Sha256::new();
    digest.update(b"warden-hook-revision-v1\0");
    for (relative, absolute) in files {
        digest.update(b"hook\0");
        digest.update(relative.to_string_lossy().as_bytes());
        digest.update([0]);
        digest.update(fs::read(&absolute).map_err(|source| io_error(absolute, source))?);
        digest.update([0]);
    }
    let mut modules = Vec::new();
    collect_files(modules_root, modules_root, &mut modules)?;
    modules.sort_by(|left, right| left.0.cmp(&right.0));
    for (relative, absolute) in modules {
        digest.update(b"module\0");
        digest.update(relative.to_string_lossy().as_bytes());
        digest.update([0]);
        digest.update(fs::read(&absolute).map_err(|source| io_error(absolute, source))?);
        digest.update([0]);
    }
    Ok(hex::encode(digest.finalize()))
}

fn collect_files(
    root: &Path,
    directory: &Path,
    output: &mut Vec<(PathBuf, PathBuf)>,
) -> Result<(), RegistryError> {
    for entry in fs::read_dir(directory).map_err(|source| io_error(directory.to_owned(), source))? {
        let entry = entry.map_err(|source| io_error(directory.to_owned(), source))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|source| io_error(path.clone(), source))?;
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            if !ignored_name(&entry.file_name()) {
                collect_files(root, &path, output)?;
            }
        } else if file_type.is_file() {
            if ignored_name(&entry.file_name()) {
                continue;
            }
            output.push((
                path.strip_prefix(root)
                    .expect("walk remains under root")
                    .to_owned(),
                path,
            ));
        }
    }
    Ok(())
}

fn copy_tree(source: &Path, destination: &Path) -> Result<(), RegistryError> {
    create_dir(destination)?;
    for entry in fs::read_dir(source).map_err(|error| io_error(source.to_owned(), error))? {
        let entry = entry.map_err(|error| io_error(source.to_owned(), error))?;
        let target = destination.join(entry.file_name());
        let kind = entry
            .file_type()
            .map_err(|error| io_error(entry.path(), error))?;
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            if !ignored_name(&entry.file_name()) {
                copy_tree(&entry.path(), &target)?;
            }
        } else if kind.is_file() {
            if ignored_name(&entry.file_name()) {
                continue;
            }
            fs::copy(entry.path(), &target).map_err(|error| io_error(target, error))?;
        }
    }
    Ok(())
}

fn ignored_name(name: &OsStr) -> bool {
    matches!(
        name.to_str(),
        Some("__pycache__" | ".pytest_cache" | ".ruff_cache" | ".git" | ".DS_Store")
    ) || name.to_string_lossy().ends_with(".pyc")
}

fn marker_name(id: &HookId, operation: ControlOperation) -> String {
    format!("{}-{}", id.as_str(), operation.suffix())
}

fn insert_control_marker(
    catalog: &mut HashMap<String, MarkerIntent>,
    hook: &HookId,
    operation: ControlOperation,
) -> Result<(), RegistryError> {
    insert_marker(
        catalog,
        marker_name(hook, operation),
        MarkerIntent::Control {
            hook: hook.clone(),
            operation,
        },
    )
}

fn insert_marker(
    catalog: &mut HashMap<String, MarkerIntent>,
    name: String,
    intent: MarkerIntent,
) -> Result<(), RegistryError> {
    if let Some(existing) = catalog.insert(name.clone(), intent.clone()) {
        let hook = match intent {
            MarkerIntent::Primary(hook) | MarkerIntent::Control { hook, .. } => hook,
        };
        catalog.insert(name.clone(), existing);
        return Err(RegistryError::Validation {
            hook,
            message: format!("generated marker name {name} collides with another hook"),
        });
    }
    Ok(())
}

fn control_marker_owners(
    revisions: &HashMap<HookId, Arc<HookRevision>>,
    retained_ids: &HashSet<HookId>,
) -> HashMap<String, HookId> {
    let mut owners = HashMap::new();
    for revision in revisions
        .values()
        .filter(|revision| retained_ids.contains(&revision.id))
    {
        for operation in [ControlOperation::Start, ControlOperation::Stop] {
            owners.insert(marker_name(&revision.id, operation), revision.id.clone());
        }
        if revision.metadata.persistent_agent_sessions {
            for operation in [ControlOperation::Pause, ControlOperation::Resume] {
                owners.insert(marker_name(&revision.id, operation), revision.id.clone());
            }
        }
    }
    owners
}

fn base_control_collision(id: &HookId, candidate_ids: &HashSet<HookId>) -> Option<String> {
    [ControlOperation::Start, ControlOperation::Stop]
        .into_iter()
        .map(|operation| marker_name(id, operation))
        .find(|name| {
            candidate_ids
                .iter()
                .any(|candidate| candidate.as_str() == name)
        })
        .map(|name| format!("generated marker name {name} collides with an authored hook"))
}

fn stateful_control_collision(
    id: &HookId,
    metadata: &HookMetadata,
    candidate_ids: &HashSet<HookId>,
) -> Option<String> {
    metadata.persistent_agent_sessions.then_some(())?;
    [ControlOperation::Pause, ControlOperation::Resume]
        .into_iter()
        .map(|operation| marker_name(id, operation))
        .find(|name| {
            candidate_ids
                .iter()
                .any(|candidate| candidate.as_str() == name)
        })
        .map(|name| format!("generated marker name {name} collides with an authored hook"))
}

fn marker_content(name: &str, intent: &MarkerIntent) -> String {
    let description = match intent {
        MarkerIntent::Primary(hook) => format!("Activate the {hook} Warden hook for this turn."),
        MarkerIntent::Control { hook, operation } => format!(
            "{} continuous activation of the {hook} Warden hook for this task.",
            match operation {
                ControlOperation::Start => "Start",
                ControlOperation::Pause => "Pause",
                ControlOperation::Resume => "Resume",
                ControlOperation::Stop => "Stop",
            }
        ),
    };
    format!(
        "---\nname: {name}\ndescription: {description}\n---\n\n{}\n",
        MARKER_BODY
    )
}

fn ensure_marker(root: &Path, name: &str, intent: &MarkerIntent) -> Result<(), RegistryError> {
    let directory = root.join(name);
    create_dir(&directory)?;
    let path = directory.join("SKILL.md");
    let wanted = marker_content(name, intent);
    if fs::read_to_string(&path).ok().as_deref() == Some(&wanted) {
        return Ok(());
    }
    if path.exists()
        && !fs::read_to_string(&path)
            .ok()
            .is_some_and(|existing| existing.contains(MARKER_BODY))
    {
        let hook = match intent {
            MarkerIntent::Primary(hook) | MarkerIntent::Control { hook, .. } => hook.clone(),
        };
        return Err(RegistryError::Validation {
            hook,
            message: format!("refusing to overwrite non-Warden marker {name}"),
        });
    }
    let temporary = directory.join(format!(".SKILL.md.{}", uuid::Uuid::new_v4()));
    fs::write(&temporary, wanted).map_err(|source| io_error(temporary.clone(), source))?;
    fs::rename(&temporary, &path).map_err(|source| io_error(path, source))
}

fn remove_marker(root: &Path, name: &str) -> Result<(), RegistryError> {
    let directory = root.join(name);
    if !directory.exists() {
        return Ok(());
    }
    let tombstone = root.join(format!(".removed-{name}-{}", uuid::Uuid::new_v4()));
    fs::rename(&directory, &tombstone).map_err(|source| io_error(directory, source))?;
    fs::remove_dir_all(&tombstone).map_err(|source| io_error(tombstone, source))
}

fn persist_current_manifest(
    revisions_root: &Path,
    revision: &HookRevision,
) -> Result<(), RegistryError> {
    let parent = revisions_root.join(revision.id.as_str());
    create_dir(&parent)?;
    let path = parent.join("current.json");
    let temporary = parent.join(format!(".current-{}.json", uuid::Uuid::new_v4()));
    let bytes = serde_json::to_vec(&CurrentRevisionManifest {
        revision: revision.revision.clone(),
    })
    .map_err(|error| RegistryError::Validation {
        hook: revision.id.clone(),
        message: format!("could not encode current revision manifest: {error}"),
    })?;
    fs::write(&temporary, bytes).map_err(|source| io_error(temporary.clone(), source))?;
    fs::File::open(&temporary)
        .and_then(|file| file.sync_all())
        .map_err(|source| io_error(temporary.clone(), source))?;
    fs::rename(&temporary, &path).map_err(|source| io_error(path, source))?;
    sync_directory(&parent)
}

fn remove_current_manifest(revisions_root: &Path, id: &HookId) -> Result<(), RegistryError> {
    let parent = revisions_root.join(id.as_str());
    let path = parent.join("current.json");
    match fs::remove_file(&path) {
        Ok(()) => sync_directory(&parent),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(io_error(path, source)),
    }
}

fn sync_directory(path: &Path) -> Result<(), RegistryError> {
    #[cfg(unix)]
    {
        fs::File::open(path)
            .and_then(|directory| directory.sync_all())
            .map_err(|source| io_error(path.to_owned(), source))?;
    }
    Ok(())
}

fn create_dir(path: &Path) -> Result<(), RegistryError> {
    fs::create_dir_all(path).map_err(|source| io_error(path.to_owned(), source))
}

fn io_error(path: PathBuf, source: io::Error) -> RegistryError {
    RegistryError::Io { path, source }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::{
            Mutex as StdMutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };
    use tempfile::TempDir;
    use tokio::sync::{Notify, Semaphore};

    struct Preparer;

    fn metadata() -> HookMetadata {
        HookMetadata {
            function: "run".into(),
            events: HashSet::from([HookEventKind::UserPromptSubmitted]),
            actions: HashSet::new(),
            blocking: false,
            persistent_agent_sessions: false,
        }
    }

    #[async_trait]
    impl HookPreparer for Preparer {
        async fn prepare(&self, _id: &HookId, source: &Path) -> Result<HookMetadata, String> {
            let text = fs::read_to_string(source.join("hook.py")).map_err(|e| e.to_string())?;
            if text.contains("INVALID") {
                return Err("invalid candidate".into());
            }
            let mut prepared = metadata();
            prepared.persistent_agent_sessions = text.contains("STATEFUL");
            Ok(prepared)
        }
    }

    struct BlockingPreparer {
        calls: AtomicUsize,
        first_entered: Notify,
        second_entered: Notify,
        release: Semaphore,
    }

    impl BlockingPreparer {
        fn new() -> Self {
            Self {
                calls: AtomicUsize::new(0),
                first_entered: Notify::new(),
                second_entered: Notify::new(),
                release: Semaphore::new(0),
            }
        }
    }

    #[async_trait]
    impl HookPreparer for BlockingPreparer {
        async fn prepare(&self, _: &HookId, _: &Path) -> Result<HookMetadata, String> {
            match self.calls.fetch_add(1, Ordering::SeqCst) {
                0 => self.first_entered.notify_one(),
                _ => self.second_entered.notify_one(),
            }
            let _permit = self
                .release
                .acquire()
                .await
                .map_err(|_| "test release closed".to_owned())?;
            Ok(metadata())
        }
    }

    struct SnapshotPreparer {
        calls: AtomicUsize,
        first_entered: Notify,
        release_first: Semaphore,
        observed: StdMutex<Vec<(String, String)>>,
    }

    impl SnapshotPreparer {
        fn new() -> Self {
            Self {
                calls: AtomicUsize::new(0),
                first_entered: Notify::new(),
                release_first: Semaphore::new(0),
                observed: StdMutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl HookPreparer for SnapshotPreparer {
        async fn prepare(&self, _: &HookId, source: &Path) -> Result<HookMetadata, String> {
            let hook = fs::read_to_string(source.join("hook.py")).map_err(|e| e.to_string())?;
            let module = fs::read_to_string(
                source
                    .parent()
                    .expect("snapshot source has a parent")
                    .join("modules/shared.py"),
            )
            .map_err(|e| e.to_string())?;
            self.observed.lock().unwrap().push((hook, module));
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                self.first_entered.notify_one();
                let _permit = self
                    .release_first
                    .acquire()
                    .await
                    .map_err(|_| "test release closed".to_owned())?;
            }
            Ok(metadata())
        }
    }

    struct MutatingPreparer;

    #[async_trait]
    impl HookPreparer for MutatingPreparer {
        async fn prepare(&self, _: &HookId, source: &Path) -> Result<HookMetadata, String> {
            fs::write(source.join("hook.py"), "def run(event): return 'mutated'")
                .map_err(|e| e.to_string())?;
            Ok(metadata())
        }
    }

    fn registry(temp: &TempDir) -> HookRegistry {
        HookRegistry::new(
            temp.path().join("hooks"),
            temp.path().join("modules"),
            temp.path().join("skills"),
            temp.path().join("runtimes"),
            Arc::new(Preparer),
        )
    }

    #[tokio::test]
    async fn publishes_immutable_revision_and_exact_marker_body() {
        let temp = TempDir::new().unwrap();
        let registry = registry(&temp);
        let authored = temp.path().join("hooks/demo");
        fs::create_dir_all(&authored).unwrap();
        fs::write(authored.join("hook.py"), "def run(event): pass").unwrap();
        let delta = registry.refresh().await.unwrap();
        assert_eq!(delta.published, [HookId::parse("demo").unwrap()]);
        let marker = fs::read_to_string(temp.path().join("skills/demo/SKILL.md")).unwrap();
        assert_eq!(
            marker.split("---\n\n").nth(1).unwrap().trim_end(),
            MARKER_BODY
        );
        assert!(!temp.path().join("hooks.json").exists());
        assert!(temp.path().join("skills/demo-start/SKILL.md").is_file());
        assert!(temp.path().join("skills/demo-stop/SKILL.md").is_file());
        assert!(!temp.path().join("skills/demo-pause").exists());
        assert!(!temp.path().join("skills/demo-resume").exists());

        let original = registry
            .current(&HookId::parse("demo").unwrap())
            .await
            .unwrap();
        fs::write(authored.join("hook.py"), "def run(event): return 2").unwrap();
        registry.refresh().await.unwrap();
        let replacement = registry
            .current(&HookId::parse("demo").unwrap())
            .await
            .unwrap();
        assert_ne!(original.revision, replacement.revision);
        assert!(
            fs::read_to_string(&original.hook_file)
                .unwrap()
                .contains("pass")
        );
    }

    #[tokio::test]
    async fn stateful_revision_adds_pause_resume_and_stateless_revision_removes_them() {
        let temp = TempDir::new().unwrap();
        let registry = registry(&temp);
        let authored = temp.path().join("hooks/demo");
        fs::create_dir_all(&authored).unwrap();
        fs::write(authored.join("hook.py"), "# STATEFUL\ndef run(event): pass").unwrap();
        registry.refresh().await.unwrap();

        for name in [
            "demo",
            "demo-start",
            "demo-pause",
            "demo-resume",
            "demo-stop",
        ] {
            let path = temp.path().join("skills").join(name).join("SKILL.md");
            assert!(path.is_file(), "missing {name}");
            assert_eq!(
                fs::read_to_string(path)
                    .unwrap()
                    .split("---\n\n")
                    .nth(1)
                    .unwrap()
                    .trim_end(),
                MARKER_BODY
            );
        }
        assert!(matches!(
            registry.marker_intent("demo-pause").await,
            Some(MarkerIntent::Control {
                operation: ControlOperation::Pause,
                ..
            })
        ));

        fs::write(authored.join("hook.py"), "def run(event): pass").unwrap();
        registry.refresh().await.unwrap();
        assert!(!temp.path().join("skills/demo-pause").exists());
        assert!(!temp.path().join("skills/demo-resume").exists());
        assert!(temp.path().join("skills/demo-start/SKILL.md").is_file());
        assert!(temp.path().join("skills/demo-stop/SKILL.md").is_file());
    }

    #[tokio::test]
    async fn authored_hook_colliding_with_generated_start_is_rejected_without_overwrite() {
        let temp = TempDir::new().unwrap();
        let registry = registry(&temp);
        for name in ["demo", "demo-start"] {
            let authored = temp.path().join("hooks").join(name);
            fs::create_dir_all(&authored).unwrap();
            fs::write(authored.join("hook.py"), "def run(event): pass").unwrap();
        }
        let delta = registry.refresh().await.unwrap();
        assert!(delta.failed.iter().any(|(hook, _)| hook.as_str() == "demo"));
        assert!(
            registry
                .current(&HookId::parse("demo").unwrap())
                .await
                .is_none()
        );
        assert!(
            registry
                .current(&HookId::parse("demo-start").unwrap())
                .await
                .is_some()
        );
    }

    #[tokio::test]
    async fn invalid_candidate_preserves_last_valid_revision() {
        let temp = TempDir::new().unwrap();
        let registry = registry(&temp);
        let authored = temp.path().join("hooks/demo");
        fs::create_dir_all(&authored).unwrap();
        fs::write(authored.join("hook.py"), "def run(event): pass").unwrap();
        registry.refresh().await.unwrap();
        let original = registry
            .current(&HookId::parse("demo").unwrap())
            .await
            .unwrap();
        fs::write(authored.join("hook.py"), "INVALID").unwrap();
        let delta = registry.refresh().await.unwrap();
        assert_eq!(delta.failed.len(), 1);
        assert_eq!(
            registry
                .current(&HookId::parse("demo").unwrap())
                .await
                .unwrap()
                .revision,
            original.revision
        );
    }

    #[tokio::test]
    async fn restart_restores_last_valid_revision_before_rejecting_bad_authored_bytes() {
        let temp = TempDir::new().unwrap();
        let authored = temp.path().join("hooks/demo");
        fs::create_dir_all(&authored).unwrap();
        fs::write(authored.join("hook.py"), "def run(event): return 'valid'").unwrap();
        let first_registry = registry(&temp);
        first_registry.refresh().await.unwrap();
        let id = HookId::parse("demo").unwrap();
        let valid_revision = first_registry.current(&id).await.unwrap().revision.clone();
        assert!(
            temp.path()
                .join("runtimes/revisions/demo/current.json")
                .is_file()
        );

        fs::write(authored.join("hook.py"), "INVALID").unwrap();
        drop(first_registry);
        let restarted = registry(&temp);
        let delta = restarted.refresh().await.unwrap();
        assert_eq!(delta.failed.len(), 1);
        assert_eq!(
            restarted.current(&id).await.unwrap().revision,
            valid_revision
        );
        assert_eq!(
            fs::read_to_string(&restarted.current(&id).await.unwrap().hook_file).unwrap(),
            "def run(event): return 'valid'"
        );
    }

    #[tokio::test]
    async fn concurrent_refreshes_are_one_serial_publication_transaction() {
        let temp = TempDir::new().unwrap();
        let authored = temp.path().join("hooks/demo");
        fs::create_dir_all(&authored).unwrap();
        fs::write(authored.join("hook.py"), "def run(event): pass").unwrap();
        let preparer = Arc::new(BlockingPreparer::new());
        let registry = HookRegistry::new(
            temp.path().join("hooks"),
            temp.path().join("modules"),
            temp.path().join("skills"),
            temp.path().join("runtimes"),
            preparer.clone(),
        );

        let first_registry = registry.clone();
        let first = tokio::spawn(async move { first_registry.refresh().await });
        tokio::time::timeout(Duration::from_secs(1), preparer.first_entered.notified())
            .await
            .expect("first refresh did not reach validation");

        let second_registry = registry.clone();
        let second = tokio::spawn(async move { second_registry.refresh().await });
        let overlapped = tokio::time::timeout(
            Duration::from_millis(100),
            preparer.second_entered.notified(),
        )
        .await
        .is_ok();
        preparer.release.add_permits(2);

        let first_delta = first.await.unwrap().unwrap();
        let second_delta = second.await.unwrap().unwrap();
        assert!(!overlapped, "a second refresh entered candidate validation");
        assert_eq!(preparer.calls.load(Ordering::SeqCst), 1);
        assert_eq!(first_delta.published, [HookId::parse("demo").unwrap()]);
        assert_eq!(second_delta.unchanged, [HookId::parse("demo").unwrap()]);
    }

    #[tokio::test]
    async fn live_mutation_during_validation_cannot_change_the_staged_revision() {
        let temp = TempDir::new().unwrap();
        let authored = temp.path().join("hooks/demo");
        let modules = temp.path().join("modules");
        fs::create_dir_all(&authored).unwrap();
        fs::create_dir_all(&modules).unwrap();
        fs::write(authored.join("hook.py"), "def run(event): return 'a'").unwrap();
        fs::write(modules.join("shared.py"), "VALUE = 'a'").unwrap();
        let preparer = Arc::new(SnapshotPreparer::new());
        let registry = HookRegistry::new(
            temp.path().join("hooks"),
            modules.clone(),
            temp.path().join("skills"),
            temp.path().join("runtimes"),
            preparer.clone(),
        );

        let first_registry = registry.clone();
        let first = tokio::spawn(async move { first_registry.refresh().await });
        tokio::time::timeout(Duration::from_secs(1), preparer.first_entered.notified())
            .await
            .expect("candidate validation did not start");
        fs::write(authored.join("hook.py"), "def run(event): return 'b'").unwrap();
        fs::write(modules.join("shared.py"), "VALUE = 'b'").unwrap();
        preparer.release_first.add_permits(1);
        first.await.unwrap().unwrap();

        let first_revision = registry
            .current(&HookId::parse("demo").unwrap())
            .await
            .unwrap();
        assert!(
            fs::read_to_string(&first_revision.hook_file)
                .unwrap()
                .contains("'a'")
        );
        assert!(
            fs::read_to_string(first_revision.modules_dir.join("shared.py"))
                .unwrap()
                .contains("'a'")
        );
        assert_eq!(
            first_revision.revision,
            hash_candidate(&first_revision.source_dir, &first_revision.modules_dir).unwrap()
        );

        registry.refresh().await.unwrap();
        let second_revision = registry
            .current(&HookId::parse("demo").unwrap())
            .await
            .unwrap();
        assert_ne!(first_revision.revision, second_revision.revision);
        assert!(
            fs::read_to_string(&second_revision.hook_file)
                .unwrap()
                .contains("'b'")
        );
        assert_eq!(
            preparer.observed.lock().unwrap().as_slice(),
            [
                ("def run(event): return 'a'".into(), "VALUE = 'a'".into()),
                ("def run(event): return 'b'".into(), "VALUE = 'b'".into()),
            ]
        );
    }

    #[tokio::test]
    async fn candidate_byte_mutation_during_validation_is_rejected() {
        let temp = TempDir::new().unwrap();
        let authored = temp.path().join("hooks/demo");
        fs::create_dir_all(&authored).unwrap();
        fs::write(
            authored.join("hook.py"),
            "def run(event): return 'original'",
        )
        .unwrap();
        let registry = HookRegistry::new(
            temp.path().join("hooks"),
            temp.path().join("modules"),
            temp.path().join("skills"),
            temp.path().join("runtimes"),
            Arc::new(MutatingPreparer),
        );

        let delta = registry.refresh().await.unwrap();
        assert_eq!(delta.failed.len(), 1);
        assert!(delta.failed[0].1.contains("changed during validation"));
        assert!(
            registry
                .current(&HookId::parse("demo").unwrap())
                .await
                .is_none()
        );
        assert!(!temp.path().join("skills/demo/SKILL.md").exists());
        let revision_parent = temp.path().join("runtimes/revisions/demo");
        assert!(
            fs::read_dir(revision_parent).unwrap().next().is_none(),
            "failed candidate left staged or published bytes behind"
        );
    }
}
