use serde::{Deserialize, Serialize};
use std::{
    env, fs,
    io::Write,
    path::{Path, PathBuf},
};
use thiserror::Error;
use toml_edit::{value, DocumentMut};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionMode {
    #[serde(alias = "manual")]
    Default,
    AcceptEdits,
    Plan,
    Auto,
    // Kept for compatibility with existing config files. Interactive sessions
    // only enter this mode through --dangerously-skip-permissions.
    Yolo,
}
impl Default for PermissionMode {
    fn default() -> Self {
        Self::Default
    }
}

impl PermissionMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Default => "Manual",
            Self::AcceptEdits => "Accept edits",
            Self::Plan => "Plan",
            Self::Auto => "Auto",
            Self::Yolo => "Bypass permissions",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Default => "ask before edits and commands",
            Self::AcceptEdits => "apply workspace edits; ask before commands",
            Self::Plan => "read and explore without changing files",
            Self::Auto => "run tools automatically within safety rules",
            Self::Yolo => "skip permission checks",
        }
    }

    pub fn next_interactive(self) -> Self {
        match self {
            Self::Yolo => Self::Default,
            Self::Default => Self::AcceptEdits,
            Self::AcceptEdits => Self::Plan,
            Self::Plan => Self::Auto,
            Self::Auto => Self::Default,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{load_config, update_global_defaults, PermissionMode};
    use std::fs;

    #[test]
    fn interactive_modes_cycle_in_display_order() {
        let mut mode = PermissionMode::Default;
        let labels = (0..4)
            .map(|_| {
                let label = mode.label();
                mode = mode.next_interactive();
                label
            })
            .collect::<Vec<_>>();
        assert_eq!(labels, ["Manual", "Accept edits", "Plan", "Auto"]);
        assert_eq!(mode, PermissionMode::Default);
        assert_eq!(
            PermissionMode::Yolo.next_interactive(),
            PermissionMode::Default
        );
    }

    #[test]
    fn manual_alias_preserves_default_config_compatibility() {
        assert_eq!(
            serde_json::from_str::<PermissionMode>("\"manual\"").unwrap(),
            PermissionMode::Default
        );
        assert_eq!(
            serde_json::from_str::<PermissionMode>("\"default\"").unwrap(),
            PermissionMode::Default
        );
    }

    #[test]
    fn desktop_defaults_preserve_comments_and_unknown_keys() {
        let home = tempfile::tempdir().unwrap();
        fs::create_dir_all(home.path().join(".ronin")).unwrap();
        fs::write(
            home.path().join(".ronin/config.toml"),
            "# keep me\nunknown_future_key = \"value\"\n",
        )
        .unwrap();
        update_global_defaults(home.path(), "openai/gpt-5", PermissionMode::Plan, 12.5).unwrap();
        let raw = fs::read_to_string(home.path().join(".ronin/config.toml")).unwrap();
        assert!(raw.contains("# keep me"));
        assert!(raw.contains("unknown_future_key"));
        let loaded = load_config(home.path(), home.path()).unwrap();
        assert_eq!(loaded.default_model.as_deref(), Some("openai/gpt-5"));
        assert_eq!(loaded.permission_mode, PermissionMode::Plan);
        assert_eq!(loaded.max_credits, Some(12.5));
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RoninConfig {
    pub api_url: String,
    pub default_model: Option<String>,
    pub internal_model: Option<String>,
    pub permission_mode: PermissionMode,
    pub allowed_tools: Vec<String>,
    pub denied_tools: Vec<String>,
    pub allowed_commands: Vec<String>,
    pub denied_commands: Vec<String>,
    pub web_search: bool,
    pub max_credits: Option<f64>,
    pub max_rounds: u32,
}
impl Default for RoninConfig {
    fn default() -> Self {
        Self {
            api_url: "https://chat-api.ronin.africa".into(),
            default_model: None,
            internal_model: None,
            permission_mode: PermissionMode::Default,
            allowed_tools: vec![],
            denied_tools: vec![],
            allowed_commands: vec![],
            denied_commands: vec![],
            web_search: true,
            max_credits: None,
            max_rounds: 50,
        }
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("Invalid TOML in {0}: {1}")]
    Toml(PathBuf, String),
    #[error("Invalid Ronin config: {0}")]
    Invalid(String),
}

pub fn load_config(cwd: &Path, home: &Path) -> Result<RoninConfig, ConfigError> {
    let mut value = toml::Value::try_from(RoninConfig::default()).unwrap();
    merge_file(&mut value, &home.join(".ronin/config.toml"))?;
    merge_file(&mut value, &cwd.join("ronin.toml"))?;
    let table = value.as_table_mut().unwrap();
    for (env_key, key) in [
        ("RONIN_API_URL", "api_url"),
        ("RONIN_MODEL", "default_model"),
        ("RONIN_INTERNAL_MODEL", "internal_model"),
        ("RONIN_PERMISSION_MODE", "permission_mode"),
    ] {
        if let Ok(v) = env::var(env_key) {
            if !v.is_empty() {
                table.insert(key.into(), toml::Value::String(v));
            }
        }
    }
    if let Ok(value) = env::var("RONIN_WEB_SEARCH") {
        if !value.is_empty() {
            let enabled = match value.to_ascii_lowercase().as_str() {
                "1" | "true" | "yes" | "on" => true,
                "0" | "false" | "no" | "off" => false,
                _ => {
                    return Err(ConfigError::Invalid(
                        "web_search: expected true or false".into(),
                    ))
                }
            };
            table.insert("web_search".into(), toml::Value::Boolean(enabled));
        }
    }
    for (env_key, key) in [
        ("RONIN_ALLOWED_TOOLS", "allowed_tools"),
        ("RONIN_DENIED_TOOLS", "denied_tools"),
        ("RONIN_ALLOWED_COMMANDS", "allowed_commands"),
        ("RONIN_DENIED_COMMANDS", "denied_commands"),
    ] {
        if let Ok(v) = env::var(env_key) {
            if !v.is_empty() {
                table.insert(
                    key.into(),
                    toml::Value::Array(
                        v.split(',')
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(|s| toml::Value::String(s.into()))
                            .collect(),
                    ),
                );
            }
        }
    }
    for (env_key, key) in [
        ("RONIN_MAX_CREDITS", "max_credits"),
        ("RONIN_MAX_ROUNDS", "max_rounds"),
    ] {
        if let Ok(v) = env::var(env_key) {
            if !v.is_empty() {
                let n = v
                    .parse::<f64>()
                    .map_err(|_| ConfigError::Invalid(format!("{key}: expected a number")))?;
                table.insert(
                    key.into(),
                    if key == "max_rounds" {
                        toml::Value::Integer(n as i64)
                    } else {
                        toml::Value::Float(n)
                    },
                );
            }
        }
    }
    let config: RoninConfig = value
        .try_into()
        .map_err(|e| ConfigError::Invalid(e.to_string()))?;
    if !(config.api_url.starts_with("http://") || config.api_url.starts_with("https://")) {
        return Err(ConfigError::Invalid(
            "api_url: expected an HTTP(S) URL".into(),
        ));
    }
    if config.max_rounds == 0 || config.max_rounds > 200 {
        return Err(ConfigError::Invalid(
            "max_rounds: must be from 1 to 200".into(),
        ));
    }
    if config.max_credits.is_some_and(|v| v <= 0.0) {
        return Err(ConfigError::Invalid("max_credits: must be positive".into()));
    }
    Ok(config)
}

pub fn update_global_defaults(
    home: &Path,
    model: &str,
    permission_mode: PermissionMode,
    max_credits: f64,
) -> Result<(), ConfigError> {
    if model.trim().is_empty()
        || model.len() > 128
        || !max_credits.is_finite()
        || max_credits <= 0.0
    {
        return Err(ConfigError::Invalid("desktop defaults are invalid".into()));
    }
    let path = home.join(".ronin/config.toml");
    let raw = match fs::read_to_string(&path) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(ConfigError::Toml(path, error.to_string())),
    };
    let mut document = raw
        .parse::<DocumentMut>()
        .map_err(|error| ConfigError::Toml(path.clone(), error.to_string()))?;
    document["default_model"] = value(model);
    document["permission_mode"] = value(match permission_mode {
        PermissionMode::Default => "default",
        PermissionMode::AcceptEdits => "accept-edits",
        PermissionMode::Plan => "plan",
        PermissionMode::Auto => "auto",
        PermissionMode::Yolo => "default",
    });
    document["max_credits"] = value(max_credits);
    fs::create_dir_all(path.parent().expect("config has a parent"))
        .map_err(|error| ConfigError::Toml(path.clone(), error.to_string()))?;
    let mut temp = tempfile::NamedTempFile::new_in(path.parent().expect("config has a parent"))
        .map_err(|error| ConfigError::Toml(path.clone(), error.to_string()))?;
    temp.write_all(document.to_string().as_bytes())
        .map_err(|error| ConfigError::Toml(path.clone(), error.to_string()))?;
    temp.persist(&path)
        .map_err(|error| ConfigError::Toml(path, error.error.to_string()))?;
    Ok(())
}
fn merge_file(base: &mut toml::Value, path: &Path) -> Result<(), ConfigError> {
    let raw = match fs::read_to_string(path) {
        Ok(v) => v,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(ConfigError::Toml(path.into(), e.to_string())),
    };
    let next: toml::Value =
        toml::from_str(&raw).map_err(|e| ConfigError::Toml(path.into(), e.to_string()))?;
    let b = base.as_table_mut().unwrap();
    for (k, v) in next
        .as_table()
        .ok_or_else(|| ConfigError::Toml(path.into(), "root must be a table".into()))?
    {
        b.insert(k.clone(), v.clone());
    }
    Ok(())
}
