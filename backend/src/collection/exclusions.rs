//! 对已解析的请求事实应用内建来源规则和用户配置，保持请求观测只读
use regex::{RegexBuilder, RegexSet, RegexSetBuilder};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

use super::Metadata;

const MAX_PATTERNS: usize = 32;
const MAX_PATTERN_BYTES: usize = 1024;
const MAX_COMPILED_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_USER_AGENT_BYTES: usize = 8192;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExclusionSettings {
    pub user_agent_patterns: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExclusionConfigError {
    pub pattern_index: Option<usize>,
    pub message: String,
}

impl fmt::Display for ExclusionConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ExclusionConfigError {}

#[derive(Debug)]
pub struct CompiledExclusions {
    settings: ExclusionSettings,
    user_agents: RegexSet,
}

impl CompiledExclusions {
    pub fn settings(&self) -> &ExclusionSettings {
        &self.settings
    }
}

#[derive(Debug)]
pub struct ExclusionPolicy {
    compiled: Option<CompiledExclusions>,
}

impl Default for ExclusionPolicy {
    fn default() -> Self {
        Self {
            compiled: Some(CompiledExclusions {
                settings: ExclusionSettings::default(),
                user_agents: RegexSet::empty(),
            }),
        }
    }
}

impl From<CompiledExclusions> for ExclusionPolicy {
    fn from(compiled: CompiledExclusions) -> Self {
        Self {
            compiled: Some(compiled),
        }
    }
}

impl ExclusionPolicy {
    pub fn compile(
        settings: ExclusionSettings,
    ) -> Result<CompiledExclusions, ExclusionConfigError> {
        if settings.user_agent_patterns.len() > MAX_PATTERNS {
            return Err(ExclusionConfigError {
                pattern_index: None,
                message: format!("At most {MAX_PATTERNS} User-Agent patterns are allowed"),
            });
        }
        for (index, pattern) in settings.user_agent_patterns.iter().enumerate() {
            let error = if pattern.trim().is_empty() {
                Some("A User-Agent pattern must not be blank".to_owned())
            } else if pattern.len() > MAX_PATTERN_BYTES {
                Some(format!(
                    "A User-Agent pattern must not exceed {MAX_PATTERN_BYTES} UTF-8 bytes"
                ))
            } else {
                // Validate each row separately so a save can identify the offending pattern.
                RegexBuilder::new(pattern)
                    .size_limit(MAX_COMPILED_BYTES)
                    .build()
                    .err()
                    .map(|error| error.to_string())
            };
            if let Some(message) = error {
                return Err(ExclusionConfigError {
                    pattern_index: Some(index),
                    message,
                });
            }
        }
        let user_agents = RegexSetBuilder::new(&settings.user_agent_patterns)
            .size_limit(MAX_COMPILED_BYTES)
            .build()
            .map_err(|error| ExclusionConfigError {
                pattern_index: None,
                message: error.to_string(),
            })?;
        Ok(CompiledExclusions {
            settings,
            user_agents,
        })
    }

    /// Corrupt saved settings suspend capture until a valid policy is saved.
    pub fn unavailable() -> Self {
        Self { compiled: None }
    }

    pub fn settings(&self) -> Option<&ExclusionSettings> {
        self.compiled.as_ref().map(|compiled| &compiled.settings)
    }

    pub(super) fn assess(
        &self,
        facts: RequestFacts<'_>,
    ) -> Result<EligibleRequest, ExclusionReason> {
        let compiled = self
            .compiled
            .as_ref()
            .ok_or(ExclusionReason::UnavailableSettings)?;
        if facts.automatic_prompt {
            return Err(ExclusionReason::AutomaticPrompt);
        }
        let source = facts.source;
        if source.subagent_kind.is_some_and(|value| !value.is_empty())
            || source
                .compatible_subagent
                .is_some_and(|value| !value.is_empty())
        {
            return Err(ExclusionReason::Subagent);
        }
        if source.source.as_deref().is_some_and(|value| {
            matches!(value, "system" | "thread_title") || internal_source(value)
        }) || source.kind.as_deref().is_some_and(internal_source)
            || source.trigger.as_deref().is_some_and(internal_source)
        {
            return Err(ExclusionReason::InternalSource);
        }
        if facts.user_agents.iter().any(|value| {
            !value.is_empty()
                && value.len() <= MAX_USER_AGENT_BYTES
                && compiled.user_agents.is_match(value)
        }) {
            return Err(ExclusionReason::UserAgent);
        }
        Ok(EligibleRequest {
            metadata: facts.metadata,
        })
    }
}

pub(super) struct RequestFacts<'a> {
    pub metadata: Metadata,
    pub source: SourceFacts,
    pub automatic_prompt: bool,
    pub user_agents: &'a [String],
}

