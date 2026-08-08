use crate::*;
use async_trait::async_trait;
use futures::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use std::{pin::Pin, sync::Arc};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

pub type EventStream =
    Pin<Box<dyn Stream<Item = Result<GenerationStreamEvent, AgentLoopError>> + Send>>;

pub struct CompletionStream {
    pub generation_id: String,
    pub events: EventStream,
}

#[async_trait]
pub trait CompletionClient: Send + Sync {
    async fn create_completion(
        &self,
        request: AgentCompletionRequest,
        cancel: &CancellationToken,
    ) -> Result<CompletionStream, AgentLoopError>;
    async fn resume_completion(
        &self,
        generation_id: &str,
        cancel: &CancellationToken,
    ) -> Result<CompletionStream, AgentLoopError>;
    async fn stop_generation(&self, generation_id: &str) -> Result<(), AgentLoopError>;
}

#[derive(Debug, Error)]
#[error("{message}")]
pub struct AgentLoopError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
}
impl AgentLoopError {
    pub fn new(code: impl Into<String>, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            retryable,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum AgentStopReason {
    Complete,
    MaxRounds,
    BudgetExhausted,
    ContextLimit,
    Aborted,
    Stopped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentLoopEvent {
    RoundStart(u32),
    GenerationStart(u32, String),
    Delta(String),
    Reasoning(String),
    ToolStart(String, String, String),
    ToolEnd(String, String, bool, String, u64, crate::ToolResultMetadata),
    Citation(SourceCitation),
    RoundCost(u64, u64),
    RoundUsage(TokenUsage, Option<f64>),
}

#[derive(Debug, Clone)]
pub struct AgentLoopCheckpoint {
    pub phase: &'static str,
    pub messages: Vec<AgentMessage>,
    pub rounds: u32,
    pub cost_micro: u64,
    pub active_generation_id: Option<String>,
    pub last_usage: Option<TokenUsage>,
    pub context_percent: Option<f64>,
    pub stop_reason: Option<AgentStopReason>,
}

pub struct AgentLoopOptions {
    pub client: Arc<dyn CompletionClient>,
    pub model: String,
    pub messages: Vec<AgentMessage>,
    pub tools: Vec<DynTool>,
    pub max_rounds: u32,
    pub max_credits_micro: Option<u64>,
    pub temperature: Option<f64>,
    pub max_output_tokens: Option<u32>,
    pub context_window: Option<u64>,
    pub context_limit_ratio: f64,
    pub session_id: Option<String>,
    pub cache_control: bool,
    pub web_search: bool,
    pub authorizer: Option<Arc<dyn PermissionAuthorizer>>,
    pub cancel: CancellationToken,
    pub resume_generation_id: Option<String>,
    pub on_event: Option<Arc<dyn Fn(AgentLoopEvent) + Send + Sync>>,
    pub on_checkpoint: Option<Arc<dyn Fn(AgentLoopCheckpoint) + Send + Sync>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentLoopResult {
    pub messages: Vec<AgentMessage>,
    pub final_text: String,
    pub rounds: u32,
    pub cost_micro: u64,
    pub last_usage: Option<TokenUsage>,
    pub context_percent: Option<f64>,
    pub stop_reason: AgentStopReason,
    pub sources: Vec<SourceCitation>,
}

pub async fn run_agent_loop(
    mut options: AgentLoopOptions,
) -> Result<AgentLoopResult, AgentLoopError> {
    let mut messages = std::mem::take(&mut options.messages);
    let mut cost = 0u64;
    let mut final_text = String::new();
    let mut last_usage = None;
    let mut context_percent = None;
    let mut sources = Vec::<SourceCitation>::new();
    let tool_map: std::collections::HashMap<String, DynTool> = options
        .tools
        .iter()
        .map(|t| (t.definition().name.clone(), t.clone()))
        .collect();
    execute_pending(
        &options,
        &tool_map,
        &mut messages,
        0,
        cost,
        &last_usage,
        context_percent,
    )
    .await;
    let mut resume = options.resume_generation_id.take();
    for round in 0..options.max_rounds {
        if options.cancel.is_cancelled() {
            return Ok(finish(
                &options,
                messages,
                final_text,
                round + 1,
                cost,
                last_usage,
                context_percent,
                sources.clone(),
                AgentStopReason::Aborted,
            ));
        }
        let remaining_cost_micro = options
            .max_credits_micro
            .map(|cap| cap.saturating_sub(cost));
        if remaining_cost_micro == Some(0) {
            return Ok(finish(
                &options,
                messages,
                final_text,
                round,
                cost,
                last_usage,
                context_percent,
                sources.clone(),
                AgentStopReason::BudgetExhausted,
            ));
        }
        emit(&options, AgentLoopEvent::RoundStart(round));
        checkpoint(
            &options,
            "before_generation",
            &messages,
            round,
            cost,
            None,
            &last_usage,
            context_percent,
            None,
        );
        let request = AgentCompletionRequest {
            model: options.model.clone(),
            messages: messages.clone(),
            tools: options.tools.iter().map(|t| t.definition()).collect(),
            web_search: options.web_search.then_some(true),
            session_id: options.session_id.clone(),
            cache_control: options.cache_control.then(CacheControl::default),
            max_output_tokens: options.max_output_tokens,
            max_cost_micro: remaining_cost_micro,
            temperature: options.temperature,
        };
        let stream = if let Some(id) = resume.take() {
            options
                .client
                .resume_completion(&id, &options.cancel)
                .await?
        } else {
            options
                .client
                .create_completion(request, &options.cancel)
                .await?
        };
        emit(
            &options,
            AgentLoopEvent::GenerationStart(round, stream.generation_id.clone()),
        );
        checkpoint(
            &options,
            "generation_started",
            &messages,
            round,
            cost,
            Some(stream.generation_id.clone()),
            &last_usage,
            context_percent,
            None,
        );
        let client = options.client.clone();
        let id = stream.generation_id.clone();
        let cancel = options.cancel.clone();
        let stop_task = tokio::spawn(async move {
            cancel.cancelled().await;
            let _ = client.stop_generation(&id).await;
        });
        let mut events = stream.events;
        let mut text = String::new();
        let mut calls = Vec::new();
        let mut reason = FinishReason::Complete;
        while let Some(item) = events.next().await {
            match item? {
                GenerationStreamEvent::Delta { text: delta } => {
                    text.push_str(&delta);
                    emit(&options, AgentLoopEvent::Delta(delta));
                }
                GenerationStreamEvent::Reasoning { text } => {
                    emit(&options, AgentLoopEvent::Reasoning(text))
                }
                GenerationStreamEvent::Citation {
                    url,
                    title,
                    content,
                } => {
                    if !sources.iter().any(|source| source.url == url) {
                        let citation = SourceCitation {
                            url,
                            title,
                            content,
                        };
                        emit(&options, AgentLoopEvent::Citation(citation.clone()));
                        sources.push(citation);
                    }
                }
                GenerationStreamEvent::ToolCall {
                    tool_call_id,
                    tool_name,
                    args,
                } => calls.push(AgentToolCall {
                    id: tool_call_id,
                    name: tool_name,
                    arguments: args,
                }),
                GenerationStreamEvent::Done {
                    usage,
                    cost_micro,
                    finish_reason,
                    ..
                } => {
                    reason = finish_reason;
                    if let Some(u) = usage {
                        context_percent = options
                            .context_window
                            .map(|w| (u.input_tokens + u.output_tokens) as f64 / w as f64);
                        emit(
                            &options,
                            AgentLoopEvent::RoundUsage(u.clone(), context_percent),
                        );
                        last_usage = Some(u);
                    }
                    if let Some(c) = cost_micro {
                        cost += c;
                        emit(&options, AgentLoopEvent::RoundCost(c, cost));
                    }
                }
                GenerationStreamEvent::Error {
                    code,
                    message,
                    retryable,
                } => return Err(AgentLoopError::new(code, message, retryable)),
                GenerationStreamEvent::ToolResult { .. } => {}
            }
        }
        stop_task.abort();
        if !text.is_empty() {
            final_text = text.clone();
        }
        if reason == FinishReason::Stopped || options.cancel.is_cancelled() {
            messages.push(AgentMessage::Assistant {
                content: text,
                tool_calls: None,
            });
            let why = if options.cancel.is_cancelled() {
                AgentStopReason::Aborted
            } else {
                AgentStopReason::Stopped
            };
            return Ok(finish(
                &options,
                messages,
                final_text,
                round + 1,
                cost,
                last_usage,
                context_percent,
                sources.clone(),
                why,
            ));
        }
        if reason != FinishReason::ToolCalls || calls.is_empty() {
            messages.push(AgentMessage::Assistant {
                content: text,
                tool_calls: None,
            });
            return Ok(finish(
                &options,
                messages,
                final_text,
                round + 1,
                cost,
                last_usage,
                context_percent,
                sources.clone(),
                AgentStopReason::Complete,
            ));
        }
        messages.push(AgentMessage::Assistant {
            content: text,
            tool_calls: Some(calls.clone()),
        });
        checkpoint(
            &options,
            "after_round",
            &messages,
            round + 1,
            cost,
            None,
            &last_usage,
            context_percent,
            None,
        );
        let interaction = interaction_call_id(&tool_map, &calls);
        for call in calls {
            let outcome = execute_call(&options, &tool_map, &call, interaction.as_deref()).await;
            messages.push(AgentMessage::Tool {
                tool_call_id: call.id,
                content: outcome,
            });
            checkpoint(
                &options,
                "after_tool",
                &messages,
                round + 1,
                cost,
                None,
                &last_usage,
                context_percent,
                None,
            );
        }
        if options.max_credits_micro.is_some_and(|cap| cost >= cap) {
            return Ok(finish(
                &options,
                messages,
                final_text,
                round + 1,
                cost,
                last_usage,
                context_percent,
                sources.clone(),
                AgentStopReason::BudgetExhausted,
            ));
        }
        if context_percent.is_some_and(|p| p >= options.context_limit_ratio) {
            return Ok(finish(
                &options,
                messages,
                final_text,
                round + 1,
                cost,
                last_usage,
                context_percent,
                sources.clone(),
                AgentStopReason::ContextLimit,
            ));
        }
    }
    Ok(finish(
        &options,
        messages,
        final_text,
        options.max_rounds,
        cost,
        last_usage,
        context_percent,
        sources,
        AgentStopReason::MaxRounds,
    ))
}

async fn execute_call(
    options: &AgentLoopOptions,
    tools: &std::collections::HashMap<String, DynTool>,
    call: &AgentToolCall,
    exclusive_interaction_id: Option<&str>,
) -> String {
    if exclusive_interaction_id.is_some_and(|id| id != call.id) {
        return serde_json::json!({
            "error": {
                "code": "deferred_for_user_input",
                "message": "This tool was not executed because ask_user must run alone. Reconsider and retry it after reading the user's answer."
            }
        })
        .to_string();
    }
    let Some(tool) = tools.get(&call.name) else {
        return format!("Unknown tool: {}", call.name);
    };
    let args = match parse_tool_args(&call.arguments) {
        Ok(v) => v,
        Err(e) => return e,
    };
    if matches!(tool.kind(), AgentToolKind::Edit | AgentToolKind::Shell) {
        let desc = match tool.describe_permission(&args).await {
            Ok(v) => v,
            Err(e) => return format!("Tool authorization failed: {e}"),
        };
        let allowed = match &options.authorizer {
            Some(a) => a.authorize(tool.as_ref(), &args, desc.as_ref()).await,
            None => false,
        };
        if !allowed {
            return "Permission denied for this tool call. Ask before retrying, or take another approach.".into();
        }
    }
    emit(
        options,
        AgentLoopEvent::ToolStart(call.name.clone(), call.arguments.clone(), call.id.clone()),
    );
    let started = std::time::Instant::now();
    let result = tool.execute(&args, &options.cancel).await;
    emit(
        options,
        AgentLoopEvent::ToolEnd(
            call.name.clone(),
            result.result.clone(),
            result.is_error,
            call.id.clone(),
            started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
            result.metadata.clone(),
        ),
    );
    result.result
}

async fn execute_pending(
    options: &AgentLoopOptions,
    tools: &std::collections::HashMap<String, DynTool>,
    messages: &mut Vec<AgentMessage>,
    rounds: u32,
    cost: u64,
    usage: &Option<TokenUsage>,
    context: Option<f64>,
) {
    let Some((idx, calls)) = messages.iter().enumerate().rev().find_map(|(i, m)| {
        if let AgentMessage::Assistant {
            tool_calls: Some(c),
            ..
        } = m
        {
            Some((i, c.clone()))
        } else {
            None
        }
    }) else {
        return;
    };
    let completed: std::collections::HashSet<_> = messages[idx + 1..]
        .iter()
        .filter_map(|m| {
            if let AgentMessage::Tool { tool_call_id, .. } = m {
                Some(tool_call_id.clone())
            } else {
                None
            }
        })
        .collect();
    let exclusive = interaction_call_id(tools, &calls);
    for call in calls {
        if !completed.contains(&call.id) {
            let value = execute_call(options, tools, &call, exclusive.as_deref()).await;
            messages.push(AgentMessage::Tool {
                tool_call_id: call.id,
                content: value,
            });
            checkpoint(
                options,
                "after_tool",
                messages,
                rounds,
                cost,
                None,
                usage,
                context,
                None,
            );
        }
    }
}

fn interaction_call_id(
    tools: &std::collections::HashMap<String, DynTool>,
    calls: &[AgentToolCall],
) -> Option<String> {
    calls.iter().find_map(|call| {
        tools
            .get(&call.name)
            .filter(|tool| tool.kind() == AgentToolKind::Interact)
            .map(|_| call.id.clone())
    })
}

fn emit(o: &AgentLoopOptions, e: AgentLoopEvent) {
    if let Some(f) = &o.on_event {
        f(e);
    }
}
#[allow(clippy::too_many_arguments)]
fn checkpoint(
    o: &AgentLoopOptions,
    phase: &'static str,
    messages: &[AgentMessage],
    rounds: u32,
    cost: u64,
    active: Option<String>,
    usage: &Option<TokenUsage>,
    context: Option<f64>,
    stop: Option<AgentStopReason>,
) {
    if let Some(f) = &o.on_checkpoint {
        f(AgentLoopCheckpoint {
            phase,
            messages: messages.to_vec(),
            rounds,
            cost_micro: cost,
            active_generation_id: active,
            last_usage: usage.clone(),
            context_percent: context,
            stop_reason: stop,
        });
    }
}
#[allow(clippy::too_many_arguments)]
fn finish(
    o: &AgentLoopOptions,
    messages: Vec<AgentMessage>,
    final_text: String,
    rounds: u32,
    cost: u64,
    usage: Option<TokenUsage>,
    context: Option<f64>,
    sources: Vec<SourceCitation>,
    stop: AgentStopReason,
) -> AgentLoopResult {
    checkpoint(
        o,
        "terminal",
        &messages,
        rounds,
        cost,
        None,
        &usage,
        context,
        Some(stop.clone()),
    );
    AgentLoopResult {
        messages,
        final_text,
        rounds,
        cost_micro: cost,
        last_usage: usage,
        context_percent: context,
        stop_reason: stop,
        sources,
    }
}
