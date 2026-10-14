//! 使用共享的匿名会话验证最新输入、请求内上下文与多媒体采集
use std::collections::BTreeMap;
use std::sync::OnceLock;

use chrono::{DateTime, Utc};
use codex_proxy_audit::collection::{
    AuditDraft, ContentBlock, RequestObservation, collect as collect_with_policy,
    exclusions::ExclusionPolicy,
};
use serde::Deserialize;
use serde_json::{Value, json};

fn collect(observation: &RequestObservation) -> Option<AuditDraft> {
    static POLICY: OnceLock<ExclusionPolicy> = OnceLock::new();
    collect_with_policy(observation, POLICY.get_or_init(ExclusionPolicy::default))
}

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
        user_agents: Vec::new(),
        observed_at: DateTime::parse_from_rfc3339("2026-10-09T12:00:00.000Z")
            .unwrap()
            .with_timezone(&Utc),
    }
}

fn assert_snapshot(actual: &AuditDraft, expected: &FixtureAudit) {
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
fn fixed_codex_sessions_preserve_latest_input_and_request_context() {
    let fixtures = fixtures();
    assert!(!fixtures.audits.is_empty());
    for audit in &fixtures.audits {
        let request = observation(audit, format!("fixture:{}", audit.id));
        let draft =
            collect(&request).unwrap_or_else(|| panic!("fixture {} must form an audit", audit.id));
        assert_snapshot(&draft, audit);
    }
}

#[test]
fn repeating_the_same_fixture_request_produces_another_draft() {
    let fixtures = fixtures();
    let audit = &fixtures.audits[0];
    let mut request = observation(audit, "fixture:first".to_owned());
    assert_snapshot(&collect(&request).unwrap(), audit);
    request.request_id = "fixture:repeated".to_owned();
    let repeated = collect(&request).unwrap();
    assert_snapshot(&repeated, audit);
    assert_eq!(repeated.request_id, "fixture:repeated");
}

#[test]
fn delta_fixture_does_not_restore_omitted_context() {
    let fixtures = fixtures();
    let audit = fixtures
        .audits
        .iter()
        .find(|audit| !audit.context.is_empty())
        .unwrap();
    let mut request = observation(audit, "fixture:delta".to_owned());
    let latest = request.body["input"].as_array_mut().unwrap().pop().unwrap();
    request.body["input"] = json!([latest]);
    request.body["previous_response_id"] = json!("previous-response");
    let draft = collect(&request).unwrap();
    assert_eq!(
        draft.blocks,
        audit
            .input
            .iter()
            .map(FixtureBlock::content)
            .collect::<Vec<_>>()
    );
    assert!(draft.context.is_empty());
}

#[test]
fn fixture_tool_continuation_does_not_collect_the_earlier_user_input() {
    let fixtures = fixtures();
    let mut request = observation(&fixtures.audits[0], "fixture:tool".to_owned());
    request.body["input"].as_array_mut().unwrap().push(json!({
        "type":"function_call_output", "call_id":"tool-call", "output":"done"
    }));
    assert!(collect(&request).is_none());
}
