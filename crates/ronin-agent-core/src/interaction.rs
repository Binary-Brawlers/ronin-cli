use crate::{AgentTool, AgentToolDefinition, AgentToolKind, DynTool, ToolResult};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::HashSet, sync::Arc};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UserQuestionOption {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UserQuestion {
    pub id: String,
    pub header: String,
    pub question: String,
    pub options: Vec<UserQuestionOption>,
    #[serde(default)]
    pub multi_select: bool,
    #[serde(default)]
    pub allow_other: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UserAnswer {
    pub question_id: String,
    #[serde(default)]
    pub selected_option_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub other_text: Option<String>,
}

#[async_trait]
pub trait UserQuestioner: Send + Sync {
    async fn ask(
        &self,
        questions: Vec<UserQuestion>,
        cancel: &CancellationToken,
    ) -> Result<Vec<UserAnswer>, String>;
}

pub struct UnavailableQuestioner;

#[async_trait]
impl UserQuestioner for UnavailableQuestioner {
    async fn ask(
        &self,
        _questions: Vec<UserQuestion>,
        _cancel: &CancellationToken,
    ) -> Result<Vec<UserAnswer>, String> {
        Err("interactive_input_unavailable: ask_user requires the interactive TUI; continue without it or explain what input is needed".into())
    }
}

pub fn create_ask_user_tool(questioner: Arc<dyn UserQuestioner>) -> DynTool {
    Arc::new(AskUser(questioner))
}

struct AskUser(Arc<dyn UserQuestioner>);

#[async_trait]
impl AgentTool for AskUser {
    fn definition(&self) -> AgentToolDefinition {
        AgentToolDefinition {
            name: "ask_user".into(),
            description: "Ask the user 1-3 concise single-select, multi-select, or free-text questions when their decision is required. Call this tool alone and wait for the answers before requesting any other tool.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "questions": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": 3,
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": { "type": "string", "minLength": 1, "maxLength": 64 },
                                "header": { "type": "string", "minLength": 1, "maxLength": 40 },
                                "question": { "type": "string", "minLength": 1, "maxLength": 500 },
                                "kind": { "type": "string", "enum": ["single_select", "multi_select", "free_text"] },
                                "options": {
                                    "type": "array",
                                    "minItems": 0,
                                    "maxItems": 4,
                                    "items": {
                                        "type": "object",
                                        "properties": {
                                            "id": { "type": "string", "minLength": 1, "maxLength": 64 },
                                            "label": { "type": "string", "minLength": 1, "maxLength": 80 },
                                            "description": { "type": "string", "maxLength": 240 }
                                        },
                                        "required": ["id", "label"],
                                        "additionalProperties": false
                                    }
                                },
                                "multiSelect": { "type": "boolean", "default": false },
                                "allowOther": { "type": "boolean", "default": false }
                            },
                            "required": ["id", "header", "question", "options"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["questions"],
                "additionalProperties": false
            }),
        }
    }

    fn kind(&self) -> AgentToolKind {
        AgentToolKind::Interact
    }

    async fn execute(&self, args: &Value, cancel: &CancellationToken) -> ToolResult {
        let questions = match parse_questions(args) {
            Ok(value) => value,
            Err(message) => return structured_error("invalid_questions", message),
        };
        let answers = tokio::select! {
            _ = cancel.cancelled() => return structured_error("cancelled", "The user cancelled the question."),
            value = self.0.ask(questions.clone(), cancel) => match value {
                Ok(value) => value,
                Err(message) => {
                    let code = if message.starts_with("interactive_input_unavailable:") {
                        "interactive_input_unavailable"
                    } else {
                        "question_failed"
                    };
                    return structured_error(code, message);
                }
            }
        };
        if let Err(message) = validate_answers(&questions, &answers) {
            return structured_error("invalid_answers", message);
        }
        ToolResult::ok(json!({ "answers": answers }).to_string())
    }
}

