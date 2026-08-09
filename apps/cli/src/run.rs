use crate::{
    client::RoninApiClient,
    config::{PermissionMode, RoninConfig},
    context::initial_messages,
    permissions::PermissionController,
    storage::{LocalSession, SessionState, SessionStore},
};
use futures::StreamExt;
use ronin_agent_core::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex},
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub struct RunRequest<'a> {
    pub prompt: &'a str,
    pub model: &'a str,
    pub max_credits: Option<f64>,
    pub dangerous: bool,
    pub cwd: &'a Path,
    pub session: Option<LocalSession>,
    pub cancel: Option<CancellationToken>,
    pub event_tx: Option<mpsc::UnboundedSender<RunEvent>>,
    pub authorizer: Option<Arc<dyn PermissionAuthorizer>>,
    pub web_search: bool,
    pub questioner: Option<Arc<dyn UserQuestioner>>,
    pub source: &'a str,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunWarning {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone)]
pub enum RunEvent {
    Agent(AgentLoopEvent),
    Warning(RunWarning),
    CompactionStarted { model: String },
    CompactionCompleted { model: String, cost_micro: u64 },
    Checkpoint(AgentLoopCheckpoint),
}

#[derive(Debug, Clone)]
pub struct RunOutcome {
    pub code: i32,
    pub session: LocalSession,
    pub result: AgentLoopResult,
    pub invocation_cost_micro: u64,
    pub warnings: Vec<RunWarning>,
    pub file_changes: Vec<FileChange>,
    pub affected_paths: Vec<String>,
    pub commands: Vec<String>,
}

fn publish(tx: &Option<mpsc::UnboundedSender<RunEvent>>, event: RunEvent) {
    if let Some(tx) = tx {
        let _ = tx.send(event);
    }
}

