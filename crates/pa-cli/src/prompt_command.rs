//! `prime-agent prompt`: dump the fully-assembled effective system prompt with
//! its per-layer breakdown, without starting a session or a provider.

use pa_core::resources::{load_resources, ResourceLoaderOptions};
use pa_core::settings::SettingsManager;

use crate::config::get_agent_dir;

/// The command's parsed arguments.
#[derive(Debug, Default, PartialEq, Eq)]
struct PromptCommandArgs {
    model: Option<String>,
    cwd: Option<String>,
    json: bool,
}

fn parse_args(args: &[String]) -> Result<PromptCommandArgs, String> {
    let mut parsed = PromptCommandArgs::default();
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--json" => parsed.json = true,
            "--model" => {
                let Some(value) = rest.next() else {
                    return Err("--model requires a value".to_string());
                };
                parsed.model = Some(value.clone());
            }
            "--cwd" => {
                let Some(value) = rest.next() else {
                    return Err("--cwd requires a value".to_string());
                };
                parsed.cwd = Some(value.clone());
            }
            "--help" | "-h" => {
                return Err(String::new());
            }
            other => return Err(format!("unknown option {other:?}")),
        }
    }
    Ok(parsed)
}

/// Run the `prompt` command; returns the process exit code.
pub fn run_prompt_command(args: &[String]) -> i32 {
    let parsed = match parse_args(args) {
        Ok(parsed) => parsed,
        Err(help_request) if help_request.is_empty() => return 0,
        Err(error) => {
            eprintln!("Error: {error}");
            eprintln!("Run `prime-agent help prompt` for usage.");
            return 1;
        }
    };
    let cwd = parsed
        .cwd
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    match assemble_breakdown(&cwd, &get_agent_dir(), parsed.model.as_deref()) {
        Ok((breakdown, errors)) => {
            for error in &errors {
                eprintln!("Error: {error}");
            }
            if parsed.json {
                print_json(&breakdown);
            } else {
                print_text(&breakdown);
            }
            0
        }
        Err(error) => {
            eprintln!("Error: {error:#}");
            1
        }
    }
}

fn assemble_breakdown(
    cwd: &std::path::Path,
    agent_dir: &std::path::Path,
    model: Option<&str>,
) -> anyhow::Result<(pa_core::prompts::SystemPromptBreakdown, Vec<String>)> {
    let model_prompts = pa_core::prompts::model_prompts::load_model_prompts(model, agent_dir);
    let settings = SettingsManager::create(cwd, agent_dir);
    // MCP gating: auth-gated built-in integrations drop their skills; enabled persistent generic
    // servers add the prompt MCP guidance.
    let user_servers = settings
        .settings()
        .mcp_servers
        .clone()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(server, config)| {
            serde_json::from_value(config)
                .ok()
                .map(|parsed| (server, parsed))
        })
        .collect::<std::collections::HashMap<String, pa_core::mcp::McpServerConfig>>();
    let (skill_overrides, generic_servers, _manager) =
        pa_core::mcp::McpManager::prompt_gating(user_servers, agent_dir);
    let edit_format = pa_core::models::edit_format_for_model_reference(&settings, agent_dir, model);
    let resources = load_resources(ResourceLoaderOptions {
        extra_builtin_skill_overrides: skill_overrides,
        system_prompt: None,
        ..ResourceLoaderOptions::new(cwd.to_path_buf(), agent_dir.to_path_buf())
    })?;
    let breakdown = pa_core::prompts::system_prompt::system_prompt_breakdown(
        &pa_core::prompts::BuildSystemPromptOptions {
            cwd: cwd.display().to_string(),
            // A dump has no session file; the tail states the value it uses.
            messages_path: None,
            custom_prompt: resources.system_prompt.clone(),
            context_files: resources
                .agents_files
                .iter()
                .map(|file| (file.path.display().to_string(), file.content.clone()))
                .collect(),
            skills: pa_core::session_engine::runtime_wiring::skills_for_edit_format(
                resources.skills.clone(),
                edit_format,
            ),
            edit_format,
            // The shipped model-tool surface: `python_repl` only; bash/edit are kernel-resident
            // programmatic tools.
            selected_tools: Some(vec![pa_types::ai::PYTHON_TOOL_NAME]),
            allow_recursion: Some(true),
            generic_mcp_servers: generic_servers,
            rlm_depth: Some(0),
            model_prompt_extras: model_prompts.extras.as_deref(),
            ..Default::default()
        },
    );
    Ok((breakdown, model_prompts.errors))
}

