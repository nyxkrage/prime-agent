//! The `model_pattern_warning` display row: a settings model pattern that
//! names a models.json alias matches nothing. Shown once on a top-level
//! session's first turn; `convert_to_llm` keeps it from the model.

use pa_types::session::CustomMessage;

use crate::models::registry::ModelRegistry;

pub(crate) const MODEL_PATTERN_WARNING_CUSTOM_TYPE: &str = "model_pattern_warning";

/// The warning row, or `None` when no pattern names an alias.
pub(crate) fn notice_message(
    registry: &ModelRegistry,
    allowed_models: Option<&[String]>,
    subagent_models: Option<&[String]>,
) -> Option<CustomMessage> {
    let lines: Vec<String> = [
        ("allowedModels", allowed_models),
        ("subagentModels", subagent_models),
    ]
    .into_iter()
    .flat_map(|(setting, patterns)| {
        registry
            .alias_patterns(patterns.unwrap_or_default())
            .into_iter()
            .map(move |alias_pattern| format!("- settings \"{setting}\": {alias_pattern}"))
    })
    .collect();
    if lines.is_empty() {
        return None;
    }
    Some(CustomMessage {
        custom_type: MODEL_PATTERN_WARNING_CUSTOM_TYPE.to_string(),
        content: pa_types::ai::UserContent::Text(format!(
            "[model-pattern-warning]\n\n{}",
            lines.join("\n")
        )),
        display: true,
        details: None,
        timestamp: super::refine::now_millis(),
        rest: serde_json::Map::default(),
    })
}
