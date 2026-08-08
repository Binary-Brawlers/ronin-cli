use crate::permissions::PermissionController;
use async_trait::async_trait;
use ronin_agent_core::{AgentTool, PermissionAuthorizer, ToolPermissionDescription};
use serde_json::Value;
use std::{
    io::{self, Write},
    sync::Arc,
};

pub struct TerminalPermissionAuthorizer {
    policy: Arc<PermissionController>,
    interactive: bool,
}

impl TerminalPermissionAuthorizer {
    pub fn new(policy: Arc<PermissionController>, interactive: bool) -> Self {
        Self {
            policy,
            interactive,
        }
    }
}

#[async_trait]
impl PermissionAuthorizer for TerminalPermissionAuthorizer {
    async fn authorize(
        &self,
        tool: &dyn AgentTool,
        _: &Value,
        description: Option<&ToolPermissionDescription>,
    ) -> bool {
        if let Some(decision) = self.policy.preauthorize(tool, description) {
            return decision;
        }
        let Some(description) = description else {
            return false;
        };
        if !self.interactive {
            return false;
        }
        eprintln!("\n{}", description.summary);
        if let Some(warning) = &description.warning {
            eprintln!("{warning}");
        }
        if let Some(preview) = &description.preview {
            eprintln!("{}", preview.trim_end());
        }
        eprint!("Allow? [y]es / [n]o / [a]lways: ");
        let _ = io::stderr().flush();
        let mut answer = String::new();
        if io::stdin().read_line(&mut answer).is_err() {
            return false;
        }
        match answer.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" => true,
            "a" | "always" => self.policy.persist_grant(tool, description).is_ok(),
            _ => false,
        }
    }
}
