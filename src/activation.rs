use crate::{
    event::{HookEvent, HookEventKind, turn_input},
    registry::{HookId, HookRegistry, HookRevision},
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
    user_prompt_delivered: bool,
    delivered: HashSet<DeliveryIdentity>,
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
    gaps: Arc<RwLock<Vec<CoverageGap>>>,
}

impl ActivationRouter {
    pub fn new(generated_skills_root: PathBuf, registry: HookRegistry) -> Self {
        Self {
            generated_skills_root,
            registry,
            active: Arc::new(RwLock::new(HashMap::new())),
            gaps: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// Resolves activation from Codex's selected-skill marker before events from the starting
    /// source frame are routed. Literal slash-command text is intentionally ignored.
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
        let mut activated = Vec::new();
        for skill_path in structured_skill_paths(input) {
            let Ok(id) = resolve_marker(&self.generated_skills_root, &skill_path) else {
                continue;
            };
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
                    user_prompt_delivered: false,
                    delivered: HashSet::new(),
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
        let mut active = self.active.write().await;
        let matching_keys = active
            .keys()
            .filter(|key| key.thread_id == thread_id && key.turn_id == turn_id)
            .cloned()
            .collect::<Vec<_>>();
        let mut deliveries = Vec::new();
        for key in &matching_keys {
            let record = active.get_mut(key).expect("key was collected from map");
            if !record.revision.metadata.events.contains(&event.kind) {
                continue;
            }
            if event.kind == HookEventKind::UserPromptSubmitted && record.user_prompt_delivered {
                continue;
            }
            let identity = DeliveryIdentity {
                kind: event.kind,
                item_id: event.item_id.clone(),
                observer_disambiguator: delivery_disambiguator(&event),
            };
            if record.delivered.contains(&identity) {
                continue;
            }
            record.delivered.insert(identity);
            if event.kind == HookEventKind::UserPromptSubmitted {
                record.user_prompt_delivered = true;
            }
            deliveries.push(HookDelivery {
                revision: record.revision.clone(),
                event: event.clone(),
            });
        }
        if terminal {
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
        self.gaps.write().await.push(gap.clone());
        gap
    }

    pub async fn active_count(&self) -> usize {
        self.active.read().await.len()
    }

    pub async fn gaps(&self) -> Vec<CoverageGap> {
        self.gaps.read().await.clone()
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
    let mut rest = text.trim_start();
    let mut paths = Vec::new();
    while let Some(marker) = rest.strip_prefix("[$") {
        let Some(name_end) = marker.find("](") else {
            break;
        };
        let name = &marker[..name_end];
        if name.is_empty() {
            break;
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
            break;
        }
        paths.push(path);
        rest = marker[path_end + 1..].trim_start();
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

fn resolve_marker(root: &Path, selected_path: &Path) -> Result<HookId, String> {
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
    HookId::parse(id.to_owned()).map_err(|error| error.to_string())
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
        async fn prepare(&self, _: &HookId, _: &Path) -> Result<HookMetadata, String> {
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
    async fn outside_skill_and_literal_slash_text_do_not_activate_and_gap_expires_state() {
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
                {"type":"text","text":"/demo"},
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
}