pub async fn run_prompt(
    client: RoninApiClient,
    config: &RoninConfig,
    store: Arc<SessionStore>,
    request: RunRequest<'_>,
) -> Result<RunOutcome, String> {
    let models = client.models().await.map_err(|error| error.to_string())?;
    let selected = models
        .iter()
        .find(|model| model.model_id == request.model && model.supports_tools)
        .ok_or_else(|| {
            format!(
                "Model {} is unavailable or does not support tools.",
                request.model
            )
        })?;
    let mode = if request.dangerous {
        PermissionMode::Yolo
    } else {
        config.permission_mode
    };
    let mut tools = create_read_only_tools(request.cwd)?;
    tools.push(create_ask_user_tool(
        request
            .questioner
            .clone()
            .unwrap_or_else(|| Arc::new(UnavailableQuestioner)),
    ));
    if mode != PermissionMode::Plan {
        tools.extend(create_mutation_tools(request.cwd)?);
        tools.push(create_bash_tool(request.cwd)?);
    }

    let mut warnings = Vec::new();
    let (mut messages, context_warning) = if request.session.is_some() {
        (vec![], None)
    } else {
        initial_messages(request.cwd)
    };
    if let Some(message) = context_warning {
        let warning = RunWarning {
            code: "project_instructions_warning".into(),
            message,
        };
        publish(&request.event_tx, RunEvent::Warning(warning.clone()));
        warnings.push(warning);
    }
    let mut session = match request.session {
        Some(value) => value,
        None => store
            .create(
                request.cwd,
                &selected.model_id,
                std::mem::take(&mut messages),
            )
            .map_err(|error| error.to_string())?,
    };
    let _run_lease = if request.source == "cli" {
        Some(store.acquire_lease(&session.id, "cli", env!("CARGO_PKG_VERSION"), false)
            .map_err(|_| "This session is owned by another Ronin client. Open it there or start a new session.".to_string())?)
    } else {
        None
    };
    let invocation_start_cost = session.cost_micro;
    let prior_model = models.iter().find(|model| model.model_id == session.model);
    let switching_smaller =
        prior_model.is_some_and(|prior| selected.context_window < prior.context_window);
    let used_tokens = session
        .last_usage
        .as_ref()
        .map(|usage| usage.input_tokens + usage.output_tokens);
    let multiple_turns = session
        .messages
        .iter()
        .filter(|message| matches!(message, AgentMessage::User { .. }))
        .count()
        > 1;
    let switch_needs_compaction = switching_smaller
        && used_tokens.map_or(multiple_turns, |tokens| {
            tokens as f64 / selected.context_window as f64 >= 0.8
        });
    if session.context_percent.unwrap_or(0.0) >= 0.8 || switch_needs_compaction {
        let internal = config
            .internal_model
            .as_ref()
            .and_then(|id| models.iter().find(|model| &model.model_id == id))
            .unwrap_or(selected);
        if config.internal_model.as_deref() != Some(&internal.model_id) {
            let warning = RunWarning {
                code: "internal_model_fallback".into(),
                message: format!(
                    "No eligible internal_model is configured; using {}.",
                    internal.model_id
                ),
            };
            publish(&request.event_tx, RunEvent::Warning(warning.clone()));
            warnings.push(warning);
        }
        publish(
            &request.event_tx,
            RunEvent::CompactionStarted {
                model: internal.model_id.clone(),
            },
        );
        let before = session.cost_micro;
        let remaining_compaction_budget = request
            .max_credits
            .or(config.max_credits)
            .map(|cap| cap - (session.cost_micro - invocation_start_cost) as f64 / 1_000_000.0);
        session = compact_session(
            &client,
            &store,
            session,
            &internal.model_id,
            remaining_compaction_budget,
        )
        .await?;
        publish(
            &request.event_tx,
            RunEvent::CompactionCompleted {
                model: internal.model_id.clone(),
                cost_micro: session.cost_micro.saturating_sub(before),
            },
        );
    }
    let invocation_spent_micro = session.cost_micro.saturating_sub(invocation_start_cost);
    let invocation_cap_micro = request
        .max_credits
        .or(config.max_credits)
        .map(|value| (value * 1_000_000.0).round() as u64);
    if invocation_cap_micro.is_some_and(|cap| invocation_spent_micro >= cap) {
        return Err("The invocation credit budget was exhausted by context compaction.".into());
    }

    if session.title.is_none() && !request.prompt.trim().is_empty() {
        session.title = request
            .prompt
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map(|line| line.chars().take(80).collect());
    }
    let mut invocation_messages = session.messages.clone();
    if !request.prompt.trim().is_empty() {
        session.pending_turn_base = Some(session.messages.len());
        invocation_messages.push(AgentMessage::user(request.prompt));
    }
    let permissions: Arc<dyn PermissionAuthorizer> = match request.authorizer {
        Some(value) => value,
        None => Arc::new(PermissionController::new(
            request.cwd,
            &home_dir(),
            RoninConfig {
                permission_mode: mode,
                ..config.clone()
            },
            request.dangerous,
            false,
        )?),
    };
    let session_cell = Arc::new(Mutex::new(session.clone()));
    let activity_cell = Arc::new(Mutex::new(session.activity.clone()));
    let change_cell = Arc::new(Mutex::new(Vec::<FileChange>::new()));
    let affected_path_cell = Arc::new(Mutex::new(Vec::<String>::new()));
    let command_cell = Arc::new(Mutex::new(Vec::<String>::new()));
    let checkpoint_cell = session_cell.clone();
    let checkpoint_activity = activity_cell.clone();
    let checkpoint_affected_paths = affected_path_cell.clone();
    let checkpoint_commands = command_cell.clone();
    let checkpoint_store = store.clone();
    let checkpoint_tx = request.event_tx.clone();
    let base_cost = session.cost_micro;
    let base_rounds = session.rounds;
    let on_checkpoint = Arc::new(move |checkpoint: AgentLoopCheckpoint| {
        let mut current = checkpoint_cell
            .lock()
            .expect("session checkpoint lock poisoned");
        current.messages = checkpoint.messages.clone();
        current.cost_micro = base_cost + checkpoint.cost_micro;
        current.rounds = base_rounds + checkpoint.rounds;
        current.active_generation_id = checkpoint.active_generation_id.clone();
        current.last_usage = checkpoint.last_usage.clone();
        current.context_percent = checkpoint.context_percent;
        current.stop_reason = checkpoint.stop_reason.clone();
        current.activity = checkpoint_activity
            .lock()
            .expect("session activity lock poisoned")
            .clone();
        current.last_turn_affected_paths = checkpoint_affected_paths
            .lock()
            .expect("turn affected-path lock poisoned")
            .clone();
        current.last_turn_commands = checkpoint_commands
            .lock()
            .expect("turn command lock poisoned")
            .clone();
        current.state = if current.stop_reason.is_some() {
            SessionState::Completed
        } else {
            SessionState::Interrupted
        };
        if let Ok(saved) = checkpoint_store.save(&current) {
            *current = saved;
        }
        publish(&checkpoint_tx, RunEvent::Checkpoint(checkpoint));
    });
    let agent_tx = request.event_tx.clone();
    let event_activity = activity_cell.clone();
    let event_changes = change_cell.clone();
    let event_affected_paths = affected_path_cell.clone();
    let event_commands = command_cell.clone();
    let pending_commands = Arc::new(Mutex::new(HashMap::<String, String>::new()));
    let on_event = Arc::new(move |event: AgentLoopEvent| {
        if let AgentLoopEvent::ToolStart(name, arguments, call_id) = &event {
            if name == "bash" {
                if let Some(command) = serde_json::from_str::<serde_json::Value>(arguments)
                    .ok()
                    .and_then(|value| value.get("command")?.as_str().map(str::to_owned))
                {
                    pending_commands
                        .lock()
                        .expect("pending command lock poisoned")
                        .insert(call_id.clone(), command);
                }
            }
        }
        if let AgentLoopEvent::ToolEnd(_, _, is_error, call_id, _, metadata) = &event {
            let command = pending_commands
                .lock()
                .expect("pending command lock poisoned")
                .remove(call_id);
            if !is_error {
                if let Some(command) = command {
                    event_commands
                        .lock()
                        .expect("turn command lock poisoned")
                        .push(command);
                }
                let mut affected = event_affected_paths
                    .lock()
                    .expect("turn affected-path lock poisoned");
                for path in &metadata.affected_paths {
                    if !affected.contains(path) {
                        affected.push(path.clone());
                    }
                }
                drop(affected);
                let mut changes = event_changes.lock().expect("turn change lock poisoned");
                for change in &metadata.file_changes {
                    if let Some(existing) = changes.iter_mut().find(|item| item.path == change.path)
                    {
                        existing.after = change.after.clone();
                    } else {
                        changes.push(change.clone());
                    }
                }
            }
        }
        let entry = match &event {
            AgentLoopEvent::ToolEnd(name, output, is_error, call_id, _, _metadata) => {
                Some(crate::storage::SessionActivity {
                    id: call_id.clone(),
                    kind: "tool".into(),
                    label: format!(
                        "{} {}",
                        if *is_error { "Failed" } else { "Completed" },
                        name
                    ),
                    detail: output.chars().take(24_000).collect(),
                    created_at: chrono::Utc::now().to_rfc3339(),
                })
            }
            AgentLoopEvent::Citation(source) => Some(crate::storage::SessionActivity {
                id: uuid::Uuid::new_v4().to_string(),
                kind: "citation".into(),
                label: source.title.clone(),
                detail: source.url.clone(),
                created_at: chrono::Utc::now().to_rfc3339(),
            }),
            _ => None,
        };
        if let Some(entry) = entry {
            let mut activity = event_activity
                .lock()
                .expect("session activity lock poisoned");
            activity.push(entry);
            if activity.len() > 500 {
                let excess = activity.len() - 500;
                activity.drain(..excess);
            }
        }
        publish(&agent_tx, RunEvent::Agent(event));
    });
    let result = run_agent_loop(AgentLoopOptions {
        client: Arc::new(client.clone()),
        model: selected.model_id.clone(),
        messages: invocation_messages,
        tools,
        max_rounds: config.max_rounds,
        max_credits_micro: request
            .max_credits
            .or(config.max_credits)
            .map(|value| (value * 1_000_000.0).round() as u64)
            .map(|cap| cap.saturating_sub(invocation_spent_micro)),
        temperature: None,
        max_output_tokens: None,
        context_window: Some(selected.context_window),
        context_limit_ratio: 0.9,
        session_id: Some(session.provider_session_id.clone()),
        cache_control: true,
        web_search: request.web_search,
        authorizer: Some(permissions),
        cancel: request.cancel.unwrap_or_default(),
        resume_generation_id: session.active_generation_id.clone(),
        on_event: Some(on_event),
        on_checkpoint: Some(on_checkpoint),
    })
    .await
    .map_err(|error| error.to_string())?;

    session = session_cell
        .lock()
        .expect("session result lock poisoned")
        .clone();
    session.model = selected.model_id.clone();
    if session.model_history.last() != Some(&session.model) {
        session.model_history.push(session.model.clone());
    }
    session.messages = result.messages.clone();
    session.active_generation_id = None;
    session.stop_reason = Some(result.stop_reason.clone());
    session.state = SessionState::Completed;
    session.pending_turn_base = None;
    let file_changes = change_cell
        .lock()
        .expect("turn change lock poisoned")
        .clone();
    let affected_paths = affected_path_cell
        .lock()
        .expect("turn affected-path lock poisoned")
        .clone();
    let commands = command_cell
        .lock()
        .expect("turn command lock poisoned")
        .clone();
    session.last_turn_changes = file_changes.clone();
    session.last_turn_affected_paths = affected_paths.clone();
    session.last_turn_commands = commands.clone();
    session = store.save(&session).map_err(|error| error.to_string())?;
    let sync_result = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        client.upsert_agent_session(&session.metadata_for(request.cwd, request.source)),
    )
    .await;
    let sync_error = match sync_result {
        Ok(Ok(_)) => None,
        Ok(Err(error)) => Some(error.to_string()),
        Err(_) => Some("request timed out".into()),
    };
    if let Some(message) = sync_error {
        let warning = RunWarning {
            code: "session_metadata_sync_failed".into(),
            message,
        };
        publish(&request.event_tx, RunEvent::Warning(warning.clone()));
        warnings.push(warning);
    }
    let invocation_cost_micro = session.cost_micro.saturating_sub(invocation_start_cost);
    let code = i32::from(result.stop_reason != AgentStopReason::Complete);
    Ok(RunOutcome {
        code,
        session,
        result,
        invocation_cost_micro,
        warnings,
        file_changes,
        affected_paths,
        commands,
    })
}

