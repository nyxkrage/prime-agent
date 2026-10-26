//! The daemon-level model allowlist (settings `allowedModels`): the model
//! patterns a daemon may resolve to — a daemon policy, not a session
//! preference; an outside model fails loudly with [`ModelAllowlistRefusal`].
//! Rust-only guardrail (no TS equivalent; unset means unrestricted).

/// A model the daemon refused to resolve: the resolved selector is outside
/// the configured allowlist, typed so enforcement seams can downcast the
/// refusal (adoption telemetry) out of the TS-parity resolution errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelAllowlistRefusal {
    /// The refused model, full selector form `provider/model-id`.
    pub selector: String,
    pub alias_patterns: Vec<AliasPattern>,
}

impl std::fmt::Display for ModelAllowlistRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Model \"{}\" is blocked by the daemon model allowlist (settings \"allowedModels\"); \
             the daemon never falls back to a different model. Allow it in the settings \
             or pick an allowed model.",
            self.selector
        )?;
        for alias_pattern in &self.alias_patterns {
            write!(f, " Note: \"allowedModels\" {alias_pattern}.")?;
        }
        Ok(())
    }
}

impl std::error::Error for ModelAllowlistRefusal {}

/// The settings `subagentModels` filter on RLM child models. Read from
/// global settings only; never falls back to `enabledModels`. Like
/// `allowedModels` it fails closed: an unreadable policy is not an
/// unrestricted one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubagentModelFilter {
    Unrestricted,
    /// Patterns matched against canonical `provider/model-id` selectors.
    Only(Vec<String>),
    /// The global settings or the `subagentModels` value could not be read:
    /// no model is permitted.
    Unreadable(String),
}

impl SubagentModelFilter {
    /// The lenient settings load drops a wrong-typed field to unset, so the
    /// raw document is checked too: a malformed value or a list of only
    /// blank patterns fails closed; `null` and `[]` stay unset.
    #[must_use]
    pub fn load(settings: &crate::settings::SettingsManager) -> Self {
        if let Some(error) = settings
            .errors()
            .iter()
            .find(|error| error.scope == crate::settings::SettingsScope::Global)
        {
            return Self::Unreadable(error.message.clone());
        }
        if let Some(patterns) = settings.get_subagent_models() {
            return Self::Only(patterns);
        }
        let Some(raw) = settings.global_raw() else {
            return Self::Unrestricted;
        };
        if !raw.is_object() {
            return Self::Unreadable(
                "the global settings document is not a JSON object".to_string(),
            );
        }
        match raw.get("subagentModels") {
            None | Some(serde_json::Value::Null) => Self::Unrestricted,
            Some(serde_json::Value::Array(items)) if items.is_empty() => Self::Unrestricted,
            Some(serde_json::Value::Array(items))
                if items.iter().all(serde_json::Value::is_string) =>
            {
                Self::Unreadable("subagentModels contains only empty patterns".to_string())
            }
            Some(_) => Self::Unreadable(
                "subagentModels is present but is not an array of strings".to_string(),
            ),
        }
    }

    #[must_use]
    pub fn permits(&self, selector: &str) -> bool {
        match self {
            Self::Unrestricted => true,
            Self::Only(patterns) => model_allowed(selector, patterns),
            Self::Unreadable(_) => false,
        }
    }
}

/// A model pattern (`allowedModels`, `subagentModels`) that equals a
/// models.json alias and matches no model: patterns never expand aliases,
/// so it is reported instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AliasPattern {
    pub pattern: String,
    /// The `provider/model-id` selector the alias names.
    pub selector: String,
}

impl std::fmt::Display for AliasPattern {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self { pattern, selector } = self;
        write!(
            f,
            "pattern \"{pattern}\" is the models.json alias of {selector}, but model patterns \
             match real model names, not aliases, so it matches nothing; use \"{selector}\""
        )
    }
}

