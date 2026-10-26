//! Static system-prompt layers assembled into the cache-stable prefix. Layers must never
//! contain session-specific values (the cache-safety guard test pins the boundary).

use std::borrow::Cow;

use pa_types::ai::EditFormat;

pub const CORE_LAYER: &str = include_str!("layers/core.md");
pub const USAGE_LAYER: &str = include_str!("layers/usage.md");
pub const OPINIONATED_LAYER: &str = include_str!("layers/opinionated.md");

/// The edit-guidance slots of the static layers: the line starting with each
/// prefix is the `edit` guidance; the paired file (whole lines) replaces it
/// for the `apply_patch` edit format. The choice is per model, so each
/// model's static prefix stays cache-stable.
const EDIT_GUIDANCE_SLOTS: [(&str, &str); 3] = [
    ("- `edit.run(", include_str!("layers/apply-patch-core.md")),
    (
        "- Edits of existing files must be performed using `edit`",
        include_str!("layers/apply-patch-usage.md"),
    ),
    (
        "- Agents use Python for reading files and searching in them",
        include_str!("layers/apply-patch-usage-variables.md"),
    ),
];

/// A static layer with its edit-guidance slots filled for `edit_format`
/// (`Edit` is the layer file byte for byte).
#[must_use]
pub fn layer_for_edit_format(layer: &'static str, edit_format: EditFormat) -> Cow<'static, str> {
    match edit_format {
        EditFormat::Edit => Cow::Borrowed(layer),
        EditFormat::ApplyPatch => Cow::Owned(
            layer
                .split_inclusive('\n')
                .map(|line| {
                    EDIT_GUIDANCE_SLOTS
                        .iter()
                        .find(|(prefix, _)| line.starts_with(prefix))
                        .map_or(line, |(_, replacement)| replacement)
                })
                .collect(),
        ),
    }
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

    /// Each slot prefix names exactly one layer line, and each replacement is
    /// whole lines, so a layer edit cannot silently break the swap.
    #[test]
    fn every_edit_guidance_slot_matches_exactly_one_layer_line() {
        for (prefix, replacement) in EDIT_GUIDANCE_SLOTS {
            let matches = [CORE_LAYER, USAGE_LAYER, OPINIONATED_LAYER]
                .iter()
                .flat_map(|layer| layer.lines())
                .filter(|line| line.starts_with(prefix))
                .count();
            assert_eq!(matches, 1, "slot {prefix:?}");
            assert!(replacement.ends_with('\n'), "slot {prefix:?} replacement");
        }
    }
}
