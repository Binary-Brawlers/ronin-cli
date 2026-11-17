use async_trait::async_trait;
use futures::stream;
use ronin_agent_core::{
    AgentCompletionRequest, AgentLoopError, CompletionClient, CompletionStream, FinishReason,
    GenerationStreamEvent, ModelSummary, TokenUsage,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex, RwLock},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
    sync::mpsc,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::config::PermissionMode;

const CODEX_PREFIX: &str = "codex/";
const CLAUDE_PREFIX: &str = "claude/";
const OPENCODE_PREFIX: &str = "opencode/";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HarnessKind {
    Codex,
    Claude,
    OpenCode,
}

impl HarnessKind {
    pub fn id(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::OpenCode => "opencode",
        }
    }

    fn binary(self) -> &'static str {
        self.id()
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "codex" => Some(Self::Codex),
            "claude" => Some(Self::Claude),
            "opencode" => Some(Self::OpenCode),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct HarnessStatus {
    pub kind: HarnessKind,
    pub installed: bool,
    pub authenticated: bool,
    pub detail: String,
}

#[derive(Debug, Clone)]
struct HarnessContext {
    cwd: PathBuf,
    permission_mode: PermissionMode,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct NativeSessions {
    #[serde(default)]
    sessions: HashMap<String, NativeSession>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct NativeSession {
    harness: String,
    native_id: String,
}

#[derive(Clone)]
pub struct HarnessRegistry {
    home: PathBuf,
    context: Arc<RwLock<HarnessContext>>,
    sessions: Arc<Mutex<NativeSessions>>,
    cancellations: Arc<Mutex<HashMap<String, CancellationToken>>>,
}

impl HarnessRegistry {
    pub fn new(home: &Path, cwd: &Path, permission_mode: PermissionMode) -> Self {
        let sessions = load_native_sessions(home);
        Self {
            home: home.to_path_buf(),
            context: Arc::new(RwLock::new(HarnessContext {
                cwd: cwd.to_path_buf(),
                permission_mode,
            })),
            sessions: Arc::new(Mutex::new(sessions)),
            cancellations: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn configure(&self, cwd: &Path, permission_mode: PermissionMode) {
        *self.context.write().expect("harness context lock poisoned") = HarnessContext {
            cwd: cwd.to_path_buf(),
            permission_mode,
        };
    }

    pub fn handles_model(model: &str) -> bool {
        model.starts_with(CODEX_PREFIX)
            || model.starts_with(CLAUDE_PREFIX)
            || model.starts_with(OPENCODE_PREFIX)
    }

    pub fn cancel_generation(&self, generation_id: &str) -> bool {
        let cancellation = self
            .cancellations
            .lock()
            .expect("harness cancellation lock poisoned")
            .get(generation_id)
            .cloned();
        if let Some(cancellation) = cancellation {
            cancellation.cancel();
            true
        } else {
            false
        }
    }

    pub async fn statuses(&self) -> Vec<HarnessStatus> {
        let mut statuses = Vec::new();
        for kind in [
            HarnessKind::Codex,
            HarnessKind::Claude,
            HarnessKind::OpenCode,
        ] {
            statuses.push(probe_status(kind).await);
        }
        statuses
    }

    pub async fn setup(&self, kind: HarnessKind) -> Result<(), String> {
        let mut command = Command::new(kind.binary());
        match kind {
            HarnessKind::Codex => {
                command.arg("login");
            }
            HarnessKind::Claude => {
                command.args(["auth", "login"]);
            }
            HarnessKind::OpenCode => {
                command.args(["auth", "login"]);
            }
        }
        let status = command
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .await
            .map_err(|error| format!("Could not start {} login: {error}", kind.id()))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("{} login exited with {status}.", kind.id()))
        }
    }

    pub async fn models(&self) -> Vec<ModelSummary> {
        let statuses = self.statuses().await;
        let mut models = Vec::new();
        for status in statuses {
            if !status.installed || !status.authenticated {
                continue;
            }
            match status.kind {
                HarnessKind::Codex => models.extend(codex_models().await),
                HarnessKind::Claude => models.extend(claude_models()),
                HarnessKind::OpenCode => models.extend(opencode_models().await),
            }
        }
        models
    }

    fn context(&self) -> HarnessContext {
        self.context
            .read()
            .expect("harness context lock poisoned")
            .clone()
    }

    fn native_session(&self, ronin_session_id: Option<&str>, kind: HarnessKind) -> Option<String> {
        let id = ronin_session_id?;
        self.sessions
            .lock()
            .expect("native session lock poisoned")
            .sessions
            .get(&native_session_key(id, kind))
            .filter(|session| session.harness == kind.id())
            .map(|session| session.native_id.clone())
    }

    fn save_native_session(
        &self,
        ronin_session_id: Option<&str>,
        kind: HarnessKind,
        native_id: &str,
    ) {
        let Some(id) = ronin_session_id else {
            return;
        };
        let mut sessions = self.sessions.lock().expect("native session lock poisoned");
        sessions.sessions.insert(
            native_session_key(id, kind),
            NativeSession {
                harness: kind.id().into(),
                native_id: native_id.into(),
            },
        );
        let _ = persist_native_sessions(&self.home, &sessions);
    }
}

fn native_session_key(ronin_session_id: &str, kind: HarnessKind) -> String {
    format!("{ronin_session_id}:{}", kind.id())
}

#[async_trait]
impl CompletionClient for HarnessRegistry {
    async fn create_completion(
        &self,
        request: AgentCompletionRequest,
        cancel: &CancellationToken,
    ) -> Result<CompletionStream, AgentLoopError> {
        let (kind, model) = split_model(&request.model).ok_or_else(|| {
            AgentLoopError::new(
                "unsupported_harness_model",
                format!("No native harness owns model {}", request.model),
                false,
            )
        })?;
        let prompt = request
            .messages
            .iter()
            .rev()
            .find_map(|message| match message {
                ronin_agent_core::AgentMessage::User { content } => Some(content.clone()),
                _ => None,
            })
            .ok_or_else(|| {
                AgentLoopError::new("missing_prompt", "No user prompt was supplied", false)
            })?;
        let generation_id = Uuid::new_v4().to_string();
        let generation_cancel = cancel.child_token();
        self.cancellations
            .lock()
            .expect("harness cancellation lock poisoned")
            .insert(generation_id.clone(), generation_cancel.clone());

        let (tx, rx) = mpsc::channel(128);
        let registry = self.clone();
        let request_session_id = request.session_id.clone();
        let native_session = self.native_session(request.session_id.as_deref(), kind);
        let context = self.context();
        let generation_for_cleanup = generation_id.clone();
        tokio::spawn(async move {
            let result = run_harness(
                kind,
                &model,
                &prompt,
                native_session.as_deref(),
                request_session_id.as_deref(),
                &context,
                &generation_cancel,
                &tx,
                &registry,
            )
            .await;
            if let Err(error) = result {
                let _ = tx
                    .send(Ok(GenerationStreamEvent::Error {
                        code: error.code,
                        message: error.message,
                        retryable: error.retryable,
                    }))
                    .await;
            }
            registry
                .cancellations
                .lock()
                .expect("harness cancellation lock poisoned")
                .remove(&generation_for_cleanup);
        });
        Ok(CompletionStream {
            generation_id,
            events: Box::pin(stream::unfold(rx, |mut receiver| async move {
                receiver.recv().await.map(|event| (event, receiver))
            })),
        })
    }

    async fn resume_completion(
        &self,
        _generation_id: &str,
        _cancel: &CancellationToken,
    ) -> Result<CompletionStream, AgentLoopError> {
        Err(AgentLoopError::new(
            "native_generation_not_resumable",
            "Interrupted native harness generations cannot be reattached; send the turn again.",
            false,
        ))
    }

    async fn stop_generation(&self, generation_id: &str) -> Result<(), AgentLoopError> {
        self.cancel_generation(generation_id);
        Ok(())
    }
}

fn split_model(model: &str) -> Option<(HarnessKind, String)> {
    if let Some(model) = model.strip_prefix(CODEX_PREFIX) {
        return Some((HarnessKind::Codex, model.into()));
    }
    if let Some(model) = model.strip_prefix(CLAUDE_PREFIX) {
        return Some((HarnessKind::Claude, model.into()));
    }
    model
        .strip_prefix(OPENCODE_PREFIX)
        .map(|model| (HarnessKind::OpenCode, model.into()))
}

async fn probe_status(kind: HarnessKind) -> HarnessStatus {
    let version = Command::new(kind.binary()).arg("--version").output().await;
    let Ok(version) = version else {
        return HarnessStatus {
            kind,
            installed: false,
            authenticated: false,
            detail: format!("{} is not installed or is not on PATH", kind.binary()),
        };
    };
    if !version.status.success() {
        return HarnessStatus {
            kind,
            installed: false,
            authenticated: false,
            detail: String::from_utf8_lossy(&version.stderr).trim().into(),
        };
    }
    let auth = match kind {
        HarnessKind::Codex => {
            Command::new("codex")
                .args(["login", "status"])
                .output()
                .await
        }
        HarnessKind::Claude => {
            Command::new("claude")
                .args(["auth", "status", "--json"])
                .output()
                .await
        }
        HarnessKind::OpenCode => {
            Command::new("opencode")
                .args(["auth", "list"])
                .output()
                .await
        }
    };
    let (authenticated, auth_detail) = match auth {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let authenticated = match kind {
                HarnessKind::Claude => serde_json::from_str::<Value>(&stdout)
                    .ok()
                    .and_then(|value| value.get("loggedIn").and_then(Value::as_bool))
                    .unwrap_or(false),
                HarnessKind::OpenCode => {
                    !stdout.trim().is_empty()
                        && !stdout.to_ascii_lowercase().contains("no credentials")
                        && !stdout.to_ascii_lowercase().contains("0 credentials")
                }
                HarnessKind::Codex => true,
            };
            (authenticated, stdout.trim().to_string())
        }
        Ok(output) => (false, String::from_utf8_lossy(&output.stderr).trim().into()),
        Err(error) => (false, error.to_string()),
    };
    let version = String::from_utf8_lossy(&version.stdout).trim().to_string();
    HarnessStatus {
        kind,
        installed: true,
        authenticated,
        detail: if authenticated {
            version
        } else if auth_detail.is_empty() {
            format!("{version}; not authenticated")
        } else {
            format!("{version}; {auth_detail}")
        },
    }
}

fn claude_models() -> Vec<ModelSummary> {
    [
        ("claude/sonnet", "Claude Sonnet"),
        ("claude/opus", "Claude Opus"),
        ("claude/haiku", "Claude Haiku"),
        ("claude/fable", "Claude Fable"),
    ]
    .into_iter()
    .map(|(model_id, display_name)| ModelSummary {
        model_id: model_id.into(),
        display_name: display_name.into(),
        category: "Claude Code subscription".into(),
        supports_tools: true,
        context_window: 200_000,
        credit_price_micro: None,
    })
    .collect()
}

async fn opencode_models() -> Vec<ModelSummary> {
    let Ok(output) = Command::new("opencode").arg("models").output().await else {
        return vec![];
    };
    if !output.status.success() {
        return vec![];
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && line.contains('/'))
        .map(|slug| ModelSummary {
            model_id: format!("{OPENCODE_PREFIX}{slug}"),
            display_name: slug.into(),
            category: "OpenCode harness".into(),
            supports_tools: true,
            context_window: 200_000,
            credit_price_micro: None,
        })
        .collect()
}

async fn codex_models() -> Vec<ModelSummary> {
    let mut child = match Command::new("codex")
        .arg("app-server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return vec![],
    };
    let Some(mut stdin) = child.stdin.take() else {
        return vec![];
    };
    let Some(stdout) = child.stdout.take() else {
        return vec![];
    };
    let requests = [
        serde_json::json!({"method":"initialize","id":1,"params":{"clientInfo":{"name":"ronin-cli","title":"Ronin CLI","version":env!("CARGO_PKG_VERSION")},"capabilities":{"experimentalApi":true}}}),
        serde_json::json!({"method":"initialized","params":{}}),
        serde_json::json!({"method":"model/list","id":2,"params":{"includeHidden":false,"limit":100}}),
    ];
    for request in requests {
        if stdin
            .write_all(format!("{request}\n").as_bytes())
            .await
            .is_err()
        {
            let _ = child.kill().await;
            return vec![];
        }
    }
    let mut lines = BufReader::new(stdout).lines();
    let response = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if value.get("id").and_then(Value::as_i64) == Some(2) {
                return Some(value);
            }
        }
        None
    })
    .await
    .ok()
    .flatten();
    let _ = child.kill().await;
    response
        .and_then(|value| {
            value
                .pointer("/result/data")
                .and_then(Value::as_array)
                .cloned()
        })
        .unwrap_or_default()
        .into_iter()
        .filter_map(|model| {
            let id = model
                .get("model")
                .or_else(|| model.get("id"))?
                .as_str()?
                .to_string();
            let display = model
                .get("displayName")
                .and_then(Value::as_str)
                .unwrap_or(&id)
                .to_string();
            Some(ModelSummary {
                model_id: format!("{CODEX_PREFIX}{id}"),
                display_name: display,
                category: "Codex subscription".into(),
                supports_tools: true,
                context_window: 256_000,
                credit_price_micro: None,
            })
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
async fn run_harness(
    kind: HarnessKind,
    model: &str,
    prompt: &str,
    native_session: Option<&str>,
    ronin_session_id: Option<&str>,
    context: &HarnessContext,
    cancel: &CancellationToken,
    tx: &mpsc::Sender<Result<GenerationStreamEvent, AgentLoopError>>,
    registry: &HarnessRegistry,
) -> Result<(), AgentLoopError> {
    let mut command = Command::new(kind.binary());
    match kind {
        HarnessKind::Codex => {
            command.current_dir(&context.cwd).arg("exec");
            if let Some(session) = native_session {
                command.args(["resume", session]);
            } else {
                command.args(["--cd", context.cwd.to_string_lossy().as_ref()]);
            }
            command.args(["--model", model, "--json", "--skip-git-repo-check"]);
            match context.permission_mode {
                PermissionMode::Plan | PermissionMode::Default => {
                    command.args([
                        "--config",
                        "approval_policy=\"never\"",
                        "--config",
                        "sandbox_mode=\"read-only\"",
                    ]);
                }
                PermissionMode::AcceptEdits | PermissionMode::Auto => {
                    command.args([
                        "--config",
                        "approval_policy=\"never\"",
                        "--config",
                        "sandbox_mode=\"workspace-write\"",
                    ]);
                }
                PermissionMode::Yolo => {
                    command.arg("--dangerously-bypass-approvals-and-sandbox");
                }
            }
            command.arg("-");
        }
        HarnessKind::Claude => {
            command
                .current_dir(&context.cwd)
                .args(["--print", "--output-format", "stream-json"])
                .args(["--include-partial-messages", "--verbose"])
                .args(["--model", model]);
            if let Some(session) = native_session {
                command.args(["--resume", session]);
            } else if let Some(session) = ronin_session_id {
                command.args(["--session-id", session]);
            }
            let mode = match context.permission_mode {
                // Print mode cannot surface Claude's interactive permission UI.
                // Deny approval-requiring actions instead of silently granting them.
                PermissionMode::Default => "dontAsk",
                PermissionMode::AcceptEdits => "acceptEdits",
                PermissionMode::Plan => "plan",
                PermissionMode::Auto => "auto",
                PermissionMode::Yolo => "bypassPermissions",
            };
            command.args(["--permission-mode", mode]);
            if context.permission_mode == PermissionMode::Yolo {
                command.arg("--allow-dangerously-skip-permissions");
            }
        }
        HarnessKind::OpenCode => {
            command.current_dir(&context.cwd).arg("run");
            if let Some(session) = native_session {
                command.args(["--session", session]);
            }
            command.args(["--model", model, "--format", "json"]);
            if matches!(
                context.permission_mode,
                PermissionMode::Auto | PermissionMode::Yolo
            ) {
                command.arg("--auto");
            }
        }
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().map_err(|error| {
        AgentLoopError::new(
            "harness_spawn_failed",
            format!("Could not start {}: {error}", kind.id()),
            false,
        )
    })?;
    let mut stdin = child.stdin.take().expect("piped harness stdin");
    stdin.write_all(prompt.as_bytes()).await.map_err(|error| {
        AgentLoopError::new(
            "harness_prompt_failed",
            format!("Could not send the prompt to {}: {error}", kind.id()),
            true,
        )
    })?;
    drop(stdin);
    let stdout = child.stdout.take().expect("piped harness stdout");
    let stderr = child.stderr.take().expect("piped harness stderr");
    let stderr_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        let mut output = String::new();
        while let Ok(Some(line)) = lines.next_line().await {
            if output.len() < 32_000 {
                output.push_str(&line);
                output.push('\n');
            }
        }
        output
    });
    let mut lines = BufReader::new(stdout).lines();
    let mut emitted_text = false;
    let mut usage = None;
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                let _ = child.kill().await;
                let _ = tx.send(Ok(GenerationStreamEvent::Done {
                    message_id: None,
                    usage,
                    cost_micro: None,
                    finish_reason: FinishReason::Stopped,
                })).await;
                return Ok(());
            }
            line = lines.next_line() => {
                let line = line.map_err(|error| AgentLoopError::new("harness_stream_failed", error.to_string(), true))?;
                let Some(line) = line else { break };
                let Ok(value) = serde_json::from_str::<Value>(&line) else { continue };
                if let Some(native_id) = native_session_id(kind, &value) {
                    registry.save_native_session(ronin_session_id, kind, &native_id);
                }
                if let Some(next_usage) = extract_usage(&value) {
                    usage = Some(next_usage);
                }
                if let Some(reasoning) = extract_reasoning(kind, &value) {
                    let _ = tx.send(Ok(GenerationStreamEvent::Reasoning { text: reasoning })).await;
                }
                let duplicate_claude_result = kind == HarnessKind::Claude
                    && emitted_text
                    && value.get("type").and_then(Value::as_str) == Some("result");
                if !duplicate_claude_result {
                    if let Some(text) = extract_delta(kind, &value) {
                    if !text.is_empty() {
                        emitted_text = true;
                        let _ = tx.send(Ok(GenerationStreamEvent::Delta { text })).await;
                    }
                    }
                }
            }
        }
    }
    let status = child
        .wait()
        .await
        .map_err(|error| AgentLoopError::new("harness_wait_failed", error.to_string(), true))?;
    let stderr = stderr_task.await.unwrap_or_default();
    if !status.success() {
        return Err(AgentLoopError::new(
            "harness_failed",
            if stderr.trim().is_empty() {
                format!("{} exited with {status}", kind.id())
            } else {
                stderr.trim().to_string()
            },
            false,
        ));
    }
    if !emitted_text {
        return Err(AgentLoopError::new(
            "harness_empty_response",
            format!("{} completed without returning assistant text", kind.id()),
            true,
        ));
    }
    let _ = tx
        .send(Ok(GenerationStreamEvent::Done {
            message_id: None,
            usage,
            cost_micro: None,
            finish_reason: FinishReason::Complete,
        }))
        .await;
    Ok(())
}

