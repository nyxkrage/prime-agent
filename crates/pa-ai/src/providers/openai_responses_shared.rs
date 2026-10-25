//! `OpenAI` Responses API message and tool conversion: reasoning item replay via thinkingSignature,
//! text signatures, foreign tool-call id normalization. The stream event processor lives in
//! [`crate::providers::openai_responses_stream`].

use std::collections::HashSet;

use serde_json::{json, Map, Value};

use crate::providers::transform_messages::transform_messages_with_normalizer;
use crate::types::{
    AssistantContent, AssistantMessage, Context, Model, ModelExt, TextSignaturePhase, Tool,
};
use crate::utils_inner::hash::short_hash;
use crate::utils_inner::sanitize_unicode::sanitize_surrogates;

pub(crate) use crate::providers::openai_responses_hooks::FreeformToolArguments;
pub use crate::providers::openai_responses_hooks::ReasoningSummary;
pub use crate::providers::openai_responses_stream::{
    apply_service_tier_pricing, ResponsesStreamHooks, ResponsesStreamProcessor,
};

pub(crate) fn encode_text_signature_v1(id: &str, phase: Option<TextSignaturePhase>) -> String {
    let mut payload = Map::new();
    payload.insert("v".into(), json!(1));
    payload.insert("id".into(), json!(id));
    if let Some(phase) = phase {
        payload.insert("phase".into(), json!(phase));
    }
    Value::Object(payload).to_string()
}

#[derive(Clone, Debug, PartialEq)]
pub struct ParsedTextSignature {
    pub id: String,
    pub phase: Option<TextSignaturePhase>,
}

fn parse_text_signature(signature: Option<&str>) -> Option<ParsedTextSignature> {
    let signature = signature?;
    if signature.starts_with('{') {
        if let Ok(parsed) = serde_json::from_str::<Value>(signature) {
            if parsed.get("v") == Some(&json!(1)) {
                if let Some(id) = parsed.get("id").and_then(|value| value.as_str()) {
                    let phase = match parsed.get("phase").and_then(|value| value.as_str()) {
                        Some("commentary") => Some(TextSignaturePhase::Commentary),
                        Some("final_answer") => Some(TextSignaturePhase::FinalAnswer),
                        _ => None,
                    };
                    return Some(ParsedTextSignature {
                        id: id.to_string(),
                        phase,
                    });
                }
            }
        }
        // Fall through to legacy plain-string handling.
    }
    Some(ParsedTextSignature {
        id: signature.to_string(),
        phase: None,
    })
}

/// Providers whose tool-call IDs may carry the `call_id|item_id` Responses encoding natively.
pub const OPENAI_TOOL_CALL_PROVIDERS: [&str; 3] = ["openai", "openai-codex", "opencode"];
pub const AZURE_TOOL_CALL_PROVIDERS: [&str; 4] = [
    "openai",
    "openai-codex",
    "opencode",
    "azure-openai-responses",
];

#[derive(Clone, Copy, Debug, Default)]
pub struct ConvertResponsesMessagesOptions {
    pub include_system_prompt: bool,
}

impl ConvertResponsesMessagesOptions {
    pub fn include_system_prompt() -> Self {
        Self {
            include_system_prompt: true,
        }
    }
}

fn normalize_id_part(part: &str) -> String {
    let sanitized: String = part
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let mut normalized: String = sanitized.chars().take(64).collect();
    while normalized.ends_with('_') {
        normalized.pop();
    }
    normalized
}

fn build_foreign_responses_item_id(item_id: &str) -> String {
    let normalized = format!("fc_{}", short_hash(item_id));
    if normalized.len() > 64 {
        normalized.chars().take(64).collect()
    } else {
        normalized
    }
}

