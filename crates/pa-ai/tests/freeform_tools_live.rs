//! Opt-in live check of freeform (Responses `custom`) tools against `opencode-go/gpt-5.6-luna`:
//! the model answers a raw-text tool with a `custom_tool_call`, and the follow-up request that
//! replays it (`ctc_` id, `custom_tool_call_output`) is accepted. Skipped unless
//! `OPENCODE_API_KEY` is set; never runs in the merge gates.

use std::collections::HashMap;

use pa_ai::types::{
    AssistantContent, Context, Message, Model, StopReason, StreamOptions, TextContent, Tool,
    ToolResultMessage, UserMessage, UserMessageContent, UserOrToolContent,
};
use serde_json::{json, Map};

fn luna() -> Model {
    serde_json::from_value(json!({
        "id": "gpt-5.6-luna", "name": "GPT-5.6 Luna", "api": "openai-responses",
        "provider": "opencode-go", "baseUrl": "https://opencode.ai/zen/go/v1",
        "reasoning": true, "input": ["text"], "contextWindow": 400_000, "maxTokens": 4096,
        "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
    }))
    .expect("model")
}

fn options() -> StreamOptions {
    StreamOptions {
        // opencode-go rejects session-less requests (MissingSessionID); a fixed id keeps this
        // check independent of how the provider path forwards session ids.
        headers: Some(HashMap::from([(
            "x-opencode-session".to_string(),
            "pa-freeform-live".to_string(),
        )])),
        ..StreamOptions::default()
    }
}

#[tokio::test]
async fn luna_answers_and_replays_a_custom_tool_call() {
    if std::env::var_os("OPENCODE_API_KEY").is_none() {
        eprintln!("skipped: OPENCODE_API_KEY is not set");
        return;
    }
    let model = luna();
    let mut context = Context {
        system_prompt: Some("You are a helpful assistant.".into()),
        messages: vec![Message::User(UserMessage {
            content: UserMessageContent::Text(
                "Use python_repl to compute 17*23 and print it. Then reply with just the number."
                    .into(),
            ),
            timestamp: 0,
            rest: Map::default(),
        })],
        tools: Some(vec![Tool {
            name: "python_repl".into(),
            description: "Run Python code in a persistent REPL. Send raw Python source.".into(),
            parameters: json!({
                "type": "object",
                "properties": { "code": { "type": "string" } },
                "required": ["code"],
            }),
            freeform_argument: Some("code".into()),
        }]),
    };
    let first = pa_ai::stream(&model, &context, Some(options()))
        .expect("provider")
        .result()
        .await;
    assert_eq!(first.stop_reason, StopReason::ToolUse, "{first:?}");
    let Some(AssistantContent::ToolCall(call)) = first
        .content
        .iter()
        .find(|block| matches!(block, AssistantContent::ToolCall(_)))
        .cloned()
    else {
        panic!("no tool call in {first:?}");
    };
    assert!(
        call.id.contains("|ctc_"),
        "not a custom tool call: {call:?}"
    );
    assert!(call.arguments["code"].is_string(), "{call:?}");

    context.messages.push(Message::Assistant(first));
    context
        .messages
        .push(Message::ToolResult(ToolResultMessage {
            tool_call_id: call.id.clone(),
            tool_name: call.name.clone(),
            content: vec![UserOrToolContent::Text(TextContent {
                text: "391\n".into(),
                text_signature: None,
                rest: Map::default(),
            })],
            details: None,
            is_error: false,
            timestamp: 0,
            rest: Map::default(),
        }));
    let second = pa_ai::stream(&model, &context, Some(options()))
        .expect("provider")
        .result()
        .await;
    assert_eq!(second.stop_reason, StopReason::Stop, "{second:?}");
}
