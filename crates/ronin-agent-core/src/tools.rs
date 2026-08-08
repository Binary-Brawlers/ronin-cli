use crate::*;
use async_trait::async_trait;
use ignore::WalkBuilder;
use regex::Regex;
use serde_json::{json, Value};
use similar::TextDiff;
use std::{
    fs,
    io::Write,
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{io::AsyncReadExt, process::Command};
use tokio_util::sync::CancellationToken;

const MAX_RESULT_CHARS: usize = 30_000;
const MAX_FILE_BYTES: u64 = 5 * 1024 * 1024;
const SHELL_WARNING: &str = "Warning: the shell is shallowly contained. Commands start inside the workspace but can access paths outside it.";

#[derive(Clone)]
pub struct WorkspacePolicy {
    root: PathBuf,
}

impl WorkspacePolicy {
    pub fn new(cwd: impl AsRef<Path>) -> Result<Self, String> {
        Ok(Self {
            root: fs::canonicalize(cwd).map_err(|e| e.to_string())?,
        })
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn resolve_existing(
        &self,
        value: &str,
        directory: bool,
    ) -> Result<(PathBuf, String), String> {
        if secret_path(value) {
            return Err("Secret or protected files cannot be accessed.".into());
        }
        let candidate = if Path::new(value).is_absolute() {
            PathBuf::from(value)
        } else {
            self.root.join(value)
        };
        let actual = fs::canonicalize(&candidate).map_err(|e| e.to_string())?;
        if !actual.starts_with(&self.root) {
            return Err("Path escapes the workspace.".into());
        }
        let meta = fs::metadata(&actual).map_err(|e| e.to_string())?;
        if directory != meta.is_dir() {
            return Err(if directory {
                "Expected a directory."
            } else {
                "Expected a file."
            }
            .into());
        }
        if self.is_ignored(&actual) {
            return Err("Path is ignored by .gitignore or .roninignore.".into());
        }
        Ok((actual.clone(), relative(&self.root, &actual)))
    }
    pub fn resolve_write(&self, value: &str) -> Result<(PathBuf, String), String> {
        if value.trim().is_empty() || secret_path(value) {
            return Err("Secret, protected, or empty paths cannot be written.".into());
        }
        let path = Path::new(value);
        if path.components().any(|c| matches!(c, Component::ParentDir)) {
            return Err("Path traversal is not allowed.".into());
        }
        let candidate = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };
        let relative_path = candidate
            .strip_prefix(&self.root)
            .map_err(|_| "Path escapes the workspace.")?
            .to_path_buf();
        let mut cursor = self.root.clone();
        for part in relative_path.components() {
            cursor.push(part);
            if cursor.exists()
                && fs::symlink_metadata(&cursor)
                    .map_err(|e| e.to_string())?
                    .file_type()
                    .is_symlink()
            {
                return Err("Traversal through symlinks is not allowed for writes.".into());
            }
        }
        if self.is_ignored(&candidate) {
            return Err("Path is ignored by .gitignore or .roninignore.".into());
        }
        Ok((candidate.clone(), relative(&self.root, &candidate)))
    }
    fn is_ignored(&self, path: &Path) -> bool {
        let mut patterns = ignore::gitignore::GitignoreBuilder::new(&self.root);
        let mut walker = WalkBuilder::new(&self.root);
        walker
            .hidden(false)
            .git_ignore(false)
            .git_exclude(false)
            .parents(false);
        for entry in walker.build().filter_map(Result::ok) {
            if entry.file_type().is_some_and(|kind| kind.is_file())
                && matches!(
                    entry.file_name().to_str(),
                    Some(".gitignore" | ".roninignore")
                )
            {
                let _ = patterns.add(entry.path());
            }
        }
        patterns.build().is_ok_and(|rules| {
            rules
                .matched_path_or_any_parents(path, path.is_dir())
                .is_ignore()
        })
    }
}

pub fn create_read_only_tools(cwd: impl AsRef<Path>) -> Result<Vec<DynTool>, String> {
    let policy = Arc::new(WorkspacePolicy::new(cwd)?);
    Ok(vec![
        Arc::new(ReadFile(policy.clone())),
        Arc::new(ListDir(policy.clone())),
        Arc::new(Glob(policy.clone())),
        Arc::new(Grep(policy)),
    ])
}
pub fn create_mutation_tools(cwd: impl AsRef<Path>) -> Result<Vec<DynTool>, String> {
    let policy = Arc::new(WorkspacePolicy::new(cwd)?);
    Ok(vec![
        Arc::new(Mutation::new(policy.clone(), MutationKind::Write)),
        Arc::new(Mutation::new(policy.clone(), MutationKind::Edit)),
        Arc::new(Mutation::new(policy, MutationKind::MultiEdit)),
    ])
}
pub fn create_bash_tool(cwd: impl AsRef<Path>) -> Result<DynTool, String> {
    Ok(Arc::new(Bash(Arc::new(WorkspacePolicy::new(cwd)?))))
}

struct ReadFile(Arc<WorkspacePolicy>);
#[async_trait]
impl AgentTool for ReadFile {
    fn definition(&self) -> AgentToolDefinition {
        definition(
            "read_file",
            "Read a UTF-8 text file inside the workspace with one-based line offsets.",
            json!({"type":"object","properties":{"path":{"type":"string"},"offset":{"type":"integer","minimum":1,"default":1},"limit":{"type":"integer","minimum":1,"maximum":1000,"default":200}},"required":["path"],"additionalProperties":false}),
        )
    }
    fn kind(&self) -> AgentToolKind {
        AgentToolKind::Read
    }
    async fn execute(&self, args: &Value, _: &CancellationToken) -> ToolResult {
        let run = || -> Result<String, String> {
            let path = string(args, "path")?;
            let offset = integer(args, "offset", 1, 1, usize::MAX)?;
            let limit = integer(args, "limit", 200, 1, 1000)?;
            let (abs, rel) = self.0.resolve_existing(path, false)?;
            let meta = fs::metadata(&abs).map_err(|e| e.to_string())?;
            if meta.len() > MAX_FILE_BYTES {
                return Err("File exceeds the 5 MiB read limit.".into());
            }
            let bytes = fs::read(abs).map_err(|e| e.to_string())?;
            if bytes.contains(&0) {
                return Err("Binary files cannot be returned by read_file.".into());
            }
            let text =
                String::from_utf8(bytes).map_err(|_| "File is not valid UTF-8.".to_string())?;
            if let Some(s) = detect_secret(&text) {
                return Err(format!(
                    "File content was withheld because it resembles a {s}."
                ));
            }
            let lines: Vec<_> = text.lines().collect();
            if offset > lines.len() {
                return Ok(format!(
                    "{rel} has {} line(s); offset {offset} is past EOF.",
                    lines.len()
                ));
            }
            let end = (offset - 1 + limit).min(lines.len());
            let width = end.to_string().len();
            let body = lines[offset - 1..end]
                .iter()
                .enumerate()
                .map(|(i, l)| format!("{:>width$}: {l}", offset + i, width = width))
                .collect::<Vec<_>>()
                .join("\n");
            let more = if end < lines.len() {
                format!("\n[{} more line(s)]", lines.len() - end)
            } else {
                String::new()
            };
            Ok(truncate(format!(
                "{rel} (lines {offset}-{end} of {})\n{body}{more}",
                lines.len()
            )))
        };
        result(run())
    }
}

struct ListDir(Arc<WorkspacePolicy>);
#[async_trait]
impl AgentTool for ListDir {
    fn definition(&self) -> AgentToolDefinition {
        definition(
            "list_dir",
            "List one directory inside the workspace. Directories end with / and symlinks with @.",
            json!({"type":"object","properties":{"path":{"type":"string","default":"."}},"additionalProperties":false}),
        )
    }
    fn kind(&self) -> AgentToolKind {
        AgentToolKind::Read
    }
    async fn execute(&self, args: &Value, _: &CancellationToken) -> ToolResult {
        let run = || -> Result<String, String> {
            let path = args.get("path").and_then(Value::as_str).unwrap_or(".");
            let (abs, rel) = self.0.resolve_existing(path, true)?;
            let mut names = Vec::new();
            for entry in fs::read_dir(abs).map_err(|e| e.to_string())? {
                let e = entry.map_err(|e| e.to_string())?;
                let p = e.path();
                if secret_path(&p.to_string_lossy()) || self.0.is_ignored(&p) {
                    continue;
                }
                let ft = e.file_type().map_err(|e| e.to_string())?;
                let suffix = if ft.is_dir() {
                    "/"
                } else if ft.is_symlink() {
                    "@"
                } else {
                    ""
                };
                names.push(format!("{}{}", e.file_name().to_string_lossy(), suffix));
            }
            names.sort();
            Ok(truncate(format!(
                "{}\n{}",
                if rel.is_empty() { "." } else { &rel },
                names.join("\n")
            )))
        };
        result(run())
    }
}

struct Glob(Arc<WorkspacePolicy>);
#[async_trait]
impl AgentTool for Glob {
    fn definition(&self) -> AgentToolDefinition {
        definition(
            "glob",
            "Find workspace files whose relative paths match a glob pattern.",
            json!({"type":"object","properties":{"pattern":{"type":"string"}},"required":["pattern"],"additionalProperties":false}),
        )
    }
    fn kind(&self) -> AgentToolKind {
        AgentToolKind::Read
    }
    async fn execute(&self, args: &Value, _: &CancellationToken) -> ToolResult {
        let pattern = match string(args, "pattern") {
            Ok(v) => v,
            Err(e) => return ToolResult::error(e),
        };
        let matcher = match ignore::overrides::OverrideBuilder::new(self.0.root())
            .add(pattern)
            .and_then(|b| b.build())
        {
            Ok(v) => v,
            Err(e) => return ToolResult::error(e.to_string()),
        };
        let mut b = WalkBuilder::new(self.0.root());
        b.hidden(false)
            .add_custom_ignore_filename(".roninignore")
            .overrides(matcher);
        let mut out: Vec<_> = b
            .build()
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
            .map(|e| relative(self.0.root(), e.path()))
            .filter(|p| !secret_path(p))
            .collect();
        out.sort();
        ToolResult::ok(truncate(if out.is_empty() {
            "No files matched.".into()
        } else {
            out.join("\n")
        }))
    }
}

struct Grep(Arc<WorkspacePolicy>);
#[async_trait]
impl AgentTool for Grep {
    fn definition(&self) -> AgentToolDefinition {
        definition(
            "grep",
            "Search UTF-8 workspace files with a regular expression.",
            json!({"type":"object","properties":{"pattern":{"type":"string"},"path":{"type":"string","default":"."},"ignore_case":{"type":"boolean","default":false}},"required":["pattern"],"additionalProperties":false}),
        )
    }
    fn kind(&self) -> AgentToolKind {
        AgentToolKind::Read
    }
    async fn execute(&self, args: &Value, cancel: &CancellationToken) -> ToolResult {
        let pattern = match string(args, "pattern") {
            Ok(v) => v,
            Err(e) => return ToolResult::error(e),
        };
        let source = if args
            .get("ignore_case")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            format!("(?i){pattern}")
        } else {
            pattern.into()
        };
        let re = match Regex::new(&source) {
            Ok(v) => v,
            Err(e) => return ToolResult::error(e.to_string()),
        };
        let path = args.get("path").and_then(Value::as_str).unwrap_or(".");
        let (abs, _) = match self.0.resolve_existing(path, true) {
            Ok(v) => v,
            Err(e) => return ToolResult::error(e),
        };
        let mut b = WalkBuilder::new(abs);
        b.hidden(false).add_custom_ignore_filename(".roninignore");
        let mut out = Vec::new();
        for e in b.build().filter_map(Result::ok) {
            if cancel.is_cancelled() {
                return ToolResult::error("grep aborted.");
            }
            if !e.file_type().is_some_and(|t| t.is_file())
                || secret_path(&e.path().to_string_lossy())
            {
                continue;
            }
            if let Ok(text) = fs::read_to_string(e.path()) {
                for (i, line) in text.lines().enumerate() {
                    if re.is_match(line) {
                        out.push(format!(
                            "{}:{}:{}",
                            relative(self.0.root(), e.path()),
                            i + 1,
                            line
                        ));
                        if out.len() >= 1000 {
                            break;
                        }
                    }
                }
            }
            if out.len() >= 1000 {
                break;
            }
        }
        ToolResult::ok(truncate(if out.is_empty() {
            "No matches found.".into()
        } else {
            out.join("\n")
        }))
    }
}

