use codex_control::SequencedEvent;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum HookEventKind {
    UserPromptSubmitted,
    TurnStarted,
    PreToolUse,
    PostToolUse,
    PostToolUseFailure,
    AgentMessageCompleted,
    TurnCompleted,
    TurnFailed,
    TurnInterrupted,
    UnknownUpstreamEvent,
}

#[derive(Clone, Debug)]
pub struct HookEvent {
    pub kind: HookEventKind,
    pub origin: HookEventOrigin,
    pub item_id: Option<String>,
    pub payload: Value,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEventOriginKind {
    Observed,
    Native,
}

#[derive(Clone, Debug)]
pub enum HookEventOrigin {
    Observed(Arc<SequencedEvent>),
    Native {
        event_name: String,
        thread_id: String,
        turn_id: String,
        receipt_ordinal: u64,
        unix_receipt_ms: u64,
        raw_payload: Value,
    },
}

#[derive(Clone, Debug)]
pub struct NativeHookContext {
    pub event_name: String,
    pub thread_id: String,
    pub turn_id: String,
    pub receipt_ordinal: u64,
    pub unix_receipt_ms: u64,
    pub raw_payload: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HookEventEnvelope {
    /// Backward-compatible total-delivery value. For observed events this is the
    /// authoritative app-server sequence; for native events it is Warden's receipt ordinal.
    pub sequence: u64,
    pub origin: HookEventOriginKind,
    pub source_sequence: Option<u64>,
    pub receipt_ordinal: u64,
    pub native_event_name: Option<String>,
    pub kind: HookEventKind,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub item_id: Option<String>,
    pub unix_receipt_ms: u64,
    pub emitted_at_ms: Option<u64>,
    pub reconstructed: bool,
    pub payload: Value,
    pub raw_method: Option<String>,
    pub raw_payload: Value,
}

impl HookEvent {
    pub fn native(
        kind: HookEventKind,
        context: NativeHookContext,
        item_id: Option<String>,
        payload: Value,
    ) -> Self {
        Self {
            kind,
            origin: HookEventOrigin::Native {
                event_name: context.event_name,
                thread_id: context.thread_id,
                turn_id: context.turn_id,
                receipt_ordinal: context.receipt_ordinal,
                unix_receipt_ms: context.unix_receipt_ms,
                raw_payload: context.raw_payload,
            },
            item_id,
            payload,
        }
    }

    pub fn thread_id(&self) -> Option<&str> {
        match &self.origin {
            HookEventOrigin::Observed(source) => source.thread_id.as_deref(),
            HookEventOrigin::Native { thread_id, .. } => Some(thread_id),
        }
    }

    pub fn turn_id(&self) -> Option<&str> {
        match &self.origin {
            HookEventOrigin::Observed(source) => source.turn_id.as_deref(),
            HookEventOrigin::Native { turn_id, .. } => Some(turn_id),
        }
    }

    pub fn source_sequence(&self) -> Option<u64> {
        match &self.origin {
            HookEventOrigin::Observed(source) => Some(source.sequence),
            HookEventOrigin::Native { .. } => None,
        }
    }

    pub fn receipt_ordinal(&self) -> u64 {
        match &self.origin {
            HookEventOrigin::Observed(source) => source.sequence,
            HookEventOrigin::Native {
                receipt_ordinal, ..
            } => *receipt_ordinal,
        }
    }