/// An RLM child model refused by the `subagentModels` filter, even when
/// authenticated; typed so the spawn seams can count it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubagentModelRefusal {
    OutsideFilter {
        selector: String,
        /// Spawnable names (`alias (selector)` or the selector) so the
        /// caller can retry in one step.
        permitted: Vec<String>,
        alias_patterns: Vec<AliasPattern>,
    },
    UnreadableFilter {
        selector: String,
        error: String,
    },
}

impl std::fmt::Display for SubagentModelRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OutsideFilter {
                selector,
                permitted,
                alias_patterns,
            } => {
                write!(
                    f,
                    "Model \"{selector}\" is not permitted for subagents (settings \"subagentModels\"); \
                     there is no fallback to another model. "
                )?;
                if permitted.is_empty() {
                    write!(
                        f,
                        "No authenticated model matches \"subagentModels\": authenticate one or fix the setting."
                    )?;
                } else {
                    write!(f, "Pass model= one of: {}.", permitted.join(", "))?;
                }
                for alias_pattern in alias_patterns {
                    write!(f, " Note: \"subagentModels\" {alias_pattern}.")?;
                }
                Ok(())
            }
            Self::UnreadableFilter { selector, error } => write!(
                f,
                "The subagent model filter (settings \"subagentModels\") could not be read ({error}); \
                 refusing model \"{selector}\": the filter fails closed instead of allowing every \
                 model. Fix the global settings.json and retry."
            ),
        }
    }
}

impl std::error::Error for SubagentModelRefusal {}

/// Whether a selector is allowed by the `allowedModels` patterns (the
/// `--models` grammar, case-insensitive, matched against the full selector
/// `provider/model-id` and the bare id, whose `prime-inference` ids may
/// carry slashes): wildcards glob, plain patterns match exactly. An empty
/// pattern list allows nothing.
#[must_use]
pub fn model_allowed(selector: &str, allowlist: &[String]) -> bool {
    allowlist
        .iter()
        .any(|pattern| pattern_matches_selector(pattern, selector))
}