fn parse_questions(args: &Value) -> Result<Vec<UserQuestion>, String> {
    let raw_questions = args
        .get("questions")
        .cloned()
        .ok_or("questions is required")?;
    let mut questions: Vec<UserQuestion> =
        serde_json::from_value(raw_questions.clone()).map_err(|error| error.to_string())?;
    if let Some(raw) = raw_questions.as_array() {
        for (question, source) in questions.iter_mut().zip(raw) {
            match source.get("kind").and_then(Value::as_str) {
                Some("free_text") => {
                    question.options.clear();
                    question.multi_select = false;
                    question.allow_other = true;
                }
                Some("multi_select") => question.multi_select = true,
                Some("single_select") => question.multi_select = false,
                Some(_) => return Err("unsupported question kind".into()),
                None => {}
            }
        }
    }
    if !(1..=3).contains(&questions.len()) {
        return Err("questions must contain 1-3 items".into());
    }
    let mut question_ids = HashSet::new();
    for question in &questions {
        bounded(&question.id, 64, "question id")?;
        bounded(&question.header, 40, "question header")?;
        bounded(&question.question, 500, "question text")?;
        if !question_ids.insert(&question.id) {
            return Err(format!("duplicate question id: {}", question.id));
        }
        if question.options.is_empty() && question.allow_other {
            continue;
        }
        if !(2..=4).contains(&question.options.len()) {
            return Err(format!("question {} must contain 2-4 options", question.id));
        }
        let mut option_ids = HashSet::new();
        for option in &question.options {
            bounded(&option.id, 64, "option id")?;
            bounded(&option.label, 80, "option label")?;
            if option.description.chars().count() > 240 {
                return Err("option description exceeds 240 characters".into());
            }
            if !option_ids.insert(&option.id) {
                return Err(format!("duplicate option id in question {}", question.id));
            }
        }
    }
    Ok(questions)
}

pub fn validate_answers(questions: &[UserQuestion], answers: &[UserAnswer]) -> Result<(), String> {
    if answers.len() != questions.len() {
        return Err("one answer is required for every question".into());
    }
    let mut answered = HashSet::new();
    for answer in answers {
        if !answered.insert(&answer.question_id) {
            return Err(format!("duplicate answer for {}", answer.question_id));
        }
        let question = questions
            .iter()
            .find(|question| question.id == answer.question_id)
            .ok_or_else(|| format!("unknown question id: {}", answer.question_id))?;
        let valid: HashSet<_> = question
            .options
            .iter()
            .map(|option| option.id.as_str())
            .collect();
        let selected: HashSet<_> = answer.selected_option_ids.iter().collect();
        if selected.len() != answer.selected_option_ids.len() {
            return Err(format!("answer for {} repeats an option", question.id));
        }
        if answer
            .selected_option_ids
            .iter()
            .any(|id| !valid.contains(id.as_str()))
        {
            return Err(format!(
                "answer for {} contains an unknown option",
                question.id
            ));
        }
        let other = answer
            .other_text
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty());
        if other.is_some_and(|value| value.chars().count() > 4_000) {
            return Err(format!(
                "answer for {} exceeds 4000 characters",
                question.id
            ));
        }
        if other.is_some() && !question.allow_other {
            return Err(format!("question {} does not allow Other", question.id));
        }
        let count = answer.selected_option_ids.len() + usize::from(other.is_some());
        if count == 0 || (!question.multi_select && count != 1) {
            return Err(format!(
                "question {} has an invalid selection count",
                question.id
            ));
        }
    }
    Ok(())
}

fn bounded(value: &str, max: usize, name: &str) -> Result<(), String> {
    let len = value.chars().count();
    if len == 0 || len > max {
        Err(format!("{name} must contain 1-{max} characters"))
    } else {
        Ok(())
    }
}

fn structured_error(code: &str, message: impl Into<String>) -> ToolResult {
    ToolResult::error(json!({ "error": { "code": code, "message": message.into() } }).to_string())
}
