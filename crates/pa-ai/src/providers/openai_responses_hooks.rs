//! `OpenAI` Responses stream hooks, reasoning-summary options, and service-tier pricing.

use std::collections::HashMap;

use serde_json::{Map, Value};

use crate::types::{Tool, ToolCall, Usage};

/// Reasoning summary mode requested from the Responses API.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)] // full TS option surface; variants set by callers
pub enum ReasoningSummary {
    Auto,
    Detailed,
    Concise,
}

impl ReasoningSummary {
    pub fn as_str(self) -> &'static str {
        match self {
            ReasoningSummary::Auto => "auto",
            ReasoningSummary::Detailed => "detailed",
            ReasoningSummary::Concise => "concise",
        }
    }
}

/// Hook resolving the effective service tier from response and request tiers.
pub type ResolveServiceTierFn =
    Box<dyn Fn(Option<String>, Option<String>) -> Option<String> + Send>;
/// Hook applying service-tier pricing to a usage block.
pub type ApplyServiceTierPricingFn = Box<dyn Fn(&mut Usage, Option<String>) + Send>;

/// The request's freeform (Responses `custom`) tools: tool name -> the argument its raw text
/// input maps onto (`python_repl` -> `code`).
#[derive(Default)]
pub(crate) struct FreeformToolArguments(HashMap<String, String>);

impl FreeformToolArguments {
    pub fn from_tools(tools: Option<&[Tool]>) -> Self {
        Self(
            tools
                .unwrap_or_default()
                .iter()
                .filter_map(|tool| {
                    tool.freeform_argument
                        .as_ref()
                        .map(|argument| (tool.name.clone(), argument.clone()))
                })
                .collect(),
        )
    }

    /// The argument object a `custom_tool_call` input becomes: `{<argument>: input}`. An
    /// undeclared tool keys it under `input`, so the tool layer still reports the bad call.
    pub fn arguments_from_input(&self, tool_name: &str, input: &str) -> Map<String, Value> {
        Map::from_iter([(
            self.argument_name(tool_name).to_string(),
            Value::String(input.to_string()),
        )])
    }

    pub fn argument_name(&self, tool_name: &str) -> &str {
        // Exact name only (no aliases): the API constrains custom call names to declared tools.
        self.0.get(tool_name).map_or("input", String::as_str)
    }

    /// The raw input a recorded call replays as a `custom_tool_call`: only when the tool is
    /// freeform in this request and the arguments are exactly its one string argument.
    pub fn custom_input<'a>(&self, tool_call: &'a ToolCall) -> Option<&'a str> {
        let argument = self.0.get(&tool_call.name)?;
        if tool_call.arguments.len() != 1 {
            return None;
        }
        tool_call.arguments.get(argument).and_then(Value::as_str)
    }
}

/// Service-tier hooks mirroring the TS `OpenAIResponsesStreamOptions`, plus the request's
/// freeform tools.
#[derive(Default)]
pub struct ResponsesStreamHooks {
    pub request_service_tier: Option<crate::types::ServiceTier>,
    pub resolve_service_tier: Option<ResolveServiceTierFn>,
    pub apply_service_tier_pricing: Option<ApplyServiceTierPricingFn>,
    pub freeform_tools: FreeformToolArguments,
}

/// Multipliers per <https://developers.openai.com/api/docs/pricing>.
pub fn get_service_tier_cost_multiplier(model_id: &str, service_tier: Option<&str>) -> f64 {
    match service_tier {
        Some("flex") => 0.5,
        Some("priority") => {
            if model_id == "gpt-5.5" {
                2.5
            } else {
                2.0
            }
        }
        _ => 1.0,
    }
}

/// Apply service-tier pricing multipliers to a usage block.
pub fn apply_service_tier_pricing(usage: &mut Usage, service_tier: Option<&str>, model_id: &str) {
    let multiplier = get_service_tier_cost_multiplier(model_id, service_tier);
    // The multiplier table is discrete; equality with the 1.0 sentinel is the no-op contract.
    #[allow(clippy::float_cmp)]
    if multiplier == 1.0 {
        return;
    }
    usage.cost.input = (usage.cost.input.as_f64() * multiplier).into();
    usage.cost.output = (usage.cost.output.as_f64() * multiplier).into();
    usage.cost.cache_read = (usage.cost.cache_read.as_f64() * multiplier).into();
    usage.cost.cache_write = (usage.cost.cache_write.as_f64() * multiplier).into();
    usage.cost.total = (usage.cost.input.as_f64()
        + usage.cost.output.as_f64()
        + usage.cost.cache_read.as_f64()
        + usage.cost.cache_write.as_f64())
    .into();
}
