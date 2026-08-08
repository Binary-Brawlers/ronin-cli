use async_trait::async_trait;
use ronin_agent_core::*;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

struct FixedQuestioner;

#[async_trait]
impl UserQuestioner for FixedQuestioner {
    async fn ask(
        &self,
        questions: Vec<UserQuestion>,
        _: &CancellationToken,
    ) -> Result<Vec<UserAnswer>, String> {
        Ok(vec![
            UserAnswer {
                question_id: questions[0].id.clone(),
                selected_option_ids: vec!["api".into(), "cli".into()],
                other_text: None,
            },
            UserAnswer {
                question_id: questions[1].id.clone(),
                selected_option_ids: vec![],
                other_text: Some("Keep citations concise".into()),
            },
        ])
    }
}

struct PendingQuestioner;

#[async_trait]
impl UserQuestioner for PendingQuestioner {
    async fn ask(
        &self,
        _: Vec<UserQuestion>,
        _: &CancellationToken,
    ) -> Result<Vec<UserAnswer>, String> {
        std::future::pending().await
    }
}

fn rich_args() -> serde_json::Value {
    serde_json::json!({
        "questions": [
            {
                "id": "scope",
                "header": "Scope",
                "question": "Which surfaces?",
                "multiSelect": true,
                "allowOther": false,
                "options": [
                    { "id": "api", "label": "API", "description": "Server support" },
                    { "id": "cli", "label": "CLI", "description": "Terminal support" }
                ]
            },
            {
                "id": "notes",
                "header": "Notes",
                "question": "Anything else?",
                "allowOther": true,
                "options": [
                    { "id": "none", "label": "Nothing" },
                    { "id": "tests", "label": "More tests" }
                ]
            }
        ]
    })
}

#[tokio::test]
async fn ask_user_returns_valid_structured_answers() {
    let tool = create_ask_user_tool(Arc::new(FixedQuestioner));
    assert_eq!(tool.kind(), AgentToolKind::Interact);
    let result = tool.execute(&rich_args(), &CancellationToken::new()).await;
    assert!(!result.is_error);
    let value: serde_json::Value = serde_json::from_str(&result.result).unwrap();
    assert_eq!(value["answers"][0]["selectedOptionIds"][1], "cli");
    assert_eq!(value["answers"][1]["otherText"], "Keep citations concise");
}

#[tokio::test]
async fn headless_questioner_fails_without_blocking() {
    let tool = create_ask_user_tool(Arc::new(UnavailableQuestioner));
    let result = tool.execute(&rich_args(), &CancellationToken::new()).await;
    assert!(result.is_error);
    assert!(result.result.contains("interactive_input_unavailable"));
}

#[tokio::test]
async fn invalid_batches_are_rejected_before_prompting() {
    let tool = create_ask_user_tool(Arc::new(FixedQuestioner));
    let result = tool
        .execute(
            &serde_json::json!({ "questions": [] }),
            &CancellationToken::new(),
        )
        .await;
    assert!(result.is_error);
    assert!(result.result.contains("invalid_questions"));
}

#[tokio::test]
async fn cancellation_stops_a_pending_question() {
    let tool = create_ask_user_tool(Arc::new(PendingQuestioner));
    let cancel = CancellationToken::new();
    cancel.cancel();
    let result = tool.execute(&rich_args(), &cancel).await;
    assert!(result.is_error);
    assert!(result.result.contains("cancelled"));
}