/// Convert a conversation to Responses API `input` items.
// Long by design: mirrors the provider's stream shape.
#[allow(clippy::too_many_lines)]
pub fn convert_responses_messages(
    model: &Model,
    context: &Context,
    allowed_tool_call_providers: &[&str],
    options: ConvertResponsesMessagesOptions,
) -> Vec<Value> {
    use crate::types::{Message, UserMessageContent, UserOrToolContent};
    let mut messages: Vec<Value> = Vec::new();

    // The wire shape of every recorded tool call follows this request's tools, never the shape it
    // was recorded in: a call to a freeform tool replays as `custom_tool_call`, anything else as
    // `function_call`.
    let freeform_tools = FreeformToolArguments::from_tools(context.tools.as_deref());

    let normalize_tool_call_id = |id: &str, source: &AssistantMessage| -> String {
        if !allowed_tool_call_providers.contains(&model.provider.as_str()) {
            return normalize_id_part(id);
        }
        if !id.contains('|') {
            return normalize_id_part(id);
        }
        let (call_id, item_id) = id.split_once('|').unwrap_or((id, ""));
        let normalized_call_id = normalize_id_part(call_id);
        let is_foreign_tool_call = source.provider != model.provider || source.api != model.api;
        let mut normalized_item_id = if is_foreign_tool_call {
            build_foreign_responses_item_id(item_id)
        } else {
            normalize_id_part(item_id)
        };
        // OpenAI Responses API requires item id to start with "fc".
        if !normalized_item_id.starts_with("fc_") {
            normalized_item_id = normalize_id_part(&format!("fc_{normalized_item_id}"));
        }
        format!("{normalized_call_id}|{normalized_item_id}")
    };

    let transformed =
        transform_messages_with_normalizer(&context.messages, model, &|id, _model, source| {
            Some(normalize_tool_call_id(id, source))
        });
    // Calls emitted as `custom_tool_call` (by their transformed id): their results go back as
    // `custom_tool_call_output`.
    let mut custom_call_ids: HashSet<String> = HashSet::new();

    let include_system_prompt = options.include_system_prompt;
    if include_system_prompt {
        if let Some(system_prompt) = &context.system_prompt {
            let role = if model.reasoning {
                "developer"
            } else {
                "system"
            };
            messages.push(json!({
                "role": role,
                "content": sanitize_surrogates(system_prompt),
            }));
        }
    }

    for (msg_index, msg) in transformed.iter().enumerate() {
        match msg {
            Message::User(user) => match &user.content {
                UserMessageContent::Text(text) => messages.push(json!({
                    "role": "user",
                    "content": [{ "type": "input_text", "text": sanitize_surrogates(text) }],
                })),
                UserMessageContent::Blocks(blocks) => {
                    let content_items: Vec<Value> = blocks
                        .iter()
                        .map(|item| match crate::types::user_block_payload(item) {
                            crate::types::UserBlockPayload::Text(text) => json!({
                                "type": "input_text",
                                "text": sanitize_surrogates(text),
                            }),
                            crate::types::UserBlockPayload::Image { data, mime_type } => json!({
                                "type": "input_image",
                                "detail": "auto",
                                "image_url": format!("data:{mime_type};base64,{data}"),
                            }),
                            crate::types::UserBlockPayload::Opaque(json) => json!({
                                "type": "input_text",
                                "text": sanitize_surrogates(&json),
                            }),
                        })
                        .collect();
                    if content_items.is_empty() {
                        continue;
                    }
                    messages.push(json!({
                        "role": "user",
                        "content": content_items,
                    }));
                }
            },
            Message::Assistant(assistant) => {
                let mut output: Vec<Value> = Vec::new();
                let is_different_model = assistant.model != model.id
                    && assistant.provider == model.provider
                    && assistant.api == model.api;
                let is_same_model = assistant.model == model.id
                    && assistant.provider == model.provider
                    && assistant.api == model.api;

                for block in &assistant.content {
                    match block {
                        AssistantContent::Thinking(thinking) => {
                            if let Some(signature) = &thinking.thinking_signature {
                                if let Ok(mut reasoning_item) =
                                    serde_json::from_str::<Value>(signature)
                                {
                                    if model.provider == "xai" {
                                        if let Some(item) = reasoning_item.as_object_mut() {
                                            item.remove("status");
                                        }
                                    }
                                    output.push(reasoning_item);
                                }
                            }
                        }
                        AssistantContent::Text(text) => {
                            let parsed_signature =
                                parse_text_signature(text.text_signature.as_deref());
                            // OpenAI requires id to be max 64 characters and rejects empty ids
                            // ([ApiParam][invalid_id]); a message without a usable signature id
                            // falls back to its index in the converted history.
                            let msg_id = match parsed_signature
                                .as_ref()
                                .filter(|signature| !signature.id.is_empty())
                            {
                                None => format!("msg_{msg_index}"),
                                Some(signature) => {
                                    if signature.id.len() > 64 {
                                        format!("msg_{}", short_hash(&signature.id))
                                    } else {
                                        signature.id.clone()
                                    }
                                }
                            };
                            let mut entry = Map::new();
                            entry.insert("type".into(), json!("message"));
                            entry.insert("role".into(), json!("assistant"));
                            entry.insert(
                                "content".into(),
                                json!([{
                                    "type": "output_text",
                                    "text": sanitize_surrogates(&text.text),
                                    "annotations": [],
                                }]),
                            );
                            entry.insert("status".into(), json!("completed"));
                            entry.insert("id".into(), json!(msg_id));
                            if let Some(signature) = &parsed_signature {
                                if let Some(phase) = signature.phase {
                                    entry.insert("phase".into(), json!(phase));
                                }
                            }
                            output.push(Value::Object(entry));
                        }
                        AssistantContent::ToolCall(tool_call) => {
                            // The item id is the `fc_` segment after the `|`. Without a `|` there
                            // is no item id: the `id` key is omitted (an empty id is rejected by
                            // the API with [ApiParam][invalid_id]) and the whole id serves as the
                            // call id.
                            let (call_id, item_id) = match tool_call.id.split_once('|') {
                                Some((call_id, item_id)) => {
                                    (call_id, (!item_id.is_empty()).then_some(item_id))
                                }
                                None => (tool_call.id.as_str(), None),
                            };
                            let mut entry = Map::new();
                            if let Some(input) = freeform_tools.custom_input(tool_call) {
                                entry.insert("type".into(), json!("custom_tool_call"));
                                // The item id replays only to the model that produced it, where it
                                // keeps the call after its reasoning item: an item id dropped while
                                // its `rs_` reasoning item is still sent is rejected ("reasoning
                                // item provided without its required following item"). A stored
                                // `fc_` id (a call recorded as a function call) goes out as
                                // `ctc_<id>`, mirroring the `function_call` rule below; any other
                                // id passes through unchanged (gateways may issue their own item
                                // ids). Another model's item id is omitted. History keeps the
                                // stored id.
                                if let Some(item_id) = item_id.filter(|_| is_same_model) {
                                    let item_id = if item_id.starts_with("fc_") {
                                        normalize_id_part(&format!("ctc_{item_id}"))
                                    } else {
                                        item_id.to_string()
                                    };
                                    entry.insert("id".into(), json!(item_id));
                                }
                                entry.insert("call_id".into(), json!(call_id));
                                entry.insert("name".into(), json!(tool_call.name));
                                entry.insert("input".into(), json!(sanitize_surrogates(input)));
                                custom_call_ids.insert(tool_call.id.clone());
                                output.push(Value::Object(entry));
                                continue;
                            }
                            entry.insert("type".into(), json!("function_call"));
                            // For different-model messages, omit the id to avoid pairing validation
                            // against rs_ reasoning items tracked by the provider.
                            let omit_id = item_id.is_none()
                                || (is_different_model
                                    && item_id.is_some_and(|id| id.starts_with("fc_")));
                            // A `ctc_` custom-tool-call id (a freeform call replayed with
                            // freeform tools off) is rejected as a `function_call` item id
                            // ("Expected an ID that begins with 'fc'"), so it goes out as
                            // `fc_<id>` on every Responses provider: prefixed rather than dropped,
                            // so the item still follows its reasoning item. Any other id passes
                            // through unchanged (gateways may issue their own item ids). History
                            // keeps the stored id.
                            if let Some(item_id) = item_id.filter(|_| !omit_id) {
                                let item_id = if item_id.starts_with("ctc_") {
                                    normalize_id_part(&format!("fc_{item_id}"))
                                } else {
                                    item_id.to_string()
                                };
                                entry.insert("id".into(), json!(item_id));
                            }
                            entry.insert("call_id".into(), json!(call_id));
                            entry.insert("name".into(), json!(tool_call.name));
                            entry.insert(
                                "arguments".into(),
                                json!(Value::Object(tool_call.arguments.clone()).to_string()),
                            );
                            output.push(Value::Object(entry));
                        }
                    }
                }
                if output.is_empty() {
                    continue;
                }
                messages.extend(output);
            }
            Message::ToolResult(tool_result) => {
                let text_result = tool_result
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        UserOrToolContent::Text(text) => Some(text.text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let has_images = tool_result
                    .content
                    .iter()
                    .any(|block| matches!(block, UserOrToolContent::Image(_)));
                let has_text = !text_result.is_empty();
                let call_id = tool_result
                    .tool_call_id
                    .split('|')
                    .next()
                    .unwrap_or_default();

                let output_value = if has_images && model.supports_image_input() {
                    let mut content_parts: Vec<Value> = Vec::new();
                    if has_text {
                        content_parts.push(json!({
                            "type": "input_text",
                            "text": sanitize_surrogates(&text_result),
                        }));
                    }
                    for block in &tool_result.content {
                        if let UserOrToolContent::Image(image) = block {
                            content_parts.push(json!({
                                "type": "input_image",
                                "detail": "auto",
                                "image_url": format!("data:{};base64,{}", image.mime_type, image.data),
                            }));
                        }
                    }
                    json!(content_parts)
                } else {
                    json!(sanitize_surrogates(if has_text {
                        text_result.as_str()
                    } else if has_images {
                        "(see attached image)"
                    } else {
                        ""
                    }))
                };

                let output_type = if custom_call_ids.contains(&tool_result.tool_call_id) {
                    "custom_tool_call_output"
                } else {
                    "function_call_output"
                };
                messages.push(json!({
                    "type": output_type,
                    "call_id": call_id,
                    "output": output_value,
                }));
            }
        }
    }

    messages
}

#[derive(Clone, Copy, Debug)]
pub struct ConvertResponsesToolsOptions {
    pub strict: Option<bool>,
}

/// Convert tool definitions to Responses API tools. A tool with a free-text argument goes out as
/// a raw-text `custom` tool; every other tool stays a JSON-schema `function` tool.
pub fn convert_responses_tools(
    tools: &[Tool],
    options: ConvertResponsesToolsOptions,
) -> Vec<Value> {
    let strict = options.strict.unwrap_or(false);
    tools
        .iter()
        .map(|tool| match &tool.freeform_argument {
            // A custom tool has no parameter schema, so the free-text argument's description
            // (what the raw input is, e.g. Python source, and how to write it) joins the tool's.
            Some(argument) => {
                let argument_description = tool
                    .parameters
                    .pointer(&format!("/properties/{argument}/description"))
                    .and_then(Value::as_str)
                    .filter(|description| !description.is_empty());
                let description = match argument_description {
                    Some(argument_description) => format!(
                        "{}\n\nInput: the raw `{argument}` text, not JSON. {argument_description}",
                        tool.description
                    ),
                    None => tool.description.clone(),
                };
                json!({
                    "type": "custom",
                    "name": tool.name,
                    "description": description,
                    "format": { "type": "text" },
                })
            }
            None => json!({
                "type": "function",
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.parameters,
                "strict": strict,
            }),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        Message, StopReason, TextContent, ThinkingContent, ToolCall, ToolResultMessage, Usage,
        UserMessage, UserMessageContent, UserOrToolContent,
    };

    fn codex_model() -> Model {
        Model {
            id: "gpt-5.1-codex".into(),
            name: "gpt-5.1-codex".into(),
            api: "openai-codex-responses".into(),
            provider: "openai-codex".into(),
            base_url: "https://chatgpt.com/backend-api".into(),
            reasoning: true,
            thinking_level_map: None,
            input: vec![crate::types::ModelInput::Text],
            cost: crate::types::zero_model_cost(),
            context_window: 400_000,
            max_tokens: 128_000,
            max_tokens_explicit: false,
            featured: None,
            headers: None,
            compat: None,
        }
    }

    /// An `openai` provider model on the `openai-responses` API.
    fn openai_model() -> Model {
        Model {
            id: "gpt-5.5".into(),
            name: "gpt-5.5".into(),
            api: "openai-responses".into(),
            provider: "openai".into(),
            base_url: "https://api.openai.com/v1".into(),
            ..codex_model()
        }
    }

    /// `messages` as recorded by `model`: every assistant turn carries its api, provider and id,
    /// so the conversion treats them as same-model history.
    fn recorded_by(model: &Model, mut messages: Vec<Message>) -> Vec<Message> {
        for message in &mut messages {
            if let Message::Assistant(assistant) = message {
                assistant.api.clone_from(&model.api);
                assistant.provider.clone_from(&model.provider);
                assistant.model.clone_from(&model.id);
            }
        }
        messages
    }

    fn user(text: &str) -> Message {
        Message::User(UserMessage {
            content: UserMessageContent::Text(text.into()),
            timestamp: 0,
            rest: Map::default(),
        })
    }

    fn assistant(content: Vec<AssistantContent>, model_id: &str) -> Message {
        Message::Assistant(AssistantMessage {
            content,
            api: "openai-codex-responses".into(),
            provider: "openai-codex".into(),
            model: model_id.into(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            usage: Usage::default(),
            stop_reason: StopReason::ToolUse,
            stop_reason_raw: None,
            error_message: None,
            timestamp: 0,
            rest: Map::default(),
        })
    }

    fn tool_call(id: &str) -> AssistantContent {
        AssistantContent::ToolCall(ToolCall {
            id: id.into(),
            name: "bash".into(),
            arguments: [("cmd".to_string(), json!("ls"))].into_iter().collect(),
            thought_signature: None,
            rest: Map::default(),
        })
    }

    fn text_block(text: &str, signature: Option<&str>) -> AssistantContent {
        AssistantContent::Text(TextContent {
            text: text.into(),
            text_signature: signature.map(str::to_string),
            rest: Map::default(),
        })
    }

    fn tool_result(tool_call_id: &str) -> Message {
        Message::ToolResult(ToolResultMessage {
            tool_call_id: tool_call_id.into(),
            tool_name: "bash".into(),
            content: vec![UserOrToolContent::Text(TextContent {
                text: "ok".into(),
                text_signature: None,
                rest: Map::default(),
            })],
            details: None,
            is_error: false,
            timestamp: 0,
            rest: Map::default(),
        })
    }

    fn convert(model: &Model, messages: Vec<Message>) -> Vec<Value> {
        convert_with_tools(model, messages, Vec::new())
    }

    fn convert_with_tools(model: &Model, messages: Vec<Message>, tools: Vec<Tool>) -> Vec<Value> {
        convert_responses_messages(
            model,
            &Context {
                system_prompt: None,
                messages,
                tools: Some(tools),
            },
            &OPENAI_TOOL_CALL_PROVIDERS,
            ConvertResponsesMessagesOptions {
                include_system_prompt: false,
            },
        )
    }

    /// Every input item that carries an `id` or `call_id` must be non-empty: the Responses API
    /// rejects empty ids with `[ApiParam][invalid_id]`.
    fn assert_no_empty_ids(items: &[Value]) {
        for item in items {
            for key in ["id", "call_id"] {
                if let Some(Value::String(id)) = item.get(key) {
                    assert!(!id.is_empty(), "empty {key} in item: {item}");
                }
            }
        }
    }

    /// A tool call without the `call_id|item_id` encoding omits the item `id` entirely and keeps
    /// the whole id as `call_id` (TS evidence: `convertResponsesMessages` with a pipe-less
    /// toolCall.id; the dogfood bug emitted `id: ""` here and the API rejected the turn).
    #[test]
    fn pipe_less_tool_call_id_omits_item_id() {
        let items = convert(
            &codex_model(),
            vec![
                user("hi"),
                assistant(vec![tool_call("call_x")], "gpt-5.1-codex"),
                tool_result("call_x"),
            ],
        );
        assert_no_empty_ids(&items);
        assert_eq!(
            items,
            vec![
                json!({
                    "role": "user",
                    "content": [{ "type": "input_text", "text": "hi" }],
                }),
                json!({
                    "type": "function_call",
                    "call_id": "call_x",
                    "name": "bash",
                    "arguments": "{\"cmd\":\"ls\"}",
                }),
                json!({
                    "type": "function_call_output",
                    "call_id": "call_x",
                    "output": "ok",
                }),
            ]
        );
    }

    /// A tool call id with an empty `fc_` segment (stream items without an id) omits the item `id`
    /// instead of emitting `id: ""`.
    #[test]
    fn empty_item_id_segment_omits_item_id() {
        let items = convert(
            &codex_model(),
            vec![
                user("hi"),
                assistant(vec![tool_call("call_x|")], "gpt-5.1-codex"),
                tool_result("call_x|"),
            ],
        );
        assert_no_empty_ids(&items);
        let function_call = items
            .iter()
            .find(|item| item.get("type").and_then(Value::as_str) == Some("function_call"));
        let Some(function_call) = function_call else {
            panic!("missing function_call item in {items:?}");
        };
        assert!(function_call.get("id").is_none());
        assert_eq!(function_call.get("call_id"), Some(&json!("call_x")));
    }

    /// A fully-empty tool call id omits the item `id`; the `call_id` fields stay empty on both
    /// items, matching the TS reference exactly (the reachable dogfood shape never produces an
    /// empty tool call id because tool results inherit the tool call id verbatim).
    #[test]
    fn empty_tool_call_id_omits_item_id() {
        let items = convert(
            &codex_model(),
            vec![
                user("hi"),
                assistant(vec![tool_call("")], "gpt-5.1-codex"),
                tool_result(""),
            ],
        );
        let function_call = items
            .iter()
            .find(|item| item.get("type").and_then(Value::as_str) == Some("function_call"));
        let Some(function_call) = function_call else {
            panic!("missing function_call item in {items:?}");
        };
        assert!(function_call.get("id").is_none());
        assert_eq!(function_call.get("call_id"), Some(&json!("")));
    }

    /// Same-provider messages from a different model omit the `fc_` item id so the API does not
    /// pair it against `rs_` reasoning items tracked for this model (TS: `itemId = undefined`).
    #[test]
    fn different_model_fc_item_id_is_omitted() {
        let items = convert(
            &codex_model(),
            vec![
                user("a"),
                assistant(vec![tool_call("call_x|fc_y")], "gpt-5"),
                tool_result("call_x|fc_y"),
            ],
        );
        let function_call = items
            .iter()
            .find(|item| item.get("type").and_then(Value::as_str) == Some("function_call"));
        let Some(function_call) = function_call else {
            panic!("missing function_call item in {items:?}");
        };
        assert!(function_call.get("id").is_none());
        assert_eq!(function_call.get("call_id"), Some(&json!("call_x")));
    }

    /// A piped same-model tool call keeps its `fc_` item id.
    #[test]
    fn piped_tool_call_id_keeps_item_id() {
        let items = convert(
            &codex_model(),
            vec![
                user("hi"),
                assistant(vec![tool_call("call_x|fc_y")], "gpt-5.1-codex"),
                tool_result("call_x|fc_y"),
            ],
        );
        assert_no_empty_ids(&items);
        let function_call = items
            .iter()
            .find(|item| item.get("type").and_then(Value::as_str) == Some("function_call"));
        let Some(function_call) = function_call else {
            panic!("missing function_call item in {items:?}");
        };
        assert_eq!(function_call.get("id"), Some(&json!("fc_y")));
        assert_eq!(function_call.get("call_id"), Some(&json!("call_x")));
    }

    /// A same-model `function_call` item id the API would reject (a `ctc_` custom-tool-call id)
    /// goes out `fc_`-prefixed and capped at 64 characters, for a provider on the piped-id list
    /// and for a gateway off it. Any other id, such as a gateway's own non-`fc` item id, is sent
    /// unchanged.
    #[test]
    fn same_model_non_fc_item_id_gets_fc_prefix() {
        let long_ctc = format!("ctc_{}", "a".repeat(70));
        let cases = [
            ("call_x|ctc_y", "fc_ctc_y".to_string()),
            ("call_x|gw_item_1", "gw_item_1".to_string()),
            (
                &*format!("call_x|{long_ctc}"),
                format!("fc_{long_ctc}").chars().take(64).collect(),
            ),
        ];
        for provider in ["openai-codex", "opencode-go"] {
            for (tool_call_id, expected_id) in &cases {
                let model = Model {
                    provider: provider.into(),
                    ..codex_model()
                };
                let mut call = assistant(vec![tool_call(tool_call_id)], "gpt-5.1-codex");
                if let Message::Assistant(assistant) = &mut call {
                    assistant.provider = provider.into();
                }
                let items = convert(&model, vec![user("hi"), call, tool_result(tool_call_id)]);
                let function_call = items
                    .iter()
                    .find(|item| item.get("type").and_then(Value::as_str) == Some("function_call"));
                assert_eq!(
                    function_call,
                    Some(&json!({
                        "type": "function_call",
                        "id": expected_id,
                        "call_id": "call_x",
                        "name": "bash",
                        "arguments": "{\"cmd\":\"ls\"}",
                    })),
                    "provider {provider}, id {tool_call_id}"
                );
            }
        }
    }

    /// Assistant text without a signature id falls back to its converted history index instead of
    /// an empty message id (TS: `msg_${msgIndex}`).
    #[test]
    fn message_without_signature_uses_index_fallback_id() {
        let items = convert(
            &codex_model(),
            vec![
                user("a"),
                assistant(vec![text_block("one", None)], "gpt-5.1-codex"),
            ],
        );
        let message = items
            .iter()
            .find(|item| item.get("type").and_then(Value::as_str) == Some("message"));
        let Some(message) = message else {
            panic!("missing message item in {items:?}");
        };
        assert_eq!(message.get("id"), Some(&json!("msg_1")));
    }

    /// A signature with an empty id is treated as missing (TS `!msgId`).
    #[test]
    fn message_with_empty_signature_id_uses_index_fallback() {
        let signature = "{\"v\":1,\"id\":\"\"}";
        let items = convert(
            &codex_model(),
            vec![
                user("a"),
                assistant(vec![text_block("hello", Some(signature))], "gpt-5.1-codex"),
            ],
        );
        let message = items
            .iter()
            .find(|item| item.get("type").and_then(Value::as_str) == Some("message"));
        let Some(message) = message else {
            panic!("missing message item in {items:?}");
        };
        assert_eq!(message.get("id"), Some(&json!("msg_1")));
    }

    fn python_tool(freeform_argument: Option<&str>) -> Tool {
        Tool {
            name: "python_repl".into(),
            description: "Run Python.".into(),
            parameters: json!({
                "type": "object",
                "properties": { "code": { "type": "string" } },
                "required": ["code"],
            }),
            freeform_argument: freeform_argument.map(str::to_string),
        }
    }

    fn python_call(id: &str, code: &str) -> AssistantContent {
        AssistantContent::ToolCall(ToolCall {
            id: id.into(),
            name: "python_repl".into(),
            arguments: [("code".to_string(), json!(code))].into_iter().collect(),
            thought_signature: None,
            rest: Map::default(),
        })
    }

    /// The history a freeform session records: the canonical tool call (`{"code": ...}`) under the
    /// Responses `call_id|ctc_item_id` encoding, then its result.
    fn freeform_history(model_id: &str) -> Vec<Message> {
        vec![
            user("hi"),
            assistant(vec![python_call("call_x|ctc_y", "print(1)")], model_id),
            tool_result("call_x|ctc_y"),
        ]
    }

    /// A tool with a free-text argument goes out as a raw-text custom tool; structured tools keep
    /// the JSON-schema function form.
    #[test]
    fn freeform_tool_converts_to_custom_tool() {
        let bash = Tool {
            name: "bash".into(),
            description: "Run bash.".into(),
            parameters: json!({ "type": "object" }),
            freeform_argument: None,
        };
        assert_eq!(
            convert_responses_tools(
                &[python_tool(Some("code")), bash],
                ConvertResponsesToolsOptions { strict: None },
            ),
            vec![
                json!({
                    "type": "custom",
                    "name": "python_repl",
                    "description": "Run Python.",
                    "format": { "type": "text" },
                }),
                json!({
                    "type": "function",
                    "name": "bash",
                    "description": "Run bash.",
                    "parameters": { "type": "object" },
                    "strict": false,
                }),
            ]
        );
    }

    /// Regression: a custom tool has no parameter schema, so the free-text argument's schema
    /// description must reach the model through the tool description.
    #[test]
    fn custom_tool_description_carries_the_argument_description() {
        let tool = Tool {
            parameters: json!({
                "type": "object",
                "properties": {
                    "code": { "type": "string", "description": "Python code to execute." },
                },
                "required": ["code"],
            }),
            ..python_tool(Some("code"))
        };
        assert_eq!(
            convert_responses_tools(&[tool], ConvertResponsesToolsOptions { strict: None }),
            vec![json!({
                "type": "custom",
                "name": "python_repl",
                "description": "Run Python.\n\nInput: the raw `code` text, not JSON. \
                    Python code to execute.",
                "format": { "type": "text" },
            })]
        );
    }

    /// With freeform tools on, a recorded custom call replays to the same model as
    /// `custom_tool_call` with its `ctc_` item id intact, and its result as
    /// `custom_tool_call_output`.
    #[test]
    fn freeform_call_replays_as_custom_tool_call_with_ctc_id() {
        let items = convert_with_tools(
            &codex_model(),
            freeform_history("gpt-5.1-codex"),
            vec![python_tool(Some("code"))],
        );
        assert_eq!(
            items,
            vec![
                json!({
                    "role": "user",
                    "content": [{ "type": "input_text", "text": "hi" }],
                }),
                json!({
                    "type": "custom_tool_call",
                    "id": "ctc_y",
                    "call_id": "call_x",
                    "name": "python_repl",
                    "input": "print(1)",
                }),
                json!({
                    "type": "custom_tool_call_output",
                    "call_id": "call_x",
                    "output": "ok",
                }),
            ]
        );
    }

    /// A different model of the same provider keeps the custom form but drops the `ctc_` item id
    /// (the id pairs with that model's reasoning items), and the normalizer must not rewrite it to
    /// an `fc_` id on the way.
    #[test]
    fn freeform_call_from_another_model_omits_its_item_id() {
        let items = convert_with_tools(
            &codex_model(),
            freeform_history("gpt-5"),
            vec![python_tool(Some("code"))],
        );
        assert_eq!(
            items[1..],
            [
                json!({
                    "type": "custom_tool_call",
                    "call_id": "call_x",
                    "name": "python_repl",
                    "input": "print(1)",
                }),
                json!({
                    "type": "custom_tool_call_output",
                    "call_id": "call_x",
                    "output": "ok",
                }),
            ]
        );
    }

    /// Function-call history (recorded with freeform tools off) replays to the same model as
    /// custom tool calls once they are on; its `fc_` item id goes out `ctc_`-prefixed.
    #[test]
    fn function_call_history_replays_as_custom_tool_call_when_enabled() {
        let items = convert_with_tools(
            &codex_model(),
            vec![
                user("hi"),
                assistant(
                    vec![python_call("call_x|fc_y", "print(1)")],
                    "gpt-5.1-codex",
                ),
                tool_result("call_x|fc_y"),
            ],
            vec![python_tool(Some("code"))],
        );
        assert_eq!(
            items[1..],
            [
                json!({
                    "type": "custom_tool_call",
                    "id": "ctc_fc_y",
                    "call_id": "call_x",
                    "name": "python_repl",
                    "input": "print(1)",
                }),
                json!({
                    "type": "custom_tool_call_output",
                    "call_id": "call_x",
                    "output": "ok",
                }),
            ]
        );
    }

    /// Regression: a session recorded with freeform tools replays with the flag off. The call
    /// goes out as a `function_call` carrying the JSON arguments; from another model of the same
    /// provider the normalized `fc_ctc_y` id is omitted like any other-model `fc_` id.
    #[test]
    fn freeform_history_replays_as_function_call_when_disabled() {
        let items = convert_with_tools(
            &codex_model(),
            freeform_history("gpt-5"),
            vec![python_tool(None)],
        );
        assert_eq!(
            items[1..],
            [
                json!({
                    "type": "function_call",
                    "call_id": "call_x",
                    "name": "python_repl",
                    "arguments": "{\"code\":\"print(1)\"}",
                }),
                json!({
                    "type": "function_call_output",
                    "call_id": "call_x",
                    "output": "ok",
                }),
            ]
        );
    }

    /// The same-model half of the flag-off replay: the `ctc_` item id must reach the wire as
    /// `fc_ctc_y`.
    #[test]
    fn freeform_history_replays_to_the_same_model_as_function_call_when_disabled() {
        let items = convert_with_tools(
            &codex_model(),
            freeform_history("gpt-5.1-codex"),
            vec![python_tool(None)],
        );
        assert_eq!(
            items[1..],
            [
                json!({
                    "type": "function_call",
                    "id": "fc_ctc_y",
                    "call_id": "call_x",
                    "name": "python_repl",
                    "arguments": "{\"code\":\"print(1)\"}",
                }),
                json!({
                    "type": "function_call_output",
                    "call_id": "call_x",
                    "output": "ok",
                }),
            ]
        );
    }

    /// Toggling freeform tools on -> off -> on changes only the wire shape of each request: the
    /// history keeps its recorded ids, so the third request replays both the call recorded with
    /// freeform on (`ctc_y`) and the one recorded with it off (`fc_w`, sent as `ctc_fc_w`) as
    /// custom tool calls again.
    #[test]
    fn freeform_toggle_on_off_on_replays_custom_calls_again() {
        let mut history = freeform_history("gpt-5.1-codex");
        history.extend([
            user("again"),
            assistant(
                vec![python_call("call_z|fc_w", "print(2)")],
                "gpt-5.1-codex",
            ),
            tool_result("call_z|fc_w"),
        ]);
        let on = || vec![python_tool(Some("code"))];
        let first = convert_with_tools(&codex_model(), history.clone(), on());
        let off = convert_with_tools(&codex_model(), history.clone(), vec![python_tool(None)]);
        let again = convert_with_tools(&codex_model(), history, on());

        assert_eq!(first, again);
        let calls = |items: &[Value]| -> Vec<Value> {
            items
                .iter()
                .filter(|item| item.get("call_id").is_some())
                .cloned()
                .collect()
        };
        assert_eq!(
            calls(&again),
            vec![
                json!({
                    "type": "custom_tool_call",
                    "id": "ctc_y",
                    "call_id": "call_x",
                    "name": "python_repl",
                    "input": "print(1)",
                }),
                json!({ "type": "custom_tool_call_output", "call_id": "call_x", "output": "ok" }),
                json!({
                    "type": "custom_tool_call",
                    "id": "ctc_fc_w",
                    "call_id": "call_z",
                    "name": "python_repl",
                    "input": "print(2)",
                }),
                json!({ "type": "custom_tool_call_output", "call_id": "call_z", "output": "ok" }),
            ]
        );
        assert_eq!(
            calls(&off),
            vec![
                json!({
                    "type": "function_call",
                    "id": "fc_ctc_y",
                    "call_id": "call_x",
                    "name": "python_repl",
                    "arguments": "{\"code\":\"print(1)\"}",
                }),
                json!({ "type": "function_call_output", "call_id": "call_x", "output": "ok" }),
                json!({
                    "type": "function_call",
                    "id": "fc_w",
                    "call_id": "call_z",
                    "name": "python_repl",
                    "arguments": "{\"code\":\"print(2)\"}",
                }),
                json!({ "type": "function_call_output", "call_id": "call_z", "output": "ok" }),
            ]
        );
    }

    /// Structured arguments that are not exactly the free-text argument cannot become raw input:
    /// such a call stays a `function_call` even with freeform tools on.
    #[test]
    fn structured_arguments_stay_a_function_call() {
        let call = AssistantContent::ToolCall(ToolCall {
            id: "call_x|fc_y".into(),
            name: "python_repl".into(),
            arguments: [
                ("code".to_string(), json!("print(1)")),
                ("timeout".to_string(), json!(5)),
            ]
            .into_iter()
            .collect(),
            thought_signature: None,
            rest: Map::default(),
        });
        let items = convert_with_tools(
            &codex_model(),
            vec![
                user("hi"),
                assistant(vec![call], "gpt-5.1-codex"),
                tool_result("call_x|fc_y"),
            ],
            vec![python_tool(Some("code"))],
        );
        assert_eq!(
            items[1..],
            [
                json!({
                    "type": "function_call",
                    "id": "fc_y",
                    "call_id": "call_x",
                    "name": "python_repl",
                    "arguments": "{\"code\":\"print(1)\",\"timeout\":5}",
                }),
                json!({
                    "type": "function_call_output",
                    "call_id": "call_x",
                    "output": "ok",
                }),
            ]
        );
    }

    /// A same-model turn whose reasoning item precedes the call: a reasoning item recorded with
    /// the turn, then the call under `id`, then its result.
    fn reasoning_call_history(id: &str) -> Vec<Message> {
        let reasoning = AssistantContent::Thinking(ThinkingContent {
            thinking: String::new(),
            thinking_signature: Some(
                json!({ "type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": "enc" })
                    .to_string(),
            ),
            redacted: None,
            rest: Map::default(),
        });
        vec![
            user("hi"),
            assistant(
                vec![reasoning, python_call(id, "print(1)")],
                "gpt-5.1-codex",
            ),
            tool_result(id),
        ]
    }

    /// Regression: toggling freeform tools in either direction replays a same-model call in the
    /// other shape with an item id of that shape, so the call still follows its reasoning item
    /// (an id-less call after a sent `rs_` item is rejected by the API).
    #[test]
    fn shape_change_keeps_the_item_id_after_its_reasoning_item() {
        let reasoning =
            json!({ "type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": "enc" });
        // off -> on: a function call recorded with freeform tools off replays as a custom call.
        let off_to_on = convert_with_tools(
            &codex_model(),
            reasoning_call_history("call_x|fc_y"),
            vec![python_tool(Some("code"))],
        );
        assert_eq!(
            off_to_on[1..],
            [
                reasoning.clone(),
                json!({
                    "type": "custom_tool_call",
                    "id": "ctc_fc_y",
                    "call_id": "call_x",
                    "name": "python_repl",
                    "input": "print(1)",
                }),
                json!({ "type": "custom_tool_call_output", "call_id": "call_x", "output": "ok" }),
            ]
        );
        // on -> off: a custom call recorded with freeform tools on replays as a function call.
        let on_to_off = convert_with_tools(
            &codex_model(),
            reasoning_call_history("call_x|ctc_y"),
            vec![python_tool(None)],
        );
        assert_eq!(
            on_to_off[1..],
            [
                reasoning,
                json!({
                    "type": "function_call",
                    "id": "fc_ctc_y",
                    "call_id": "call_x",
                    "name": "python_repl",
                    "arguments": "{\"code\":\"print(1)\"}",
                }),
                json!({ "type": "function_call_output", "call_id": "call_x", "output": "ok" }),
            ]
        );
    }

    /// Regression: replaying a same-model call as a `custom_tool_call` rewrites only a stored
    /// `fc_` item id (to `ctc_<id>`), mirroring the `function_call` side, which rewrites only
    /// `ctc_`. A `ctc_` id and a gateway's own item id pass through unchanged.
    #[test]
    fn custom_tool_call_rewrites_only_fc_item_ids() {
        let cases = [
            ("call_x|fc_y", "ctc_fc_y"),
            ("call_x|ctc_y", "ctc_y"),
            ("call_x|gw_item_1", "gw_item_1"),
        ];
        let opencode_model = Model {
            provider: "opencode-go".into(),
            ..openai_model()
        };
        for model in [codex_model(), openai_model(), opencode_model] {
            for (tool_call_id, expected_id) in cases {
                let items = convert_with_tools(
                    &model,
                    recorded_by(&model, reasoning_call_history(tool_call_id)),
                    vec![python_tool(Some("code"))],
                );
                assert_eq!(
                    items.get(2),
                    Some(&json!({
                        "type": "custom_tool_call",
                        "id": expected_id,
                        "call_id": "call_x",
                        "name": "python_repl",
                        "input": "print(1)",
                    })),
                    "provider {}, id {tool_call_id}",
                    model.provider
                );
            }
        }
    }
}
