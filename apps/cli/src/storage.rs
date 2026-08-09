use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::{DateTime, Utc};
use fs2::FileExt;
use ronin_agent_core::{
    AgentMessage, AgentSessionMetadata, AgentStopReason, FileChange, TokenUsage,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs,
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Credentials {
    #[serde(default)]
    pub dev_user_id: Option<String>,
    #[serde(default)]
    pub access_token: Option<String>,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_at: Option<i64>,
}

pub fn ronin_dir(home: &Path) -> PathBuf {
    home.join(".ronin")
}
pub fn credentials_path(home: &Path) -> PathBuf {
    ronin_dir(home).join("credentials")
}
pub fn load_credentials(home: &Path) -> Credentials {
    fs::read_to_string(credentials_path(home))
        .ok()
        .and_then(|v| serde_json::from_str(&v).ok())
        .unwrap_or_default()
}

/// Load OAuth credentials from the OS credential vault, migrating the legacy
/// mode-0600 file on first successful access. Development identities remain
/// file-backed because they are intentionally scoped to local API work.
pub fn load_credentials_for(home: &Path, api_url: &str) -> Credentials {
    if let Some(value) = keychain_read(api_url) {
        return value;
    }
    let legacy = load_credentials(home);
    if legacy.access_token.is_some() && keychain_write(api_url, &legacy) {
        let _ = fs::remove_file(credentials_path(home));
    }
    legacy
}
pub fn save_credentials(home: &Path, value: &Credentials) -> Result<(), std::io::Error> {
    secure_dir(&ronin_dir(home))?;
    atomic_json(&credentials_path(home), value)
}

pub fn save_credentials_for(
    home: &Path,
    api_url: &str,
    value: &Credentials,
) -> Result<(), std::io::Error> {
    if value.dev_user_id.is_none() && keychain_write(api_url, value) {
        let _ = fs::remove_file(credentials_path(home));
        return Ok(());
    }
    save_credentials(home, value)
}
pub fn clear_credentials(home: &Path) {
    let _ = fs::remove_file(credentials_path(home));
}

pub fn clear_credentials_for(home: &Path, api_url: &str) {
    keychain_delete(api_url);
    clear_credentials(home);
}

const KEYCHAIN_SERVICE: &str = "africa.ronin.cli";

fn keychain_account(api_url: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(api_url.trim_end_matches('/').as_bytes());
    format!("api-{}", hex(&digest[..12]))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn keychain_read(api_url: &str) -> Option<Credentials> {
    let account = keychain_account(api_url);
    let entry = keyring::Entry::new(KEYCHAIN_SERVICE, &account).ok()?;
    let secret = entry.get_password().ok()?;
    serde_json::from_str(&secret).ok()
}

fn keychain_write(api_url: &str, value: &Credentials) -> bool {
    let Ok(secret) = serde_json::to_string(value) else {
        return false;
    };
    let account = keychain_account(api_url);
    keyring::Entry::new(KEYCHAIN_SERVICE, &account)
        .and_then(|entry| entry.set_password(&secret))
        .is_ok()
}

fn keychain_delete(api_url: &str) {
    let account = keychain_account(api_url);
    if let Ok(entry) = keyring::Entry::new(KEYCHAIN_SERVICE, &account) {
        let _ = entry.delete_credential();
    }
}
pub fn access_token_expiry(token: &str) -> Option<i64> {
    let raw = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(raw).ok()?;
    serde_json::from_slice::<Value>(&bytes)
        .ok()?
        .get("exp")?
        .as_i64()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalSession {
    pub version: u8,
    pub id: String,
    pub cwd: String,
    pub model: String,
    pub provider_session_id: String,
    pub messages: Vec<AgentMessage>,
    pub cost_micro: u64,
    pub rounds: u32,
    #[serde(default)]
    pub last_usage: Option<TokenUsage>,
    #[serde(default)]
    pub context_percent: Option<f64>,
    #[serde(default)]
    pub active_generation_id: Option<String>,
    #[serde(default)]
    pub stop_reason: Option<AgentStopReason>,
    pub state: SessionState,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub compaction_count: u32,
    #[serde(default)]
    pub model_history: Vec<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub archived_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub parent_session_id: Option<String>,
    #[serde(default = "default_permission_mode")]
    pub permission_mode: String,
    #[serde(default)]
    pub pending_turn_base: Option<usize>,
    #[serde(default)]
    pub activity: Vec<SessionActivity>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub last_turn_changes: Vec<FileChange>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub last_turn_affected_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub last_turn_commands: Vec<String>,
}

fn default_permission_mode() -> String {
    "default".into()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionActivity {
    pub id: String,
    pub kind: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionLeaseOwner {
    pub version: u8,
    pub session_id: String,
    pub surface: String,
    pub pid: u32,
    pub app_version: String,
    pub acquired_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub enum SessionLeaseStatus {
    Available,
    Live(SessionLeaseOwner),
    Stale(SessionLeaseOwner),
}

pub struct SessionLease {
    file: File,
    sidecar: PathBuf,
}

impl Drop for SessionLease {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
        let _ = fs::remove_file(&self.sidecar);
    }
}

#[derive(Debug, Clone)]
pub enum SessionScanEntry {
    Session(Box<LocalSession>),
    Unsupported {
        id: String,
        version: u64,
    },
    Quarantined {
        id: String,
        reason: String,
        path: String,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Active,
    Completed,
    Interrupted,
}
#[derive(Debug, Error)]
pub enum SessionError {
    #[error("Session {0} was not found.")]
    NotFound(String),
    #[error("Session {0} is corrupt: {1}")]
    Corrupt(String, String),
    #[error("Session {0} uses unsupported schema version {1}.")]
    Version(String, u64),
    #[error("{0}")]
    Io(#[from] std::io::Error),
}
pub struct SessionStore {
    home: PathBuf,
}

impl LocalSession {
    pub fn metadata(&self, cwd: &Path) -> AgentSessionMetadata {
        self.metadata_for(cwd, "cli")
    }

    pub fn metadata_for(&self, cwd: &Path, source: &str) -> AgentSessionMetadata {
        AgentSessionMetadata {
            schema_version: 2,
            session_id: self.id.clone(),
            title: None,
            summary: None,
            workspace_name: cwd
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_owned),
            models: if self.model_history.is_empty() {
                vec![self.model.clone()]
            } else {
                self.model_history.clone()
            },
            cost_micro: self.cost_micro,
            rounds: self.rounds,
            state: match &self.state {
                SessionState::Active => "active",
                SessionState::Completed => "completed",
                SessionState::Interrupted => "interrupted",
            }
            .into(),
            source: source.into(),
            started_at: self.created_at.to_rfc3339(),
            updated_at: self.updated_at.to_rfc3339(),
            archived_at: self.archived_at.map(|value| value.to_rfc3339()),
            parent_session_id: self.parent_session_id.clone(),
        }
    }
}
impl SessionStore {
    pub fn new(home: impl Into<PathBuf>) -> Self {
        Self { home: home.into() }
    }
    fn dir(&self) -> PathBuf {
        ronin_dir(&self.home).join("sessions")
    }
    fn path(&self, id: &str) -> Result<PathBuf, SessionError> {
        if !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err(SessionError::Corrupt(id.into(), "invalid id".into()));
        }
        Ok(self.dir().join(format!("{id}.json")))
    }
    pub fn create(
        &self,
        cwd: &Path,
        model: &str,
        messages: Vec<AgentMessage>,
    ) -> Result<LocalSession, SessionError> {
        let now = Utc::now();
        let session = LocalSession {
            version: 3,
            id: Uuid::new_v4().to_string(),
            cwd: cwd.to_string_lossy().into(),
            model: model.into(),
            provider_session_id: Uuid::new_v4().to_string(),
            messages,
            cost_micro: 0,
            rounds: 0,
            last_usage: None,
            context_percent: None,
            active_generation_id: None,
            stop_reason: None,
            state: SessionState::Active,
            created_at: now,
            updated_at: now,
            compaction_count: 0,
            model_history: vec![model.into()],
            title: None,
            archived_at: None,
            parent_session_id: None,
            permission_mode: default_permission_mode(),
            pending_turn_base: None,
            activity: vec![],
            last_turn_changes: vec![],
            last_turn_affected_paths: vec![],
            last_turn_commands: vec![],
        };
        self.save(&session)
    }
    pub fn load(&self, id: &str) -> Result<LocalSession, SessionError> {
        let raw = fs::read_to_string(self.path(id)?).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                SessionError::NotFound(id.into())
            } else {
                SessionError::Io(e)
            }
        })?;
        let value: Value = serde_json::from_str(&raw)
            .map_err(|e| SessionError::Corrupt(id.into(), e.to_string()))?;
        let version = value.get("version").and_then(Value::as_u64).unwrap_or(0);
        if !matches!(version, 1..=3) {
            return Err(SessionError::Version(id.into(), version));
        }
        let mut session: LocalSession = serde_json::from_value(value)
            .map_err(|e| SessionError::Corrupt(id.into(), e.to_string()))?;
        if version <= 2 {
            session.version = 3;
            session.compaction_count = 0;
            if session.model_history.is_empty() {
                session.model_history = vec![session.model.clone()]
            }
        }
        Ok(session)
    }
    pub fn save(&self, value: &LocalSession) -> Result<LocalSession, SessionError> {
        let mut next = value.clone();
        next.version = 3;
        next.activity.truncate(500);
        next.updated_at = Utc::now();
        secure_dir(&self.dir())?;
        atomic_json(&self.path(&next.id)?, &next)?;
        Ok(next)
    }
    pub fn list_all(&self, cwd: Option<&Path>) -> Vec<LocalSession> {
        let mut out = fs::read_dir(self.dir())
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                e.path()
                    .file_stem()
                    .and_then(|v| v.to_str())
                    .and_then(|id| self.load(id).ok())
            })
            .filter(|s| cwd.is_none_or(|c| s.cwd == c.to_string_lossy()))
            .collect::<Vec<_>>();
        out.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        out
    }
    pub fn list(&self, cwd: Option<&Path>) -> Vec<LocalSession> {
        self.list_all(cwd)
            .into_iter()
            .filter(|session| session.archived_at.is_none())
            .collect()
    }
    pub fn latest(&self, cwd: &Path) -> Option<LocalSession> {
        self.list(Some(cwd)).into_iter().next()
    }

    fn trash_dir(&self) -> PathBuf {
        ronin_dir(&self.home).join("session-trash")
    }
    fn quarantine_dir(&self) -> PathBuf {
        ronin_dir(&self.home).join("session-quarantine")
    }
    fn lease_dir(&self) -> PathBuf {
        ronin_dir(&self.home).join("session-leases")
    }

    pub fn rename(&self, id: &str, title: &str) -> Result<LocalSession, SessionError> {
        let mut session = self.load(id)?;
        let title = title.trim();
        if title.is_empty() || title.chars().count() > 200 {
            return Err(SessionError::Corrupt(
                id.into(),
                "title must contain 1-200 characters".into(),
            ));
        }
        session.title = Some(title.into());
        self.save(&session)
    }

    pub fn set_archived(&self, id: &str, archived: bool) -> Result<LocalSession, SessionError> {
        let mut session = self.load(id)?;
        session.archived_at = archived.then(Utc::now);
        self.save(&session)
    }

    pub fn fork(&self, id: &str) -> Result<LocalSession, SessionError> {
        let source = self.load(id)?;
        let now = Utc::now();
        let mut fork = source.clone();
        fork.id = Uuid::new_v4().to_string();
        fork.provider_session_id = Uuid::new_v4().to_string();
        fork.parent_session_id = Some(source.id);
        fork.title = Some(format!(
            "Fork of {}",
            source.title.unwrap_or_else(|| "Untitled session".into())
        ));
        fork.archived_at = None;
        fork.cost_micro = 0;
        fork.rounds = 0;
        fork.last_usage = None;
        fork.context_percent = None;
        fork.active_generation_id = None;
        fork.stop_reason = None;
        fork.state = SessionState::Active;
        fork.created_at = now;
        fork.updated_at = now;
        fork.compaction_count = 0;
        fork.pending_turn_base = None;
        fork.activity.clear();
        fork.last_turn_changes.clear();
        fork.last_turn_affected_paths.clear();
        fork.last_turn_commands.clear();
        fork.permission_mode = default_permission_mode();
        self.save(&fork)
    }

    pub fn trash(&self, id: &str) -> Result<(), SessionError> {
        let source = self.path(id)?;
        if !source.exists() {
            return Err(SessionError::NotFound(id.into()));
        }
        secure_dir(&self.trash_dir())?;
        fs::rename(source, self.trash_dir().join(format!("{id}.json")))?;
        Ok(())
    }

    pub fn list_trash(&self, cwd: Option<&Path>) -> Vec<LocalSession> {
        let mut out = fs::read_dir(self.trash_dir())
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                let raw = fs::read_to_string(entry.path()).ok()?;
                serde_json::from_str::<LocalSession>(&raw).ok()
            })
            .filter(|session| cwd.is_none_or(|path| session.cwd == path.to_string_lossy()))
            .collect::<Vec<_>>();
        out.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        out
    }

    pub fn restore(&self, id: &str) -> Result<LocalSession, SessionError> {
        let source = self.trash_dir().join(format!("{id}.json"));
        if !source.exists() {
            return Err(SessionError::NotFound(id.into()));
        }
        secure_dir(&self.dir())?;
        fs::rename(&source, self.path(id)?)?;
        self.load(id)
    }

    pub fn empty_trash(&self) -> Result<usize, SessionError> {
        let entries = fs::read_dir(self.trash_dir())
            .into_iter()
            .flatten()
            .flatten()
            .collect::<Vec<_>>();
        let mut removed = 0;
        for entry in entries {
            if entry.path().extension().and_then(|value| value.to_str()) == Some("json") {
                fs::remove_file(entry.path())?;
                removed += 1;
            }
        }
        Ok(removed)
    }

    pub fn scan(&self, cwd: Option<&Path>) -> Vec<SessionScanEntry> {
        let mut entries = Vec::new();
        for entry in fs::read_dir(self.dir()).into_iter().flatten().flatten() {
            let id = entry
                .path()
                .file_stem()
                .and_then(|value| value.to_str())
                .unwrap_or("unknown")
                .to_string();
            let raw = match fs::read_to_string(entry.path()) {
                Ok(value) => value,
                Err(error) => {
                    entries.push(SessionScanEntry::Quarantined {
                        id,
                        reason: error.to_string(),
                        path: entry.path().to_string_lossy().into(),
                    });
                    continue;
                }
            };
            let value: Value = match serde_json::from_str(&raw) {
                Ok(value) => value,
                Err(error) => {
                    let _ = secure_dir(&self.quarantine_dir());
                    let target = self.quarantine_dir().join(format!(
                        "{}-{}.json",
                        id,
                        Utc::now().timestamp()
                    ));
                    let _ = fs::rename(entry.path(), &target);
                    entries.push(SessionScanEntry::Quarantined {
                        id,
                        reason: error.to_string(),
                        path: target.to_string_lossy().into(),
                    });
                    continue;
                }
            };
            let version = value.get("version").and_then(Value::as_u64).unwrap_or(0);
            if version > 3 {
                entries.push(SessionScanEntry::Unsupported { id, version });
                continue;
            }
            match self.load(&id) {
                Ok(session) if cwd.is_none_or(|path| session.cwd == path.to_string_lossy()) => {
                    entries.push(SessionScanEntry::Session(Box::new(session)))
                }
                Ok(_) => {}
                Err(error) => entries.push(SessionScanEntry::Quarantined {
                    id,
                    reason: error.to_string(),
                    path: entry.path().to_string_lossy().into(),
                }),
            }
        }
        entries
    }

    fn lease_paths(&self, id: &str) -> Result<(PathBuf, PathBuf), SessionError> {
        self.path(id)?;
        Ok((
            self.lease_dir().join(format!("{id}.lock")),
            self.lease_dir().join(format!("{id}.owner.json")),
        ))
    }

    pub fn lease_status(&self, id: &str) -> Result<SessionLeaseStatus, SessionError> {
        let (lock_path, sidecar) = self.lease_paths(id)?;
        secure_dir(&self.lease_dir())?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)?;
        let owner = fs::read_to_string(&sidecar)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok());
        match file.try_lock_exclusive() {
            Ok(()) => {
                let _ = FileExt::unlock(&file);
                Ok(owner
                    .map(SessionLeaseStatus::Stale)
                    .unwrap_or(SessionLeaseStatus::Available))
            }
            Err(error) if error.kind() == fs2::lock_contended_error().kind() => {
                Ok(owner.map(SessionLeaseStatus::Live).unwrap_or_else(|| {
                    SessionLeaseStatus::Live(SessionLeaseOwner {
                        version: 1,
                        session_id: id.into(),
                        surface: "another client".into(),
                        pid: 0,
                        app_version: "unknown".into(),
                        acquired_at: Utc::now(),
                    })
                }))
            }
            Err(error) => Err(SessionError::Io(error)),
        }
    }

    pub fn acquire_lease(
        &self,
        id: &str,
        surface: &str,
        app_version: &str,
        allow_stale: bool,
    ) -> Result<SessionLease, SessionError> {
        let status = self.lease_status(id)?;
        if matches!(status, SessionLeaseStatus::Live(_))
            || matches!(status, SessionLeaseStatus::Stale(_)) && !allow_stale
        {
            return Err(SessionError::Io(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "session writer lease is unavailable",
            )));
        }
        let (lock_path, sidecar) = self.lease_paths(id)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)?;
        file.try_lock_exclusive()?;
        let owner = SessionLeaseOwner {
            version: 1,
            session_id: id.into(),
            surface: surface.into(),
            pid: std::process::id(),
            app_version: app_version.into(),
            acquired_at: Utc::now(),
        };
        atomic_json(&sidecar, &owner)?;
        Ok(SessionLease { file, sidecar })
    }
}

