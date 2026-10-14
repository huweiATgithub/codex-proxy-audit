//! 从当前请求提取最新用户输入与有限的前序对话，不执行 I/O 或维护会话状态
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub mod exclusions;
use exclusions::{ExclusionPolicy, RequestFacts, SourceFacts};

pub const WINDOW_MESSAGES: usize = 12;
pub const WINDOW_CHARACTERS: usize = 32_000;
const MAX_METADATA_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug)]
pub struct RequestObservation {
    pub key_id: String,
    pub request_id: String,
    pub body: Value,
    pub headers: BTreeMap<String, String>,
    pub user_agents: Vec<String>,
    pub observed_at: DateTime<Utc>,
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
    pub observed_at: DateTime<Utc>,
    pub request_id: String,
    pub thread_id: Option<String>,
    pub session_id: Option<String>,
    pub turn_id: Option<String>,
    pub blocks: Vec<ContentBlock>,
    pub input_kinds: Vec<Option<String>>,
    pub repositories: Option<Vec<Repository>>,
    pub context: Vec<ContextMessage>,
}

#[derive(Default)]
struct Metadata {
    thread_id: Option<String>,
    session_id: Option<String>,
    repositories: Option<Vec<Repository>>,
}

/// Only the last raw input item can introduce a new audit
pub fn collect(observation: &RequestObservation, policy: &ExclusionPolicy) -> Option<AuditDraft> {
    let mut input_kinds = Vec::new();
    let (blocks, turn_id, earlier) = match observation.body.get("input")? {
        Value::String(text) => (
            vec![ContentBlock::Text { text: text.clone() }],
            None,
            &[][..],
        ),
        Value::Array(items) => {
            let (latest, earlier) = items.split_last()?;
            if message_role(latest) != Some("user") {
                return None;
            }
            let blocks = parse_blocks(latest, Some(&mut input_kinds));
            let turn_id = bounded(
                latest
                    .get("internal_chat_message_metadata_passthrough")
                    .and_then(|value| value.get("turn_id"))
                    .and_then(Value::as_str),
                256,
            )
            .filter(|value| !value.is_empty());
            (blocks, turn_id, earlier)
        }
        _ => return None,
    };
    if blocks.is_empty() {
        return None;
    }
    let automatic_prompt = !input_kinds.is_empty()
        && input_kinds
            .iter()
            .all(|kind| kind.as_deref().is_some_and(automatic_prompt_kind));
    let metadata = policy
        .assess(request_facts(observation, automatic_prompt))
        .ok()?
        .metadata;
    if input_kinds.is_empty() {
        input_kinds.resize(blocks.len(), None);
    } else if serde_json::to_vec(&input_kinds)
        .map_or(true, |bytes| bytes.len() > MAX_METADATA_BYTES)
    {
        input_kinds.fill(None);
    }
    let mut context: Vec<ContextMessage> = earlier
        .iter()
        .rev()
        .filter_map(|item| {
            let role = message_role(item)?;
            if !matches!(role, "user" | "assistant") {
                return None;
            }
            if role == "assistant" && item.get("phase").and_then(Value::as_str) == Some("analysis")
            {
                return None;
            }
            let blocks = parse_blocks(item, None);
            (!blocks.is_empty()).then(|| ContextMessage {
                role: role.to_owned(),
                phase: bounded(item.get("phase").and_then(Value::as_str), 80),
                truncated: false,
                blocks,
            })
        })
        .take(WINDOW_MESSAGES)
        .collect();
    context.reverse();
    Some(AuditDraft {
        key_id: observation.key_id.clone(),
        observed_at: observation.observed_at,
        request_id: observation.request_id.clone(),
        thread_id: metadata.thread_id,
        session_id: metadata.session_id,
        turn_id,
        blocks,
        input_kinds,
        repositories: metadata.repositories,
        context: limit_window(context),
    })
}

fn message_role(item: &Value) -> Option<&str> {
    if item
        .get("type")
        .is_some_and(|kind| kind.as_str() != Some("message"))
    {
        return None;
    }
    item.get("role").and_then(Value::as_str)
}