#[derive(Clone, Copy)]
enum MutationKind {
    Write,
    Edit,
    MultiEdit,
}
struct Mutation {
    policy: Arc<WorkspacePolicy>,
    kind: MutationKind,
}
impl Mutation {
    fn new(policy: Arc<WorkspacePolicy>, kind: MutationKind) -> Self {
        Self { policy, kind }
    }
    fn name(&self) -> &'static str {
        match self.kind {
            MutationKind::Write => "write_file",
            MutationKind::Edit => "edit_file",
            MutationKind::MultiEdit => "multi_edit",
        }
    }
    fn plan(&self, args: &Value) -> Result<(PathBuf, String, String, String), String> {
        let path = string(args, "path")?;
        let (abs, rel) = self.policy.resolve_write(path)?;
        let before = if abs.exists() {
            fs::read_to_string(&abs).map_err(|e| e.to_string())?
        } else {
            String::new()
        };
        let after = match self.kind {
            MutationKind::Write => string(args, "content")?.to_string(),
            MutationKind::Edit => replace_once(
                &before,
                string(args, "old_string")?,
                string(args, "new_string")?,
            )?,
            MutationKind::MultiEdit => {
                let edits = args
                    .get("edits")
                    .and_then(Value::as_array)
                    .ok_or("edits must contain between 1 and 100 replacements.")?;
                if edits.is_empty() || edits.len() > 100 {
                    return Err("edits must contain between 1 and 100 replacements.".into());
                }
                let mut value = before.clone();
                for (index, e) in edits.iter().enumerate() {
                    value = replace_once(&value, string(e, "old_string")?, string(e, "new_string")?)
                        .map_err(|x| format!("Edit {}: {x}", index + 1))?
                }
                value
            }
        };
        if let Some(s) = detect_secret(&after) {
            return Err(format!(
                "Refusing to write content containing a detected {s}."
            ));
        }
        let diff = TextDiff::from_lines(&before, &after)
            .unified_diff()
            .header(&format!("a/{rel}"), &format!("b/{rel}"))
            .to_string();
        Ok((abs, rel, after, diff))
    }
}
#[async_trait]
impl AgentTool for Mutation {
    fn definition(&self) -> AgentToolDefinition {
        let props = match self.kind {
            MutationKind::Write => json!({"path":{"type":"string"},"content":{"type":"string"}}),
            MutationKind::Edit => {
                json!({"path":{"type":"string"},"old_string":{"type":"string"},"new_string":{"type":"string"}})
            }
            MutationKind::MultiEdit => json!({"path":{"type":"string"},"edits":{"type":"array"}}),
        };
        definition(
            self.name(),
            "Modify a text file inside the workspace.",
            json!({"type":"object","properties":props}),
        )
    }
    fn kind(&self) -> AgentToolKind {
        AgentToolKind::Edit
    }
    async fn describe_permission(
        &self,
        args: &Value,
    ) -> Result<Option<ToolPermissionDescription>, String> {
        let (_, rel, _, preview) = self.plan(args)?;
        Ok(Some(ToolPermissionDescription {
            summary: format!("{} will modify {rel}", self.name()),
            preview: Some(preview),
            persistence_key: self.name().into(),
            warning: None,
        }))
    }
    async fn execute(&self, args: &Value, _: &CancellationToken) -> ToolResult {
        let run = || -> Result<(String, String), String> {
            let (abs, rel, after, _) = self.plan(args)?;
            let prior_permissions = fs::metadata(&abs).ok().map(|meta| meta.permissions());
            if let Some(parent) = abs.parent() {
                fs::create_dir_all(parent).map_err(|e| e.to_string())?
            }
            let mut tmp = tempfile::NamedTempFile::new_in(abs.parent().unwrap())
                .map_err(|e| e.to_string())?;
            tmp.write_all(after.as_bytes()).map_err(|e| e.to_string())?;
            if let Some(permissions) = prior_permissions {
                tmp.as_file()
                    .set_permissions(permissions)
                    .map_err(|e| e.to_string())?;
            }
            tmp.persist(&abs).map_err(|e| e.to_string())?;
            Ok((format!("{} updated {rel}.", self.name()), rel))
        };
        match run() {
            Ok((message, path)) => ToolResult {
                result: message,
                is_error: false,
                metadata: crate::ToolResultMetadata {
                    affected_paths: vec![path],
                    ..Default::default()
                },
            },
            Err(error) => ToolResult::error(error),
        }
    }
}

