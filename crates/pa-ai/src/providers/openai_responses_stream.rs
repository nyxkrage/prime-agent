//! `OpenAI` Responses stream event processor: hook types and service-tier pricing live in
//! [`super::openai_responses_hooks`].
//!
//! `handle_event` stays one large function: a 1:1 port of the TS event match, kept whole for
//! traceability to the TS source.

use std::collections::HashMap;

use serde_json::{json, Map, Value};

use crate::event_stream::{AssistantMessageEvent, AssistantMessageEventWriter};
use crate::models::calculate_cost;
pub use crate::providers::openai_responses_hooks::{
    apply_service_tier_pricing, ResponsesStreamHooks,
};
use crate::providers::openai_responses_shared::encode_text_signature_v1;
use crate::types::{
    AssistantContent, AssistantMessage, Model, StopReason, TextContent, TextSignaturePhase,
    ThinkingContent, ToolCall, Usage, UsageCost,
};
use crate::utils_inner::json_parse::{parse_streaming_json, StreamingJsonAccumulator};
use crate::utils_inner::stream_failure::{
    classify_stream_failure, ProviderError, StreamFailureError, StreamFailureInfo,
    StreamFailureKind,
};

/// Streaming slot state for one output item.
struct Slot {
    /// The provider's item object (reasoning/message), kept for replay.
    item: Value,
    /// Scratch for tool-call argument accumulation.
    partial_json: StreamingJsonAccumulator,
    /// Scratch for a custom tool call's raw text input (empty for every other item).
    custom_input: String,
    /// How much of `custom_input` the `toolcall_delta` events have carried so far.
    custom_deltas: CustomInputDeltas,
    content_index: usize,
}

/// Progress of a custom tool call's `toolcall_delta` stream. The deltas carry JSON argument
/// fragments, like a `function_call`'s: joined, they parse as `{"<argument>": input}`.
enum CustomInputDeltas {
    /// No fragment sent yet; the first one opens the object (`{"code":"`).
    Unopened,
    /// The object is open and carries this much raw input (JSON-escaped on the wire).
    Open { emitted: String },
    /// The closing `"}` was sent; later input changes get no further fragment.
    Closed,
}

impl Slot {
    /// The JSON-escaped custom input not yet carried by a fragment. `None` when the input was
    /// rewritten past what was already streamed: sent text cannot be retracted, so fragments
    /// stop there and the finalized tool call carries the authoritative arguments.
    fn unsent_custom_input(&self) -> Option<String> {
        let emitted = match &self.custom_deltas {
            CustomInputDeltas::Open { emitted } => emitted.as_str(),
            CustomInputDeltas::Unopened | CustomInputDeltas::Closed => "",
        };
        let new_text = self.custom_input.strip_prefix(emitted)?;
        let quoted = Value::String(new_text.to_string()).to_string();
        Some(quoted[1..quoted.len() - 1].to_string())
    }

    /// The next JSON argument fragment for the custom input received since the last one: the
    /// opener (`{"code":"`) with the first text, then the escaped new text. Empty when there
    /// is nothing new.
    fn stream_custom_delta(&mut self, argument: &str) -> String {
        let opener = match &self.custom_deltas {
            CustomInputDeltas::Closed => return String::new(),
            CustomInputDeltas::Unopened if self.custom_input.is_empty() => return String::new(),
            CustomInputDeltas::Unopened => json_object_opener(argument),
            CustomInputDeltas::Open { .. } => String::new(),
        };
        let Some(unsent) = self.unsent_custom_input() else {
            return opener;
        };
        self.custom_deltas = CustomInputDeltas::Open {
            emitted: self.custom_input.clone(),
        };
        format!("{opener}{unsent}")
    }

    /// The last fragment of the custom input: any unsent text plus the closing `"}` (the whole
    /// `{"code":"..."}` object when nothing was streamed). Empty once closed.
    fn close_custom_delta(&mut self, argument: &str) -> String {
        let opener = match &self.custom_deltas {
            CustomInputDeltas::Closed => return String::new(),
            CustomInputDeltas::Unopened => json_object_opener(argument),
            CustomInputDeltas::Open { .. } => String::new(),
        };
        let unsent = self.unsent_custom_input().unwrap_or_default();
        self.custom_deltas = CustomInputDeltas::Closed;
        format!("{opener}{unsent}\"}}")
    }
}

/// The start of a one-string-argument JSON object: `{"<argument>":"`.
fn json_object_opener(argument: &str) -> String {
    let key = Value::String(argument.to_string()).to_string();
    format!("{{{key}:\"")
}

/// Incremental Responses stream event processor (port of `processResponsesStream`).
pub struct ResponsesStreamProcessor<'a> {
    model: &'a Model,
    output: &'a mut AssistantMessage,
    writer: &'a AssistantMessageEventWriter,
    hooks: ResponsesStreamHooks,
    slots: HashMap<u64, Slot>,
    current_output_index: Option<u64>,
    saw_terminal_response: bool,
}

impl<'a> ResponsesStreamProcessor<'a> {
    pub fn new(
        model: &'a Model,
        output: &'a mut AssistantMessage,
        writer: &'a AssistantMessageEventWriter,
        hooks: ResponsesStreamHooks,
    ) -> Self {
        Self {
            model,
            output,
            writer,
            hooks,
            slots: HashMap::new(),
            current_output_index: None,
            saw_terminal_response: false,
        }
    }