/// Context retains readable text; input also preserves positions of nontext blocks
fn parse_blocks(
    item: &Value,
    mut input_kinds: Option<&mut Vec<Option<String>>>,
) -> Vec<ContentBlock> {
    enum Content<'a> {
        Text(&'a str),
        Blocks(&'a [Value]),
    }
    let content = match item.get("content") {
        Some(Value::String(text)) => Content::Text(text),
        Some(Value::Array(blocks)) => Content::Blocks(blocks),
        _ => return vec![],
    };
    let user = item.get("role").and_then(Value::as_str) == Some("user");
    let count = match &content {
        Content::Text(_) => 1,
        Content::Blocks(blocks) => blocks.len(),
    };
    let kinds = item
        .get("internal_chat_message_metadata_passthrough")
        .and_then(|value| value.get("content_item_kinds"))
        .and_then(Value::as_array)
        .filter(|kinds| kinds.len() == count && kinds.iter().all(Value::is_string));
    let kind_at = |position: usize| kinds.and_then(|kinds| kinds[position].as_str());
    let mut blocks = Vec::new();
    let raw = match content {
        Content::Text(text) => {
            let kind = kind_at(0);
            if !(user && kind.is_some_and(excluded_kind)) {
                blocks.push(ContentBlock::Text {
                    text: text.to_owned(),
                });
                if let Some(kinds) = input_kinds.as_mut() {
                    kinds.push(bounded(kind, 256));
                }
            }
            return blocks;
        }
        Content::Blocks(blocks) => blocks,
    };
    for (position, block) in raw.iter().enumerate() {
        let kind = kind_at(position);
        if user && kind.is_some_and(excluded_kind) {
            continue;
        }
        let block_type = block
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("input_text");
        let text = block
            .get("text")
            .or_else(|| block.get("refusal"))
            .and_then(Value::as_str);
        if let Some(text) = text.filter(|_| {
            matches!(
                block_type,
                "input_text" | "output_text" | "text" | "refusal"
            )
        }) {
            blocks.push(ContentBlock::Text {
                text: text.to_owned(),
            });
        } else if user && input_kinds.is_some() {
            let media = block
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            blocks.push(ContentBlock::Nontext {
                media_type: bounded(Some(media), 80).unwrap_or_else(|| "unknown".to_owned()),
                description: bounded(block.get("description").and_then(Value::as_str), 512),
            });
        } else {
            continue;
        }
        if let Some(kinds) = input_kinds.as_mut() {
            kinds.push(bounded(kind, 256));
        }
    }
    blocks
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

fn automatic_prompt_kind(kind: &str) -> bool {
    matches!(kind, "recap.prompt" | "daemon_recovery.internal_context")
}

fn limit_window(mut window: Vec<ContextMessage>) -> Vec<ContextMessage> {
    let characters = |message: &ContextMessage| -> usize {
        message
            .blocks
            .iter()
            .map(|block| match block {
                ContentBlock::Text { text } => text.chars().count(),
                ContentBlock::Nontext { .. } => 0,
            })
            .sum()
    };
    let mut total: usize = window.iter().map(characters).sum();
    while window.len() > 1 && total > WINDOW_CHARACTERS {
        let position = window
            .iter()
            .position(|item| item.role == "user")
            .unwrap_or(0);
        total -= characters(&window.remove(position));
    }
    if total > WINDOW_CHARACTERS {
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

fn request_facts(observation: &RequestObservation, automatic_prompt: bool) -> RequestFacts<'_> {
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
                if current_flat_source {
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
            // Current request source classification supersedes handshake headers
            (canonical.is_none() && !current_flat_source)
                .then(|| {
                    observation
                        .headers
                        .iter()
                        .find(|(key, _)| key.eq_ignore_ascii_case("x-openai-subagent"))
                        .map(|(_, value)| value.clone())
                })
                .flatten()
        });
    let subagent_kind = field(&["subagent_kind", "x-codex-subagent-kind"]);
    let repository_source = canonical.as_ref().or(header_metadata.as_ref());
    let repositories = repository_source
        .and_then(|value| value.get("workspaces"))
        .and_then(parse_repositories);
    RequestFacts {
        metadata: Metadata {
            thread_id: bounded(thread_id, 256).filter(|value| !value.is_empty()),
            session_id: bounded(session_id, 256).filter(|value| !value.is_empty()),
            repositories,
        },
        source: SourceFacts {
            source,
            kind,
            trigger,
            subagent_kind,
            compatible_subagent,
        },
        automatic_prompt,
        user_agents: &observation.user_agents,
    }
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
    use serde_json::json;
    use std::sync::OnceLock;

    fn collect(observation: &RequestObservation) -> Option<AuditDraft> {
        static POLICY: OnceLock<ExclusionPolicy> = OnceLock::new();
        super::collect(observation, POLICY.get_or_init(ExclusionPolicy::default))
    }

    fn observation(input: Value) -> RequestObservation {
        RequestObservation {
            key_id: "key-a".to_owned(),
            request_id: "request-a".to_owned(),
            body: json!({"input":input,"client_metadata":{"thread_id":"thread-a"}}),
            headers: BTreeMap::new(),
            user_agents: Vec::new(),
            observed_at: DateTime::parse_from_rfc3339("2026-10-09T12:00:00.000Z")
                .unwrap()
                .with_timezone(&Utc),
        }
    }

    fn user(text: &str) -> Value {
        json!({"role":"user","content":[{"type":"input_text","text":text}]})
    }

    fn assistant(text: &str) -> Value {
        json!({"role":"assistant","content":[{"type":"output_text","text":text}],"phase":"commentary"})
    }

    #[test]
    fn user_agent_exclusion_only_suppresses_the_audit_draft() {
        let policy = ExclusionPolicy::compile(exclusions::ExclusionSettings {
            user_agent_patterns: vec!["^Monitor/".to_owned()],
        })
        .unwrap()
        .into();
        let mut request = observation(json!([user("task")]));
        request.user_agents = vec!["ordinary/1".to_owned(), "Monitor/1".to_owned()];
        request
            .headers
            .insert("X-Test".to_owned(), "original".to_owned());
        let original = request.clone();
        assert!(super::collect(&request, &policy).is_none());
        assert_eq!(request.body, original.body);
        assert_eq!(request.headers, original.headers);
        assert_eq!(request.user_agents, original.user_agents);
        request.user_agents.pop();
        assert!(super::collect(&request, &policy).is_some());
        assert!(super::collect(&request, &ExclusionPolicy::unavailable()).is_none());
    }

    #[test]
    fn configured_policy_preserves_source_precedence_and_content_filtering() {
        let policy = ExclusionPolicy::compile(exclusions::ExclusionSettings {
            user_agent_patterns: vec!["^Monitor/".to_owned()],
        })
        .unwrap()
        .into();
        let mut request = observation(json!([{
            "role":"user", "content":[
                {"type":"input_text","text":"notification"},
                {"type":"input_text","text":"task"}
            ], "internal_chat_message_metadata_passthrough":{
                "content_item_kinds":["multi_agent.subagent_notification","user.text"]
            }
        }]));
        request.user_agents = vec!["ordinary/1".to_owned()];
        request
            .headers
            .insert("X-OpenAI-Subagent".to_owned(), "review".to_owned());
        request.body["client_metadata"]["x-codex-turn-metadata"] =
            json!(json!({"thread_source":"user"}).to_string());
        let draft = super::collect(&request, &policy).unwrap();
        assert_eq!(
            draft.blocks,
            vec![ContentBlock::Text {
                text: "task".to_owned()
            }]
        );
        request.body["client_metadata"]["thread_source"] = json!("subagent");
        // Canonical current-request metadata remains authoritative over flat and header sources.
        assert!(super::collect(&request, &policy).is_some());
        request.body["client_metadata"]["x-codex-turn-metadata"] =
            json!(json!({"thread_source":"subagent"}).to_string());
        assert!(super::collect(&request, &policy).is_none());
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

    #[test]
    fn only_the_last_raw_input_item_can_be_collected() {
        for latest in [
            json!({"type":"function_call","call_id":"call","name":"tool","arguments":"{}"}),
            json!({"type":"function_call_output","call_id":"call","output":"done"}),
            json!({"type":"function_call_output","role":"user","content":"forged input"}),
            assistant("working"),
            json!({"role":"developer","content":"instructions"}),
            json!({"role":"user","content":[]}),
        ] {
            assert!(collect(&observation(json!([user("historical input"), latest]))).is_none());
        }
        for input in [Value::Null, json!([])] {
            assert!(collect(&observation(input)).is_none());
        }
    }

    #[test]
    fn latest_user_keeps_only_readable_preceding_request_messages() {
        let request = observation(json!([
            user("first"), assistant("plan"),
            {"type":"function_call_output","output":"tool payload"},
            {"role":"developer","content":"instructions"},
            {"type":"reasoning","role":"assistant","content":"private reasoning"},
            {"role":"assistant","phase":"analysis","content":"private analysis"},
            user("latest")
        ]));
        let draft = collect(&request).unwrap();
        assert_eq!(
            draft.blocks,
            vec![ContentBlock::Text {
                text: "latest".to_owned()
            }]
        );
        assert_eq!(
            draft.context.iter().map(text).collect::<Vec<_>>(),
            vec!["first", "plan"]
        );
        assert_eq!(draft.context[1].phase.as_deref(), Some("commentary"));
    }

    #[test]
    fn string_input_is_a_user_input_and_predecessor_ids_supply_no_context() {
        let mut request = observation(json!("latest"));
        request.body["previous_response_id"] = json!("unavailable-history");
        let draft = collect(&request).unwrap();
        assert_eq!(
            draft.blocks,
            vec![ContentBlock::Text {
                text: "latest".to_owned()
            }]
        );
        assert_eq!(draft.input_kinds, vec![None]);
        assert!(draft.context.is_empty());
    }

    #[test]
    fn repeated_request_and_message_ids_still_produce_drafts() {
        let mut message = user("same input");
        message["id"] = json!("stable-message");
        let mut request = observation(json!([message]));
        let first = collect(&request).unwrap();
        assert_eq!(collect(&request).unwrap().blocks, first.blocks);
        request.request_id = "request-repeated".to_owned();
        let repeated = collect(&request).unwrap();
        assert_eq!(repeated.blocks, first.blocks);
        assert_eq!(repeated.request_id, "request-repeated");
    }

    #[test]
    fn filtering_preserves_user_blocks_and_their_aligned_classifications() {
        let request = observation(json!([{
            "role":"user", "content":[
                {"type":"input_text","text":"injected"},
                {"type":"input_text","text":"AGENTS.md quoted by user"},
                {"type":"input_text","text":"automatic"},
                {"type":"input_text","text":"answer"}
            ], "internal_chat_message_metadata_passthrough":{
                "content_item_kinds":["agents_md.instructions","new.unknown.kind","user.heartbeat","user.answered_question"]
            }
        }]));
        let draft = collect(&request).unwrap();
        assert_eq!(draft.blocks.len(), 2);
        assert_eq!(
            draft.input_kinds,
            vec![
                Some("new.unknown.kind".to_owned()),
                Some("user.answered_question".to_owned())
            ]
        );
        let mut automatic = user("automatic");
        automatic["internal_chat_message_metadata_passthrough"] =
            json!({"content_item_kinds":["user.heartbeat"]});
        assert!(collect(&observation(json!([user("earlier user"), automatic]))).is_none());
    }

    #[test]
    fn missing_mismatched_or_invalid_classifications_preserve_content() {
        for kinds in [
            Value::Null,
            json!(["agents_md.instructions"]),
            json!(["agents_md.instructions", null]),
        ] {
            let request = observation(json!([{
                "role":"user", "content":[
                    {"type":"input_text","text":"a"}, {"type":"input_text","text":"b"}
                ], "internal_chat_message_metadata_passthrough":{"content_item_kinds":kinds}
            }]));
            let draft = collect(&request).unwrap();
            assert_eq!(draft.blocks.len(), 2);
            assert_eq!(draft.input_kinds, vec![None, None]);
        }
    }

    #[test]
    fn known_injected_classifications_are_skipped_but_unknown_kinds_are_preserved() {
        for kind in [
            "agents_md.instructions",
            "environments.environment_context",
            "multi_agent.subagent_notification",
            "user.heartbeat",
        ] {
            let mut message = user("injected");
            message["internal_chat_message_metadata_passthrough"] =
                json!({"content_item_kinds":[kind]});
            assert!(collect(&observation(json!([user("actual input"), message]))).is_none());
        }
        for kind in ["user.automatic_future", "extension.internal_context"] {
            let mut message = user("future kind");
            message["internal_chat_message_metadata_passthrough"] =
                json!({"content_item_kinds":[kind]});
            assert!(collect(&observation(json!([message]))).is_some());
        }
    }

    #[test]
    fn subagent_and_internal_sources_exclude_requests() {
        for metadata in [
            json!({"subagent_kind":"thread_spawn"}),
            json!({"thread_source":"subagent"}),
            json!({"thread_source":{"subagent":"review"}}),
            json!({"request_kind":"memory"}),
            json!({"request_kind":"compaction"}),
            json!({"thread_source":"guardian_review"}),
        ] {
            let mut request = observation(json!([user("task")]));
            request.body["client_metadata"]["x-codex-turn-metadata"] = json!(metadata.to_string());
            assert!(collect(&request).is_none());
        }
        let mut request = observation(json!([user("task")]));
        request
            .headers
            .insert("X-OpenAI-Subagent".to_owned(), "custom-task".to_owned());
        assert!(collect(&request).is_none());
    }

    fn catch_up() -> Value {
        let mut message = user(
            "Write a brief catch-up for a user returning to this task. Return JSON with summary and nullable next_action.\n\nConversation:\nUser: Please continue.\nAssistant: The changes are implemented.",
        );
        message["internal_chat_message_metadata_passthrough"] =
            json!({"content_item_kinds":["user.text"]});
        message
    }

    #[test]
    fn system_catch_up_requests_are_excluded_across_metadata_forms() {
        let canonical = json!({"thread_source":"system","request_kind":"turn"}).to_string();
        for (client_metadata, headers) in [
            (json!({"x-codex-turn-metadata":canonical}), BTreeMap::new()),
            (
                json!({"thread_source":"system","request_kind":"turn"}),
                BTreeMap::new(),
            ),
            (
                json!({}),
                BTreeMap::from([("X-Codex-Turn-Metadata".to_owned(), canonical)]),
            ),
            (
                json!({}),
                BTreeMap::from([("Thread_Source".to_owned(), "system".to_owned())]),
            ),
            (
                json!({}),
                BTreeMap::from([("X-Codex-Thread-Source".to_owned(), "system".to_owned())]),
            ),
        ] {
            let mut request = observation(json!([catch_up()]));
            request.body["client_metadata"] = client_metadata;
            request.headers = headers;
            assert!(collect(&request).is_none());
        }
    }

    #[test]
    fn catch_up_text_and_unrecognized_purposes_do_not_exclude_user_inputs() {
        for source in [None, Some("user"), Some("future_feature")] {
            let message = catch_up();
            let expected = vec![ContentBlock::Text {
                text: message["content"][0]["text"].as_str().unwrap().to_owned(),
            }];
            let mut request = observation(json!([message]));
            request.body["client_metadata"]["x-codex-turn-metadata"] = json!(
                json!({"thread_source":source,"request_kind":"system","turn_trigger":"system"})
                    .to_string()
            );
            let draft = collect(&request).unwrap();
            assert_eq!(draft.input_kinds, vec![Some("user.text".to_owned())]);
            assert_eq!(draft.blocks, expected);
        }
    }

    #[test]
    fn current_user_source_supersedes_system_handshake_headers() {
        for client_metadata in [
            json!({"x-codex-turn-metadata":json!({"thread_source":"user"}).to_string()}),
            json!({"thread_source":"user"}),
        ] {
            let mut request = observation(json!([catch_up()]));
            request.body["client_metadata"] = client_metadata;
            request.headers = BTreeMap::from([
                (
                    "X-Codex-Turn-Metadata".to_owned(),
                    json!({"thread_source":"system"}).to_string(),
                ),
                ("X-Codex-Thread-Source".to_owned(), "system".to_owned()),
            ]);
            assert!(collect(&request).is_some());
        }
    }

    #[test]
    fn automatic_prompt_inputs_are_skipped_without_stripping_carried_content() {
        for kind in ["recap.prompt", "daemon_recovery.internal_context"] {
            let mut generated = user("generated context");
            generated["internal_chat_message_metadata_passthrough"]["content_item_kinds"] =
                json!([kind]);
            assert!(collect(&observation(json!([user("earlier input"), generated]))).is_none());
            let request = observation(json!([
                user("earlier input"),
                generated,
                assistant("visible progress"),
                {
                    "role":"user", "content":[
                        {"type":"input_text","text":"generated context"},
                        {"type":"input_text","text":"Please continue."}
                    ], "internal_chat_message_metadata_passthrough":{
                        "content_item_kinds":[kind,"user.text"]
                    }
                }
            ]));
            let draft = collect(&request).unwrap();
            assert_eq!(
                draft.blocks,
                vec![
                    ContentBlock::Text {
                        text: "generated context".to_owned()
                    },
                    ContentBlock::Text {
                        text: "Please continue.".to_owned()
                    }
                ]
            );
            assert_eq!(
                draft.input_kinds,
                vec![Some(kind.to_owned()), Some("user.text".to_owned())]
            );
            assert_eq!(
                draft.context.iter().map(text).collect::<Vec<_>>(),
                vec!["earlier input", "generated context", "visible progress"]
            );

            let mut future_input = request.clone();
            future_input.body["input"][3]["internal_chat_message_metadata_passthrough"]["content_item_kinds"]
                [1] = json!("future.user_input");
            let future_draft = collect(&future_input).unwrap();
            assert_eq!(future_draft.blocks, draft.blocks);
            assert_eq!(
                future_draft.input_kinds,
                vec![Some(kind.to_owned()), Some("future.user_input".to_owned())]
            );

            for kinds in [
                Value::Null,
                json!([]),
                json!([kind, "user.text"]),
                json!([null]),
            ] {
                let mut uncertain = generated.clone();
                uncertain["internal_chat_message_metadata_passthrough"]["content_item_kinds"] =
                    kinds;
                assert!(collect(&observation(json!([uncertain]))).is_some());
            }
        }
    }

    #[test]
    fn carried_context_does_not_suppress_user_initiated_audits() {
        for kind in [
            "skills.selected_skill_instructions",
            "goal.internal_context",
            "generic.turn_aborted",
            "compaction.summary",
            "memories.extraction_evidence",
            "guardian.permissions",
            "guardian.node_repl_review_evidence",
            "guardian.context_omission",
            "guardian.retained_instructions",
            "guardian.sender_messages",
            "guardian.tool_descriptions",
            "apply_patch.legacy_exec_command_warning",
            "model_switch.legacy_mismatch_warning",
            "unified_exec.legacy_process_limit_warning",
            "additional_content.selected_file",
            "additional_content.future_client_context",
        ] {
            let mut carried = user("carried context");
            carried["internal_chat_message_metadata_passthrough"]["content_item_kinds"] =
                json!([kind]);
            let draft = collect(&observation(json!([user("actual input"), carried]))).unwrap();
            assert_eq!(draft.input_kinds, vec![Some(kind.to_owned())]);
            assert_eq!(
                draft.blocks,
                vec![ContentBlock::Text {
                    text: "carried context".to_owned()
                }]
            );
            assert_eq!(
                draft.context.iter().map(text).collect::<Vec<_>>(),
                vec!["actual input"]
            );
        }
    }

    #[test]
    fn explicit_user_actions_media_placeholders_and_quoted_context_are_preserved() {
        let quoted =
            "<codex_internal_context source=\"goal\">quoted by user</codex_internal_context>";
        for kind in [
            "user.text",
            "user.answered_question",
            "user.goal",
            "user.goal.omitted",
            "shell.user_command",
            "images.preparation_error",
            "images.unsupported",
            "audio.unsupported",
            "unknown",
        ] {
            let request = observation(json!([{
                "role":"user", "content":[{"type":"input_text","text":quoted}],
                "internal_chat_message_metadata_passthrough":{"content_item_kinds":[kind]}
            }]));
            let draft = collect(&request).unwrap();
            assert_eq!(
                draft.blocks,
                vec![ContentBlock::Text {
                    text: quoted.to_owned()
                }]
            );
            assert_eq!(draft.input_kinds, vec![Some(kind.to_owned())]);
        }
        let mut request = observation(json!([user(quoted)]));
        let unknown = collect(&request).unwrap();
        assert_eq!(
            unknown.blocks,
            vec![ContentBlock::Text {
                text: quoted.to_owned()
            }]
        );
        assert_eq!(unknown.input_kinds, vec![None]);

        request.body["input"] = json!([{
            "role":"user", "content":[
                {"type":"input_image","image_url":"data:image/png;base64,not-copied"},
                {"type":"input_audio","audio_url":"data:audio/wav;base64,not-copied"}
            ], "internal_chat_message_metadata_passthrough":{"content_item_kinds":["user.image","user.audio"]}
        }]);
        let media = collect(&request).unwrap();
        assert_eq!(media.blocks.len(), 2);
        assert_eq!(
            media.input_kinds,
            vec![Some("user.image".to_owned()), Some("user.audio".to_owned())]
        );
        assert!(
            media
                .blocks
                .iter()
                .all(|block| matches!(block, ContentBlock::Nontext { .. }))
        );
    }

    #[test]
    fn current_source_classification_supersedes_handshake_metadata() {
        let mut request = observation(json!([user("task")]));
        request
            .headers
            .insert("X-OpenAI-Subagent".to_owned(), "review".to_owned());
        request.body["client_metadata"]["x-codex-turn-metadata"] =
            json!(json!({"thread_source":"user"}).to_string());
        assert!(collect(&request).is_some());
        request.body["client_metadata"]["x-openai-subagent"] = json!("review");
        assert!(collect(&request).is_none());

        let mut request = observation(json!([user("task")]));
        request.headers.insert(
            "X-Codex-Turn-Metadata".to_owned(),
            json!({"request_kind":"compaction"}).to_string(),
        );
        assert!(collect(&request).is_none());
        request.body["client_metadata"]["thread_source"] = json!("user");
        assert!(collect(&request).is_some());
    }

    #[test]
    fn hidden_title_generation_is_excluded_without_matching_user_text() {
        let prompt =
            "Generate a concise, single-line task title.\n\nUser prompt:\nFix the audit filter.";
        for source in ["thread_title", "user"] {
            let mut request = observation(json!([user(prompt)]));
            request.body["client_metadata"]["x-codex-turn-metadata"] =
                json!(json!({"thread_source":source,"request_kind":"turn"}).to_string());
            assert_eq!(collect(&request).is_some(), source == "user");
        }
    }

    #[test]
    fn latest_message_turn_is_preserved_without_a_request_turn_fallback() {
        let mut latest = user("latest");
        latest["internal_chat_message_metadata_passthrough"] = json!({"turn_id":"message-turn"});
        let mut request = observation(json!([user("earlier"), latest]));
        request.body["client_metadata"]["x-codex-turn-metadata"] =
            json!(json!({"session_id":"session-only", "turn_id":"request-turn"}).to_string());
        let draft = collect(&request).unwrap();
        assert_eq!(draft.turn_id.as_deref(), Some("message-turn"));
        assert_eq!(draft.thread_id.as_deref(), Some("thread-a"));
        assert_eq!(draft.session_id.as_deref(), Some("session-only"));
        request.body["client_metadata"] =
            json!({"session_id":"session-only", "turn_id":"request-turn"});
        request.body["input"][1]
            .as_object_mut()
            .unwrap()
            .remove("internal_chat_message_metadata_passthrough");
        let draft = collect(&request).unwrap();
        assert!(draft.turn_id.is_none());
        assert!(draft.thread_id.is_none());
        assert_eq!(draft.session_id.as_deref(), Some("session-only"));
    }

    #[test]
    fn multimedia_positions_are_saved_without_binary_content_or_media_urls() {
        let request = observation(json!([{
            "role":"user", "content":[
                {"type":"input_text","text":"see diagram"},
                {"type":"input_image","image_url":"https://example.com/a.png","description":"diagram"},
                {"type":"input_audio","data":"binary-content"}
            ]
        }]));
        let draft = collect(&request).unwrap();
        assert_eq!(
            draft.blocks,
            vec![
                ContentBlock::Text {
                    text: "see diagram".to_owned()
                },
                ContentBlock::Nontext {
                    media_type: "input_image".to_owned(),
                    description: Some("diagram".to_owned())
                },
                ContentBlock::Nontext {
                    media_type: "input_audio".to_owned(),
                    description: None
                },
            ]
        );
        assert_eq!(draft.input_kinds, vec![None, None, None]);
        let encoded = serde_json::to_string(&draft).unwrap();
        assert!(!encoded.contains("a.png"));
        assert!(!encoded.contains("binary-content"));
    }

    #[test]
    fn repository_snapshots_preserve_absence_and_empty_and_strip_credentials() {
        let mut request = observation(json!([user("task")]));
        assert!(collect(&request).unwrap().repositories.is_none());
        request.body["client_metadata"]["x-codex-turn-metadata"] =
            json!(json!({"workspaces":{}}).to_string());
        assert_eq!(collect(&request).unwrap().repositories, Some(vec![]));
        request.body["client_metadata"]["x-codex-turn-metadata"] = json!(json!({"workspaces":{
            "/repo":{"associated_remote_urls":{"origin":"https://user:password@example.com/org/repo.git","ssh":"git@github.com:org/repo.git"},"commit_hash":"excluded","has_changes":true},
            "/empty":{}
        }}).to_string());
        let draft = collect(&request).unwrap();
        let repositories = draft.repositories.as_ref().unwrap();
        assert_eq!(repositories[0].remotes, None);
        assert_eq!(
            repositories[1].remotes.as_ref().unwrap()[0].url,
            "https://example.com/org/repo.git"
        );
        let encoded = serde_json::to_string(&draft).unwrap();
        assert!(!encoded.contains("password"));
        assert!(!encoded.contains("commit_hash"));
    }

    #[test]
    fn optional_identifier_and_metadata_bounds_do_not_reject_the_input() {
        let mut request = observation(json!([user("body\0preserved")]));
        request.body["client_metadata"] =
            json!({"thread_id":"bad\0thread","session_id":"bad\0session"});
        request.body["input"][0]["internal_chat_message_metadata_passthrough"] =
            json!({"turn_id":"bad\0turn"});
        let draft = collect(&request).unwrap();
        assert!(draft.thread_id.is_none());
        assert!(draft.session_id.is_none());
        assert!(draft.turn_id.is_none());
        assert_eq!(
            draft.blocks,
            vec![ContentBlock::Text {
                text: "body\0preserved".to_owned()
            }]
        );

        let workspaces = json!({"/repo":{"associated_remote_urls":{"origin":"x".repeat(8193)}}});
        request.body["client_metadata"]["x-codex-turn-metadata"] =
            json!(json!({"workspaces":workspaces}).to_string());
        assert!(collect(&request).unwrap().repositories.is_none());
    }

    #[test]
    fn context_is_bounded_to_recent_messages_and_unicode_characters() {
        let mut input: Vec<Value> = (0..30)
            .map(|index| user(&format!("input {index}")))
            .collect();
        input.push(user("current"));
        let draft = collect(&observation(json!(input))).unwrap();
        assert_eq!(draft.context.len(), WINDOW_MESSAGES);
        assert_eq!(text(&draft.context[0]), "input 18");
        assert_eq!(text(draft.context.last().unwrap()), "input 29");

        let long = "🙂".repeat(WINDOW_CHARACTERS + 10);
        let draft = collect(&observation(json!([
            user("old input"),
            assistant(&long),
            user("current")
        ])))
        .unwrap();
        assert_eq!(draft.context.len(), 1);
        assert!(draft.context[0].truncated);
        assert_eq!(text(&draft.context[0]), "🙂".repeat(WINDOW_CHARACTERS));
    }

    #[test]
    fn remote_credentials_are_removed_without_breaking_git_and_local_locators() {
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
}