#[derive(Default)]
pub(super) struct SourceFacts {
    pub source: Option<String>,
    pub kind: Option<String>,
    pub trigger: Option<String>,
    pub subagent_kind: Option<String>,
    pub compatible_subagent: Option<String>,
}

pub(super) struct EligibleRequest {
    pub metadata: Metadata,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum ExclusionReason {
    UnavailableSettings,
    AutomaticPrompt,
    Subagent,
    InternalSource,
    UserAgent,
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn policy(patterns: &[&str]) -> ExclusionPolicy {
        ExclusionPolicy::compile(ExclusionSettings {
            user_agent_patterns: patterns
                .iter()
                .map(|pattern| (*pattern).to_owned())
                .collect(),
        })
        .unwrap()
        .into()
    }

    fn facts(user_agents: &[String]) -> RequestFacts<'_> {
        RequestFacts {
            metadata: Metadata::default(),
            source: SourceFacts::default(),
            automatic_prompt: false,
            user_agents,
        }
    }

    #[test]
    fn user_agent_patterns_follow_rust_regex_search_semantics() {
        for (pattern, user_agent, excluded) in [
            ("Monitor", "ExampleMonitor/1", true),
            ("Monitor", "examplemonitor/1", false),
            ("^Monitor/", "ExampleMonitor/1", false),
            ("^Monitor/", "Monitor/1", true),
            ("Monitor$", "Monitor/1", false),
            ("Monitor$", "ExampleMonitor", true),
            ("(?i)^monitor/", "MONITOR/1", true),
            ("^\\p{L}+$", "客户端", true),
            (" Monitor ", "Monitor", false),
            (" Monitor ", " Monitor ", true),
        ] {
            let user_agents = [user_agent.to_owned()];
            assert_eq!(
                policy(&[pattern]).assess(facts(&user_agents)).err(),
                excluded.then_some(ExclusionReason::UserAgent),
                "{pattern:?} against {user_agent:?}"
            );
        }
    }

    #[test]
    fn any_pattern_can_match_any_duplicate_user_agent_value() {
        let policy = policy(&["^Monitor/", "(?i)^internal-bot/"]);
        let user_agents = ["ordinary/1".to_owned(), "INTERNAL-BOT/1".to_owned()];
        assert_eq!(
            policy.assess(facts(&user_agents)).err(),
            Some(ExclusionReason::UserAgent)
        );
        assert!(policy.assess(facts(&user_agents[..1])).is_ok());
    }

    #[test]
    fn missing_empty_and_oversized_user_agents_follow_normal_source_rules() {
        let policy = policy(&[".*"]);
        let user_agents = [String::new(), "a".repeat(MAX_USER_AGENT_BYTES + 1)];
        assert!(policy.assess(facts(&[])).is_ok());
        assert!(policy.assess(facts(&user_agents)).is_ok());
        let user_agents = ["é".repeat(MAX_USER_AGENT_BYTES / 2 + 1)];
        assert!(policy.assess(facts(&user_agents)).is_ok());
        let user_agents = ["a".repeat(MAX_USER_AGENT_BYTES)];
        assert_eq!(
            policy.assess(facts(&user_agents)).err(),
            Some(ExclusionReason::UserAgent)
        );
    }

    #[test]
    fn default_and_cleared_settings_retain_no_custom_patterns() {
        let compiled = ExclusionPolicy::compile(ExclusionSettings::default()).unwrap();
        assert_eq!(compiled.settings(), &ExclusionSettings::default());
        let policy = ExclusionPolicy::from(compiled);
        let user_agents = ["Monitor/1".to_owned()];
        assert!(policy.assess(facts(&user_agents)).is_ok());
        assert!(
            ExclusionPolicy::default()
                .assess(facts(&user_agents))
                .is_ok()
        );
        assert_eq!(policy.settings(), Some(&ExclusionSettings::default()));
    }

