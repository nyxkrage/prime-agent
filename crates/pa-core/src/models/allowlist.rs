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
}

impl std::fmt::Display for ModelAllowlistRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Model \"{}\" is blocked by the daemon model allowlist (settings \"allowedModels\"); \
             the daemon never falls back to a different model. Allow it in the settings \
             or pick an allowed model.",
            self.selector
        )
    }
}

impl std::error::Error for ModelAllowlistRefusal {}

/// The settings `subagentModels` filter: the models `rlm.find_models` lists
/// and `rlm.spawn`/`rlm.create_session` accept. Global settings only; unset
/// never falls back to `enabledModels`. Patterns match canonical
/// `provider/model-id` names only (models.json aliases are never expanded).
/// Like the daemon `allowedModels` allowlist it fails CLOSED: a policy that
/// could not be read is never an unrestricted one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubagentModelFilter {
    /// No `subagentModels` configured: every searchable model.
    Unrestricted,
    /// The configured patterns, matched against canonical selectors.
    Only(Vec<String>),
    /// The global settings document could not be loaded, or `subagentModels`
    /// is present but not an array of strings: the configured policy is
    /// unknown, so no model is permitted for subagents.
    Unreadable(String),
}

impl SubagentModelFilter {
    /// Read the global `subagentModels` list, failing closed on an
    /// unreadable global document or a malformed value (the lenient settings
    /// load drops a wrong-typed field to unset, which must not read as
    /// "no filter"); explicit `null` and an empty list stay unset.
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
            Some(serde_json::Value::Array(items))
                if items.iter().all(serde_json::Value::is_string) =>
            {
                Self::Unrestricted
            }
            Some(_) => Self::Unreadable(
                "subagentModels is present but is not an array of strings".to_string(),
            ),
        }
    }

    /// Whether the resolved `provider/model-id` selector passes the filter
    /// (never when the filter is unreadable).
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
/// models.json alias and matches no canonical model name. Patterns match
/// `provider/model-id` names only, so such a pattern selects nothing; it is
/// surfaced as a warning instead of being silently expanded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AliasPattern {
    /// The pattern as configured.
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

/// An RLM child model outside the settings `subagentModels` filter (named
/// explicitly or inherited from the parent), refused even when it is
/// authenticated; typed so the spawn seams can count the refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentModelRefusal {
    /// The refused model, full selector form `provider/model-id`.
    pub selector: String,
    /// Why the filter refused it.
    pub cause: SubagentModelRefusalCause,
}

/// Why [`SubagentModelRefusal`] refused a model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubagentModelRefusalCause {
    /// The model matches none of the configured patterns.
    OutsideFilter {
        /// The permitted models as spawnable names (`alias (selector)` or
        /// the selector), so the caller can retry in one step.
        permitted: Vec<String>,
        /// Filter patterns that name an alias instead of a model (they
        /// match nothing); listed so the refusal explains a short list.
        alias_patterns: Vec<AliasPattern>,
    },
    /// The filter could not be read ([`SubagentModelFilter::Unreadable`]):
    /// every model is refused until the settings are fixed.
    UnreadableFilter(String),
}

impl std::fmt::Display for SubagentModelRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let selector = &self.selector;
        match &self.cause {
            SubagentModelRefusalCause::OutsideFilter {
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
            SubagentModelRefusalCause::UnreadableFilter(error) => write!(
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
        };
        assert_eq!(refusal.selector, "zai/glm-5.3");
        let message = refusal.to_string();
        assert!(
            message.contains("Model \"zai/glm-5.3\" is blocked by the daemon model allowlist"),
            "{message}"
        );
        assert!(message.contains("never falls back"), "{message}");
    }

    #[test]
    fn the_subagent_filter_permits_only_matching_selectors() {
        assert!(SubagentModelFilter::Unrestricted.permits("zai/glm-5.3"));
        let filter =
            SubagentModelFilter::Only(vec!["openai/gpt-6.1-sol".to_string(), "zai/*".to_string()]);
        assert!(filter.permits("openai/gpt-6.1-sol"));
        assert!(filter.permits("zai/glm-5.3"));
        assert!(!filter.permits("openai/gpt-6-luna"));
    }

    /// A wrong-typed `subagentModels` or an unreadable global document
    /// fails closed (the lenient load would otherwise read it as unset, so
    /// every model would pass); `null` and an empty list stay unset.
    #[test]
    fn a_malformed_subagent_models_setting_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let agent_dir = dir.path().join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        let load = |document: &str| {
            std::fs::write(agent_dir.join("settings.json"), document).unwrap();
            SubagentModelFilter::load(&crate::settings::SettingsManager::create(
                dir.path(),
                &agent_dir,
            ))
        };
        let malformed = SubagentModelFilter::Unreadable(
            "subagentModels is present but is not an array of strings".to_string(),
        );
        assert_eq!(load(r#"{ "subagentModels": "sol" }"#), malformed);
        assert_eq!(load(r#"{ "subagentModels": ["zai/*", 42] }"#), malformed);
        assert_eq!(load(r#"{ "subagentModels": { "sol": true } }"#), malformed);
        assert!(!malformed.permits("zai/glm-5.3"));
        assert!(matches!(
            load("{ not json"),
            SubagentModelFilter::Unreadable(_)
        ));
        assert!(matches!(load("[]"), SubagentModelFilter::Unreadable(_)));
        assert_eq!(
            load(r#"{ "subagentModels": null }"#),
            SubagentModelFilter::Unrestricted
        );
        assert_eq!(
            load(r#"{ "subagentModels": [] }"#),
            SubagentModelFilter::Unrestricted
        );
        assert_eq!(load("{}"), SubagentModelFilter::Unrestricted);
        assert_eq!(
            load(r#"{ "subagentModels": ["zai/*"] }"#),
            SubagentModelFilter::Only(vec!["zai/*".to_string()])
        );
    }

    #[test]
    fn the_subagent_refusal_lists_the_permitted_models() {
        let refusal = SubagentModelRefusal {
            selector: "openai/gpt-6-luna".to_string(),
            cause: SubagentModelRefusalCause::OutsideFilter {
                permitted: vec!["sol (openai/gpt-6.1-sol)".to_string()],
                alias_patterns: vec![AliasPattern {
                    pattern: "luna".to_string(),
                    selector: "openai/gpt-6-luna".to_string(),
                }],
            },
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
