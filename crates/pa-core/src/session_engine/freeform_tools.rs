//! The `freeformTools` gate at the session's stream boundary: tools keep their free-text
//! argument marker only while the setting resolves on for the request's provider, so the
//! provider picks the raw-text (custom) or JSON-schema wire form per request. History never
//! stores the wire form, so a session resumed after the setting changed replays in the other.

use std::sync::Arc;

use pa_agent::stream::StreamFn;

use super::telemetry::SessionCounters;
use crate::settings::FreeformToolsSettings;

/// The provider APIs that send a marked tool as a custom tool (others ignore the marker).
const CUSTOM_TOOL_APIS: [&str; 3] = [
    "openai-responses",
    "openai-codex-responses",
    "azure-openai-responses",
];

/// Wrap `stream_fn`: drop every tool's marker where the setting resolves off; count a request
/// that keeps one on an API with custom tools into the adoption counter.
pub(crate) fn gate_stream_fn(
    settings: FreeformToolsSettings,
    counters: Arc<SessionCounters>,
    stream_fn: StreamFn,
) -> StreamFn {
    Arc::new(move |model, mut context, options| {
        if settings.is_enabled_for(&model.provider) {
            if CUSTOM_TOOL_APIS.contains(&model.api.as_str())
                && context
                    .tools
                    .iter()
                    .any(|tool| tool.freeform_argument.is_some())
            {
                counters.note_freeform_tools_request();
            }
        } else {
            for tool in &mut context.tools {
                tool.freeform_argument = None;
            }
        }
        stream_fn(model, context, options)
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use pa_agent::stream::{LlmContext, StreamRequestOptions, ToolDefinition};

    use super::*;

    /// Send one `python_repl` request through the gate; returns the marker the provider saw and
    /// the adoption count.
    async fn run_gate(
        settings: FreeformToolsSettings,
        provider: &str,
        api: &str,
    ) -> (Option<String>, u64) {
        let model = serde_json::from_value(serde_json::json!({
            "id": "m", "api": api, "provider": provider,
        }))
        .expect("model");
        let seen = Arc::new(Mutex::new(None));
        let recorder = Arc::clone(&seen);
        let inner: StreamFn = Arc::new(move |_model, context: LlmContext, _options| {
            *recorder.lock().expect("seen") = context.tools[0].freeform_argument.clone();
            Box::pin(async { Err(anyhow::anyhow!("recorded")) })
        });
        let counters = Arc::new(SessionCounters::default());
        let gated = gate_stream_fn(settings, Arc::clone(&counters), inner);
        let context = LlmContext {
            system_prompt: None,
            messages: Vec::new(),
            tools: vec![ToolDefinition {
                name: "python_repl".into(),
                description: "run python".into(),
                parameters: serde_json::json!({"type": "object"}),
                freeform_argument: Some("code".into()),
            }],
        };
        let options = StreamRequestOptions {
            temperature: None,
            max_tokens: None,
            reasoning: pa_agent::types::ThinkingLevel::Off,
            session_id: None,
            service_tier: None,
            api_key: None,
            signal: pa_agent::abort::AbortSignal::default(),
            on_payload: None,
            on_response: None,
            headers: None,
        };
        assert!(gated(model, context, options).await.is_err());
        let marker = seen.lock().expect("seen").clone();
        (marker, counters.freeform_tools_request_count())
    }

    /// The marker survives exactly where the setting resolves on: provider entry, then the
    /// global `enabled`, then the built-in default (on for `openai` and `openai-codex` only).
    #[tokio::test]
    async fn gate_keeps_the_marker_only_where_the_setting_resolves_on() {
        let global_off_opencode_on = FreeformToolsSettings {
            enabled: Some(false),
            providers: Some([("opencode-go".to_string(), true)].into_iter().collect()),
        };
        let global_on_openai_off = FreeformToolsSettings {
            enabled: Some(true),
            providers: Some([("openai".to_string(), false)].into_iter().collect()),
        };
        let cases = [
            (FreeformToolsSettings::default(), "openai", true),
            (FreeformToolsSettings::default(), "openai-codex", true),
            (
                FreeformToolsSettings::default(),
                "azure-openai-responses",
                false,
            ),
            (global_off_opencode_on.clone(), "opencode-go", true),
            (global_off_opencode_on, "openai", false),
            (global_on_openai_off.clone(), "openai", false),
            (global_on_openai_off, "anthropic", true),
        ];
        for (settings, provider, kept) in cases {
            let (marker, _) = run_gate(settings, provider, "openai-responses").await;
            assert_eq!(marker.is_some(), kept, "{provider}");
        }
    }

    /// Regression: the adoption counter counts only requests whose API sends custom tools; an
    /// enabled provider on another API keeps the marker (its provider ignores it) uncounted.
    #[tokio::test]
    async fn adoption_counts_only_responses_api_requests() {
        let enabled = FreeformToolsSettings {
            enabled: Some(true),
            providers: None,
        };
        let mut counts = Vec::new();
        for api in [
            "openai-completions",
            "anthropic-messages",
            "openai-responses",
            "openai-codex-responses",
            "azure-openai-responses",
        ] {
            counts.push(run_gate(enabled.clone(), "p", api).await.1);
        }
        assert_eq!(counts, vec![0, 0, 1, 1, 1]);
    }
}