struct Bash(Arc<WorkspacePolicy>);
#[async_trait]
impl AgentTool for Bash {
    fn definition(&self) -> AgentToolDefinition {
        definition(
            "bash",
            &format!("Run a shell command. {SHELL_WARNING}"),
            json!({"type":"object","properties":{"command":{"type":"string"},"cwd":{"type":"string"},"timeout_seconds":{"type":"integer","minimum":1,"maximum":120,"default":30}},"required":["command"],"additionalProperties":false}),
        )
    }
    fn kind(&self) -> AgentToolKind {
        AgentToolKind::Shell
    }
    async fn describe_permission(
        &self,
        args: &Value,
    ) -> Result<Option<ToolPermissionDescription>, String> {
        let cmd = string(args, "command")?;
        let cwd = args.get("cwd").and_then(Value::as_str).unwrap_or(".");
        self.0.resolve_existing(cwd, true)?;
        Ok(Some(ToolPermissionDescription {
            summary: format!("Run in {cwd}: {cmd}"),
            preview: Some(format!("$ {cmd}")),
            persistence_key: normalize_command(cmd),
            warning: Some(SHELL_WARNING.into()),
        }))
    }
    async fn execute(&self, args: &Value, cancel: &CancellationToken) -> ToolResult {
        let command = match string(args, "command") {
            Ok(v) => v,
            Err(e) => return ToolResult::error(e),
        };
        if let Some(s) = detect_secret(command) {
            return ToolResult::error(format!(
                "Refusing to run a command containing a detected {s}."
            ));
        }
        let timeout = match integer(args, "timeout_seconds", 30, 1, 120) {
            Ok(v) => v,
            Err(e) => return ToolResult::error(e),
        };
        let cwd = args.get("cwd").and_then(Value::as_str).unwrap_or(".");
        let (abs, _) = match self.0.resolve_existing(cwd, true) {
            Ok(v) => v,
            Err(e) => return ToolResult::error(e),
        };
        #[cfg(windows)]
        let mut process = {
            let mut process = Command::new("powershell.exe");
            process.args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command"]);
            process
        };
        #[cfg(not(windows))]
        let mut process = {
            let mut process = Command::new("/bin/sh");
            process.arg("-c");
            process
        };
        let mut child = match process
            .arg(command)
            .current_dir(abs)
            .kill_on_drop(true)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
        {
            Ok(v) => v,
            Err(e) => return ToolResult::error(format!("Failed to start command: {e}")),
        };
        let mut stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        let wait = async {
            let mut out = Vec::new();
            let mut err = Vec::new();
            let read = tokio::try_join!(stdout.read_to_end(&mut out), stderr.read_to_end(&mut err));
            let status = child.wait().await;
            (read, status, out, err)
        };
        let outcome = tokio::select! {_=cancel.cancelled()=>{let _=child.kill().await;return ToolResult::error("Command aborted.")},value=tokio::time::timeout(Duration::from_secs(timeout as u64),wait)=>value};
        let Ok((read, status, out, err)) = outcome else {
            return ToolResult::error("Command timed out.");
        };
        if let Err(e) = read {
            return ToolResult::error(e.to_string());
        }
        let status = match status {
            Ok(v) => v,
            Err(e) => return ToolResult::error(e.to_string()),
        };
        let output_truncated = out.len() > 1024 * 1024 || err.len() > 1024 * 1024;
        let out = String::from_utf8_lossy(&out[..out.len().min(1024 * 1024)]);
        let err = String::from_utf8_lossy(&err[..err.len().min(1024 * 1024)]);
        if let Some(s) = detect_secret(&format!("{out}\n{err}")) {
            return ToolResult::error(format!(
                "Command output withheld because it contained a detected {s}. Exit code: {:?}.",
                status.code()
            ));
        }
        let text = truncate(format!(
            "Command exit code {}\n\nstdout:\n{}\n\nstderr:\n{}",
            status
                .code()
                .map_or_else(|| "unknown".into(), |v| v.to_string()),
            if out.is_empty() { "(empty)" } else { &out },
            if err.is_empty() { "(empty)" } else { &err }
        ));
        ToolResult {
            result: text,
            is_error: !status.success(),
            metadata: crate::ToolResultMetadata {
                exit_code: status.code(),
                truncated: output_truncated,
                affected_paths: vec![],
                stdout: Some(out.chars().take(24_000).collect()),
                stderr: Some(err.chars().take(24_000).collect()),
            },
        }
    }
}

