use async_trait::async_trait;
use futures::stream;
use ronin_agent_core::*;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

struct Scripted {
    rounds: Mutex<Vec<Vec<GenerationStreamEvent>>>,
    requests: Mutex<Vec<AgentCompletionRequest>>,
}
#[async_trait]
impl CompletionClient for Scripted {
    async fn create_completion(
        &self,
        request: AgentCompletionRequest,
        _: &CancellationToken,
    ) -> Result<CompletionStream, AgentLoopError> {
        self.requests.lock().unwrap().push(request);
        let events = self.rounds.lock().unwrap().remove(0).into_iter().map(Ok);
        Ok(CompletionStream {
            generation_id: "g".into(),
            events: Box::pin(stream::iter(events)),
        })
    }
    async fn resume_completion(
        &self,
        _: &str,
        cancel: &CancellationToken,
    ) -> Result<CompletionStream, AgentLoopError> {
        self.create_completion(
            AgentCompletionRequest {
                model: String::new(),
                messages: vec![],
                tools: vec![],
                web_search: None,
                session_id: None,
                cache_control: None,
                max_output_tokens: None,
                max_cost_micro: None,
                temperature: None,
            },
            cancel,
        )
        .await
    }
    async fn stop_generation(&self, _: &str) -> Result<(), AgentLoopError> {
        Ok(())
    }
}

struct EchoTool;
#[async_trait]
impl AgentTool for EchoTool {
    fn definition(&self) -> AgentToolDefinition {
        AgentToolDefinition {
            name: "echo".into(),
            description: String::new(),
            parameters: serde_json::json!({"type":"object"}),
        }
    }
    fn kind(&self) -> AgentToolKind {
        AgentToolKind::Read
    }
    async fn execute(&self, args: &serde_json::Value, _: &CancellationToken) -> ToolResult {
        ToolResult::ok(args["value"].as_str().unwrap())
    }
}

#[tokio::test]
async fn executes_tool_round_then_completes() {
    let client = Arc::new(Scripted {
        requests: Mutex::new(vec![]),
        rounds: Mutex::new(vec![
            vec![
                GenerationStreamEvent::ToolCall {
                    tool_call_id: "1".into(),
                    tool_name: "echo".into(),
                    args: "{\"value\":\"ok\"}".into(),
                },
                GenerationStreamEvent::Done {
                    message_id: None,
                    usage: None,
                    cost_micro: Some(5),
                    finish_reason: FinishReason::ToolCalls,
                },
            ],
            vec![
                GenerationStreamEvent::Citation {
                    url: "https://example.com/source".into(),
                    title: "Example".into(),
                    content: None,
                },
                GenerationStreamEvent::Citation {
                    url: "https://example.com/source".into(),
                    title: "Duplicate".into(),
                    content: None,
                },
                GenerationStreamEvent::Delta {
                    text: "finished".into(),
                },
                GenerationStreamEvent::Done {
                    message_id: None,
                    usage: None,
                    cost_micro: Some(7),
                    finish_reason: FinishReason::Complete,
                },
            ],
        ]),
    });
    let result = run_agent_loop(AgentLoopOptions {
        client,
        model: "m".into(),
        messages: vec![AgentMessage::user("go")],
        tools: vec![Arc::new(EchoTool)],
        max_rounds: 5,
        max_credits_micro: None,
        temperature: None,
        max_output_tokens: None,
        context_window: None,
        context_limit_ratio: 0.9,
        session_id: None,
        cache_control: true,
        web_search: false,
        authorizer: None,
        cancel: CancellationToken::new(),
        resume_generation_id: None,
        on_event: None,
        on_checkpoint: None,
    })
    .await
    .unwrap();
    assert_eq!(result.final_text, "finished");
    assert_eq!(result.cost_micro, 12);
    assert_eq!(result.stop_reason, AgentStopReason::Complete);
    assert_eq!(result.sources.len(), 1);
    assert!(
        matches!(result.messages[2], AgentMessage::Tool { ref content, .. } if content == "ok")
    );
}

struct OneAnswer;

#[async_trait]
impl UserQuestioner for OneAnswer {
    async fn ask(
        &self,
        questions: Vec<UserQuestion>,
        _: &CancellationToken,
    ) -> Result<Vec<UserAnswer>, String> {
        Ok(vec![UserAnswer {
            question_id: questions[0].id.clone(),
            selected_option_ids: vec!["yes".into()],
            other_text: None,
        }])
    }
}

