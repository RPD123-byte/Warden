use crate::{
    continuous::{
        ContinuousDiagnostic, ContinuousError, ContinuousSession, ContinuousSessionStore,
    },
    event::{HookEvent, HookEventKind, turn_input},
    registry::{HookId, HookRegistry, HookRevision, MarkerIntent},
};
use codex_control::SequencedEvent;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::RwLock;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct ActivationKey {
    hook: HookId,
    thread_id: String,
    turn_id: String,
}

#[derive(Clone, Debug)]
pub struct ActivationRecord {
    pub revision: Arc<HookRevision>,
    pub thread_id: String,
    pub turn_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct DeliveryKey {
    hook: HookId,
    thread_id: String,
    turn_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct DeliveryIdentity {
    kind: HookEventKind,
    item_id: Option<String>,
    observer_disambiguator: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct HookDelivery {
    pub revision: Arc<HookRevision>,
    pub event: HookEvent,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoverageGap {
    pub after_sequence: u64,
    pub oldest_available: Option<u64>,
    pub snapshot_sequence: u64,
    pub expired_activations: usize,
}

#[derive(Clone)]
pub struct ActivationRouter {
    generated_skills_root: PathBuf,
    registry: HookRegistry,
    active: Arc<RwLock<HashMap<ActivationKey, ActivationRecord>>>,
    delivered: Arc<RwLock<HashMap<DeliveryKey, HashSet<DeliveryIdentity>>>>,
    continuous: ContinuousSessionStore,
    gaps: Arc<RwLock<Vec<CoverageGap>>>,
}

impl ActivationRouter {
    pub fn new(generated_skills_root: PathBuf, registry: HookRegistry) -> Self {
        let continuous_root = generated_skills_root
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("continuous-sessions");
        Self::with_continuous_root(generated_skills_root, registry, continuous_root)
            .expect("continuous-session state initializes")
    }

    pub fn with_continuous_root(
        generated_skills_root: PathBuf,
        registry: HookRegistry,
        continuous_root: PathBuf,
    ) -> Result<Self, ContinuousError> {
        Ok(Self {
            generated_skills_root,
            registry,
            active: Arc::new(RwLock::new(HashMap::new())),
            delivered: Arc::new(RwLock::new(HashMap::new())),
            continuous: ContinuousSessionStore::load(continuous_root)?,
            gaps: Arc::new(RwLock::new(Vec::new())),
        })
    }

    /// Resolves activation from Codex's selected-skill marker anywhere in the prompt or from a
    /// leading Warden command before events from the starting source frame are routed.
    pub async fn begin_from_turn_start(&self, source: &SequencedEvent) -> Vec<HookId> {
        self.begin_from_source_with_input(source, None).await
    }

    pub async fn begin_from_turn_start_with_input(
        &self,
        source: &SequencedEvent,
        retained_input: Option<&Value>,
    ) -> Vec<HookId> {
        self.begin_from_source_with_input(source, retained_input)
            .await
    }

    pub async fn begin_from_source_with_input(
        &self,
        source: &SequencedEvent,
        retained_input: Option<&Value>,
    ) -> Vec<HookId> {
        if source.reconstructed && retained_input.is_none() {
            return Vec::new();
        }
        let (Some(thread_id), Some(turn_id), Some(params)) = (
            source.thread_id.as_deref(),
            source.turn_id.as_deref(),
            source.frame.params(),
        ) else {
            return Vec::new();
        };
        let input = match source.method() {
            Some("turn/started") => retained_input.cloned().or_else(|| turn_input(params)),
            Some("item/started")
                if params
                    .get("item")
                    .and_then(|item| item.get("type"))
                    .and_then(Value::as_str)
                    == Some("userMessage") =>
            {
                params
                    .get("item")
                    .and_then(|item| item.get("content"))
                    .cloned()
            }
            _ => None,
        };
        let Some(input) = input else {
            return Vec::new();
        };

        self.begin_for_input(thread_id, turn_id, &input).await
    }

    pub async fn begin_native_prompt(
        &self,
        thread_id: &str,
        turn_id: &str,
        input: &Value,
    ) -> Vec<HookId> {
        self.begin_for_input(thread_id, turn_id, input).await
    }

    async fn begin_for_input(&self, thread_id: &str, turn_id: &str, input: &Value) -> Vec<HookId> {
        self.delivered
            .write()
            .await
            .retain(|key, _| key.thread_id != thread_id || key.turn_id == turn_id);
        let mut primary = Vec::new();
        let mut controls = HashMap::<HookId, HashSet<_>>::new();
        let mut selected_paths = structured_skill_paths(input);
        selected_paths.extend(
            leading_warden_commands(input)
                .into_iter()
                .map(|name| self.generated_skills_root.join(name).join("SKILL.md")),
        );
        for skill_path in selected_paths {
            let Ok(name) = resolve_marker_name(&self.generated_skills_root, &skill_path) else {
                continue;
            };
            let Some(intent) = self.registry.marker_intent(&name).await else {
                continue;
            };
            match intent {
                MarkerIntent::Primary(id) => primary.push(id),
                MarkerIntent::Control { hook, operation } => {
                    controls.entry(hook).or_default().insert(operation);
                }
            }
        }

        let mut activated = Vec::new();
        for (hook, operations) in controls {
            if operations.len() != 1 {
                self.continuous
                    .note(
                        hook,
                        thread_id,
                        "conflicting continuous controls were selected in one prompt".into(),
                    )
                    .await;
                continue;
            }
            let operation = *operations.iter().next().expect("one operation");
            let Some(revision) = self.registry.current(&hook).await else {
                self.continuous
                    .note(
                        hook,
                        thread_id,
                        "hook has no valid published revision".into(),
                    )
                    .await;
                continue;
            };
            match self
                .continuous
                .transition(
                    hook.clone(),
                    thread_id,
                    operation,
                    revision.metadata.persistent_agent_sessions,
                )
                .await
            {
                Ok(_) => activated.push(hook),
                Err(error) => {
                    self.continuous
                        .note(hook, thread_id, error.to_string())
                        .await;
                }
            }
        }

        for id in primary {
            let Some(revision) = self.registry.current(&id).await else {
                continue;
            };
            let key = ActivationKey {
                hook: id.clone(),
                thread_id: thread_id.to_owned(),
                turn_id: turn_id.to_owned(),
            };
            self.active
                .write()
                .await
                .entry(key)
                .or_insert(ActivationRecord {
                    revision,
                    thread_id: thread_id.to_owned(),
                    turn_id: turn_id.to_owned(),
                });
            activated.push(id);
        }
        activated.sort();
        activated.dedup();
        activated
    }

    pub async fn route(&self, event: HookEvent) -> Vec<HookDelivery> {
        let Some(thread_id) = event.thread_id() else {
            return Vec::new();
        };
        let Some(turn_id) = event.turn_id() else {
            return Vec::new();
        };
        let terminal = is_terminal(event.kind);
        let (matching_keys, mut eligible) = {
            let active = self.active.read().await;
            let matching_keys = active
                .keys()
                .filter(|key| key.thread_id == thread_id && key.turn_id == turn_id)
                .cloned()
                .collect::<Vec<_>>();
            let eligible = matching_keys
                .iter()
                .filter_map(|key| {
                    active
                        .get(key)
                        .map(|record| (key.hook.clone(), record.revision.clone()))
                })
                .collect::<HashMap<_, _>>();
            (matching_keys, eligible)
        };
        for hook in self.continuous.running_for_thread(thread_id).await {
            if eligible.contains_key(&hook) {
                continue;
            }
            if let Some(revision) = self.registry.current(&hook).await {
                eligible.insert(hook, revision);
            } else {
                self.continuous
                    .note(
                        hook,
                        thread_id,
                        "hook has no valid published revision".into(),
                    )
                    .await;
            }
        }

        let mut deliveries = Vec::new();
        let identity = DeliveryIdentity {
            kind: event.kind,
            item_id: (event.kind != HookEventKind::UserPromptSubmitted)
                .then(|| event.item_id.clone())
                .flatten(),
            observer_disambiguator: delivery_disambiguator(&event),
        };
        let mut delivered = self.delivered.write().await;
        for (hook, revision) in eligible {
            if !revision.metadata.events.contains(&event.kind) {
                continue;
            }
            let delivery_key = DeliveryKey {
                hook,
                thread_id: thread_id.to_owned(),
                turn_id: turn_id.to_owned(),
            };
            if !delivered
                .entry(delivery_key)
                .or_default()
                .insert(identity.clone())
            {
                continue;
            }
            deliveries.push(HookDelivery {
                revision,
                event: event.clone(),
            });
        }
        if terminal {
            let mut active = self.active.write().await;
            for key in matching_keys {
                active.remove(&key);
            }
        }
        deliveries
    }

    pub async fn note_gap(
        &self,
        after_sequence: u64,
        oldest_available: Option<u64>,
        snapshot_sequence: u64,
    ) -> CoverageGap {
        let mut active = self.active.write().await;
        let gap = CoverageGap {
            after_sequence,
            oldest_available,
            snapshot_sequence,
            expired_activations: active.len(),
        };
        // A marker may be in the missing interval. Expiring is conservative: Warden never
        // invents an activation and never keeps one alive across an unknowable terminal event.
        active.clear();
        drop(active);
        self.delivered.write().await.clear();
        self.gaps.write().await.push(gap.clone());
        gap
    }

    pub async fn active_count(&self) -> usize {
        self.active.read().await.len()
    }

    pub async fn gaps(&self) -> Vec<CoverageGap> {
        self.gaps.read().await.clone()
    }

    pub async fn continuous_sessions(&self) -> Vec<ContinuousSession> {
        self.continuous.sessions().await
    }

    pub async fn continuous_diagnostics(&self) -> Vec<ContinuousDiagnostic> {
        self.continuous.diagnostics().await
    }

    pub async fn reconcile_continuous(&self) -> Result<usize, ContinuousError> {
        self.continuous.reconcile(&self.registry).await
    }

    pub async fn remove_continuous_hook(&self, hook: &HookId) -> Result<usize, ContinuousError> {
        self.continuous.remove_hook(hook).await
    }

    pub async fn remove_continuous_thread(
        &self,
        thread_id: &str,
    ) -> Result<usize, ContinuousError> {
        self.continuous.remove_thread(thread_id).await
    }
}

fn delivery_disambiguator(event: &HookEvent) -> Option<u64> {
    if event.item_id.is_some()
        || matches!(
            event.kind,
            HookEventKind::UserPromptSubmitted
                | HookEventKind::TurnStarted
                | HookEventKind::AgentMessageCompleted
                | HookEventKind::TurnCompleted
                | HookEventKind::TurnFailed
                | HookEventKind::TurnInterrupted
        )
    {
        None
    } else {
        event.source_sequence()
    }
}

fn structured_skill_paths(input: &Value) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    collect_skill_paths(input, &mut paths);
    paths
}

fn leading_warden_commands(input: &Value) -> Vec<String> {
    let mut commands = Vec::new();
    collect_leading_warden_commands(input, &mut commands);
    commands
}

fn collect_leading_warden_commands(value: &Value, commands: &mut Vec<String>) {
    match value {
        Value::Array(values) => {
            for value in values {
                collect_leading_warden_commands(value, commands);
            }
        }
        Value::Object(object) => {
            if matches!(
                object.get("type").and_then(Value::as_str),
                Some("text" | "input_text")
            ) && let Some(text) = object.get("text").and_then(Value::as_str)
            {
                commands.extend(parse_leading_warden_commands(text));
            }
            for value in object.values() {
                collect_leading_warden_commands(value, commands);
            }
        }
        _ => {}
    }
}

fn parse_leading_warden_commands(text: &str) -> Vec<String> {
    let mut rest = text.trim_start();
    let mut commands = Vec::new();
    loop {
        let Some(command) = rest.strip_prefix('$').or_else(|| rest.strip_prefix('/')) else {
            break;
        };
        let name_end = command
            .find(|character: char| {
                !character.is_ascii_alphanumeric() && character != '-' && character != '_'
            })
            .unwrap_or(command.len());
        if name_end == 0 {
            break;
        }
        commands.push(command[..name_end].to_owned());
        rest = command[name_end..].trim_start();
    }
    commands
}

fn collect_skill_paths(value: &Value, paths: &mut Vec<PathBuf>) {
    match value {
        Value::Array(values) => {
            for value in values {
                collect_skill_paths(value, paths);
            }
        }
        Value::Object(object) => {
            if object.get("type").and_then(Value::as_str) == Some("skill") {
                if let Some(path) = object.get("path").and_then(Value::as_str) {
                    paths.push(PathBuf::from(path));
                }
                return;
            }
            // Current Codex Desktop app-server frames expose a skill selected in the prompt UI
            // as a leading `[$name](absolute/SKILL.md)` marker link. Some Codex surfaces expose
            // a structured skill item or a generated `<skill>` envelope instead. All accepted
            // forms still pass through canonical Warden-root validation below.
            if matches!(
                object.get("type").and_then(Value::as_str),
                Some("text" | "input_text")
            ) && let Some(text) = object.get("text").and_then(Value::as_str)
            {
                if let Some(path) = serialized_skill_path(text) {
                    paths.push(path);
                } else {
                    paths.extend(selected_skill_link_paths(text));
                }
            }
            for value in object.values() {
                collect_skill_paths(value, paths);
            }
        }
        _ => {}
    }
}

fn selected_skill_link_paths(text: &str) -> Vec<PathBuf> {
    let mut rest = text;
    let mut paths = Vec::new();
    while let Some(marker_start) = rest.find("[$") {
        let marker = &rest[marker_start + "[$".len()..];
        let Some(name_end) = marker.find("](") else {
            break;
        };
        let name = &marker[..name_end];
        if name.is_empty()
            || !name.chars().all(|character| {
                character.is_ascii_alphanumeric() || character == '-' || character == '_'
            })
        {
            rest = marker;
            continue;
        }
        let path_start = name_end + "](".len();
        let Some(relative_end) = marker[path_start..].find(')') else {
            break;
        };
        let path_end = relative_end + path_start;
        let path = PathBuf::from(&marker[path_start..path_end]);
        if path
            .parent()
            .and_then(Path::file_name)
            .and_then(|value| value.to_str())
            != Some(name)
        {
            rest = marker;
            continue;
        }
        paths.push(path);
        rest = &marker[path_end + 1..];
    }
    paths
}

fn serialized_skill_path(text: &str) -> Option<PathBuf> {
    let envelope = text.trim();
    if !envelope.starts_with("<skill>") || !envelope.ends_with("</skill>") {
        return None;
    }
    let path_start = envelope.find("<path>")? + "<path>".len();
    let path_end = envelope[path_start..].find("</path>")? + path_start;
    let path = envelope[path_start..path_end].trim();
    (!path.is_empty()).then(|| PathBuf::from(path))
}

fn resolve_marker_name(root: &Path, selected_path: &Path) -> Result<String, String> {
    let canonical_root = fs::canonicalize(root).map_err(|error| error.to_string())?;
    let canonical_path = fs::canonicalize(selected_path).map_err(|error| error.to_string())?;
    if !canonical_path.starts_with(&canonical_root)
        || canonical_path.file_name().and_then(|name| name.to_str()) != Some("SKILL.md")
    {
        return Err("selected skill is not a Warden marker".into());
    }
    let relative = canonical_path
        .strip_prefix(&canonical_root)
        .map_err(|error| error.to_string())?;
    let mut components = relative.components();
    let id = components
        .next()
        .and_then(|component| component.as_os_str().to_str())
        .ok_or_else(|| "marker has no hook identity".to_owned())?;
    if components.next().is_none() || components.next().is_some() {
        return Err("marker path must be <root>/<hook>/SKILL.md".into());
    }
    HookId::parse(id.to_owned())
        .map(|id| id.to_string())
        .map_err(|error| error.to_string())
}

fn is_terminal(kind: HookEventKind) -> bool {
    matches!(
        kind,
        HookEventKind::TurnCompleted | HookEventKind::TurnFailed | HookEventKind::TurnInterrupted
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{HookMetadata, HookPreparer};
    use async_trait::async_trait;
    use codex_control::{IncomingFrame, Plane};
    use serde_json::json;
    use tempfile::TempDir;

    struct Preparer;

    #[async_trait]
    impl HookPreparer for Preparer {
        async fn prepare(&self, _: &HookId, source: &Path) -> Result<HookMetadata, String> {
            let stateful = fs::read_to_string(source.join("hook.py"))
                .map(|source| source.contains("STATEFUL"))
                .unwrap_or(false);
            Ok(HookMetadata {
                function: "run".into(),
                events: HashSet::from([
                    HookEventKind::UserPromptSubmitted,
                    HookEventKind::TurnStarted,
                    HookEventKind::PreToolUse,
                    HookEventKind::TurnCompleted,
                ]),
                actions: HashSet::new(),
                blocking: false,
                persistent_agent_sessions: stateful,
            })
        }
    }

    fn source(sequence: u64, method: &str, params: Value) -> Arc<SequencedEvent> {
        let frame = IncomingFrame::parse(json!({"jsonrpc":"2.0","method":method,"params":params}))
            .expect("valid fixture frame");
        Arc::new(SequencedEvent {
            sequence,
            unix_receipt_ms: 1,
            monotonic_ms: 1,
            emitted_at_ms: None,
            plane: Plane::Lifecycle,
            thread_id: frame
                .params()
                .and_then(|p| p.get("threadId"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            turn_id: frame
                .params()
                .and_then(|p| p.get("turn"))
                .and_then(|t| t.get("id"))
                .or_else(|| frame.params().and_then(|p| p.get("turnId")))
                .and_then(Value::as_str)
                .map(str::to_owned),
            frame,
            reconstructed: false,
        })
    }

    #[tokio::test]
    async fn structured_marker_is_one_turn_only_and_replay_is_deduplicated() {
        let temp = TempDir::new().unwrap();
        let hooks = temp.path().join("hooks/demo");
        fs::create_dir_all(&hooks).unwrap();
        fs::write(hooks.join("hook.py"), "def run(event): pass").unwrap();
        let registry = HookRegistry::new(
            temp.path().join("hooks"),
            temp.path().join("modules"),
            temp.path().join("skills"),
            temp.path().join("runtimes"),
            Arc::new(Preparer),
        );
        registry.refresh().await.unwrap();
        let router = ActivationRouter::new(temp.path().join("skills"), registry);
        let marker = temp.path().join("skills/demo/SKILL.md");
        let start = source(
            1,
            "turn/started",
            json!({"threadId":"t","turn":{"id":"one","input":[{"type":"skill","name":"demo","path":marker}]}}),
        );
        assert_eq!(router.begin_from_turn_start(&start).await.len(), 1);
        let prompt = crate::event::normalize_event(start)[0].clone();
        assert_eq!(router.route(prompt.clone()).await.len(), 1);
        assert!(router.route(prompt).await.is_empty());
        let terminal = crate::event::normalize_event(source(
            2,
            "turn/completed",
            json!({"threadId":"t","turn":{"id":"one","status":"completed"}}),
        ))
        .remove(0);
        assert_eq!(router.route(terminal).await.len(), 1);
        assert_eq!(router.active_count().await, 0);

        let next = source(
            3,
            "turn/started",
            json!({"threadId":"t","turn":{"id":"two","input":[{"type":"text","text":"plain"}]}}),
        );
        assert!(router.begin_from_turn_start(&next).await.is_empty());
        assert!(
            router
                .route(crate::event::normalize_event(next)[0].clone())
                .await
                .is_empty()
        );
    }

    #[tokio::test]
    async fn desktop_selected_skill_link_activates() {
        let temp = TempDir::new().unwrap();
        let hooks = temp.path().join("hooks/demo");
        fs::create_dir_all(&hooks).unwrap();
        fs::write(hooks.join("hook.py"), "def run(event): pass").unwrap();
        let other_hooks = temp.path().join("hooks/other");
        fs::create_dir_all(&other_hooks).unwrap();
        fs::write(other_hooks.join("hook.py"), "def run(event): pass").unwrap();
        let registry = HookRegistry::new(
            temp.path().join("hooks"),
            temp.path().join("modules"),
            temp.path().join("skills"),
            temp.path().join("runtimes"),
            Arc::new(Preparer),
        );
        registry.refresh().await.unwrap();
        let router = ActivationRouter::new(temp.path().join("skills"), registry);
        let marker = temp.path().join("skills/demo/SKILL.md");
        let other_marker = temp.path().join("skills/other/SKILL.md");
        let start = source(
            1,
            "turn/started",
            json!({"threadId":"t","turn":{"id":"one","items":[
                {"id":"visible","type":"userMessage","content":[
                    {"type":"text","text":format!("[$demo]({}) [$other]({}) POP\n", marker.display(), other_marker.display()),"text_elements":[]}
                ]}
            ]}}),
        );

        assert_eq!(router.begin_from_turn_start(&start).await.len(), 2);
    }

    #[test]
    fn selected_skill_links_are_found_at_any_cursor_position() {
        let first = PathBuf::from("/tmp/generated-skills/first/SKILL.md");
        let second = PathBuf::from("/tmp/generated-skills/second/SKILL.md");
        let text = format!(
            "ordinary text before [$first]({}) and between [$second]({}) ordinary text after",
            first.display(),
            second.display()
        );

        assert_eq!(selected_skill_link_paths(&text), vec![first, second]);
    }

    #[tokio::test]
    async fn native_and_observed_copies_share_logical_delivery_identity() {
        let temp = TempDir::new().unwrap();
        let hooks = temp.path().join("hooks/demo");
        fs::create_dir_all(&hooks).unwrap();
        fs::write(hooks.join("hook.py"), "def run(event): pass").unwrap();
        let registry = HookRegistry::new(
            temp.path().join("hooks"),
            temp.path().join("modules"),
            temp.path().join("skills"),
            temp.path().join("runtimes"),
            Arc::new(Preparer),
        );
        registry.refresh().await.unwrap();
        let router = ActivationRouter::new(temp.path().join("skills"), registry);
        let marker = temp.path().join("skills/demo/SKILL.md");
        let input = json!([{"type":"skill", "name":"demo", "path": marker}]);
        assert_eq!(router.begin_native_prompt("t", "u", &input).await.len(), 1);

        for (kind, item_id) in [
            (HookEventKind::UserPromptSubmitted, None),
            (HookEventKind::TurnStarted, None),
            (HookEventKind::PreToolUse, Some("tool".to_owned())),
        ] {
            let event = HookEvent::native(
                kind,
                crate::event::NativeHookContext {
                    event_name: "native".into(),
                    thread_id: "t".into(),
                    turn_id: "u".into(),
                    receipt_ordinal: 1,
                    unix_receipt_ms: 1,
                    raw_payload: json!({}),
                },
                item_id,
                json!({}),
            );
            assert_eq!(router.route(event.clone()).await.len(), 1);
            assert!(
                router.route(event).await.is_empty(),
                "native retry duplicated {kind:?}"
            );
        }

        let observed_start = crate::event::normalize_event(source(
            10,
            "turn/started",
            json!({"threadId":"t", "turn":{"id":"u", "input":input}}),
        ));
        for event in observed_start {
            assert!(router.route(event).await.is_empty());
        }
        let observed_tool = crate::event::normalize_event(source(
            11,
            "item/started",
            json!({"threadId":"t", "turnId":"u", "item":{"id":"tool", "type":"commandExecution"}}),
        ));
        assert!(router.route(observed_tool[0].clone()).await.is_empty());

        let distinct_tool = crate::event::normalize_event(source(
            12,
            "item/started",
            json!({"threadId":"t", "turnId":"u", "item":{"id":"other", "type":"commandExecution"}}),
        ));
        assert_eq!(router.route(distinct_tool[0].clone()).await.len(), 1);
    }

    #[tokio::test]
    async fn lightweight_turn_start_activates_from_retained_marker_link() {
        let temp = TempDir::new().unwrap();
        let hooks = temp.path().join("hooks/demo");
        fs::create_dir_all(&hooks).unwrap();
        fs::write(hooks.join("hook.py"), "def run(event): pass").unwrap();
        let registry = HookRegistry::new(
            temp.path().join("hooks"),
            temp.path().join("modules"),
            temp.path().join("skills"),
            temp.path().join("runtimes"),
            Arc::new(Preparer),
        );
        registry.refresh().await.unwrap();
        let router = ActivationRouter::new(temp.path().join("skills"), registry);
        let marker = temp.path().join("skills/demo/SKILL.md");
        let mut start = source(
            1,
            "turn/started",
            json!({"threadId":"t","turn":{"id":"one"}}),
        );
        Arc::make_mut(&mut start).reconstructed = true;
        let retained = json!([{
            "type":"text",
            "text":format!("[$demo]({}) POP", marker.display())
        }]);

        assert_eq!(
            router
                .begin_from_turn_start_with_input(&start, Some(&retained))
                .await,
            vec![HookId::parse("demo").unwrap()]
        );
        let recovered_prompt =
            crate::event::normalize_event_with_input(start.clone(), Some(retained.clone()))
                .remove(0);
        assert_eq!(router.route(recovered_prompt).await.len(), 1);

        let genuine = source(
            2,
            "item/started",
            json!({"threadId":"t","turnId":"one","item":{
                "id":"user","type":"userMessage","content":retained
            }}),
        );
        assert!(
            router
                .route(crate::event::normalize_event(genuine).remove(0))
                .await
                .is_empty(),
            "recovered and live copies of one prompt must not invoke twice"
        );
    }

    #[tokio::test]
    async fn authoritative_user_message_item_activates() {
        let temp = TempDir::new().unwrap();
        let hooks = temp.path().join("hooks/demo");
        fs::create_dir_all(&hooks).unwrap();
        fs::write(hooks.join("hook.py"), "def run(event): pass").unwrap();
        let registry = HookRegistry::new(
            temp.path().join("hooks"),
            temp.path().join("modules"),
            temp.path().join("skills"),
            temp.path().join("runtimes"),
            Arc::new(Preparer),
        );
        registry.refresh().await.unwrap();
        let router = ActivationRouter::new(temp.path().join("skills"), registry);
        let marker = temp.path().join("skills/demo/SKILL.md");
        let user_message = source(
            2,
            "item/started",
            json!({"threadId":"t","turnId":"one","item":{
                "id":"user",
                "type":"userMessage",
                "content":[
                    {"type":"skill","name":"demo","path":marker},
                    {"type":"text","text":"POP"}
                ]
            }}),
        );

        assert_eq!(
            router
                .begin_from_source_with_input(&user_message, None)
                .await,
            vec![HookId::parse("demo").unwrap()]
        );
    }

    #[tokio::test]
    async fn outside_skill_is_rejected_and_gap_expires_state() {
        let temp = TempDir::new().unwrap();
        let hooks = temp.path().join("hooks/demo");
        fs::create_dir_all(&hooks).unwrap();
        fs::write(hooks.join("hook.py"), "def run(event): pass").unwrap();
        let registry = HookRegistry::new(
            temp.path().join("hooks"),
            temp.path().join("modules"),
            temp.path().join("skills"),
            temp.path().join("runtimes"),
            Arc::new(Preparer),
        );
        registry.refresh().await.unwrap();
        let router = ActivationRouter::new(temp.path().join("skills"), registry);
        let outside = temp.path().join("outside/SKILL.md");
        fs::create_dir_all(outside.parent().unwrap()).unwrap();
        fs::write(&outside, "not a marker").unwrap();
        let rejected = source(
            1,
            "turn/started",
            json!({"threadId":"t","turn":{"id":"bad","input":[
                {"type":"text","text":"not a Warden command"},
                {"type":"skill","name":"demo","path":outside}
            ]}}),
        );
        assert!(router.begin_from_turn_start(&rejected).await.is_empty());

        let marker = temp.path().join("skills/demo/SKILL.md");
        let accepted = source(
            2,
            "turn/started",
            json!({"threadId":"t","turn":{"id":"good","input":[{"type":"skill","name":"demo","path":marker}]}}),
        );
        assert_eq!(router.begin_from_turn_start(&accepted).await.len(), 1);
        let gap = router.note_gap(2, Some(10), 20).await;
        assert_eq!(gap.expired_activations, 1);
        assert_eq!(router.active_count().await, 0);
    }

    #[tokio::test]
    async fn leading_dollar_and_slash_commands_activate_generated_markers() {
        let temp = TempDir::new().unwrap();
        let hooks = temp.path().join("hooks/demo");
        fs::create_dir_all(&hooks).unwrap();
        fs::write(hooks.join("hook.py"), "def run(event): pass").unwrap();
        let registry = HookRegistry::new(
            temp.path().join("hooks"),
            temp.path().join("modules"),
            temp.path().join("skills"),
            temp.path().join("runtimes"),
            Arc::new(Preparer),
        );
        registry.refresh().await.unwrap();
        let router = ActivationRouter::new(temp.path().join("skills"), registry);

        for (sequence, turn, command) in [
            (1, "dollar", "$demo inspect this turn"),
            (2, "slash", "/demo inspect this turn"),
        ] {
            let start = source(
                sequence,
                "turn/started",
                json!({"threadId":"t","turn":{"id":turn,"input":[{
                    "type":"text","text":command
                }]}}),
            );
            assert_eq!(
                router.begin_from_turn_start(&start).await,
                [HookId::parse("demo").unwrap()]
            );
        }

        let embedded = source(
            3,
            "turn/started",
            json!({"threadId":"t","turn":{"id":"embedded","input":[{
                "type":"text","text":"please run /demo"
            }]}}),
        );
        assert!(router.begin_from_turn_start(&embedded).await.is_empty());
    }

    #[tokio::test]
    async fn bare_continuous_controls_work_without_codex_skill_discovery() {
        let temp = TempDir::new().unwrap();
        let hooks = temp.path().join("hooks/demo");
        fs::create_dir_all(&hooks).unwrap();
        fs::write(hooks.join("hook.py"), "def run(event): pass").unwrap();
        let registry = HookRegistry::new(
            temp.path().join("hooks"),
            temp.path().join("modules"),
            temp.path().join("skills"),
            temp.path().join("runtimes"),
            Arc::new(Preparer),
        );
        registry.refresh().await.unwrap();
        let router = ActivationRouter::new(temp.path().join("skills"), registry);

        let start = source(
            1,
            "turn/started",
            json!({"threadId":"t","turn":{"id":"start","input":[{
                "type":"text","text":"/demo-start"
            }]}}),
        );
        assert_eq!(
            router.begin_from_turn_start(&start).await,
            [HookId::parse("demo").unwrap()]
        );
        assert_eq!(router.continuous_sessions().await.len(), 1);

        let stop = source(
            2,
            "turn/started",
            json!({"threadId":"t","turn":{"id":"stop","input":[{
                "type":"text","text":"$demo-stop"
            }]}}),
        );
        assert_eq!(
            router.begin_from_turn_start(&stop).await,
            [HookId::parse("demo").unwrap()]
        );
        assert!(router.continuous_sessions().await.is_empty());
    }

    #[tokio::test]
    async fn stateless_start_routes_later_markerless_turns_until_stop() {
        let temp = TempDir::new().unwrap();
        let hooks = temp.path().join("hooks/demo");
        fs::create_dir_all(&hooks).unwrap();
        fs::write(hooks.join("hook.py"), "def run(event): pass").unwrap();
        let registry = HookRegistry::new(
            temp.path().join("hooks"),
            temp.path().join("modules"),
            temp.path().join("skills"),
            temp.path().join("runtimes"),
            Arc::new(Preparer),
        );
        registry.refresh().await.unwrap();
        let router = ActivationRouter::new(temp.path().join("skills"), registry);

        let start = source(
            1,
            "turn/started",
            json!({"threadId":"t","turn":{"id":"one","input":[{
                "type":"skill","name":"demo-start","path":temp.path().join("skills/demo-start/SKILL.md")
            }]}}),
        );
        assert_eq!(
            router.begin_from_turn_start(&start).await,
            [HookId::parse("demo").unwrap()]
        );
        assert_eq!(
            router
                .route(crate::event::normalize_event(start)[0].clone())
                .await
                .len(),
            1
        );

        let later = source(
            2,
            "turn/started",
            json!({"threadId":"t","turn":{"id":"two","input":[{"type":"text","text":"continue"}]}}),
        );
        assert!(router.begin_from_turn_start(&later).await.is_empty());
        assert_eq!(
            router
                .route(crate::event::normalize_event(later)[0].clone())
                .await
                .len(),
            1
        );

        let stop = source(
            3,
            "turn/started",
            json!({"threadId":"t","turn":{"id":"three","input":[{
                "type":"skill","name":"demo-stop","path":temp.path().join("skills/demo-stop/SKILL.md")
            }]}}),
        );
        assert_eq!(
            router.begin_from_turn_start(&stop).await,
            [HookId::parse("demo").unwrap()]
        );
        assert!(
            router
                .route(crate::event::normalize_event(stop)[0].clone())
                .await
                .is_empty()
        );
        assert!(router.continuous_sessions().await.is_empty());
    }

    #[tokio::test]
    async fn stateful_pause_primary_and_resume_are_independent() {
        let temp = TempDir::new().unwrap();
        let hooks = temp.path().join("hooks/demo");
        fs::create_dir_all(&hooks).unwrap();
        fs::write(hooks.join("hook.py"), "# STATEFUL\ndef run(event): pass").unwrap();
        let registry = HookRegistry::new(
            temp.path().join("hooks"),
            temp.path().join("modules"),
            temp.path().join("skills"),
            temp.path().join("runtimes"),
            Arc::new(Preparer),
        );
        registry.refresh().await.unwrap();
        let router = ActivationRouter::new(temp.path().join("skills"), registry);

        for (sequence, turn, marker, expected) in [
            (1, "start", "demo-start", 1),
            (2, "pause", "demo-pause", 0),
            (3, "primary", "demo", 1),
            (4, "resume", "demo-resume", 1),
        ] {
            let start = source(
                sequence,
                "turn/started",
                json!({"threadId":"t","turn":{"id":turn,"input":[{
                    "type":"skill","name":marker,"path":temp.path().join("skills").join(marker).join("SKILL.md")
                }]}}),
            );
            router.begin_from_turn_start(&start).await;
            assert_eq!(
                router
                    .route(crate::event::normalize_event(start)[0].clone())
                    .await
                    .len(),
                expected,
                "unexpected delivery for {marker}"
            );
        }
        assert_eq!(
            router.continuous_sessions().await[0].status,
            crate::continuous::ContinuousStatus::Running
        );
    }

    #[tokio::test]
    async fn continuous_session_uses_latest_revision_and_paused_state_is_removed_if_stateless() {
        let temp = TempDir::new().unwrap();
        let hooks = temp.path().join("hooks/demo");
        fs::create_dir_all(&hooks).unwrap();
        fs::write(
            hooks.join("hook.py"),
            "# STATEFUL\ndef run(event): return 1",
        )
        .unwrap();
        let registry = HookRegistry::new(
            temp.path().join("hooks"),
            temp.path().join("modules"),
            temp.path().join("skills"),
            temp.path().join("runtimes"),
            Arc::new(Preparer),
        );
        registry.refresh().await.unwrap();
        let router = ActivationRouter::new(temp.path().join("skills"), registry.clone());

        let start = source(
            1,
            "turn/started",
            json!({"threadId":"t","turn":{"id":"one","input":[{
                "type":"skill","name":"demo-start","path":temp.path().join("skills/demo-start/SKILL.md")
            }]}}),
        );
        router.begin_from_turn_start(&start).await;
        let original = router
            .route(crate::event::normalize_event(start)[0].clone())
            .await;
        assert_eq!(original.len(), 1);

        fs::write(
            hooks.join("hook.py"),
            "# STATEFUL\ndef run(event): return 2",
        )
        .unwrap();
        registry.refresh().await.unwrap();
        let later = source(
            2,
            "turn/started",
            json!({"threadId":"t","turn":{"id":"two","input":[]}}),
        );
        let replacement = router
            .route(crate::event::normalize_event(later)[0].clone())
            .await;
        assert_eq!(replacement.len(), 1);
        assert_ne!(
            original[0].revision.revision,
            replacement[0].revision.revision
        );

        let pause = source(
            3,
            "turn/started",
            json!({"threadId":"t","turn":{"id":"three","input":[{
                "type":"skill","name":"demo-pause","path":temp.path().join("skills/demo-pause/SKILL.md")
            }]}}),
        );
        router.begin_from_turn_start(&pause).await;
        assert_eq!(
            router.continuous_sessions().await[0].status,
            crate::continuous::ContinuousStatus::Paused
        );

        fs::write(hooks.join("hook.py"), "def run(event): return 3").unwrap();
        registry.refresh().await.unwrap();
        assert_eq!(router.reconcile_continuous().await.unwrap(), 1);
        assert!(router.continuous_sessions().await.is_empty());
        assert!(!temp.path().join("skills/demo-pause").exists());
    }

    #[tokio::test]
    async fn coverage_gap_expires_one_turn_state_but_preserves_continuous_choice() {
        let temp = TempDir::new().unwrap();
        let hooks = temp.path().join("hooks/demo");
        fs::create_dir_all(&hooks).unwrap();
        fs::write(hooks.join("hook.py"), "def run(event): pass").unwrap();
        let registry = HookRegistry::new(
            temp.path().join("hooks"),
            temp.path().join("modules"),
            temp.path().join("skills"),
            temp.path().join("runtimes"),
            Arc::new(Preparer),
        );
        registry.refresh().await.unwrap();
        let router = ActivationRouter::new(temp.path().join("skills"), registry);
        let start = source(
            1,
            "turn/started",
            json!({"threadId":"t","turn":{"id":"one","input":[{
                "type":"skill","name":"demo-start","path":temp.path().join("skills/demo-start/SKILL.md")
            }]}}),
        );
        router.begin_from_turn_start(&start).await;
        router.note_gap(1, Some(10), 20).await;

        let later = source(
            21,
            "turn/started",
            json!({"threadId":"t","turn":{"id":"two","input":[]}}),
        );
        assert_eq!(
            router
                .route(crate::event::normalize_event(later)[0].clone())
                .await
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn conflicting_stateful_controls_apply_no_transition() {
        let temp = TempDir::new().unwrap();
        let hooks = temp.path().join("hooks/demo");
        fs::create_dir_all(&hooks).unwrap();
        fs::write(hooks.join("hook.py"), "# STATEFUL\ndef run(event): pass").unwrap();
        let registry = HookRegistry::new(
            temp.path().join("hooks"),
            temp.path().join("modules"),
            temp.path().join("skills"),
            temp.path().join("runtimes"),
            Arc::new(Preparer),
        );
        registry.refresh().await.unwrap();
        let router = ActivationRouter::new(temp.path().join("skills"), registry);
        let start = source(
            1,
            "turn/started",
            json!({"threadId":"t","turn":{"id":"one","input":[{
                "type":"skill","name":"demo-start","path":temp.path().join("skills/demo-start/SKILL.md")
            }]}}),
        );
        router.begin_from_turn_start(&start).await;

        let conflict = source(
            2,
            "turn/started",
            json!({"threadId":"t","turn":{"id":"two","input":[
                {"type":"skill","name":"demo-pause","path":temp.path().join("skills/demo-pause/SKILL.md")},
                {"type":"skill","name":"demo-resume","path":temp.path().join("skills/demo-resume/SKILL.md")}
            ]}}),
        );
        assert!(router.begin_from_turn_start(&conflict).await.is_empty());
        assert_eq!(
            router.continuous_sessions().await[0].status,
            crate::continuous::ContinuousStatus::Running
        );
        assert_eq!(router.continuous_diagnostics().await.len(), 1);
    }

    #[tokio::test]
    async fn restart_restores_paused_and_running_sessions_without_cross_task_routing() {
        let temp = TempDir::new().unwrap();
        let hooks = temp.path().join("hooks/demo");
        fs::create_dir_all(&hooks).unwrap();
        fs::write(hooks.join("hook.py"), "# STATEFUL\ndef run(event): pass").unwrap();
        let make_registry = || {
            HookRegistry::new(
                temp.path().join("hooks"),
                temp.path().join("modules"),
                temp.path().join("skills"),
                temp.path().join("runtimes"),
                Arc::new(Preparer),
            )
        };
        let registry = make_registry();
        registry.refresh().await.unwrap();
        let state_root = temp.path().join("continuous-state");
        let router = ActivationRouter::with_continuous_root(
            temp.path().join("skills"),
            registry,
            state_root.clone(),
        )
        .unwrap();
        for (sequence, thread) in [(1, "task-a"), (2, "task-b")] {
            let start = source(
                sequence,
                "turn/started",
                json!({"threadId":thread,"turn":{"id":"start","input":[{
                    "type":"skill","name":"demo-start","path":temp.path().join("skills/demo-start/SKILL.md")
                }]}}),
            );
            router.begin_from_turn_start(&start).await;
        }
        let pause = source(
            3,
            "turn/started",
            json!({"threadId":"task-a","turn":{"id":"pause","input":[{
                "type":"skill","name":"demo-pause","path":temp.path().join("skills/demo-pause/SKILL.md")
            }]}}),
        );
        router.begin_from_turn_start(&pause).await;
        drop(router);

        let restarted_registry = make_registry();
        restarted_registry.refresh().await.unwrap();
        let restarted = ActivationRouter::with_continuous_root(
            temp.path().join("skills"),
            restarted_registry,
            state_root,
        )
        .unwrap();
        restarted.reconcile_continuous().await.unwrap();
        let sessions = restarted.continuous_sessions().await;
        assert_eq!(sessions.len(), 2);
        assert!(sessions.iter().any(|session| {
            session.key.thread_id == "task-a"
                && session.status == crate::continuous::ContinuousStatus::Paused
        }));
        assert!(sessions.iter().any(|session| {
            session.key.thread_id == "task-b"
                && session.status == crate::continuous::ContinuousStatus::Running
        }));

        for (sequence, thread, expected) in [(4, "task-a", 0), (5, "task-b", 1)] {
            let event = source(
                sequence,
                "turn/started",
                json!({"threadId":thread,"turn":{"id":"later","input":[]}}),
            );
            assert_eq!(
                restarted
                    .route(crate::event::normalize_event(event)[0].clone())
                    .await
                    .len(),
                expected
            );
        }
    }
}