pub async fn compact_session(
    client: &RoninApiClient,
    store: &SessionStore,
    mut session: LocalSession,
    model: &str,
    max_credits: Option<f64>,
) -> Result<LocalSession, String> {
    if max_credits.is_some_and(|cap| cap <= 0.0) {
        return Err("The session credit budget is exhausted; compaction cannot run.".into());
    }
    let mut messages = vec![AgentMessage::system("Summarize the coding-agent session for loss-minimized continuation. Preserve user intent, decisions, constraints, files inspected or changed, commands and test outcomes, unresolved work, and exact identifiers. Do not add commentary or Markdown fences.")];
    messages.extend(session.messages.clone());
    messages.push(AgentMessage::user(
        "Return only the durable continuation summary now.",
    ));
    let cancel = CancellationToken::new();
    let stream = client
        .create_completion(
            AgentCompletionRequest {
                model: model.into(),
                messages,
                tools: vec![],
                web_search: None,
                session_id: Some(scoped_session_id(&session.provider_session_id, "compact")),
                cache_control: Some(CacheControl::default()),
                max_output_tokens: Some(4096),
                max_cost_micro: max_credits
                    .map(|value| (value * 1_000_000.0).max(0.0).round() as u64),
                temperature: None,
            },
            &cancel,
        )
        .await
        .map_err(|error| error.to_string())?;
    let mut events = stream.events;
    let mut summary = String::new();
    let mut cost = 0;
    let mut complete = false;
    while let Some(event) = events.next().await {
        match event.map_err(|error| error.to_string())? {
            GenerationStreamEvent::Delta { text } => summary.push_str(&text),
            GenerationStreamEvent::Done {
                cost_micro,
                finish_reason,
                ..
            } => {
                cost = cost_micro.unwrap_or(0);
                complete = finish_reason == FinishReason::Complete;
            }
            GenerationStreamEvent::Error { message, .. } => return Err(message),
            _ => {}
        }
    }
    if !complete || summary.trim().is_empty() {
        return Err("Internal generation did not complete.".into());
    }
    let last_user = session
        .messages
        .iter()
        .rposition(|message| matches!(message, AgentMessage::User { .. }));
    let mut compacted = session.messages.iter().filter(|message| matches!(message, AgentMessage::System { content } if !content.starts_with("[RONIN_COMPACTED_CONTEXT]"))).cloned().collect::<Vec<_>>();
    compacted.push(AgentMessage::system(format!(
        "[RONIN_COMPACTED_CONTEXT]\n{}",
        summary.trim()
    )));
    if let Some(index) = last_user {
        compacted.extend_from_slice(&session.messages[index..]);
    }
    session.messages = compacted;
    session.cost_micro += cost;
    session.compaction_count += 1;
    session.context_percent = None;
    session.last_usage = None;
    session.active_generation_id = None;
    store.save(&session).map_err(|error| error.to_string())
}

fn scoped_session_id(base: &str, scope: &str) -> String {
    let suffix = format!(":{scope}");
    format!(
        "{}{}",
        &base[..base.len().min(256usize.saturating_sub(suffix.len()))],
        suffix
    )
}

fn home_dir() -> std::path::PathBuf {
    directories::UserDirs::new()
        .map(|dirs| dirs.home_dir().to_path_buf())
        .unwrap_or_else(|| ".".into())
}