fn print_text(breakdown: &pa_core::prompts::SystemPromptBreakdown) {
    println!("# prime-agent system prompt breakdown");
    println!();
    println!("cached prefix (static layers, byte-stable across sessions):");
    for segment in &breakdown.segments {
        if segment.kind == pa_core::prompts::SegmentKind::Static {
            print_segment(segment);
        }
    }
    println!("dynamic tail (session-specific, appended after the cached prefix):");
    for segment in &breakdown.segments {
        if segment.kind == pa_core::prompts::SegmentKind::Dynamic {
            print_segment(segment);
        }
    }
    println!();
    println!(
        "cached prefix: {} of {} total chars; dynamic tail follows",
        breakdown.cached_prefix_len,
        breakdown.assembled.len()
    );
    println!();
    println!("{}", breakdown.assembled);
}

fn print_segment(segment: &pa_core::prompts::PromptSegment) {
    println!(
        "  {:<20} {:<38} {:>7} chars",
        segment.name,
        segment.source,
        segment.text.chars().count()
    );
}

fn print_json(breakdown: &pa_core::prompts::SystemPromptBreakdown) {
    let segments = breakdown
        .segments
        .iter()
        .map(|segment| {
            serde_json::json!({
                "name": segment.name,
                "kind": if segment.kind == pa_core::prompts::SegmentKind::Static { "static" } else { "dynamic" },
                "source": segment.source,
                "chars": segment.text.chars().count(),
            })
        })
        .collect::<Vec<_>>();
    println!(
        "{}",
        serde_json::json!({
            "segments": segments,
            "cachedPrefixLen": breakdown.cached_prefix_len,
            "prompt": breakdown.assembled,
        })
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_flags() {
        let parsed = parse_args(&[
            "--model".to_string(),
            "mock/mock-1".to_string(),
            "--json".to_string(),
            "--cwd".to_string(),
            "/w".to_string(),
        ])
        .unwrap();
        assert_eq!(parsed.model.as_deref(), Some("mock/mock-1"));
        assert_eq!(parsed.cwd.as_deref(), Some("/w"));
        assert!(parsed.json);
        assert_eq!(
            parse_args(&["--bogus".to_string()]),
            Err("unknown option \"--bogus\"".to_string())
        );
        assert_eq!(
            parse_args(&["--model".to_string()]),
            Err("--model requires a value".to_string())
        );
    }

    #[test]
    fn help_request_prints_usage() {
        assert_eq!(run_prompt_command(&["--help".to_string()]), 0);
    }

    /// GPT-5.1 gets the `apply_patch` guidance and skill; `gpt-5-codex`, a
    /// non-OpenAI model, and GPT-5.1 under `"applyPatch": "off"` get today's
    /// prompt byte for byte; a models.json `editFormat` flips the default.
    #[test]
    fn the_edit_guidance_follows_the_resolved_edit_format() {
        let dir = tempfile::tempdir().unwrap();
        let agent_dir = dir.path().join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        let prompt = |model: &str| {
            let (breakdown, _errors) = assemble_breakdown(dir.path(), &agent_dir, Some(model))
                .expect("the prompt assembles");
            breakdown
        };
        let today = prompt("anthropic/claude-fable-5");
        assert!(today.segments[0].text.contains("- `edit.run("));
        let gpt_5_1 = prompt("openai-codex/gpt-5.1");
        assert!(gpt_5_1.segments[0]
            .text
            .contains("- `apply_patch(patch: str)"));
        assert_eq!(
            gpt_5_1.segments[0].source,
            "prompts/layers/core.md + apply-patch-core.md"
        );
        // The skills inventory follows the same decision as the guidance.
        assert!(gpt_5_1.assembled[gpt_5_1.cached_prefix_len..].contains("<name>apply-patch</name>"));
        assert!(!today.assembled.contains("<name>apply-patch</name>"));
        // (`openai-codex/gpt-5` would fuzzy-resolve to the cataloged gpt-5.1,
        // exactly as `--model` does for a session.)
        assert_eq!(
            prompt("openai-codex/gpt-5-codex").assembled[..today.cached_prefix_len],
            today.assembled[..today.cached_prefix_len]
        );

        std::fs::write(
            agent_dir.join("models.json"),
            r#"{ "providers": {
                "openai-codex": { "modelOverrides": { "gpt-5.1": { "editFormat": "edit" } } },
                "anthropic": { "modelOverrides": { "claude-fable-5": { "editFormat": "apply_patch" } } }
            } }"#,
        )
        .unwrap();
        assert_eq!(
            prompt("openai-codex/gpt-5.1").assembled[..today.cached_prefix_len],
            today.assembled[..today.cached_prefix_len]
        );
        assert_eq!(
            prompt("anthropic/claude-fable-5").segments[0].text,
            gpt_5_1.segments[0].text
        );

        std::fs::remove_file(agent_dir.join("models.json")).unwrap();
        std::fs::write(
            agent_dir.join("settings.json"),
            r#"{ "applyPatch": "off" }"#,
        )
        .unwrap();
        let off = prompt("openai-codex/gpt-5.1");
        assert_eq!(off.cached_prefix_len, today.cached_prefix_len);
        assert_eq!(
            off.assembled[..off.cached_prefix_len],
            today.assembled[..today.cached_prefix_len]
        );
    }

    /// Parity evidence for the sanctioned divergence: a non-GPT model, and a
    /// GPT-5.1 model under `"applyPatch": "off"`, get main's whole assembled
    /// prompt byte for byte. Main's prompt is the same builder over the
    /// unmodified layer files (the `edit` variant, `EditFormat::default()`) and
    /// the bundled skills without `apply-patch`, which main does not ship.
    #[test]
    fn the_edit_variant_is_mains_whole_prompt_byte_for_byte() {
        let dir = tempfile::tempdir().unwrap();
        let agent_dir = dir.path().join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        let mains = |model: &str| {
            let model_prompts =
                pa_core::prompts::model_prompts::load_model_prompts(Some(model), &agent_dir);
            let (skill_overrides, generic_servers, _manager) =
                pa_core::mcp::McpManager::prompt_gating(
                    std::collections::HashMap::new(),
                    &agent_dir,
                );
            let resources = load_resources(ResourceLoaderOptions {
                extra_builtin_skill_overrides: skill_overrides,
                system_prompt: None,
                ..ResourceLoaderOptions::new(dir.path().to_path_buf(), agent_dir.clone())
            })
            .unwrap();
            pa_core::prompts::system_prompt::system_prompt_breakdown(
                &pa_core::prompts::BuildSystemPromptOptions {
                    cwd: dir.path().display().to_string(),
                    messages_path: None,
                    custom_prompt: resources.system_prompt.clone(),
                    context_files: resources
                        .agents_files
                        .iter()
                        .map(|file| (file.path.display().to_string(), file.content.clone()))
                        .collect(),
                    skills: resources
                        .skills
                        .into_iter()
                        .filter(|skill| skill.name != "apply-patch")
                        .collect(),
                    selected_tools: Some(vec![pa_types::ai::PYTHON_TOOL_NAME]),
                    allow_recursion: Some(true),
                    generic_mcp_servers: generic_servers,
                    rlm_depth: Some(0),
                    model_prompt_extras: model_prompts.extras.as_deref(),
                    ..Default::default()
                },
            )
            .assembled
        };
        let assembled = |model: &str| {
            assemble_breakdown(dir.path(), &agent_dir, Some(model))
                .expect("the prompt assembles")
                .0
                .assembled
        };
        let non_gpt = assembled("anthropic/claude-fable-5");
        assert!(!non_gpt.contains("apply_patch") && !non_gpt.contains("apply-patch"));
        assert_eq!(non_gpt, mains("anthropic/claude-fable-5"));
        assert_ne!(
            assembled("openai-codex/gpt-5.1"),
            mains("openai-codex/gpt-5.1")
        );
        std::fs::write(
            agent_dir.join("settings.json"),
            r#"{ "applyPatch": "off" }"#,
        )
        .unwrap();
        assert_eq!(
            assembled("openai-codex/gpt-5.1"),
            mains("openai-codex/gpt-5.1")
        );
    }

    /// Without `--model`, the guidance follows the model a new session would
    /// start on (here the saved settings default), not a fixed `edit`.
    #[test]
    fn without_a_model_the_edit_guidance_follows_the_default_model() {
        let dir = tempfile::tempdir().unwrap();
        let agent_dir = dir.path().join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(
            agent_dir.join("models.json"),
            r#"{ "providers": { "my-proxy": {
                "baseUrl": "http://127.0.0.1:9",
                "apiKey": "sk-local",
                "api": "openai-completions",
                "models": [ { "id": "gpt-5.1" } ]
            } } }"#,
        )
        .unwrap();
        std::fs::write(
            agent_dir.join("settings.json"),
            r#"{ "defaultProvider": "my-proxy", "defaultModel": "gpt-5.1" }"#,
        )
        .unwrap();
        let (breakdown, _errors) = assemble_breakdown(dir.path(), &agent_dir, /*model*/ None)
            .expect("the prompt assembles");
        assert_eq!(
            breakdown.segments[0].source,
            "prompts/layers/core.md + apply-patch-core.md"
        );
    }

    #[test]
    fn assembles_the_layered_prompt_for_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        let (breakdown, errors) =
            assemble_breakdown(dir.path(), &dir.path().join("agent"), Some("mock/mock-1")).unwrap();
        assert!(errors.is_empty());
        assert!(breakdown.assembled.starts_with("# prime-agent harness"));
        assert!(breakdown
            .assembled
            .contains("Recursive agent depth: 0 (root)"));
        assert!(breakdown
            .assembled
            .contains("Conversation log: not persisted"));
        // Every static segment sits inside the cached prefix.
        for segment in &breakdown.segments {
            let inside = breakdown
                .assembled
                .find(&segment.text)
                .is_some_and(|at| at < breakdown.cached_prefix_len);
            assert_eq!(
                inside,
                segment.kind == pa_core::prompts::SegmentKind::Static,
                "segment {} on the wrong side of the cache boundary",
                segment.name
            );
        }
    }
}