    fn block_kind(&self, content_index: usize) -> Option<&'static str> {
        match self.output.content.get(content_index)? {
            AssistantContent::Text(_) => Some("text"),
            AssistantContent::Thinking(_) => Some("thinking"),
            AssistantContent::ToolCall(_) => Some("toolCall"),
        }
    }

    fn current_slot(&mut self, output_index: Option<u64>) -> Option<&mut Slot> {
        match output_index.or(self.current_output_index) {
            Some(index) => self.slots.get_mut(&index),
            None => None,
        }
    }

    /// Process one parsed stream event. Errors mirror the TS thrown `StreamFailureError`s.
    // Long by design: mirrors the provider's stream shape.
    #[allow(clippy::too_many_lines)]
    pub fn handle_event(&mut self, event: &Value) -> Result<(), ProviderError> {
        let event_type = event
            .get("type")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        let output_index = event
            .get("output_index")
            .and_then(serde_json::Value::as_u64);
        if event.get("output_index").is_some() {
            self.current_output_index = output_index;
        }

        match event_type {
            "response.created" => {
                if let Some(id) = event
                    .get("response")
                    .and_then(|response| response.get("id"))
                    .and_then(|value| value.as_str())
                {
                    self.output.response_id = Some(id.to_string());
                }
            }
            "response.output_item.added" => {
                let content_index = self.output.content.len();
                let item = event.get("item").cloned().unwrap_or(Value::Null);
                let item_type = item
                    .get("type")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                match item_type {
                    "reasoning" => {
                        self.output
                            .content
                            .push(AssistantContent::Thinking(ThinkingContent {
                                thinking: String::new(),
                                thinking_signature: None,
                                redacted: None,
                                rest: Map::default(),
                            }));
                        self.writer.push(AssistantMessageEvent::ThinkingStart {
                            content_index: content_index as u64,
                            partial: self.output.clone(),
                        });
                        if let Some(index) = output_index {
                            self.slots.insert(
                                index,
                                Slot {
                                    item,
                                    partial_json: StreamingJsonAccumulator::default(),
                                    custom_input: String::new(),
                                    custom_deltas: CustomInputDeltas::Unopened,
                                    content_index,
                                },
                            );
                        }
                    }
                    "message" => {
                        self.output
                            .content
                            .push(AssistantContent::Text(TextContent {
                                text: String::new(),
                                text_signature: None,
                                rest: Map::default(),
                            }));
                        self.writer.push(AssistantMessageEvent::TextStart {
                            content_index: content_index as u64,
                            partial: self.output.clone(),
                        });
                        if let Some(index) = output_index {
                            self.slots.insert(
                                index,
                                Slot {
                                    item,
                                    partial_json: StreamingJsonAccumulator::default(),
                                    custom_input: String::new(),
                                    custom_deltas: CustomInputDeltas::Unopened,
                                    content_index,
                                },
                            );
                        }
                    }
                    // A custom tool call is the same tool call with a raw text input: the input
                    // maps onto the tool's free-text argument (`{"code": input}`).
                    "function_call" | "custom_tool_call" => {
                        let call_id = item
                            .get("call_id")
                            .and_then(|value| value.as_str())
                            .unwrap_or("");
                        let item_id = item
                            .get("id")
                            .and_then(|value| value.as_str())
                            .unwrap_or("");
                        let name = item
                            .get("name")
                            .and_then(|value| value.as_str())
                            .unwrap_or("");
                        let (initial_arguments, custom_input, arguments) =
                            if item_type == "custom_tool_call" {
                                let input = item
                                    .get("input")
                                    .and_then(|value| value.as_str())
                                    .unwrap_or("");
                                let arguments =
                                    self.hooks.freeform_tools.arguments_from_input(name, input);
                                (String::new(), input.to_string(), arguments)
                            } else {
                                let initial_arguments = item
                                    .get("arguments")
                                    .and_then(|value| value.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                (initial_arguments, String::new(), Map::default())
                            };
                        self.output
                            .content
                            .push(AssistantContent::ToolCall(ToolCall {
                                id: format!("{call_id}|{item_id}"),
                                name: name.to_string(),
                                arguments,
                                thought_signature: None,
                                rest: Map::default(),
                            }));
                        self.writer.push(AssistantMessageEvent::ToolcallStart {
                            content_index: content_index as u64,
                            partial: self.output.clone(),
                        });
                        if let Some(index) = output_index {
                            self.slots.insert(
                                index,
                                Slot {
                                    item,
                                    partial_json: StreamingJsonAccumulator::new(initial_arguments),
                                    custom_input,
                                    custom_deltas: CustomInputDeltas::Unopened,
                                    content_index,
                                },
                            );
                        }
                    }
                    _ => {}
                }
            }
            "response.reasoning_summary_part.added" => {
                if let Some(slot) = self.current_slot(output_index) {
                    if slot.item.get("type").and_then(|v| v.as_str()) == Some("reasoning") {
                        let summary = slot
                            .item
                            .as_object_mut()
                            .expect("item is an object")
                            .entry("summary".to_string())
                            .or_insert_with(|| json!([]));
                        if let Some(array) = summary.as_array_mut() {
                            array.push(event.get("part").cloned().unwrap_or(Value::Null));
                        }
                    }
                }
            }
            "response.reasoning_summary_text.delta" => {
                let delta = event
                    .get("delta")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                let content_index = self
                    .current_slot(output_index)
                    .map(|slot| slot.content_index);
                if let Some(content_index) = content_index {
                    if self.block_kind(content_index) == Some("thinking") {
                        let mut append = false;
                        if let Some(slot) = self.current_slot(output_index) {
                            let last_part = slot
                                .item
                                .get_mut("summary")
                                .and_then(|summary| summary.as_array_mut())
                                .and_then(|parts| parts.last_mut());
                            if let Some(last_part) = last_part {
                                if let Some(Value::String(text)) = last_part
                                    .as_object_mut()
                                    .expect("part is an object")
                                    .get_mut("text")
                                {
                                    text.push_str(delta);
                                    append = true;
                                }
                            }
                        }
                        if append {
                            if let Some(AssistantContent::Thinking(block)) =
                                self.output.content.get_mut(content_index)
                            {
                                block.thinking.push_str(delta);
                            }
                            self.writer.push(AssistantMessageEvent::ThinkingDelta {
                                content_index: content_index as u64,
                                delta: delta.to_string(),
                                partial: self.output.clone(),
                            });
                        }
                    }
                }
            }
            "response.reasoning_summary_part.done" => {
                let content_index = self
                    .current_slot(output_index)
                    .map(|slot| slot.content_index);
                if let Some(content_index) = content_index {
                    if self.block_kind(content_index) == Some("thinking") {
                        if let Some(slot) = self.current_slot(output_index) {
                            let summary = slot
                                .item
                                .as_object_mut()
                                .expect("item is an object")
                                .entry("summary".to_string())
                                .or_insert_with(|| json!([]));
                            if let Some(array) = summary.as_array_mut() {
                                if let Some(Value::String(text)) = array
                                    .last_mut()
                                    .and_then(|last_part| last_part.get_mut("text"))
                                {
                                    text.push_str("\n\n");
                                }
                            }
                        }
                        if let Some(AssistantContent::Thinking(block)) =
                            self.output.content.get_mut(content_index)
                        {
                            block.thinking.push_str("\n\n");
                        }
                        self.writer.push(AssistantMessageEvent::ThinkingDelta {
                            content_index: content_index as u64,
                            delta: "\n\n".to_string(),
                            partial: self.output.clone(),
                        });
                    }
                }
            }
            "response.reasoning_text.delta" => {
                let delta = event
                    .get("delta")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                let content_index = self
                    .current_slot(output_index)
                    .map(|slot| slot.content_index);
                if let Some(content_index) = content_index {
                    if self.block_kind(content_index) == Some("thinking") {
                        if let Some(AssistantContent::Thinking(block)) =
                            self.output.content.get_mut(content_index)
                        {
                            block.thinking.push_str(delta);
                        }
                        self.writer.push(AssistantMessageEvent::ThinkingDelta {
                            content_index: content_index as u64,
                            delta: delta.to_string(),
                            partial: self.output.clone(),
                        });
                    }
                }
            }
            "response.content_part.added" => {
                if let Some(slot) = self.current_slot(output_index) {
                    if slot.item.get("type").and_then(|value| value.as_str()) == Some("message") {
                        let part_type = event
                            .get("part")
                            .and_then(|part| part.get("type"))
                            .and_then(|value| value.as_str())
                            .unwrap_or("");
                        if part_type == "output_text" || part_type == "refusal" {
                            let content = slot
                                .item
                                .as_object_mut()
                                .expect("item is an object")
                                .entry("content".to_string())
                                .or_insert_with(|| json!([]));
                            if let Some(array) = content.as_array_mut() {
                                array.push(event.get("part").cloned().unwrap_or(Value::Null));
                            }
                        }
                    }
                }
            }
            "response.output_text.delta" => {
                let delta = event
                    .get("delta")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                let content_index = self
                    .current_slot(output_index)
                    .map(|slot| slot.content_index);
                if let Some(content_index) = content_index {
                    if self.block_kind(content_index) == Some("text") {
                        let mut append = false;
                        if let Some(slot) = self.current_slot(output_index) {
                            let last_part = slot
                                .item
                                .get_mut("content")
                                .and_then(|content| content.as_array_mut())
                                .and_then(|parts| parts.last_mut());
                            if let Some(last_part) = last_part {
                                if last_part.get("type").and_then(|value| value.as_str())
                                    == Some("output_text")
                                {
                                    if let Some(Value::String(text)) = last_part.get_mut("text") {
                                        text.push_str(delta);
                                        append = true;
                                    }
                                }
                            }
                        }
                        if append {
                            if let Some(AssistantContent::Text(block)) =
                                self.output.content.get_mut(content_index)
                            {
                                block.text.push_str(delta);
                            }
                            self.writer.push(AssistantMessageEvent::TextDelta {
                                content_index: content_index as u64,
                                delta: delta.to_string(),
                                partial: self.output.clone(),
                            });
                        }
                    }
                }
            }
            "response.refusal.delta" => {
                let delta = event
                    .get("delta")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                let content_index = self
                    .current_slot(output_index)
                    .map(|slot| slot.content_index);
                if let Some(content_index) = content_index {
                    if self.block_kind(content_index) == Some("text") {
                        let mut append = false;
                        if let Some(slot) = self.current_slot(output_index) {
                            let last_part = slot
                                .item
                                .get_mut("content")
                                .and_then(|content| content.as_array_mut())
                                .and_then(|parts| parts.last_mut());
                            if let Some(last_part) = last_part {
                                if last_part.get("type").and_then(|value| value.as_str())
                                    == Some("refusal")
                                {
                                    if let Some(Value::String(refusal)) =
                                        last_part.get_mut("refusal")
                                    {
                                        refusal.push_str(delta);
                                        append = true;
                                    }
                                }
                            }
                        }
                        if append {
                            if let Some(AssistantContent::Text(block)) =
                                self.output.content.get_mut(content_index)
                            {
                                block.text.push_str(delta);
                            }
                            self.writer.push(AssistantMessageEvent::TextDelta {
                                content_index: content_index as u64,
                                delta: delta.to_string(),
                                partial: self.output.clone(),
                            });
                        }
                    }
                }
            }
            "response.function_call_arguments.delta" => {
                let delta = event
                    .get("delta")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                let content_index = self
                    .current_slot(output_index)
                    .map(|slot| slot.content_index);
                if let Some(content_index) = content_index {
                    if self.block_kind(content_index) == Some("toolCall") {
                        let mut parsed: Option<Value> = None;
                        if let Some(slot) = self.current_slot(output_index) {
                            parsed = slot.partial_json.append(delta);
                        }
                        if let Some(parsed) = parsed {
                            if let Some(AssistantContent::ToolCall(tool_call)) =
                                self.output.content.get_mut(content_index)
                            {
                                tool_call.arguments =
                                    parsed.as_object().cloned().unwrap_or_default();
                            }
                        }
                        self.writer.push(AssistantMessageEvent::ToolcallDelta {
                            content_index: content_index as u64,
                            delta: delta.to_string(),
                            partial: self.output.clone(),
                        });
                    }
                }
            }
            "response.function_call_arguments.done" => {
                let arguments = event
                    .get("arguments")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                let content_index = self
                    .current_slot(output_index)
                    .map(|slot| slot.content_index);
                if let Some(content_index) = content_index {
                    if self.block_kind(content_index) == Some("toolCall") {
                        let (previous_partial, parsed) = match self.current_slot(output_index) {
                            Some(slot) => {
                                let previous = slot.partial_json.text().to_string();
                                slot.partial_json =
                                    StreamingJsonAccumulator::new(arguments.to_string());
                                (previous, parse_streaming_json(Some(arguments)))
                            }
                            None => (String::new(), parse_streaming_json(Some(arguments))),
                        };
                        if let Some(AssistantContent::ToolCall(tool_call)) =
                            self.output.content.get_mut(content_index)
                        {
                            tool_call.arguments = parsed.as_object().cloned().unwrap_or_default();
                        }
                        if let Some(delta) = arguments.strip_prefix(&previous_partial) {
                            if !delta.is_empty() {
                                self.writer.push(AssistantMessageEvent::ToolcallDelta {
                                    content_index: content_index as u64,
                                    delta: delta.to_string(),
                                    partial: self.output.clone(),
                                });
                            }
                        }
                    }
                }
            }
            "response.custom_tool_call_input.delta" | "response.custom_tool_call_input.done" => {
                let content_index = self
                    .current_slot(output_index)
                    .map(|slot| slot.content_index);
                if let Some(content_index) = content_index {
                    if self.block_kind(content_index) == Some("toolCall") {
                        let is_done = event_type == "response.custom_tool_call_input.done";
                        let text_field = if is_done { "input" } else { "delta" };
                        let text = event
                            .get(text_field)
                            .and_then(|value| value.as_str())
                            .unwrap_or("");
                        let tool_name = match self.output.content.get(content_index) {
                            Some(AssistantContent::ToolCall(tool_call)) => tool_call.name.clone(),
                            _ => String::new(),
                        };
                        let argument = self
                            .hooks
                            .freeform_tools
                            .argument_name(&tool_name)
                            .to_string();
                        // `toolcall_delta` carries JSON argument fragments (the session-event
                        // contract), so the raw input goes out escaped inside `{"code":"..."}`.
                        let mut delta = String::new();
                        let mut input = String::new();
                        if let Some(slot) = self.current_slot(output_index) {
                            if is_done {
                                slot.custom_input = text.to_string();
                                delta = slot.close_custom_delta(&argument);
                            } else {
                                slot.custom_input.push_str(text);
                                delta = slot.stream_custom_delta(&argument);
                            }
                            input.clone_from(&slot.custom_input);
                        }
                        if let Some(AssistantContent::ToolCall(tool_call)) =
                            self.output.content.get_mut(content_index)
                        {
                            tool_call.arguments = self
                                .hooks
                                .freeform_tools
                                .arguments_from_input(&tool_call.name, &input);
                        }
                        if !delta.is_empty() {
                            self.writer.push(AssistantMessageEvent::ToolcallDelta {
                                content_index: content_index as u64,
                                delta,
                                partial: self.output.clone(),
                            });
                        }
                    }
                }
            }
            "response.output_item.done" => {
                // Read the finishing block's index before dropping its slot: removing first left
                // every branch below without a block, so no signature or `*_end` was recorded.
                let content_index = self
                    .current_slot(output_index)
                    .map(|slot| slot.content_index);
                let mut slot = output_index.and_then(|index| self.slots.remove(&index));
                let item = event.get("item").cloned().unwrap_or(Value::Null);
                let item_type = item
                    .get("type")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                match item_type {
                    "reasoning" => {
                        if let Some(content_index) = content_index {
                            if self.block_kind(content_index) == Some("thinking") {
                                let summary_text = item
                                    .get("summary")
                                    .and_then(|summary| summary.as_array())
                                    .map(|parts| {
                                        parts
                                            .iter()
                                            .filter_map(|part| {
                                                part.get("text").and_then(|t| t.as_str())
                                            })
                                            .collect::<Vec<_>>()
                                            .join("\n\n")
                                    })
                                    .unwrap_or_default();
                                let content_text = item
                                    .get("content")
                                    .and_then(|content| content.as_array())
                                    .map(|parts| {
                                        parts
                                            .iter()
                                            .filter_map(|part| {
                                                part.get("text").and_then(|t| t.as_str())
                                            })
                                            .collect::<Vec<_>>()
                                            .join("\n\n")
                                    })
                                    .unwrap_or_default();
                                let final_text = if !summary_text.is_empty() {
                                    summary_text
                                } else if !content_text.is_empty() {
                                    content_text
                                } else {
                                    match self.output.content.get(content_index) {
                                        Some(AssistantContent::Thinking(block)) => {
                                            block.thinking.clone()
                                        }
                                        _ => String::new(),
                                    }
                                };
                                if let Some(AssistantContent::Thinking(block)) =
                                    self.output.content.get_mut(content_index)
                                {
                                    block.thinking.clone_from(&final_text);
                                    block.thinking_signature = Some(item.to_string());
                                }
                                self.writer.push(AssistantMessageEvent::ThinkingEnd {
                                    content_index: content_index as u64,
                                    content: final_text,
                                    partial: self.output.clone(),
                                });
                            }
                        }
                    }
                    "message" => {
                        if let Some(content_index) = content_index {
                            if self.block_kind(content_index) == Some("text") {
                                let text = item
                                    .get("content")
                                    .and_then(|content| content.as_array())
                                    .map(|parts| {
                                        parts
                                            .iter()
                                            .map(|part| {
                                                match part
                                                    .get("type")
                                                    .and_then(|value| value.as_str())
                                                {
                                                    Some("output_text") => part
                                                        .get("text")
                                                        .and_then(|value| value.as_str())
                                                        .unwrap_or(""),
                                                    _ => part
                                                        .get("refusal")
                                                        .and_then(|value| value.as_str())
                                                        .unwrap_or(""),
                                                }
                                            })
                                            .collect::<String>()
                                    })
                                    .unwrap_or_default();
                                let signature = encode_text_signature_v1(
                                    item.get("id")
                                        .and_then(|value| value.as_str())
                                        .unwrap_or_default(),
                                    item.get("phase").and_then(|value| value.as_str()).and_then(
                                        |phase| match phase {
                                            "commentary" => Some(TextSignaturePhase::Commentary),
                                            "final_answer" => Some(TextSignaturePhase::FinalAnswer),
                                            _ => None,
                                        },
                                    ),
                                );
                                if let Some(AssistantContent::Text(block)) =
                                    self.output.content.get_mut(content_index)
                                {
                                    block.text.clone_from(&text);
                                    block.text_signature = Some(signature);
                                }
                                self.writer.push(AssistantMessageEvent::TextEnd {
                                    content_index: content_index as u64,
                                    content: text,
                                    partial: self.output.clone(),
                                });
                            }
                        }
                    }
                    "function_call" | "custom_tool_call" => {
                        let is_custom = item_type == "custom_tool_call";
                        if let Some(content_index) = content_index {
                            if self.block_kind(content_index) == Some("toolCall") {
                                let field = if is_custom { "input" } else { "arguments" };
                                let slot_text = match &slot {
                                    Some(slot) if is_custom => slot.custom_input.clone(),
                                    Some(slot) => slot.partial_json.text().to_string(),
                                    None => String::new(),
                                };
                                let item_text = item
                                    .get(field)
                                    .and_then(|value| value.as_str())
                                    .filter(|text| !text.is_empty())
                                    .unwrap_or(&slot_text);
                                let name = item
                                    .get("name")
                                    .and_then(|value| value.as_str())
                                    .unwrap_or("");
                                let arguments = if is_custom {
                                    Value::Object(
                                        self.hooks
                                            .freeform_tools
                                            .arguments_from_input(name, item_text),
                                    )
                                } else if item_text.is_empty() {
                                    json!({})
                                } else {
                                    parse_streaming_json(Some(item_text))
                                };
                                let call_id = item
                                    .get("call_id")
                                    .and_then(|value| value.as_str())
                                    .unwrap_or("");
                                let item_id = item
                                    .get("id")
                                    .and_then(|value| value.as_str())
                                    .unwrap_or("");
                                let tool_call = ToolCall {
                                    id: format!("{call_id}|{item_id}"),
                                    name: item
                                        .get("name")
                                        .and_then(|value| value.as_str())
                                        .unwrap_or("")
                                        .to_string(),
                                    arguments: arguments.as_object().cloned().unwrap_or_default(),
                                    thought_signature: None,
                                    rest: Map::default(),
                                };
                                if let Some(AssistantContent::ToolCall(block)) =
                                    self.output.content.get_mut(content_index)
                                {
                                    *block = tool_call.clone();
                                }
                                // Close a custom call's JSON fragments when no
                                // `custom_tool_call_input.done` did.
                                let closing_delta = match slot.as_mut() {
                                    Some(slot) if is_custom => {
                                        slot.custom_input = item_text.to_string();
                                        slot.close_custom_delta(
                                            self.hooks.freeform_tools.argument_name(name),
                                        )
                                    }
                                    Some(_) | None => String::new(),
                                };
                                if !closing_delta.is_empty() {
                                    self.writer.push(AssistantMessageEvent::ToolcallDelta {
                                        content_index: content_index as u64,
                                        delta: closing_delta,
                                        partial: self.output.clone(),
                                    });
                                }
                                self.writer.push(AssistantMessageEvent::ToolcallEnd {
                                    content_index: content_index as u64,
                                    tool_call,
                                    partial: self.output.clone(),
                                });
                            } else {
                                // No live block: emit a synthesized tool call.
                                let name = item
                                    .get("name")
                                    .and_then(|value| value.as_str())
                                    .unwrap_or("");
                                let arguments = if is_custom {
                                    let input = item
                                        .get("input")
                                        .and_then(|value| value.as_str())
                                        .unwrap_or("");
                                    self.hooks.freeform_tools.arguments_from_input(name, input)
                                } else {
                                    let arguments_text = item
                                        .get("arguments")
                                        .and_then(|value| value.as_str())
                                        .unwrap_or("{}");
                                    parse_streaming_json(Some(arguments_text))
                                        .as_object()
                                        .cloned()
                                        .unwrap_or_default()
                                };
                                let call_id = item
                                    .get("call_id")
                                    .and_then(|value| value.as_str())
                                    .unwrap_or("");
                                let item_id = item
                                    .get("id")
                                    .and_then(|value| value.as_str())
                                    .unwrap_or("");
                                self.output
                                    .content
                                    .push(AssistantContent::ToolCall(ToolCall {
                                        id: format!("{call_id}|{item_id}"),
                                        name: item
                                            .get("name")
                                            .and_then(|value| value.as_str())
                                            .unwrap_or("")
                                            .to_string(),
                                        arguments,
                                        thought_signature: None,
                                        rest: Map::default(),
                                    }));
                                let content_index = self.output.content.len() - 1;
                                let tool_call = match self.output.content.last() {
                                    Some(AssistantContent::ToolCall(tool_call)) => {
                                        tool_call.clone()
                                    }
                                    _ => unreachable!("just pushed a tool call"),
                                };
                                self.writer.push(AssistantMessageEvent::ToolcallEnd {
                                    content_index: content_index as u64,
                                    tool_call,
                                    partial: self.output.clone(),
                                });
                            }
                        }
                    }
                    _ => {}
                }
            }
            "response.completed" | "response.incomplete" => {
                self.saw_terminal_response = true;
                let response = event.get("response").cloned().unwrap_or(Value::Null);
                if self.model.provider == "xai" {
                    if let Some(output_items) =
                        response.get("output").and_then(|value| value.as_array())
                    {
                        for item in output_items {
                            if item.get("type").and_then(|value| value.as_str())
                                != Some("reasoning")
                            {
                                continue;
                            }
                            let encrypted = item.get("encrypted_content");
                            if encrypted.is_none() || encrypted == Some(&Value::Null) {
                                continue;
                            }
                            let item_id = item
                                .get("id")
                                .and_then(|value| value.as_str())
                                .unwrap_or("");
                            for block in &mut self.output.content {
                                let AssistantContent::Thinking(thinking) = block else {
                                    continue;
                                };
                                let Some(signature) = &thinking.thinking_signature else {
                                    continue;
                                };
                                let Ok(mut stored) = serde_json::from_str::<Value>(signature)
                                else {
                                    continue;
                                };
                                if stored.get("id").and_then(|value| value.as_str())
                                    == Some(item_id)
                                    && stored.get("encrypted_content").is_none()
                                {
                                    if let Some(object) = stored.as_object_mut() {
                                        object.insert(
                                            "encrypted_content".into(),
                                            encrypted.cloned().unwrap_or(Value::Null),
                                        );
                                    }
                                    thinking.thinking_signature = Some(stored.to_string());
                                }
                            }
                        }
                    }
                }
                if let Some(id) = response.get("id").and_then(|value| value.as_str()) {
                    self.output.response_id = Some(id.to_string());
                }
                if let Some(usage) = response.get("usage") {
                    let cached_tokens = usage
                        .get("input_tokens_details")
                        .and_then(|details| details.get("cached_tokens"))
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0);
                    let input_tokens = usage
                        .get("input_tokens")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0);
                    // OpenAI includes cached tokens in input_tokens; subtract them.
                    self.output.usage = Usage {
                        input: input_tokens.saturating_sub(cached_tokens),
                        output: usage
                            .get("output_tokens")
                            .and_then(serde_json::Value::as_u64)
                            .unwrap_or(0),
                        cache_read: cached_tokens,
                        cache_write: 0,
                        total_tokens: usage
                            .get("total_tokens")
                            .and_then(serde_json::Value::as_u64)
                            .unwrap_or(0),
                        cost: UsageCost::default(),
                    };
                }
                calculate_cost(self.model, &mut self.output.usage, None);
                if let Some(apply) = &self.hooks.apply_service_tier_pricing {
                    let response_tier = response
                        .get("service_tier")
                        .and_then(|value| value.as_str())
                        .map(std::string::ToString::to_string);
                    let request_tier = self.hooks.request_service_tier.map(|tier| {
                        serde_json::to_value(tier)
                            .unwrap_or_default()
                            .as_str()
                            .unwrap_or_default()
                            .to_string()
                    });
                    let service_tier = match &self.hooks.resolve_service_tier {
                        Some(resolve) => resolve(response_tier, request_tier),
                        None => response_tier.or(request_tier),
                    };
                    apply(&mut self.output.usage, service_tier);
                }
                let status = response.get("status").and_then(|value| value.as_str());
                self.output.stop_reason = map_responses_stop_reason(status);
                let has_tool_call = self
                    .output
                    .content
                    .iter()
                    .any(|block| matches!(block, AssistantContent::ToolCall(_)));
                if has_tool_call && self.output.stop_reason == StopReason::Stop {
                    self.output.stop_reason = StopReason::ToolUse;
                }
                if self.output.stop_reason == StopReason::Error {
                    if let Some(status) = status {
                        self.output.stop_reason_raw = Some(status.to_string());
                    }
                }
            }
            "error" => {
                let code = event.get("code").and_then(|value| value.as_str());
                let message = event
                    .get("message")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                return Err(ProviderError::StreamFailure(StreamFailureError {
                    message: format!("Error Code {}: {}", code.unwrap_or_default(), message),
                    info: StreamFailureInfo {
                        kind: classify_stream_failure(code, None),
                        provider_error_type: code.map(std::string::ToString::to_string),
                        ..StreamFailureInfo::unknown()
                    },
                }));
            }
            "response.failed" => {
                let response = event.get("response").cloned().unwrap_or(Value::Null);
                let error = response.get("error");
                let details = response.get("incomplete_details");
                let provider_error_type = error
                    .and_then(|error| error.get("code"))
                    .and_then(|value| value.as_str())
                    .or_else(|| {
                        details
                            .and_then(|details| details.get("reason"))
                            .and_then(|value| value.as_str())
                    });
                let msg = if let Some(error) = error {
                    format!(
                        "{}: {}",
                        error
                            .get("code")
                            .and_then(|value| value.as_str())
                            .unwrap_or("unknown"),
                        error
                            .get("message")
                            .and_then(|value| value.as_str())
                            .unwrap_or("no message")
                    )
                } else if let Some(reason) = details
                    .and_then(|details| details.get("reason"))
                    .and_then(|value| value.as_str())
                {
                    format!("incomplete: {reason}")
                } else {
                    "Unknown error (no error details in response)".to_string()
                };
                return Err(ProviderError::StreamFailure(StreamFailureError {
                    message: msg,
                    info: StreamFailureInfo {
                        kind: classify_stream_failure(provider_error_type, None),
                        provider_error_type: provider_error_type
                            .map(std::string::ToString::to_string),
                        ..StreamFailureInfo::unknown()
                    },
                }));
            }
            _ => {}
        }

        Ok(())
    }

    /// Terminal stop reason of the message under construction (read through the processor so
    /// error-path checks can run while the borrow of the output message lives here).
    pub fn stop_reason(&self) -> StopReason {
        self.output.stop_reason
    }

    /// Raw stop-reason string of the message under construction.
    pub fn stop_reason_raw(&self) -> Option<&str> {
        self.output.stop_reason_raw.as_deref()
    }

    /// The TS catch settle: finalize tool-call blocks whose parsed preview may lag the accumulated
    /// text under the growth throttle. The outer providers call this on their error paths before
    /// the error event carries the message.
    pub fn settle_partial_tool_calls(&mut self) {
        for slot in self.slots.values_mut() {
            let Some(parsed) = slot.partial_json.flush() else {
                continue;
            };
            if let Some(AssistantContent::ToolCall(tool_call)) =
                self.output.content.get_mut(slot.content_index)
            {
                tool_call.arguments = parsed.as_object().cloned().unwrap_or_default();
            }
        }
    }

    /// Check invariants after the stream ended (port of the trailing checks).
    pub fn finish(&self) -> Result<(), ProviderError> {
        if self.model.provider == "xai" && !self.saw_terminal_response {
            return Err(ProviderError::StreamFailure(StreamFailureError {
                message: "xAI Responses stream ended before a terminal response event".to_string(),
                info: StreamFailureInfo {
                    kind: StreamFailureKind::Unknown,
                    ..StreamFailureInfo::unknown()
                },
            }));
        }
        Ok(())
    }
}