/// One pattern against one selector: case-insensitive exact or glob match
/// on the full selector or the bare id.
fn pattern_matches_selector(pattern: &str, selector: &str) -> bool {
    let pattern = pattern.trim().to_lowercase();
    if pattern.is_empty() {
        return false;
    }
    let selector = selector.to_lowercase();
    let bare_id = selector
        .split_once('/')
        .map_or(selector.as_str(), |(_, id)| id);
    if !pattern.contains(['*', '?', '[']) {
        return pattern == selector || pattern == bare_id;
    }
    let matcher = match globset::Glob::new(&pattern) {
        Ok(glob) => glob.compile_matcher(),
        Err(_) => return false,
    };
    matcher.is_match(&selector) || matcher.is_match(bare_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SELECTOR: &str = "prime-inference/internal/glm-5.3-fast";

    #[test]
    fn exact_and_bare_id_patterns_match_case_insensitively() {
        assert!(model_allowed(
            SELECTOR,
            &["prime-inference/internal/glm-5.3-fast".to_string()]
        ));
        assert!(model_allowed(
            SELECTOR,
            &["Prime-Inference/Internal/GLM-5.3-Fast".to_string()]
        ));
        // The bare id (prime-inference ids carry slashes) matches without
        // the provider.
        assert!(model_allowed(
            SELECTOR,
            &["internal/glm-5.3-fast".to_string()]
        ));
        assert!(!model_allowed(SELECTOR, &["glm-5.3-fast".to_string()]));
        assert!(!model_allowed(
            SELECTOR,
            &["prime-inference/internal/glm-5.3-turbo".to_string()]
        ));
    }

    #[test]
    fn glob_patterns_match_the_full_selector_or_bare_id() {
        let allow = |pattern: &str| vec![pattern.to_string()];
        assert!(model_allowed(SELECTOR, &allow("prime-inference/*")));
        assert!(model_allowed(
            SELECTOR,
            &allow("prime-inference/internal/*")
        ));
        assert!(model_allowed(SELECTOR, &allow("internal/*")));
        assert!(model_allowed(SELECTOR, &allow("PRIME-INFERENCE/*")));
        // The provider wildcard does not swallow other providers.
        assert!(!model_allowed("zai/glm-5.3", &allow("prime-inference/*")));
        // A question mark is a wildcard, not a plain character.
        assert!(model_allowed("zai/glm-5.3", &allow("zai/glm-5.?")));
        // An empty pattern list allows nothing (the settings getter never
        // produces one, but the matching must stay total).
        assert!(!model_allowed(SELECTOR, &[]));
        assert!(!model_allowed(SELECTOR, &["  ".to_string()]));
        assert!(!model_allowed(SELECTOR, &allow("[")));
    }

    #[test]
    fn the_refusal_error_carries_the_selector() {
        let refusal = ModelAllowlistRefusal {
            selector: "zai/glm-5.3".to_string(),
            alias_patterns: Vec::new(),
        };
        assert_eq!(refusal.selector, "zai/glm-5.3");
        let message = refusal.to_string();
        assert!(
            message.contains("Model \"zai/glm-5.3\" is blocked by the daemon model allowlist"),
            "{message}"
        );
        assert!(message.contains("never falls back"), "{message}");
    }

    /// The lenient load would read a wrong-typed value as unset (every
    /// model passes); it must fail closed instead.
    #[test]
    fn a_malformed_subagent_models_setting_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let agent_dir = dir.path().join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        let malformed = || {
            SubagentModelFilter::Unreadable(
                "subagentModels is present but is not an array of strings".to_string(),
            )
        };
        let cases = [
            (r#"{ "subagentModels": "sol" }"#, malformed()),
            (r#"{ "subagentModels": ["zai/*", 42] }"#, malformed()),
            (r#"{ "subagentModels": { "sol": true } }"#, malformed()),
            (
                r#"{ "subagentModels": ["", " "] }"#,
                SubagentModelFilter::Unreadable(
                    "subagentModels contains only empty patterns".to_string(),
                ),
            ),
            (
                "[]",
                SubagentModelFilter::Unreadable(
                    "the global settings document is not a JSON object".to_string(),
                ),
            ),
            (
                r#"{ "subagentModels": null }"#,
                SubagentModelFilter::Unrestricted,
            ),
            (
                r#"{ "subagentModels": [] }"#,
                SubagentModelFilter::Unrestricted,
            ),
            ("{}", SubagentModelFilter::Unrestricted),
            (
                r#"{ "subagentModels": ["zai/*"] }"#,
                SubagentModelFilter::Only(vec!["zai/*".to_string()]),
            ),
        ];
        for (document, expected) in cases {
            std::fs::write(agent_dir.join("settings.json"), document).unwrap();
            let settings = crate::settings::SettingsManager::create(dir.path(), &agent_dir);
            assert_eq!(SubagentModelFilter::load(&settings), expected, "{document}");
        }
        std::fs::write(agent_dir.join("settings.json"), "{ not json").unwrap();
        let settings = crate::settings::SettingsManager::create(dir.path(), &agent_dir);
        assert!(matches!(
            SubagentModelFilter::load(&settings),
            SubagentModelFilter::Unreadable(_)
        ));
        assert!(!malformed().permits("zai/glm-5.3"));
    }

    #[test]
    fn the_subagent_refusal_lists_the_permitted_models() {
        let refusal = SubagentModelRefusal::OutsideFilter {
            selector: "openai/gpt-6-luna".to_string(),
            permitted: vec!["sol (openai/gpt-6.1-sol)".to_string()],
            alias_patterns: vec![AliasPattern {
                pattern: "luna".to_string(),
                selector: "openai/gpt-6-luna".to_string(),
            }],
        };
        assert_eq!(
            refusal.to_string(),
            "Model \"openai/gpt-6-luna\" is not permitted for subagents (settings \"subagentModels\"); \
             there is no fallback to another model. Pass model= one of: sol (openai/gpt-6.1-sol). \
             Note: \"subagentModels\" pattern \"luna\" is the models.json alias of openai/gpt-6-luna, \
             but model patterns match real model names, not aliases, so it matches nothing; \
             use \"openai/gpt-6-luna\"."
        );
    }
}
