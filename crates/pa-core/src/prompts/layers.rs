//! Static system-prompt layers assembled into the cache-stable prefix. Layers must never
//! contain session-specific values (the cache-safety guard test pins the boundary).

use std::borrow::Cow;

use pa_types::ai::EditFormat;

pub const CORE_LAYER: &str = include_str!("layers/core.md");
pub const USAGE_LAYER: &str = include_str!("layers/usage.md");
pub const OPINIONATED_LAYER: &str = include_str!("layers/opinionated.md");

/// The edit-guidance slots of the static layers: the line starting with the
/// prefix (one in `core.md`, one in `usage.md`) is the `edit` guidance, and
/// the paired file (one or more whole lines) replaces it for models whose
/// edit format is `apply_patch`.
/// The choice is per model, so each model's static prefix stays cache-stable.
const EDIT_GUIDANCE_SLOTS: [(&str, &str); 2] = [
    ("- `edit.run(", include_str!("layers/apply-patch-core.md")),
    (
        "- Edits of existing files must be performed using `edit`",
        include_str!("layers/apply-patch-usage.md"),
    ),
];

/// A static layer with its edit-guidance slot filled for `edit_format`:
/// `Edit` is the layer file byte for byte; `ApplyPatch` swaps the slot lines.
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

    /// Each slot prefix names exactly one line across the layers, so the
    /// swap can neither miss its line nor hit a second one; each replacement
    /// is whole lines.
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

    /// The `apply_patch` variant is the layer with each slot line replaced by
    /// its file and nothing else changed; `edit` borrows the layer as is.
    #[test]
    fn the_apply_patch_variant_swaps_only_the_slot_lines() {
        let swapped = |layer: &str| {
            let mut expected = layer.to_string();
            for (prefix, replacement) in EDIT_GUIDANCE_SLOTS {
                if let Some(start) = expected.find(&format!("\n{prefix}")) {
                    let start = start + 1;
                    let end = expected[start..]
                        .find('\n')
                        .map_or(expected.len(), |end| start + end + 1);
                    expected.replace_range(start..end, replacement);
                }
            }
            expected
        };
        assert_eq!(
            layer_for_edit_format(CORE_LAYER, EditFormat::ApplyPatch),
            swapped(CORE_LAYER)
        );
        assert_eq!(
            layer_for_edit_format(USAGE_LAYER, EditFormat::ApplyPatch),
            swapped(USAGE_LAYER)
        );
        assert_ne!(swapped(CORE_LAYER), CORE_LAYER);
        assert_ne!(swapped(USAGE_LAYER), USAGE_LAYER);
        assert!(matches!(
            layer_for_edit_format(CORE_LAYER, EditFormat::Edit),
            Cow::Borrowed(CORE_LAYER)
        ));
    }
}
