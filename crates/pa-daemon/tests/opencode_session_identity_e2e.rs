//! Port of the TS regression `6009-opencode-maintenance-session.test.ts`: every model
//! request a session makes to the `opencode`/`opencode-go` providers routes under the
//! owning session's durable id (`x-opencode-session` = the session file's id). That
//! covers the main turns and every maintenance call: manual, split-turn and automatic
//! compaction, the compact-trigger auto-refine review, `/refine` planning, tree
//! branch summaries and side questions. Resume, reload and model switches keep the
//! id; a replacement session (`/new`) and another session never reuse it.
//!
//! The mock answers like opencode-go: a request without the header is refused with
//! the provider's `MissingSessionID` 400, so a call that drops the header fails the
//! operation as well as the identity assertion.
#![allow(clippy::large_futures)]
// Fn length is a style gate, not correctness.
#![allow(clippy::too_many_lines)]
#![cfg(unix)]

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const SESSION_HEADER: &str = "x-opencode-session";
const MISSING_SESSION: &str =
    "Error from provider (Console Go): Request is missing x-opencode-session and cannot be routed efficiently.";
const PROVIDERS: [&str; 2] = ["opencode", "opencode-go"];
const PROPOSAL: &str =
    r#"{"summary":"No change","rationale":"Fixture","expectedOutcome":"No change","edits":[]}"#;
const REVIEW_DECLINE: &str = r#"{"shouldRefine":false,"rationale":"No lesson"}"#;

/// One request the mock received: the operation the test was driving, the
/// lowercased request headers, and the JSON body (for failure messages).
#[derive(Debug, Clone)]
struct Recorded {
    operation: String,
    headers: BTreeMap<String, String>,
    body: Value,
}

#[derive(Default)]
struct MockState {
    operation: String,
    reply: String,
    /// Report a usage that crosses the auto-compaction threshold on the next reply.
    crossing_usage_next: bool,
    requests: Vec<Recorded>,
}

/// An opencode-shaped chat-completions endpoint that records every request.
struct OpencodeMock {
    state: Arc<Mutex<MockState>>,
    port: u16,
}