pub fn normalize_command(command: &str) -> String {
    command.split_whitespace().collect::<Vec<_>>().join(" ")
}
pub fn detect_secret(text: &str) -> Option<&'static str> {
    let lower = text.to_ascii_lowercase();
    if Regex::new(r"(?i)-----begin (rsa |ec |openssh )?private key-----")
        .unwrap()
        .is_match(text)
    {
        Some("private key")
    } else if Regex::new(r"(?i)(sk-[a-z0-9_-]{20,}|gh[opusr]_[a-z0-9]{20,}|workos_[a-z0-9_-]{20,})")
        .unwrap()
        .is_match(text)
    {
        Some("API token")
    } else if lower.contains("aws_secret_access_key=") {
        Some("AWS secret")
    } else {
        None
    }
}
fn secret_path(path: &str) -> bool {
    Path::new(path).components().any(|c| {
        let s = c.as_os_str().to_string_lossy().to_ascii_lowercase();
        s == ".env"
            || s.starts_with(".env.")
            || s.ends_with(".pem")
            || s.ends_with(".key")
            || s == "id_rsa"
            || s == "id_ed25519"
            || s == "credentials"
    })
}
fn definition(name: &str, description: &str, parameters: Value) -> AgentToolDefinition {
    AgentToolDefinition {
        name: name.into(),
        description: description.into(),
        parameters,
    }
}
fn string<'a>(v: &'a Value, key: &str) -> Result<&'a str, String> {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("{key} must be a non-empty string."))
}
fn integer(v: &Value, key: &str, default: usize, min: usize, max: usize) -> Result<usize, String> {
    let value = v.get(key).and_then(Value::as_u64).unwrap_or(default as u64) as usize;
    if value < min || value > max {
        Err(format!("{key} must be from {min} to {max}."))
    } else {
        Ok(value)
    }
}
fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}
fn truncate(mut value: String) -> String {
    if value.chars().count() > MAX_RESULT_CHARS {
        value = value.chars().take(MAX_RESULT_CHARS).collect();
        value.push_str("\n[result truncated]")
    }
    value
}
fn result(value: Result<String, String>) -> ToolResult {
    match value {
        Ok(v) => ToolResult::ok(v),
        Err(e) => ToolResult::error(e),
    }
}
fn replace_once(input: &str, old: &str, new: &str) -> Result<String, String> {
    if old.is_empty() {
        return Err("old_string must not be empty.".into());
    }
    let count = input.matches(old).count();
    if count != 1 {
        return Err(format!(
            "old_string must occur exactly once; found {count}."
        ));
    }
    Ok(input.replacen(old, new, 1))
}