fn map_responses_stop_reason(status: Option<&str>) -> StopReason {
    match status {
        None | Some("completed" | "in_progress" | "queued") => StopReason::Stop,
        Some("incomplete") => StopReason::Length,
        Some(_) => StopReason::Error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_stream::{
        create_assistant_message_event_stream, initial_assistant_message, AssistantMessageEventExt,
    };
    use crate::providers::openai_responses_hooks::FreeformToolArguments;

    fn responses_model() -> Model {
        Model {
            id: "gpt-5.6-luna".into(),
            name: "gpt-5.6-luna".into(),
            api: "openai-responses".into(),
            provider: "opencode-go".into(),
            base_url: "https://opencode.ai/zen/go/v1".into(),
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

    /// The `output_item.done` events finalize their blocks: the reasoning item's JSON (with
    /// `encrypted_content`) becomes the thinking signature, the message id/phase the text
    /// signature, and each block gets its `*_end` event. The event shapes follow a live
    /// opencode-go `gpt-5.6-luna` capture. Regression: the slot was removed before its content
    /// index was read, so every finalize branch was skipped and replay lost the reasoning.
    #[tokio::test]
    async fn output_item_done_finalizes_signatures_for_reasoning_message_and_tool_call() {
        let reasoning_done = json!({
            "id": "rs_1", "type": "reasoning", "encrypted_content": "gAAAA-final",
            "summary": [{"type": "summary_text", "text": "Multiply."}],
        });
        let events = [
            json!({"type": "response.created", "response": {"id": "resp_1"}}),
            json!({"type": "response.output_item.added", "output_index": 0, "item": {
                "id": "rs_1", "type": "reasoning", "encrypted_content": "gAAAA-added", "summary": []}}),
            json!({"type": "response.reasoning_summary_part.added", "output_index": 0,
                "part": {"type": "summary_text", "text": ""}}),
            json!({"type": "response.reasoning_summary_text.delta", "output_index": 0,
                "delta": "Multiply."}),
            json!({"type": "response.reasoning_summary_part.done", "output_index": 0}),
            json!({"type": "response.output_item.done", "output_index": 0,
                "item": reasoning_done.clone()}),
            json!({"type": "response.output_item.added", "output_index": 1, "item": {
                "id": "msg_1", "type": "message", "status": "in_progress", "role": "assistant",
                "phase": "final_answer", "content": []}}),
            json!({"type": "response.content_part.added", "output_index": 1, "content_index": 0,
                "part": {"type": "output_text", "text": ""}}),
            json!({"type": "response.output_text.delta", "output_index": 1, "content_index": 0,
                "delta": "391"}),
            json!({"type": "response.output_item.done", "output_index": 1, "item": {
                "id": "msg_1", "type": "message", "status": "completed", "role": "assistant",
                "phase": "final_answer", "content": [{"type": "output_text", "text": "391"}]}}),
            json!({"type": "response.output_item.added", "output_index": 2, "item": {
                "id": "fc_1", "type": "function_call", "call_id": "call_1", "name": "bash",
                "arguments": ""}}),
            json!({"type": "response.function_call_arguments.delta", "output_index": 2,
                "delta": "{\"cmd\":\"echo hi\"}"}),
            json!({"type": "response.output_item.done", "output_index": 2, "item": {
                "id": "fc_1", "type": "function_call", "call_id": "call_1", "name": "bash",
                "arguments": "{\"cmd\":\"echo hi\"}"}}),
            json!({"type": "response.completed", "response": {"id": "resp_1",
                "status": "completed"}}),
        ];
        let model = responses_model();
        let mut output = initial_assistant_message(&model.api, &model.provider, &model.id);
        let (writer, stream) = create_assistant_message_event_stream();
        {
            let mut processor = ResponsesStreamProcessor::new(
                &model,
                &mut output,
                &writer,
                ResponsesStreamHooks::default(),
            );
            for event in &events {
                processor.handle_event(event).expect("scripted event");
            }
        }
        writer.end(None);
        let end_events: Vec<&str> = stream
            .collect()
            .await
            .iter()
            .map(AssistantMessageEventExt::event_type)
            .filter(|event_type| event_type.ends_with("_end"))
            .collect();

        assert_eq!(
            output.content,
            vec![
                AssistantContent::Thinking(ThinkingContent {
                    thinking: "Multiply.".into(),
                    thinking_signature: Some(reasoning_done.to_string()),
                    redacted: None,
                    rest: Map::default(),
                }),
                AssistantContent::Text(TextContent {
                    text: "391".into(),
                    text_signature: Some(encode_text_signature_v1(
                        "msg_1",
                        Some(TextSignaturePhase::FinalAnswer),
                    )),
                    rest: Map::default(),
                }),
                AssistantContent::ToolCall(ToolCall {
                    id: "call_1|fc_1".into(),
                    name: "bash".into(),
                    arguments: [("cmd".to_string(), json!("echo hi"))]
                        .into_iter()
                        .collect(),
                    thought_signature: None,
                    rest: Map::default(),
                }),
            ]
        );
        assert_eq!(end_events, ["thinking_end", "text_end", "toolcall_end"]);
    }

    /// Run scripted Responses events through a processor whose request declared `python_repl`
    /// freeform (`code`); returns the final message and every emitted event.
    async fn run_freeform_events(
        events: &[Value],
    ) -> (AssistantMessage, Vec<AssistantMessageEvent>) {
        let model = responses_model();
        let tools = [crate::types::Tool {
            name: "python_repl".into(),
            description: "Run Python.".into(),
            parameters: json!({
                "type": "object",
                "properties": { "code": { "type": "string" } },
                "required": ["code"],
            }),
            freeform_argument: Some("code".into()),
        }];
        let mut output = initial_assistant_message(&model.api, &model.provider, &model.id);
        let (writer, stream) = create_assistant_message_event_stream();
        {
            let mut processor = ResponsesStreamProcessor::new(
                &model,
                &mut output,
                &writer,
                ResponsesStreamHooks {
                    freeform_tools: FreeformToolArguments::from_tools(Some(&tools)),
                    ..ResponsesStreamHooks::default()
                },
            );
            for event in events {
                processor.handle_event(event).expect("scripted event");
            }
        }
        writer.end(None);
        (output, stream.collect().await)
    }

    fn custom_item(input: &str) -> Value {
        json!({
            "type": "custom_tool_call", "id": "ctc_1", "call_id": "call_1",
            "name": "python_repl", "input": input,
        })
    }

    /// Regression: `output_item.done` for a `custom_tool_call` finalizes the block. It emits
    /// `toolcall_end`, and the item's final `input` wins over the streamed deltas. The slot was
    /// removed before its content index was read, so the whole tool-call arm was skipped.
    #[tokio::test]
    async fn output_item_done_finalizes_a_custom_tool_call_with_the_items_input() {
        let events = [
            json!({"type": "response.created", "response": {"id": "resp_1"}}),
            json!({"type": "response.output_item.added", "output_index": 0,
                "item": custom_item("")}),
            json!({"type": "response.custom_tool_call_input.delta", "output_index": 0,
                "item_id": "ctc_1", "delta": "print("}),
            json!({"type": "response.custom_tool_call_input.delta", "output_index": 0,
                "item_id": "ctc_1", "delta": "1)"}),
            json!({"type": "response.output_item.done", "output_index": 0,
                "item": custom_item("print(2)")}),
            json!({"type": "response.completed", "response": {"id": "resp_1",
                "status": "completed"}}),
        ];
        let (output, emitted) = run_freeform_events(&events).await;
        let expected_call = ToolCall {
            id: "call_1|ctc_1".into(),
            name: "python_repl".into(),
            arguments: [("code".to_string(), json!("print(2)"))]
                .into_iter()
                .collect(),
            thought_signature: None,
            rest: Map::default(),
        };
        let event_types: Vec<&str> = emitted
            .iter()
            .map(AssistantMessageEventExt::event_type)
            .collect();
        let end_call = emitted.iter().find_map(|event| match event {
            AssistantMessageEvent::ToolcallEnd { tool_call, .. } => Some(tool_call.clone()),
            _ => None,
        });

        assert_eq!(
            output.content,
            vec![AssistantContent::ToolCall(expected_call.clone())]
        );
        assert_eq!(end_call, Some(expected_call));
        assert_eq!(
            event_types,
            [
                "toolcall_start",
                "toolcall_delta",
                "toolcall_delta",
                "toolcall_delta",
                "toolcall_end"
            ]
        );
    }

    /// Regression: a custom call's `toolcall_delta` events carry JSON argument fragments, like a
    /// `function_call`'s (the session-event contract), not the raw input. Joined, they parse as
    /// the same `{"code": ...}` arguments object, escapes included, whether the input's `.done`
    /// or only the item's `.done` closes them.
    #[tokio::test]
    async fn custom_tool_call_deltas_join_into_the_arguments_json() {
        let code = "x = \"a\\b\"\nprint(x, 'é')";
        let input_done = json!({"type": "response.custom_tool_call_input.done",
            "output_index": 0, "item_id": "ctc_1", "input": code});
        for closes_on_input_done in [true, false] {
            let mut events = vec![
                json!({"type": "response.created", "response": {"id": "resp_1"}}),
                json!({"type": "response.output_item.added", "output_index": 0,
                    "item": custom_item("")}),
                json!({"type": "response.custom_tool_call_input.delta", "output_index": 0,
                    "item_id": "ctc_1", "delta": &code[..9]}),
                json!({"type": "response.custom_tool_call_input.delta", "output_index": 0,
                    "item_id": "ctc_1", "delta": &code[9..]}),
            ];
            if closes_on_input_done {
                events.push(input_done.clone());
            }
            events.extend([
                json!({"type": "response.output_item.done", "output_index": 0,
                    "item": custom_item(code)}),
                json!({"type": "response.completed", "response": {"id": "resp_1",
                    "status": "completed"}}),
            ]);
            let (_, emitted) = run_freeform_events(&events).await;
            let deltas: Vec<&str> = emitted
                .iter()
                .filter_map(|event| match event {
                    AssistantMessageEvent::ToolcallDelta { delta, .. } => Some(delta.as_str()),
                    _ => None,
                })
                .collect();
            let joined: Value =
                serde_json::from_str(&deltas.concat()).expect("joined deltas parse");

            assert_eq!(
                (joined, deltas.len()),
                (json!({ "code": code }), 3),
                "{deltas:?}"
            );
        }
    }
}
