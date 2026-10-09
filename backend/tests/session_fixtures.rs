//! The UI demo and production collector consume the same anonymized, fixed
//! multi-turn Codex sessions. No local session files are read by these tests.
use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use codex_proxy_audit::collection::{
    AuditDraft, ContentBlock, ContextMessage, RequestObservation, ResponseSnapshot, SessionState,
    apply_response, collect,
};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize)]
struct Fixtures {
    audits: Vec<FixtureAudit>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FixtureAudit {
    id: String,
    message_id: String,
    key_id: String,
    key_name: Option<String>,
    thread_id: Option<String>,
    session_id: Option<String>,
    turn_id: Option<String>,
    input: Vec<FixtureBlock>,
    context: Vec<FixtureMessage>,
}

#[derive(Deserialize)]
struct FixtureMessage {
    id: String,
    role: String,
    phase: Option<String>,
    blocks: Vec<FixtureBlock>,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum FixtureBlock {
    Text {
        text: String,
        #[serde(rename = "inputKind", default)]
        input_kind: Option<String>,
    },
    Nontext {
        #[serde(rename = "mediaType")]
        media_type: String,
        description: Option<String>,
        #[serde(rename = "inputKind", default)]
        input_kind: Option<String>,
    },
}

impl FixtureBlock {
    fn content(&self) -> ContentBlock {
        match self {
            Self::Text { text, .. } => ContentBlock::Text { text: text.clone() },
            Self::Nontext {
                media_type,
                description,
                ..
            } => ContentBlock::Nontext {
                media_type: media_type.clone(),
                description: description.clone(),
            },
        }
    }

    fn input_kind(&self) -> Option<String> {
        match self {
            Self::Text { input_kind, .. } | Self::Nontext { input_kind, .. } => input_kind.clone(),
        }
    }