impl OpencodeMock {
    fn start() -> OpencodeMock {
        let state = Arc::new(Mutex::new(MockState {
            operation: "ordinary".to_string(),
            reply: "OK".to_string(),
            ..MockState::default()
        }));
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock");
        let port = listener.local_addr().expect("mock addr").port();
        let thread_state = Arc::clone(&state);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let state = Arc::clone(&thread_state);
                std::thread::spawn(move || {
                    let _ = serve(stream, &state);
                });
            }
        });
        OpencodeMock { state, port }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.port)
    }

    fn set_operation(&self, operation: &str, reply: &str) {
        let mut state = self.state.lock().expect("mock lock");
        state.operation = operation.to_string();
        state.reply = reply.to_string();
    }

    fn cross_threshold_next(&self) {
        self.state.lock().expect("mock lock").crossing_usage_next = true;
    }

    fn requests(&self) -> Vec<Recorded> {
        self.state.lock().expect("mock lock").requests.clone()
    }

    fn count(&self, operation: &str) -> usize {
        self.requests()
            .iter()
            .filter(|request| request.operation == operation)
            .count()
    }

    /// Wait (bounded) until `operation` has at least `count` requests: some
    /// maintenance calls (the side question, the post-compaction review) settle
    /// after their command's response.
    fn wait_for(&self, operation: &str, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while self.count(operation) < count {
            assert!(
                Instant::now() < deadline,
                "no {operation} request arrived; requests: {:#?}",
                self.requests()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Some request under `operation` sent `marker` in its body.
    fn assert_sent(&self, operation: &str, marker: &str) {
        let requests = self.requests();
        assert!(
            requests.iter().any(|request| request.operation == operation
                && request.body.to_string().contains(marker)),
            "no {operation} request sent {marker:?}: {requests:#?}"
        );
    }

    /// The test drove exactly `count` requests under `operation`, and every one
    /// carried `session_id`.
    fn assert_identity(&self, operation: &str, session_id: &str, count: usize) {
        let requests: Vec<Recorded> = self
            .requests()
            .into_iter()
            .filter(|request| request.operation == operation)
            .collect();
        assert_eq!(requests.len(), count, "{operation} requests: {requests:#?}");
        let carried: Vec<Option<String>> = requests
            .iter()
            .map(|request| request.headers.get(SESSION_HEADER).cloned())
            .collect();
        assert_eq!(
            carried,
            vec![Some(session_id.to_string()); requests.len()],
            "every {operation} request routes under the owning session: {requests:#?}"
        );
    }
}

fn sse(text: &str, usage: &Value) -> String {
    let chunk = |delta: Value, finish: Option<&str>| {
        json!({
            "id": "chatcmpl-fixture",
            "object": "chat.completion.chunk",
            "created": 1_750_000_000,
            "model": "routing-fixture",
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
            "usage": usage,
        })
        .to_string()
    };
    format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        chunk(json!({"role": "assistant", "content": text}), None),
        chunk(json!({}), Some("stop")),
    )
}

fn serve(mut stream: TcpStream, state: &Arc<Mutex<MockState>>) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut head = String::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(());
        }
        head.push_str(&line);
        if line == "\r\n" {
            break;
        }
    }
    let headers: BTreeMap<String, String> = head
        .lines()
        .skip(1)
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            Some((name.trim().to_ascii_lowercase(), value.trim().to_string()))
        })
        .collect();
    let content_length: usize = headers
        .get("content-length")
        .and_then(|value| value.parse().ok())
        .unwrap_or_default();
    let mut body = vec![0u8; content_length];
    reader.read_exact(&mut body)?;
    let (reply, usage) = {
        let mut state = state.lock().expect("mock lock");
        let recorded = Recorded {
            operation: state.operation.clone(),
            headers: headers.clone(),
            body: serde_json::from_slice(&body).unwrap_or(Value::Null),
        };
        state.requests.push(recorded);
        let prompt_tokens = if std::mem::take(&mut state.crossing_usage_next) {
            126_000
        } else {
            10
        };
        let usage = json!({
            "prompt_tokens": prompt_tokens, "completion_tokens": 5,
            "total_tokens": prompt_tokens + 5,
            "prompt_tokens_details": {"cached_tokens": 0},
        });
        (state.reply.clone(), usage)
    };
    if !headers.contains_key(SESSION_HEADER) {
        let error = json!({"error": {"message": MISSING_SESSION, "type": "invalid_request_error"}})
            .to_string();
        return stream.write_all(
            format!(
                "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{error}",
                error.len()
            )
            .as_bytes(),
        );
    }
    stream.write_all(
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{}",
            sse(&reply, &usage)
        )
        .as_bytes(),
    )
}

