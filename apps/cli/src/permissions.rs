use crate::config::{PermissionMode, RoninConfig};
use async_trait::async_trait;
use ronin_agent_core::{
    normalize_command, AgentTool, AgentToolKind, PermissionAuthorizer, ToolPermissionDescription,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::Mutex,
};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceGrants {
    path: String,
    allowed_tools: Vec<String>,
    allowed_commands: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PermissionStore {
    version: u8,
    workspaces: HashMap<String, WorkspaceGrants>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredPermissionGrant {
    pub id: String,
    pub workspace_path: String,
    pub kind: String,
    pub value: String,
}
impl Default for PermissionStore {
    fn default() -> Self {
        Self {
            version: 1,
            workspaces: HashMap::new(),
        }
    }
}

pub struct PermissionController {
    config: RoninConfig,
    dangerous: bool,
    key: String,
    root: String,
    path: PathBuf,
    store: Mutex<PermissionStore>,
    warnings: Mutex<Vec<String>>,
}
impl PermissionController {
    pub fn new(
        cwd: &Path,
        home: &Path,
        config: RoninConfig,
        dangerous: bool,
        _interactive: bool,
    ) -> Result<Self, String> {
        let root = fs::canonicalize(cwd)
            .map_err(|e| e.to_string())?
            .to_string_lossy()
            .into_owned();
        let key = format!("{:x}", Sha256::digest(root.as_bytes()));
        let path = home.join(".ronin/permissions.json");
        let mut warnings = Vec::new();
        let store = match fs::read_to_string(&path) {
            Ok(v) => serde_json::from_str(&v).unwrap_or_else(|_| {
                warnings.push(format!(
                    "Ignoring malformed permission store at {}; no grants were loaded.",
                    path.display()
                ));
                PermissionStore::default()
            }),
            Err(_) => PermissionStore::default(),
        };
        Ok(Self {
            config,
            dangerous,
            key,
            root,
            path,
            store: Mutex::new(store),
            warnings: Mutex::new(warnings),
        })
    }
    fn save(&self, store: &PermissionStore) -> Result<(), String> {
        fs::create_dir_all(self.path.parent().unwrap()).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(
                self.path.parent().unwrap(),
                fs::Permissions::from_mode(0o700),
            )
            .map_err(|e| e.to_string())?;
        }
        let mut tmp = tempfile::NamedTempFile::new_in(self.path.parent().unwrap())
            .map_err(|e| e.to_string())?;
        serde_json::to_writer_pretty(&mut tmp, store).map_err(|e| e.to_string())?;
        writeln!(tmp).map_err(|e| e.to_string())?;
        tmp.persist(&self.path).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&self.path, fs::Permissions::from_mode(0o600))
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }
    pub fn preauthorize(
        &self,
        tool: &dyn AgentTool,
        description: Option<&ToolPermissionDescription>,
    ) -> Option<bool> {
        if self.config.permission_mode == PermissionMode::Yolo {
            return Some(self.dangerous);
        }
        let name = tool.definition().name;
        if self.config.denied_tools.contains(&name) {
            return Some(false);
        }
        if tool.kind() == AgentToolKind::Read {
            return Some(true);
        }
        let desc = description?;
        let command =
            (tool.kind() == AgentToolKind::Shell).then(|| normalize_command(&desc.persistence_key));
        if command
            .as_ref()
            .is_some_and(|c| matches_rule(c, &self.config.denied_commands))
        {
            return Some(false);
        }
        let store = self.store.lock().unwrap();
        let grants = store.workspaces.get(&self.key);
        let allowed = self.config.allowed_tools.contains(&name)
            || command
                .as_ref()
                .is_some_and(|c| matches_rule(c, &self.config.allowed_commands))
            || grants.is_some_and(|g| {
                if tool.kind() == AgentToolKind::Edit {
                    g.allowed_tools.contains(&name)
                } else {
                    command
                        .as_ref()
                        .is_some_and(|c| g.allowed_commands.contains(c))
                }
            });
        if allowed {
            return Some(true);
        }
        match self.config.permission_mode {
            PermissionMode::Plan => Some(false),
            PermissionMode::AcceptEdits if tool.kind() == AgentToolKind::Edit => Some(true),
            PermissionMode::Auto => Some(true),
            PermissionMode::Yolo => unreachable!("handled before permission rules"),
            _ => None,
        }
    }
    pub fn persist_grant(
        &self,
        tool: &dyn AgentTool,
        description: &ToolPermissionDescription,
    ) -> Result<(), String> {
        let name = tool.definition().name;
        let command = (tool.kind() == AgentToolKind::Shell)
            .then(|| normalize_command(&description.persistence_key));
        let mut store = self.store.lock().unwrap();
        let grants = store
            .workspaces
            .entry(self.key.clone())
            .or_insert_with(|| WorkspaceGrants {
                path: self.root.clone(),
                ..Default::default()
            });
        if tool.kind() == AgentToolKind::Edit {
            if !grants.allowed_tools.contains(&name) {
                grants.allowed_tools.push(name);
            }
        } else if let Some(command) = command {
            if !grants.allowed_commands.contains(&command) {
                grants.allowed_commands.push(command);
            }
        }
        let snapshot = store.clone();
        drop(store);
        self.save(&snapshot)
    }

    pub fn take_warnings(&self) -> Vec<String> {
        std::mem::take(
            &mut *self
                .warnings
                .lock()
                .expect("permission warning lock poisoned"),
        )
    }

    pub fn grants(&self) -> Vec<StoredPermissionGrant> {
        let store = self.store.lock().expect("permission store lock poisoned");
        let mut out = Vec::new();
        for (workspace_key, workspace) in &store.workspaces {
            for value in &workspace.allowed_tools {
                out.push(grant(workspace_key, &workspace.path, "tool", value));
            }
            for value in &workspace.allowed_commands {
                out.push(grant(workspace_key, &workspace.path, "command", value));
            }
        }
        out.sort_by(|a, b| {
            a.workspace_path
                .cmp(&b.workspace_path)
                .then(a.value.cmp(&b.value))
        });
        out
    }

    pub fn revoke_grant(&self, id: &str) -> Result<bool, String> {
        let mut store = self.store.lock().expect("permission store lock poisoned");
        let mut removed = false;
        for (workspace_key, workspace) in &mut store.workspaces {
            workspace.allowed_tools.retain(|value| {
                let keep = grant_id(workspace_key, "tool", value) != id;
                removed |= !keep;
                keep
            });
            workspace.allowed_commands.retain(|value| {
                let keep = grant_id(workspace_key, "command", value) != id;
                removed |= !keep;
                keep
            });
        }
        if removed {
            let snapshot = store.clone();
            drop(store);
            self.save(&snapshot)?;
        }
        Ok(removed)
    }

    pub fn reset_workspace(&self) -> Result<usize, String> {
        let mut store = self.store.lock().expect("permission store lock poisoned");
        let removed = store
            .workspaces
            .remove(&self.key)
            .map(|workspace| workspace.allowed_tools.len() + workspace.allowed_commands.len())
            .unwrap_or(0);
        if removed > 0 {
            let snapshot = store.clone();
            drop(store);
            self.save(&snapshot)?;
        }
        Ok(removed)
    }
}

fn grant(workspace_key: &str, path: &str, kind: &str, value: &str) -> StoredPermissionGrant {
    StoredPermissionGrant {
        id: grant_id(workspace_key, kind, value),
        workspace_path: path.into(),
        kind: kind.into(),
        value: value.into(),
    }
}

fn grant_id(workspace_key: &str, kind: &str, value: &str) -> String {
    format!(
        "{:x}",
        Sha256::digest(format!("{workspace_key}:{kind}:{value}").as_bytes())
    )
}
#[async_trait]
impl PermissionAuthorizer for PermissionController {
    async fn authorize(
        &self,
        tool: &dyn AgentTool,
        _: &Value,
        description: Option<&ToolPermissionDescription>,
    ) -> bool {
        if let Some(decision) = self.preauthorize(tool, description) {
            return decision;
        }
        false
    }
}
pub fn matches_rule(command: &str, rules: &[String]) -> bool {
    let command = normalize_command(command);
    rules.iter().any(|rule| {
        let rule = normalize_command(rule);
        !rule.is_empty() && (command == rule || command.starts_with(&format!("{rule} ")))
    })
}
