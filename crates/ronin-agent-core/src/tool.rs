use crate::AgentToolDefinition;
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentToolKind {
    Read,
    Interact,
    Edit,
    Shell,
}

#[derive(Debug, Clone)]
pub struct ToolResult {
    pub result: String,
    pub is_error: bool,
    pub metadata: ToolResultMetadata,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultMetadata {
    pub exit_code: Option<i32>,
    pub truncated: bool,
    pub affected_paths: Vec<String>,
    pub stdout: Option<String>,
    pub stderr: Option<String>,
}
impl ToolResult {
    pub fn ok(value: impl Into<String>) -> Self {
        Self {
            result: value.into(),
            is_error: false,
            metadata: ToolResultMetadata::default(),
        }
    }
    pub fn error(value: impl Into<String>) -> Self {
        Self {
            result: value.into(),
            is_error: true,
            metadata: ToolResultMetadata::default(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ToolPermissionDescription {
    pub summary: String,
    pub preview: Option<String>,
    pub persistence_key: String,
    pub warning: Option<String>,
}

#[async_trait]
pub trait AgentTool: Send + Sync {
    fn definition(&self) -> AgentToolDefinition;
    fn kind(&self) -> AgentToolKind;
    async fn describe_permission(
        &self,
        _args: &Value,
    ) -> Result<Option<ToolPermissionDescription>, String> {
        Ok(None)
    }
    async fn execute(&self, args: &Value, cancel: &CancellationToken) -> ToolResult;
}

pub type DynTool = Arc<dyn AgentTool>;

pub fn parse_tool_args(raw: &str) -> Result<Value, String> {
    if raw.trim().is_empty() {
        return Ok(serde_json::json!({}));
    }
    serde_json::from_str(raw).map_err(|_| {
        format!(
            "Tool arguments were not valid JSON: {}",
            raw.chars().take(200).collect::<String>()
        )
    })
}

#[async_trait]
pub trait PermissionAuthorizer: Send + Sync {
    async fn authorize(
        &self,
        tool: &dyn AgentTool,
        args: &Value,
        description: Option<&ToolPermissionDescription>,
    ) -> bool;
}
