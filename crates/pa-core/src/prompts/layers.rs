//! Static system-prompt layers assembled into the cache-stable prefix. Layers must never
//! contain session-specific values (the cache-safety guard test pins the boundary).

use pa_types::ai::EditFormat;

pub const CORE_LAYER: &str = include_str!("layers/core.md");
pub const USAGE_LAYER: &str = include_str!("layers/usage.md");
pub const OPINIONATED_LAYER: &str = include_str!("layers/opinionated.md");

/// Marker lines that branch a static layer by edit format: lines after the
/// `edit` marker render for `edit`, lines after the `apply_patch` marker for
/// `apply_patch`, up to the end marker. Markers never render, so the `edit`
/// rendering is the TS layer byte for byte, and each model's prefix stays
/// cache-stable.
const EDIT_FORMAT_EDIT: &str = "<!-- edit-format: edit -->";
const EDIT_FORMAT_APPLY_PATCH: &str = "<!-- edit-format: apply_patch -->";
const EDIT_FORMAT_END: &str = "<!-- /edit-format -->";

/// A static layer rendered for `edit_format`.
#[must_use]
pub fn layer_for_edit_format(layer: &str, edit_format: EditFormat) -> String {
    let mut branch = None;
    layer
        .split_inclusive('\n')
        .filter(|line| match line.trim_end() {
            EDIT_FORMAT_EDIT => {
                branch = Some(EditFormat::Edit);
                false
            }
            EDIT_FORMAT_APPLY_PATCH => {
                branch = Some(EditFormat::ApplyPatch);
                false
            }
            EDIT_FORMAT_END => {
                branch = None;
                false
            }
            _ => branch.is_none_or(|branch| branch == edit_format),
        })
        .collect()
}

/// Source file of one layer (breakdown provenance).
#[must_use]
pub fn layer_source(name: &str) -> Option<&'static str> {
    match name {
        "core" => Some("prompts/layers/core.md"),
        "usage" => Some("prompts/layers/usage.md"),
        "opinionated" => Some("prompts/layers/opinionated.md"),
        "model-prompts" => Some("prompts/layers/model_prompts.toml"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every edit-format block opens with the `edit` branch, then the
    /// `apply_patch` branch, then closes, so neither format loses guidance.
    #[test]
    fn every_edit_format_block_has_both_branches() {
        for layer in [CORE_LAYER, USAGE_LAYER, OPINIONATED_LAYER] {
            let markers: Vec<&str> = layer
                .lines()
                .filter(|line| line.starts_with("<!-- ") && line.contains("edit-format"))
                .collect();
            assert_eq!(markers.len() % 3, 0, "{markers:?}");
            for block in markers.chunks(3) {
                assert_eq!(
                    block,
                    [EDIT_FORMAT_EDIT, EDIT_FORMAT_APPLY_PATCH, EDIT_FORMAT_END]
                );
            }
        }
    }
}