struct Supervisor {
    child: Child,
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[allow(clippy::zombie_processes)]
fn spawn_supervisor(socket: &Path, agent_dir: &Path) -> Supervisor {
    let child = Command::new(env!("CARGO_BIN_EXE_pa-daemon"))
        .arg("supervisor")
        .arg("--socket")
        .arg(socket)
        .arg("--agent-dir")
        .arg(agent_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env_remove("PRIME_API_KEY")
        .env_remove("PRIME_AGENT_CODING_AGENT_DIR")
        .env_remove("OPENCODE_API_KEY")
        .env(
            pa_daemon::worker::WORKER_SUPERVISOR_LOST_EXIT_MS_ENV,
            "15000",
        )
        .spawn()
        .expect("spawn pa-daemon supervisor");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !socket.exists() {
        assert!(
            Instant::now() < deadline,
            "supervisor socket never appeared"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    Supervisor { child }
}

struct Client {
    reader: BufReader<std::os::unix::net::UnixStream>,
    writer: std::os::unix::net::UnixStream,
    /// Every non-response line read while waiting for something else.
    frames: Vec<Value>,
    next_id: u64,
}

impl Client {
    fn connect(socket: &Path) -> Client {
        let stream = std::os::unix::net::UnixStream::connect(socket).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .expect("timeout");
        let writer = stream.try_clone().expect("clone");
        let mut client = Client {
            reader: BufReader::new(stream),
            writer,
            frames: Vec::new(),
            next_id: 0,
        };
        let hello = client.read_line();
        assert_eq!(hello["type"], "daemon_hello");
        client
    }

    fn read_line(&mut self) -> Value {
        let deadline = Instant::now() + Duration::from_mins(1);
        let mut line = String::new();
        loop {
            match self.reader.read_line(&mut line) {
                Ok(0) => panic!("supervisor closed the connection"),
                Ok(_) if line.trim().is_empty() => line.clear(),
                Ok(_) => return serde_json::from_str(line.trim()).expect("parse line"),
                Err(error) => assert!(
                    Instant::now() < deadline,
                    "timed out waiting for a supervisor line: {error}"
                ),
            }
        }
    }

    /// Send `command` and return its successful response's `data`.
    fn request(&mut self, command: &Value) -> Value {
        self.next_id += 1;
        let id = format!("r{}", self.next_id);
        let envelope = json!({
            "type": "command",
            "id": id,
            "protocol": { "name": "prime-agent.daemon", "version": 7 },
            "command": command,
        });
        let mut line = serde_json::to_string(&envelope).expect("serialize");
        line.push('\n');
        self.writer.write_all(line.as_bytes()).expect("send");
        loop {
            let frame = self.read_line();
            if frame.get("id").and_then(Value::as_str) == Some(id.as_str()) {
                assert_eq!(frame["success"], true, "{command} failed: {frame}");
                return frame["data"].clone();
            }
            self.frames.push(frame);
        }
    }

    /// Wait for a frame matching `matches` (already-buffered frames count).
    fn wait_for_frame(&mut self, matches: impl Fn(&Value) -> bool) -> Value {
        if let Some(index) = self.frames.iter().position(&matches) {
            return self.frames.remove(index);
        }
        loop {
            let frame = self.read_line();
            if matches(&frame) {
                return frame;
            }
            self.frames.push(frame);
        }
    }
}

/// One live session's ids as the daemon reports them.
struct Session {
    active_id: String,
    durable_id: String,
    file: PathBuf,
}

impl Session {
    fn from_summary(summary: &Value) -> Session {
        Session {
            active_id: summary["id"].as_str().expect("active id").to_string(),
            durable_id: summary["sessionId"]
                .as_str()
                .expect("durable session id")
                .to_string(),
            file: PathBuf::from(summary["sessionFile"].as_str().expect("session file")),
        }
    }
}

/// A supervisor over an agent dir whose only provider is `provider`, served by the mock.
struct Harness {
    provider: &'static str,
    mock: OpencodeMock,
    client: Client,
    cwd: PathBuf,
    session_dir: PathBuf,
    _supervisor: Supervisor,
    _dir: tempfile::TempDir,
}

impl Harness {
    fn start(provider: &'static str, settings: &Value) -> Harness {
        let dir = tempfile::tempdir().expect("temp dir");
        let agent_dir = dir.path().join("agent");
        let session_dir = agent_dir.join("sessions");
        std::fs::create_dir_all(&session_dir).expect("session dir");
        let mock = OpencodeMock::start();
        let model = |id: &str| {
            json!({
                "id": id, "name": id, "api": "openai-completions", "reasoning": false,
                "contextWindow": 128_000, "maxTokens": 4096,
            })
        };
        std::fs::write(
            agent_dir.join("models.json"),
            json!({
                "providers": {
                    provider: {
                        "api": "openai-completions",
                        "baseUrl": mock.url(),
                        "apiKey": "fixture-not-a-real-key",
                        "models": [model("routing-fixture"), model("routing-fixture-2")],
                    }
                }
            })
            .to_string(),
        )
        .expect("write models.json");
        std::fs::write(agent_dir.join("settings.json"), settings.to_string())
            .expect("write settings.json");
        let socket = dir.path().join("daemon.sock");
        let supervisor = spawn_supervisor(&socket, &agent_dir);
        let client = Client::connect(&socket);
        Harness {
            provider,
            mock,
            client,
            cwd: dir.path().to_path_buf(),
            session_dir,
            _supervisor: supervisor,
            _dir: dir,
        }
    }

    fn create(&mut self, session_path: Option<&Path>) -> Session {
        let mut create = json!({
            "type": "create",
            "config": {
                "cwd": self.cwd.to_string_lossy(),
                "sessionDir": self.session_dir.to_string_lossy(),
                "provider": self.provider,
                "model": "routing-fixture",
            },
        });
        if let Some(path) = session_path {
            create["sessionPath"] = json!(path.to_string_lossy());
        }
        let created = self.client.request(&create);
        let session = Session::from_summary(&created);
        self.client
            .request(&json!({"type": "attach", "activeSessionId": session.active_id}));
        session
    }

    /// A main turn: it must succeed and route under the session's durable id.
    fn ordinary(&mut self, session: &Session, text: &str) {
        self.mock.set_operation("ordinary", "OK");
        let before = self.mock.count("ordinary");
        // `followUp` resumes the queued-input suspension a manual compaction leaves
        // (TS `_resumeSessionInputAdmission`); on an idle session it is a plain prompt.
        self.client.request(&json!({
            "type": "prompt_and_wait", "activeSessionId": session.active_id, "message": text,
            "streamingBehavior": "followUp",
        }));
        let requests: Vec<Recorded> = self
            .mock
            .requests()
            .into_iter()
            .filter(|request| request.operation == "ordinary")
            .skip(before)
            .collect();
        let turn = requests
            .iter()
            .map(|request| request.headers.get(SESSION_HEADER).cloned())
            .collect::<Vec<_>>();
        assert_eq!(
            turn,
            vec![Some(session.durable_id.clone())],
            "the main turn routes under the owning session ({}): {requests:#?}",
            self.provider
        );
    }

    fn compact(&mut self, session: &Session, operation: &str, reply: &str) {
        self.mock.set_operation(operation, reply);
        let compacted = self
            .client
            .request(&json!({"type": "compact", "activeSessionId": session.active_id}));
        assert!(
            compacted.get("summary").is_some() || compacted.get("result").is_some(),
            "the compaction ran ({}): {compacted}",
            self.provider
        );
    }

    /// The first assistant message entry id in the session file.
    fn first_assistant_entry(session: &Session) -> String {
        std::fs::read_to_string(&session.file)
            .expect("read session file")
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find(|entry| entry["type"] == "message" && entry["message"]["role"] == "assistant")
            .and_then(|entry| entry["id"].as_str().map(str::to_string))
            .expect("an assistant entry")
    }
}

fn base_settings() -> Value {
    json!({
        "compaction": {"enabled": false, "keepRecentTokens": 1},
        "autoRefine": {"enabled": false},
        "retry": {"enabled": true, "maxRetries": 1, "baseDelayMs": 1},
    })
}

#[test]
fn every_maintenance_call_routes_under_the_owning_session() {
    for provider in PROVIDERS {
        let mut harness = Harness::start(provider, &base_settings());
        let session = harness.create(None);
        harness.ordinary(&session, "remember this request");

        // Turn-prefix compaction: one turn, so the cut splits it.
        harness.compact(&session, "turn-prefix", "OK");
        harness
            .mock
            .assert_identity("turn-prefix", &session.durable_id, 1);

        // Split-turn compaction: history plus the turn prefix.
        harness.ordinary(&session, "second request");
        harness.ordinary(&session, "third request");
        harness.compact(&session, "split-compaction", "OK");
        harness
            .mock
            .assert_identity("split-compaction", &session.durable_id, 2);

        // `/refine` planning.
        harness.mock.set_operation("refinement-plan", PROPOSAL);
        harness
            .client
            .request(&json!({"type": "refine", "activeSessionId": session.active_id}));
        harness
            .mock
            .assert_identity("refinement-plan", &session.durable_id, 1);

        // A tree branch summary of the abandoned branch.
        let target = Harness::first_assistant_entry(&session);
        harness.ordinary(&session, "explore a branch");
        harness.mock.set_operation("branch-summary", "OK");
        let navigated = harness.client.request(&json!({
            "type": "navigate_tree", "activeSessionId": session.active_id,
            "targetId": target, "summarize": true,
        }));
        assert_eq!(navigated["cancelled"], false, "{navigated}");
        harness
            .mock
            .assert_identity("branch-summary", &session.durable_id, 1);

        // A side question rides the parent session's id.
        harness.mock.set_operation("side-question", "OK");
        harness.client.request(&json!({
            "type": "start_side_question", "activeSessionId": session.active_id,
            "sideQuestionId": "sq-1", "question": "what did I ask?",
        }));
        let settled = harness.client.wait_for_frame(|frame| {
            frame["type"] == "side_question_event" && frame["event"]["status"] != "running"
        });
        assert_eq!(settled["event"]["status"], "complete", "{settled}");
        harness
            .mock
            .assert_identity("side-question", &session.durable_id, 1);

        // A model switch and a reload keep the session's id.
        harness.client.request(&json!({
            "type": "set_model", "activeSessionId": session.active_id,
            "provider": provider, "modelId": "routing-fixture-2",
        }));
        harness.ordinary(&session, "after the model switch");
        harness
            .client
            .request(&json!({"type": "reload", "activeSessionId": session.active_id}));
        harness.ordinary(&session, "after the reload");
    }
}

#[test]
fn automatic_compaction_and_the_auto_refine_review_route_under_the_owning_session() {
    let settings = json!({
        // A tiny reserve: the crossing usage below fires the threshold compaction.
        "compaction": {"enabled": true, "reserveTokens": 500, "keepRecentTokens": 1},
        "autoRefine": {"enabled": true},
        "retry": {"enabled": true, "maxRetries": 1, "baseDelayMs": 1},
    });
    for provider in PROVIDERS {
        let mut harness = Harness::start(provider, &settings);
        let session = harness.create(None);
        harness.ordinary(&session, "remember this request");

        // The crossing turn's settle fires the threshold compaction.
        harness.mock.cross_threshold_next();
        harness.mock.set_operation("automatic-compaction", "OK");
        harness.client.request(&json!({
            "type": "prompt_and_wait", "activeSessionId": session.active_id,
            "message": "crossing request",
        }));
        harness.client.wait_for_frame(|frame| {
            frame["type"] == "session_event" && frame["event"]["type"] == "compaction_end"
        });
        // The crossing turn plus both split-turn summaries.
        harness.mock.wait_for("automatic-compaction", 3);
        harness
            .mock
            .assert_identity("automatic-compaction", &session.durable_id, 3);

        // The compaction armed the compact-trigger auto-refine review; the next
        // prompt's admission runs it (declined) ahead of the turn.
        harness
            .mock
            .set_operation("refinement-review", REVIEW_DECLINE);
        harness.client.request(&json!({
            "type": "prompt_and_wait", "activeSessionId": session.active_id,
            "message": "another request", "streamingBehavior": "followUp",
        }));
        harness.mock.assert_sent("refinement-review", "<trigger>");
        harness
            .mock
            .assert_identity("refinement-review", &session.durable_id, 2);
    }
}

#[test]
fn resume_keeps_and_replacement_changes_the_routing_identity() {
    for provider in PROVIDERS {
        let mut harness = Harness::start(provider, &base_settings());
        let original = harness.create(None);
        harness.ordinary(&original, "remember this request");
        harness.compact(&original, "first-compaction", "OK");
        harness
            .mock
            .assert_identity("first-compaction", &original.durable_id, 1);

        // `/new` replaces the session inside the same worker: its turns and
        // maintenance route under the NEW session's id.
        let replaced = harness
            .client
            .request(&json!({"type": "new_session", "activeSessionId": original.active_id}));
        assert_eq!(replaced["cancelled"], false, "{replaced}");
        let state = harness
            .client
            .request(&json!({"type": "get_state", "activeSessionId": original.active_id}));
        let replacement = Session {
            active_id: original.active_id.clone(),
            durable_id: state["sessionId"].as_str().expect("session id").to_string(),
            file: PathBuf::from(state["sessionFile"].as_str().expect("session file")),
        };
        assert_ne!(replacement.durable_id, original.durable_id);
        harness.ordinary(&replacement, "a fresh conversation");
        harness.compact(&replacement, "replacement-compaction", "OK");
        harness
            .mock
            .assert_identity("replacement-compaction", &replacement.durable_id, 1);

        // Switching back to the original file restores its id.
        harness.client.request(&json!({
            "type": "switch_session", "activeSessionId": original.active_id,
            "sessionPath": original.file.to_string_lossy(),
        }));
        harness.ordinary(&original, "back on the original");

        // A separate session never shares the original's id.
        let other = harness.create(None);
        assert_ne!(other.durable_id, original.durable_id);
        harness.ordinary(&other, "an unrelated session");
        harness.compact(&other, "other-compaction", "OK");
        harness
            .mock
            .assert_identity("other-compaction", &other.durable_id, 1);

        // Resume: a new worker over the original file keeps its id for turns,
        // compaction and refinement.
        harness
            .client
            .request(&json!({"type": "kill", "activeSessionId": original.active_id}));
        let resumed = harness.create(Some(&original.file));
        assert_eq!(resumed.durable_id, original.durable_id);
        harness.ordinary(&resumed, "continue resumed session");
        harness.compact(&resumed, "resumed-compaction", "OK");
        harness
            .mock
            .assert_identity("resumed-compaction", &original.durable_id, 1);
        harness.mock.set_operation("resumed-refinement", PROPOSAL);
        harness
            .client
            .request(&json!({"type": "refine", "activeSessionId": resumed.active_id}));
        harness
            .mock
            .assert_identity("resumed-refinement", &original.durable_id, 1);
    }
}
