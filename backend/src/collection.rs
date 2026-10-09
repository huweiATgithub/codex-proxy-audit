//! Request-boundary collection. This module performs no I/O; callers serialize
//! each scope in a database transaction before saving drafts and the new state.
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

pub const POLICY_VERSION: u32 = 1;
pub const WINDOW_MESSAGES: usize = 12;
pub const WINDOW_CHARACTERS: usize = 32_000;
const MAX_ANCHORS: usize = 256;
const MAX_RESPONSES: usize = 64;
const MAX_METADATA_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug)]
pub struct RequestObservation {
    pub key_id: String,
    pub request_id: String,
    pub body: Value,
    pub headers: BTreeMap<String, String>,
    pub observed_at: DateTime<Utc>,
    pub key_name: Option<String>,
    pub key_name_observed_at: Option<DateTime<Utc>>,
    /// WebSocket response.create input is incremental even without a predecessor.
    pub incremental: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Nontext {
        #[serde(rename = "mediaType", alias = "media_type")]
        media_type: String,
        description: Option<String>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContextMessage {
    pub role: String,
    pub phase: Option<String>,
    pub truncated: bool,
    pub blocks: Vec<ContentBlock>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Remote {
    pub name: String,
    pub url: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Repository {
    pub root_path: String,
    pub remotes: Option<Vec<Remote>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuditDraft {
    pub key_id: String,
    pub key_name: Option<String>,
    pub key_name_observed_at: Option<DateTime<Utc>>,
    pub observed_at: DateTime<Utc>,
    pub request_id: String,
    pub scope: String,
    pub thread_id: Option<String>,
    pub session_id: Option<String>,
    pub turn_id: Option<String>,
    pub blocks: Vec<ContentBlock>,
    pub input_kinds: Vec<Option<String>>,
    pub repositories: Option<Vec<Repository>>,
    pub context: Vec<ContextMessage>,
    pub context_gap: bool,
    /// Only stable message IDs and established ordered positions are unique.
    pub identity: Option<String>,
    pub identity_basis: String,
    pub observation_status: String,
    pub source_position: usize,
    pub policy_version: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct SessionState {
    #[serde(default)]
    pub scope: Option<String>,
    pub anchors: Vec<Anchor>,
    #[serde(default)]
    pub history_length: usize,
    #[serde(default)]
    pub history_hash: String,
    pub responses: Vec<ResponseContext>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Anchor {
    pub fingerprint: String,
    pub message_id: Option<String>,
    pub identity: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResponseContext {
    pub scope: String,
    pub response_id: Option<String>,
    pub anchors: Vec<Anchor>,
    #[serde(default)]
    pub history_length: usize,
    #[serde(default)]
    pub history_hash: String,
    pub window: Vec<ContextMessage>,
    pub context_gap: bool,
}

#[derive(Clone, Debug, Default)]
pub struct PreparedCollection {
    pub audits: Vec<AuditDraft>,
    pub state: SessionState,
    pub response_context: Option<ResponseContext>,
    /// Oversized directories cannot be exposed under the management budget.
    pub losses: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ResponseSnapshot {
    pub response_id: String,
    pub messages: Vec<ContextMessage>,
    pub complete: bool,
}

#[derive(Default)]
struct Metadata {
    thread_id: Option<String>,
    session_id: Option<String>,
    repositories: Option<Vec<Repository>>,
    excluded: bool,
}

/// A session ID remains a session ID: it is never persisted as a thread ID.
pub fn scope(observation: &RequestObservation) -> String {
    let metadata = metadata(observation);
    scope_with_metadata(observation, &metadata)
}

fn scope_with_metadata(observation: &RequestObservation, metadata: &Metadata) -> String {
    let conversation = if let Some(thread) = &metadata.thread_id {
        json!(["thread", thread])
    } else if let Some(session) = &metadata.session_id {
        json!(["session", session])
    } else {
        json!(["request", observation.request_id])
    };
    json!([observation.key_id, conversation]).to_string()
}

/// Input snapshots are produced before the response observer exists. Response
/// updates only finite continuation state and can never mutate an AuditDraft.
pub fn collect(state: &SessionState, observation: &RequestObservation) -> PreparedCollection {
    let metadata = metadata(observation);
    let scope = scope_with_metadata(observation, &metadata);
    let empty = SessionState::default();
    let state = if state.scope.as_deref() == Some(&scope) {
        state
    } else {
        &empty
    };
    let mut result = PreparedCollection {
        state: state.clone(),
        ..PreparedCollection::default()
    };
    if metadata.excluded {
        return result;
    }
    result.state.scope = Some(scope.clone());
    let input: Vec<Value> = match observation.body.get("input") {
        Some(Value::Array(items)) => items.clone(),
        Some(Value::String(text)) => vec![json!({"role":"user","content":text})],
        _ => vec![],
    };
    let previous = observation
        .body
        .get("previous_response_id")
        .and_then(Value::as_str);
    let predecessor =
        previous.and_then(|id| {
            state.responses.iter().rev().find(|response| {
                response.response_id.as_deref() == Some(id) && response.scope == scope
            })
        });
    let parsed: Vec<ParsedMessage> = input.iter().map(parse_message).collect();
    let incoming: Vec<Anchor> = parsed.iter().map(|item| item.anchor.clone()).collect();
    let has_visible_assistant = parsed
        .iter()
        .any(|item| item.role.as_deref() == Some("assistant"));
    // Responses previous_response_id also supports HTTP deltas. A single
    // same-text user input after an empty response is a new appended input.
    let incremental = observation.incremental || (previous.is_some() && !has_visible_assistant);
    // Incremental input is appended to its explicit predecessor. Without one,
    // matching the last response or a same-text input would invent a linkage.
    let known = if let Some(predecessor) = predecessor {
        &predecessor.anchors[..]
    } else if !incremental {
        &state.anchors[..]
    } else {
        &[]
    };
    let overlap = if incremental {
        // A WebSocket delta with a predecessor can contain the same text as
        // its predecessor's final input; only stable message IDs prove replay.
        0
    } else {
        let state_prefix = verified_prefix(&incoming, state.history_length, &state.history_hash);
        let response_prefix = predecessor.map_or(0, |response| {
            verified_prefix(&incoming, response.history_length, &response.history_hash)
        });
        aligned_prefix(known, &incoming)
            .max(state_prefix)
            .max(response_prefix)
    };
    let use_predecessor_window = predecessor.is_some()
        && (incremental
            || overlap > 0
            || !parsed
                .iter()
                .any(|item| item.role.as_deref() == Some("assistant")));
    let mut window = predecessor
        .filter(|_| use_predecessor_window)
        .map(|value| value.window.clone())
        .unwrap_or_default();
    let context_gap = predecessor
        .map(|value| value.context_gap)
        .unwrap_or(previous.is_some());
    let mut anchors = if incremental {
        predecessor
            .map(|value| value.anchors.clone())
            .unwrap_or_default()
    } else {
        vec![]
    };
    for (position, item) in parsed.into_iter().enumerate() {
        let aligned_anchor = if position < overlap && overlap > known.len() {
            let length =
                predecessor.map_or(state.history_length, |response| response.history_length);
            position
                .checked_sub(length.saturating_sub(known.len()))
                .and_then(|index| known.get(index))
        } else if position < overlap && position + known.len() >= overlap {
            known.get(position + known.len() - overlap)
        } else {
            None
        };
        let known_anchor = aligned_anchor
            .filter(|anchor| {
                anchor.fingerprint == item.anchor.fingerprint
                    && (anchor.message_id.is_none()
                        || item.anchor.message_id.is_none()
                        || anchor.message_id == item.anchor.message_id)
            })
            .or_else(|| {
                item.anchor.message_id.as_ref().and_then(|id| {
                    state
                        .anchors
                        .iter()
                        .chain(state.responses.iter().flat_map(|r| &r.anchors))
                        .find(|anchor| anchor.message_id.as_ref() == Some(id))
                })
            });
        let mut anchor = item.anchor.clone();
        if let Some(known_anchor) = known_anchor {
            anchor.identity.clone_from(&known_anchor.identity);
        }
        if item.role.as_deref() == Some("user") && !item.blocks.is_empty() {
            let stable_identity = anchor.message_id.as_ref().map(|id| format!("message:{id}"));
            let established = !incremental || predecessor.is_some();
            if position >= overlap && known_anchor.is_none() {
                // An ordered position is seeded by a request, never by text or
                // a turn ID. The persisted anchors carry that identity on replay.
                anchor.identity = stable_identity.or_else(|| {
                    established.then(|| format!("position:{}:{position}", observation.request_id))
                });
                let historical = input[position + 1..]
                    .iter()
                    .any(|value| value.get("role").and_then(Value::as_str) == Some("assistant"));
                let mut kinds = item.input_kinds;
                if serde_json::to_vec(&kinds).map_or(true, |bytes| bytes.len() > MAX_METADATA_BYTES)
                {
                    kinds.fill(None);
                    result
                        .losses
                        .push("input classifications exceeded optional metadata budget".to_owned());
                }
                result.audits.push(AuditDraft {
                    key_id: observation.key_id.clone(),
                    key_name: bounded(observation.key_name.as_deref(), 512),
                    key_name_observed_at: observation.key_name_observed_at,
                    observed_at: observation.observed_at,
                    request_id: observation.request_id.clone(),
                    scope: scope.clone(),
                    thread_id: metadata.thread_id.clone(),
                    session_id: metadata.session_id.clone(),
                    turn_id: item.turn_id,
                    blocks: item.blocks.clone(),
                    input_kinds: kinds,
                    repositories: metadata.repositories.clone(),
                    context: limit_window(&window),
                    context_gap,
                    identity: anchor.identity.clone(),
                    identity_basis: if anchor.message_id.is_some() {
                        "message_id"
                    } else if established {
                        "ordered_history"
                    } else {
                        "uncertain"
                    }
                    .to_owned(),
                    observation_status: if historical {
                        "historical"
                    } else if predecessor.is_some() {
                        "new"
                    } else {
                        "uncertain"
                    }
                    .to_owned(),
                    source_position: position,
                    policy_version: POLICY_VERSION,
                });
            }
        }
        // A full-history replay supplies its own context. For a known explicit
        // predecessor, overlapping messages are already in its finite window.
        if (!use_predecessor_window || position >= overlap)
            && !(incremental && predecessor.is_some() && known_anchor.is_some())
            && let Some(message) = item.dialogue
        {
            window.push(message);
        }
        anchors.push(anchor);
    }
    let (history_length, history_hash) = if incremental {
        let initial = predecessor
            .map(|value| (value.history_length, value.history_hash.as_str()))
            .unwrap_or((0, ""));
        (initial.0 + incoming.len(), chain_hash(initial.1, &incoming))
    } else {
        (incoming.len(), chain_hash("", &incoming))
    };
    trim_anchors(&mut anchors);
    if !incremental || predecessor.is_some() || state.anchors.is_empty() {
        result.state.anchors = anchors.clone();
        result.state.history_length = history_length;
        result.state.history_hash.clone_from(&history_hash);
    }
    result.response_context = Some(ResponseContext {
        scope,
        response_id: None,
        anchors,
        history_length,
        history_hash,
        window: limit_window(&window),
        context_gap,
    });
    result
}

pub fn apply_response(
    state: &mut SessionState,
    context: &ResponseContext,
    snapshot: &ResponseSnapshot,
) {
    if snapshot.response_id.is_empty()
        || state
            .scope
            .as_deref()
            .is_some_and(|scope| scope != context.scope)
    {
        return;
    }
    state.scope = Some(context.scope.clone());
    let mut response = context.clone();
    response.response_id = Some(snapshot.response_id.clone());
    response.context_gap |= !snapshot.complete;
    for message in &snapshot.messages {
        let anchor = Anchor {
            fingerprint: dialogue_fingerprint(message),
            message_id: None,
            identity: None,
        };
        response.history_hash = chain_hash(&response.history_hash, std::slice::from_ref(&anchor));
        response.history_length += 1;
        response.anchors.push(anchor);
        response.window.push(message.clone());
    }
    trim_anchors(&mut response.anchors);
    response.window = limit_window(&response.window);
    state
        .responses
        .retain(|value| value.response_id != response.response_id || value.scope != response.scope);
    state.responses.push(response);
    if state.responses.len() > MAX_RESPONSES {
        state
            .responses
            .drain(..state.responses.len() - MAX_RESPONSES);
    }
}

fn trim_anchors(anchors: &mut Vec<Anchor>) {
    if anchors.len() > MAX_ANCHORS {
        anchors.drain(..anchors.len() - MAX_ANCHORS);
    }
}

fn aligned_prefix(known: &[Anchor], incoming: &[Anchor]) -> usize {
    for count in (1..=known.len().min(incoming.len())).rev() {
        let suffix = &known[known.len() - count..];
        if suffix.iter().zip(incoming).all(|(left, right)| {
            left.fingerprint == right.fingerprint
                && (left.message_id.is_none()
                    || right.message_id.is_none()
                    || left.message_id == right.message_id)
        }) && (count > 1 || count == known.len() || incoming[0].message_id.is_some())
        {
            return count;
        }
    }
    0
}

fn verified_prefix(incoming: &[Anchor], length: usize, expected: &str) -> usize {
    if length > 0
        && incoming.len() >= length
        && !expected.is_empty()
        && chain_hash("", &incoming[..length]) == expected
    {
        length
    } else {
        0
    }
}

fn chain_hash(initial: &str, anchors: &[Anchor]) -> String {
    anchors.iter().fold(initial.to_owned(), |previous, anchor| {
        hash(&json!([previous, anchor.fingerprint]))
    })
}

struct ParsedMessage {
    role: Option<String>,
    turn_id: Option<String>,
    blocks: Vec<ContentBlock>,
    input_kinds: Vec<Option<String>>,
    dialogue: Option<ContextMessage>,
    anchor: Anchor,
}

fn parse_message(item: &Value) -> ParsedMessage {
    let role = item.get("role").and_then(Value::as_str).map(str::to_owned);
    let meta = item.get("internal_chat_message_metadata_passthrough");
    let turn_id = meta
        .and_then(|value| value.get("turn_id"))
        .and_then(Value::as_str);
    let raw: Vec<Value> = match item.get("content") {
        Some(Value::Array(blocks)) => blocks.clone(),
        Some(Value::String(text)) => vec![json!({"type":"input_text","text":text})],
        _ => vec![],
    };
    let kinds = meta
        .and_then(|value| value.get("content_item_kinds"))
        .and_then(Value::as_array);
    let aligned =
        kinds.is_some_and(|kinds| kinds.len() == raw.len() && kinds.iter().all(Value::is_string));
    let mut blocks = vec![];
    let mut input_kinds = vec![];
    for (position, block) in raw.iter().enumerate() {
        let kind = aligned.then(|| kinds.unwrap()[position].as_str().unwrap());
        if role.as_deref() == Some("user") && kind.is_some_and(excluded_kind) {
            continue;
        }
        if let Some(text) = block
            .get("text")
            .or_else(|| block.get("refusal"))
            .and_then(Value::as_str)
        {
            let block_type = block
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("input_text");
            if matches!(
                block_type,
                "input_text" | "output_text" | "text" | "refusal"
            ) {
                blocks.push(ContentBlock::Text {
                    text: text.to_owned(),
                });
                input_kinds.push(bounded(kind, 256));
                continue;
            }
        }
        if role.as_deref() == Some("user") {
            let media = block
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            blocks.push(ContentBlock::Nontext {
                media_type: bounded(Some(media), 80).unwrap_or_else(|| "unknown".to_owned()),
                description: bounded(block.get("description").and_then(Value::as_str), 512),
            });
            input_kinds.push(bounded(kind, 256));
        }
    }
    let dialogue_blocks: Vec<ContentBlock> = blocks
        .iter()
        .filter(|block| matches!(block, ContentBlock::Text { .. }))
        .cloned()
        .collect();
    let dialogue =
        if matches!(role.as_deref(), Some("user" | "assistant")) && !dialogue_blocks.is_empty() {
            Some(ContextMessage {
                role: role.clone().unwrap(),
                phase: bounded(item.get("phase").and_then(Value::as_str), 80),
                truncated: false,
                blocks: dialogue_blocks,
            })
        } else {
            None
        };
    let fingerprint = if role.as_deref() == Some("user") {
        // Classifications can disappear in later retransmissions. Identity
        // follows the observed message content, not the filtering projection.
        let identity_blocks: Vec<Value> = raw
            .iter()
            .map(|block| {
                if let Some(text) = block.get("text").and_then(Value::as_str)
                    && matches!(
                        block
                            .get("type")
                            .and_then(Value::as_str)
                            .unwrap_or("input_text"),
                        "input_text" | "output_text" | "text"
                    )
                {
                    json!({"kind":"text","text":text})
                } else {
                    json!({"kind":"nontext","fingerprint":hash(block)})
                }
            })
            .collect();
        hash(&json!([role, identity_blocks]))
    } else if let Some(dialogue) = &dialogue {
        dialogue_fingerprint(dialogue)
    } else {
        hash(&json!([
            item.get("type"),
            item.get("call_id"),
            item.get("id"),
            item.get("name"),
            item.get("output")
        ]))
    };
    ParsedMessage {
        role,
        turn_id: bounded(turn_id, 256).filter(|value| !value.is_empty()),
        blocks,
        input_kinds,
        dialogue,
        anchor: Anchor {
            fingerprint,
            message_id: bounded(item.get("id").and_then(Value::as_str), 256)
                .filter(|value| !value.is_empty()),
            identity: None,
        },
    }
}

fn excluded_kind(kind: &str) -> bool {
    matches!(
        kind,
        "agents_md.instructions"
            | "environments.environment_context"
            | "multi_agent.subagent_notification"
            | "user.heartbeat"
    )
}

fn dialogue_fingerprint(message: &ContextMessage) -> String {
    // Phase may appear only in the final response; it is not input identity.
    hash(&json!([message.role, message.blocks]))
}

fn hash(value: &Value) -> String {
    let mut hash = Sha256::new();
    hash.update(value.to_string().as_bytes());
    format!("{:x}", hash.finalize())
}

pub fn limit_window(messages: &[ContextMessage]) -> Vec<ContextMessage> {
    let mut window: Vec<_> = messages
        .iter()
        .rev()
        .take(WINDOW_MESSAGES)
        .cloned()
        .collect();
    window.reverse();
    let count = |items: &[ContextMessage]| -> usize {
        items
            .iter()
            .flat_map(|item| &item.blocks)
            .map(|block| match block {
                ContentBlock::Text { text } => text.chars().count(),
                ContentBlock::Nontext { .. } => 0,
            })
            .sum()
    };
    while window.len() > 1 && count(&window) > WINDOW_CHARACTERS {
        let position = window
            .iter()
            .position(|item| item.role == "user")
            .unwrap_or(0);
        window.remove(position);
    }
    if count(&window) > WINDOW_CHARACTERS {
        let message = &mut window[0];
        let mut remaining = WINDOW_CHARACTERS;
        let mut suffix = vec![];
        for block in message.blocks.iter().rev() {
            if let ContentBlock::Text { text } = block {
                if remaining == 0 {
                    break;
                }
                let length = text.chars().count();
                let text = text
                    .chars()
                    .skip(length.saturating_sub(remaining))
                    .collect::<String>();
                remaining = remaining.saturating_sub(length);
                suffix.push(ContentBlock::Text { text });
            }
        }
        suffix.reverse();
        message.blocks = suffix;
        message.truncated = true;
    }
    window
}

fn bounded(value: Option<&str>, maximum: usize) -> Option<String> {
    value
        .filter(|value| !value.contains('\0') && value.chars().count() <= maximum)
        .map(str::to_owned)
}

fn metadata(observation: &RequestObservation) -> Metadata {
    let flat = observation.body.get("client_metadata");
    let canonical = flat
        .and_then(|value| value.get("x-codex-turn-metadata"))
        .and_then(Value::as_str)
        .and_then(|text| serde_json::from_str::<Value>(text).ok())
        .filter(Value::is_object);
    let header_metadata = observation
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("x-codex-turn-metadata"))
        .and_then(|(_, text)| serde_json::from_str::<Value>(text).ok())
        .filter(Value::is_object);
    let current_flat_source = [
        "thread_source",
        "subagent_kind",
        "request_kind",
        "turn_trigger",
        "x-openai-subagent",
    ]
    .iter()
    .any(|name| {
        flat.and_then(|value| value.get(name))
            .and_then(Value::as_str)
            .is_some()
    });
    // Current message metadata is authoritative over WebSocket handshake headers.
    let field = |names: &[&str]| -> Option<String> {
        if let Some(object) = &canonical {
            return names
                .iter()
                .find_map(|name| object.get(name))
                .and_then(source_string);
        }
        names
            .iter()
            .find_map(|name| {
                flat.and_then(|value| value.get(name))
                    .and_then(Value::as_str)
            })
            .map(str::to_owned)
            .or_else(|| {
                if observation.incremental && current_flat_source {
                    return None;
                }
                names
                    .iter()
                    .find_map(|name| header_metadata.as_ref().and_then(|value| value.get(name)))
                    .and_then(source_string)
                    .or_else(|| {
                        names.iter().find_map(|name| {
                            observation
                                .headers
                                .iter()
                                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                                .map(|(_, value)| value.clone())
                        })
                    })
            })
    };
    let thread_id = canonical
        .as_ref()
        .and_then(|value| value.get("thread_id"))
        .and_then(Value::as_str)
        .or_else(|| {
            flat.and_then(|value| value.get("thread_id"))
                .and_then(Value::as_str)
        })
        .or_else(|| {
            header_metadata
                .as_ref()
                .and_then(|value| value.get("thread_id"))
                .and_then(Value::as_str)
        });
    let session_id = canonical
        .as_ref()
        .and_then(|value| value.get("session_id"))
        .and_then(Value::as_str)
        .or_else(|| {
            flat.and_then(|value| value.get("session_id"))
                .and_then(Value::as_str)
        })
        .or_else(|| {
            header_metadata
                .as_ref()
                .and_then(|value| value.get("session_id"))
                .and_then(Value::as_str)
        })
        .or_else(|| {
            observation
                .headers
                .iter()
                .find(|(name, _)| {
                    name.eq_ignore_ascii_case("session_id")
                        || name.eq_ignore_ascii_case("x-codex-session-id")
                })
                .map(|(_, value)| value.as_str())
        });
    let source = field(&["thread_source", "x-codex-thread-source"]);
    let kind = field(&["request_kind", "x-codex-request-kind"]);
    let trigger = field(&["turn_trigger", "x-codex-turn-trigger"]);
    let compatible_subagent = field(&["x-openai-subagent"])
        .or_else(|| {
            flat.and_then(|value| value.get("x-openai-subagent"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .or_else(|| {
            // Current WS source classification supersedes stale handshake headers;
            // HTTP compatibility headers remain evidence even beside canonical JSON.
            (!observation.incremental || (canonical.is_none() && !current_flat_source))
                .then(|| {
                    observation
                        .headers
                        .iter()
                        .find(|(key, _)| key.eq_ignore_ascii_case("x-openai-subagent"))
                        .map(|(_, value)| value.clone())
                })
                .flatten()
        });
    let excluded = field(&["subagent_kind", "x-codex-subagent-kind"])
        .is_some_and(|value| !value.is_empty())
        || compatible_subagent.is_some_and(|value| !value.is_empty())
        || source.as_deref().is_some_and(internal_source)
        || kind.as_deref().is_some_and(internal_source)
        || trigger.as_deref().is_some_and(internal_source);
    let repository_source = canonical.as_ref().or_else(|| {
        (!observation.incremental)
            .then_some(header_metadata.as_ref())
            .flatten()
    });
    let repositories = repository_source
        .and_then(|value| value.get("workspaces"))
        .and_then(parse_repositories);
    Metadata {
        thread_id: bounded(thread_id, 256).filter(|value| !value.is_empty()),
        session_id: bounded(session_id, 256).filter(|value| !value.is_empty()),
        repositories,
        excluded,
    }
}

fn internal_source(source: &str) -> bool {
    matches!(
        source,
        "subagent"
            | "compaction"
            | "compact"
            | "memory"
            | "memory_consolidation"
            | "guardian_review"
            | "guardian"
            | "review"
    ) || serde_json::from_str::<Value>(source)
        .ok()
        .is_some_and(|value| value.get("subagent").is_some() || value.get("SubAgent").is_some())
}

fn source_string(value: &Value) -> Option<String> {
    value.as_str().map(str::to_owned).or_else(|| {
        (value.get("subagent").is_some() || value.get("SubAgent").is_some())
            .then(|| value.to_string())
    })
}

fn parse_repositories(workspaces: &Value) -> Option<Vec<Repository>> {
    let mut repositories = vec![];
    for (root_path, workspace) in workspaces.as_object()? {
        if root_path.contains('\0') || root_path.chars().count() > 4096 {
            return None;
        }
        let remotes = match workspace.get("associated_remote_urls") {
            None | Some(Value::Null) => None,
            Some(value) => {
                let mut remotes = vec![];
                for (name, remote) in value.as_object()? {
                    let remote = remote.as_str()?;
                    if name.chars().count() > 256 || remote.chars().count() > 8192 {
                        return None;
                    }
                    remotes.push(Remote {
                        name: name.clone(),
                        url: sanitize_remote(remote)?,
                    });
                }
                Some(remotes)
            }
        };
        repositories.push(Repository {
            root_path: root_path.clone(),
            remotes,
        });
    }
    (serde_json::to_vec(&repositories).ok()?.len() <= MAX_METADATA_BYTES).then_some(repositories)
}

/// Only Git's conventional SSH/SCP `git` username is a non-secret locator.
pub fn sanitize_remote(remote: &str) -> Option<String> {
    if remote.chars().any(char::is_control) {
        return None;
    }
    if remote.contains("://") {
        let mut url = url::Url::parse(remote).ok()?;
        if url.username().is_empty() && url.password().is_none() {
            return Some(remote.to_owned());
        }
        let keep_git = url.scheme() == "ssh" && url.username() == "git";
        url.set_username(if keep_git { "git" } else { "" }).ok()?;
        url.set_password(None).ok()?;
        return Some(url.to_string());
    }
    if let Some((userinfo, rest)) = remote.split_once('@') {
        return Some(if userinfo == "git" {
            remote.to_owned()
        } else {
            rest.to_owned()
        });
    }
    Some(remote.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observation(request: &str, input: Value) -> RequestObservation {
        RequestObservation {
            key_id: "key-a".to_owned(),
            request_id: request.to_owned(),
            body: json!({"input":input,"client_metadata":{"thread_id":"thread-a"}}),
            headers: BTreeMap::new(),
            observed_at: DateTime::parse_from_rfc3339("2026-10-09T12:00:00.000Z")
                .unwrap()
                .with_timezone(&Utc),
            key_name: Some("name-a".to_owned()),
            key_name_observed_at: None,
            incremental: false,
        }
    }

    fn user(text: &str) -> Value {
        json!({"role":"user","content":[{"type":"input_text","text":text}]})
    }
    fn assistant(text: &str) -> Value {
        json!({"role":"assistant","content":[{"type":"output_text","text":text}],"phase":"commentary"})
    }
    fn text(message: &ContextMessage) -> String {
        message
            .blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn responded(
        prepared: &PreparedCollection,
        id: &str,
        answer: &str,
        complete: bool,
    ) -> SessionState {
        let mut state = prepared.state.clone();
        apply_response(
            &mut state,
            prepared.response_context.as_ref().unwrap(),
            &ResponseSnapshot {
                response_id: id.to_owned(),
                messages: vec![ContextMessage {
                    role: "assistant".to_owned(),
                    phase: Some("final_answer".to_owned()),
                    truncated: false,
                    blocks: vec![ContentBlock::Text {
                        text: answer.to_owned(),
                    }],
                }],
                complete,
            },
        );
        state
    }

    #[test]
    fn historical_retransmission_and_repeated_identical_new_inputs_have_distinct_positions() {
        let first = collect(
            &SessionState::default(),
            &observation("req-1", json!([user("请继续")])),
        );
        assert_eq!(first.audits.len(), 1);
        let replay = collect(&first.state, &observation("req-2", json!([user("请继续")])));
        assert!(replay.audits.is_empty());
        let append = collect(
            &replay.state,
            &observation("req-3", json!([user("请继续"), user("请继续")])),
        );
        assert_eq!(append.audits.len(), 1);
        assert_ne!(first.audits[0].identity, append.audits[0].identity);
        assert_eq!(append.audits[0].context.len(), 1);
    }

    #[test]
    fn message_turn_is_preserved_without_becoming_identity_or_request_fallback() {
        let mut first_message = user("a");
        first_message["internal_chat_message_metadata_passthrough"] =
            json!({"turn_id":"same-turn"});
        let mut second_message = user("b");
        second_message["internal_chat_message_metadata_passthrough"] =
            json!({"turn_id":"same-turn"});
        let mut request = observation("req", json!([first_message, second_message, user("c")]));
        request.body["client_metadata"]["x-codex-turn-metadata"] =
            json!(json!({"thread_id":"thread-a","turn_id":"request-turn"}).to_string());
        let result = collect(&SessionState::default(), &request);
        assert_eq!(result.audits.len(), 3);
        assert_eq!(result.audits[0].turn_id.as_deref(), Some("same-turn"));
        assert_eq!(result.audits[1].turn_id.as_deref(), Some("same-turn"));
        assert_eq!(result.audits[2].turn_id, None);
        assert_ne!(result.audits[0].identity, result.audits[1].identity);
    }

    #[test]
    fn each_input_uses_its_own_predecessors_and_later_output_cannot_rewrite_snapshot() {
        let result = collect(
            &SessionState::default(),
            &observation(
                "req",
                json!([
                    user("first"),
                    assistant("plan"),
                    user("second"),
                    assistant("later"),
                    user("third")
                ]),
            ),
        );
        assert_eq!(result.audits[0].context.len(), 0);
        assert_eq!(
            result.audits[1]
                .context
                .iter()
                .map(text)
                .collect::<Vec<_>>(),
            vec!["first", "plan"]
        );
        assert_eq!(
            result.audits[2]
                .context
                .iter()
                .map(text)
                .collect::<Vec<_>>(),
            vec!["first", "plan", "second", "later"]
        );
        assert_eq!(result.audits[0].observation_status, "historical");
        let _ = responded(&result, "response", "after all inputs", true);
        assert!(result.audits[0].context.is_empty());
        assert_eq!(result.audits[1].context.len(), 2);
    }

    #[test]
    fn websocket_prewarm_collects_input_and_explicit_incremental_predecessor_supplies_context() {
        let mut prewarm = observation("warm", json!([user("please work")]));
        prewarm.incremental = true;
        prewarm.body["generate"] = json!(false);
        let prepared = collect(&SessionState::default(), &prewarm);
        assert_eq!(prepared.audits.len(), 1);
        assert!(prepared.audits[0].identity.is_none());
        let state = responded(&prepared, "warm-response", "plan", true);
        let mut delta = observation("delta", json!([user("请继续")]));
        delta.incremental = true;
        delta.body["previous_response_id"] = json!("warm-response");
        let next = collect(&state, &delta);
        assert_eq!(next.audits.len(), 1);
        assert_eq!(
            next.audits[0].context.iter().map(text).collect::<Vec<_>>(),
            vec!["please work", "plan"]
        );
        let state = responded(&next, "next-response", "more", true);
        delta.request_id = "delta2".to_owned();
        delta.body["previous_response_id"] = json!("next-response");
        let repeated_text_new_input = collect(&state, &delta);
        assert_eq!(repeated_text_new_input.audits.len(), 1);
        assert_ne!(
            next.audits[0].identity,
            repeated_text_new_input.audits[0].identity
        );
    }

    #[test]
    fn unknown_incremental_predecessor_never_uses_latest_response() {
        let prepared = collect(
            &SessionState::default(),
            &observation("req", json!([user("a")])),
        );
        let state = responded(&prepared, "known", "secret previous plan", true);
        let mut request = observation("req2", json!([user("continue")]));
        request.incremental = true;
        request.body["previous_response_id"] = json!("unknown");
        let result = collect(&state, &request);
        assert!(result.audits[0].context.is_empty());
        assert!(result.audits[0].context_gap);
        assert!(result.audits[0].identity.is_none());
    }

    #[test]
    fn a_partial_response_marks_context_gap_and_complete_snapshot_replaces_it() {
        let prepared = collect(
            &SessionState::default(),
            &observation("req", json!([user("a")])),
        );
        let mut state = responded(&prepared, "r", "partial", false);
        assert!(state.responses[0].context_gap);
        apply_response(
            &mut state,
            prepared.response_context.as_ref().unwrap(),
            &ResponseSnapshot {
                response_id: "r".to_owned(),
                messages: vec![ContextMessage {
                    role: "assistant".to_owned(),
                    phase: None,
                    truncated: false,
                    blocks: vec![ContentBlock::Text {
                        text: "complete".to_owned(),
                    }],
                }],
                complete: true,
            },
        );
        assert_eq!(state.responses.len(), 1);
        assert!(!state.responses[0].context_gap);
        assert_eq!(
            state.responses[0]
                .window
                .iter()
                .map(text)
                .collect::<Vec<_>>(),
            vec!["a", "complete"]
        );
    }

    #[test]
    fn state_scope_prevents_cross_key_and_cross_thread_reuse() {
        let prepared = collect(
            &SessionState::default(),
            &observation("req", json!([user("same")])),
        );
        let state = responded(&prepared, "r", "private", true);
        for (key, thread) in [("key-b", "thread-a"), ("key-a", "thread-b")] {
            let mut request = observation("req2", json!([user("same")]));
            request.key_id = key.to_owned();
            request.body["client_metadata"]["thread_id"] = json!(thread);
            request.body["previous_response_id"] = json!("r");
            let result = collect(&state, &request);
            assert_eq!(result.audits.len(), 1);
            assert!(result.audits[0].context.is_empty());
        }
    }

    #[test]
    fn no_session_identity_limits_context_to_the_actual_request() {
        let mut request = observation("req", json!([user("same")]));
        request.body["client_metadata"] = json!({"prompt_cache_key":"not-a-session"});
        let first = collect(&SessionState::default(), &request);
        request.request_id = "req2".to_owned();
        let second = collect(&first.state, &request);
        assert_eq!(second.audits.len(), 1);
        assert_ne!(first.audits[0].scope, second.audits[0].scope);
        assert!(second.audits[0].thread_id.is_none());
    }

    #[test]
    fn stable_ids_and_ordered_anchors_survive_serialized_restart() {
        let mut message = user("a");
        message["id"] = json!("stable-user-id");
        let first = collect(
            &SessionState::default(),
            &observation("req", json!([message.clone(), assistant("answer")])),
        );
        let serialized = serde_json::to_vec(&first.state).unwrap();
        let state: SessionState = serde_json::from_slice(&serialized).unwrap();
        let mut request = observation("req2", json!([message]));
        request.incremental = true;
        assert!(collect(&state, &request).audits.is_empty());
        assert_eq!(
            first.audits[0].identity.as_deref(),
            Some("message:stable-user-id")
        );
    }

    #[test]
    fn filtering_removes_only_classified_injected_blocks_and_aligns_retained_kinds() {
        let request = observation(
            "req",
            json!([{
                "role":"user","content":[
                    {"type":"input_text","text":"injected"},
                    {"type":"input_text","text":"AGENTS.md quoted by user"},
                    {"type":"input_text","text":"automatic"},
                    {"type":"input_text","text":"answer"}
                ],"internal_chat_message_metadata_passthrough":{"content_item_kinds":["agents_md.instructions","new.unknown.kind","user.heartbeat","user.answered_question"]}
            }]),
        );
        let result = collect(&SessionState::default(), &request);
        assert_eq!(result.audits[0].blocks.len(), 2);
        assert_eq!(
            result.audits[0].input_kinds,
            vec![
                Some("new.unknown.kind".to_owned()),
                Some("user.answered_question".to_owned())
            ]
        );
    }

    #[test]
    fn missing_mismatched_or_invalid_classifications_preserve_candidates_as_null() {
        for kinds in [
            Value::Null,
            json!(["agents_md.instructions"]),
            json!(["agents_md.instructions", null]),
        ] {
            let mut message = user("a");
            message["content"]
                .as_array_mut()
                .unwrap()
                .push(json!({"type":"input_text","text":"b"}));
            message["internal_chat_message_metadata_passthrough"] =
                json!({"content_item_kinds":kinds});
            let result = collect(
                &SessionState::default(),
                &observation("req", json!([message])),
            );
            assert_eq!(result.audits[0].blocks.len(), 2);
            assert_eq!(result.audits[0].input_kinds, vec![None, None]);
        }
    }

    #[test]
    fn explicit_subagent_and_internal_sources_filter_inputs_and_response_registration() {
        for metadata in [
            json!({"subagent_kind":"thread_spawn"}),
            json!({"thread_source":"subagent"}),
            json!({"request_kind":"memory"}),
            json!({"request_kind":"compaction"}),
            json!({"thread_source":"guardian_review"}),
        ] {
            let mut request = observation("req", json!([user("task")]));
            request.body["client_metadata"]["x-codex-turn-metadata"] = json!(metadata.to_string());
            let result = collect(&SessionState::default(), &request);
            assert!(result.audits.is_empty());
            assert!(result.response_context.is_none());
        }
        for marker in [
            "review",
            "compact",
            "memory_consolidation",
            "collab_spawn",
            "custom-task",
        ] {
            let mut request = observation("req", json!([user("task")]));
            request
                .headers
                .insert("X-OpenAI-Subagent".to_owned(), marker.to_owned());
            assert!(
                collect(&SessionState::default(), &request)
                    .audits
                    .is_empty()
            );
        }
    }

    #[test]
    fn current_websocket_source_wins_over_handshake_source_but_conflicts_filter() {
        let mut request = observation("req", json!([user("task")]));
        request.incremental = true;
        request
            .headers
            .insert("X-OpenAI-Subagent".to_owned(), "review".to_owned());
        request.body["client_metadata"]["x-codex-turn-metadata"] =
            json!(json!({"thread_source":"user","thread_id":"thread-a"}).to_string());
        assert_eq!(collect(&SessionState::default(), &request).audits.len(), 1);
        request.body["client_metadata"]["x-openai-subagent"] = json!("review");
        assert!(
            collect(&SessionState::default(), &request)
                .audits
                .is_empty()
        );
    }

    #[test]
    fn workspace_snapshot_distinguishes_absence_empty_and_removes_credentials() {
        let mut request = observation("req", json!([user("task")]));
        assert!(
            collect(&SessionState::default(), &request).audits[0]
                .repositories
                .is_none()
        );
        request.body["client_metadata"]["x-codex-turn-metadata"] =
            json!(json!({"workspaces":{}}).to_string());
        assert_eq!(
            collect(&SessionState::default(), &request).audits[0].repositories,
            Some(vec![])
        );
        request.body["client_metadata"]["x-codex-turn-metadata"] = json!(json!({"workspaces":{
            "/repo":{"associated_remote_urls":{"origin":"https://user:password@example.com/org/repo.git","ssh":"git@github.com:org/repo.git"},"commit_hash":"excluded","has_changes":true},
            "/empty":{}
        }}).to_string());
        let result = collect(&SessionState::default(), &request);
        let repositories = result.audits[0].repositories.as_ref().unwrap();
        assert_eq!(repositories[0].remotes, None);
        assert_eq!(
            repositories[1].remotes.as_ref().unwrap()[0].url,
            "https://example.com/org/repo.git"
        );
        let encoded = serde_json::to_string(&result.audits[0]).unwrap();
        assert!(!encoded.contains("password"));
        assert!(!encoded.contains("commit_hash"));
    }

    #[test]
    fn nontext_positions_are_preserved_and_distinct_media_do_not_deduplicate() {
        let first = collect(
            &SessionState::default(),
            &observation(
                "req",
                json!([{
                    "role":"user","content":[{"type":"input_image","image_url":"https://example.com/a.png","description":"diagram"}]
                }]),
            ),
        );
        assert_eq!(
            first.audits[0].blocks,
            vec![ContentBlock::Nontext {
                media_type: "input_image".to_owned(),
                description: Some("diagram".to_owned())
            }]
        );
        let second = collect(
            &first.state,
            &observation(
                "req2",
                json!([{
                    "role":"user","content":[{"type":"input_image","image_url":"https://example.com/b.png","description":"diagram"}]
                }]),
            ),
        );
        assert_eq!(second.audits.len(), 1);
        assert!(
            !serde_json::to_string(&second.audits[0])
                .unwrap()
                .contains("b.png")
        );
    }

    #[test]
    fn unicode_window_budget_discards_users_first_then_retains_assistant_suffix() {
        let message = |role: &str, body: &str| ContextMessage {
            role: role.to_owned(),
            phase: None,
            truncated: false,
            blocks: vec![ContentBlock::Text {
                text: body.to_owned(),
            }],
        };
        let long = "🙂".repeat(WINDOW_CHARACTERS + 10);
        let window = limit_window(&[message("user", "old input"), message("assistant", &long)]);
        assert_eq!(window.len(), 1);
        assert!(window[0].truncated);
        assert_eq!(text(&window[0]).chars().count(), WINDOW_CHARACTERS);
        assert_eq!(text(&window[0]), "🙂".repeat(WINDOW_CHARACTERS));
    }

    #[test]
    fn finite_response_and_anchor_state_does_not_become_a_replay_log() {
        let request = observation(
            "req",
            Value::Array((0..400).map(|index| user(&index.to_string())).collect()),
        );
        let prepared = collect(&SessionState::default(), &request);
        assert_eq!(prepared.audits.len(), 400);
        assert_eq!(prepared.state.anchors.len(), MAX_ANCHORS);
        let mut state = prepared.state.clone();
        for index in 0..100 {
            apply_response(
                &mut state,
                prepared.response_context.as_ref().unwrap(),
                &ResponseSnapshot {
                    response_id: index.to_string(),
                    messages: vec![],
                    complete: true,
                },
            );
        }
        assert_eq!(state.responses.len(), MAX_RESPONSES);
    }

    #[test]
    fn full_history_replay_larger_than_the_anchor_window_keeps_occurrences_and_deduplicates() {
        let history: Vec<Value> = (0..400).map(|_| user("请继续")).collect();
        let first = collect(
            &SessionState::default(),
            &observation("first", json!(history)),
        );
        let state: SessionState =
            serde_json::from_str(&serde_json::to_string(&first.state).unwrap()).unwrap();
        let replay = collect(&state, &observation("replay", json!(history)));
        assert!(replay.audits.is_empty());
        let mut appended = history;
        appended.push(user("请继续"));
        let next = collect(&replay.state, &observation("next", json!(appended)));
        assert_eq!(next.audits.len(), 1);
        assert_eq!(next.audits[0].source_position, 400);
    }

    #[test]
    fn unknown_automatic_named_classification_is_not_invented_as_an_exclusion() {
        let mut message = user("quoted automatic event");
        message["internal_chat_message_metadata_passthrough"] =
            json!({"content_item_kinds":["user.automatic_future"]});
        let result = collect(
            &SessionState::default(),
            &observation("req", json!([message])),
        );
        assert_eq!(result.audits.len(), 1);
        assert_eq!(
            result.audits[0].input_kinds,
            vec![Some("user.automatic_future".to_owned())]
        );
    }

    #[test]
    fn canonical_compatibility_header_is_parsed_and_current_flat_websocket_source_wins() {
        let mut request = observation("req", json!([user("task")]));
        request.headers.insert(
            "X-Codex-Turn-Metadata".to_owned(),
            json!({"thread_id":"header-thread","request_kind":"compaction"}).to_string(),
        );
        assert!(
            collect(&SessionState::default(), &request)
                .audits
                .is_empty()
        );
        request.incremental = true;
        request.body["client_metadata"]["thread_source"] = json!("user");
        assert_eq!(collect(&SessionState::default(), &request).audits.len(), 1);
        request.body["client_metadata"]["x-codex-turn-metadata"] =
            json!(json!({"subagent_kind":null,"request_kind":42}).to_string());
        assert_eq!(collect(&SessionState::default(), &request).audits.len(), 1);
    }

    #[test]
    fn remote_credentials_are_removed_without_breaking_git_or_local_locators() {
        for (raw, expected) in [
            (
                "ghp_secret@github.com:org/repo.git",
                "github.com:org/repo.git",
            ),
            (
                "user:password@github.com:org/repo.git",
                "github.com:org/repo.git",
            ),
            ("git@github.com:org/repo.git", "git@github.com:org/repo.git"),
            (
                "ssh://git@github.com/org/repo.git",
                "ssh://git@github.com/org/repo.git",
            ),
            ("file:///repo.git", "file:///repo.git"),
        ] {
            assert_eq!(sanitize_remote(raw).as_deref(), Some(expected));
        }
    }

    #[test]
    fn invalid_optional_identifiers_do_not_reject_the_input() {
        let mut request = observation("req", json!([user("body\0preserved")]));
        request.body["client_metadata"] =
            json!({"thread_id":"bad\0thread","session_id":"bad\0session"});
        request.body["input"][0]["internal_chat_message_metadata_passthrough"] =
            json!({"turn_id":"bad\0turn"});
        let result = collect(&SessionState::default(), &request);
        assert_eq!(result.audits.len(), 1);
        assert!(result.audits[0].thread_id.is_none());
        assert!(result.audits[0].session_id.is_none());
        assert!(result.audits[0].turn_id.is_none());
        assert_eq!(
            result.audits[0].blocks,
            vec![ContentBlock::Text {
                text: "body\0preserved".to_owned()
            }]
        );
    }

    #[test]
    fn full_visible_history_is_windowed_at_each_input_boundary() {
        let mut input = vec![assistant(&"🙂".repeat(WINDOW_CHARACTERS + 1))];
        input.extend((0..13).map(|index| user(&format!("input {index}"))));
        input.push(user("current"));
        let result = collect(&SessionState::default(), &observation("req", json!(input)));
        let current = result.audits.last().unwrap();
        assert_eq!(current.context.len(), WINDOW_MESSAGES);
        assert_eq!(text(&current.context[0]), "input 1");
        assert!(current.context.iter().all(|message| message.role == "user"));
    }

    #[test]
    fn removal_of_classification_projection_does_not_change_ordered_message_identity() {
        let mut message = user("injected");
        message["content"]
            .as_array_mut()
            .unwrap()
            .push(json!({"type":"input_text","text":"actual input"}));
        message["internal_chat_message_metadata_passthrough"] =
            json!({"content_item_kinds":["agents_md.instructions","user.text"]});
        let first = collect(
            &SessionState::default(),
            &observation("req", json!([message.clone()])),
        );
        assert_eq!(first.audits[0].blocks.len(), 1);
        message
            .as_object_mut()
            .unwrap()
            .remove("internal_chat_message_metadata_passthrough");
        let replay = collect(&first.state, &observation("replay", json!([message])));
        assert!(replay.audits.is_empty());
    }

    #[test]
    fn many_input_blocks_are_preserved_independently_of_context_and_optional_metadata_limits() {
        let blocks: Vec<Value> = (0..5000)
            .map(|_| json!({"type":"input_text","text":""}))
            .collect();
        let result = collect(
            &SessionState::default(),
            &observation("req", json!([{"role":"user","content":blocks}])),
        );
        assert_eq!(result.audits.len(), 1);
        assert_eq!(result.audits[0].blocks.len(), 5000);
        assert_eq!(result.audits[0].input_kinds.len(), 5000);
    }

    #[test]
    fn unaligned_visible_full_history_cannot_prepend_later_predecessor_messages() {
        let initial = collect(
            &SessionState::default(),
            &observation("initial", json!([user("seen input")])),
        );
        let state = responded(&initial, "r", "later plan", true);
        let mut request = observation(
            "req",
            json!([
                user("earlier unseen history"),
                assistant("earlier explanation"),
                user("seen input"),
                assistant("later plan"),
                user("new input")
            ]),
        );
        request.body["previous_response_id"] = json!("r");
        let result = collect(&state, &request);
        assert!(result.audits[0].context.is_empty());
        assert_eq!(
            result.audits[1]
                .context
                .iter()
                .map(text)
                .collect::<Vec<_>>(),
            vec!["earlier unseen history", "earlier explanation"]
        );
        assert_eq!(
            result
                .audits
                .last()
                .unwrap()
                .context
                .iter()
                .map(text)
                .collect::<Vec<_>>(),
            vec![
                "earlier unseen history",
                "earlier explanation",
                "seen input",
                "later plan"
            ]
        );
    }

    #[test]
    fn http_previous_response_deltas_append_same_text_after_an_empty_response() {
        let first = collect(
            &SessionState::default(),
            &observation("first", json!([user("请继续")])),
        );
        let mut state = first.state.clone();
        apply_response(
            &mut state,
            first.response_context.as_ref().unwrap(),
            &ResponseSnapshot {
                response_id: "r".to_owned(),
                messages: vec![],
                complete: true,
            },
        );
        let mut request = observation("next", json!([user("请继续")]));
        request.body["previous_response_id"] = json!("r");
        let next = collect(&state, &request);
        assert_eq!(next.audits.len(), 1);
        assert_ne!(first.audits[0].identity, next.audits[0].identity);
        let replay = collect(
            &next.state,
            &observation("full-replay", json!([user("请继续"), user("请继续")])),
        );
        assert!(replay.audits.is_empty());
    }
}
