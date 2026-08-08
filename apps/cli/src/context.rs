use ronin_agent_core::{detect_secret, AgentMessage};
use std::{fs, path::Path};

const SAFETY_PROMPT: &str = "Filesystem tools are workspace-contained. The bash tool is shallow: commands start in the workspace but can access paths outside it. Request only commands appropriate for the active permission policy.";

#[derive(Debug, Clone)]
pub struct ProjectInstructions {
    pub source: String,
    pub content: String,
}

pub fn load_project_instructions(cwd: &Path) -> Result<Option<ProjectInstructions>, String> {
    for name in ["RONIN.md", "AGENTS.md"] {
        let path = cwd.join(name);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(format!("{name} could not be read: {error}")),
        };
        if let Some(secret) = detect_secret(&text) {
            return Err(format!(
                "{name} was not loaded because it resembles a {secret}."
            ));
        }
        return Ok(Some(ProjectInstructions {
            source: name.into(),
            content: text,
        }));
    }
    Ok(None)
}

pub fn initial_messages(cwd: &Path) -> (Vec<AgentMessage>, Option<String>) {
    let safety = AgentMessage::system(SAFETY_PROMPT);
    match load_project_instructions(cwd) {
        Ok(Some(value)) => (
            vec![
                safety,
                AgentMessage::system(format!(
                    "Project instructions from {}:\n\n{}",
                    value.source, value.content
                )),
            ],
            None,
        ),
        Ok(None) => (vec![safety], None),
        Err(warning) => (vec![safety], Some(warning)),
    }
}
pub fn init_content(cwd: &Path, force: bool) -> Result<String, String> {
    let path = cwd.join("RONIN.md");
    if path.exists() && !force {
        return Err("RONIN.md already exists. Use /init --force to replace it.".into());
    }
    let name = cwd
        .file_name()
        .and_then(|v| v.to_str())
        .unwrap_or("project");
    let content=format!("# {name}\n\n## Project instructions\n\n- Describe the architecture, commands, conventions, and safety constraints here.\n");
    Ok(content)
}

#[cfg(test)]
mod tests {
    use super::load_project_instructions;
    use std::fs;

    #[test]
    fn ronin_instructions_take_precedence_over_agents() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("RONIN.md"), "ronin rules").unwrap();
        fs::write(workspace.path().join("AGENTS.md"), "agent rules").unwrap();
        let value = load_project_instructions(workspace.path())
            .unwrap()
            .unwrap();
        assert_eq!(value.source, "RONIN.md");
        assert_eq!(value.content, "ronin rules");
    }
}