    pub fn envelope(&self) -> HookEventEnvelope {
        let (
            origin,
            native_event_name,
            unix_receipt_ms,
            emitted_at_ms,
            reconstructed,
            raw_method,
            raw_payload,
        ) = match &self.origin {
            HookEventOrigin::Observed(source) => (
                HookEventOriginKind::Observed,
                None,
                source.unix_receipt_ms,
                source.emitted_at_ms,
                source.reconstructed,
                source.method().map(ToOwned::to_owned),
                source.frame.raw().clone(),
            ),
            HookEventOrigin::Native {
                event_name,
                unix_receipt_ms,
                raw_payload,
                ..
            } => (
                HookEventOriginKind::Native,
                Some(event_name.clone()),
                *unix_receipt_ms,
                None,
                false,
                None,
                raw_payload.clone(),
            ),
        };
        HookEventEnvelope {
            sequence: self.receipt_ordinal(),
            origin,
            source_sequence: self.source_sequence(),
            receipt_ordinal: self.receipt_ordinal(),
            native_event_name,
            kind: self.kind,
            thread_id: self.thread_id().map(ToOwned::to_owned),
            turn_id: self.turn_id().map(ToOwned::to_owned),
            item_id: self.item_id.clone(),
            unix_receipt_ms,
            emitted_at_ms,
            reconstructed,
            payload: self.payload.clone(),
            raw_method,
            raw_payload,
        }
    }
}

/// Normalizes one authoritative app-server event. A single turn start yields both the
/// user-prompt and turn-start views because hooks may subscribe to either semantic event.
pub fn normalize_event(source: Arc<SequencedEvent>) -> Vec<HookEvent> {
    normalize_event_with_input(source, None)
}

pub(crate) fn normalize_event_with_input(
    source: Arc<SequencedEvent>,
    retained_input: Option<Value>,
) -> Vec<HookEvent> {
    let Some(method) = source.method().map(str::to_owned) else {
        return Vec::new();
    };
    let params = source.frame.params().cloned().unwrap_or(Value::Null);
    match method.as_str() {
        "turn/started" => {
            let mut events = Vec::with_capacity(2);
            if !source.reconstructed || retained_input.is_some() {
                events.push(HookEvent {
                    kind: HookEventKind::UserPromptSubmitted,
                    origin: HookEventOrigin::Observed(source.clone()),
                    item_id: None,
                    payload: retained_input
                        .or_else(|| turn_input(&params))
                        .unwrap_or(Value::Null),
                });
            }
            events.push(HookEvent {
                kind: HookEventKind::TurnStarted,
                origin: HookEventOrigin::Observed(source),
                item_id: None,
                payload: params,
            });
            events
        }
        "turn/completed" => {
            let kind = match terminal_status(&params) {
                Some("failed") => HookEventKind::TurnFailed,
                Some("interrupted" | "cancelled" | "canceled") => HookEventKind::TurnInterrupted,
                _ => HookEventKind::TurnCompleted,
            };
            vec![HookEvent {
                kind,
                origin: HookEventOrigin::Observed(source),
                item_id: None,
                payload: params,
            }]
        }
        "item/started" | "item/completed" => normalize_item(source, &params, &method),
        _ => vec![HookEvent {
            kind: HookEventKind::UnknownUpstreamEvent,
            origin: HookEventOrigin::Observed(source),
            item_id: None,
            payload: params,
        }],
    }
}

fn normalize_item(source: Arc<SequencedEvent>, params: &Value, method: &str) -> Vec<HookEvent> {
    let item = params
        .get("item")
        .cloned()
        .unwrap_or_else(|| params.clone());
    let item_type = item.get("type").and_then(Value::as_str).unwrap_or_default();
    let item_id = item
        .get("id")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let kind = if item_type == "userMessage" && method == "item/started" {
        HookEventKind::UserPromptSubmitted
    } else if item_type == "agentMessage" && method == "item/completed" {
        HookEventKind::AgentMessageCompleted
    } else if is_tool_item(item_type) && method == "item/started" {
        HookEventKind::PreToolUse
    } else if is_tool_item(item_type) && method == "item/completed" {
        if item_failed(&item) {
            HookEventKind::PostToolUseFailure
        } else {
            HookEventKind::PostToolUse
        }
    } else {
        HookEventKind::UnknownUpstreamEvent
    };
    let payload = if kind == HookEventKind::UserPromptSubmitted {
        item.get("content").cloned().unwrap_or(item)
    } else {
        item
    };
    vec![HookEvent {
        kind,
        origin: HookEventOrigin::Observed(source),
        item_id,
        payload,
    }]
}

fn is_tool_item(item_type: &str) -> bool {
    matches!(
        item_type,
        "commandExecution"
            | "mcpToolCall"
            | "dynamicToolCall"
            | "webSearch"
            | "imageView"
            | "sleep"
            | "fileChange"
            | "imageGeneration"
            | "computerUse"
            | "collabAgentToolCall"
            | "collabToolCall"
    )
}

fn item_failed(item: &Value) -> bool {
    let status = item.get("status");
    status
        .and_then(|value| value.as_str().or_else(|| value.get("type")?.as_str()))
        .is_some_and(|status| {
            matches!(
                status,
                "failed"
                    | "error"
                    | "denied"
                    | "declined"
                    | "cancelled"
                    | "canceled"
                    | "interrupted"
            )
        })
        || item.get("success").and_then(Value::as_bool) == Some(false)
        || item.get("error").is_some_and(|error| !error.is_null())
}

fn terminal_status(params: &Value) -> Option<&str> {
    params
        .get("turn")
        .and_then(|turn| turn.get("status"))
        .or_else(|| params.get("status"))
        .and_then(|status| status.as_str().or_else(|| status.get("type")?.as_str()))
}

pub(crate) fn turn_input(params: &Value) -> Option<Value> {
    if let Some(input) = params
        .get("turn")
        .and_then(|turn| turn.get("input"))
        .or_else(|| params.get("input"))
    {
        return Some(input.clone());
    }

    let items = params
        .get("turn")
        .and_then(|turn| turn.get("items"))
        .and_then(Value::as_array)?;
    user_message_content(items)
}

pub(crate) fn user_message_content(items: &[Value]) -> Option<Value> {
    let content = items
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("userMessage"))
        .filter_map(|item| item.get("content"))
        .flat_map(|content| match content {
            Value::Array(values) => values.clone(),
            value => vec![value.clone()],
        })
        .collect::<Vec<_>>();
    (!content.is_empty()).then_some(Value::Array(content))
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_control::{IncomingFrame, Plane, Sequence};
    use serde_json::json;

