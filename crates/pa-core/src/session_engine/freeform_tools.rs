//! The `freeformTools` gate at the session's stream boundary: tools carry their free-text
//! argument marker into every request only while the setting resolves on for the request's
//! provider, so the provider picks the raw-text (custom) or JSON-schema wire form per request.
//! The setting is read once at session start. The recorded session never stores the wire form,
//! so a session resumed after the setting changed replays the same history in the other form.

use std::sync::Arc;

use pa_agent::stream::StreamFn;

use super::telemetry::SessionCounters;
use crate::settings::FreeformToolsSettings;

/// The provider APIs that send a marked tool as a raw-text custom tool (every other API ignores
/// the marker and sends the JSON-schema form).
const CUSTOM_TOOL_APIS: [&str; 3] = [
    "openai-responses",
    "openai-codex-responses",
    "azure-openai-responses",
];

/// Wrap `stream_fn`: a request to a provider the setting resolves off for drops every tool's
/// free-text marker; a request that keeps one on an API with custom tools counts into the
/// session's adoption counter.
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

    fn python_tool() -> ToolDefinition {
        ToolDefinition {
            name: "python_repl".into(),
            description: "run python".into(),
            parameters: serde_json::json!({"type": "object"}),
            freeform_argument: Some("code".into()),
        }
    }

    fn model(provider: &str) -> pa_agent::types::Model {
        model_on_api(provider, "openai-responses")
    }

    fn model_on_api(provider: &str, api: &str) -> pa_agent::types::Model {
        serde_json::from_value(serde_json::json!({
            "id": "m", "api": api, "provider": provider,
        }))
        .expect("model")
    }

    /// Run one request through the gate and return the free-text markers the provider saw.
    async fn markers_seen(
        settings: FreeformToolsSettings,
        provider: &str,
        counters: Arc<SessionCounters>,
    ) -> Vec<Option<String>> {
        markers_seen_on(settings, model(provider), counters).await
    }

    async fn markers_seen_on(
        settings: FreeformToolsSettings,
        model: pa_agent::types::Model,
        counters: Arc<SessionCounters>,
    ) -> Vec<Option<String>> {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&seen);
        let inner: StreamFn = Arc::new(move |_model, context: LlmContext, _options| {
            *recorder.lock().expect("seen") = context
                .tools
                .iter()
                .map(|tool| tool.freeform_argument.clone())
                .collect();
            Box::pin(async { Err(anyhow::anyhow!("recorded")) })
        });
        let gated = gate_stream_fn(settings, counters, inner);
        let context = LlmContext {
            system_prompt: None,
            messages: Vec::new(),
            tools: vec![python_tool()],
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
        let markers = seen.lock().expect("seen").clone();
        markers
    }

    #[tokio::test]
    async fn default_keeps_the_marker_for_openai_and_drops_it_elsewhere() {
        let counters = Arc::new(SessionCounters::default());
        let defaults = FreeformToolsSettings::default();
        assert_eq!(
            markers_seen(defaults.clone(), "openai", Arc::clone(&counters)).await,
            vec![Some("code".to_string())]
        );
        assert_eq!(
            markers_seen(defaults.clone(), "openai-codex", Arc::clone(&counters)).await,
            vec![Some("code".to_string())]
        );
        assert_eq!(
            markers_seen(defaults, "azure-openai-responses", Arc::clone(&counters)).await,
            vec![None]
        );
    }

    #[tokio::test]
    async fn provider_entry_beats_the_global_override() {
        let settings = FreeformToolsSettings {
            enabled: Some(false),
            providers: Some([("opencode-go".to_string(), true)].into_iter().collect()),
        };
        let counters = Arc::new(SessionCounters::default());
        assert_eq!(
            markers_seen(settings.clone(), "opencode-go", Arc::clone(&counters)).await,
            vec![Some("code".to_string())]
        );
        assert_eq!(
            markers_seen(settings, "openai", Arc::clone(&counters)).await,
            vec![None]
        );
    }

    #[test]
    fn resolution_order_is_provider_then_global_then_default() {
        let global_on = FreeformToolsSettings {
            enabled: Some(true),
            providers: Some([("openai".to_string(), false)].into_iter().collect()),
        };
        let resolved: Vec<bool> = ["openai", "openai-codex", "anthropic"]
            .into_iter()
            .map(|provider| global_on.is_enabled_for(provider))
            .collect();
        assert_eq!(resolved, vec![false, true, true]);
    }

    /// Regression: the adoption counter counts only requests whose API sends custom tools.
    /// An enabled provider on Chat Completions or Anthropic Messages keeps the marker (the
    /// provider ignores it) but must not count as freeform adoption.
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
            let counters = Arc::new(SessionCounters::default());
            markers_seen_on(
                enabled.clone(),
                model_on_api("p", api),
                Arc::clone(&counters),
            )
            .await;
            counts.push(counters.freeform_tools_request_count());
        }
        assert_eq!(counts, vec![0, 0, 1, 1, 1]);
    }
}
