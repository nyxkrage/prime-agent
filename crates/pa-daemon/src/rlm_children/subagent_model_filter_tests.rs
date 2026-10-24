use super::*;
use crate::protocol::response_failure;
use pa_core::models::{SubagentModelRefusal, SubagentModelRefusalCause};
use pa_types::platform::transport::bind_transport;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

/// A fake supervisor that records every command type it receives and
/// refuses it: a spawn that slips past the `subagentModels` filter reaches
/// it (and fails with a different error) instead of hanging.
async fn spawn_recording_supervisor(
    socket: std::path::PathBuf,
    seen_tx: mpsc::UnboundedSender<String>,
) {
    let listener = bind_transport(&socket).await.unwrap();
    tokio::spawn(async move {
        loop {
            let Ok(stream) = listener.accept().await else {
                return;
            };
            let seen_tx = seen_tx.clone();
            tokio::spawn(async move {
                let (reader, mut writer) = stream.split();
                let mut reader = BufReader::new(reader);
                writer
                    .write_all(
                        b"{\"type\":\"daemon_hello\",\"protocol\":{\"name\":\"prime-agent.daemon\",\"version\":7}}\n",
                    )
                    .await
                    .unwrap();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).await.unwrap() == 0 {
                        return;
                    }
                    let value: Value = serde_json::from_str(line.trim()).unwrap();
                    let id = value["id"].as_str().unwrap_or_default().to_string();
                    let command_type = value["command"]["type"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string();
                    let _ = seen_tx.send(command_type.clone());
                    let response = response_failure(
                        Some(&id),
                        &command_type,
                        "the recording supervisor refuses every command",
                        None,
                    );
                    let mut line = serde_json::to_string(&response).unwrap();
                    line.push('\n');
                    if writer.write_all(line.as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
}

/// An agent dir with two authenticated models and the given global
/// settings document.
fn agent_dir_with_settings(settings: &str) -> tempfile::TempDir {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("models.json"),
        json!({
            "providers": {
                "test-provider": {
                    "baseUrl": "http://localhost:9",
                    "apiKey": "test-key",
                    "api": "openai-completions",
                    "models": [
                        { "id": "glm-5.3", "contextWindow": 1000, "maxTokens": 100 },
                        { "id": "glm-5.3-turbo", "alias": "turbo", "contextWindow": 1000, "maxTokens": 100 }
                    ]
                }
            }
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(dir.path().join("settings.json"), settings).unwrap();
    dir
}

/// The children registry for a depth-0 parent on `test-provider/glm-5.3`,
/// against the recording supervisor.
async fn sessions_for(
    agent_dir: &Path,
    seen_tx: mpsc::UnboundedSender<String>,
) -> SupervisorChildSessions {
    let socket = std::env::temp_dir().join(format!(
        "pa-rlm-filter-{}.sock",
        uuid::Uuid::new_v4().simple()
    ));
    spawn_recording_supervisor(socket.clone(), seen_tx).await;
    let link = Arc::new(crate::supervisor_link::SupervisorLink::new(socket));
    let sessions = SupervisorChildSessions::new(
        link,
        agent_dir.to_path_buf(),
        "parent-live".to_string(),
        Arc::new(crate::model_allowlist::ModelRefusalTelemetry::new(
            agent_dir.to_path_buf(),
            /*telemetry_disabled*/ true,
        )),
        String::new(),
    );
    sessions.set_identity(ParentIdentity {
        model: Some("test-provider/glm-5.3".to_string()),
        cwd: Some(agent_dir.to_string_lossy().to_string()),
        ..ParentIdentity::with_default_depth()
    });
    sessions
}

fn spawn_request(model: Option<&str>) -> RlmSpawnRequest {
    RlmSpawnRequest {
        prompt: "go".to_string(),
        name: None,
        model: model.map(str::to_string),
        thinking: None,
        spawned_by_request_id: None,
    }
}

fn create_session_request(model: Option<&str>) -> RlmCreateSessionRequest {
    RlmCreateSessionRequest {
        prompt: "go".to_string(),
        name: None,
        model: model.map(str::to_string),
        thinking: None,
        cwd: None,
    }
}

/// Drive `spawn` and `create_session` for the explicit and the inherited
/// (`model=None`) form; each must be refused with exactly `expected` before
/// any command reaches the supervisor.
async fn assert_every_child_refused(settings: &str, expected: &SubagentModelRefusal) {
    let dir = agent_dir_with_settings(settings);
    let (seen_tx, mut seen_rx) = mpsc::unbounded_channel();
    let sessions = sessions_for(dir.path(), seen_tx).await;
    for model in [Some(expected.selector.as_str()), None] {
        let spawned = sessions
            .spawn(spawn_request(model))
            .await
            .expect_err("the spawn must be refused");
        assert_eq!(
            spawned.downcast_ref::<SubagentModelRefusal>(),
            Some(expected),
            "spawn(model={model:?}): {spawned:#}"
        );
        let created = sessions
            .create_session(create_session_request(model))
            .await
            .expect_err("create_session must be refused");
        assert_eq!(
            created.downcast_ref::<SubagentModelRefusal>(),
            Some(expected),
            "create_session(model={model:?}): {created:#}"
        );
    }
    assert!(
        seen_rx.try_recv().is_err(),
        "a refused child must never reach the supervisor"
    );
}

/// The supervisor host enforces `subagentModels` on a real spawn and
/// `create_session`: an authenticated model outside the filter is refused,
/// named explicitly or inherited from the parent.
#[tokio::test]
async fn the_supervisor_host_refuses_children_outside_the_subagent_filter() {
    assert_every_child_refused(
        r#"{ "subagentModels": ["test-provider/glm-5.3-turbo"] }"#,
        &SubagentModelRefusal {
            selector: "test-provider/glm-5.3".to_string(),
            cause: SubagentModelRefusalCause::OutsideFilter {
                permitted: vec!["turbo (test-provider/glm-5.3-turbo)".to_string()],
                alias_patterns: Vec::new(),
            },
        },
    )
    .await;
}

/// A malformed `subagentModels` (a string, not a list) fails closed on the
/// supervisor host: every child is refused, naming the problem.
#[tokio::test]
async fn the_supervisor_host_fails_closed_on_a_malformed_subagent_filter() {
    assert_every_child_refused(
        r#"{ "subagentModels": "test-provider/glm-5.3" }"#,
        &SubagentModelRefusal {
            selector: "test-provider/glm-5.3".to_string(),
            cause: SubagentModelRefusalCause::UnreadableFilter(
                "subagentModels is present but is not an array of strings".to_string(),
            ),
        },
    )
    .await;
}