    fn wire(&self, role: &str) -> Value {
        match self {
            Self::Text { text, .. } => json!({
                "type": if role == "assistant" { "output_text" } else { "input_text" },
                "text": text,
            }),
            Self::Nontext {
                media_type,
                description,
                ..
            } => json!({
                "type": media_type,
                "description": description,
            }),
        }
    }
}

fn fixtures() -> Fixtures {
    serde_json::from_str(include_str!("../../tests/fixtures/codex-sessions.json"))
        .expect("the shared, anonymized Codex fixture must remain valid")
}

fn observation(audit: &FixtureAudit, request_id: String) -> RequestObservation {
    let mut input: Vec<Value> = audit.context.iter().map(|message| json!({
        "id":message.id,
        "role":message.role,
        "phase":message.phase,
        "content":message.blocks.iter().map(|block| block.wire(&message.role)).collect::<Vec<_>>(),
    })).collect();
    input.push(json!({
        "id":audit.message_id,
        "role":"user",
        "content":audit.input.iter().map(|block| block.wire("user")).collect::<Vec<_>>(),
        "internal_chat_message_metadata_passthrough":{
            "turn_id":audit.turn_id,
            "content_item_kinds":audit.input.iter().map(FixtureBlock::input_kind).collect::<Vec<_>>(),
        },
    }));
    RequestObservation {
        key_id: audit.key_id.clone(),
        request_id,
        body: json!({
            "input":input,
            "client_metadata":{"x-codex-turn-metadata":json!({
                "thread_id":audit.thread_id,"session_id":audit.session_id,"thread_source":"user",
            }).to_string()},
        }),
        headers: BTreeMap::new(),
        observed_at: DateTime::parse_from_rfc3339("2026-10-09T12:00:00.000Z")
            .unwrap()
            .with_timezone(&Utc),
        key_name: audit.key_name.clone(),
        key_name_observed_at: None,
        incremental: false,
    }
}

fn assert_snapshot(actual: &AuditDraft, expected: &FixtureAudit) {
    assert_eq!(
        actual.identity.as_deref(),
        Some(format!("message:{}", expected.message_id).as_str())
    );
    assert_eq!(
        actual.blocks,
        expected
            .input
            .iter()
            .map(FixtureBlock::content)
            .collect::<Vec<_>>(),
        "input {}",
        expected.id
    );
    assert_eq!(
        actual.input_kinds,
        expected
            .input
            .iter()
            .map(FixtureBlock::input_kind)
            .collect::<Vec<_>>(),
        "input classifications {}",
        expected.id
    );
    assert_eq!(actual.thread_id, expected.thread_id);
    assert_eq!(actual.session_id, expected.session_id);
    assert_eq!(actual.turn_id, expected.turn_id);
    assert_eq!(
        actual.context.len(),
        expected.context.len(),
        "context {}",
        expected.id
    );
    for (observed, fixture) in actual.context.iter().zip(&expected.context) {
        assert_eq!(observed.role, fixture.role, "context role {}", fixture.id);
        assert_eq!(
            observed.phase, fixture.phase,
            "context phase {}",
            fixture.id
        );
        assert_eq!(
            observed.blocks,
            fixture
                .blocks
                .iter()
                .map(FixtureBlock::content)
                .collect::<Vec<_>>(),
            "context body {}",
            fixture.id
        );
    }
}

#[test]
fn fixed_codex_sessions_preserve_input_context_and_deduplicate_after_restart() {
    let fixtures = fixtures();
    assert!(
        !fixtures.audits.is_empty(),
        "shared fixture must exercise real multi-turn data"
    );
    for audit in &fixtures.audits {
        let request = observation(audit, format!("fixture:{}:first", audit.id));
        let prepared = collect(&SessionState::default(), &request);
        let identity = format!("message:{}", audit.message_id);
        let target = prepared
            .audits
            .iter()
            .find(|draft| draft.identity.as_deref() == Some(&identity))
            .unwrap_or_else(|| panic!("fixture input {} must form an audit", audit.id));
        assert_snapshot(target, audit);

        let mut replay = request.clone();
        replay.request_id = format!("fixture:{}:replay", audit.id);
        assert!(
            collect(&prepared.state, &replay).audits.is_empty(),
            "history replay {}",
            audit.id
        );
        let restarted: SessionState =
            serde_json::from_slice(&serde_json::to_vec(&prepared.state).unwrap()).unwrap();
        replay.request_id = format!("fixture:{}:restart", audit.id);
        assert!(
            collect(&restarted, &replay).audits.is_empty(),
            "restart replay {}",
            audit.id
        );
    }
}

fn adjacent_pair(fixtures: &Fixtures) -> (&FixtureAudit, &FixtureAudit) {
    fixtures
        .audits
        .iter()
        .find_map(|earlier| {
            let known_users: BTreeSet<&str> = earlier
                .context
                .iter()
                .filter(|message| message.role == "user")
                .map(|message| message.id.as_str())
                .chain(std::iter::once(earlier.message_id.as_str()))
                .collect();
            fixtures
                .audits
                .iter()
                .find(|later| {
                    later.message_id != earlier.message_id
                        && later.key_id == earlier.key_id
                        && later.thread_id.is_some()
                        && later.thread_id == earlier.thread_id
                        && later.context.iter().any(|message| {
                            message.role == "user" && message.id == earlier.message_id
                        })
                        && later
                            .context
                            .iter()
                            .filter(|message| message.role == "user")
                            .all(|message| known_users.contains(message.id.as_str()))
                })
                .map(|later| (earlier, later))
        })
        .expect("shared multi-turn fixture must retain an adjacent pair with stable history IDs")
}

#[test]
fn consecutive_fixture_inputs_reuse_history_ids_and_only_add_the_next_input() {
    let fixtures = fixtures();
    let pair = adjacent_pair(&fixtures);
    let first = collect(
        &SessionState::default(),
        &observation(pair.0, "fixture:first-round".to_owned()),
    );
    let later = collect(
        &first.state,
        &observation(pair.1, "fixture:next-round".to_owned()),
    );
    assert_eq!(
        later.audits.len(),
        1,
        "the prior round and its context must not become new inputs"
    );
    assert_snapshot(&later.audits[0], pair.1);
}

#[test]
fn fixture_assistant_response_supplies_the_same_context_to_a_websocket_delta() {
    let fixtures = fixtures();
    let (earlier, later) = adjacent_pair(&fixtures);
    let first = collect(
        &SessionState::default(),
        &observation(earlier, "fixture:ws-first".to_owned()),
    );
    let position = later
        .context
        .iter()
        .position(|message| message.id == earlier.message_id)
        .unwrap();
    let response_messages = &later.context[position + 1..];
    assert!(
        !response_messages.is_empty(),
        "the adjacent fixture pair must include the Agent reply"
    );
    assert!(
        response_messages
            .iter()
            .all(|message| message.role == "assistant")
    );
    let mut state = first.state;
    apply_response(
        &mut state,
        first.response_context.as_ref().unwrap(),
        &ResponseSnapshot {
            response_id: "fixture-protocol-response".to_owned(),
            messages: response_messages
                .iter()
                .map(|message| ContextMessage {
                    role: message.role.clone(),
                    phase: message.phase.clone(),
                    truncated: false,
                    blocks: message.blocks.iter().map(FixtureBlock::content).collect(),
                })
                .collect(),
            complete: true,
        },
    );
    let mut request = observation(later, "fixture:ws-delta".to_owned());
    let target = request.body["input"].as_array_mut().unwrap().pop().unwrap();
    request.body["input"] = json!([target]);
    request.body["previous_response_id"] = json!("fixture-protocol-response");
    request.incremental = true;
    let delta = collect(&state, &request);
    assert_eq!(delta.audits.len(), 1);
    assert_snapshot(&delta.audits[0], later);
}