fn secure_dir(path: &Path) -> Result<(), std::io::Error> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?
    }
    Ok(())
}
fn atomic_json<T: Serialize>(path: &Path, value: &T) -> Result<(), std::io::Error> {
    secure_dir(path.parent().unwrap())?;
    let mut tmp = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
    tmp.write_all(serde_json::to_string_pretty(value)?.as_bytes())?;
    tmp.write_all(b"\n")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tmp.as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?
    }
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ronin_agent_core::AgentMessage;

    #[test]
    fn fork_resets_accounting_and_trash_is_recoverable() {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = SessionStore::new(home.path());
        let mut session = store
            .create(
                workspace.path(),
                "openai/test",
                vec![AgentMessage::user("hello")],
            )
            .unwrap();
        session.title = Some("Original".into());
        session.cost_micro = 42;
        session.rounds = 3;
        session = store.save(&session).unwrap();
        let fork = store.fork(&session.id).unwrap();
        assert_eq!(fork.parent_session_id.as_deref(), Some(session.id.as_str()));
        assert_eq!(fork.cost_micro, 0);
        assert_eq!(fork.rounds, 0);
        assert_eq!(fork.messages, session.messages);
        store.trash(&fork.id).unwrap();
        assert!(store.load(&fork.id).is_err());
        assert_eq!(store.list_trash(Some(workspace.path())).len(), 1);
        assert_eq!(store.restore(&fork.id).unwrap().id, fork.id);
    }

    #[test]
    fn live_and_stale_leases_are_distinguished() {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = SessionStore::new(home.path());
        let session = store
            .create(workspace.path(), "openai/test", vec![])
            .unwrap();
        let lease = store
            .acquire_lease(&session.id, "desktop", "test", false)
            .unwrap();
        assert!(matches!(
            store.lease_status(&session.id).unwrap(),
            SessionLeaseStatus::Live(_)
        ));
        drop(lease);
        assert!(matches!(
            store.lease_status(&session.id).unwrap(),
            SessionLeaseStatus::Available
        ));
        let (_, sidecar) = store.lease_paths(&session.id).unwrap();
        atomic_json(
            &sidecar,
            &SessionLeaseOwner {
                version: 1,
                session_id: session.id.clone(),
                surface: "desktop".into(),
                pid: 999,
                app_version: "test".into(),
                acquired_at: Utc::now(),
            },
        )
        .unwrap();
        assert!(matches!(
            store.lease_status(&session.id).unwrap(),
            SessionLeaseStatus::Stale(_)
        ));
        assert!(store
            .acquire_lease(&session.id, "cli", "test", false)
            .is_err());
        assert!(store
            .acquire_lease(&session.id, "cli", "test", true)
            .is_ok());
    }

    #[test]
    fn malformed_records_are_quarantined_and_newer_records_stay_read_only() {
        let home = tempfile::tempdir().unwrap();
        let store = SessionStore::new(home.path());
        secure_dir(&store.dir()).unwrap();
        fs::write(store.dir().join("broken.json"), "not json").unwrap();
        fs::write(
            store.dir().join("future.json"),
            r#"{"version":99,"id":"future"}"#,
        )
        .unwrap();
        let scan = store.scan(None);
        assert!(scan.iter().any(
            |entry| matches!(entry, SessionScanEntry::Quarantined { id, .. } if id == "broken")
        ));
        assert!(scan.iter().any(|entry| matches!(entry, SessionScanEntry::Unsupported { id, version: 99 } if id == "future")));
        assert!(store.dir().join("future.json").exists());
    }
}