fn native_session_id(kind: HarnessKind, value: &Value) -> Option<String> {
    match kind {
        HarnessKind::Codex => value
            .get("thread_id")
            .or_else(|| value.pointer("/thread/id"))
            .and_then(Value::as_str),
        HarnessKind::Claude => value
            .get("session_id")
            .or_else(|| value.pointer("/message/session_id"))
            .and_then(Value::as_str),
        HarnessKind::OpenCode => value
            .get("sessionID")
            .or_else(|| value.get("session_id"))
            .and_then(Value::as_str),
    }
    .map(str::to_string)
}

fn extract_delta(kind: HarnessKind, value: &Value) -> Option<String> {
    match kind {
        HarnessKind::Codex => {
            if value.get("type").and_then(Value::as_str) == Some("item.completed")
                && value.pointer("/item/type").and_then(Value::as_str) == Some("agent_message")
            {
                return value
                    .pointer("/item/text")
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
            if value.get("type").and_then(Value::as_str) == Some("item.agentMessage.delta") {
                return value
                    .get("delta")
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
            None
        }
        HarnessKind::Claude => {
            if value.get("type").and_then(Value::as_str) == Some("stream_event")
                && value.pointer("/event/type").and_then(Value::as_str)
                    == Some("content_block_delta")
            {
                return value
                    .pointer("/event/delta/text")
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
            if value.get("type").and_then(Value::as_str) == Some("result") {
                return value
                    .get("result")
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
            None
        }
        HarnessKind::OpenCode => {
            if value.get("type").and_then(Value::as_str) == Some("text") {
                return value
                    .pointer("/part/text")
                    .or_else(|| value.get("text"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
            None
        }
    }
}

fn extract_reasoning(kind: HarnessKind, value: &Value) -> Option<String> {
    match kind {
        HarnessKind::Claude
            if value.get("type").and_then(Value::as_str) == Some("stream_event")
                && value.pointer("/event/type").and_then(Value::as_str)
                    == Some("content_block_delta") =>
        {
            value
                .pointer("/event/delta/thinking")
                .and_then(Value::as_str)
                .map(str::to_string)
        }
        HarnessKind::OpenCode if value.get("type").and_then(Value::as_str) == Some("reasoning") => {
            value
                .pointer("/part/text")
                .or_else(|| value.get("text"))
                .and_then(Value::as_str)
                .map(str::to_string)
        }
        _ => None,
    }
}

fn extract_usage(value: &Value) -> Option<TokenUsage> {
    let usage = value
        .get("usage")
        .or_else(|| value.pointer("/message/usage"))?;
    let input_tokens = usage
        .get("input_tokens")
        .or_else(|| usage.get("inputTokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output_tokens = usage
        .get("output_tokens")
        .or_else(|| usage.get("outputTokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if input_tokens == 0 && output_tokens == 0 {
        return None;
    }
    Some(TokenUsage {
        input_tokens,
        output_tokens,
        cached_tokens: usage
            .get("cached_input_tokens")
            .or_else(|| usage.get("cache_read_input_tokens"))
            .and_then(Value::as_u64),
        cache_write_tokens: usage
            .get("cache_creation_input_tokens")
            .and_then(Value::as_u64),
    })
}

fn native_sessions_path(home: &Path) -> PathBuf {
    home.join(".ronin").join("harness-sessions.json")
}

fn load_native_sessions(home: &Path) -> NativeSessions {
    std::fs::read_to_string(native_sessions_path(home))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn persist_native_sessions(home: &Path, sessions: &NativeSessions) -> std::io::Result<()> {
    let path = native_sessions_path(home);
    std::fs::create_dir_all(path.parent().expect("harness session path has parent"))?;
    let raw = serde_json::to_vec_pretty(sessions)?;
    let mut temp = tempfile::NamedTempFile::new_in(path.parent().expect("path has parent"))?;
    use std::io::Write;
    temp.write_all(&raw)?;
    temp.persist(path).map_err(|error| error.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_namespaced_models() {
        assert_eq!(
            split_model("codex/gpt-test"),
            Some((HarnessKind::Codex, "gpt-test".into()))
        );
        assert_eq!(
            split_model("claude/sonnet"),
            Some((HarnessKind::Claude, "sonnet".into()))
        );
        assert_eq!(
            split_model("opencode/openai/gpt-test"),
            Some((HarnessKind::OpenCode, "openai/gpt-test".into()))
        );
        assert_eq!(split_model("openrouter/model"), None);
    }

    #[test]
    fn parses_claude_stream_delta() {
        let value = serde_json::json!({
            "type":"stream_event",
            "event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"hello"}}
        });
        assert_eq!(
            extract_delta(HarnessKind::Claude, &value).as_deref(),
            Some("hello")
        );
    }

    #[test]
    fn persists_native_session_mapping() {
        let home = tempfile::tempdir().unwrap();
        let registry = HarnessRegistry::new(home.path(), home.path(), PermissionMode::Default);
        registry.save_native_session(Some("ronin-id"), HarnessKind::Codex, "native-id");
        registry.save_native_session(Some("ronin-id"), HarnessKind::Claude, "claude-native-id");
        let restored = HarnessRegistry::new(home.path(), home.path(), PermissionMode::Default);
        assert_eq!(
            restored
                .native_session(Some("ronin-id"), HarnessKind::Codex)
                .as_deref(),
            Some("native-id")
        );
        assert_eq!(
            restored
                .native_session(Some("ronin-id"), HarnessKind::Claude)
                .as_deref(),
            Some("claude-native-id")
        );
    }
}