#[tokio::test]
async fn interaction_call_defers_sibling_tools() {
    let client = Arc::new(Scripted {
        requests: Mutex::new(vec![]),
        rounds: Mutex::new(vec![
            vec![
                GenerationStreamEvent::ToolCall {
                    tool_call_id: "echo-call".into(),
                    tool_name: "echo".into(),
                    args: "{\"value\":\"must not run\"}".into(),
                },
                GenerationStreamEvent::ToolCall {
                    tool_call_id: "question-call".into(),
                    tool_name: "ask_user".into(),
                    args: serde_json::json!({
                        "questions": [{
                            "id": "confirm",
                            "header": "Confirm",
                            "question": "Continue?",
                            "options": [
                                { "id": "yes", "label": "Yes" },
                                { "id": "no", "label": "No" }
                            ]
                        }]
                    })
                    .to_string(),
                },
                GenerationStreamEvent::Done {
                    message_id: None,
                    usage: None,
                    cost_micro: Some(1),
                    finish_reason: FinishReason::ToolCalls,
                },
            ],
            vec![GenerationStreamEvent::Done {
                message_id: None,
                usage: None,
                cost_micro: Some(1),
                finish_reason: FinishReason::Complete,
            }],
        ]),
    });
    let result = run_agent_loop(AgentLoopOptions {
        client,
        model: "m".into(),
        messages: vec![AgentMessage::user("go")],
        tools: vec![
            Arc::new(EchoTool),
            create_ask_user_tool(Arc::new(OneAnswer)),
        ],
        max_rounds: 5,
        max_credits_micro: None,
        temperature: None,
        max_output_tokens: None,
        context_window: None,
        context_limit_ratio: 0.9,
        session_id: None,
        cache_control: false,
        web_search: false,
        authorizer: None,
        cancel: CancellationToken::new(),
        resume_generation_id: None,
        on_event: None,
        on_checkpoint: None,
    })
    .await
    .unwrap();
    assert!(matches!(
        &result.messages[2],
        AgentMessage::Tool { content, .. } if content.contains("deferred_for_user_input")
    ));
    assert!(matches!(
        &result.messages[3],
        AgentMessage::Tool { content, .. } if content.contains("selectedOptionIds")
    ));
}

#[tokio::test]
async fn resumed_pending_interaction_is_represented_and_completed() {
    let client = Arc::new(Scripted {
        requests: Mutex::new(vec![]),
        rounds: Mutex::new(vec![vec![GenerationStreamEvent::Done {
            message_id: None,
            usage: None,
            cost_micro: Some(1),
            finish_reason: FinishReason::Complete,
        }]]),
    });
    let pending = AgentToolCall {
        id: "question-call".into(),
        name: "ask_user".into(),
        arguments: serde_json::json!({
            "questions": [{
                "id": "confirm",
                "header": "Confirm",
                "question": "Continue?",
                "options": [
                    { "id": "yes", "label": "Yes" },
                    { "id": "no", "label": "No" }
                ]
            }]
        })
        .to_string(),
    };
    let result = run_agent_loop(AgentLoopOptions {
        client,
        model: "m".into(),
        messages: vec![
            AgentMessage::user("go"),
            AgentMessage::Assistant {
                content: String::new(),
                tool_calls: Some(vec![pending]),
            },
        ],
        tools: vec![create_ask_user_tool(Arc::new(OneAnswer))],
        max_rounds: 2,
        max_credits_micro: None,
        temperature: None,
        max_output_tokens: None,
        context_window: None,
        context_limit_ratio: 0.9,
        session_id: None,
        cache_control: false,
        web_search: false,
        authorizer: None,
        cancel: CancellationToken::new(),
        resume_generation_id: None,
        on_event: None,
        on_checkpoint: None,
    })
    .await
    .unwrap();
    assert!(matches!(
        &result.messages[2],
        AgentMessage::Tool { content, .. } if content.contains("selectedOptionIds")
    ));
}

#[tokio::test]
async fn budget_stops_between_rounds() {
    let client = Arc::new(Scripted {
        requests: Mutex::new(vec![]),
        rounds: Mutex::new(vec![vec![
            GenerationStreamEvent::ToolCall {
                tool_call_id: "1".into(),
                tool_name: "echo".into(),
                args: "{}".into(),
            },
            GenerationStreamEvent::Done {
                message_id: None,
                usage: None,
                cost_micro: Some(10),
                finish_reason: FinishReason::ToolCalls,
            },
        ]]),
    });
    let result = run_agent_loop(AgentLoopOptions {
        client: client.clone(),
        model: "m".into(),
        messages: vec![AgentMessage::user("go")],
        tools: vec![],
        max_rounds: 5,
        max_credits_micro: Some(10),
        temperature: None,
        max_output_tokens: None,
        context_window: None,
        context_limit_ratio: 0.9,
        session_id: None,
        cache_control: false,
        web_search: false,
        authorizer: None,
        cancel: CancellationToken::new(),
        resume_generation_id: None,
        on_event: None,
        on_checkpoint: None,
    })
    .await
    .unwrap();
    assert_eq!(client.requests.lock().unwrap()[0].max_cost_micro, Some(10));
    assert_eq!(result.stop_reason, AgentStopReason::BudgetExhausted);
}