    #[test]
    fn invalid_regex_patterns_report_the_original_row() {
        for invalid in ["(", "(?=Monitor)", "(a)\\1", "", " \t\n"] {
            let error = ExclusionPolicy::compile(ExclusionSettings {
                user_agent_patterns: vec!["ordinary".to_owned(), invalid.to_owned()],
            })
            .unwrap_err();
            assert_eq!(error.pattern_index, Some(1), "{invalid:?}");
            assert!(!error.message.is_empty());
        }
    }

    #[test]
    fn pattern_limits_apply_to_count_utf8_bytes_and_compiled_size() {
        let too_many = ExclusionPolicy::compile(ExclusionSettings {
            user_agent_patterns: vec!["pattern".to_owned(); MAX_PATTERNS + 1],
        })
        .unwrap_err();
        assert_eq!(too_many.pattern_index, None);
        assert!(policy(&vec!["pattern"; MAX_PATTERNS]).settings().is_some());

        let too_long = ExclusionPolicy::compile(ExclusionSettings {
            user_agent_patterns: vec!["é".repeat(MAX_PATTERN_BYTES / 2 + 1)],
        })
        .unwrap_err();
        assert_eq!(too_long.pattern_index, Some(0));
        assert!(
            policy(&[&"é".repeat(MAX_PATTERN_BYTES / 2)])
                .settings()
                .is_some()
        );

        let too_large = ExclusionPolicy::compile(ExclusionSettings {
            user_agent_patterns: vec![r"\w{1000}".to_owned()],
        })
        .unwrap_err();
        assert_eq!(too_large.pattern_index, Some(0));
    }

    #[test]
    fn settings_deserialization_preserves_patterns_and_rejects_unknown_fields() {
        let settings: ExclusionSettings =
            serde_json::from_value(json!({"userAgentPatterns":[" Monitor ","(?i)^bot/"]})).unwrap();
        assert_eq!(settings.user_agent_patterns, [" Monitor ", "(?i)^bot/"]);
        assert!(
            serde_json::from_value::<ExclusionSettings>(json!({
                "userAgentPatterns":[], "unexpected":true,
            }))
            .is_err()
        );
    }

    #[test]
    fn compiled_size_limit_applies_to_the_combined_matcher() {
        let pattern = "[a-z]{10000}";
        assert!(policy(&[pattern]).settings().is_some());
        let error = ExclusionPolicy::compile(ExclusionSettings {
            user_agent_patterns: vec![pattern.to_owned(); MAX_PATTERNS],
        })
        .unwrap_err();
        assert_eq!(error.pattern_index, None);
    }

    #[test]
    fn shared_policy_keeps_built_in_and_configured_exclusions() {
        let policy = policy(&["^Monitor/"]);
        let ordinary_agent = ["ordinary/1".to_owned()];
        let mut subagent = facts(&ordinary_agent);
        subagent.source.subagent_kind = Some("review".to_owned());
        assert_eq!(
            policy.assess(subagent).err(),
            Some(ExclusionReason::Subagent)
        );

        for source in [
            "system",
            "thread_title",
            "memory",
            "{\"SubAgent\":\"review\"}",
        ] {
            let mut internal = facts(&ordinary_agent);
            internal.source.source = Some(source.to_owned());
            assert_eq!(
                policy.assess(internal).err(),
                Some(ExclusionReason::InternalSource)
            );
        }
        let mut automatic = facts(&ordinary_agent);
        automatic.automatic_prompt = true;
        assert_eq!(
            policy.assess(automatic).err(),
            Some(ExclusionReason::AutomaticPrompt)
        );
        assert!(policy.assess(facts(&ordinary_agent)).is_ok());
        let matching_agent = ["Monitor/1".to_owned()];
        assert_eq!(
            policy.assess(facts(&matching_agent)).err(),
            Some(ExclusionReason::UserAgent)
        );
    }

    #[test]
    fn unavailable_settings_suspend_audit_eligibility() {
        let policy = ExclusionPolicy::unavailable();
        assert!(policy.settings().is_none());
        assert_eq!(
            policy.assess(facts(&[])).err(),
            Some(ExclusionReason::UnavailableSettings)
        );
    }
}