    fn event(sequence: Sequence, method: &str, params: Value) -> Arc<SequencedEvent> {
        let raw = json!({"jsonrpc":"2.0","method":method,"params":params});
        let frame = IncomingFrame::parse(raw).expect("valid fixture frame");
        Arc::new(SequencedEvent {
            sequence,
            unix_receipt_ms: 10,
            monotonic_ms: 10,
            emitted_at_ms: None,
            plane: Plane::Lifecycle,
            thread_id: frame
                .params()
                .and_then(|p| p.get("threadId"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            turn_id: frame
                .params()
                .and_then(|p| p.get("turn"))
                .and_then(|t| t.get("id"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            frame,
            reconstructed: false,
        })
    }

    #[test]
    fn turn_start_has_prompt_and_started_views() {
        let source = event(
            1,
            "turn/started",
            json!({"threadId":"thread","turn":{"id":"turn","input":[{"type":"text","text":"hi"}]}}),
        );
        let normalized = normalize_event(source);
        assert_eq!(normalized.len(), 2);
        assert_eq!(normalized[0].kind, HookEventKind::UserPromptSubmitted);
        assert_eq!(normalized[1].kind, HookEventKind::TurnStarted);
    }

    #[test]
    fn turn_start_extracts_real_user_message_item_content() {
        let source = event(
            2,
            "turn/started",
            json!({"threadId":"thread","turn":{"id":"turn","status":"inProgress","items":[
                {"id":"user","type":"userMessage","content":[{"type":"skill","name":"demo","path":"/tmp/demo/SKILL.md"}]}
            ]}}),
        );
        let normalized = normalize_event(source);
        assert_eq!(normalized[0].kind, HookEventKind::UserPromptSubmitted);
        assert_eq!(normalized[0].payload[0]["type"], "skill");
    }

    #[test]
    fn turn_start_combines_all_user_message_items() {
        let source = event(
            3,
            "turn/started",
            json!({"threadId":"thread","turn":{"id":"turn","status":"inProgress","items":[
                {"id":"visible","type":"userMessage","content":[{"type":"input_text","text":"[$demo](marker) POP"}]},
                {"id":"skill-context","type":"userMessage","content":[{"type":"input_text","text":"<skill><path>/tmp/demo/SKILL.md</path></skill>"}]}
            ]}}),
        );
        let normalized = normalize_event(source);

        assert_eq!(normalized[0].payload.as_array().unwrap().len(), 2);
        assert_eq!(
            normalized[0].payload[1]["text"],
            "<skill><path>/tmp/demo/SKILL.md</path></skill>"
        );
    }

    #[test]
    fn retained_input_populates_lightweight_turn_start() {
        let mut source = event(
            4,
            "turn/started",
            json!({"threadId":"thread","turn":{"id":"turn"}}),
        );
        Arc::make_mut(&mut source).reconstructed = true;
        let retained = json!([{"type":"text","text":"[$demo](/tmp/demo/SKILL.md) POP"}]);
        let normalized = normalize_event_with_input(source, Some(retained.clone()));

        assert_eq!(normalized[0].kind, HookEventKind::UserPromptSubmitted);
        assert_eq!(normalized[0].payload, retained);
    }

    #[test]
    fn tool_start_success_failure_and_agent_message_are_distinct() {
        let cases = [
            (
                "item/started",
                json!({"threadId":"t","turnId":"u","item":{"id":"1","type":"commandExecution","status":"inProgress"}}),
                HookEventKind::PreToolUse,
            ),
            (
                "item/completed",
                json!({"threadId":"t","turnId":"u","item":{"id":"2","type":"mcpToolCall","status":"completed"}}),
                HookEventKind::PostToolUse,
            ),
            (
                "item/completed",
                json!({"threadId":"t","turnId":"u","item":{"id":"3","type":"commandExecution","status":"failed"}}),
                HookEventKind::PostToolUseFailure,
            ),
            (
                "item/completed",
                json!({"threadId":"t","turnId":"u","item":{"id":"4","type":"agentMessage","text":"done"}}),
                HookEventKind::AgentMessageCompleted,
            ),
            (
                "item/completed",
                json!({"threadId":"t","turnId":"u","item":{"id":"5","type":"collabAgentToolCall","status":"declined"}}),
                HookEventKind::PostToolUseFailure,
            ),
            (
                "item/completed",
                json!({"threadId":"t","turnId":"u","item":{"id":"6","type":"dynamicToolCall","status":"completed","success":false}}),
                HookEventKind::PostToolUseFailure,
            ),
            (
                "item/completed",
                json!({"threadId":"t","turnId":"u","item":{"id":"7","type":"imageView","path":"/tmp/image.png"}}),
                HookEventKind::PostToolUse,
            ),
            (
                "item/started",
                json!({"threadId":"t","turnId":"u","item":{"id":"8","type":"sleep","durationMs":10}}),
                HookEventKind::PreToolUse,
            ),
        ];
        for (index, (method, params, expected)) in cases.into_iter().enumerate() {
            let normalized = normalize_event(event(index as u64 + 1, method, params));
            assert_eq!(normalized.len(), 1);
            assert_eq!(normalized[0].kind, expected);
        }
    }

    #[test]
    fn user_message_item_is_the_authoritative_prompt_event() {
        let normalized = normalize_event(event(
            5,
            "item/started",
            json!({"threadId":"t","turnId":"u","item":{
                "id":"user",
                "type":"userMessage",
                "content":[
                    {"type":"skill","name":"demo","path":"/tmp/demo/SKILL.md"},
                    {"type":"text","text":"POP"}
                ]
            }}),
        ));

        assert_eq!(normalized[0].kind, HookEventKind::UserPromptSubmitted);
        assert_eq!(normalized[0].payload[1]["text"], "POP");
    }

    #[test]
    fn terminal_status_is_preserved() {
        for (status, expected) in [
            ("completed", HookEventKind::TurnCompleted),
            ("failed", HookEventKind::TurnFailed),
            ("interrupted", HookEventKind::TurnInterrupted),
        ] {
            let normalized = normalize_event(event(
                1,
                "turn/completed",
                json!({"threadId":"t","turn":{"id":"u","status":status}}),
            ));
            assert_eq!(normalized[0].kind, expected);
        }
    }

    #[test]
    fn unknown_lifecycle_message_is_preserved_without_misclassification() {
        let normalized = normalize_event(event(
            9,
            "future/notification",
            json!({"threadId":"t","turnId":"u","future":true}),
        ));
        assert_eq!(normalized.len(), 1);
        assert_eq!(normalized[0].kind, HookEventKind::UnknownUpstreamEvent);
        assert_eq!(
            normalized[0].envelope().raw_method.as_deref(),
            Some("future/notification")
        );
    }

    #[test]
    fn unknown_item_type_is_preserved_as_unknown() {
        let normalized = normalize_event(event(
            100,
            "item/completed",
            json!({"threadId":"t","turnId":"u","item":{"id":"future","type":"futureTool","newField":true}}),
        ));
        assert_eq!(normalized.len(), 1);
        assert_eq!(normalized[0].kind, HookEventKind::UnknownUpstreamEvent);
        assert_eq!(normalized[0].item_id.as_deref(), Some("future"));
        assert_eq!(normalized[0].payload["newField"], true);
    }
}
