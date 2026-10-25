//! The model-pattern warning (`model_pattern_warning`): a settings model pattern
//! (`allowedModels`, `subagentModels`) that names a models.json alias matches
//! nothing, because patterns match canonical `provider/model-id` names only.
//! A top-level session reports such patterns once, as a display row riding its
//! first admitted turn. The row is user-facing and never reaches the model
//! (`convert_to_llm` drops it).

use pa_types::session::CustomMessage;

use crate::models::AliasPattern;

pub(crate) const MODEL_PATTERN_WARNING_CUSTOM_TYPE: &str = "model_pattern_warning";

/// The warning row for `(setting, pattern)` pairs, or `None` when every
/// pattern names a real model.
pub(crate) fn notice_message(alias_patterns: &[(&str, AliasPattern)]) -> Option<CustomMessage> {
    if alias_patterns.is_empty() {
        return None;
    }
    let mut lines = vec!["[model-pattern-warning]".to_string(), String::new()];
    lines.extend(
        alias_patterns
            .iter()
            .map(|(setting, alias_pattern)| format!("- settings \"{setting}\": {alias_pattern}")),
    );
    Some(CustomMessage {
        custom_type: MODEL_PATTERN_WARNING_CUSTOM_TYPE.to_string(),
        content: pa_types::ai::UserContent::Text(lines.join("\n")),
        display: true,
        details: None,
        timestamp: super::refine::now_millis(),
        rest: serde_json::Map::default(),
    })
}
